//! Integration tests for PgStore.
//!
//! Requires:
//! - `--features postgres`
//! - `DATABASE_URL` env var pointing to a PostgreSQL instance
//! - The schema from `migrations/001_initial.sql` applied
//!
//! Run with:
//!   DATABASE_URL=postgres://... cargo test --features postgres pgstore

use super::postgres::PgStore;
use super::{Store, TxnEntry};
use crate::log_synthesis::{
    AddressFilter, GerEntry, LogFilter, SyntheticLog, UPDATE_HASH_CHAIN_VALUE_TOPIC,
};
use alloy::consensus::{TxEip1559, TxEnvelope};
use alloy::primitives::{Address, Signature, TxHash, U256};

/// Helper: create a PgStore from DATABASE_URL or skip the test.
async fn pg_store() -> Option<PgStore> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("DATABASE_URL not set — skipping PgStore integration test");
            return None;
        }
    };
    Some(
        PgStore::new(&url)
            .await
            .expect("failed to connect to PgStore"),
    )
}

/// Reset the service_state singleton to defaults before each test.
async fn reset_state(store: &PgStore) {
    // We access the pool indirectly through the Store trait methods
    // by setting known values. For a proper reset, we use the store methods.
    let _ = store.set_latest_block_number(0).await;
}

fn dummy_log(block: u64, tx_hash: &str) -> SyntheticLog {
    SyntheticLog {
        log_index: 0,
        address: "0xdead".to_string(),
        topics: vec!["0xabcd".to_string()],
        data: "0x1234".to_string(),
        block_number: block,
        block_hash: [0u8; 32],
        transaction_hash: tx_hash.to_string(),
        transaction_index: 0,
        removed: false,
    }
}

fn dummy_txn_entry() -> TxnEntry {
    let tx = TxEip1559::default();
    TxnEntry {
        id: None,
        envelope: TxEnvelope::Eip1559(alloy::consensus::Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            TxHash::ZERO,
        )),
        signer: Address::ZERO,
        expires_at: None,
        logs: vec![],
    }
}

// ── Block number ─────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_block_number() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    store.set_latest_block_number(42).await.unwrap();
    assert_eq!(store.get_latest_block_number().await.unwrap(), 42);

    let new = store.advance_block_number().await.unwrap();
    assert_eq!(new, 43);
    assert_eq!(store.get_latest_block_number().await.unwrap(), 43);
}

// ── Note-reconciler sweep cursor (migration 010) ─────────────

#[tokio::test]
async fn test_pgstore_reconcile_cursor_round_trip() {
    let Some(store) = pg_store().await else {
        return;
    };

    // Round-trip through the service_state.reconcile_cursor column, including
    // the reset-to-0 the recovery flows (--restore / --reset-miden-store /
    // --resweep-from-genesis) perform. Prod incident: the cursor was
    // memory-only, so every container restart re-swept from genesis (~3h of
    // resync on prod history).
    store.set_reconcile_cursor(0).await.unwrap();
    assert_eq!(store.get_reconcile_cursor().await.unwrap(), 0);

    store.set_reconcile_cursor(200).await.unwrap();
    assert_eq!(store.get_reconcile_cursor().await.unwrap(), 200);

    store.set_reconcile_cursor(123_456).await.unwrap();
    assert_eq!(store.get_reconcile_cursor().await.unwrap(), 123_456);

    // Reset-to-genesis must persist too (recovery flows depend on it).
    store.set_reconcile_cursor(0).await.unwrap();
    assert_eq!(store.get_reconcile_cursor().await.unwrap(), 0);
}

// ── Logs ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_logs() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let log = dummy_log(10, "0xaaa");
    store.add_log(log).await.unwrap();

    let filter = LogFilter {
        from_block: Some("0xa".to_string()),
        to_block: Some("0xa".to_string()),
        address: None,
        topics: None,
        block_hash: None,
    };
    let results = store.get_logs(&filter, 10).await.unwrap();
    assert!(!results.is_empty(), "should find log at block 10");

    let tx_logs = store.get_logs_for_tx("0xaaa").await.unwrap();
    assert!(!tx_logs.is_empty(), "should find log by tx hash");
}

// ── GER ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_ger_lifecycle() {
    let Some(store) = pg_store().await else {
        return;
    };

    let ger = [0x42u8; 32];
    let entry = GerEntry {
        mainnet_exit_root: Some([0x01; 32]),
        rollup_exit_root: Some([0x02; 32]),
        block_number: 100,
        timestamp: 1234567890,
        evidence_verified: false,
    };

    // Initially not seen
    assert!(!store.has_seen_ger(&ger).await.unwrap());

    // Mark seen — returns true (newly inserted)
    assert!(store.mark_ger_seen(&ger, entry.clone()).await.unwrap());

    // Now seen
    assert!(store.has_seen_ger(&ger).await.unwrap());

    // Duplicate insert returns false
    assert!(!store.mark_ger_seen(&ger, entry).await.unwrap());

    // Latest GER should be this one
    let latest = store.get_latest_ger().await.unwrap();
    assert!(latest.is_some());

    // Get entry
    let fetched = store.get_ger_entry(&ger).await.unwrap().unwrap();
    assert_eq!(fetched.block_number, 100);
    assert_eq!(fetched.mainnet_exit_root, Some([0x01; 32]));

    // Not injected yet
    assert!(!store.is_ger_injected(&ger).await.unwrap());

    // Mark injected (via the atomic commit, which folds in injection)
    store
        .commit_ger_event_atomic(100, [0u8; 32], "0xger_inject_tx", &ger, None, None, 999)
        .await
        .unwrap();
    assert!(store.is_ger_injected(&ger).await.unwrap());
}

// ── GER update event (hash chain) ────────────────────────────

#[tokio::test]
async fn test_pgstore_ger_update_event() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;
    store.set_latest_block_number(50).await.unwrap();

    let ger = [0x55u8; 32];
    store
        .commit_ger_event_atomic(50, [0u8; 32], "0xger_tx", &ger, None, None, 999)
        .await
        .unwrap();

    // Should have emitted a log
    let logs = store.get_logs_for_tx("0xger_tx").await.unwrap();
    assert!(!logs.is_empty(), "ger update event should emit a log");

    // GER should be seen
    assert!(store.has_seen_ger(&ger).await.unwrap());
}

/// Audit H2 (PG twin) — `commit_ger_event_atomic` must be idempotent on retry.
/// The legacy two-step path (rolling the chain + emitting the log, then a
/// separate injection mark) left a crash window: if the process died between
/// them the chain had ALREADY been rolled while `is_injected` was still FALSE, so on
/// restart the projector re-rolled the hash chain and emitted a DUPLICATE
/// UpdateHashChainValue log — diverging the proxy's `hash_chain_value` from
/// aggkit. Calling `commit_ger_event_atomic` twice with the same deterministic
/// `tx_hash` must emit exactly ONE log and leave the emitted `hash_chain_value`
/// (the log's 3rd topic) unchanged. This is the Postgres twin of
/// `memory.rs::h2_commit_ger_event_atomic_is_idempotent_on_retry` — only the
/// in-memory path was covered before.
///
/// PG-gated: skips when `DATABASE_URL` is unset; runs in the postgres-feature CI.
#[tokio::test]
async fn test_pgstore_h2_commit_ger_event_atomic_is_idempotent_on_retry() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    // Unique tx_hash + GER so the assertions stay isolated from any GER that a
    // concurrent test rolls into the shared `service_state` singleton: the
    // idempotency gate keys on `tx_hash`, `is_ger_injected` keys on the GER, and
    // the log we inspect is fetched by `tx_hash`.
    let nonce = rand_u64();
    let tx_hash = format!("0xh2_atomic_{nonce:016x}");
    let mut ger = [0x55u8; 32];
    ger[..8].copy_from_slice(&nonce.to_be_bytes());

    assert!(!store.is_ger_injected(&ger).await.unwrap());

    // First commit: rolls the chain, emits one log, sets is_injected = TRUE.
    store
        .commit_ger_event_atomic(10, [0xaa; 32], &tx_hash, &ger, None, None, 1000)
        .await
        .unwrap();
    assert!(store.is_ger_injected(&ger).await.unwrap());

    let logs_first = store.get_logs_for_tx(&tx_hash).await.unwrap();
    assert_eq!(
        logs_first.len(),
        1,
        "first commit must emit exactly one UpdateHashChainValue log"
    );
    assert_eq!(
        logs_first[0].topics[0].to_lowercase(),
        UPDATE_HASH_CHAIN_VALUE_TOPIC.to_lowercase(),
        "emitted log must carry the UpdateHashChainValue topic"
    );
    // topic[2] is the rolled hash_chain_value the log carries — our observable
    // proxy for the on-chain-visible chain value.
    let chain_after_first = logs_first[0].topics[2].clone();

    // Retry with the SAME tx_hash — simulates a re-projection after a crash
    // before the txn committed. The gate (a log with this tx_hash already
    // exists) must skip the chain roll + log emission; is_injected stays TRUE.
    store
        .commit_ger_event_atomic(10, [0xaa; 32], &tx_hash, &ger, None, None, 1000)
        .await
        .unwrap();

    let logs_after = store.get_logs_for_tx(&tx_hash).await.unwrap();
    assert_eq!(
        logs_after.len(),
        1,
        "retry must NOT emit a duplicate UpdateHashChainValue log"
    );
    assert_eq!(
        logs_after[0].topics[2], chain_after_first,
        "retry must NOT roll the hash chain a second time"
    );
    assert!(
        store.is_ger_injected(&ger).await.unwrap(),
        "GER must remain injected after the idempotent retry"
    );
}

/// Audit H2 (case-insensitivity). The idempotency gate keys on `transaction_hash`,
/// which is canonically lowercase hex everywhere else in the store
/// (`get_logs_for_tx` queries `lower(transaction_hash)`, `memory.rs` stores
/// lowercase). A case-SENSITIVE gate would miss an already-stored lowercase row
/// when a retry arrives with a mixed/upper-case form of the SAME hash → the
/// chain would re-roll and a DUPLICATE UpdateHashChainValue log would be emitted.
/// Committing with an UPPER-case `tx_hash` and retrying with its lowercase form
/// must emit exactly ONE log and leave `hash_chain_value` unchanged.
///
/// PG-gated: skips when `DATABASE_URL` is unset; runs in the postgres-feature CI.
#[tokio::test]
async fn test_pgstore_h2_commit_ger_event_atomic_is_idempotent_case_insensitive() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let nonce = rand_u64();
    // Upper/mixed-case hex tx_hash for the first commit.
    let tx_hash_upper = format!("0xH2CASE_{nonce:016X}");
    let tx_hash_lower = tx_hash_upper.to_lowercase();
    let mut ger = [0x77u8; 32];
    ger[..8].copy_from_slice(&nonce.to_be_bytes());

    assert!(!store.is_ger_injected(&ger).await.unwrap());

    // First commit with the UPPER-case tx_hash.
    store
        .commit_ger_event_atomic(10, [0xbb; 32], &tx_hash_upper, &ger, None, None, 1000)
        .await
        .unwrap();
    assert!(store.is_ger_injected(&ger).await.unwrap());

    // Canonical lowercase lookup must find the emitted log (stored lowercase).
    let logs_first = store.get_logs_for_tx(&tx_hash_lower).await.unwrap();
    assert_eq!(
        logs_first.len(),
        1,
        "first commit must emit exactly one UpdateHashChainValue log (found by lowercase key)"
    );
    let chain_after_first = logs_first[0].topics[2].clone();

    // Retry with the DIFFERENTLY-CASED form of the SAME hash (lowercase). The
    // case-insensitive gate must recognize it as already-emitted and skip.
    store
        .commit_ger_event_atomic(10, [0xbb; 32], &tx_hash_lower, &ger, None, None, 1000)
        .await
        .unwrap();

    let logs_after = store.get_logs_for_tx(&tx_hash_lower).await.unwrap();
    assert_eq!(
        logs_after.len(),
        1,
        "differently-cased retry must NOT emit a duplicate UpdateHashChainValue log"
    );
    assert_eq!(
        logs_after[0].topics[2], chain_after_first,
        "differently-cased retry must NOT roll the hash chain a second time"
    );
    assert!(
        store.is_ger_injected(&ger).await.unwrap(),
        "GER must remain injected after the idempotent case-insensitive retry"
    );
}

