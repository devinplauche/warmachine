//! Durable approval queue (AC-4.1, AC-4.2).
//!
//! Approval-required actions in non-interactive runs are queued here,
//! in `harness.db`, not held in any process's memory: the requesting
//! run may be a short-lived scheduled job, and the user answers from
//! whatever surface is convenient (CLI today, gateway later). An
//! unanswered request expires into a denial — default-deny on
//! timeout — and the expiry is itself recorded on the request.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }

    /// Expired counts as denied: default-deny on timeout (AC-4.2).
    pub fn is_denial(&self) -> bool {
        matches!(self, ApprovalStatus::Denied | ApprovalStatus::Expired)
    }
}

impl FromStr for ApprovalStatus {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "pending" => Ok(ApprovalStatus::Pending),
            "approved" => Ok(ApprovalStatus::Approved),
            "denied" => Ok(ApprovalStatus::Denied),
            "expired" => Ok(ApprovalStatus::Expired),
            other => anyhow::bail!("unknown approval status '{other}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action: String,
    pub action_class: String,
    pub origin: String,
    pub args_json: String,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    pub note: Option<String>,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS approval_requests (
    id TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    action_class TEXT NOT NULL,
    origin TEXT NOT NULL,
    args_json TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    decided_at TEXT,
    decided_by TEXT,
    note TEXT
);
CREATE INDEX IF NOT EXISTS idx_approvals_status ON approval_requests(status);
";

pub struct ApprovalStore {
    pool: SqlitePool,
}

type ApprovalRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn row_to_request(row: ApprovalRow) -> Result<ApprovalRequest> {
    let parse = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .map(|dt| dt.with_timezone(&Utc))
            .with_context(|| format!("bad timestamp {text}"))
    };
    Ok(ApprovalRequest {
        id: row.0,
        action: row.1,
        action_class: row.2,
        origin: row.3,
        args_json: row.4,
        status: ApprovalStatus::from_str(&row.5)?,
        created_at: parse(&row.6)?,
        expires_at: parse(&row.7)?,
        decided_at: row.8.as_deref().map(parse).transpose()?,
        decided_by: row.9,
        note: row.10,
    })
}

impl ApprovalStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating approval store dir {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening approval store {}", path.display()))?;
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

