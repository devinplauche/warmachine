//! `warmachine daemon` (AC-3): the harness daemon. Hosts the existing
//! scheduler, serves the loopback webhook trigger (design decision
//! D6), dispatches trigger runs as ledger tasks, and delivers
//! completed task results through the delivery policy.
//!
//! State is inspectable while it runs: the daemon heartbeats a
//! `daemon-state.json` in the config dir, and the append-only
//! `delivery_log` table in harness.db records every policy decision.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use warmachine::config::paths::Paths;
use warmachine::harness::{
    ApprovalStore, Decision, DeliverOutcome, DeliveryLog, DeliveryPolicy, FileDeliveryChannel,
    HarnessConfig, NewTask, Task, TaskStatus, TaskStore, TriggerGuard, WebhookTrigger,
};

#[derive(Debug, Serialize, Deserialize)]
struct DaemonState {
    pid: u32,
    started_at: String,
    webhook: String,
    triggers: Vec<String>,
    scheduler_jobs: usize,
    tasks_open: usize,
    tasks_failed: usize,
    tasks_pending_delivery: usize,
    last_tick: String,
}

fn load_config() -> Result<HarnessConfig> {
    let path = Paths::in_config_dir("config.yaml");
    let yaml = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    warmachine::harness::config::parse_harness_config(&yaml)
        .map_err(|e| anyhow::anyhow!("invalid harness config in {}: {e}", path.display()))
}

fn state_path() -> PathBuf {
    Paths::in_config_dir("daemon-state.json")
}

fn deliveries_path() -> PathBuf {
    Paths::in_config_dir("deliveries.md")
}

