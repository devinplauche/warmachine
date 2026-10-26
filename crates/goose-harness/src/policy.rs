//! Delivery policy engine (AC-3.3): every proactive outbound message
//! is checked against the user's delivery config before it is sent.
//! Denied items are logged with their reason and never sent.
//!
//! Checks, in order: master switch, quiet hours (local time),
//! per-topic opt-outs, and dedupe against recent deliveries recorded
//! in the delivery log (same `harness.db`).

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use crate::config::DeliveryConfig;

/// How far back the dedupe check looks for an identical dedupe key.
pub const DEDUPE_WINDOW_HOURS: i64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    Disabled,
    QuietHours,
    TopicOptedOut,
    Duplicate,
}

impl DenyReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DenyReason::Disabled => "delivery disabled",
            DenyReason::QuietHours => "quiet hours",
            DenyReason::TopicOptedOut => "topic opted out",
            DenyReason::Duplicate => "duplicate of a recent delivery",
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

pub struct DeliveryPolicy {
    config: DeliveryConfig,
}

impl DeliveryPolicy {
    pub fn new(config: DeliveryConfig) -> Self {
        Self { config }
    }

    /// Pure evaluation. `recently_delivered` is the dedupe input,
    /// queried from the delivery log by the caller-facing check.
    pub fn evaluate(
        &self,
        topic: Option<&str>,
        now_local: DateTime<Local>,
        recently_delivered: bool,
    ) -> Decision {
        if !self.config.enabled {
            return Decision::Deny(DenyReason::Disabled);
        }
        if self.in_quiet_hours(now_local) {
            return Decision::Deny(DenyReason::QuietHours);
        }
        if let Some(topic) = topic {
            if self.config.topic_opt_outs.iter().any(|t| t == topic) {
                return Decision::Deny(DenyReason::TopicOptedOut);
            }
        }
        if recently_delivered {
            return Decision::Deny(DenyReason::Duplicate);
        }
        Decision::Allow
    }

    fn in_quiet_hours(&self, now_local: DateTime<Local>) -> bool {
        let (Some(start), Some(end)) = (
            self.config.quiet_hours_start.as_deref(),
            self.config.quiet_hours_end.as_deref(),
        ) else {
            return false;
        };
        let (Ok(start), Ok(end)) = (parse_hhmm(start), parse_hhmm(end)) else {
            return false;
        };
        let time = now_local.time();
        if start <= end {
            time >= start && time < end
        } else {
            // Window wraps midnight (the default 22:00–07:00).
            time >= start || time < end
        }
    }

    /// Evaluate against the live clock and the delivery log, and
    /// record the outcome either way (AC-3.3 denial logging).
    pub async fn check_and_log(
        &self,
        log: &DeliveryLog,
        topic: Option<&str>,
        dedupe_key: &str,
    ) -> Result<Decision> {
        let recent = log
            .was_delivered_recently(dedupe_key, DEDUPE_WINDOW_HOURS)
            .await?;
        let decision = self.evaluate(topic, Local::now(), recent);
        let (decision_text, reason) = match decision {
            Decision::Allow => ("allow", None),
            Decision::Deny(reason) => ("deny", Some(reason.as_str())),
        };
        log.record(topic, dedupe_key, decision_text, reason).await?;
        Ok(decision)
    }
}

fn parse_hhmm(text: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(text, "%H:%M")
        .map_err(|e| anyhow::anyhow!("invalid HH:MM time {text}: {e}"))
}

const LOG_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS delivery_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    topic TEXT,
    dedupe_key TEXT NOT NULL,
    decision TEXT NOT NULL,
    reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_delivery_log_key ON delivery_log(dedupe_key, seq);
";

/// Append-only record of policy decisions in `harness.db`.
pub struct DeliveryLog {
    pool: SqlitePool,
}

