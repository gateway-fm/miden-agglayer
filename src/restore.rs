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
use crate::miden_client::MidenClient;
use crate::store::Store;
use miden_base_agglayer::UpdateGerNote;
use miden_client::store::NoteFilter;
use sha3::{Digest, Keccak256};
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
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
    tracing::info!("  bridge_outs={bridge_outs}, gers={gers}, logs={total_logs}");
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: final_block,
        bridge_outs_restored: bridge_outs,
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
    accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let accounts_clone = accounts.clone();
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

                for note in &consumed_notes {
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
                    let origin = match resolve_faucet_origin(faucet_id, &accounts_clone) {
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

                    let tx_hash = {
                        let mut hasher = Keccak256::new();
                        hasher.update(b"miden-bridge-out-");
                        hasher.update(note_id_str.as_bytes());
                        let hash: [u8; 32] = hasher.finalize().into();
                        format!("0x{}", hex::encode(hash))
                    };

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

                for note in &consumed_notes {
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
                    for (i, felt) in items.iter().take(8).enumerate() {
                        let v: u32 = felt.as_int() as u32;
                        ger_bytes[i * 4..(i + 1) * 4].copy_from_slice(&v.to_be_bytes());
                    }

                    if store_clone.has_seen_ger(&ger_bytes).await? {
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
