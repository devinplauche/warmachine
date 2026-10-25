//! Durable task ledger (AC-2): first-class tasks in the same
//! `harness.db` as the memory store, with an append-only event log and
//! guarded state transitions.
//!
//! Closing conditions (AC-2.5) are enforced by the transition table, not
//! by callers: a task reaches a terminal state only by delivery of its
//! result, explicit completion confirmation, or user cancellation —
//! never by timeout or worker failure. A failure moves the task to the
//! visible `failed` state and leaves it open.
//!
//! Delivery is exactly-once in normal operation (AC-2.4): claiming a
//! completed task for delivery is one atomic status update, so
//! re-running or racing the delivery path cannot send twice. If the
//! channel send fails after the claim, the claim is reverted and the
//! failure is logged, so the result is not lost.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    InProgress,
    Failed,
    /// Outcome recorded, result awaiting delivery.
    Done,
    /// Terminal: result delivered to the user's channel.
    Delivered,
    /// Terminal: cancelled by the user.
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Failed => "failed",
            TaskStatus::Done => "done",
            TaskStatus::Delivered => "delivered",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Delivered | TaskStatus::Cancelled)
    }

    /// The AC-2.5 transition table, enforced for every status change.
    fn can_transition_to(&self, next: TaskStatus) -> bool {
        use TaskStatus::*;
        if self.is_terminal() || *self == next {
            return false;
        }
        matches!(
            (self, next),
            (Open, InProgress)
                | (Open, Failed)
                | (InProgress, Failed)
                | (Failed, InProgress)
                | (Failed, Open)
                | (Open, Cancelled)
                | (InProgress, Cancelled)
                | (Failed, Cancelled)
                | (Done, Cancelled)
        )
    }
}

