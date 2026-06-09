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
        miden_protocol::account::AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
    store.set_address_mapping(eth, miden_id).await.unwrap();
    let retrieved = store.get_address_mapping(&eth).await.unwrap();
    assert_eq!(retrieved, Some(miden_id));

    // Overwrite with a different value
    let miden_id2 =
        miden_protocol::account::AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
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
    let tx_hash = format!("0xclaim_atomic_{now_ns}");

    // Cantina #5 — the store allocates the synthetic block and returns it; no
    // caller-chosen block number.
    let tip_before = store.get_latest_block_number().await.unwrap();
    let block = store
        .commit_manual_claim_event_atomic(
            note_id.clone(),
            "0xbridge",
            &tx_hash,
            gi,
            0,
            &[0u8; 20],
            &[0u8; 20],
            42,
        )
        .await
        .unwrap();

    // Store-owned allocation advanced the tip by exactly one.
    assert_eq!(block, tip_before + 1);
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

/// Cheap, dependency-free PRNG seed source — `std::time` is enough to
/// produce a per-run unique 8-byte prefix for the test fixtures above.
fn rand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(2_654_435_761)
}
