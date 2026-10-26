//! Persistent memory store (AC-1): typed records in a dedicated SQLite
//! database (`harness.db`, design decision D1), with provenance,
//! supersession, a secret-excluding write policy, and a token-budgeted
//! brief for session start.
//!
//! Record lifecycle: a record is written either as **active** (trusted
//! write paths: the user's CLI, the agent's memory tool) or as a
//! **candidate** (the consolidation pass, AC-1.6). Candidates are never
//! returned by briefs or searches until the user approves them. Writing
//! an active record whose (kind, subject) matches an existing active
//! record supersedes it: the old record stays in the table with status
//! `superseded` and is never returned by retrieval again (AC-1.4).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Person,
    Decision,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Person => "person",
            MemoryKind::Decision => "decision",
        }
    }

    /// Brief ordering: standing decisions first, then preferences,
    /// people, and plain facts.
    fn brief_rank(&self) -> u8 {
        match self {
            MemoryKind::Decision => 0,
            MemoryKind::Preference => 1,
            MemoryKind::Person => 2,
            MemoryKind::Fact => 3,
        }
    }
}

impl FromStr for MemoryKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fact" => Ok(MemoryKind::Fact),
            "preference" => Ok(MemoryKind::Preference),
            "person" => Ok(MemoryKind::Person),
            "decision" => Ok(MemoryKind::Decision),
            other => Err(anyhow!("unknown memory kind: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Candidate,
    Active,
    Superseded,
}

impl RecordStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordStatus::Candidate => "candidate",
            RecordStatus::Active => "active",
            RecordStatus::Superseded => "superseded",
        }
    }
}

