//! Canonical opener for direct miden-client sqlite access.
//!
//! The upstream sqlite store enables foreign keys on pooled connections but
//! otherwise leaves the database in rollback-journal mode.
//!
//! POLICY (2026-07-04, experiment concluded): the proxy's store has exactly
//! ONE owner — the in-process singleton `MidenClient` (see the guard in
//! `crate::miden_client`). Cross-process sharing of `store.sqlite3` is
//! UNSUPPORTED: external clients (bridge-out tooling, e2e scripts) must run
//! against their OWN store (`bridge-out-tool --create-wallet`), matching the
//! production topology. Under that contract the full 10/25/50/250 ladder ran
//! with ZERO locks and no WAL. WAL is deliberately NOT set here — a lock
//! observed in rollback-journal mode is a loud signal that something violated
//! the single-owner contract, which WAL would paper over.

use rusqlite::Connection;
use std::path::Path;

pub fn open_store_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Documents WHY cross-process sharing is unsupported: in rollback-journal
    // mode a concurrent reader blocks a writer's COMMIT. Ignored by policy —
    // the single-owner contract makes the scenario illegal rather than fixed.
    #[test]
    #[ignore = "cross-process store sharing is unsupported by policy (single-owner contract)"]
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
