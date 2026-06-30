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
        reader.execute_batch("BEGIN; SELECT COUNT(*) FROM t;").unwrap();

        // The RPC claim path commits a write concurrently. Short busy_timeout so
        // the RED case fails in ~300ms instead of after the 5s default.
        let writer = open_store_connection(&db).unwrap();
        writer.busy_timeout(Duration::from_millis(300)).unwrap();
        let res =
            writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO t (v) VALUES (1); COMMIT;");

        res.expect(
            "writer COMMIT must not be blocked by a concurrent reader \
             (needs journal_mode=WAL)",
        );
    }
}
