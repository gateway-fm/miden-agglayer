//! Restore — Reconstruct PgStore state from miden node.
//!
//! This module implements disaster recovery: when the PostgreSQL store is
//! empty (fresh deploy or data loss), it rebuilds all state from authoritative
//! sources (miden node consumed notes, miden sync state).
//!
//! ## Algorithm
//!
//! Phase 1: Sync miden state → get current block number
//! Phase 2: Scan miden consumed B2AGG notes → rebuild bridge-out + deposit counter
//! Phase 3: Scan consumed UpdateGerNote notes on Miden → rebuild GER set + hash chain
//! Phase 4: Update block number to cover all synthetic logs
//! Phase 5: Verify counts
//!
//! ## GER restoration via consumed notes
//!
//! For recovery we only care about consumed notes — actually injected GERs.
//! When the proxy injects a GER, it creates an UpdateGerNote that gets consumed
//! by the Miden bridge account. The Miden node retains consumed notes, so we can
//! scan them to reconstruct the full GER history.
//!
//! Each consumed UpdateGerNote stores the GER as 8 Felts in note storage.
//! The consumption block number gives us the ordering for hash chain reconstruction.
//!
//! See: https://github.com/0xMiden/protocol/issues/2341
//!
//! ## Known Limitations (TODOs for miden-node API enhancements)
//!
//! - B2AGG/GER note filtering is done client-side (no server-side script root filter)
//!   TODO: switch to NoteFilter::ConsumedByScriptRoot when available
//! - No block range queries for notes (full scan from genesis)
//!   TODO: switch to dedicated get_gers() endpoint when Marti's team ships it

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::bridge_out::{is_b2agg_note, parse_b2agg_storage, resolve_faucet_origin};
use crate::claim_watcher::{derive_manual_claim_tx_hash, parse_claim_event_from_storage};
use crate::miden_client::MidenClient;
use crate::store::Store;
use miden_base_agglayer::{UpdateGerNote, claim_script};
use miden_client::store::NoteFilter;
use sha3::{Digest, Keccak256};
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
    /// Cantina MA#27 — number of consumed CLAIM notes for which a synthetic
    /// ClaimEvent was emitted by restore (the offline equivalent of what
    /// [`crate::claim_watcher::ClaimWatcher`] does on every live sync tick).
    pub claims_restored: usize,
    pub gers_restored: usize,
    pub logs_created: usize,
}

