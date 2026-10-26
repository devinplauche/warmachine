//! Credential vault (AC-5.1): connector secrets live in the OS
//! keychain, never in config files. Config only ever holds vault
//! account *names*.
//!
//! Unlike goose's provider-config secrets, the vault never falls
//! back to plaintext file storage: if the keychain is unavailable,
//! operations fail loudly. A silent plaintext fallback would defeat
//! the vault's entire contract.
//!
//! The keychain cannot be enumerated, so `set`/`delete` maintain a
//! metadata-only index (account names + timestamps) in harness.db
//! for `vault list`. The index never contains secret values.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

/// Keyring service name for all harness vault entries; the account
/// distinguishes individual secrets.
pub const VAULT_SERVICE: &str = "warmachine-harness";

pub trait Vault: Send + Sync {
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn delete(&self, account: &str) -> Result<bool>;
    fn backend(&self) -> &'static str;
}

/// The OS keychain (macOS Keychain, Windows Credential Manager,
/// Linux kernel keyutils / secret service via the keyring crate).
pub struct KeyringVault;

impl Vault for KeyringVault {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(VAULT_SERVICE, account).context("opening keychain entry")?;
        entry
            .set_password(secret)
            .with_context(|| format!("storing secret '{account}' in the OS keychain"))?;
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<String>> {
        let entry =
            keyring::Entry::new(VAULT_SERVICE, account).context("opening keychain entry")?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("reading secret '{account}' from the OS keychain"))
            }
        }
    }

    fn delete(&self, account: &str) -> Result<bool> {
        let entry =
            keyring::Entry::new(VAULT_SERVICE, account).context("opening keychain entry")?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => {
                Err(e).with_context(|| format!("deleting secret '{account}' from the OS keychain"))
            }
        }
    }

    fn backend(&self) -> &'static str {
        "os-keychain"
    }
}

/// In-memory vault for tests and dry runs. Never used implicitly.
#[derive(Default)]
pub struct MemoryVault {
    secrets: Mutex<HashMap<String, String>>,
}

impl Vault for MemoryVault {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.secrets
            .lock()
            .expect("memory vault poisoned")
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self
            .secrets
            .lock()
            .expect("memory vault poisoned")
            .get(account)
            .cloned())
    }

    fn delete(&self, account: &str) -> Result<bool> {
        Ok(self
            .secrets
            .lock()
            .expect("memory vault poisoned")
            .remove(account)
            .is_some())
    }

    fn backend(&self) -> &'static str {
        "memory"
    }
}

const INDEX_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS vault_index (
    account TEXT PRIMARY KEY,
    updated_at TEXT NOT NULL
);
";

/// Metadata-only index of vault accounts, in harness.db.
pub struct VaultIndex {
    pool: SqlitePool,
}

impl VaultIndex {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating vault index dir {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .with_context(|| format!("opening vault index {}", path.display()))?;
        sqlx::query(INDEX_SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query(INDEX_SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn record_set(&self, account: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO vault_index (account, updated_at) VALUES (?, ?) \
             ON CONFLICT(account) DO UPDATE SET updated_at = excluded.updated_at",
        )
        .bind(account)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_delete(&self, account: &str) -> Result<()> {
        sqlx::query("DELETE FROM vault_index WHERE account = ?")
            .bind(account)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT account FROM vault_index ORDER BY account")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(a,)| a).collect())
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}