impl FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "open" => Ok(TaskStatus::Open),
            "in_progress" | "in-progress" => Ok(TaskStatus::InProgress),
            "failed" => Ok(TaskStatus::Failed),
            "done" => Ok(TaskStatus::Done),
            "delivered" => Ok(TaskStatus::Delivered),
            "cancelled" | "canceled" => Ok(TaskStatus::Cancelled),
            other => Err(anyhow!("unknown task status: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    /// Who owns the work: a session, a scheduled job, the CLI.
    pub owner: String,
    pub status: TaskStatus,
    pub next_step: Option<String>,
    pub deadline: Option<DateTime<Utc>>,
    /// The completion outcome, set when the task is completed.
    pub result: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    pub title: String,
    pub owner: String,
    pub next_step: Option<String>,
    pub deadline: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub event: String,
    pub detail: Option<String>,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    owner TEXT NOT NULL,
    status TEXT NOT NULL,
    next_step TEXT,
    deadline TEXT,
    result TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE TABLE IF NOT EXISTS task_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    at TEXT NOT NULL,
    actor TEXT NOT NULL,
    event TEXT NOT NULL,
    detail TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_events_task ON task_events(task_id, seq);
";

pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger db dir {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening ledger db {}", path.display()))?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    async fn log(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        task_id: &str,
        actor: &str,
        event: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_events (task_id, at, actor, event, detail) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(Utc::now().to_rfc3339())
        .bind(actor)
        .bind(event)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn create(&self, new: NewTask, actor: &str) -> Result<Task> {
        let now = Utc::now();
        let task = Task {
            id: Uuid::new_v4().to_string(),
            title: new.title,
            owner: new.owner,
            status: TaskStatus::Open,
            next_step: new.next_step,
            deadline: new.deadline,
            result: None,
            created_at: now,
            updated_at: now,
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO tasks (id, title, owner, status, next_step, deadline, result, created_at, updated_at)
             VALUES (?, ?, ?, 'open', ?, ?, NULL, ?, ?)",
        )
        .bind(&task.id)
        .bind(&task.title)
        .bind(&task.owner)
        .bind(&task.next_step)
        .bind(task.deadline.map(|d| d.to_rfc3339()))
        .bind(task.created_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        Self::log(&mut tx, &task.id, actor, "created", None).await?;
        tx.commit().await?;
        Ok(task)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        let row = sqlx::query(
            "SELECT id, title, owner, status, next_step, deadline, result, created_at, updated_at
             FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| task_from_row(&r)).transpose()
    }

    pub async fn list(
        &self,
        status: Option<TaskStatus>,
        owner: Option<&str>,
        overdue_only: bool,
    ) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT id, title, owner, status, next_step, deadline, result, created_at, updated_at
             FROM tasks
             WHERE (? IS NULL OR status = ?)
               AND (? IS NULL OR owner = ?)
               AND (? = 0 OR (deadline IS NOT NULL AND deadline < ? AND status IN ('open', 'in_progress', 'failed')))
             ORDER BY COALESCE(deadline, created_at), created_at",
        )
        .bind(status.map(|s| s.as_str()))
        .bind(status.map(|s| s.as_str()))
        .bind(owner)
        .bind(owner)
        .bind(overdue_only as i64)
        .bind(Utc::now().to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(task_from_row).collect()
    }

    /// Patch mutable fields on a non-terminal task.
    pub async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        next_step: Option<&str>,
        deadline: Option<DateTime<Utc>>,
        actor: &str,
    ) -> Result<Task> {
        let task = self.get(id).await?.ok_or_else(|| anyhow!("no task {id}"))?;
        if task.status.is_terminal() {
            return Err(anyhow!(
                "task {id} is {} and cannot change",
                task.status.as_str()
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE tasks SET title = COALESCE(?, title), next_step = COALESCE(?, next_step),
             deadline = COALESCE(?, deadline), updated_at = ? WHERE id = ?",
        )
        .bind(title)
        .bind(next_step)
        .bind(deadline.map(|d| d.to_rfc3339()))
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        Self::log(&mut tx, id, actor, "updated", None).await?;
        tx.commit().await?;
        self.get(id).await?.ok_or_else(|| anyhow!("task vanished"))
    }

    async fn transition(
        &self,
        id: &str,
        next: TaskStatus,
        event: &str,
        detail: Option<&str>,
        actor: &str,
    ) -> Result<Task> {
        let task = self.get(id).await?.ok_or_else(|| anyhow!("no task {id}"))?;
        if !task.status.can_transition_to(next) {
            return Err(anyhow!(
                "task {id} cannot move from {} to {}",
                task.status.as_str(),
                next.as_str()
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE tasks SET status = ?, updated_at = ? WHERE id = ?")
            .bind(next.as_str())
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        Self::log(&mut tx, id, actor, event, detail).await?;
        tx.commit().await?;
        self.get(id).await?.ok_or_else(|| anyhow!("task vanished"))
    }

    pub async fn start(&self, id: &str, actor: &str) -> Result<Task> {
        self.transition(id, TaskStatus::InProgress, "started", None, actor)
            .await
    }

    /// Record a failure. The task stays open in the failed state
    /// (AC-2.5); it never auto-closes.
    pub async fn fail(&self, id: &str, reason: &str, actor: &str) -> Result<Task> {
        self.transition(id, TaskStatus::Failed, "failed", Some(reason), actor)
            .await
    }

    pub async fn cancel(&self, id: &str, actor: &str) -> Result<Task> {
        self.transition(id, TaskStatus::Cancelled, "cancelled", None, actor)
            .await
    }

    pub async fn add_note(&self, id: &str, note: &str, actor: &str) -> Result<()> {
        let task = self.get(id).await?.ok_or_else(|| anyhow!("no task {id}"))?;
        if task.status.is_terminal() {
            return Err(anyhow!("task {id} is closed"));
        }
        let mut tx = self.pool.begin().await?;
        Self::log(&mut tx, id, actor, "note", Some(note)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record the outcome. The task is done but not closed: it closes
    /// when its result is delivered (or the user cancels it).
    pub async fn complete(&self, id: &str, result: &str, actor: &str) -> Result<Task> {
        let task = self.get(id).await?.ok_or_else(|| anyhow!("no task {id}"))?;
        if task.status.is_terminal() || task.status == TaskStatus::Done {
            return Err(anyhow!(
                "task {id} is {} and cannot be completed",
                task.status.as_str()
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE tasks SET status = 'done', result = ?, updated_at = ? WHERE id = ?")
            .bind(result)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        Self::log(&mut tx, id, actor, "completed", None).await?;
        tx.commit().await?;
        self.get(id).await?.ok_or_else(|| anyhow!("task vanished"))
    }

    pub async fn history(&self, id: &str) -> Result<Vec<TaskEvent>> {
        let rows = sqlx::query(
            "SELECT task_id, at, actor, event, detail FROM task_events WHERE task_id = ? ORDER BY seq",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let at: String = row.try_get("at")?;
                Ok(TaskEvent {
                    task_id: row.try_get("task_id")?,
                    at: DateTime::parse_from_rfc3339(&at)?.with_timezone(&Utc),
                    actor: row.try_get("actor")?,
                    event: row.try_get("event")?,
                    detail: row.try_get("detail")?,
                })
            })
            .collect()
    }

    /// Deliver every `done` task's result through the channel,
    /// exactly once each (AC-2.3, AC-2.4).
    pub async fn deliver_pending(
        &self,
        channel: &dyn DeliveryChannel,
        actor: &str,
    ) -> Result<DeliveryReport> {
        let mut report = DeliveryReport::default();
        for task in self.list(Some(TaskStatus::Done), None, false).await? {
            // Atomic claim: exactly one deliverer can win this update.
            let claimed = sqlx::query(
                "UPDATE tasks SET status = 'delivered', updated_at = ? WHERE id = ? AND status = 'done'",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(&task.id)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if claimed == 0 {
                continue;
            }
            let mut tx = self.pool.begin().await?;
            Self::log(
                &mut tx,
                &task.id,
                actor,
                "delivery_claimed",
                Some(channel.name()),
            )
            .await?;
            tx.commit().await?;

            match channel.send(&task).await {
                Ok(receipt) => {
                    let mut tx = self.pool.begin().await?;
                    Self::log(&mut tx, &task.id, actor, "delivered", Some(&receipt)).await?;
                    tx.commit().await?;
                    report.delivered.push(task.id.clone());
                }
                Err(e) => {
                    // Revert the claim so the result is not lost; the
                    // task returns to done and can be delivered later.
                    sqlx::query("UPDATE tasks SET status = 'done', updated_at = ? WHERE id = ?")
                        .bind(Utc::now().to_rfc3339())
                        .bind(&task.id)
                        .execute(&self.pool)
                        .await?;
                    let mut tx = self.pool.begin().await?;
                    Self::log(
                        &mut tx,
                        &task.id,
                        actor,
                        "delivery_failed",
                        Some(&e.to_string()),
                    )
                    .await?;
                    tx.commit().await?;
                    report.failed.push((task.id.clone(), e.to_string()));
                }
            }
        }
        Ok(report)
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

fn task_from_row(row: &SqliteRow) -> Result<Task> {
    let deadline: Option<String> = row.try_get("deadline")?;
    let created: String = row.try_get("created_at")?;
    let updated: String = row.try_get("updated_at")?;
    Ok(Task {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        owner: row.try_get("owner")?,
        status: TaskStatus::from_str(&row.try_get::<String, _>("status")?)?,
        next_step: row.try_get("next_step")?,
        deadline: deadline
            .map(|d| DateTime::parse_from_rfc3339(&d).map(|p| p.with_timezone(&Utc)))
            .transpose()?,
        result: row.try_get("result")?,
        created_at: DateTime::parse_from_rfc3339(&created)?.with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&updated)?.with_timezone(&Utc),
    })
}

#[derive(Debug, Default)]
pub struct DeliveryReport {
    pub delivered: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Where completed task results go. The daemon and gateway channels
/// implement this in later phases; the ledger only requires the
/// contract plus the receipt string for the event log.
#[async_trait]
pub trait DeliveryChannel: Send + Sync {
    fn name(&self) -> &str;
    /// Send one task's result; returns a receipt reference recorded in
    /// the task's history.
    async fn send(&self, task: &Task) -> Result<String>;
}

/// Appends results to a local file — the always-available channel for
/// headless use, and the delivery target for Phase 2's end-to-end demo.
pub struct FileDeliveryChannel {
    pub path: PathBuf,
}

#[async_trait]
impl DeliveryChannel for FileDeliveryChannel {
    fn name(&self) -> &str {
        "file"
    }

    async fn send(&self, task: &Task) -> Result<String> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(
            file,
            "## Task delivered: {}\nowner: {} | completed task {}\n\n{}\n",
            task.title,
            task.owner,
            task.id,
            task.result.as_deref().unwrap_or_default()
        )?;
        Ok(format!("file:{}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CountingChannel {
        sent: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl DeliveryChannel for CountingChannel {
        fn name(&self) -> &str {
            "counting"
        }

        async fn send(&self, task: &Task) -> Result<String> {
            if self.fail {
                return Err(anyhow!("channel down"));
            }
            self.sent.lock().unwrap().push(task.id.clone());
            Ok("receipt-1".to_string())
        }
    }

    fn new_task(title: &str) -> NewTask {
        NewTask {
            title: title.to_string(),
            owner: "scheduler:test".to_string(),
            next_step: None,
            deadline: None,
        }
    }

    #[tokio::test]
    async fn tasks_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("harness-ledger-test-{}", Uuid::new_v4()));
        let path = dir.join("harness.db");
        let store = TaskStore::open(&path).await.unwrap();
        let task = store.create(new_task("scan bids"), "test").await.unwrap();
        store.close().await;
        let reopened = TaskStore::open(&path).await.unwrap();
        let listed = reopened.list(None, None, false).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, task.id);
        assert_eq!(listed[0].status, TaskStatus::Open);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn transition_guards_enforce_closing_conditions() {
        let store = TaskStore::open_in_memory().await.unwrap();
        let task = store.create(new_task("t"), "test").await.unwrap();
        // Open cannot jump straight to done via the generic table;
        // completion has its own path.
        assert!(store.cancel(&task.id, "test").await.is_ok());
        // Terminal states are immutable (AC-2.5).
        assert!(store.start(&task.id, "test").await.is_err());
        assert!(store.fail(&task.id, "x", "test").await.is_err());
        assert!(store.complete(&task.id, "r", "test").await.is_err());
    }

    #[tokio::test]
    async fn failure_leaves_task_open_and_visible() {
        let store = TaskStore::open_in_memory().await.unwrap();
        let task = store.create(new_task("flaky job"), "test").await.unwrap();
        store.start(&task.id, "worker").await.unwrap();
        let failed = store
            .fail(&task.id, "network timeout", "worker")
            .await
            .unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        assert!(!failed.status.is_terminal());
        // Still listed, still retryable.
        assert_eq!(
            store
                .list(Some(TaskStatus::Failed), None, false)
                .await
                .unwrap()
                .len(),
            1
        );
        let retried = store.start(&task.id, "worker").await.unwrap();
        assert_eq!(retried.status, TaskStatus::InProgress);
        let history = store.history(&task.id).await.unwrap();
        let events: Vec<&str> = history.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(events, ["created", "started", "failed", "started"]);
    }

    #[tokio::test]
    async fn delivery_is_exactly_once() {
        let store = TaskStore::open_in_memory().await.unwrap();
        let channel = CountingChannel {
            sent: Mutex::new(Vec::new()),
            fail: false,
        };
        let task = store.create(new_task("report"), "test").await.unwrap();
        store
            .complete(&task.id, "the result", "worker")
            .await
            .unwrap();

        let first = store.deliver_pending(&channel, "daemon").await.unwrap();
        assert_eq!(first.delivered, vec![task.id.clone()]);
        // Re-running the delivery path sends nothing again (AC-2.4).
        let second = store.deliver_pending(&channel, "daemon").await.unwrap();
        assert!(second.delivered.is_empty());
        assert_eq!(channel.sent.lock().unwrap().len(), 1);
        assert_eq!(
            store.get(&task.id).await.unwrap().unwrap().status,
            TaskStatus::Delivered
        );
    }

    #[tokio::test]
    async fn failed_delivery_reverts_claim() {
        let store = TaskStore::open_in_memory().await.unwrap();
        let down = CountingChannel {
            sent: Mutex::new(Vec::new()),
            fail: true,
        };
        let task = store.create(new_task("report"), "test").await.unwrap();
        store
            .complete(&task.id, "the result", "worker")
            .await
            .unwrap();
        let report = store.deliver_pending(&down, "daemon").await.unwrap();
        assert_eq!(report.failed.len(), 1);
        // Claim reverted: still done, deliverable later.
        assert_eq!(
            store.get(&task.id).await.unwrap().unwrap().status,
            TaskStatus::Done
        );
        let up = CountingChannel {
            sent: Mutex::new(Vec::new()),
            fail: false,
        };
        let retry = store.deliver_pending(&up, "daemon").await.unwrap();
        assert_eq!(retry.delivered.len(), 1);
        let events: Vec<String> = store
            .history(&task.id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.event.clone())
            .collect();
        assert!(
            events.contains(&"delivery_failed".to_string()),
            "{events:?}"
        );
        assert!(events.contains(&"delivered".to_string()), "{events:?}");
    }

    #[tokio::test]
    async fn overdue_filter() {
        let store = TaskStore::open_in_memory().await.unwrap();
        let mut overdue = new_task("old");
        overdue.deadline = Some(Utc::now() - chrono::Duration::days(1));
        store.create(overdue, "test").await.unwrap();
        store.create(new_task("fresh"), "test").await.unwrap();
        let listed = store.list(None, None, true).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "old");
    }
}