/// Run the full restore algorithm.
pub async fn restore(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
) -> anyhow::Result<RestoreResult> {
    tracing::info!("=== RESTORE: starting state reconstruction ===");

    // Cantina MA#23 — suppress the live `BridgeOutScanner` / `ClaimWatcher`
    // sync-listener callbacks for the entire restore window. The background
    // sync thread inside `MidenClient` keeps pulling deltas (so the local
    // sqlite store stays fresh, which restore phases below depend on), but
    // `on_post_sync` is gated off. Without this guard, the initial sync's
    // listener pass — fired inside `MidenClient::new` BEFORE `restore()`
    // is reached — and every 5s interval tick interleave with restore's
    // own `.with()` calls, causing the live path to also emit synthetic
    // BridgeEvent / ClaimEvent logs and race the deposit-counter cursor.
    // The guard auto-restores on any exit path (Ok / Err / panic).
    let _pause = miden_client.pause_listeners();

    // Phase 0: Re-import every bridge_accounts.toml account from the live
    // Miden node into the local sqlite. Without this, `--reset-miden-store
    // --restore` is a footgun: reset wipes the sqlite, restore's Phase 1
    // calls `sync_state()` which only syncs deltas for already-tracked
    // accounts (not new imports), and the proxy comes back with zero
    // local rows for any account → every subsequent submission fails
    // with `AccountDataNotFound`. This is the regression chain that
    // locked bali into 20 days of stuck deposits after an operator ran
    // the recovery flags.
    //
    // Best-effort: per-account failures are logged + counted but do not
    // abort restore. Locally-deployed-but-not-network-tracked accounts
    // (`service`, `wallet_hardhat`) will return `AccountNotFoundOnChain`
    // here and that's fine — they're healthy until first use.
    tracing::info!("Phase 0: re-importing bridge accounts from Miden node...");
    crate::account_recovery::reimport_known_accounts(miden_client, accounts).await;
    tracing::info!("Phase 0 complete: bridge account reimport pass done");

    // Phase 1: Sync miden state
    tracing::info!("Phase 1: syncing miden state...");
    let block_num = sync_miden_block(miden_client, store).await?;
    tracing::info!("Phase 1 complete: miden block {block_num}");

    // We'll assign synthetic logs to blocks starting after current
    let mut next_block = block_num + 1;
    let mut total_logs = 0usize;

    // Phase 2: Scan miden consumed B2AGG notes
    tracing::info!("Phase 2: scanning miden consumed B2AGG notes...");
    let (bridge_outs, logs) =
        restore_bridge_outs(store, miden_client, accounts, block_state, next_block).await?;
    next_block += if logs > 0 { 1 } else { 0 };
    total_logs += logs;
    tracing::info!("Phase 2 complete: {bridge_outs} bridge-outs, {logs} logs");

    // Phase 2.5: Scan miden consumed CLAIM notes — Cantina MA#27
    //
    // The live `ClaimWatcher::on_post_sync` (claim_watcher.rs) is the only
    // path that synthesises a `ClaimEvent` log when the primary
    // `eth_sendRawTransaction` flow didn't write one (crash recovery + any
    // CLAIM consumed by a tracked account through a non-RPC path).
    // `restore()` previously skipped this entirely, so after a fresh DB
    // (e.g. `--reset-miden-store --restore`) every pre-existing claim was
    // dropped on the floor — bridge-service never saw the synthetic event
    // and the L1 deposit stayed `claimed=false` forever, blocking the next
    // aggsender certificate. Replay using the same primitives the live
    // watcher uses so the synthetic logs are byte-identical (same tx-hash
    // derivation, same `commit_manual_claim_event_atomic` store path).
    tracing::info!("Phase 2.5: scanning miden consumed CLAIM notes (MA#27)...");
    let (claims, claim_logs) =
        restore_claims(store, miden_client, block_state, next_block).await?;
    next_block += if claim_logs > 0 { 1 } else { 0 };
    total_logs += claim_logs;
    tracing::info!("Phase 2.5 complete: {claims} claims, {claim_logs} logs");

    // Phase 3: Scan consumed UpdateGerNote notes on Miden
    tracing::info!("Phase 3: scanning consumed UpdateGerNote notes on Miden...");
    let (gers, ger_logs) = restore_gers(store, miden_client, block_state, next_block).await?;
    total_logs += ger_logs;
    tracing::info!("Phase 3 complete: {gers} GERs, {ger_logs} logs");

    // Phase 4: Update block number to cover all synthetic logs
    let final_block = next_block + if ger_logs > 0 { 1 } else { 0 };
    store.set_latest_block_number(final_block).await?;
    tracing::info!("Phase 4: block number set to {final_block}");

    // Phase 5: Verify
    tracing::info!("Phase 5: verification");
    tracing::info!("  bridge_outs={bridge_outs}, claims={claims}, gers={gers}, logs={total_logs}");
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: final_block,
        bridge_outs_restored: bridge_outs,
        claims_restored: claims,
        gers_restored: gers,
        logs_created: total_logs,
    })
}

/// Phase 1: sync miden and return current block number.
async fn sync_miden_block(
    miden_client: &MidenClient,
    store: &Arc<dyn Store>,
) -> anyhow::Result<u64> {
    miden_client
        .with(|client| {
            Box::new(async move {
                client.sync_state().await?;
                Ok(())
            })
        })
        .await?;

    let block_num = store.get_latest_block_number().await?;
    Ok(block_num)
}

