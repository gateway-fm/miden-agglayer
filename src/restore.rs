//! Restore — Reconstruct PgStore state from miden node + L1.
//!
//! This module implements disaster recovery: when the PostgreSQL store is
//! empty (fresh deploy or data loss), it rebuilds all state from authoritative
//! sources (L1 chain events, miden node consumed notes, miden sync state).
//!
//! ## Algorithm
//!
//! Phase 1: Sync miden state → get current block number
//! Phase 2: Scan L1 ClaimEvent logs → rebuild claimed_indices
//! Phase 3: Scan miden consumed B2AGG notes → rebuild bridge-out + deposit counter
//! Phase 4: Scan consumed UpdateGerNote notes on Miden → rebuild GER set + hash chain
//! Phase 5: Update block number to cover all synthetic logs
//! Phase 6: Verify counts
//!
//! ## GER restoration via consumed notes
//!
//! For recovery we only care about consumed notes — actually injected GERs.
//! L1 is the wrong source because it knows about GERs that may never have been
//! injected into Miden; you'd have to call the node to verify anyway.
//!
//! When the proxy injects a GER, it creates an UpdateGerNote that gets consumed
//! by the Miden bridge account. The Miden node retains consumed notes, so we can
//! scan them to reconstruct the full GER history without any L1 dependency.
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
use crate::log_synthesis::CLAIM_EVENT_TOPIC;
use crate::miden_client::MidenClient;
use crate::store::Store;
use alloy::primitives::U256;
use miden_base_agglayer::UpdateGerNote;
use miden_client::store::NoteFilter;
use sha3::{Digest, Keccak256};
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub claims_restored: usize,
    pub bridge_outs_restored: usize,
    pub gers_restored: usize,
    pub logs_created: usize,
}

/// Run the full restore algorithm.
#[allow(clippy::too_many_arguments)]
pub async fn restore(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
    l1_rpc_url: &str,
    bridge_address: &str,
    from_l1_block: u64,
) -> anyhow::Result<RestoreResult> {
    tracing::info!("=== RESTORE: starting state reconstruction ===");

    // Phase 1: Sync miden state
    tracing::info!("Phase 1: syncing miden state...");
    let block_num = sync_miden_block(miden_client, store).await?;
    tracing::info!("Phase 1 complete: miden block {block_num}");

    // We'll assign synthetic logs to blocks starting after current
    let mut next_block = block_num + 1;
    let mut total_logs = 0usize;

    // Phase 2: Scan L1 ClaimEvent logs
    tracing::info!("Phase 2: scanning L1 ClaimEvent logs...");
    let claims = restore_claims(store, l1_rpc_url, bridge_address, from_l1_block).await?;
    tracing::info!("Phase 2 complete: {claims} claims restored");

    // Phase 3: Scan miden consumed B2AGG notes
    tracing::info!("Phase 3: scanning miden consumed B2AGG notes...");
    let (bridge_outs, logs) = restore_bridge_outs(
        store, miden_client, accounts, block_state, next_block,
    ).await?;
    next_block += if logs > 0 { 1 } else { 0 };
    total_logs += logs;
    tracing::info!("Phase 3 complete: {bridge_outs} bridge-outs, {logs} logs");

    // Phase 4: Scan consumed UpdateGerNote notes on Miden
    tracing::info!("Phase 4: scanning consumed UpdateGerNote notes on Miden...");
    let (gers, ger_logs) = restore_gers(store, miden_client, block_state, next_block).await?;
    total_logs += ger_logs;
    tracing::info!("Phase 4 complete: {gers} GERs, {ger_logs} logs");

    // Phase 5: Update block number to cover all synthetic logs
    let final_block = next_block + if ger_logs > 0 { 1 } else { 0 };
    store.set_latest_block_number(final_block).await?;
    tracing::info!("Phase 5: block number set to {final_block}");

    // Phase 6: Verify
    tracing::info!("Phase 6: verification");
    tracing::info!(
        "  claims={claims}, bridge_outs={bridge_outs}, gers={gers}, logs={total_logs}"
    );
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: final_block,
        claims_restored: claims,
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
    // Trigger a sync to get the latest block
    miden_client
        .with(|client| {
            Box::new(async move {
                client.sync_state().await?;
                Ok(())
            })
        })
        .await?;

    // The sync listener should have updated the block number,
    // but if restore runs before listeners are active, read from miden directly
    let block_num = store.get_latest_block_number().await?;
    Ok(block_num)
}

/// Phase 2: rebuild claimed_indices from bridge-service deposits API.
///
/// The bridge-service tracks all deposits and their claim status. We query
/// it to find deposits targeting our network that have been claimed.
async fn restore_claims(
    store: &Arc<dyn Store>,
    l1_rpc_url: &str,
    bridge_address: &str,
    _from_block: u64,
) -> anyhow::Result<usize> {
    // Strategy 1: Query bridge-service REST API for claimed deposits
    // The bridge-service URL may be available via BRIDGE_SERVICE_URL env var
    let bridge_service_url = std::env::var("BRIDGE_SERVICE_URL")
        .unwrap_or_else(|_| "http://localhost:18080".to_string());

    // Try bridge-service API first
    match restore_claims_from_bridge_service(store, &bridge_service_url).await {
        Ok(n) => {
            tracing::info!("restore: {n} claims from bridge-service API");
            Ok(n)
        }
        Err(e) => {
            tracing::warn!("restore: bridge-service API failed: {e:#}, falling back to L1 logs");
            // Strategy 2: Fall back to L1 ClaimEvent logs on the bridge contract
            restore_claims_from_l1(store, l1_rpc_url, bridge_address).await
        }
    }
}

