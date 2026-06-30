//! Canonical opener for miden-client store connections, and the regression test
//! for the proxy's `database is locked`.
//!
//! Root cause: `miden-client-sqlite-store`'s pool manager opens each pooled
//! connection with only `PRAGMA foreign_keys=ON` (its `new_connection`) and
//! leaves the database in the default **rollback-journal** mode. rusqlite
//! already applies a 5s `busy_timeout`, so the issue is NOT transient
//! write/write contention — it's that in rollback-journal mode a reader's
//! SHARED lock blocks a writer's COMMIT (which must take EXCLUSIVE). The proxy
//! reads the store from its Miden sync thread while the RPC claim path commits a
//! write on another pooled connection → the commit is blocked and eventually
//! returns SQLITE_BUSY ("database is locked"). `journal_mode=WAL` lets readers
//! and a single writer coexist, so the commit is never blocked by a reader.

use rusqlite::Connection;
use std::path::Path;

/// Open a miden-client store connection with the PRAGMAs required for safe
/// concurrent access from the proxy's multiple contexts. Mirrors what the store
/// crate's pool manager applies to every pooled connection.
pub fn open_store_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // Matches miden-client-sqlite-store `pool_manager::new_connection`.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

// ─────────────────────────────────────────────────────────────────────────────
// RD-1112 — prod tripwire for the sqlite "database is locked" contention.
//
// Every catch site that handles a miden-client store error is routed through
// `trace_store_locked`, which emits one log line prefixed with the stable token
// below and bumps `miden_store_locked_total{path}`. That gives ops a single
// prod query (Grafana/Loki: search `RD-1112`, or alert on the counter rate) for
// a regression that otherwise hides inside a generic
// "MidenClient::sync non-connection error" line. The WAL remediation in
// `open_store_connection` (once `journal_mode=WAL` is set) makes this path
// unreachable; the tag + counter stay as the canary.

/// Stable log prefix / search token for the sqlite "database is locked"
/// contention. Branch `fix/miden-store-db-lock`.
pub const STORE_LOCK_TAG: &str = "RD-1112";

/// True iff `err` renders like sqlite's "database is locked" condition
/// (SQLITE_BUSY / SQLITE_LOCKED). `miden-client-sqlite-store` surfaces it as
/// `StoreError::DatabaseError(..)`, which through `ClientError` / `anyhow`
/// renders as `"... database-related non-query error: database is locked"`.
/// Matched on the rendered chain (not a typed enum) because the store crate's
/// error is wrapped opaquely — by the time it reaches our catch sites it's an
/// `anyhow::Error` (which deliberately does not impl `std::error::Error`), so we
/// key off `Display` (`{:#}` prints anyhow's full cause chain).
pub fn is_store_locked<E: std::fmt::Display + ?Sized>(err: &E) -> bool {
    let rendered = format!("{err:#}");
    rendered.contains("database is locked")
        || rendered.contains("database-related non-query error")
        || rendered.contains("SQLITE_BUSY")
        || rendered.contains("SQLITE_LOCKED")
}

/// Log + meter a miden-client sqlite "database is locked" hit at `origin`
/// (e.g. `"miden_client::sync"`, `"wait_for_transaction_commit"`). No-op for
/// any other error, so callers can route every store error through it cheaply.
/// Returns whether the lock signature fired.
pub fn trace_store_locked<E: std::fmt::Display + ?Sized>(origin: &'static str, err: &E) -> bool {
    if !is_store_locked(err) {
        return false;
    }
    tracing::error!("{STORE_LOCK_TAG}: miden-client sqlite store locked at {origin}: {err:#}",);
    metrics::counter!("miden_store_locked_total", "path" => origin).increment(1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// RED: a writer committing while another connection holds an open read
    /// transaction must not be blocked into SQLITE_BUSY. Fails in
    /// rollback-journal mode; passes once `open_store_connection` sets
    /// `journal_mode=WAL`.
    #[test]
    fn writer_commit_not_blocked_by_concurrent_reader() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.sqlite3");
        open_store_connection(&db)
            .unwrap()
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();

        // The sync thread holds a SHARED lock via an open read transaction.
        let reader = open_store_connection(&db).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM t;")
            .unwrap();

        // The RPC claim path commits a write concurrently. Short busy_timeout so
        // the RED case fails in ~300ms instead of after the 5s default.
        let writer = open_store_connection(&db).unwrap();
        writer.busy_timeout(Duration::from_millis(300)).unwrap();
        let res = writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO t (v) VALUES (1); COMMIT;");

        res.expect(
            "writer COMMIT must not be blocked by a concurrent reader \
             (needs journal_mode=WAL)",
        );
    }
}