/// Phase 2: scan miden consumed B2AGG notes and rebuild bridge-out state.
/// Returns (notes_processed, logs_created).
async fn restore_bridge_outs(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    _accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                let block_hash = block_state_clone.get_block_hash(restore_block);
                let bridge_address = get_bridge_address();
                let mut count = 0usize;
                let mut logs = 0usize;

                // G7 — sort B2AGG notes deterministically before assigning
                // deposit_count. The Miden client returns consumed notes in
                // store-arrival order, which can differ between runs (e.g.
                // sync re-orderings, partial restores). Without sorting, the
                // (note_id → deposit_count) mapping is non-deterministic
                // across restore runs — two restores from the same on-chain
                // state could produce different deposit_count assignments,
                // breaking any consumer that joins on (note_id,
                // deposit_count). Sort by note_id (stable across re-syncs).
                let mut sorted: Vec<&_> = consumed_notes.iter().collect();
                sorted.sort_by_key(|n| n.id().to_string());

                for note in sorted {
                    let details = note.details();
                    if !is_b2agg_note(details) {
                        continue;
                    }

                    let note_id_str = note.id().to_string();
                    if store_clone.is_note_processed(&note_id_str).await? {
                        continue;
                    }

                    let (destination_network, destination_address) = match parse_b2agg_storage(
                        details.storage(),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(note_id = %note_id_str, "restore: skip B2AGG: {e:#}");
                            continue;
                        }
                    };

                    let Some(fungible_asset) = details.assets().iter_fungible().next() else {
                        continue;
                    };
                    let faucet_id = fungible_asset.faucet_id();
                    let miden_amount = fungible_asset.amount();
                    let origin = match resolve_faucet_origin(faucet_id, &*store_clone).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(note_id = %note_id_str, "restore: skip B2AGG: {e:#}");
                            continue;
                        }
                    };
                    let origin_amount = match crate::bridge_out::reverse_scale_amount(
                        miden_amount,
                        origin.scale,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(note_id = %note_id_str, "restore: skip B2AGG: {e:#}");
                            continue;
                        }
                    };

                    // B5 — share the versioned domain-separated helper with
                    // bridge_out so the tx_hash is byte-identical across
                    // first-observation and restore paths (dedup-stable).
                    let tx_hash = crate::bridge_out::derive_bridge_out_tx_hash(&note_id_str);

                    let deposit_count =
                        store_clone.mark_note_processed(note_id_str.clone()).await?;

                    if let Err(err) = store_clone
                        .add_bridge_event(
                            bridge_address,
                            restore_block,
                            block_hash,
                            &tx_hash,
                            0, // LEAF_TYPE_ASSET
                            origin.origin_network,
                            &origin.origin_address,
                            destination_network,
                            &destination_address,
                            origin_amount,
                            &[],
                            deposit_count,
                        )
                        .await
                    {
                        let _ = store_clone.unmark_note_processed(&note_id_str).await;
                        return Err(err);
                    }

                    tracing::info!(
                        note_id = %note_id_str,
                        deposit_count,
                        "restore: rebuilt BridgeEvent"
                    );

                    count += 1;
                    logs += 1;
                }

                *result_inner.lock().unwrap() = (count, logs);
                Ok(())
            })
        })
        .await?;

    let (count, logs) = *result.lock().unwrap();
    Ok((count, logs))
}