/// The webhook token lives in an owner-only file in the config dir.
/// (Design D6 called for the OS keychain; a 0600 file is the portable
/// equivalent until the Phase 5 vault lands.)
fn load_or_create_token() -> Result<String> {
    let path = Paths::in_config_dir("daemon.token");
    if path.exists() {
        return Ok(std::fs::read_to_string(&path)?.trim().to_string());
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

pub async fn run_daemon() -> Result<()> {
    let config = load_config()?;
    let bind: IpAddr = config
        .daemon
        .webhook_bind
        .parse()
        .with_context(|| format!("invalid webhook_bind {}", config.daemon.webhook_bind))?;
    if !bind.is_loopback() {
        anyhow::bail!(
            "webhook_bind must be a loopback address (got {bind}); the harness webhook never listens externally"
        );
    }
    let port = config.daemon.webhook_port;
    let token = Arc::new(load_or_create_token()?);

    let db_path = Paths::in_config_dir("harness.db");
    let store = Arc::new(TaskStore::open(&db_path).await?);
    let approvals = Arc::new(ApprovalStore::open(&db_path).await?);
    let delivery_log = Arc::new(DeliveryLog::open(&db_path).await?);
    let audit_path = Paths::in_config_dir("audit.log");
    let alert_primary = config
        .delivery
        .alert_primary_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(deliveries_path);
    let alert_fallback = config
        .delivery
        .alert_fallback_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Paths::in_config_dir("alerts.md"));
    let policy = Arc::new(DeliveryPolicy::new(config.delivery.clone()));
    let guard = Arc::new(TriggerGuard::new());
    let triggers: Arc<Vec<WebhookTrigger>> = Arc::new(config.daemon.triggers.clone());

    // Host the existing scheduler (AC-3.1): constructing it starts
    // its cron loop; scheduled recipe jobs run while the daemon lives.
    #[cfg(feature = "scheduler")]
    let scheduler = {
        let storage = warmachine::scheduler::get_default_scheduler_storage_path()
            .context("failed to resolve scheduler storage path")?;
        let session_manager = Arc::new(warmachine::session::SessionManager::instance());
        let scheduler = warmachine::scheduler::Scheduler::new(storage, session_manager)
            .await
            .map_err(|e| anyhow::anyhow!("failed to start scheduler: {e}"))?;
        println!(
            "scheduler hosted ({} job(s) loaded)",
            scheduler.list_scheduled_jobs().await.len()
        );
        Some(scheduler)
    };
    let listener = TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("binding webhook listener on {bind}:{port}"))?;
    println!("warmachine daemon listening on http://{bind}:{port} (loopback only)");
    println!(
        "webhook token: {} (send it as the X-Harness-Token header)",
        Paths::in_config_dir("daemon.token").display()
    );
    println!(
        "triggers configured: {}",
        if triggers.is_empty() {
            "none — add harness.daemon.triggers entries to config.yaml".to_string()
        } else {
            triggers
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    let started_at = chrono::Utc::now().to_rfc3339();
    #[cfg(feature = "scheduler")]
    let scheduler_jobs = match &scheduler {
        Some(s) => s.list_scheduled_jobs().await.len(),
        None => 0,
    };
    #[cfg(not(feature = "scheduler"))]
    let scheduler_jobs = 0;
    write_state(&store, scheduler_jobs, &started_at, bind, port, &triggers).await;

    let server = {
        let store = store.clone();
        let guard = guard.clone();
        let triggers = triggers.clone();
        let token = token.clone();
        let audit_path = audit_path.clone();
        let alert_primary = alert_primary.clone();
        let alert_fallback = alert_fallback.clone();
        #[cfg(feature = "scheduler")]
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let ctx = ConnCtx {
                    store: store.clone(),
                    guard: guard.clone(),
                    triggers: triggers.clone(),
                    token: token.clone(),
                    audit_path: audit_path.clone(),
                    alert_primary: alert_primary.clone(),
                    alert_fallback: alert_fallback.clone(),
                    #[cfg(feature = "scheduler")]
                    scheduler: scheduler.clone(),
                };
                tokio::spawn(handle_connection(socket, ctx));
            }
        })
    };

    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    // Both Ctrl-C and SIGTERM shut the daemon down cleanly: systemd
    // and `daemon stop` use SIGTERM, and the state file cleanup below
    // must run either way.
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                println!("shutting down (signal)");
                break;
            }
            _ = tick.tick() => {
                // Surface approval expiries promptly (AC-4.2): the
                // requesting run also sees the denial on its next
                // poll via lazy expiry, but the audit entry is the
                // user's record that it happened.
                match approvals.expire_due().await {
                    Ok(expired) => {
                        for id in expired {
                            let _ = warmachine::harness::audit::append(
                                &audit_path,
                                "approval_expired",
                                "",
                                serde_json::json!({"approval_id": id}),
                            );
                        }
                    }
                    Err(e) => eprintln!("approval sweep error: {e}"),
                }
                deliver_due(&store, &delivery_log, &policy, &triggers, &audit_path).await;
                #[cfg(feature = "scheduler")]
                let scheduler_jobs = match &scheduler {
                    Some(s) => s.list_scheduled_jobs().await.len(),
                    None => 0,
                };
                #[cfg(not(feature = "scheduler"))]
                let scheduler_jobs = 0;
                write_state(&store, scheduler_jobs, &started_at, bind, port, &triggers).await;
            }
        }
    }
    server.abort();
    let _ = std::fs::remove_file(state_path());
    println!("daemon stopped");
    Ok(())
}