// ── Transactions ─────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_txn_lifecycle() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let tx_hash = TxHash::from([0xBBu8; 32]);
    let entry = dummy_txn_entry();

    // Begin
    store.txn_begin(tx_hash, entry).await.unwrap();

    // Should be retrievable
    let data = store.txn_get(tx_hash).await.unwrap();
    assert!(data.is_some(), "txn should exist after begin");
    let data = data.unwrap();
    assert!(data.result.is_none(), "pending txn should have no result");

    // Receipt should be None (pending)
    let receipt = store.txn_receipt(tx_hash).await.unwrap();
    assert!(receipt.is_none(), "pending txn should have no receipt");

    // Commit success
    store.set_latest_block_number(5).await.unwrap();
    store
        .txn_commit(tx_hash, Ok(()), 5, [0u8; 32])
        .await
        .unwrap();

    // Receipt should now exist
    let receipt = store.txn_receipt(tx_hash).await.unwrap();
    assert!(receipt.is_some());
    let (result, block) = receipt.unwrap();
    assert!(result.is_ok());
    assert_eq!(block, 5);
}

#[tokio::test]
async fn test_pgstore_txn_failure() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let tx_hash = TxHash::from([0xCCu8; 32]);
    let entry = dummy_txn_entry();

    store.txn_begin(tx_hash, entry).await.unwrap();
    store
        .txn_commit(tx_hash, Err("test error".to_string()), 1, [0u8; 32])
        .await
        .unwrap();

    let receipt = store.txn_receipt(tx_hash).await.unwrap().unwrap();
    assert!(receipt.0.is_err());
    assert_eq!(receipt.0.unwrap_err(), "test error");
}

/// PR #127 follow-up — `txn_commit` on a tx_hash with no `txn_begin` row must
/// ERROR (matching `InMemoryStore::txn_commit`), not silently update zero
/// rows and return Ok. Pre-fix the zero-row UPDATE was invisible: a projector
/// racing the GER submitter could "finalise" a receipt whose row didn't exist
/// yet, and the late `txn_begin` then left the real receipt pending forever.
/// Memory twin: `memory::tests::test_txn_commit_missing_row_errors`.
/// PG-gated: skips when `DATABASE_URL` is unset; runs in the postgres-feature CI.
#[tokio::test]
async fn test_pgstore_txn_commit_missing_row_errors() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let tx_hash = TxHash::from([0xDDu8; 32]);
    // No txn_begin for this hash.
    let err = store
        .txn_commit(tx_hash, Ok(()), 7, [0u8; 32])
        .await
        .expect_err("txn_commit without a prior txn_begin must error");
    assert!(
        err.to_string().contains("not found"),
        "error must identify the missing row, got: {err:#}"
    );
    // The bail happens before tx.commit(): no receipt, no leaked synthetic
    // logs or log-counter advance from the rolled-back transaction.
    assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());
    assert!(store.txn_get(tx_hash).await.unwrap().is_none());

    // The failure-status flavour must error identically (same UPDATE).
    let err = store
        .txn_commit(tx_hash, Err("boom".to_string()), 7, [0u8; 32])
        .await
        .expect_err("failed-status txn_commit without a row must also error");
    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn test_pgstore_confirmed_duplicate_finalizes_linked_pending() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut hash_bytes = [0xdeu8; 32];
    hash_bytes[16..].copy_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_be_bytes(),
    );
    let tx_hash = TxHash::from(hash_bytes);
    let tx_key = format!("{tx_hash:#x}");
    store.txn_begin(tx_hash, dummy_txn_entry()).await.unwrap();
    store
        .prepare_note_handoff(&tx_key, "confirmed-duplicate-commitment", "note-id", 10)
        .await
        .unwrap();
    store
        .txn_commit(tx_hash, Err("raw Miden error".into()), 10, [0; 32])
        .await
        .unwrap();
    assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());

    store
        .txn_commit_confirmed_duplicate(
            tx_hash,
            Err("execution reverted: AlreadyClaimed()".into()),
            11,
        )
        .await
        .unwrap();
    let (result, block) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
    assert!(result.is_err());
    assert_eq!(block, 11);
    assert!(store.get_logs_for_tx(&tx_key).await.unwrap().is_empty());
}

/// BLOCKER 2 (success-always-wins CAS) — PG twin of
/// `memory::tests::test_txn_commit_terminal_success_not_clobbered_by_failure`.
/// Once a receipt is 'success', the failure-commit CAS predicate
/// (`status = 'pending'`) excludes it, so a later failure is a zero-row no-op
/// that returns Ok, preserving status 0x1. Models the TTL sweeper racing a
/// worker that already landed the Miden op — the pre-fix overwrite made aggkit
/// resubmit a landed op. PG-gated: skips when `DATABASE_URL` is unset.
#[tokio::test]
async fn test_pgstore_terminal_success_not_clobbered_by_failure() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let tx_hash = TxHash::from([0x5Au8; 32]);
    store.txn_begin(tx_hash, dummy_txn_entry()).await.unwrap();
    store
        .txn_commit(tx_hash, Ok(()), 12, [0u8; 32])
        .await
        .unwrap();

    // Late failure commit for the already-terminal (success) row: accepted,
    // but a NO-OP — the success must survive.
    store
        .txn_commit(
            tx_hash,
            Err("TTL expired (>300s in non-terminal state)".to_string()),
            15,
            [0u8; 32],
        )
        .await
        .expect("late failure commit must be an accepted no-op, not an error");

    let (res, block) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
    assert!(res.is_ok(), "success must win; got failure: {res:?}");
    assert_eq!(block, 12, "success block preserved");
}

/// BLOCKER 2 (success-always-wins CAS) — PG twin of
/// `memory::tests::test_txn_commit_success_supersedes_prior_failure_with_claimevent`.
/// A real Miden landing supersedes a prior (TTL) failure: the success-commit CAS
/// predicate (`status <> 'success'`) updates the 'failed' row to 'success' and
/// materialises the attached ClaimEvent. Guarantees a claim that actually landed
/// never ends stuck at a TTL-failure. PG-gated: skips when `DATABASE_URL` unset.
#[tokio::test]
async fn test_pgstore_success_supersedes_prior_failure() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let tx_hash = TxHash::from([0x5Bu8; 32]);
    // Attach a ClaimEvent-shaped log so the override's materialisation is checked.
    let mut entry = dummy_txn_entry();
    entry.logs = vec![alloy::primitives::LogData::new_unchecked(
        vec![alloy::primitives::B256::from([0xC1u8; 32])],
        alloy::primitives::Bytes::from(vec![0xAB]),
    )];
    store.txn_begin(tx_hash, entry).await.unwrap();

    // TTL sweeper fails the still-running job first (no logs materialised).
    store
        .txn_commit(tx_hash, Err("TTL expired".to_string()), 3, [0u8; 32])
        .await
        .unwrap();
    assert!(
        store
            .get_logs_for_tx(&format!("{tx_hash:#x}"))
            .await
            .unwrap()
            .is_empty(),
        "a failure must NOT materialise the ClaimEvent"
    );

    // Miden landed → projector commits success for the same hash → override.
    store
        .txn_commit(tx_hash, Ok(()), 5, [0u8; 32])
        .await
        .expect("a real landing must supersede the provisional failure");

    let (res, block) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
    assert!(
        res.is_ok(),
        "success must supersede the TTL failure; got {res:?}"
    );
    assert_eq!(block, 5, "success block wins");
    assert_eq!(
        store
            .get_logs_for_tx(&format!("{tx_hash:#x}"))
            .await
            .unwrap()
            .len(),
        1,
        "the ClaimEvent must be materialised on the success override"
    );
}

/// BLOCKER 2 (re-review) — single-snapshot CAS under concurrency. Many
/// `txn_commit`s race `txn_begin`s for distinct hashes. The invariant that the
/// old UPDATE-then-separate-SELECT could violate under READ COMMITTED: a
/// `txn_commit` returning Ok must correspond to a row that is ACTUALLY terminal
/// (never a silent wrong-Ok that leaves the row pending). The SELECT ... FOR
/// UPDATE takes the classify + update off one locked snapshot, so this holds.
/// PG-gated: skips when `DATABASE_URL` is unset.
#[tokio::test]
async fn test_pgstore_txn_commit_concurrent_begin_single_snapshot() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;
    let store = std::sync::Arc::new(store);

    let mut handles = Vec::new();
    for i in 0..64u8 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let tx_hash = TxHash::from([i; 32]);
            // Racer A: begin then a failure commit.
            let a = {
                let store = store.clone();
                tokio::spawn(async move {
                    store.txn_begin(tx_hash, dummy_txn_entry()).await.ok();
                    store
                        .txn_commit(tx_hash, Err("x".into()), 1, [0u8; 32])
                        .await
                })
            };
            // Racer B: a failure commit that may observe the row missing or present.
            let b = {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .txn_commit(tx_hash, Err("y".into()), 1, [0u8; 32])
                        .await
                })
            };
            let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
            // INVARIANT: any commit that returned Ok must have left a terminal
            // receipt — never a wrong-Ok over a still-pending/absent row.
            if ra.is_ok() || rb.is_ok() {
                let receipt = store.txn_receipt(tx_hash).await.unwrap();
                assert!(
                    receipt.is_some(),
                    "a committed-Ok hash must have a terminal receipt (no wrong-Ok): {tx_hash}"
                );
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

/// The one selected evidence-scan cursor round-trips through the legacy
/// `l1_indexer_state.finalized_scan_cursor` column. PG-gated: skips when
/// `DATABASE_URL` is unset.
#[tokio::test]
async fn test_pgstore_l1_evidence_cursor_roundtrip() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    store.set_l1_evidence_cursor(0).await.unwrap();
    assert_eq!(store.get_l1_evidence_cursor().await.unwrap(), 0);
    store.set_l1_evidence_cursor(12_345).await.unwrap();
    assert_eq!(store.get_l1_evidence_cursor().await.unwrap(), 12_345);
}

/// Evidence provenance binding requires a dedicated freshly-migrated database
/// because the singleton policy is intentionally immutable.
#[tokio::test]
#[ignore = "requires a dedicated fresh PostgreSQL database"]
async fn test_pgstore_l1_evidence_policy_binding_is_immutable() {
    let store = pg_store().await.expect("DATABASE_URL must be set");
    store.bind_l1_evidence_policy("finalized").await.unwrap();
    store.bind_l1_evidence_policy("finalized").await.unwrap();
    let err = store
        .bind_l1_evidence_policy("safe")
        .await
        .expect_err("a PostgreSQL evidence policy change must fail closed");
    assert!(format!("{err:#}").contains("bound to `finalized`"));
}