impl DeliveryLog {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating delivery log dir {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .with_context(|| format!("opening delivery log {}", path.display()))?;
        sqlx::query(LOG_SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query(LOG_SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn record(
        &self,
        topic: Option<&str>,
        dedupe_key: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO delivery_log (at, topic, dedupe_key, decision, reason) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(topic)
        .bind(dedupe_key)
        .bind(decision)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Was this dedupe key delivered within the window? Judged by the
    /// latest log entry for the key: an `allow` counts, a later
    /// `send_failed` supersedes it (the send did not happen, so a
    /// retry is not a duplicate). Denials never count — a held
    /// message may go out later.
    pub async fn was_delivered_recently(
        &self,
        dedupe_key: &str,
        within_hours: i64,
    ) -> Result<bool> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT decision, at FROM delivery_log WHERE dedupe_key = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(dedupe_key)
        .fetch_optional(&self.pool)
        .await?;
        let Some((decision, at)) = row else {
            return Ok(false);
        };
        if decision != "allow" {
            return Ok(false);
        }
        let at = DateTime::parse_from_rfc3339(&at)?.with_timezone(&Utc);
        Ok(Utc::now() - at < chrono::Duration::hours(within_hours))
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn policy() -> DeliveryPolicy {
        DeliveryPolicy::new(DeliveryConfig::default())
    }

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 25, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn quiet_hours_deny_inside_window() {
        let p = policy();
        // Default window 22:00-07:00 wraps midnight.
        assert_eq!(
            p.evaluate(None, at(23, 30), false),
            Decision::Deny(DenyReason::QuietHours)
        );
        assert_eq!(
            p.evaluate(None, at(2, 0), false),
            Decision::Deny(DenyReason::QuietHours)
        );
        assert_eq!(
            p.evaluate(None, at(6, 59), false),
            Decision::Deny(DenyReason::QuietHours)
        );
        assert_eq!(p.evaluate(None, at(7, 0), false), Decision::Allow);
        assert_eq!(p.evaluate(None, at(12, 0), false), Decision::Allow);
        assert_eq!(p.evaluate(None, at(21, 59), false), Decision::Allow);
    }

    #[test]
    fn disabled_denies_everything() {
        let p = DeliveryPolicy::new(DeliveryConfig {
            enabled: false,
            ..Default::default()
        });
        assert_eq!(
            p.evaluate(None, at(12, 0), false),
            Decision::Deny(DenyReason::Disabled)
        );
    }

    #[test]
    fn opted_out_topic_denied() {
        let p = DeliveryPolicy::new(DeliveryConfig {
            topic_opt_outs: vec!["sports".to_string()],
            ..Default::default()
        });
        assert_eq!(
            p.evaluate(Some("sports"), at(12, 0), false),
            Decision::Deny(DenyReason::TopicOptedOut)
        );
        assert_eq!(p.evaluate(Some("bids"), at(12, 0), false), Decision::Allow);
        assert_eq!(p.evaluate(None, at(12, 0), false), Decision::Allow);
    }

    #[test]
    fn recent_delivery_denied_as_duplicate() {
        let p = policy();
        assert_eq!(
            p.evaluate(None, at(12, 0), true),
            Decision::Deny(DenyReason::Duplicate)
        );
    }

    #[tokio::test]
    async fn check_and_log_records_decisions_and_dedupes() {
        // No quiet hours in this config: the test must be wall-clock
        // independent.
        let p = DeliveryPolicy::new(DeliveryConfig {
            quiet_hours_start: None,
            quiet_hours_end: None,
            ..Default::default()
        });
        let log = DeliveryLog::open_in_memory().await.unwrap();

        let first = p.check_and_log(&log, Some("bids"), "task-1").await.unwrap();
        assert_eq!(first, Decision::Allow);
        // Same key again inside the window: duplicate.
        let second = p.check_and_log(&log, Some("bids"), "task-1").await.unwrap();
        assert_eq!(second, Decision::Deny(DenyReason::Duplicate));
        // A send failure supersedes the allow; retry is not a duplicate.
        log.record(Some("bids"), "task-1", "send_failed", None)
            .await
            .unwrap();
        let third = p.check_and_log(&log, Some("bids"), "task-1").await.unwrap();
        assert_eq!(third, Decision::Allow);
        // A different key is unaffected.
        let other = p.check_and_log(&log, Some("bids"), "task-2").await.unwrap();
        assert_eq!(other, Decision::Allow);
    }
}
