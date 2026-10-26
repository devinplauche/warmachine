//! Integration test: the MCP health handshake against the fake
//! stdio server fixture, with the token resolved from a vault.
//! Proves the connector loop — config refs → vault → env injection →
//! server accepts/rejects — for everything except the OS-keychain
//! syscall itself (which needs a secret service the test runner may
//! not have).

use goose_harness::config::{ConnectorDef, EnvSecret};
use goose_harness::connectors::health_check;
use goose_harness::vault::{MemoryVault, Vault};

fn fixture_path() -> String {
    // The fixture ships in the repo next to this test.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake_mcp_server.py")
        .to_string_lossy()
        .to_string()
}

fn def() -> ConnectorDef {
    ConnectorDef {
        name: "fake-mcp".to_string(),
        description: None,
        command: Some("python3".to_string()),
        args: vec![fixture_path()],
        url: None,
        env_secrets: vec![EnvSecret {
            env: "DEMO_TOKEN".to_string(),
            account: "demo-mcp-token".to_string(),
        }],
    }
}

#[tokio::test]
async fn health_check_accepts_the_vault_token_and_rejects_a_wrong_one() {
    assert!(
        std::path::Path::new(&fixture_path()).exists(),
        "fixture missing: {}",
        fixture_path()
    );
    let vault = MemoryVault::default();

    // No secret at all → health check refuses with the account name.
    let err = health_check(&def(), &vault).await.unwrap_err();
    assert!(err.to_string().contains("demo-mcp-token"), "{err}");

    // Wrong secret → the server itself rejects the handshake.
    vault.set("demo-mcp-token", "wrong-token").unwrap();
    let err = health_check(&def(), &vault).await.unwrap_err();
    assert!(err.to_string().contains("unauthorized"), "{err}");

    // Right secret → handshake completes with the server identity.
    vault.set("demo-mcp-token", "phase5-token-123").unwrap();
    let detail = health_check(&def(), &vault).await.unwrap();
    assert_eq!(detail, "fake-mcp 0.1.0");
}
