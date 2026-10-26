//! Harness action-class gate for non-interactive runs (AC-4.3).
//!
//! Called from `ExtensionManager::dispatch_tool_call_inner`, the one
//! funnel both reply engines pass through before a tool executes.
//! Interactive (user) sessions are never gated — they keep the
//! existing in-band permission flow. Background sessions (scheduled
//! jobs, daemon-dispatched runs) are classified by tool name against
//! the harness action classes:
//!
//! - Forbidden: denied outright, audited.
//! - AutoApproved: allowed, audited.
//! - ApprovalRequired: a durable approval request is queued in
//!   harness.db and the call waits for the user's decision from any
//!   surface (CLI today). Unanswered requests expire into a denial —
//!   default-deny on timeout (AC-4.2).
//!
//! There is no path around this check for a background session: the
//! funnel is below both engines, and a session's type is fixed when
//! the scheduler creates it (AC-4.6).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

use crate::config::paths::Paths;
use crate::harness::audit;
use crate::harness::{ActionClass, ApprovalStatus, ApprovalStore};
use crate::session::{SessionManager, SessionType};

static BACKGROUND_CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn background_cache() -> &'static Mutex<HashMap<String, bool>> {
    BACKGROUND_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Is this session a non-interactive (background) run? Resolved once
/// per session from the session record and cached for the process.
/// A session that cannot be found is treated as interactive: the
/// gate exists to constrain unattended runs, and misclassifying a
/// live interactive session would deadlock it behind a queue nobody
/// is watching in-band.
async fn is_background(session_id: &str) -> bool {
    if let Some(cached) = background_cache()
        .lock()
        .expect("harness gate cache poisoned")
        .get(session_id)
    {
        return *cached;
    }
    let background = match SessionManager::instance()
        .get_session(session_id, false)
        .await
    {
        Ok(session) => session.session_type == SessionType::Scheduled,
        Err(_) => false,
    };
    background_cache()
        .lock()
        .expect("harness gate cache poisoned")
        .insert(session_id.to_string(), background);
    background
}

fn audit_best_effort(event: &str, session_id: &str, details: serde_json::Value) {
    let path = Paths::in_config_dir("audit.log");
    if let Err(e) = audit::append(&path, event, session_id, details) {
        tracing::warn!("harness audit write failed: {e:#}");
    }
}

/// Gate one tool call. `Ok(())` lets the call proceed; `Err(message)`
/// denies it with a user-visible reason.
pub async fn check(
    session_id: &str,
    tool_name: &str,
    arguments_json: &str,
    cancellation_token: &CancellationToken,
) -> Result<(), String> {
    if !is_background(session_id).await {
        return Ok(());
    }

    let config_path = Paths::in_config_dir("config.yaml");
    let yaml = std::fs::read_to_string(&config_path).unwrap_or_default();
    let config = crate::harness::config::parse_harness_config(&yaml).map_err(|e| {
        format!(
            "harness policy: invalid config in {}: {e}",
            config_path.display()
        )
    })?;

    match config.action_classes.classify(tool_name) {
        ActionClass::Forbidden => {
            audit_best_effort(
                "action_forbidden",
                session_id,
                serde_json::json!({"tool": tool_name}),
            );
            Err(format!(
                "harness policy: tool '{tool_name}' is forbidden in background runs"
            ))
        }
        ActionClass::AutoApproved => {
            audit_best_effort(
                "action_auto_approved",
                session_id,
                serde_json::json!({"tool": tool_name}),
            );
            Ok(())
        }
        ActionClass::ApprovalRequired => {
            let db_path = Paths::in_config_dir("harness.db");
            let store = ApprovalStore::open(&db_path)
                .await
                .map_err(|e| format!("harness policy: approval queue unavailable: {e}"))?;
            let request = store
                .create(
                    tool_name,
                    "approval_required",
                    &format!("session:{session_id}"),
                    arguments_json,
                    config.approvals.default_expiry_minutes,
                )
                .await
                .map_err(|e| format!("harness policy: could not queue approval: {e}"))?;
            audit_best_effort(
                "approval_requested",
                session_id,
                serde_json::json!({"approval_id": request.id, "tool": tool_name}),
            );
            let request_id = request.id.clone();
            let outcome = wait_for_decision(&store, &request_id, cancellation_token).await;
            store.close().await;
            match outcome {
                Ok(true) => {
                    audit_best_effort(
                        "approval_granted",
                        session_id,
                        serde_json::json!({"approval_id": request_id, "tool": tool_name}),
                    );
                    Ok(())
                }
                Ok(false) => Err(format!(
                    "harness policy: approval for tool '{tool_name}' was denied or expired (request {request_id})"
                )),
                Err(message) => Err(message),
            }
        }
    }
}

/// Poll the queue until the request is decided. Expiry is a denial.
async fn wait_for_decision(
    store: &ApprovalStore,
    request_id: &str,
    cancellation_token: &CancellationToken,
) -> Result<bool, String> {
    loop {
        if cancellation_token.is_cancelled() {
            return Err(
                "harness policy: run cancelled while awaiting approval; the request stays queued and will expire"
                    .to_string(),
            );
        }
        let request = store
            .get(request_id)
            .await
            .map_err(|e| format!("harness policy: approval queue read failed: {e}"))?
            .ok_or_else(|| "harness policy: approval request vanished".to_string())?;
        match request.status {
            ApprovalStatus::Approved => return Ok(true),
            ApprovalStatus::Denied | ApprovalStatus::Expired => return Ok(false),
            ApprovalStatus::Pending => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}
