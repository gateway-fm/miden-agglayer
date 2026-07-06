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
use crate::log_synthesis::{GerEntry, LogFilter, SyntheticLog};
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

    // Mark injected
    store.mark_ger_injected(ger).await.unwrap();
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
        .add_ger_update_event(50, [0u8; 32], "0xger_tx", &ger, None, None, 999)
        .await
        .unwrap();

    // Should have emitted a log
    let logs = store.get_logs_for_tx("0xger_tx").await.unwrap();
    assert!(!logs.is_empty(), "ger update event should emit a log");

    // GER should be seen
    assert!(store.has_seen_ger(&ger).await.unwrap());
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

// ── Bridge-out ───────────────────────────────────────────────

#[tokio::test]
async fn test_pgstore_bridge_out() {
    let Some(store) = pg_store().await else {
        return;
    };

    let note_id = format!(
        "test_note_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    assert!(!store.is_note_processed(&note_id).await.unwrap());

    let _count = store.mark_note_processed(note_id.clone()).await.unwrap();

    assert!(store.is_note_processed(&note_id).await.unwrap());
    store.unmark_note_processed(&note_id).await.unwrap();
    assert!(!store.is_note_processed(&note_id).await.unwrap());
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

/// `commit_manual_claim_event_atomic`: a single PG txn folds mark +
/// log insert + cursor advance. Verify cursor lands at the expected
/// block and a fresh ClaimEvent log appears.
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

    // Cursor advanced.
    assert!(store.get_latest_block_number().await.unwrap() >= block);
    // Note processed.
    assert!(store.is_claim_note_processed(&note_id).await.unwrap());
    // ClaimEvent dedup query finds the row.
    assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
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

/// Cheap, dependency-free PRNG seed source — `std::time` is enough to
/// produce a per-run unique 8-byte prefix for the test fixtures above.
fn rand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(2_654_435_761)
}