/// An upgraded database with policy-derived state but no provenance is
/// ambiguous and must not be silently labelled with the current setting.
#[tokio::test]
#[ignore = "requires a dedicated fresh PostgreSQL database"]
async fn test_pgstore_untagged_evidence_state_is_rejected() {
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let (client, connection) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls)
        .await
        .expect("connect for test setup");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "UPDATE l1_indexer_state
             SET evidence_tag = NULL, finalized_block = 0, finalized_scan_cursor = 1;
             UPDATE ger_entries SET finalized_verified = FALSE;",
        )
        .await
        .expect("seed ambiguous pre-policy state");

    let store = PgStore::new(&db_url).await.expect("create PgStore");
    let err = store
        .bind_l1_evidence_policy("finalized")
        .await
        .expect_err("untagged evidence progress must fail closed");
    assert!(format!("{err:#}").contains("without an evidence policy"));

    client
        .execute(
            "UPDATE l1_indexer_state
             SET evidence_tag = NULL, finalized_block = 0, finalized_scan_cursor = 0",
            &[],
        )
        .await
        .expect("restore clean singleton state");
}

/// The configured scan writes roots, L1 metadata, and its provenance marker in
/// one UPSERT. The PostgreSQL marker retains its legacy physical column name.
/// PG-gated: skips when `DATABASE_URL` is unset.
#[tokio::test]
async fn test_pgstore_set_ger_exit_roots_verifies_evidence() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    let ger = [0x3Au8; 32];
    store
        .set_ger_exit_roots(&ger, [0x01u8; 32], [0x02u8; 32], 100, 1_700_000_000)
        .await
        .unwrap();
    let entry = store.get_ger_entry(&ger).await.unwrap().unwrap();
    assert!(
        entry.evidence_verified,
        "the selected scan must set its provenance marker with the roots"
    );
    assert_eq!(entry.mainnet_exit_root, Some([0x01u8; 32]));
    assert_eq!(entry.rollup_exit_root, Some([0x02u8; 32]));
    assert_eq!(entry.block_number, 100);
    assert_eq!(entry.timestamp, 1_700_000_000);
}

// ── Nonces ───────────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_nonces() {
    let Some(store) = pg_store().await else {
        return;
    };

    // Use a unique address to avoid collisions with other tests
    let addr = format!(
        "0xnonce_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    assert_eq!(store.nonce_get(&addr).await.unwrap(), 0);

    let n = store.nonce_increment(&addr).await.unwrap();
    assert_eq!(n, 0, "first increment returns 0 (pre-increment)");

    assert_eq!(store.nonce_get(&addr).await.unwrap(), 1);

    let n = store.nonce_increment(&addr).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(store.nonce_get(&addr).await.unwrap(), 2);
}

// ── Claims ───────────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_claims() {
    let Some(store) = pg_store().await else {
        return;
    };

    let idx = U256::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    );

    assert!(!store.is_claimed(&idx).await.unwrap());

    store.try_claim(idx).await.unwrap();
    assert!(store.is_claimed(&idx).await.unwrap());

    // Duplicate claim should fail
    assert!(store.try_claim(idx).await.is_err());

    // Unclaim
    store.unclaim(&idx).await.unwrap();
    assert!(!store.is_claimed(&idx).await.unwrap());
}

// ── Address mappings ─────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_address_mappings() {
    let Some(store) = pg_store().await else {
        return;
    };

    let eth = Address::from([0xAA; 20]);

    // No mapping initially
    assert!(store.get_address_mapping(&eth).await.unwrap().is_none());

    // Set + get round-trip
    let miden_id =
        miden_protocol::account::AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
    store.set_address_mapping(eth, miden_id).await.unwrap();
    let retrieved = store.get_address_mapping(&eth).await.unwrap();
    assert_eq!(retrieved, Some(miden_id));

    // Overwrite with a different value
    let miden_id2 =
        miden_protocol::account::AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
    store.set_address_mapping(eth, miden_id2).await.unwrap();
    let retrieved2 = store.get_address_mapping(&eth).await.unwrap();
    assert_eq!(retrieved2, Some(miden_id2));
}

// ── Claim watcher ────────────────────────────────────────────

/// `is_claim_note_processed` + `mark_claim_note_processed` round-trip,
/// and ON CONFLICT DO NOTHING semantics for the idempotency mark.
#[tokio::test]
async fn test_pgstore_claim_watcher_processed_lifecycle() {
    let Some(store) = pg_store().await else {
        return;
    };

    let note_id = format!(
        "claim_test_note_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let gi = {
        let mut g = [0u8; 32];
        g[31] = 0x42;
        g
    };

    assert!(!store.is_claim_note_processed(&note_id).await.unwrap());
    store
        .mark_claim_note_processed(note_id.clone(), gi, 7)
        .await
        .unwrap();
    assert!(store.is_claim_note_processed(&note_id).await.unwrap());

    // Second mark must be a no-op (ON CONFLICT DO NOTHING).
    store
        .mark_claim_note_processed(note_id.clone(), gi, 99)
        .await
        .unwrap();
    assert!(store.is_claim_note_processed(&note_id).await.unwrap());
}

/// `has_claim_event_for_global_index` finds a watcher-emitted ClaimEvent
/// AND an `add_claim_event`-emitted one (data-prefix scan path).
#[tokio::test]
async fn test_pgstore_has_claim_event_for_global_index_finds_both_sources() {
    let Some(store) = pg_store().await else {
        return;
    };

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    // Source A: watcher-emitted via commit_manual_claim_event_atomic.
    let gi_a = {
        let mut g = [0u8; 32];
        g[..16].copy_from_slice(&now_ns.to_be_bytes());
        g
    };
    let note_id_a = format!("claim_test_note_a_{now_ns}");
    assert!(!store.has_claim_event_for_global_index(&gi_a).await.unwrap());
    store
        .commit_manual_claim_event_atomic(
            note_id_a,
            "0xbridge",
            (now_ns % 1_000_000) as u64,
            [0u8; 32],
            "0xwatchertx",
            gi_a,
            0,
            &[0u8; 20],
            &[0u8; 20],
            1234,
        )
        .await
        .unwrap();
    assert!(store.has_claim_event_for_global_index(&gi_a).await.unwrap());

    // Source B: normal-RPC-path emission via add_claim_event.
    let gi_b = {
        let mut g = [0u8; 32];
        g[..16].copy_from_slice(&(now_ns + 1).to_be_bytes());
        g
    };
    assert!(!store.has_claim_event_for_global_index(&gi_b).await.unwrap());
    store
        .add_claim_event(
            "0xbridge",
            (now_ns % 1_000_000) as u64 + 1,
            [0u8; 32],
            &format!("0xrpctx_{now_ns}"),
            &gi_b,
            0,
            &[0u8; 20],
            &[0u8; 20],
            5678,
        )
        .await
        .unwrap();
    assert!(store.has_claim_event_for_global_index(&gi_b).await.unwrap());
}

/// `commit_manual_claim_event_atomic`: a single PG txn folds the processed
/// marker, log insert, and linked receipt completion. Block-tip advancement is
/// intentionally left to the projector after the full block.
#[tokio::test]
async fn test_pgstore_commit_manual_claim_event_atomic() {
    let Some(store) = pg_store().await else {
        return;
    };

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let gi = {
        let mut g = [0u8; 32];
        g[..16].copy_from_slice(&now_ns.to_be_bytes());
        g
    };
    let note_id = format!("claim_atomic_test_{now_ns}");
    // Use a high block_number namespaced by timestamp so tests don't fight.
    let block = (now_ns % 1_000_000) as u64 + 10_000;
    let tx_hash = format!("0xclaim_atomic_{now_ns}");

    store
        .commit_manual_claim_event_atomic(
            note_id.clone(),
            "0xbridge",
            block,
            [0u8; 32],
            &tx_hash,
            gi,
            0,
            &[0u8; 20],
            &[0u8; 20],
            42,
        )
        .await
        .unwrap();

    // Note processed.
    assert!(store.is_claim_note_processed(&note_id).await.unwrap());
    // ClaimEvent dedup query finds the row.
    assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());

    // ── Reviewer concern #2 (write-before-seal): the atomic must NOT advance the tip. ──
    // `insert_pending_claim_calldata` leaves a PENDING envelope; the atomic finalises that
    // receipt AND emits the ClaimEvent, but sealing block N is the projector's job at
    // end-of-block (`project_block_notes`). If the atomic — or a stray `txn_commit` in the
    // claim path — advanced `latest_block_number` mid-block, aggkit could scan a partial
    // block N and permanently miss its later logs. (Invisible to the in-memory store, whose
    // `txn_commit` never touches the tip — this is the Postgres-only half of the fix.)
    let before = store.get_latest_block_number().await.unwrap();
    let seal_block = before + 50_000; // strictly above the current tip
    let seal_hash: TxHash = {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(&(now_ns + 5).to_be_bytes());
        TxHash::from(h)
    };
    let seal_hash_str = format!("{seal_hash:#x}");
    let seal_note = format!("claim_seal_note_{now_ns}");
    let seal_gi = {
        let mut g = [0u8; 32];
        g[..16].copy_from_slice(&(now_ns + 6).to_be_bytes());
        g
    };
    // A pending envelope under `seal_hash` (exactly what insert_pending_claim_calldata leaves).
    store.txn_begin(seal_hash, dummy_txn_entry()).await.unwrap();
    assert!(
        store.txn_receipt(seal_hash).await.unwrap().is_none(),
        "pending before the atomic finalises it"
    );
    store
        .commit_manual_claim_event_atomic(
            seal_note,
            "0xbridge",
            seal_block,
            [0u8; 32],
            &seal_hash_str,
            seal_gi,
            0,
            &[0u8; 20],
            &[0u8; 20],
            42,
        )
        .await
        .unwrap();
    assert!(
        store.get_latest_block_number().await.unwrap() < seal_block,
        "the atomic ClaimEvent commit must NOT seal the block — only project_block_notes \
         advances latest_block_number, at end-of-block (write-before-seal)"
    );
    // The linked receipt IS finalised together with the ClaimEvent, at the claim's block.
    let (res, blk) = store
        .txn_receipt(seal_hash)
        .await
        .unwrap()
        .expect("the linked receipt is finalised inline by the atomic");
    assert!(res.is_ok());
    assert_eq!(blk, seal_block, "receipt block == ClaimEvent block");
}

