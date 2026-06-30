# DB-lock WIP — handoff (`fix/miden-store-db-lock`, off `v0.15.1`)

**Status:** RED reproduced (unit); RED e2e in progress; GREEN (WAL fix) **not yet applied**.

## The bug
The proxy opens the miden-client sqlite store (`<store-dir>/store.sqlite3`) from **two of its own contexts** — the Miden **sync thread** (`MidenClient` loop, `sync_state()`) and the **RPC claim path** (`src/service_send_raw_txn.rs`). The store runs in default **rollback-journal** mode: `miden-client-sqlite-store`'s pool manager sets only `foreign_keys=ON`, and rusqlite's default 5s `busy_timeout` applies. In rollback-journal mode a reader's SHARED lock blocks a writer's COMMIT (which needs EXCLUSIVE), so under concurrent load the claim-path COMMIT is blocked by the sync reader and eventually returns `SQLITE_BUSY` → `database is locked`. No external process is involved.

> NOTE: prod's `database is locked` was the **Postgres** store — a *separate* issue. This branch is the **sqlite** one. The PRST-4035 bridge-out recovery (now shipped as **v0.15.2**, PR #96; merge-back PR #97) is unrelated to this work.

## RED (done)
- **Unit** — `src/sqlite_pragmas.rs::writer_commit_not_blocked_by_concurrent_reader`: reader holds `BEGIN; SELECT`, writer does `BEGIN IMMEDIATE; INSERT; COMMIT` w/ 300ms busy_timeout. RED in rollback-journal, GREEN with WAL.
  Run: `cargo test --lib sqlite_pragmas` (currently **fails = RED**, as intended).
- **E2E** — `scripts/e2e-bridge-loadtest.sh` (N=250 PARALLEL=5) on a fresh stack drives concurrent bridge load (parallel-batched L1→L2 deposits + sequential L2→L1) to reproduce the runtime lock. Watch: `docker logs miden-agglayer-miden-agglayer-1 2>&1 | grep -c "database is locked"`.
  (The plain `e2e-fuzz-bridge.sh` does NOT reproduce it — its concurrency is read-only.)

## GREEN (TODO — the fix)
Open the store once in **WAL mode** at startup, before the miden-client builder opens it (WAL is persistent → the pool's connections inherit it):
1. `src/sqlite_pragmas.rs::open_store_connection` — add `conn.pragma_update(None, "journal_mode", "WAL")?;` next to `foreign_keys=ON`. → flips the unit test GREEN.
2. `src/miden_client.rs` `MidenClient::new` — the store is opened via `ClientBuilder` + `ClientBuilderSqliteExt` (line ~9), path `<store_dir>/store.sqlite3`. Call `crate::sqlite_pragmas::open_store_connection(<store_dir>/store.sqlite3)` **before** the builder runs, to set WAL persistently (then drop the connection).
3. Verify: `cargo test --lib sqlite_pragmas` (GREEN) **and** re-run the loadtest (0 `database is locked`).

## Continue on another machine
```bash
git fetch && git checkout fix/miden-store-db-lock
make e2e-up                                          # fresh 0.15.1 stack (node v0.15.0, ~10-20 min build)
N=250 PARALLEL=5 ./scripts/e2e-bridge-loadtest.sh    # RED: should produce "database is locked" in proxy logs
# --- apply the GREEN fix above, then:
make e2e-up                                          # rebuild proxy image with the fix
N=250 PARALLEL=5 ./scripts/e2e-bridge-loadtest.sh    # GREEN: 0 lock lines
cargo test --lib sqlite_pragmas                      # unit RED -> GREEN
```
Then commit the WAL fix, open a PR to `release/0.15` (same base as PR #96).