    pub async fn create(
        &self,
        action: &str,
        action_class: &str,
        origin: &str,
        args_json: &str,
        ttl_minutes: u64,
    ) -> Result<ApprovalRequest> {
        let now = Utc::now();
        let request = ApprovalRequest {
            id: uuid::Uuid::new_v4().to_string(),
            action: action.to_string(),
            action_class: action_class.to_string(),
            origin: origin.to_string(),
            args_json: args_json.to_string(),
            status: ApprovalStatus::Pending,
            created_at: now,
            expires_at: now + chrono::Duration::minutes(ttl_minutes as i64),
            decided_at: None,
            decided_by: None,
            note: None,
        };
        sqlx::query(
            "INSERT INTO approval_requests (id, action, action_class, origin, args_json, status, created_at, expires_at) VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(&request.id)
        .bind(&request.action)
        .bind(&request.action_class)
        .bind(&request.origin)
        .bind(&request.args_json)
        .bind(request.created_at.to_rfc3339())
        .bind(request.expires_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(request)
    }

    /// Fetch one request, applying lazy expiry: a pending request past
    /// its deadline is recorded as expired (denied) on read, so no
    /// sweeper is required for correctness.
    pub async fn get(&self, id: &str) -> Result<Option<ApprovalRequest>> {
        let row: Option<ApprovalRow> = sqlx::query_as(
            "SELECT id, action, action_class, origin, args_json, status, created_at, expires_at, decided_at, decided_by, note FROM approval_requests WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let request = row_to_request(row)?;
        if request.status == ApprovalStatus::Pending && Utc::now() > request.expires_at {
            sqlx::query(
                "UPDATE approval_requests SET status = 'expired' WHERE id = ? AND status = 'pending'",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
            let mut expired = request;
            expired.status = ApprovalStatus::Expired;
            return Ok(Some(expired));
        }
        Ok(Some(request))
    }

    pub async fn list(&self, status: Option<ApprovalStatus>) -> Result<Vec<ApprovalRequest>> {
        // Sweep first so listings never show a past-deadline request
        // as still pending.
        self.expire_due().await?;
        let rows: Vec<ApprovalRow> = match status {
            Some(status) => {
                sqlx::query_as(
                    "SELECT id, action, action_class, origin, args_json, status, created_at, expires_at, decided_at, decided_by, note FROM approval_requests WHERE status = ? ORDER BY created_at",
                )
                .bind(status.as_str())
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT id, action, action_class, origin, args_json, status, created_at, expires_at, decided_at, decided_by, note FROM approval_requests ORDER BY created_at",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(row_to_request).collect()
    }

    /// Record the user's decision. Only a live pending request can be
    /// decided; deciding an expired or already-decided request is an
    /// error, so a late approval can never resurrect a denied action.
    pub async fn decide(
        &self,
        id: &str,
        approved: bool,
        decided_by: &str,
        note: Option<&str>,
    ) -> Result<ApprovalRequest> {
        let request = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no approval request {id}"))?;
        if request.status != ApprovalStatus::Pending {
            anyhow::bail!(
                "approval request {id} is already {} and cannot be decided",
                request.status.as_str()
            );
        }
        let new_status = if approved { "approved" } else { "denied" };
        let changed = sqlx::query(
            "UPDATE approval_requests SET status = ?, decided_at = ?, decided_by = ?, note = ? WHERE id = ? AND status = 'pending'",
        )
        .bind(new_status)
        .bind(Utc::now().to_rfc3339())
        .bind(decided_by)
        .bind(note)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed == 0 {
            anyhow::bail!("approval request {id} was decided concurrently");
        }
        self.get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("approval request {id} vanished"))
    }

    /// Expire every due pending request; returns the expired ids.
    /// Used by the daemon's tick so expiries are surfaced (and
    /// audited) even when nobody polls the request.
    pub async fn expire_due(&self) -> Result<Vec<String>> {
        let now = Utc::now().to_rfc3339();
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM approval_requests WHERE status = 'pending' AND expires_at < ?",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await?;
        let mut expired = Vec::new();
        for (id,) in rows {
            let changed = sqlx::query(
                "UPDATE approval_requests SET status = 'expired' WHERE id = ? AND status = 'pending'",
            )
            .bind(&id)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if changed == 1 {
                expired.push(id);
            }
        }
        Ok(expired)
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_and_terminal_immutability() {
        let store = ApprovalStore::open_in_memory().await.unwrap();
        let request = store
            .create("send_email", "approval_required", "session:abc", "{}", 60)
            .await
            .unwrap();
        assert_eq!(request.status, ApprovalStatus::Pending);

        let decided = store
            .decide(&request.id, true, "cli", Some("looks fine"))
            .await
            .unwrap();
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decided_by.as_deref(), Some("cli"));

        // A decided request cannot be decided again.
        assert!(store.decide(&request.id, false, "cli", None).await.is_err());
    }

    #[tokio::test]
    async fn expiry_is_default_deny() {
        let store = ApprovalStore::open_in_memory().await.unwrap();
        // TTL of 0 minutes: already expired at creation.
        let request = store
            .create("spend_money", "approval_required", "daemon", "{}", 0)
            .await
            .unwrap();
        let fetched = store.get(&request.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, ApprovalStatus::Expired);
        assert!(fetched.status.is_denial());
        // And a late approval cannot resurrect it.
        assert!(store.decide(&request.id, true, "cli", None).await.is_err());
    }

    #[tokio::test]
    async fn expire_due_sweeps_pending() {
        let store = ApprovalStore::open_in_memory().await.unwrap();
        let stale = store
            .create("a", "approval_required", "t", "{}", 0)
            .await
            .unwrap();
        let fresh = store
            .create("b", "approval_required", "t", "{}", 60)
            .await
            .unwrap();
        let expired = store.expire_due().await.unwrap();
        assert_eq!(expired, vec![stale.id.clone()]);
        let fresh_after = store.get(&fresh.id).await.unwrap().unwrap();
        assert_eq!(fresh_after.status, ApprovalStatus::Pending);
    }
}