/// Audit H1/H3 — a reservation assigns the index before the atomic BridgeEvent commit.
/// A second commit for the same note must reuse that index and emit no duplicate log. Run with
/// `DATABASE_URL=postgres://… cargo test --lib test_pgstore_commit_b2agg`.
#[tokio::test]
async fn test_pgstore_commit_b2agg_event_atomic_idempotent() {
    let Some(store) = pg_store().await else {
        return;
    };

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let note_id = format!("b2agg_atomic_test_{now_ns}");
    let block = (now_ns % 1_000_000) as u64 + 20_000;
    let tx_hash = format!("0xb2agg_atomic_{now_ns}");

    let dc_before = store.get_deposit_count().await.unwrap();
    store.reserve_deposit_index(&note_id).await.unwrap();

    let dc1 = store
        .commit_b2agg_event_atomic(
            note_id.clone(),
            "0xbridge",
            block,
            [0u8; 32],
            &tx_hash,
            0,
            1,
            &[0u8; 20],
            0,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();
    assert!(store.is_note_processed(&note_id).await.unwrap());

    // Retry — simulates a re-projection after a crash before commit.
    let dc2 = store
        .commit_b2agg_event_atomic(
            note_id.clone(),
            "0xbridge",
            block,
            [0u8; 32],
            &tx_hash,
            0,
            1,
            &[0u8; 20],
            0,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();

    assert_eq!(dc1, dc2, "retry must reuse the same deposit_count");
    assert!(store.get_deposit_count().await.unwrap() >= dc_before + 1);
}

/// Audit H1/H3 — PG-layer log-once idempotency. Calling
/// `commit_b2agg_event_atomic` TWICE with the same deterministic `tx_hash`
/// (the projector re-derives it from the note, so a re-projection produces the
/// identical hash) must emit the synthetic BridgeEvent EXACTLY ONCE and leave
/// store state unchanged on the second call. Complements
/// `test_pgstore_commit_b2agg_event_atomic_idempotent`, which covers the
/// deposit_count reuse; this asserts the log-count invariant directly via
/// `get_logs_for_tx`. Only the InMemoryStore equivalent existed before. DB-gated
/// (skipped without `DATABASE_URL`).
#[tokio::test]
async fn test_pgstore_commit_b2agg_event_atomic_emits_log_once() {
    let Some(store) = pg_store().await else {
        return;
    };

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let note_id = format!("b2agg_logonce_test_{now_ns}");
    let block = (now_ns % 1_000_000) as u64 + 30_000;
    let tx_hash = format!("0xb2agg_logonce_{now_ns}");
    store.reserve_deposit_index(&note_id).await.unwrap();

    // First commit emits the BridgeEvent.
    let dc1 = store
        .commit_b2agg_event_atomic(
            note_id.clone(),
            "0xbridge",
            block,
            [0u8; 32],
            &tx_hash,
            0,
            1,
            &[0u8; 20],
            0,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();

    let logs_after_first = store.get_logs_for_tx(&tx_hash).await.unwrap();
    assert_eq!(
        logs_after_first.len(),
        1,
        "first commit must emit exactly one BridgeEvent"
    );
    // Retry with the SAME tx_hash — must reuse the reservation and emit no log.
    let dc2 = store
        .commit_b2agg_event_atomic(
            note_id.clone(),
            "0xbridge",
            block,
            [0u8; 32],
            &tx_hash,
            0,
            1,
            &[0u8; 20],
            0,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();

    let logs_after_retry = store.get_logs_for_tx(&tx_hash).await.unwrap();
    assert_eq!(
        logs_after_retry.len(),
        1,
        "retry must NOT emit a duplicate BridgeEvent — log emitted exactly once"
    );
    assert_eq!(dc2, dc1, "retry must reuse the same reservation");
}

// ── RD-913 monitor trackers ─────────────────────────────────

/// PgStore round-trip for monitor_burn_serials. INSERT … ON CONFLICT
/// must report true on first observation and false on the duplicate,
/// matching the InMemoryStore contract.
#[tokio::test]
async fn test_pgstore_rd913_burn_serial_observe() {
    let Some(store) = pg_store().await else {
        return;
    };
    // Use a random serial per-run so this test is safe to re-run without
    // truncating the table (other suites may populate it concurrently).
    let mut serial = [0u8; 32];
    serial[..8].copy_from_slice(&rand_u64().to_be_bytes());
    assert!(!store.burn_serial_seen(&serial).await.unwrap());
    assert!(store.burn_serial_observe(&serial).await.unwrap());
    assert!(store.burn_serial_seen(&serial).await.unwrap());
    // Second insert returns false (Cantina #5 duplicate signal).
    assert!(!store.burn_serial_observe(&serial).await.unwrap());
}

/// PgStore round-trip for monitor_twin_notes. Per-NoteId commitments
/// must be retrievable for the twin-detection branch.
#[tokio::test]
async fn test_pgstore_rd913_twin_notes() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut note_id = [0u8; 32];
    note_id[..8].copy_from_slice(&rand_u64().to_be_bytes());
    let c1 = [0x11u8; 32];
    let c2 = [0x22u8; 32];

    assert!(store.twin_note_observe(&note_id, &c1).await.unwrap());
    assert!(!store.twin_note_observe(&note_id, &c1).await.unwrap());
    assert!(store.twin_note_observe(&note_id, &c2).await.unwrap());

    let commitments = store.twin_note_commitments(&note_id).await.unwrap();
    assert_eq!(commitments.len(), 2);
    assert!(commitments.contains(&c1));
    assert!(commitments.contains(&c2));
}

/// PgStore round-trip for monitor_expected_mints. Record → load → tick
/// updates → remove. Exercises the full state machine the
/// `ExpectedMintTracker` drives.
#[tokio::test]
async fn test_pgstore_rd913_expected_mints() {
    let Some(store) = pg_store().await else {
        return;
    };
    let mut gi = [0u8; 32];
    gi[..8].copy_from_slice(&rand_u64().to_be_bytes());
    let mint = [0xCCu8; 32];

    store.expected_mint_record(&gi, &mint).await.unwrap();
    let rows = store.expected_mint_load_all().await.unwrap();
    let found = rows.iter().find(|(g, _, _, _)| *g == gi).unwrap();
    assert_eq!(found.1, mint);
    assert_eq!(found.2, 0);
    assert!(!found.3);

    // Bump tick + alerted flag.
    store.expected_mint_update_tick(&gi, 5, true).await.unwrap();
    let rows = store.expected_mint_load_all().await.unwrap();
    let found = rows.iter().find(|(g, _, _, _)| *g == gi).unwrap();
    assert_eq!(found.2, 5);
    assert!(found.3);

    // Remove. The row should be gone.
    store.expected_mint_remove(&gi).await.unwrap();
    let rows = store.expected_mint_load_all().await.unwrap();
    assert!(rows.iter().all(|(g, _, _, _)| *g != gi));
}

// ── Cantina MA#18 — unbridgeable bridge-outs ─────────────────

/// PgStore round-trip + first-write-wins idempotency for
/// `record_unbridgeable_bridge_out` / `get_unbridgeable_bridge_out`
/// (`postgres.rs`, migration 006). The InMemoryStore contract is pinned via
/// the `bridge_out::tests::ma18_*` wiring tests; this is the PG twin.
#[tokio::test]
async fn test_pgstore_ma18_unbridgeable_bridge_out_roundtrip_first_write_wins() {
    use super::{UnbridgeableBridgeOut, UnbridgeableBridgeOutReason};

    let Some(store) = pg_store().await else {
        return;
    };

    let note_id = format!("ma18_test_note_{}", rand_u64());
    let bridge =
        miden_protocol::account::AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

    // Unknown note → None (not an error).
    assert!(
        store
            .get_unbridgeable_bridge_out(&note_id)
            .await
            .unwrap()
            .is_none()
    );

    // First write lands and reports true (newly recorded).
    let first = UnbridgeableBridgeOut {
        note_id: note_id.clone(),
        bridge_account: bridge,
        reason: UnbridgeableBridgeOutReason::StorageParseFailed,
        detail: "storage too short: 1 felt".to_string(),
        note_dump: "{\"script_root\":\"0xabc\",\"storage_items\":[0]}".to_string(),
        observed_block: 42,
    };
    assert!(
        store.record_unbridgeable_bridge_out(first).await.unwrap(),
        "first quarantine write must report newly-recorded"
    );

    let row = store
        .get_unbridgeable_bridge_out(&note_id)
        .await
        .unwrap()
        .expect("quarantine row must round-trip");
    assert_eq!(row.note_id, note_id);
    assert_eq!(row.bridge_account, bridge);
    assert_eq!(row.reason, UnbridgeableBridgeOutReason::StorageParseFailed);
    assert_eq!(row.detail, "storage too short: 1 felt");
    assert!(row.note_dump.contains("storage_items"));
    assert_eq!(row.observed_block, 42);

    // Second write for the same note_id (later tick, different detail) must
    // be a no-op: reports false, row keeps the FIRST observation.
    let second = UnbridgeableBridgeOut {
        note_id: note_id.clone(),
        bridge_account: bridge,
        reason: UnbridgeableBridgeOutReason::UnknownFaucet,
        detail: "overwritten detail".to_string(),
        note_dump: "{}".to_string(),
        observed_block: 99,
    };
    assert!(
        !store.record_unbridgeable_bridge_out(second).await.unwrap(),
        "duplicate quarantine write must report already-recorded"
    );
    let row = store
        .get_unbridgeable_bridge_out(&note_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.reason,
        UnbridgeableBridgeOutReason::StorageParseFailed,
        "first-write-wins: reason must not be overwritten"
    );
    assert_eq!(row.observed_block, 42, "first-write-wins: block preserved");
    assert_eq!(row.detail, "storage too short: 1 felt");
}

// ── S3 / S4 / S9 — atomicity + decode-failure hardening ─────

/// S3 — `add_log`'s counter UPDATE + row INSERT run in ONE PG transaction.
/// A storm of concurrent `add_log` calls must produce exactly one row per
/// call with all `log_index` values distinct (no dupes) and none missing (no
/// counter bump without a matching row — the pre-fix gap signature).
#[tokio::test]
async fn test_pgstore_s3_add_log_concurrent_storm_no_gaps_no_dupes() {
    use std::sync::Arc;

    let Some(store) = pg_store().await else {
        return;
    };
    let store = Arc::new(store);

    const N: usize = 16;
    // Per-run unique block so this test's rows are isolated from every other
    // suite writing synthetic_logs into the shared database.
    let block = 2_000_000 + (rand_u64() % 1_000_000);
    let run = rand_u64();

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .add_log(dummy_log(block, &format!("0xs3_{run}_{i}")))
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().expect("concurrent add_log must succeed");
    }

    let filter = LogFilter {
        from_block: Some(format!("0x{block:x}")),
        to_block: Some(format!("0x{block:x}")),
        address: None,
        topics: None,
        block_hash: None,
    };
    let logs = store.get_logs(&filter, block).await.unwrap();
    assert_eq!(
        logs.len(),
        N,
        "every add_log must have exactly one materialised row (no gaps)"
    );
    let mut indices: Vec<u64> = logs.iter().map(|l| l.log_index).collect();
    indices.sort_unstable();
    indices.dedup();
    assert_eq!(
        indices.len(),
        N,
        "log_index values must be globally unique (no dupes) — the atomic \
         counter+INSERT must serialise correctly under concurrency"
    );
    // Every submitted tx hash landed (nothing silently dropped).
    for i in 0..N {
        let want = format!("0xs3_{run}_{i}");
        assert!(
            logs.iter().any(|l| l.transaction_hash == want),
            "log for {want} missing — counter advanced without its row?"
        );
    }
}

/// S4 — `txn_commit` folds the status UPDATE and the materialisation of ALL
/// attached logs into one PG transaction. Under a storm of concurrent
/// commits, every committed txn must surface success + its FULL log set
/// (never success-with-partial-logs), and the inlined per-log counter bumps
/// must stay collision-free across the storm.
#[tokio::test]
async fn test_pgstore_s4_txn_commit_concurrent_storm_status_and_logs_atomic() {
    use alloy::primitives::{B256, Bytes};
    use std::sync::Arc;

    let Some(store) = pg_store().await else {
        return;
    };
    let store = Arc::new(store);

    const N: usize = 8;
    const LOGS_PER_TXN: usize = 2;
    let run = rand_u64();
    let block = 3_000_000 + (run % 1_000_000);

    let mut hashes = Vec::with_capacity(N);
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        // Per-run unique tx hash (transactions.tx_hash is the primary key).
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&run.to_be_bytes());
        h[31] = i as u8;
        let tx_hash = TxHash::from(h);
        hashes.push(tx_hash);

        let mut entry = dummy_txn_entry();
        entry.logs = (0..LOGS_PER_TXN)
            .map(|j| {
                alloy::primitives::LogData::new_unchecked(
                    vec![B256::from([(i * LOGS_PER_TXN + j) as u8; 32])],
                    Bytes::from(vec![i as u8, j as u8]),
                )
            })
            .collect();

        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store.txn_begin(tx_hash, entry).await?;
            store.txn_commit(tx_hash, Ok(()), block, [0u8; 32]).await
        }));
    }
    for h in handles {
        h.await
            .unwrap()
            .expect("concurrent txn_commit must succeed");
    }

    let mut all_indices = Vec::new();
    for tx_hash in &hashes {
        let (result, committed_block) = store
            .txn_receipt(*tx_hash)
            .await
            .unwrap()
            .expect("committed txn must have a receipt");
        assert!(result.is_ok(), "status must be success");
        assert_eq!(committed_block, block);

        let logs = store
            .get_logs_for_tx(&format!("{tx_hash:#x}"))
            .await
            .unwrap();
        assert_eq!(
            logs.len(),
            LOGS_PER_TXN,
            "success status must NEVER be visible with a partial log set (S4)"
        );
        all_indices.extend(logs.iter().map(|l| l.log_index));
    }
    let mut deduped = all_indices.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        deduped.len(),
        N * LOGS_PER_TXN,
        "inlined counter bumps inside txn_commit must not collide across \
         concurrent commits"
    );
}