impl FromStr for RecordStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "candidate" => Ok(RecordStatus::Candidate),
            "active" => Ok(RecordStatus::Active),
            "superseded" => Ok(RecordStatus::Superseded),
            other => Err(anyhow!("unknown record status: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub kind: MemoryKind,
    pub subject: Option<String>,
    pub content: String,
    /// Who or what wrote the record: a session id, the CLI, or the
    /// consolidation pass (AC-1.3 attribution).
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub status: RecordStatus,
    /// The record this one replaced, if any (AC-1.4 trail).
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRecord {
    pub kind: MemoryKind,
    pub subject: Option<String>,
    pub content: String,
    pub source: String,
}

/// The write policy every path into the durable store passes through
/// (AC-1.6): durable records only, and never credentials or secrets.
/// Deterministic pattern checks — no model judgment — so the seeded
/// secret test is exact.
pub struct WritePolicy;

impl WritePolicy {
    pub fn check(content: &str) -> Result<()> {
        let lower = content.to_ascii_lowercase();
        if lower.contains("-----begin") && lower.contains("private key") {
            return Err(anyhow!(
                "write policy: private key material is never stored"
            ));
        }
        if contains_aws_access_key_id(content) {
            return Err(anyhow!("write policy: cloud access key id detected"));
        }
        if lower.contains("bearer ") && content.len() > 24 {
            return Err(anyhow!("write policy: bearer token detected"));
        }
        for key in [
            "password",
            "passwd",
            "api_key",
            "apikey",
            "api-key",
            "secret",
            "access_token",
            "credential",
        ] {
            if let Some(idx) = lower.find(key) {
                let Some(rest) = content.get(idx + key.len()..) else {
                    continue;
                };
                let rest = rest.trim_start();
                if let Some(value) = rest.strip_prefix([':', '=']) {
                    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                    let placeholder = value.starts_with('<')
                        || value.eq_ignore_ascii_case("redacted")
                        || value.chars().all(|c| c == 'x' || c == '*');
                    if value.len() >= 4 && !placeholder {
                        return Err(anyhow!(
                            "write policy: credential assignment ({key}) is never stored"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn contains_aws_access_key_id(content: &str) -> bool {
    let bytes = content.as_bytes();
    for window in bytes.windows(20) {
        if &window[..4] == b"AKIA"
            && window[4..]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS memory_records (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    subject TEXT,
    content TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    status TEXT NOT NULL,
    supersedes TEXT
);
CREATE INDEX IF NOT EXISTS idx_memory_status ON memory_records(status);
CREATE INDEX IF NOT EXISTS idx_memory_kind_subject ON memory_records(kind, subject);
";

/// The durable store. One `harness.db` in the WarMachine config dir,
/// shared by the memory service now and the task ledger in Phase 2.
pub struct MemoryStore {
    pool: SqlitePool,
}

impl MemoryStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating memory db dir {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // Full sync: a kill -9 mid-write must not lose a committed
            // record (AC-1.5).
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening memory db {}", path.display()))?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// In-memory store for tests.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Write a record. Active writes supersede the currently active
    /// record with the same (kind, subject); candidates never supersede
    /// anything until approved.
    pub async fn write(&self, new: NewRecord, as_candidate: bool) -> Result<MemoryRecord> {
        WritePolicy::check(&new.content)?;
        let record = MemoryRecord {
            id: Uuid::new_v4().to_string(),
            kind: new.kind,
            subject: new.subject.clone(),
            content: new.content.clone(),
            source: new.source.clone(),
            created_at: Utc::now(),
            status: if as_candidate {
                RecordStatus::Candidate
            } else {
                RecordStatus::Active
            },
            supersedes: None,
        };
        let mut tx = self.pool.begin().await?;
        let mut record = record;
        // Supersession keys on (kind, subject); subjectless records are
        // independent statements and never supersede each other.
        if !as_candidate && record.subject.is_some() {
            if let Some(old) = self
                .active_for(&mut tx, record.kind, record.subject.as_deref())
                .await?
            {
                sqlx::query("UPDATE memory_records SET status = 'superseded' WHERE id = ?")
                    .bind(&old.id)
                    .execute(&mut *tx)
                    .await?;
                record.supersedes = Some(old.id);
            }
        }
        sqlx::query(
            "INSERT INTO memory_records (id, kind, subject, content, source, created_at, status, supersedes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(record.kind.as_str())
        .bind(&record.subject)
        .bind(&record.content)
        .bind(&record.source)
        .bind(record.created_at.to_rfc3339())
        .bind(record.status.as_str())
        .bind(&record.supersedes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(record)
    }

    async fn active_for(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        kind: MemoryKind,
        subject: Option<&str>,
    ) -> Result<Option<MemoryRecord>> {
        let row = sqlx::query(
            "SELECT id, kind, subject, content, source, created_at, status, supersedes
             FROM memory_records
             WHERE kind = ? AND subject IS ? AND status = 'active'
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(kind.as_str())
        .bind(subject)
        .fetch_optional(&mut **tx)
        .await?;
        row.map(|r| record_from_row(&r)).transpose()
    }

    /// Promote a candidate to active, applying normal supersession.
    pub async fn approve(&self, id: &str) -> Result<MemoryRecord> {
        let record = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("no memory record {id}"))?;
        if record.status != RecordStatus::Candidate {
            return Err(anyhow!("record {id} is not a candidate"));
        }
        let mut tx = self.pool.begin().await?;
        let mut supersedes = None;
        if record.subject.is_some() {
            if let Some(old) = self
                .active_for(&mut tx, record.kind, record.subject.as_deref())
                .await?
            {
                sqlx::query("UPDATE memory_records SET status = 'superseded' WHERE id = ?")
                    .bind(&old.id)
                    .execute(&mut *tx)
                    .await?;
                supersedes = Some(old.id);
            }
        }
        sqlx::query("UPDATE memory_records SET status = 'active', supersedes = ? WHERE id = ?")
            .bind(&supersedes)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.get(id)
            .await?
            .ok_or_else(|| anyhow!("record vanished"))
    }

    /// Correct a record: write its replacement and supersede it
    /// explicitly (AC-1.7), regardless of subject matching.
    pub async fn correct(&self, id: &str, new_content: &str, source: &str) -> Result<MemoryRecord> {
        WritePolicy::check(new_content)?;
        let old = self
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("no memory record {id}"))?;
        let record = MemoryRecord {
            id: Uuid::new_v4().to_string(),
            kind: old.kind,
            subject: old.subject.clone(),
            content: new_content.to_string(),
            source: source.to_string(),
            created_at: Utc::now(),
            status: RecordStatus::Active,
            supersedes: Some(old.id.clone()),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE memory_records SET status = 'superseded' WHERE id = ?")
            .bind(&old.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO memory_records (id, kind, subject, content, source, created_at, status, supersedes)
             VALUES (?, ?, ?, ?, ?, ?, 'active', ?)",
        )
        .bind(&record.id)
        .bind(record.kind.as_str())
        .bind(&record.subject)
        .bind(&record.content)
        .bind(&record.source)
        .bind(record.created_at.to_rfc3339())
        .bind(&record.supersedes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(record)
    }

    /// Hard delete: the record is gone from every retrieval path
    /// (AC-1.7). Supersession trails in other records keep the id as a
    /// dangling reference, which is history, not retrieval.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM memory_records WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, id: &str) -> Result<Option<MemoryRecord>> {
        let row = sqlx::query(
            "SELECT id, kind, subject, content, source, created_at, status, supersedes
             FROM memory_records WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| record_from_row(&r)).transpose()
    }

    pub async fn list(
        &self,
        status: Option<RecordStatus>,
        kind: Option<MemoryKind>,
    ) -> Result<Vec<MemoryRecord>> {
        let rows = sqlx::query(
            "SELECT id, kind, subject, content, source, created_at, status, supersedes
             FROM memory_records
             WHERE (? IS NULL OR status = ?) AND (? IS NULL OR kind = ?)
             ORDER BY created_at",
        )
        .bind(status.map(|s| s.as_str()))
        .bind(status.map(|s| s.as_str()))
        .bind(kind.map(|k| k.as_str()))
        .bind(kind.map(|k| k.as_str()))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(record_from_row).collect()
    }

    /// Search active records by substring over subject and content.
    /// Superseded and candidate records are never returned (AC-1.4,
    /// AC-1.6).
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryRecord>> {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let rows = sqlx::query(
            "SELECT id, kind, subject, content, source, created_at, status, supersedes
             FROM memory_records
             WHERE status = 'active' AND (content LIKE ? ESCAPE '\\' OR subject LIKE ? ESCAPE '\\')
             ORDER BY created_at",
        )
        .bind(&pattern)
        .bind(&pattern)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(record_from_row).collect()
    }

    /// The session-start brief (AC-1.2): active records, decisions
    /// first, truncated by whole records at the token budget.
    pub async fn build_brief(&self, max_tokens: usize) -> Result<String> {
        let mut records = self.list(Some(RecordStatus::Active), None).await?;
        records.sort_by_key(|r| r.kind.brief_rank());
        let mut lines = vec!["## Long-term memory".to_string()];
        let mut used = estimate_tokens(&lines[0]);
        let mut omitted = 0;
        for record in &records {
            let line = match &record.subject {
                Some(subject) => format!(
                    "- [{}] {}: {}",
                    record.kind.as_str(),
                    subject,
                    record.content
                ),
                None => format!("- [{}] {}", record.kind.as_str(), record.content),
            };
            let cost = estimate_tokens(&line);
            if used + cost > max_tokens {
                omitted += 1;
                continue;
            }
            used += cost;
            lines.push(line);
        }
        if omitted > 0 {
            lines.push(format!(
                "- ({omitted} more records omitted by the brief token budget)"
            ));
        }
        if records.is_empty() {
            return Ok(String::new());
        }
        Ok(lines.join("\n"))
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

fn estimate_tokens(text: &str) -> usize {
    // Rough planning heuristic only; the budget is a guardrail against
    // unbounded prompt growth, not an accounting figure.
    text.chars().count() / 4 + 1
}

fn record_from_row(row: &SqliteRow) -> Result<MemoryRecord> {
    let created: String = row.try_get("created_at")?;
    Ok(MemoryRecord {
        id: row.try_get("id")?,
        kind: MemoryKind::from_str(&row.try_get::<String, _>("kind")?)?,
        subject: row.try_get("subject")?,
        content: row.try_get("content")?,
        source: row.try_get("source")?,
        created_at: DateTime::parse_from_rfc3339(&created)?.with_timezone(&Utc),
        status: RecordStatus::from_str(&row.try_get::<String, _>("status")?)?,
        supersedes: row.try_get("supersedes")?,
    })
}

/// One extracted candidate plus the policy rejections from a
/// consolidation run (AC-1.6).
#[derive(Debug, Default)]
pub struct ConsolidationReport {
    pub stored: Vec<MemoryRecord>,
    pub rejected: Vec<String>,
}

/// Extract durable statements from a transcript. The consolidation
/// contract: the agent marks durable statements inline as
/// `Fact: ...`, `Preference: ...`, `Person: ...`, or `Decision: ...`
/// lines; everything else in the transcript is conversation, not
/// memory. An optional `Kind[subject]: ...` form sets the subject used
/// for supersession.
pub fn extract_candidates(transcript: &str, source: &str) -> Vec<NewRecord> {
    let mut out = Vec::new();
    for raw_line in transcript.lines() {
        let line = raw_line.trim().trim_start_matches(['-', '*', ' ']);
        for (prefix, kind) in [
            ("fact", MemoryKind::Fact),
            ("preference", MemoryKind::Preference),
            ("person", MemoryKind::Person),
            ("decision", MemoryKind::Decision),
        ] {
            let Some(rest) = strip_prefix_ci(line, prefix) else {
                continue;
            };
            let rest = rest.trim_start();
            let (subject, content) = match (rest.starts_with('['), rest.find("]:")) {
                (true, Some(bracket_end)) => (
                    rest.get(1..bracket_end).map(|s| s.trim().to_string()),
                    rest.get(bracket_end + 2..)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                ),
                _ => (None, rest.trim_start_matches(':').trim().to_string()),
            };
            if !content.is_empty() {
                out.push(NewRecord {
                    kind,
                    subject,
                    content,
                    source: source.to_string(),
                });
            }
            break;
        }
    }
    out
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        line.get(prefix.len()..)
    } else {
        None
    }
}

/// Run a consolidation pass: extract candidates, gate each through the
/// write policy, and store the survivors as candidates — never active
/// (AC-1.6). Policy failures are reported, not stored.
pub async fn consolidate(
    store: &MemoryStore,
    transcript: &str,
    source: &str,
) -> Result<ConsolidationReport> {
    let mut report = ConsolidationReport::default();
    for candidate in extract_candidates(transcript, source) {
        match store.write(candidate, true).await {
            Ok(record) => report.stored.push(record),
            Err(e) => report.rejected.push(e.to_string()),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(kind: MemoryKind, subject: Option<&str>, content: &str) -> NewRecord {
        NewRecord {
            kind,
            subject: subject.map(str::to_string),
            content: content.to_string(),
            source: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn write_search_and_brief_roundtrip() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        store
            .write(
                new(MemoryKind::Fact, Some("wife"), "Lily is a PhD candidate"),
                false,
            )
            .await
            .unwrap();
        let hits = store.search("PhD").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].status, RecordStatus::Active);
        let brief = store.build_brief(2000).await.unwrap();
        assert!(brief.contains("Lily is a PhD candidate"), "{brief}");
    }

    #[tokio::test]
    async fn same_subject_write_supersedes() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        let old = store
            .write(
                new(MemoryKind::Fact, Some("timezone"), "user is in Chicago"),
                false,
            )
            .await
            .unwrap();
        let new = store
            .write(
                new(MemoryKind::Fact, Some("timezone"), "user is in Detroit"),
                false,
            )
            .await
            .unwrap();
        assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
        let old_now = store.get(&old.id).await.unwrap().unwrap();
        assert_eq!(old_now.status, RecordStatus::Superseded);
        // The superseded value is never returned again (AC-1.4).
        assert!(store.search("Chicago").await.unwrap().is_empty());
        assert_eq!(store.search("Detroit").await.unwrap().len(), 1);
        let brief = store.build_brief(2000).await.unwrap();
        assert!(brief.contains("Detroit") && !brief.contains("Chicago"));
    }

    #[tokio::test]
    async fn subjectless_writes_do_not_supersede() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        store
            .write(new(MemoryKind::Fact, None, "one"), false)
            .await
            .unwrap();
        store
            .write(new(MemoryKind::Fact, None, "two"), false)
            .await
            .unwrap();
        assert_eq!(
            store
                .list(Some(RecordStatus::Active), None)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn write_policy_rejects_secrets() {
        // Assembled at runtime: the contiguous literal would trip
        // secret scanners in the toolchain itself.
        let aws_key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let bad_cases = [
            format!("the key is {aws_key}"),
            "his password: hunter2hunter2".to_string(),
            "api_key = sk-live-1234567890".to_string(),
            "-----BEGIN RSA PRIVATE KEY-----".to_string(),
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz".to_string(),
        ];
        for bad in &bad_cases {
            assert!(WritePolicy::check(bad).is_err(), "should reject: {bad}");
        }
        for good in [
            "user prefers dark mode",
            "the password policy requires 12 chars",
            "api_key rotation is handled by IT",
        ] {
            assert!(WritePolicy::check(good).is_ok(), "should accept: {good}");
        }
    }

    #[tokio::test]
    async fn consolidation_excludes_secrets_and_holds_candidates() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        let aws_key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let transcript = format!(
            "\
We talked about a lot of things today.
Fact: the deploy window is Friday nights
Fact: staging uses {aws_key} for access
Preference: brief answers over long ones
Just chatter with no marker."
        );
        let report = consolidate(&store, &transcript, "consolidation:test")
            .await
            .unwrap();
        assert_eq!(report.stored.len(), 2);
        assert_eq!(report.rejected.len(), 1, "{:?}", report.rejected);
        // Nothing from the transcript is active yet (AC-1.6).
        assert!(store.search("deploy window").await.unwrap().is_empty());
        assert_eq!(store.build_brief(2000).await.unwrap(), "");
        // The seeded secret is nowhere in the store.
        let all = store.list(None, None).await.unwrap();
        assert!(all.iter().all(|r| !r.content.contains("AKIA")));
        // Approval promotes a candidate.
        let deploy = report
            .stored
            .iter()
            .find(|r| r.content.contains("deploy window"))
            .unwrap();
        store.approve(&deploy.id).await.unwrap();
        assert_eq!(store.search("deploy window").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn correct_and_delete() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        let rec = store
            .write(new(MemoryKind::Preference, None, "likes tea"), false)
            .await
            .unwrap();
        let fixed = store
            .correct(&rec.id, "likes coffee", "test")
            .await
            .unwrap();
        assert_eq!(fixed.supersedes.as_deref(), Some(rec.id.as_str()));
        assert!(store.search("tea").await.unwrap().is_empty());
        assert!(store.delete(&fixed.id).await.unwrap());
        assert!(store.get(&fixed.id).await.unwrap().is_none());
        assert!(store.search("coffee").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn records_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("harness-mem-test-{}", Uuid::new_v4()));
        let path = dir.join("harness.db");
        let store = MemoryStore::open(&path).await.unwrap();
        store
            .write(
                new(MemoryKind::Decision, Some("storage"), "SQLite via sqlx"),
                false,
            )
            .await
            .unwrap();
        store.close().await;
        let reopened = MemoryStore::open(&path).await.unwrap();
        let hits = reopened.search("SQLite").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, MemoryKind::Decision);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn brief_respects_token_budget() {
        let store = MemoryStore::open_in_memory().await.unwrap();
        for i in 0..20 {
            store
                .write(
                    new(
                        MemoryKind::Fact,
                        None,
                        &format!("fact number {i} with some extra words to spend tokens"),
                    ),
                    false,
                )
                .await
                .unwrap();
        }
        let brief = store.build_brief(40).await.unwrap();
        assert!(
            brief.contains("omitted by the brief token budget"),
            "{brief}"
        );
        assert!(brief.len() < 400, "brief should stay near budget: {brief}");
    }
}