async fn write_state(
    store: &Arc<TaskStore>,
    scheduler_jobs: usize,
    started_at: &str,
    bind: IpAddr,
    port: u16,
    triggers: &[WebhookTrigger],
) {
    let count = |status: TaskStatus| async move {
        store
            .list(Some(status), None, false)
            .await
            .map(|v| v.len())
            .unwrap_or(0)
    };
    let state = DaemonState {
        pid: std::process::id(),
        started_at: started_at.to_string(),
        webhook: format!("http://{bind}:{port}"),
        triggers: triggers.iter().map(|t| t.name.clone()).collect(),
        scheduler_jobs,
        tasks_open: count(TaskStatus::Open).await + count(TaskStatus::InProgress).await,
        tasks_failed: count(TaskStatus::Failed).await,
        tasks_pending_delivery: count(TaskStatus::Done).await,
        last_tick: chrono::Utc::now().to_rfc3339(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(state_path(), text);
    }
}

/// One delivery pass: completed tasks go out through the policy.
/// Denied tasks stay `done` and are retried on a later tick; the
/// denial itself is in the delivery log with its reason (AC-3.3).
async fn deliver_due(
    store: &Arc<TaskStore>,
    delivery_log: &Arc<DeliveryLog>,
    policy: &Arc<DeliveryPolicy>,
    triggers: &[WebhookTrigger],
    audit_path: &std::path::Path,
) {
    let Ok(done) = store.list(Some(TaskStatus::Done), None, false).await else {
        return;
    };
    if done.is_empty() {
        return;
    }
    let channel = FileDeliveryChannel {
        path: deliveries_path(),
    };
    for task in done {
        // Trigger tasks carry their configured topic; other owners
        // deliver under no topic.
        let topic = task
            .owner
            .strip_prefix("trigger:")
            .and_then(|name| triggers.iter().find(|t| t.name == name))
            .and_then(|t| t.topic.as_deref());
        match policy.check_and_log(delivery_log, topic, &task.id).await {
            Ok(Decision::Allow) => match store.deliver_one(&task.id, &channel, "daemon").await {
                Ok(DeliverOutcome::Delivered) => {
                    let _ = warmachine::harness::audit::append(
                        audit_path,
                        "delivery",
                        "",
                        serde_json::json!({"task_id": task.id, "channel": "file"}),
                    );
                }
                Ok(DeliverOutcome::Failed(error)) => {
                    let _ = delivery_log
                        .record(topic, &task.id, "send_failed", Some(&error))
                        .await;
                }
                Ok(DeliverOutcome::NotDone) => {}
                Err(e) => eprintln!("delivery error for {}: {e}", task.id),
            },
            Ok(Decision::Deny(_)) => {
                // Logged by check_and_log; retried next tick.
            }
            Err(e) => eprintln!("policy check error for {}: {e}", task.id),
        }
    }
}

struct ConnCtx {
    store: Arc<TaskStore>,
    guard: Arc<TriggerGuard>,
    triggers: Arc<Vec<WebhookTrigger>>,
    token: Arc<String>,
    audit_path: PathBuf,
    alert_primary: PathBuf,
    alert_fallback: PathBuf,
    #[cfg(feature = "scheduler")]
    scheduler: Option<Arc<warmachine::scheduler::Scheduler>>,
}

async fn handle_connection(mut socket: tokio::net::TcpStream, ctx: ConnCtx) {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    // Read until the headers are complete, then the body per
    // Content-Length. Requests here are tiny webhook payloads.
    let header_end = loop {
        match socket.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 65_536 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut token_header: Option<String> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "x-harness-token" => token_header = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length && body.len() < 1_048_576 {
        match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);

    let (code, payload) = route(&method, &path, token_header.as_deref(), &body, &ctx).await;
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = socket.write_all(response.as_bytes()).await;
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Deserialize)]
struct TriggerRequest {
    trigger: String,
}

async fn route(
    method: &str,
    path: &str,
    token_header: Option<&str>,
    body: &[u8],
    ctx: &ConnCtx,
) -> (u16, String) {
    if method == "GET" && path == "/health" {
        return (200, "{\"status\":\"ok\"}".to_string());
    }
    if method != "POST" || path != "/trigger" {
        return (404, "{\"error\":\"not found\"}".to_string());
    }
    if token_header != Some(ctx.token.as_str()) {
        return (401, "{\"error\":\"invalid token\"}".to_string());
    }
    let request: TriggerRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return (400, format!("{{\"error\":\"bad request: {e}\"}}")),
    };
    let Some(trigger) = ctx.triggers.iter().find(|t| t.name == request.trigger) else {
        return (
            404,
            format!("{{\"error\":\"unknown trigger {}\"}}", request.trigger),
        );
    };
    let Some(permit) = ctx.guard.try_acquire(&trigger.name) else {
        return (
            409,
            format!(
                "{{\"status\":\"already_running\",\"trigger\":\"{}\"}}",
                trigger.name
            ),
        );
    };
    // File the ledger task first so the response can name it.
    let task = match ctx
        .store
        .create(
            NewTask {
                title: format!("Webhook trigger: {}", trigger.name),
                owner: format!("trigger:{}", trigger.name),
                next_step: Some(format!("run recipe {}", trigger.recipe)),
                deadline: None,
            },
            "daemon",
        )
        .await
    {
        Ok(task) => task,
        Err(e) => return (400, format!("{{\"error\":\"ledger error: {e}\"}}")),
    };
    let _ = ctx.store.start(&task.id, "daemon").await;
    let _ = warmachine::harness::audit::append(
        &ctx.audit_path,
        "trigger_fire",
        "",
        serde_json::json!({"trigger": trigger.name, "task_id": task.id}),
    );
    let trigger = trigger.clone();
    #[cfg(feature = "scheduler")]
    let scheduler = ctx.scheduler.clone();
    let store = ctx.store.clone();
    let task_id = task.id.clone();
    let audit_path = ctx.audit_path.clone();
    let alert_primary = ctx.alert_primary.clone();
    let alert_fallback = ctx.alert_fallback.clone();
    tokio::spawn(async move {
        let _permit = permit;
        #[cfg(feature = "scheduler")]
        run_trigger(
            store,
            scheduler,
            trigger,
            task_id,
            audit_path,
            alert_primary,
            alert_fallback,
        )
        .await;
        #[cfg(not(feature = "scheduler"))]
        run_trigger(
            store,
            trigger,
            task_id,
            audit_path,
            alert_primary,
            alert_fallback,
        )
        .await;
    });
    (
        202,
        format!("{{\"status\":\"accepted\",\"task_id\":\"{}\"}}", task.id),
    )
}

