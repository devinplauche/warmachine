//! Deterministic proof for the Phase 4 approval gate
//! (`agents::harness_gate::check`), exercised at the exact function
//! the tool-dispatch funnel calls — no LLM required:
//!
//! - interactive sessions are never gated;
//! - background (Scheduled) sessions: auto-approved tools pass,
//!   forbidden tools are denied, approval-required tools queue a
//!   durable request and block until an external decision arrives;
//! - decisions and classifications land in the audit log.
//!
//! Runs in its own process (integration test) because the gate reads
//! process-global state (config root via WARMACHINE_PATH_ROOT).

#![cfg(all(feature = "harness", not(feature = "onprem")))]

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use warmachine::agents::harness_gate;
use warmachine::config::paths::Paths;
use warmachine::config::GooseMode;
use warmachine::harness::{ApprovalStatus, ApprovalStore};
use warmachine::session::{SessionManager, SessionType};

#[tokio::test]
async fn gate_enforces_action_classes_for_background_sessions_only() {
    // Fresh config root for this process.
    let root = std::env::temp_dir().join(format!("harness-gate-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::env::set_var("WARMACHINE_PATH_ROOT", &root);

    std::fs::write(
        root.join("config/config.yaml"),
        "harness:\n  action_classes:\n    auto_approved: [\"read_*\"]\n    forbidden: [\"*danger*\"]\n",
    )
    .unwrap();

    let manager = SessionManager::instance();
    let interactive = manager
        .create_session(
            root.clone(),
            "interactive".to_string(),
            SessionType::User,
            GooseMode::Auto,
        )
        .await
        .unwrap();
    let background = manager
        .create_session(
            root.clone(),
            "background".to_string(),
            SessionType::Scheduled,
            GooseMode::Auto,
        )
        .await
        .unwrap();

    // Interactive: even a forbidden-class tool passes untouched.
    harness_gate::check(
        &interactive.id,
        "danger_tool",
        "{}",
        &CancellationToken::new(),
    )
    .await
    .expect("interactive sessions are never gated");

    // Background + auto-approved: passes.
    harness_gate::check(&background.id, "read_file", "{}", &CancellationToken::new())
        .await
        .expect("auto-approved tool passes");

    // Background + forbidden: denied.
    let err = harness_gate::check(
        &background.id,
        "danger_tool",
        "{}",
        &CancellationToken::new(),
    )
    .await
    .expect_err("forbidden tool must be denied");
    assert!(err.contains("forbidden"), "unexpected denial: {err}");

    // Background + approval-required: the call blocks; a queue row
    // appears; an external approve releases it.
    let bg_id = background.id.clone();
    let waiter = tokio::spawn(async move {
        harness_gate::check(
            &bg_id,
            "shell",
            "{\"command\":\"echo hi\"}",
            &CancellationToken::new(),
        )
        .await
    });
    let db_path = Paths::in_config_dir("harness.db");
    let mut request_id = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let store = ApprovalStore::open(&db_path).await.unwrap();
        let pending = store.list(Some(ApprovalStatus::Pending)).await.unwrap();
        store.close().await;
        if let Some(req) = pending.iter().find(|r| r.action == "shell") {
            request_id = req.id.clone();
            break;
        }
    }
    assert!(!request_id.is_empty(), "gate never queued the approval");
    let store = ApprovalStore::open(&db_path).await.unwrap();
    store
        .decide(&request_id, true, "test", Some("go"))
        .await
        .unwrap();
    store.close().await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("gate did not release after approval")
        .unwrap();
    outcome.expect("approved tool call must pass the gate");

    // Deny path: a second queued request, denied externally, fails.
    let bg_id = background.id.clone();
    let waiter = tokio::spawn(async move {
        harness_gate::check(&bg_id, "shell", "{}", &CancellationToken::new()).await
    });
    let mut deny_id = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let store = ApprovalStore::open(&db_path).await.unwrap();
        let pending = store.list(Some(ApprovalStatus::Pending)).await.unwrap();
        store.close().await;
        if let Some(req) = pending.iter().find(|r| r.action == "shell") {
            deny_id = req.id.clone();
            break;
        }
    }
    assert!(!deny_id.is_empty(), "second approval never queued");
    let store = ApprovalStore::open(&db_path).await.unwrap();
    store.decide(&deny_id, false, "test", None).await.unwrap();
    store.close().await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("gate did not release after denial")
        .unwrap();
    let err = outcome.expect_err("denied tool call must fail the gate");
    assert!(err.contains("denied"), "unexpected error: {err}");

    // The audit log recorded the gate's decisions.
    let audit = std::fs::read_to_string(Paths::in_config_dir("audit.log")).unwrap();
    for event in [
        "action_auto_approved",
        "action_forbidden",
        "approval_requested",
        "approval_granted",
    ] {
        assert!(audit.contains(event), "audit log missing {event}");
    }
    warmachine::harness::audit::verify(&Paths::in_config_dir("audit.log"))
        .expect("audit chain must verify");

    let _ = std::fs::remove_dir_all(&root);
}
