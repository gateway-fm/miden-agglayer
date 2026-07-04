//! Canonical opener for direct miden-client sqlite access.
//!
//! The upstream sqlite store enables foreign keys on pooled connections but
//! otherwise leaves the database in rollback-journal mode.
//!
//! EXPERIMENT CONCLUDED (2026-07-04): with WAL removed and a true singleton
//! `MidenClient` enforced, the ISOLATED topology (external B2AGG wallet on its
//! own store — prod shape) ran the full 10/25/50/250 ladder with ZERO locks:
//! in-process access is already serialized by the actor. But the legacy
//! shared-store path (e2e-l2-to-l1.sh runs bridge-out-tool against the
//! proxy's store) immediately failed its post-submit store update in
//! rollback-journal mode. Verdict: keep BOTH — the singleton guard (sole
//! in-process owner) AND persistent WAL (readers never block a writer's
//! COMMIT across processes).

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
