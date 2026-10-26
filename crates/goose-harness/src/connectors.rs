//! Managed connectors (AC-5.2–5.4): assess configuration state and
//! health-check MCP servers, injecting vault secrets as environment
//! variables for the check.
//!
//! Health = a real MCP `initialize` handshake: for stdio servers the
//! process is spawned and spoken to over its pipes; for HTTP servers
//! the initialize JSON-RPC is POSTed. A connector that cannot
//! complete the handshake is unhealthy, with the reason attached.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::ConnectorDef;
use crate::vault::Vault;

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorStatus {
    pub name: String,
    /// Every declared secret account is present in the vault.
    pub configured: bool,
    pub missing_accounts: Vec<String>,
    /// Result of the last health check, when one was run.
    pub healthy: Option<bool>,
    pub detail: String,
}

/// Vault accounts this connector declares that are not present.
pub fn missing_accounts(def: &ConnectorDef, vault: &dyn Vault) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for secret in &def.env_secrets {
        if vault.get(&secret.account)?.is_none() {
            missing.push(secret.account.clone());
        }
    }
    Ok(missing)
}

/// Environment for spawning this connector: the declared secrets,
/// resolved from the vault. Errors name the missing account, never
/// a value.
pub fn resolve_env(def: &ConnectorDef, vault: &dyn Vault) -> Result<HashMap<String, String>> {
    let missing = missing_accounts(def, vault)?;
    if !missing.is_empty() {
        anyhow::bail!("missing vault secrets: {}", missing.join(", "));
    }
    let mut env = HashMap::new();
    for secret in &def.env_secrets {
        if let Some(value) = vault.get(&secret.account)? {
            env.insert(secret.env.clone(), value);
        }
    }
    Ok(env)
}

pub fn assess(def: &ConnectorDef, vault: &dyn Vault) -> Result<ConnectorStatus> {
    let missing = missing_accounts(def, vault)?;
    if missing.is_empty() {
        Ok(ConnectorStatus {
            name: def.name.clone(),
            configured: true,
            missing_accounts: Vec::new(),
            healthy: None,
            detail: "configured".to_string(),
        })
    } else {
        Ok(ConnectorStatus {
            name: def.name.clone(),
            configured: false,
            missing_accounts: missing.clone(),
            healthy: None,
            detail: format!("missing vault secrets: {}", missing.join(", ")),
        })
    }
}

const PROTOCOL_VERSION: &str = "2025-03-26";

fn initialize_request() -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "warmachine-harness", "version": "1.0"}
        }
    })
}

fn parse_initialize_response(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body.trim())
        .with_context(|| format!("non-JSON response: {}", body.get(..120).unwrap_or(body)))?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("server returned an error: {error}");
    }
    let info = value
        .pointer("/result/serverInfo")
        .ok_or_else(|| anyhow::anyhow!("response has no result.serverInfo"))?;
    let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let version = info.get("version").and_then(|v| v.as_str()).unwrap_or("?");
    Ok(format!("{name} {version}"))
}

/// Run the health check. Returns the status detail (server identity)
/// on success; the error explains the failure.
pub async fn health_check(def: &ConnectorDef, vault: &dyn Vault) -> Result<String> {
    let env = resolve_env(def, vault)?;
    if let Some(command) = &def.command {
        health_check_stdio(command, &def.args, &env).await
    } else if let Some(url) = &def.url {
        health_check_http(url, &env).await
    } else {
        anyhow::bail!("connector defines neither command nor url")
    }
}

async fn health_check_stdio(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<String> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {command}"))?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);

    let request = format!("{}\n", initialize_request());
    stdin.write_all(request.as_bytes()).await?;
    stdin.flush().await?;

    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await;
    let detail = match read {
        Ok(Ok(0)) => Err(anyhow::anyhow!("server closed stdout without responding")),
        Ok(Ok(_)) => parse_initialize_response(&line),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(anyhow::anyhow!("no initialize response within 10s")),
    };
    let _ = child.kill().await;
    detail
}

async fn health_check_http(url: &str, env: &HashMap<String, String>) -> Result<String> {
    // Minimal HTTP/1.1 JSON-RPC POST — enough for an initialize
    // handshake against a streamable-HTTP MCP endpoint.
    if url.starts_with("https://") {
        anyhow::bail!("https health checks need a TLS client; use the stdio transport or an http:// loopback endpoint");
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("url must start with http:// or https://"))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((host, tail)) => (host, format!("/{tail}")),
        None => (rest, "/".to_string()),
    };
    let path = path.as_str();
    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(hostport),
    )
    .await
    .with_context(|| format!("connecting to {hostport}"))??;
    let body = initialize_request().to_string();
    let mut headers = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    // Vault secrets map to bearer-style headers for HTTP connectors:
    // an env entry named AUTHORIZATION is sent as that header.
    if let Some(auth) = env.get("AUTHORIZATION") {
        headers.push_str(&format!("Authorization: {auth}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response),
    )
    .await
    .context("reading health check response")??;
    let (_, response_body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP response"))?;
    parse_initialize_response(response_body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvSecret;
    use crate::vault::MemoryVault;

    fn def() -> ConnectorDef {
        ConnectorDef {
            name: "demo".to_string(),
            description: None,
            command: Some("cat".to_string()),
            args: Vec::new(),
            url: None,
            env_secrets: vec![EnvSecret {
                env: "DEMO_TOKEN".to_string(),
                account: "demo-token".to_string(),
            }],
        }
    }

    #[test]
    fn assess_reports_missing_then_configured() {
        let vault = MemoryVault::default();
        let status = assess(&def(), &vault).unwrap();
        assert!(!status.configured);
        assert_eq!(status.missing_accounts, vec!["demo-token".to_string()]);

        vault.set("demo-token", "s3cret-value").unwrap();
        let status = assess(&def(), &vault).unwrap();
        assert!(status.configured);
        let env = resolve_env(&def(), &vault).unwrap();
        assert_eq!(
            env.get("DEMO_TOKEN").map(String::as_str),
            Some("s3cret-value")
        );
    }

    #[test]
    fn config_serialization_never_contains_secret_values() {
        // The connector definition holds account names only; there is
        // no field a secret value could hide in (AC-5.1).
        let text = serde_json::to_string(&def()).unwrap();
        assert!(text.contains("demo-token"));
        assert!(!text.contains("s3cret"));
    }
}