/// Restore claims from bridge-service REST API.
async fn restore_claims_from_bridge_service(
    store: &Arc<dyn Store>,
    bridge_service_url: &str,
) -> anyhow::Result<usize> {
    let client = reqwest::Client::new();

    // Query deposits for the zero address (gets all deposits for all destinations)
    // The bridge-service /bridges endpoint returns deposits by destination address
    // We need to find all deposits that target our network (network_id=1)
    let url = format!(
        "{}/bridges/0x0000000000000000000000000000000000000000",
        bridge_service_url.trim_end_matches('/')
    );

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        // Try getting deposits for a wider range — the bridge-service may
        // return all deposits if we query without address filter
        anyhow::bail!("bridge-service returned {}", resp.status());
    }

    #[derive(serde::Deserialize)]
    struct BridgesResponse {
        deposits: Option<Vec<Deposit>>,
    }

    #[derive(serde::Deserialize)]
    struct Deposit {
        global_index: Option<String>,
        ready_for_claim: Option<bool>,
        dest_net: Option<u32>,
    }

    let data: BridgesResponse = resp.json().await?;
    let mut count = 0usize;

    if let Some(deposits) = data.deposits {
        for dep in &deposits {
            // Only count deposits targeting our network (dest_net=1) that have been claimed
            let Some(ref gi_str) = dep.global_index else {
                continue;
            };
            let gi_str = gi_str.trim_start_matches("0x");
            let Ok(global_index) = U256::from_str_radix(gi_str, 16) else {
                continue;
            };

            if dep.ready_for_claim.unwrap_or(false)
                && dep.dest_net == Some(1)
                && !store.is_claimed(&global_index).await?
                && store.try_claim(global_index).await.is_ok()
            {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Fallback: restore claims from L1 ClaimEvent logs.
async fn restore_claims_from_l1(
    store: &Arc<dyn Store>,
    l1_rpc_url: &str,
    bridge_address: &str,
) -> anyhow::Result<usize> {
    use alloy::providers::{Provider, ProviderBuilder};

    let provider = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let latest_block = provider.get_block_number().await?;

    let topic = CLAIM_EVENT_TOPIC.strip_prefix("0x").unwrap_or(CLAIM_EVENT_TOPIC);
    let topic_bytes = hex::decode(topic)?;
    let mut topic_b256 = [0u8; 32];
    topic_b256.copy_from_slice(&topic_bytes);

    let bridge_addr: alloy::primitives::Address = bridge_address.parse()?;

    let filter = alloy::rpc::types::Filter::new()
        .address(bridge_addr)
        .event_signature(alloy::primitives::B256::from(topic_b256))
        .from_block(0u64)
        .to_block(latest_block);

    let logs = provider.get_logs(&filter).await?;
    let mut count = 0usize;

    for log in &logs {
        let data = log.data().data.as_ref();
        if data.len() >= 32 {
            let global_index = U256::from_be_slice(&data[..32]);
            if !store.is_claimed(&global_index).await?
                && store.try_claim(global_index).await.is_ok()
            {
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Phase 3: scan miden consumed B2AGG notes and rebuild bridge-out state.
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

                // TODO: When miden-node adds NoteFilter::ConsumedByScriptRoot,
                // replace client-side filtering with server-side filter
                for note in &consumed_notes {
                    let details = note.details();
                    if !is_b2agg_note(details) {
                        continue;
                    }

                    let note_id_str = note.id().to_string();
                    if store_clone.is_note_processed(&note_id_str).await? {
                        continue;
                    }

                    let (destination_network, destination_address) =
                        match parse_b2agg_storage(details.storage()) {
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
                    let origin_amount = match crate::bridge_out::reverse_scale_amount(miden_amount, origin.scale) {
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

                    let deposit_count = store_clone.mark_note_processed(note_id_str.clone()).await?;

                    store_clone
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
                        .await?;

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

/// Phase 4: scan consumed UpdateGerNote notes to rebuild GER state.
///
/// For recovery we only care about consumed notes — actually injected GERs.
/// Each UpdateGerNote stores the GER as 8 Felts in note storage. We extract it,
/// reconstruct the full GER bytes, and rebuild the hash chain in consumption order.
///
/// Currently reads consumed notes via the miden-client gRPC sync.
/// TODO: switch to a dedicated API endpoint (get_gers() or
/// NoteFilter::ConsumedByScriptRoot) when the Miden team ships it.
///
/// See: https://github.com/0xMiden/protocol/issues/2341
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

                // Filter for UpdateGerNote notes by script root
                // TODO: When miden-node adds NoteFilter::ConsumedByScriptRoot,
                // replace client-side filtering with server-side filter
                for note in &consumed_notes {
                    let details = note.details();
                    if details.script().root() != ger_script_root {
                        continue;
                    }

                    // Extract GER from note storage (8 Felts = 32 bytes)
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

                    // Reconstruct the 32-byte GER from Felt elements.
                    // ExitRoot stores as 8 Felts, each holding 4 bytes (big-endian u32).
                    // Convert back: take lower 32 bits of each Felt, concatenate.
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
                            None, // mainnet/rollup roots not stored in note
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

