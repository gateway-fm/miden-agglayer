//! Recovery helpers for when the miden-client local state diverges from the node.
//!
//! Two modes are exposed:
//!
//! 1. **Full reset** (`reset_miden_store`) — the "big hammer". Deletes
//!    `store.sqlite3` (and its WAL/SHM sidecars) so that the next startup
//!    rebuilds an empty sqlite DB and re-syncs everything from the node. The
//!    keystore (private keys) and `bridge_accounts.toml` (on-chain account IDs)
//!    are preserved — wiping either would permanently lose control of the
//!    on-chain accounts.
//!
//! 2. **Surgical unlock** (`unlock_miden_accounts`) — clears the `locked`
//!    column on every row in miden-client's `latest_account_headers` and
//!    `historical_account_headers` tables. Use when the only symptom is a
//!    stale lock flag (see `detect_locked_accounts`) and a full resync would
//!    be overkill.
//!
//! Surgical unlock reaches directly into miden-client's sqlite schema because
//! miden-client v0.14.x does not expose a public unlock API. The column names
//! come from `crates/sqlite-store/src/store.sql` in miden-client.

use anyhow::{Context, Result};
use miden_client::ClientError;
use miden_protocol::account::AccountId;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::accounts_config::AccountsConfig;
use crate::miden_client::MidenClient;

const SQLITE_FILES: &[&str] = &["store.sqlite3", "store.sqlite3-wal", "store.sqlite3-shm"];

/// Resolve the sqlite path miden-client uses inside `store_dir`.
pub fn sqlite_path(store_dir: &Path) -> PathBuf {
    store_dir.join("store.sqlite3")
}

/// Big hammer: delete `store.sqlite3` (and its WAL/SHM sidecars) from
/// `store_dir`. Keystore and `bridge_accounts.toml` are preserved.
///
/// No-op for files that do not exist. Returns the number of files deleted.
pub fn reset_miden_store(store_dir: &Path) -> Result<usize> {
    let mut removed = 0;
    for name in SQLITE_FILES {
        let path = store_dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!("reset_miden_store: deleted {}", path.display());
                removed += 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("deleting {}", path.display()));
            }
        }
    }
    Ok(removed)
}

/// Surgical unlock: open the miden-client sqlite store directly and clear the
/// `locked` flag on every tracked account row. Returns the total number of
/// rows updated across both header tables.
///
/// Intended for the case where the proxy's local state has a stale lock but
/// the on-chain account is actually fine — cheaper than a full resync. If the
/// sqlite file does not exist, returns 0.
pub fn unlock_miden_accounts(store_dir: &Path) -> Result<usize> {
    let db_path = sqlite_path(store_dir);
    if !db_path.exists() {
        return Ok(0);
    }
    let conn =
        Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let mut total = 0;
    let mut missing_tables = 0;
    for table in ["latest_account_headers", "historical_account_headers"] {
        let sql = format!("UPDATE {table} SET locked = 0 WHERE locked = 1");
        match conn.execute(&sql, []) {
            Ok(n) => {
                if n > 0 {
                    tracing::info!("unlock_miden_accounts: cleared {n} rows in {table}");
                }
                total += n;
            }
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("no such table") || msg.contains("no such column") =>
            {
                tracing::warn!(
                    "unlock_miden_accounts: skipping {table} (schema mismatch: {msg}); \
                     miden-client may have changed its schema"
                );
                missing_tables += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("updating {table}"));
            }
        }
    }
    // C10 — fail loud if EVERY known table was missing. The previous
    // implementation silently returned 0, masking a miden-client schema
    // upgrade that would leave operators thinking the unlock succeeded
    // when it actually no-op'd. Now we return Err so the operator knows
    // surgical-unlock is no longer effective on this miden-client version
    // and must use `--reset-miden-store` instead.
    if missing_tables == 2 {
        anyhow::bail!(
            "unlock_miden_accounts: every known account-header table is missing — \
             miden-client schema has changed. Use --reset-miden-store and re-sync \
             from the node, or pin miden-client to a compatible version."
        );
    }
    Ok(total)
}