/// Critical-job failure alert (AC-4.5): try the primary channel;
/// if it is down, fall back and log the fallback in the audit log.
async fn alert_critical_failure(
    trigger: &WebhookTrigger,
    task_id: &str,
    error: &str,
    audit_path: &std::path::Path,
    primary: &std::path::Path,
    fallback: &std::path::Path,
) {
    use warmachine::harness::DeliveryChannel;
    let now = chrono::Utc::now();
    let alert = Task {
        id: format!("alert:{task_id}"),
        title: format!("CRITICAL job failed: {}", trigger.name),
        owner: format!("trigger:{}", trigger.name),
        status: TaskStatus::Failed,
        next_step: None,
        deadline: None,
        result: Some(error.to_string()),
        created_at: now,
        updated_at: now,
    };
    let primary_channel = FileDeliveryChannel {
        path: primary.to_path_buf(),
    };
    match primary_channel.send(&alert).await {
        Ok(_) => {
            let _ = warmachine::harness::audit::append(
                audit_path,
                "alert_delivered",
                "",
                serde_json::json!({"task_id": task_id, "channel": "primary"}),
            );
        }
        Err(primary_error) => {
            let _ = warmachine::harness::audit::append(
                audit_path,
                "alert_primary_failed",
                "",
                serde_json::json!({"task_id": task_id, "error": primary_error.to_string()}),
            );
            let fallback_channel = FileDeliveryChannel {
                path: fallback.to_path_buf(),
            };
            match fallback_channel.send(&alert).await {
                Ok(_) => {
                    let _ = warmachine::harness::audit::append(
                        audit_path,
                        "alert_fallback_used",
                        "",
                        serde_json::json!({"task_id": task_id, "channel": "fallback"}),
                    );
                }
                Err(fallback_error) => {
                    eprintln!(
                        "critical alert for task {task_id} failed on both channels: {primary_error}; {fallback_error}"
                    );
                }
            }
        }
    }
}

