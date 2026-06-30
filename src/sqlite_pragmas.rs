//! Canonical opener for direct miden-client sqlite access.
//!
//! The upstream sqlite store enables foreign keys on pooled connections but
//! otherwise leaves the database in rollback-journal mode. This service and
//! its helper binaries can open the same store from multiple contexts, where a
//! reader can block a writer's commit long enough to surface SQLITE_BUSY. WAL
//! is persistent for the database file and lets readers coexist with the single
//! sqlite writer.

use rusqlite::Connection;
use std::path::Path;

pub fn open_store_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn writer_commit_not_blocked_by_concurrent_reader() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.sqlite3");
        open_store_connection(&db)
            .unwrap()
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();

        let reader = open_store_connection(&db).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM t;")
            .unwrap();

        let writer = open_store_connection(&db).unwrap();
        writer.busy_timeout(Duration::from_millis(300)).unwrap();
        writer
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO t (v) VALUES (1); COMMIT;")
            .expect("writer COMMIT must not be blocked by a concurrent reader");
    }
}