/// After initial sync, query the miden-client for the lock status of every
/// account listed in `accounts`. Returns the subset that are currently locked.
///
/// The proxy will refuse to process work against a locked account, so surfacing
/// this at startup is more actionable than letting the first tx submission fail
/// with `transaction conflicts with current mempool state`.
pub async fn detect_locked_accounts(
    client: &MidenClient,
    accounts: &AccountsConfig,
) -> Result<Vec<AccountId>> {
    let mut ids: Vec<AccountId> = vec![accounts.service.0, accounts.bridge.0];
    if let Some(f) = &accounts.faucet_eth {
        ids.push(f.0);
    }
    if let Some(f) = &accounts.faucet_agg {
        ids.push(f.0);
    }
    if let Some(g) = &accounts.ger_manager {
        ids.push(g.0);
    }

    let mut locked = Vec::new();
    for id in ids {
        let result = Arc::new(OnceLock::<bool>::new());
        let result_set = result.clone();
        client
            .with(move |c| {
                Box::new(async move {
                    let reader = c.account_reader(id);
                    let is_locked = match reader.status().await {
                        Ok(status) => status.is_locked(),
                        // An account the client doesn't track cannot be locked.
                        Err(ClientError::AccountDataNotFound(_)) => false,
                        Err(err) => return Err(err.into()),
                    };
                    let _ = result_set.set(is_locked);
                    Ok(())
                })
            })
            .await?;
        if result.get().copied().unwrap_or(false) {
            locked.push(id);
        }
    }
    Ok(locked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reset_miden_store_removes_sqlite_files_and_keeps_others() {
        let dir = tempdir().unwrap();
        let p = dir.path();

        fs::write(p.join("store.sqlite3"), b"x").unwrap();
        fs::write(p.join("store.sqlite3-wal"), b"x").unwrap();
        fs::write(p.join("store.sqlite3-shm"), b"x").unwrap();
        fs::write(p.join("bridge_accounts.toml"), b"x").unwrap();
        fs::create_dir_all(p.join("keystore")).unwrap();
        fs::write(p.join("keystore").join("key.json"), b"x").unwrap();

        let removed = reset_miden_store(p).unwrap();
        assert_eq!(removed, 3);
        assert!(!p.join("store.sqlite3").exists());
        assert!(!p.join("store.sqlite3-wal").exists());
        assert!(!p.join("store.sqlite3-shm").exists());
        assert!(p.join("bridge_accounts.toml").exists());
        assert!(p.join("keystore").join("key.json").exists());
    }

    #[test]
    fn reset_miden_store_is_idempotent() {
        let dir = tempdir().unwrap();
        assert_eq!(reset_miden_store(dir.path()).unwrap(), 0);
        assert_eq!(reset_miden_store(dir.path()).unwrap(), 0);
    }

    #[test]
    fn unlock_miden_accounts_is_noop_when_no_sqlite() {
        let dir = tempdir().unwrap();
        assert_eq!(unlock_miden_accounts(dir.path()).unwrap(), 0);
    }

    #[test]
    fn unlock_miden_accounts_clears_locked_rows() {
        let dir = tempdir().unwrap();
        let db_path = sqlite_path(dir.path());
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE latest_account_headers (id INTEGER PRIMARY KEY, locked BOOLEAN NOT NULL);
             CREATE TABLE historical_account_headers (id INTEGER PRIMARY KEY, locked BOOLEAN NOT NULL);
             INSERT INTO latest_account_headers (id, locked) VALUES (1, 1), (2, 0), (3, 1);
             INSERT INTO historical_account_headers (id, locked) VALUES (1, 1), (2, 1);",
        )
        .unwrap();
        drop(conn);

        let n = unlock_miden_accounts(dir.path()).unwrap();
        assert_eq!(n, 4);

        let conn = Connection::open(&db_path).unwrap();
        let still_locked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM latest_account_headers WHERE locked = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_locked, 0);
    }

    /// Self-review C10 — repro+regression. Pre-fix when EVERY known
    /// account-header table was missing (full schema drift), the function
    /// returned `Ok(0)` and the operator thought the surgical-unlock had
    /// succeeded. The miden-client schema can change between releases and
    /// the previous behaviour silently failed. Now the function fails
    /// loud so operators see the schema mismatch and switch to
    /// `--reset-miden-store`.
    #[test]
    fn c10_unlock_miden_accounts_fails_loud_on_total_schema_drift() {
        let dir = tempdir().unwrap();
        let db_path = sqlite_path(dir.path());
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (x INTEGER);")
            .unwrap();
        drop(conn);

        let err = unlock_miden_accounts(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("schema has changed"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("--reset-miden-store"),
            "error must point operators at the recovery flag: {msg}"
        );
    }

    /// One-table-missing partial drift still succeeds at clearing rows
    /// in the surviving table (no false-positive C10 fail-loud).
    #[test]
    fn c10_unlock_miden_accounts_succeeds_with_partial_schema_drift() {
        let dir = tempdir().unwrap();
        let db_path = sqlite_path(dir.path());
        let conn = Connection::open(&db_path).unwrap();
        // Create only one of the two known tables, with a single locked row.
        conn.execute_batch(
            "CREATE TABLE latest_account_headers (id INTEGER, locked INTEGER);
             INSERT INTO latest_account_headers VALUES (1, 1);",
        )
        .unwrap();
        drop(conn);

        let n = unlock_miden_accounts(dir.path()).unwrap();
        assert_eq!(n, 1, "should clear the row in the surviving table");
    }
}