#[cfg(feature = "scheduler")]
async fn run_trigger(
    store: Arc<TaskStore>,
    scheduler: Option<Arc<warmachine::scheduler::Scheduler>>,
    trigger: WebhookTrigger,
    task_id: String,
    audit_path: PathBuf,
    alert_primary: PathBuf,
    alert_fallback: PathBuf,
) {
    let Some(scheduler) = scheduler else {
        let error = "scheduler not available in this build";
        let _ = store.fail(&task_id, error, "daemon").await;
        if trigger.critical {
            alert_critical_failure(
                &trigger,
                &task_id,
                error,
                &audit_path,
                &alert_primary,
                &alert_fallback,
            )
            .await;
        }
        return;
    };
    let job_id = format!(
        "harness-{}-{}",
        trigger.name,
        task_id.get(..8).unwrap_or(&task_id)
    );
    let job = warmachine::scheduler::ScheduledJob {
        id: job_id.clone(),
        source: trigger.recipe.clone(),
        // Yearly placeholder: the job exists only to be run now and is
        // removed right after.
        cron: "0 0 0 1 1 *".to_string(),
        last_run: None,
        currently_running: false,
        paused: false,
        current_session_id: None,
        process_start_time: None,
        parameters: Vec::new(),
        recipe_base_dir: None,
    };
    let outcome = async {
        scheduler
            .add_scheduled_job(job, true)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let session = scheduler
            .run_now(&job_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        anyhow::Ok(session)
    }
    .await;
    let _ = scheduler.remove_scheduled_job(&job_id, true).await;
    match outcome {
        Ok(session) => {
            let _ = store
                .complete(
                    &task_id,
                    &format!("Recipe run finished (session {session})"),
                    "daemon",
                )
                .await;
        }
        Err(e) => {
            let _ = store.fail(&task_id, &e.to_string(), "daemon").await;
            if trigger.critical {
                alert_critical_failure(
                    &trigger,
                    &task_id,
                    &e.to_string(),
                    &audit_path,
                    &alert_primary,
                    &alert_fallback,
                )
                .await;
            }
        }
    }
}

#[cfg(not(feature = "scheduler"))]
async fn run_trigger(
    store: Arc<TaskStore>,
    trigger: WebhookTrigger,
    task_id: String,
    audit_path: PathBuf,
    alert_primary: PathBuf,
    alert_fallback: PathBuf,
) {
    let error = "scheduler not available in this build";
    let _ = store.fail(&task_id, error, "daemon").await;
    if trigger.critical {
        alert_critical_failure(
            &trigger,
            &task_id,
            error,
            &audit_path,
            &alert_primary,
            &alert_fallback,
        )
        .await;
    }
}

pub async fn daemon_status() -> Result<()> {
    let path = state_path();
    if !path.exists() {
        println!(
            "daemon is not running (no state file at {})",
            path.display()
        );
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    let state: DaemonState = serde_json::from_str(&text)?;
    let last_tick = chrono::DateTime::parse_from_rfc3339(&state.last_tick)
        .map(|t| t.with_timezone(&chrono::Utc));
    let fresh = last_tick
        .map(|t| chrono::Utc::now() - t < chrono::Duration::seconds(90))
        .unwrap_or(false);
    println!(
        "daemon {} (pid {}, started {})",
        if fresh {
            "running"
        } else {
            "STALE — heartbeat older than 90s; the process may have exited"
        },
        state.pid,
        state.started_at
    );
    println!("webhook: {}", state.webhook);
    println!(
        "triggers: {}",
        if state.triggers.is_empty() {
            "none".to_string()
        } else {
            state.triggers.join(", ")
        }
    );
    println!("scheduler jobs: {}", state.scheduler_jobs);
    println!(
        "tasks: {} open, {} failed, {} pending delivery",
        state.tasks_open, state.tasks_failed, state.tasks_pending_delivery
    );
    println!("deliveries file: {}", deliveries_path().display());
    Ok(())
}

pub async fn daemon_stop() -> Result<()> {
    let path = state_path();
    if !path.exists() {
        println!("daemon is not running (no state file)");
        return Ok(());
    }
    let state: DaemonState = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg(state.pid.to_string())
            .status()?;
        if status.success() {
            println!("sent SIGTERM to daemon pid {}", state.pid);
        } else {
            println!("could not signal pid {} (already exited?)", state.pid);
        }
    }
    #[cfg(not(unix))]
    {
        println!(
            "stop is only implemented on unix; terminate pid {} manually",
            state.pid
        );
    }
    Ok(())
}

pub fn daemon_unit() -> Result<()> {
    let unit_text = r#"[Unit]
Description=WarMachine harness daemon
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/warmachine daemon run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#;
    print!("{unit_text}");
    println!("# install: save as ~/.config/systemd/user/warmachine-daemon.service");
    println!(
        "# then: systemctl --user daemon-reload && systemctl --user enable --now warmachine-daemon"
    );
    Ok(())
}