/// S9 — a corrupted `envelope_bytes` row must surface as `Err` from
/// `txn_get`, NOT as `Ok(None)` (which lied "tx not found" to
/// `eth_getTransactionByHash` pre-fix). The garbage row is injected directly
/// through a raw connection, bypassing the store's write path.
#[tokio::test]
async fn test_pgstore_s9_corrupt_envelope_row_surfaces_error() {
    let Some(store) = pg_store().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("raw connection");
    tokio::spawn(conn);

    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&rand_u64().to_be_bytes());
    h[31] = 0x59; // "S9" marker byte
    let tx_hash = TxHash::from(h);
    let hash_str = format!("{tx_hash:#x}");

    // Garbage bytes that no TxEnvelope decoder accepts.
    let garbage: Vec<u8> = vec![0xde, 0xad, 0xbe, 0xef];
    client
        .execute(
            "INSERT INTO transactions (tx_hash, envelope_bytes, signer, status, block_number) \
             VALUES ($1, $2, $3, 'success', 1)",
            &[&hash_str, &garbage, &format!("{:#x}", Address::ZERO)],
        )
        .await
        .expect("garbage row insert");

    let result = store.txn_get(tx_hash).await;
    let err = result.expect_err(
        "corrupt TxEnvelope row must surface Err — Ok(None) would mask \
         corruption as tx-not-found (S9)",
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("cannot be decoded"),
        "error must say the envelope failed to decode, got: {msg}"
    );
}

/// Cantina finding #12 (redesign) — PgStore `get_logs` returns ALL matches with
/// NO normal-operation row cap. The ORIGINAL fix fetched `CAP+1` and errored once
/// a range held more than 1000 raw rows (even when few matched the queried
/// address); the redesign pushes a SAFE SUPERSET into SQL and STREAMS the whole
/// matching set. This inserts >1000 matching logs into one block under a per-run
/// unique address and asserts every one comes back — no truncation, no error.
#[tokio::test]
async fn finding_12_getlogs_returns_all_no_row_cap() {
    let Some(store) = pg_store().await else {
        return;
    };

    // Per-run unique block + address so these rows are isolated from every other
    // suite writing synthetic_logs into the shared database.
    let block = 3_000_000 + (rand_u64() % 1_000_000);
    let run = rand_u64();
    let addr = format!("0x{run:x}dead");
    let n = 1_200usize; // comfortably past the OLD 1000-row cap

    for i in 0..n {
        let mut l = dummy_log(block, &format!("0xf12_{run}_{i}"));
        l.address = addr.clone();
        store.add_log(l).await.expect("add_log must succeed");
    }

    let filter = LogFilter {
        from_block: Some(format!("0x{block:x}")),
        to_block: Some(format!("0x{block:x}")),
        address: Some(AddressFilter::Single(addr.clone())),
        topics: None,
        block_hash: None,
    };

    let logs = store
        .get_logs(&filter, block)
        .await
        .expect("no row cap: a dense range must return ALL matches, not error");
    assert_eq!(logs.len(), n, "every matching log must be returned in full");
}

/// Cantina #12 (Copilot review) — a huge `toBlock` (u64 above i64::MAX) must NOT
/// wrap negative and silently return zero rows. `synthetic_logs.block_number` is
/// i64; the pre-fix `to = to_u64 as i64` made `toBlock ≈ u64::MAX` go negative, so
/// `block_number <= $to` rejected every row (confirmed live: a near-max toBlock
/// returned `[]`). The fix clamps `toBlock > i64::MAX` to i64::MAX ("query up to
/// the top") and returns empty only when `fromBlock` itself exceeds i64::MAX.
#[tokio::test]
async fn finding_12_getlogs_huge_toblock_does_not_wrap() {
    let Some(store) = pg_store().await else {
        return;
    };

    let block = 5_000_000 + (rand_u64() % 1_000_000);
    let run = rand_u64();
    let addr = format!("0x{run:x}beef");
    let n = 5usize;
    for i in 0..n {
        let mut l = dummy_log(block, &format!("0xhuge_{run}_{i}"));
        l.address = addr.clone();
        store.add_log(l).await.expect("add_log must succeed");
    }

    // toBlock ≈ u64::MAX — pre-fix this wrapped to a negative i64 and returned [].
    let huge_to = LogFilter {
        from_block: Some("0x0".to_string()),
        to_block: Some(format!("0x{:x}", u64::MAX)),
        address: Some(AddressFilter::Single(addr.clone())),
        topics: None,
        block_hash: None,
    };
    let logs = store
        .get_logs(&huge_to, block)
        .await
        .expect("huge toBlock must not error");
    assert_eq!(
        logs.len(),
        n,
        "toBlock above i64::MAX must clamp (query up to the top), not wrap to empty"
    );

    // fromBlock above i64::MAX is an absurd range (starts beyond every storable
    // block) — must return empty, not wrap.
    let huge_from = LogFilter {
        from_block: Some(format!("0x{:x}", u64::MAX)),
        to_block: Some(format!("0x{:x}", u64::MAX)),
        address: Some(AddressFilter::Single(addr)),
        topics: None,
        block_hash: None,
    };
    let empty = store
        .get_logs(&huge_from, block)
        .await
        .expect("huge fromBlock must not error");
    assert!(
        empty.is_empty(),
        "fromBlock above i64::MAX must return empty (absurd range)"
    );
}

/// Cantina #12 GUARDRAIL (PgStore twin) — the same property-based equivalence the
/// InMemoryStore test runs, now against the production SQL path: for a diverse
/// population + diverse filters, `PgStore::get_logs` MUST equal the pure-Rust
/// `matches()` oracle. This is what proves the SAFE SUPERSET `WHERE` + streaming
/// read reproduce `matches()` exactly (incl. MA#26 passthrough BOTH directions,
/// positional topics longer than a log's topics, and a sparse match in a dense
/// range that the old cap would have errored). Gated on DATABASE_URL.
///
/// `Scenario::new(base, run)` offsets blocks into a high window unused by other
/// suites and salts every block_hash with `run`, so the shared DB stays isolated
/// (the block_hash filter is range-independent, hence the hash salt).
#[tokio::test]
async fn getlogs_equivalence_matches_oracle_pgstore() {
    use crate::log_synthesis::equiv_fixtures::{SPARSE_MATCH_COUNT, Scenario, sorted_txs};

    let Some(store) = pg_store().await else {
        return;
    };

    let run = rand_u64();
    let base = 100_000_000 + (run % 50_000_000); // window no other suite writes to
    let scn = Scenario::new(base, run);
    for l in &scn.logs {
        store
            .add_log(l.clone())
            .await
            .expect("add_log must succeed");
    }

    for (name, f) in &scn.filters {
        let got = store
            .get_logs(f, scn.current_block)
            .await
            .unwrap_or_else(|e| panic!("filter `{name}`: get_logs errored: {e}"));
        let want = scn.reference_matches(f);
        assert_eq!(
            sorted_txs(&got),
            sorted_txs(&want),
            "filter `{name}`: PgStore result diverged from matches() oracle"
        );

        // eth_getLogs ordering contract — `ORDER BY block_number, log_index`.
        assert!(
            got.windows(2).all(|w| (w[0].block_number, w[0].log_index)
                <= (w[1].block_number, w[1].log_index)),
            "filter `{name}`: results must be ordered by (block_number, log_index)"
        );

        let got_txs = sorted_txs(&got);
        let has = |tx: &str| got_txs.iter().any(|t| t == tx);
        match *name {
            "sparse_in_dense" => assert_eq!(
                got.len(),
                SPARSE_MATCH_COUNT,
                "dense range must return exactly the sparse matches"
            ),
            "passthrough_include" => assert!(
                has(&scn.tx_passthrough),
                "UHCV passthrough must be returned when the query's topic0 includes UHCV"
            ),
            "passthrough_exclude_no_topic" => assert!(
                !has(&scn.tx_passthrough),
                "no topic0 filter ⇒ passthrough must NOT leak the UHCV log"
            ),
            "passthrough_exclude_other_topic" => assert!(
                !has(&scn.tx_passthrough),
                "topic0 excludes UHCV ⇒ passthrough must NOT return the UHCV log"
            ),
            "positional_longer_than_log" => {
                assert!(
                    !has(&scn.tx_positional_short),
                    "filter constrains topic position 2 but the log has only 2 topics ⇒ reject"
                );
                assert!(
                    has(&scn.tx_positional_long),
                    "len-3 log with matching positional topics ⇒ accept"
                );
            }
            _ => {}
        }
    }
}

// ── Faucet registry (finding #10) ────────────────────────────