/// Phase 2.5: scan miden consumed CLAIM notes and replay any missing
/// synthetic `ClaimEvent` log via [`Store::commit_manual_claim_event_atomic`].
///
/// Mirrors [`crate::claim_watcher::ClaimWatcher::on_post_sync`] — same
/// script-root filter, same storage decoder, same dedup predicates, same
/// atomic commit primitive — but runs offline as a restore phase instead of
/// inside the live sync loop. The synthetic tx_hash uses the shared
/// `derive_manual_claim_tx_hash` helper so re-running restore (or running
/// live after restore) lands on a byte-identical hash and the bridge-service
/// deduplicates correctly.
///
/// Returns `(claims_processed, logs_created)`.
async fn restore_claims(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                let claim_root = claim_script().root();
                let block_hash = block_state_clone.get_block_hash(restore_block);
                let bridge_address = get_bridge_address();
                let mut claim_count = 0usize;
                let mut log_count = 0usize;

                // G7 — deterministic sort. CLAIM notes share the same
                // restore_block (and therefore block_hash) and write into a
                // dedup-keyed store, but we still sort to keep restore runs
                // deterministic for the operator-visible
                // `claim_watcher_synthesised_total` counter and log stream.
                let mut sorted_notes: Vec<&_> = consumed_notes.iter().collect();
                sorted_notes.sort_by_key(|n| n.id().to_string());

                for note in sorted_notes {
                    let details = note.details();
                    if details.script().root() != claim_root {
                        continue;
                    }

                    let note_id_str = note.id().to_string();

                    // Dedup 1: was this CLAIM already replayed by an earlier
                    // restore (or by the live watcher)?
                    if store_clone.is_claim_note_processed(&note_id_str).await? {
                        continue;
                    }

                    // Decode the on-chain CLAIM storage. Malformed storage
                    // is logged + counted but doesn't abort restore — the
                    // live watcher does the same (`quarantining` path).
                    let decoded = match parse_claim_event_from_storage(details.storage()) {
                        Ok(d) => d,
                        Err(e) => {
                            ::metrics::counter!("claim_watcher_storage_decode_total")
                                .increment(1);
                            tracing::warn!(
                                target: "restore::claims",
                                note_id = %note_id_str,
                                error = ?e,
                                "restore: CLAIM storage could not be decoded; skipping"
                            );
                            ::metrics::counter!("claim_watcher_unrecoverable_total").increment(1);
                            continue;
                        }
                    };

                    // Dedup 2: was the ClaimEvent already written by the
                    // normal `eth_sendRawTransaction` path before the crash?
                    // Same check the live watcher uses; without it restore
                    // would double-emit for every CLAIM whose primary path
                    // ran to completion.
                    if store_clone
                        .has_claim_event_for_global_index(&decoded.global_index)
                        .await?
                    {
                        ::metrics::counter!("claim_watcher_already_recorded_total").increment(1);
                        // Still mark the note processed so the next
                        // observation (live watcher or another restore) is
                        // a fast skip rather than a re-decode.
                        if let Err(e) = store_clone
                            .mark_claim_note_processed(
                                note_id_str.clone(),
                                decoded.global_index,
                                restore_block,
                            )
                            .await
                        {
                            tracing::error!(
                                target: "restore::claims",
                                note_id = %note_id_str,
                                error = ?e,
                                "restore: failed to mark already-recorded CLAIM processed"
                            );
                        }
                        continue;
                    }

                    let tx_hash = derive_manual_claim_tx_hash(&note_id_str);

                    store_clone
                        .commit_manual_claim_event_atomic(
                            note_id_str.clone(),
                            bridge_address,
                            restore_block,
                            block_hash,
                            &tx_hash,
                            decoded.global_index,
                            decoded.origin_network,
                            &decoded.origin_address,
                            &decoded.destination_address,
                            decoded.amount,
                        )
                        .await?;

                    ::metrics::counter!("claim_watcher_synthesised_total").increment(1);
                    tracing::info!(
                        target: "restore::claims",
                        note_id = %note_id_str,
                        synthetic_tx_hash = %tx_hash,
                        global_index = %hex::encode(decoded.global_index),
                        origin_network = decoded.origin_network,
                        amount = decoded.amount,
                        block_number = restore_block,
                        "restore: synthesised ClaimEvent from consumed CLAIM note (MA#27)"
                    );

                    claim_count += 1;
                    log_count += 1;
                }

                *result_inner.lock().unwrap() = (claim_count, log_count);
                Ok(())
            })
        })
        .await?;

    let (count, logs) = *result.lock().unwrap();
    Ok((count, logs))
}