/// Finding #10 — PoC + regression. A second `register_faucet` for an origin
/// already owned by a *different* faucet must CONVERGE on the
/// `(origin_address, origin_network)` unique key (`idx_faucet_origin`) instead
/// of erroring on it. Pre-fix `register_faucet` only handled
/// `ON CONFLICT (faucet_id)`, so the losing first-claim worker's INSERT hit the
/// origin unique index and errored — leaving the local registry pinned to
/// faucet A while the bridge routed by faucet B, hiding later bridge-outs.
///
/// Uses a per-run-unique origin so the test is safe to re-run against a
/// persistent dev DB (no truncation needed). No-ops without
/// `DATABASE_URL`.
#[tokio::test]
async fn test_pgstore_finding_10_register_faucet_origin_convergence() {
    use super::FaucetEntry;
    use miden_protocol::account::AccountId;

    let Some(store) = pg_store().await else {
        return;
    };

    // Origin unique per RUN (not just per test) so it never collides with a
    // prior run's rows on a persistent dev DB — no reset/truncation needed.
    let mut origin = [0xF1u8; 20];
    origin[..8].copy_from_slice(&rand_u64().to_be_bytes());
    let network = 0u32;
    let id_a = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
    let id_b = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

    let entry = |faucet_id: AccountId, symbol: &str| FaucetEntry {
        faucet_id,
        origin_address: origin,
        origin_network: network,
        symbol: symbol.to_string(),
        origin_decimals: 18,
        miden_decimals: 8,
        scale: 10,
        metadata: Vec::new(),
    };

    // First claim registers faucet A for this origin.
    store.register_faucet(entry(id_a, "TKN")).await.unwrap();
    assert_eq!(
        store
            .get_faucet_by_origin(&origin, network)
            .await
            .unwrap()
            .unwrap()
            .faucet_id,
        id_a
    );

    // Losing first-claim worker: a DIFFERENT faucet for the SAME origin.
    // Post-fix this converges (first-write wins) — no error, no split state.
    store.register_faucet(entry(id_b, "TKN")).await.unwrap();
    assert_eq!(
        store
            .get_faucet_by_origin(&origin, network)
            .await
            .unwrap()
            .unwrap()
            .faucet_id,
        id_a,
        "first-write must win the origin route"
    );
    assert!(
        store.get_faucet_by_id(id_b).await.unwrap().is_none(),
        "losing faucet must not be stranded in the registry"
    );
    assert!(store.get_faucet_by_id(id_a).await.unwrap().is_some());

    // Same faucet re-registering still refreshes metadata (idempotent by id).
    store.register_faucet(entry(id_a, "WTKN")).await.unwrap();
    assert_eq!(
        store.get_faucet_by_id(id_a).await.unwrap().unwrap().symbol,
        "WTKN"
    );
}

/// Cheap, dependency-free PRNG seed source — `std::time` is enough to
/// produce a per-run unique 8-byte prefix for the test fixtures above.
fn rand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(2_654_435_761)
}

// ── #55 accept-and-revert — BLOCKER 1 & 2 regressions on the real store ──────
//
// PG-gated (skip when DATABASE_URL is unset; run in the postgres-feature CI).
// The service-level routing logic (`acquire_claim_lock` typed outcome, the
// crash-gap nonce repair) is exercised against a genuine PgStore so the atomic
// landed classification and the nonce-repair primitive are proven on BOTH stores.

/// BLOCKER 1 — `acquire_claim_lock` classifies a claim submission atomically on
/// PgStore: fresh → Acquired; locked+no-event(in TTL) → InFlight; locked+ClaimEvent
/// → Landed (the authoritative landed detection that closes the TOCTOU); orphaned
/// (locked+no-event, TTL expired) → Acquired. LANDED beats TTL-expiry recovery.
#[tokio::test]
async fn test_pgstore_acquire_claim_lock_outcomes() {
    let Some(store) = pg_store().await else {
        return;
    };
    let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(store);
    use crate::service_send_raw_txn::{ClaimLockOutcome, acquire_claim_lock};
    let ttl = std::time::Duration::from_secs(3600);

    let base = rand_u64();
    let gi_fresh = U256::from(base.wrapping_add(1));
    let gi_inflight = U256::from(base.wrapping_add(2));
    let gi_landed = U256::from(base.wrapping_add(3));
    let gi_orphan = U256::from(base.wrapping_add(4));

    // 1. Fresh index → Acquired.
    assert_eq!(
        acquire_claim_lock(&store, gi_fresh, TxHash::ZERO, ttl)
            .await
            .unwrap(),
        ClaimLockOutcome::Acquired { fence: 1 }
    );

    // 2. Locked, no ClaimEvent, within TTL → InFlight.
    store.try_claim(gi_inflight).await.unwrap();
    assert_eq!(
        acquire_claim_lock(&store, gi_inflight, TxHash::ZERO, ttl)
            .await
            .unwrap(),
        ClaimLockOutcome::InFlight
    );

    // 3. Locked + ClaimEvent → Landed (BLOCKER 1: atomic landed classification).
    store.try_claim(gi_landed).await.unwrap();
    store
        .commit_manual_claim_event_atomic(
            format!("pg-landed-note-{base}"),
            "0xbridge",
            base % 1_000_000,
            [0u8; 32],
            "0xwatchertx",
            gi_landed.to_be_bytes::<32>(),
            0,
            &[0u8; 20],
            &[0u8; 20],
            1234,
        )
        .await
        .unwrap();
    assert_eq!(
        acquire_claim_lock(&store, gi_landed, TxHash::ZERO, ttl)
            .await
            .unwrap(),
        ClaimLockOutcome::Landed
    );
    // LANDED beats TTL-expiry recovery.
    assert_eq!(
        acquire_claim_lock(&store, gi_landed, TxHash::ZERO, std::time::Duration::ZERO)
            .await
            .unwrap(),
        ClaimLockOutcome::Landed
    );

    // 4. Orphaned (locked, no ClaimEvent, TTL expired) → Acquired (superseded).
    store.try_claim(gi_orphan).await.unwrap();
    assert_eq!(
        acquire_claim_lock(&store, gi_orphan, TxHash::ZERO, std::time::Duration::ZERO)
            .await
            .unwrap(),
        ClaimLockOutcome::Acquired { fence: 1 }
    );
}

/// BLOCKER 2 — the crash-gap nonce repair on PgStore, via a PgStore-backed
/// service. Simulate the exact durable state a crash between the receipt write and
/// `nonce_increment` leaves (a known tx row, but a STALE expected nonce), then run
/// the repair: the nonce advances exactly once (idempotent), so a rebroadcast heals
/// the nonce rather than serving stale forever — the sponsor is not wedged.
#[tokio::test]
async fn test_pgstore_crash_gap_nonce_repair() {
    let Some(store) = pg_store().await else {
        return;
    };
    let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(store);
    let service = crate::test_helpers::create_test_service_with_store(store.clone());

    let signer = Address::from([0x5au8; 20]);
    let signer_str = format!("{signer:#x}");
    let tx_hash = TxHash::from([0x5bu8; 32]);

    // Precondition: expected nonce starts at 0.
    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);
    // Simulate the crash-gap durable state: a KNOWN tx (receipt persisted) at nonce
    // 0, with the nonce NOT advanced.
    store
        .txn_begin(tx_hash, dummy_txn_entry_for(signer))
        .await
        .unwrap();
    store
        .txn_commit(tx_hash, Err("crash-gap".into()), 0, [0u8; 32])
        .await
        .unwrap();
    assert!(store.txn_get(tx_hash).await.unwrap().is_some());
    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);

    // The repair (crash-gap signature: expected == tx.nonce == 0) advances once.
    let repaired = crate::service_send_raw_txn::repair_commit_gap_nonce(&service, &signer_str, 0)
        .await
        .unwrap();
    assert!(repaired, "the crash-gap must be repaired");
    assert_eq!(
        store.nonce_get(&signer_str).await.unwrap(),
        1,
        "crash-gap nonce advanced to complete the interrupted accept"
    );

    // Idempotent: a further repair for the same tx.nonce is a no-op (expected 1 != 0).
    let repaired_again =
        crate::service_send_raw_txn::repair_commit_gap_nonce(&service, &signer_str, 0)
            .await
            .unwrap();
    assert!(!repaired_again, "repair must be idempotent");
    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);

    // A normally-advanced tx (expected 1 > tx.nonce 0) is never repaired.
    assert!(
        !crate::service_send_raw_txn::repair_commit_gap_nonce(&service, &signer_str, 0)
            .await
            .unwrap()
    );
}

/// A TxnEntry carrying a real signer for the crash-gap fixture.
fn dummy_txn_entry_for(signer: Address) -> TxnEntry {
    let tx = TxEip1559::default();
    TxnEntry {
        id: None,
        envelope: TxEnvelope::Eip1559(alloy::consensus::Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            TxHash::default(),
        )),
        signer,
        expires_at: None,
        logs: vec![],
    }
}

/// BLOCKER D — `nonce_advance_cas` on PgStore: advances only WHERE the stored nonce
/// equals `expected`, returning whether it won. Fresh address is nonce 0.
#[tokio::test]
async fn test_pgstore_nonce_advance_cas() {
    let Some(store) = pg_store().await else {
        return;
    };
    let addr = format!("0x{:040x}", rand_u64() as u128);

    // Fresh (no row = 0): CAS(0) wins → 1; a second CAS(0) loses (now 1).
    assert!(store.nonce_advance_cas(&addr, 0).await.unwrap());
    assert_eq!(store.nonce_get(&addr).await.unwrap(), 1);
    assert!(!store.nonce_advance_cas(&addr, 0).await.unwrap());
    assert_eq!(store.nonce_get(&addr).await.unwrap(), 1);

    // CAS at the wrong expected loses; at the right expected wins.
    assert!(!store.nonce_advance_cas(&addr, 5).await.unwrap());
    assert!(store.nonce_advance_cas(&addr, 1).await.unwrap());
    assert_eq!(store.nonce_get(&addr).await.unwrap(), 2);
}

/// BLOCKER C — `commit_reverted_receipt_and_advance_nonce` on PgStore: one
/// transaction writes a COMMITTED reverted receipt (never pending) AND CAS-advances
/// the nonce; idempotent on tx_hash, and the CAS is a no-op once the nonce moved.
#[tokio::test]
async fn test_pgstore_commit_reverted_receipt_and_advance_nonce() {
    let Some(store) = pg_store().await else {
        return;
    };
    let base = rand_u64();
    let signer = Address::from([(base % 251) as u8 + 1; 20]);
    let signer_str = format!("{signer:#x}");
    let tx_hash = TxHash::from([(base % 241) as u8 + 2; 32]);

    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);

    // First call: receipt committed-reverted + nonce CAS-advanced (expected 0 → 1).
    let advanced = store
        .commit_reverted_receipt_and_advance_nonce(
            tx_hash,
            dummy_txn_entry_for(signer),
            "landed (AlreadyClaimed) #55".into(),
            7,
            [0u8; 32],
            &signer_str,
            0,
        )
        .await
        .unwrap();
    assert!(
        advanced,
        "the nonce CAS wins on the sync accept path (expected == 0)"
    );
    // Receipt is COMMITTED (non-null) and reverted (status 0x0).
    let (result, block) = store
        .txn_receipt(tx_hash)
        .await
        .unwrap()
        .expect("receipt is committed, never pending");
    assert!(result.is_err(), "status 0x0 reverted");
    assert_eq!(block, 7);
    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);

    // Idempotent re-entry (expected 0 again): receipt re-affirmed, nonce NOT
    // double-advanced (the CAS no-ops because the nonce already moved to 1).
    let advanced_again = store
        .commit_reverted_receipt_and_advance_nonce(
            tx_hash,
            dummy_txn_entry_for(signer),
            "landed (AlreadyClaimed) #55".into(),
            7,
            [0u8; 32],
            &signer_str,
            0,
        )
        .await
        .unwrap();
    assert!(
        !advanced_again,
        "re-entry must not double-advance the nonce"
    );
    assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);
    assert!(store.txn_receipt(tx_hash).await.unwrap().is_some());
}

/// BLOCKER 1 — `reserve_nonce` on PgStore: the first tx to reserve a (signer, nonce)
/// wins; a different tx at the same slot loses (HeldBy the winner); the same tx is
/// idempotent (HeldBy itself); a different nonce is free.
#[tokio::test]
async fn test_pgstore_reserve_nonce() {
    let Some(store) = pg_store().await else {
        return;
    };
    use crate::store::NonceReservation;
    let base = rand_u64();
    let addr = format!("0x{:040x}", base as u128);
    let h1 = TxHash::from([(base % 251) as u8 + 1; 32]);
    let h2 = TxHash::from([(base % 241) as u8 + 2; 32]);
    let lease = std::time::Duration::from_secs(90);

    // Fresh → Won(fence).
    let NonceReservation::Won { fence } = store.reserve_nonce(&addr, 5, h1, lease).await.unwrap()
    else {
        panic!("fresh slot must be Won");
    };
    // Same tx, valid lease → OwnedBySame.
    assert_eq!(
        store.reserve_nonce(&addr, 5, h1, lease).await.unwrap(),
        NonceReservation::OwnedBySame
    );
    // Different tx, same slot → HeldByOther(winner h1).
    assert_eq!(
        store.reserve_nonce(&addr, 5, h2, lease).await.unwrap(),
        NonceReservation::HeldByOther(h1)
    );
    // release-FAILURE → same tx takes over (fence bumps).
    store
        .release_reservation(&addr, 5, h1, fence, false)
        .await
        .unwrap();
    let NonceReservation::Won { fence: fence2 } =
        store.reserve_nonce(&addr, 5, h1, lease).await.unwrap()
    else {
        panic!("after release-failure the same tx must retake ownership");
    };
    assert!(fence2 > fence, "takeover bumps the fence");
    // release-SUCCESS → the exact durable tx can resume after restart.
    store
        .release_reservation(&addr, 5, h1, fence2, true)
        .await
        .unwrap();
    assert!(matches!(
        store.reserve_nonce(&addr, 5, h1, lease).await.unwrap(),
        NonceReservation::Won { fence } if fence > fence2
    ));
    // Different nonce → free.
    assert!(matches!(
        store.reserve_nonce(&addr, 6, h2, lease).await.unwrap(),
        NonceReservation::Won { .. }
    ));
}

/// Wedge #5 — abandoned-slot reclamation. An admission that crashes AFTER
/// `reserve_nonce` but BEFORE durable admission leaves the slot `executing`
/// with an expired lease and NO `transactions` row; a DIFFERENT tx must then
/// take it over (fence bumps, zombie fenced out). Guard: the same expired slot
/// WITH a durable `transactions` row stays hash-bound (HeldByOther).
#[tokio::test]
async fn test_pgstore_wedge5_abandoned_slot_reclamation() {
    let Some(store) = pg_store().await else {
        return;
    };
    use crate::store::NonceReservation;
    let base = rand_u64();
    let addr = format!("0x{:040x}", (base ^ 0x5ED5) as u128);
    let h1 = TxHash::from([(base % 199) as u8 + 3; 32]);
    let h2 = TxHash::from([(base % 193) as u8 + 4; 32]);
    let lease = std::time::Duration::from_secs(90);

    let NonceReservation::Won { fence } = store.reserve_nonce(&addr, 9, h1, lease).await.unwrap()
    else {
        panic!("fresh slot must be Won");
    };
    // A VALID executing lease still hard-rejects a different tx.
    assert_eq!(
        store.reserve_nonce(&addr, 9, h2, lease).await.unwrap(),
        NonceReservation::HeldByOther(h1)
    );

    // Backdate the lease via raw SQL — the crashed-admission signature
    // (executing, expired, never durably admitted).
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (client, conn) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls)
        .await
        .expect("raw connection");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .execute(
            "UPDATE nonce_reservations SET lease_expires_at = now() - interval '1 second'
             WHERE signer = $1 AND nonce = 9",
            &[&addr.to_lowercase()],
        )
        .await
        .expect("backdate lease");

    // Expired + unadmitted → the DIFFERENT tx reclaims with a bumped fence.
    let NonceReservation::Won { fence: fence2 } =
        store.reserve_nonce(&addr, 9, h2, lease).await.unwrap()
    else {
        panic!("an abandoned (expired, unadmitted) slot must be reclaimable by a different tx");
    };
    assert!(fence2 > fence, "reclamation must bump the fence");
    // The zombie owner is fenced out; the reclaiming owner keeps the slot.
    store
        .release_reservation(&addr, 9, h1, fence, false)
        .await
        .unwrap();
    assert_eq!(
        store.reserve_nonce(&addr, 9, h2, lease).await.unwrap(),
        NonceReservation::OwnedBySame,
        "the zombie's fenced-out release must not evict the reclaiming owner"
    );

    // Guard: expired but durably ADMITTED (transactions row exists) stays held.
    let h3 = TxHash::from([(base % 191) as u8 + 5; 32]);
    let h4 = TxHash::from([(base % 181) as u8 + 6; 32]);
    assert!(matches!(
        store.reserve_nonce(&addr, 10, h3, lease).await.unwrap(),
        NonceReservation::Won { .. }
    ));
    store.txn_begin(h3, dummy_txn_entry()).await.unwrap();
    client
        .execute(
            "UPDATE nonce_reservations SET lease_expires_at = now() - interval '1 second'
             WHERE signer = $1 AND nonce = 10",
            &[&addr.to_lowercase()],
        )
        .await
        .expect("backdate lease");
    assert_eq!(
        store.reserve_nonce(&addr, 10, h4, lease).await.unwrap(),
        NonceReservation::HeldByOther(h3),
        "an expired slot with a durable transactions row must stay hash-bound"
    );
}

/// Wedge #5 (PR#145 blocker 1) — the identical production transition on
/// PostgreSQL: a slot released as FAILURE whose hash was never durably admitted
/// (pre-admission error path, e.g. writer-queue saturation after the
/// reservation) is reclaimable by a DIFFERENT tx immediately, fence bumped; a
/// `released_failure` slot WITH a durable `transactions` row stays hash-bound.
#[tokio::test]
async fn test_pgstore_wedge5_released_failure_reclamation() {
    let Some(store) = pg_store().await else {
        return;
    };
    use crate::store::NonceReservation;
    let base = rand_u64();
    let addr = format!("0x{:040x}", (base ^ 0x5EDF) as u128);
    let h1 = TxHash::from([(base % 197) as u8 + 7; 32]);
    let h2 = TxHash::from([(base % 179) as u8 + 8; 32]);
    let lease = std::time::Duration::from_secs(90);

    // Released failure, NEVER durably admitted → reclaimable immediately.
    let NonceReservation::Won { fence } = store.reserve_nonce(&addr, 11, h1, lease).await.unwrap()
    else {
        panic!("fresh slot must be Won");
    };
    store
        .release_reservation(&addr, 11, h1, fence, false)
        .await
        .unwrap();
    let NonceReservation::Won { fence: fence2 } =
        store.reserve_nonce(&addr, 11, h2, lease).await.unwrap()
    else {
        panic!("released_failure without durable admission must be reclaimable");
    };
    assert!(fence2 > fence, "reclamation must bump the fence");

    // Released failure WITH a durable transactions row → stays hash-bound.
    let h3 = TxHash::from([(base % 173) as u8 + 9; 32]);
    let h4 = TxHash::from([(base % 167) as u8 + 10; 32]);
    let NonceReservation::Won { fence: fence3 } =
        store.reserve_nonce(&addr, 12, h3, lease).await.unwrap()
    else {
        panic!("fresh slot must be Won");
    };
    store.txn_begin(h3, dummy_txn_entry()).await.unwrap();
    store
        .release_reservation(&addr, 12, h3, fence3, false)
        .await
        .unwrap();
    assert_eq!(
        store.reserve_nonce(&addr, 12, h4, lease).await.unwrap(),
        NonceReservation::HeldByOther(h3),
        "a released_failure slot with a durable transactions row must stay hash-bound"
    );
}

/// BLOCKER 1 — FULL two-replica shared-PostgreSQL races. Two PgStore handles over
/// the SAME database (two "replicas") reserve the SAME (signer, nonce) slot.
#[tokio::test]
async fn test_pgstore_two_replica_reservation_races() {
    let Some(replica_a) = pg_store().await else {
        return;
    };
    let Some(replica_b) = pg_store().await else {
        return;
    };
    use crate::store::NonceReservation;
    let lease = std::time::Duration::from_secs(90);
    let base = rand_u64();

    // (1) DIFFERENT-hash race at the same slot: exactly one wins, the other is
    //     HeldByOther and must NOT execute.
    let addr = format!("0x{:040x}", base as u128);
    let ha = TxHash::from([(base % 251) as u8 + 7; 32]);
    let hb = TxHash::from([(base % 241) as u8 + 8; 32]);
    let ra = replica_a.reserve_nonce(&addr, 3, ha, lease).await.unwrap();
    let rb = replica_b.reserve_nonce(&addr, 3, hb, lease).await.unwrap();
    let a_won = matches!(ra, NonceReservation::Won { .. });
    let b_won = matches!(rb, NonceReservation::Won { .. });
    // Replica A reserved first (sequential here), so A wins and B is HeldByOther.
    assert!(a_won, "the first replica to reserve wins: {ra:?}");
    assert!(
        !b_won,
        "the second replica with a DIFFERENT hash must not win: {rb:?}"
    );
    assert_eq!(rb, NonceReservation::HeldByOther(ha));

    // (2) SAME-hash race at another slot: replica A wins ownership; replica B
    //     submitting the IDENTICAL tx while A's lease is valid gets OwnedBySame and
    //     must DEDUP (not execute) — no double Miden work.
    let addr2 = format!("0x{:040x}", base.wrapping_add(1) as u128);
    let h = TxHash::from([(base % 233) as u8 + 9; 32]);
    let ra2 = replica_a.reserve_nonce(&addr2, 4, h, lease).await.unwrap();
    assert!(
        matches!(ra2, NonceReservation::Won { .. }),
        "A wins: {ra2:?}"
    );
    let rb2 = replica_b.reserve_nonce(&addr2, 4, h, lease).await.unwrap();
    assert_eq!(
        rb2,
        NonceReservation::OwnedBySame,
        "the same tx on another replica while the owner's lease is valid must dedup, \
         not double-execute"
    );
}

/// BLOCKER 4 — `commit_reverted_receipt_and_advance_nonce` on PgStore is CONDITIONAL:
/// it never overwrites a REAL receipt (pending or successful) with status 0.
#[tokio::test]
async fn test_pgstore_reverted_receipt_conditional() {
    let Some(store) = pg_store().await else {
        return;
    };
    let base = rand_u64();
    let signer = Address::from([(base % 251) as u8 + 3; 20]);
    let signer_str = format!("{signer:#x}");

    // (a) SUCCESS receipt must survive.
    let tx_ok = TxHash::from([(base % 239) as u8 + 4; 32]);
    store
        .txn_begin(tx_ok, dummy_txn_entry_for(signer))
        .await
        .unwrap();
    store.txn_commit(tx_ok, Ok(()), 5, [0u8; 32]).await.unwrap();
    store
        .commit_reverted_receipt_and_advance_nonce(
            tx_ok,
            dummy_txn_entry_for(signer),
            "revert".into(),
            9,
            [0u8; 32],
            &signer_str,
            0,
        )
        .await
        .unwrap();
    let (r_ok, _) = store.txn_receipt(tx_ok).await.unwrap().expect("receipt");
    assert!(
        r_ok.is_ok(),
        "a REAL success receipt must NOT be overwritten to failed"
    );

    // (b) PENDING receipt must stay pending.
    let tx_pending = TxHash::from([(base % 233) as u8 + 5; 32]);
    store
        .txn_begin(tx_pending, dummy_txn_entry_for(signer))
        .await
        .unwrap();
    store
        .record_tx_note_link(&format!("{tx_pending:#x}"), "real-note")
        .await
        .unwrap();
    store
        .commit_reverted_receipt_and_advance_nonce(
            tx_pending,
            dummy_txn_entry_for(signer),
            "revert".into(),
            9,
            [0u8; 32],
            &signer_str,
            1,
        )
        .await
        .unwrap();
    assert!(
        store.txn_receipt(tx_pending).await.unwrap().is_none(),
        "a REAL pending receipt must stay pending, not be finalised to failed"
    );

    // (c) ABSENT hash → reverted receipt IS written.
    let tx_new = TxHash::from([(base % 229) as u8 + 6; 32]);
    store
        .commit_reverted_receipt_and_advance_nonce(
            tx_new,
            dummy_txn_entry_for(signer),
            "revert".into(),
            9,
            [0u8; 32],
            &signer_str,
            2,
        )
        .await
        .unwrap();
    let (r_new, _) = store.txn_receipt(tx_new).await.unwrap().expect("receipt");
    assert!(
        r_new.is_err(),
        "an absent hash gets the reverted (status 0x0) receipt"
    );
}