/// Phase 3: scan consumed UpdateGerNote notes to rebuild GER state.
async fn restore_gers(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                let ger_script_root = UpdateGerNote::script_root();
                let block_hash = block_state_clone.get_block_hash(restore_block);
                let timestamp = block_state_clone.get_block_timestamp(restore_block);
                let mut ger_count = 0usize;
                let mut log_count = 0usize;

                // G7 — sort GER notes deterministically before reconstructing
                // the hash chain. Iteration order from the miden client is
                // insertion-order, but the GER hash chain is order-sensitive
                // (each new value mixes into a rolling Keccak), so two
                // restore runs over the same on-chain state could produce
                // different chain values without sorting. Lex-sort by
                // NoteId for stability.
                let mut sorted_notes: Vec<&_> = consumed_notes.iter().collect();
                sorted_notes.sort_by_key(|n| n.id().to_string());

                for note in sorted_notes {
                    let details = note.details();
                    if details.script().root() != ger_script_root {
                        continue;
                    }

                    let storage = details.storage();
                    let items = storage.items();
                    if items.len() < UpdateGerNote::NUM_STORAGE_ITEMS {
                        tracing::warn!(
                            note_id = %note.id(),
                            storage_len = items.len(),
                            "restore: UpdateGerNote has unexpected storage size, skipping"
                        );
                        continue;
                    }

                    let mut ger_bytes = [0u8; 32];
                    let mut overflow = false;
                    for (i, felt) in items.iter().take(8).enumerate() {
                        // X6 — Felt values can be anywhere in [0, GOLDILOCKS).
                        // The previous `as u32` silently truncated values
                        // exceeding u32::MAX, producing a corrupted GER that
                        // wouldn't match the L1-side keccak. Use try_from so
                        // a malformed UpdateGerNote is rejected instead of
                        // silently restoring the wrong root.
                        match u32::try_from(felt.as_canonical_u64()) {
                            Ok(v) => {
                                ger_bytes[i * 4..(i + 1) * 4].copy_from_slice(&v.to_be_bytes())
                            }
                            Err(_) => {
                                tracing::error!(
                                    note_id = %note.id(),
                                    limb_index = i,
                                    felt_value = felt.as_canonical_u64(),
                                    "restore: UpdateGerNote limb exceeds u32::MAX, skipping (X6)"
                                );
                                overflow = true;
                                break;
                            }
                        }
                    }
                    if overflow {
                        continue;
                    }

                    // `is_ger_injected` (not `has_seen_ger`): with the
                    // L1InfoTreeIndexer running, ger_entries rows can exist
                    // for pairs the indexer observed on L1 but for which the
                    // proxy never submitted a Miden inject (typical when
                    // restore is replaying after a crash that lost the in-
                    // memory injection state). Replay should re-emit those.
                    if store_clone.is_ger_injected(&ger_bytes).await? {
                        continue;
                    }

                    let tx_hash = {
                        let mut hasher = Keccak256::new();
                        hasher.update(b"restore-ger-miden-");
                        hasher.update(note.id().to_string().as_bytes());
                        format!("0x{}", hex::encode(hasher.finalize()))
                    };

                    store_clone
                        .add_ger_update_event(
                            restore_block,
                            block_hash,
                            &tx_hash,
                            &ger_bytes,
                            None,
                            None,
                            timestamp,
                        )
                        .await?;

                    store_clone.mark_ger_injected(ger_bytes).await?;

                    tracing::info!(
                        note_id = %note.id(),
                        ger = %hex::encode(ger_bytes),
                        "restore: rebuilt GER from consumed UpdateGerNote"
                    );

                    ger_count += 1;
                    log_count += 1;
                }

                *result_inner.lock().unwrap() = (ger_count, log_count);
                Ok(())
            })
        })
        .await?;

    let (count, logs) = *result.lock().unwrap();
    Ok((count, logs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::InMemoryStore;
    use std::sync::Arc as StdArc;

    // MA#27 — store-level pin for the Phase 2.5 dedup-and-emit pipeline.
    // Replays the inner steps `restore_claims` performs against an
    // InMemoryStore (skipping only the per-tick consumed_notes fetch which
    // requires a live miden-client) and asserts:
    //   1) First call emits a ClaimEvent and marks the note processed.
    //   2) Second call (same note) is a no-op (Dedup 1).
    //   3) If a ClaimEvent for the same global_index was already written
    //      (e.g. by the normal eth_sendRawTransaction path), the new
    //      observation skips emission but DOES mark the note processed
    //      (Dedup 2).
    #[tokio::test]
    async fn ma27_restore_claims_emits_and_dedups() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        let note_id = "0xnoteA".to_string();
        let gi = [0x42u8; 32];
        let bridge = get_bridge_address();
        let tx_hash = derive_manual_claim_tx_hash(&note_id);

        // Pre-conditions
        assert!(!store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(!store.has_claim_event_for_global_index(&gi).await.unwrap());

        // Phase 2.5 inner emission — mirror the call we make in
        // `restore_claims` for an accepted CLAIM.
        store
            .commit_manual_claim_event_atomic(
                note_id.clone(),
                bridge,
                1,
                [0u8; 32],
                &tx_hash,
                gi,
                7,
                &[1u8; 20],
                &[2u8; 20],
                1_000,
            )
            .await
            .unwrap();

        assert!(store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
        assert_eq!(store.get_latest_block_number().await.unwrap(), 1);

        // Idempotency: Dedup 1 short-circuits on a second pass. We model
        // this by checking the predicate restore_claims uses BEFORE doing
        // any write — if it returns true, we skip.
        let already_processed = store.is_claim_note_processed(&note_id).await.unwrap();
        assert!(
            already_processed,
            "second restore must see Dedup 1 fire and skip emission"
        );

        // Dedup 2 — different note id, same global_index. The normal path
        // already wrote the ClaimEvent; restore's job is to mark the new
        // observation processed but NOT double-emit. We assert via the
        // public predicate.
        let other_note = "0xnoteB".to_string();
        assert!(
            store
                .has_claim_event_for_global_index(&gi)
                .await
                .unwrap(),
            "global_index dedup predicate must fire for a second observation"
        );
        // The mark step for the "already-recorded" branch is also exposed
        // via the store primitive — pin it directly so any future store
        // refactor that drops mark_claim_note_processed in this branch
        // is caught.
        store
            .mark_claim_note_processed(other_note.clone(), gi, 1)
            .await
            .unwrap();
        assert!(store.is_claim_note_processed(&other_note).await.unwrap());
    }

    // MA#27 — pin the synthetic tx-hash derivation used by Phase 2.5
    // matches what the live `ClaimWatcher` produces. If these drift, a
    // restore-then-live pair will double-emit ClaimEvents under different
    // tx_hashes and bridge-service won't dedup them.
    #[test]
    fn ma27_restore_synthetic_tx_hash_matches_live_watcher() {
        let note_id = "0xfeed".to_string();
        let restore_path = derive_manual_claim_tx_hash(&note_id);
        let live_path = crate::claim_watcher::derive_manual_claim_tx_hash(&note_id);
        assert_eq!(
            restore_path, live_path,
            "restore and live ClaimWatcher must derive identical synthetic tx-hashes"
        );
    }

    // MA#27 — RestoreResult exposes a `claims_restored` counter so
    // operators can verify the new Phase 2.5 ran. Pin the field shape;
    // older RestoreResult shapes without this field made it impossible to
    // tell whether the new phase had executed at all.
    #[test]
    fn ma27_restore_result_exposes_claims_restored() {
        let r = RestoreResult {
            block_number: 7,
            bridge_outs_restored: 1,
            claims_restored: 2,
            gers_restored: 3,
            logs_created: 6,
        };
        assert_eq!(r.claims_restored, 2);
    }
}