/// PostgreSQL claim reclaim is fenced through the atomic submitted-state + note-link seal.
#[tokio::test]
async fn test_pgstore_claim_reclaim_fences_stale_owner() {
    let Some(store) = pg_store().await else {
        return;
    };
    let base = rand_u64();
    let gi = U256::from(base);
    let tx_a = TxHash::from([(base % 211) as u8 + 12; 32]);
    let tx_b = TxHash::from([(base % 199) as u8 + 13; 32]);
    let a = store
        .try_claim_fenced(gi, tx_a, std::time::Duration::ZERO)
        .await
        .unwrap()
        .unwrap();
    let b = store
        .try_reclaim_claim_fenced(gi, tx_b, std::time::Duration::from_secs(90))
        .await
        .unwrap()
        .unwrap();
    assert!(b.fence > a.fence);
    assert!(
        !store
            .prepare_claim_submission_fenced(gi, tx_a, a.fence, tx_a, "stale", "stale-id", 100,)
            .await
            .unwrap()
    );
    assert!(!store.unclaim_fenced(&gi, tx_a, a.fence).await.unwrap());
    assert!(
        store
            .prepare_claim_submission_fenced(gi, tx_b, b.fence, tx_b, "winner", "winner-id", 100,)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .get_note_link_for_tx(&format!("{tx_b:#x}"))
            .await
            .unwrap()
            .as_deref(),
        Some("winner")
    );
}

/// PostgreSQL must permanently bind an ambiguous nonce slot to the first hash.
#[tokio::test]
async fn test_pgstore_different_tx_cannot_take_over() {
    let Some(store) = pg_store().await else {
        return;
    };
    use crate::store::NonceReservation;
    let lease = std::time::Duration::from_secs(90);
    let base = rand_u64();
    let addr = format!("0x{:040x}", base as u128);
    let ha = TxHash::from([(base % 251) as u8 + 10; 32]);
    let hb = TxHash::from([(base % 241) as u8 + 11; 32]);

    let NonceReservation::Won { fence } = store.reserve_nonce(&addr, 1, ha, lease).await.unwrap()
    else {
        panic!("fresh must win");
    };
    store
        .release_reservation(&addr, 1, ha, fence, false)
        .await
        .unwrap();
    assert_eq!(
        store.reserve_nonce(&addr, 1, hb, lease).await.unwrap(),
        NonceReservation::HeldByOther(ha)
    );
    assert!(matches!(
        store.reserve_nonce(&addr, 1, ha, lease).await.unwrap(),
        NonceReservation::Won { .. }
    ));
}
/// PostgreSQL reservations remain stable and un-emitted leaves stay retryable.
#[tokio::test]
async fn cantina7_pg_reservation_and_emitted_accounting() {
    let Some(store) = pg_store().await else {
        return;
    };
    let leaf0 = format!("resv0-{:x}", rand_u64()); // skipped/quarantined leaf
    let leaf1 = format!("resv1-{:x}", rand_u64()); // valid leaf
    let counter_before = store.get_deposit_count().await.unwrap();

    let i0 = store.reserve_deposit_index(&leaf0).await.unwrap();
    assert!(
        !store.is_note_processed(&leaf0).await.unwrap(),
        "a reservation with no emission is NOT processed — stays re-attemptable"
    );
    // Idempotent: re-reserving returns the same index, no double-allocation.
    assert_eq!(store.reserve_deposit_index(&leaf0).await.unwrap(), i0);

    // Leaf 1 gets its own stable reservation, then EMITS using that reservation.
    // Other shared-DB tests may allocate between these two calls, so adjacency is not assumed.
    let i1 = store.reserve_deposit_index(&leaf1).await.unwrap();
    let tx_hash = format!("0x{:064x}", rand_u64());
    let dc = store
        .commit_b2agg_event_atomic(
            leaf1.clone(),
            "0x00000000000000000000000000000000000000aa",
            9,
            [0u8; 32],
            &tx_hash,
            0,
            0,
            &[0u8; 20],
            1,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        dc, i1,
        "commit REUSES the reserved index (no re-allocation)"
    );
    assert!(
        store.is_note_processed(&leaf1).await.unwrap(),
        "an emitted leaf is processed"
    );

    // The raw counter includes both reservations; concurrent tests may only increase it further.
    assert!(
        store.get_deposit_count().await.unwrap() >= counter_before + 2,
        "durable counter reflects every reserved leaf directly"
    );

    // Retry/restart idempotence: re-committing leaf1 reuses the index, no second event.
    let dc2 = store
        .commit_b2agg_event_atomic(
            leaf1.clone(),
            "0x00000000000000000000000000000000000000aa",
            9,
            [0u8; 32],
            &tx_hash,
            0,
            0,
            &[0u8; 20],
            1,
            &[0u8; 20],
            1_000,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(dc2, i1, "index stable across retry");
}

/// Review blocker 4 — the DERIVED hash must survive a real PG store→decode round-trip.
/// A synthetic claim tx is stored under its derived hash (keccak(tag||note_id), NOT an RLP
/// hash) while PG persists only EIP-2718/RLP bytes; txn_get decodes and the envelope's hash
/// is RECOMPUTED as the RLP hash. `to_rpc_transaction` MUST re-assert the store key so a
/// client that fetched by the derived hash gets back `.hash == derived hash`.
#[tokio::test]
async fn cantina136_derived_hash_survives_pg_round_trip() {
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::primitives::TxKind;
    let Some(store) = pg_store().await else {
        return;
    };
    let block_state = crate::block_state::BlockState::new();

    // A synthetic claim tx keyed by a DERIVED hash unrelated to its RLP encoding.
    let derived =
        crate::claim_watcher::derive_manual_claim_tx_hash(&format!("rt-{:x}", rand_u64()));
    let tx_hash: TxHash = derived.parse().unwrap();
    let bridge_addr = Address::from([0xC8; 20]);
    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: TxKind::Call(bridge_addr),
        value: U256::ZERO,
        input: vec![0xDE, 0xAD, 0xBE, 0xEF].into(),
    };
    let envelope = TxEnvelope::Legacy(Signed::new_unchecked(
        tx,
        Signature::new(U256::from(1), U256::from(1), false),
        tx_hash,
    ));
    // Sanity: the envelope's RLP-recomputed hash is NOT the derived key (that is the trap).
    assert_ne!(
        format!("{:#x}", envelope.tx_hash()),
        derived,
        "fixture: the derived key must differ from the RLP hash"
    );

    store
        .txn_begin(
            tx_hash,
            TxnEntry {
                id: None,
                envelope,
                signer: bridge_addr,
                expires_at: None,
                logs: Vec::new(),
            },
        )
        .await
        .unwrap();
    store
        .txn_commit(tx_hash, Ok(()), 8831, [0xAA; 32])
        .await
        .unwrap();

    // Round-trip through PG: read back (decodes RLP → recomputes envelope hash) and render.
    let data = store
        .txn_get(tx_hash)
        .await
        .unwrap()
        .expect("row persisted under the derived hash");
    let json = data.to_rpc_transaction(tx_hash, &block_state);
    assert_eq!(
        json["hash"].as_str().unwrap().to_lowercase(),
        derived.to_lowercase(),
        "getTransactionByHash(derived).hash MUST equal the derived hash after a PG round-trip"
    );
    // The calldata is also intact (the e2e asserts .input; this pins .hash too).
    assert_eq!(json["input"].as_str().unwrap().to_lowercase(), "0xdeadbeef");
}

/// #148 — the PgStore durable claim-calldata repair backlog (migration 019).
/// Twin of the memory `count_claim_events_awaiting_calldata_gates_recovery`:
///   * seed classifies a ClaimEvent with no successful calldata as awaiting,
///   * a bare `txn_begin` (crash before a successful commit) stays counted (B1),
///   * only a successful `txn_commit` drains it,
///   * `count` reads the tiny set, not the historical-log join (B2).
/// Delta-based (the DB is shared across pgstore tests) with a unique tx_hash.
#[tokio::test]
async fn test_pgstore_claim_calldata_repair_backlog() {
    let Some(store) = pg_store().await else {
        return;
    };
    reset_state(&store).await;

    // Unique claim tx_hash so this test is isolated from other pgstore tests'
    // rows on the shared database.
    let tx_hash = TxHash::from([0x48u8; 32]);
    let tx_hash_str = format!("{tx_hash:#x}");

    let base = store.seed_claim_calldata_repair_backlog().await.unwrap();

    // A ClaimEvent whose calldata envelope is MISSING → +1 after seed.
    store
        .add_claim_event(
            "0xbridge",
            10,
            [0u8; 32],
            &tx_hash_str,
            &[0u8; 32],
            1,
            &[0u8; 20],
            &[0u8; 20],
            100,
        )
        .await
        .unwrap();
    let after_add = store.seed_claim_calldata_repair_backlog().await.unwrap();
    assert_eq!(
        after_add,
        base + 1,
        "seed classifies the missing-calldata claim as awaiting"
    );

    // B1 — a bare `txn_begin` (no successful commit) must NOT drain the backlog.
    let entry = TxnEntry {
        id: None,
        envelope: TxEnvelope::Eip1559(alloy::consensus::Signed::new_unchecked(
            TxEip1559::default(),
            Signature::test_signature(),
            tx_hash,
        )),
        signer: Address::ZERO,
        expires_at: None,
        logs: vec![],
    };
    store.txn_begin(tx_hash, entry).await.unwrap();
    assert_eq!(
        store.count_claim_events_awaiting_calldata().await.unwrap(),
        after_add,
        "txn_begin alone does not repair the claim — stays counted (blocker 1)"
    );
    assert_eq!(
        store.seed_claim_calldata_repair_backlog().await.unwrap(),
        after_add,
        "re-seed still classifies a begun-but-uncommitted claim as awaiting (blocker 1)"
    );

    // A SUCCESSFUL commit drains exactly this claim (delta back to base).
    store
        .txn_commit(tx_hash, Ok(()), 11, [0u8; 32])
        .await
        .unwrap();
    assert_eq!(
        store.count_claim_events_awaiting_calldata().await.unwrap(),
        base,
        "a successful calldata commit clears the claim from the backlog"
    );
}
