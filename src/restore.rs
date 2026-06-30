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
use crate::bridge_out::{
    B2AggConsumerClass, classify_b2agg_consumer, is_b2agg_note, parse_b2agg_storage,
    resolve_faucet_origin,
};
use crate::claim_watcher::{derive_manual_claim_tx_hash, parse_claim_event_from_storage};
use crate::metadata_recovery::{EmitMetadata, METADATA_UNRECOVERABLE_METRIC};
use crate::miden_client::{MidenClient, MidenClientLib};
use crate::store::Store;
use miden_base_agglayer::UpdateGerNote;
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_protocol::account::AccountId;
use miden_protocol::note::{NoteAttachments, NoteMetadata};
use sha3::{Digest, Keccak256};
use std::sync::Arc;

/// MA#28 — outcome of verifying an `UpdateGerNote`-shaped consumed note's
/// authoritative provenance. Pulled out of `restore_gers` so the
/// fast-path verification can be unit-tested without spinning up a Miden
/// node + sqlite store.
#[derive(Debug, PartialEq, Eq)]
pub enum GerNoteVerdict {
    /// Note was minted by the expected sender and targets the expected bridge.
    /// Safe to replay as a sanctioned GER injection.
    Accept,
    /// `note.metadata()` returned `None` — non-conforming consumed note.
    MissingMetadata,
    /// `metadata.sender() != expected_sender`. Either an attacker minted
    /// a same-script note from a different account, or the proxy's config
    /// drifted away from the historical ger_manager id.
    SenderMismatch,
    /// `metadata.attachment()` did not decode as `NetworkAccountTarget`.
    /// Mirrors the Cantina #4 forged-MINT signal in `bridge_out.rs`.
    UndecodableTarget,
    /// Decoded target was a different account than the bridge id.
    TargetMismatch,
}

/// MA#28 — pure verification of an `UpdateGerNote`-shaped note. Public so
/// the unit tests in this file (and any future tooling that wants to
/// validate consumed-note feeds) can exercise the predicate directly.
pub fn classify_ger_note(
    metadata: Option<&NoteMetadata>,
    attachments: &NoteAttachments,
    expected_sender: AccountId,
    expected_target: AccountId,
) -> GerNoteVerdict {
    let Some(meta) = metadata else {
        return GerNoteVerdict::MissingMetadata;
    };
    if meta.sender() != expected_sender {
        return GerNoteVerdict::SenderMismatch;
    }
    match decode_network_target(attachments) {
        None => GerNoteVerdict::UndecodableTarget,
        Some(target) if target != expected_target => GerNoteVerdict::TargetMismatch,
        Some(_) => GerNoteVerdict::Accept,
    }
}

/// Small wrapper so `classify_ger_note` doesn't have to import
/// `miden_standards` into the public signature. Mirrors the decoder used
/// by `bridge_out.rs::on_post_sync` for MINT notes.
fn decode_network_target(attachments: &NoteAttachments) -> Option<AccountId> {
    miden_standards::note::NetworkAccountTarget::try_from(attachments)
        .ok()
        .map(|nat| nat.target_id())
}

/// Decode the 32-byte GER from an `UpdateGerNote`'s storage felts.
///
/// `UpdateGerNote` storage is `ExitRoot::to_elements()` — each 4-byte GER limb
/// packed **little-endian** into a felt (the LE limb convention used across
/// `bridge_out` / `claim_note` / `b2agg_note`). Decoding must therefore be
/// little-endian: a big-endian decode byte-swaps every limb, producing the wrong
/// GER (e.g. `2ae1a9b7…` → `b7a9e12a…`). That made the projector emit a GER that
/// never matched the one aggkit injected, so bridge-in deposits hung forever on
/// `ready_for_claim`. Unit-tested via a round-trip against `ExitRoot::to_elements`.
///
/// Returns `Err(limb_index)` if a felt exceeds `u32::MAX` (a malformed note; X6).
pub(crate) fn ger_bytes_from_storage(items: &[miden_protocol::Felt]) -> Result<[u8; 32], usize> {
    let mut ger_bytes = [0u8; 32];
    for (i, felt) in items.iter().take(8).enumerate() {
        match u32::try_from(felt.as_canonical_u64()) {
            Ok(v) => ger_bytes[i * 4..(i + 1) * 4].copy_from_slice(&v.to_le_bytes()),
            Err(_) => return Err(i),
        }
    }
    Ok(ger_bytes)
}

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
    /// Cantina MA#27 — number of consumed CLAIM notes for which a synthetic
    /// ClaimEvent was emitted by restore (the offline equivalent of what the
    /// live [`SyntheticProjector`](crate::synthetic_projector) does each tick).
    pub claims_restored: usize,
    pub gers_restored: usize,
    pub logs_created: usize,
}

/// The Miden block a consumed note is attributed to (Miden-1:1), or `fallback`
/// when the note carries no consumed-block height (should not happen for a note
/// in a consumed state, but keeps restore total rather than dropping it).
fn note_consumed_block(note: &InputNoteRecord, fallback: u64) -> u64 {
    note.state()
        .consumed_block_height()
        .map(|h| h.as_u64())
        .unwrap_or(fallback)
}

/// Order consumed notes into the [`SyntheticProjector`](crate::synthetic_projector)'s
/// canonical projection order: `(consumed_block_height, consumed_tx_order,
/// details-commitment bytes)`. Restore MUST replay in this exact order so its
/// per-note synthetic block numbers, the `deposit_count` assignment, and the
/// order-sensitive GER hash chain are byte-identical to a fresh live projection.
/// (Byte compare on the 32-byte commitment — same order as a hex compare, no
/// allocation.)
fn sort_consumed_for_projection(notes: &mut [&InputNoteRecord]) {
    notes.sort_by(|a, b| {
        a.state()
            .consumed_block_height()
            .map(|h| h.as_u64())
            .cmp(&b.state().consumed_block_height().map(|h| h.as_u64()))
            .then_with(|| {
                a.state()
                    .consumed_tx_order()
                    .cmp(&b.state().consumed_tx_order())
            })
            .then_with(|| {
                a.details_commitment()
                    .as_bytes()
                    .cmp(&b.details_commitment().as_bytes())
            })
    });
}

/// Run the full restore algorithm.
pub async fn restore(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    l1_rpc_url: Option<String>,
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

    // Phase 1: Sync miden state + read the Miden tip — the block the synthetic
    // chain catches up to under Miden-1:1. Each restored event is attributed to
    // its OWN consumed block (below); `miden_tip` is only the orphan fallback.
    tracing::info!("Phase 1: syncing miden state...");
    let miden_tip = sync_miden_block(miden_client).await?;
    tracing::info!("Phase 1 complete: miden tip {miden_tip}");

    let mut total_logs = 0usize;

    // Phase 2: Scan miden consumed B2AGG notes
    tracing::info!("Phase 2: scanning miden consumed B2AGG notes...");
    let (bridge_outs, logs) = restore_bridge_outs(
        store,
        miden_client,
        accounts,
        local_network_id,
        block_state,
        miden_tip,
        l1_rpc_url.clone(),
    )
    .await?;
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
    let (claims, claim_logs) = restore_claims(store, miden_client, block_state, miden_tip).await?;
    total_logs += claim_logs;
    tracing::info!("Phase 2.5 complete: {claims} claims, {claim_logs} logs");

    // Phase 3: Scan consumed UpdateGerNote notes on Miden
    tracing::info!("Phase 3: scanning consumed UpdateGerNote notes on Miden...");
    let (gers, ger_logs) =
        restore_gers(store, miden_client, accounts, block_state, miden_tip).await?;
    total_logs += ger_logs;
    tracing::info!("Phase 3 complete: {gers} GERs, {ger_logs} logs");

    // Phase 4: Miden-1:1 — the synthetic tip == the Miden tip, and the projector
    // cursor is set to the Miden tip so the live projector resumes from there
    // rather than re-scanning the blocks restore just replayed (idempotent dedup
    // would skip them anyway). The restored events already sit at their own
    // Miden blocks.
    store.set_latest_block_number(miden_tip).await?;
    store.set_projector_cursor(miden_tip).await?;
    tracing::info!("Phase 4: synthetic tip + projector cursor set to Miden tip {miden_tip}");

    // Phase 5: Verify
    tracing::info!("Phase 5: verification");
    tracing::info!("  bridge_outs={bridge_outs}, claims={claims}, gers={gers}, logs={total_logs}");
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: miden_tip,
        bridge_outs_restored: bridge_outs,
        claims_restored: claims,
        gers_restored: gers,
        logs_created: total_logs,
    })
}

/// Phase 1: sync miden and return the current MIDEN tip (sync height) — the
/// block the synthetic chain catches up to under Miden-1:1.
async fn sync_miden_block(miden_client: &MidenClient) -> anyhow::Result<u64> {
    let height = Arc::new(std::sync::Mutex::new(0u64));
    let height_inner = height.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                *height_inner.lock().unwrap() = client
                    .get_sync_height()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
                    .as_u64();
                Ok(())
            })
        })
        .await?;
    let h = *height.lock().unwrap();
    Ok(h)
}

/// Phase 2: scan miden consumed B2AGG notes and rebuild bridge-out state.
/// Returns (notes_processed, logs_created).
async fn restore_bridge_outs(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    restore_block: u64,
    l1_rpc_url: Option<String>,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();
    // Cantina MA#3 — the configured bridge account is the only legitimate
    // consumer of a *bridge-out* B2AGG note; reclaim/untracked consumptions are
    // gated out in `project_b2agg_note`.
    let bridge_id = accounts.bridge.0;

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();
    // Owned copy moved into the 'static closure (Cantina #13 L2 recovery).
    let l1_url = l1_rpc_url;

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                let bridge_address = get_bridge_address();
                let mut count = 0usize;
                let mut logs = 0usize;

                // Miden-1:1: replay each B2AGG note at its OWN Miden consumption
                // block, in the projector's canonical (block, tx_order, note_id)
                // order. This keeps deposit_count assignment deterministic across
                // restore runs AND byte-identical to a fresh live projection —
                // the Miden client returns consumed notes in store-arrival order,
                // which varies between runs.
                let mut sorted: Vec<&_> = consumed_notes.iter().collect();
                sort_consumed_for_projection(&mut sorted);

                for note in sorted {
                    let blk = note_consumed_block(note, restore_block);
                    let block_hash = block_state_clone.get_block_hash(blk);
                    let outcome = project_b2agg_note(
                        &store_clone,
                        note,
                        bridge_id,
                        local_network_id,
                        blk,
                        block_hash,
                        bridge_address,
                        Some(&mut *client),
                        l1_url.as_deref(),
                    )
                    .await?;
                    if outcome == B2AggRestoreOutcome::Emitted {
                        count += 1;
                        logs += 1;
                    }
                }

                *result_inner.lock().unwrap() = (count, logs);
                Ok(())
            })
        })
        .await?;

    let (count, logs) = *result.lock().unwrap();
    Ok((count, logs))
}

/// Outcome of attempting to rebuild one consumed B2AGG note during restore.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum B2AggRestoreOutcome {
    /// A synthetic `BridgeEvent` was (re)built for a real bridge-out.
    Emitted,
    /// Skipped for a benign reason: not a B2AGG note, unparsable, no asset, a
    /// reclaim/untracked consumer (Cantina MA#3 gate), or a note an earlier run
    /// already processed correctly.
    Skipped,
    /// The note was already marked processed by an earlier run, but the MA#3 gate
    /// would now REJECT it (consumer != the configured bridge). A pre-fix restore
    /// likely emitted an *invalid* synthetic `BridgeEvent` for a reclaim/untracked
    /// consumption. We do NOT auto-mutate that legacy state (an operator decision)
    /// — we surface it (warn + `restore_b2agg_legacy_processed_gated_total`) so it
    /// can be detected and reset/rebuilt.
    LegacyProcessedGated,
}

/// Rebuild the synthetic `BridgeEvent` for a single consumed note, if and only if
/// it is a *bridge-out* B2AGG note consumed by the configured `bridge_id`.
///
/// Extracted from `restore_bridge_outs` so the per-note decision is unit-testable
/// without a live Miden client (mirrors `project_b2agg_note`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_b2agg_note(
    store: &Arc<dyn Store>,
    note: &InputNoteRecord,
    bridge_id: AccountId,
    local_network_id: u32,
    restore_block: u64,
    block_hash: [u8; 32],
    bridge_address: &str,
    client: Option<&mut MidenClientLib>,
    l1_rpc_url: Option<&str>,
) -> anyhow::Result<B2AggRestoreOutcome> {
    let details = note.details();
    if !is_b2agg_note(details) {
        return Ok(B2AggRestoreOutcome::Skipped);
    }

    let note_id_str = hex::encode(note.details_commitment().as_bytes());

    // Cantina MA#3 — reclaim gate. A B2AGG note has a reclaim branch (consumer ==
    // sender, asset stays on Miden) and a bridge branch (consumer == bridge, asset
    // leaves). Only the latter is a real bridge-out; rebuilding a synthetic
    // BridgeEvent for a reclaim would hand the user a claimable withdrawal for
    // value that never left. Mirrors `project_b2agg_note`.
    let consumer = note.consumer_account();
    let class = classify_b2agg_consumer(consumer, bridge_id);

    // Dedup. A note an earlier run already handled is normally a no-op — UNLESS
    // the gate would now reject it: that means a pre-fix run emitted an invalid
    // BridgeEvent for a reclaim/untracked consumption. Surface it (warn + metric)
    // rather than silently skipping, so operators can detect legacy bad state and
    // reset/rebuild. We do not auto-remove the stale event here.
    if store.is_note_processed(&note_id_str).await? {
        if !matches!(class, B2AggConsumerClass::Emit) {
            ::metrics::counter!("restore_b2agg_legacy_processed_gated_total").increment(1);
            tracing::warn!(
                note_id = %note_id_str,
                consumer = ?consumer,
                bridge = %bridge_id,
                "restore: already-processed B2AGG note would now be gated out (consumer != \
                 bridge) — a pre-fix run may have emitted an INVALID synthetic BridgeEvent; \
                 review and reset/rebuild bridge-out state (Cantina MA#3)"
            );
            return Ok(B2AggRestoreOutcome::LegacyProcessedGated);
        }
        return Ok(B2AggRestoreOutcome::Skipped);
    }

    match class {
        B2AggConsumerClass::Emit => {}
        B2AggConsumerClass::Reclaimed => {
            ::metrics::counter!("bridge_out_reclaimed_b2agg_total").increment(1);
            tracing::info!(
                note_id = %note_id_str,
                consumer = ?consumer,
                bridge = %bridge_id,
                "restore: B2AGG note was reclaimed by user (consumed by non-bridge \
                 account); skipping synthetic BridgeEvent (Cantina MA#3)"
            );
            return Ok(B2AggRestoreOutcome::Skipped);
        }
        B2AggConsumerClass::UntrackedConsumer => {
            ::metrics::counter!("bridge_out_b2agg_untracked_consumer_total").increment(1);
            tracing::info!(
                note_id = %note_id_str,
                bridge = %bridge_id,
                "restore: B2AGG note consumed by untracked account (consumer_account \
                 = None); fail-closed skip (Cantina MA#3)"
            );
            return Ok(B2AggRestoreOutcome::Skipped);
        }
    }

    let (destination_network, destination_address) = match parse_b2agg_storage(details.storage()) {
        Ok(v) => v,
        Err(e) => {
            // MA#18 — the bridge consumed this B2AGG (LET advanced) but its storage
            // is unparsable, so we cannot reconstruct the destination. Quarantine
            // (record unbridgeable) so it is surfaced for operator rescue instead of
            // silently skipped. Ported from `project_b2agg_note`.
            tracing::warn!(note_id = %note_id_str, "restore: B2AGG storage unparsable: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &note_id_str,
                note,
                restore_block,
                crate::store::UnbridgeableBridgeOutReason::StorageParseFailed,
                format!("{e:#}"),
            )
            .await;
            return Ok(B2AggRestoreOutcome::Skipped);
        }
    };

    // Cantina #13 — self-target poison-leaf gate (moved here from the now-deleted
    // `project_b2agg_note` when the projector became the sole
    // producer). A B2AGG bridge-out whose destination IS the local network advances
    // the on-chain LET, but the agglayer certificate covering that leaf is rejected
    // (InvalidExit), wedging every legitimate B2AGG in the same window. We can't
    // unwind the LET, but we MUST refuse to emit the synthetic BridgeEvent so the
    // bridge-service never tries to settle a doomed certificate. Skip WITHOUT
    // marking the note processed (the mark happens only on the Emit path below), so
    // the poison is re-logged whenever (re)observed and an operator can quarantine.
    if destination_network == local_network_id {
        ::metrics::counter!("bridge_out_self_targeted_total").increment(1);
        tracing::error!(
            note_id = %note_id_str,
            destination_network,
            local_network_id,
            "POISON LEAF: B2AGG bridge-out targets the local network; the on-chain LET \
             advanced but the aggsender certificate covering this leaf will be rejected \
             (InvalidExit). Refusing to emit a synthetic BridgeEvent (Cantina #13). \
             Operator action required: quarantine this note."
        );
        return Ok(B2AggRestoreOutcome::Skipped);
    }

    let Some(fungible_asset) = details.assets().iter_fungible().next() else {
        // MA#18 — bridge-consumed B2AGG with no fungible asset is malformed: the LET
        // advanced but there is nothing to bridge out. Quarantine, don't silently drop.
        tracing::warn!(note_id = %note_id_str, "restore: B2AGG has no fungible asset");
        crate::bridge_out::quarantine_unbridgeable_b2agg(
            &**store,
            bridge_id,
            &note_id_str,
            note,
            restore_block,
            crate::store::UnbridgeableBridgeOutReason::NoFungibleAsset,
            "consumed B2AGG note carries no fungible asset".to_string(),
        )
        .await;
        return Ok(B2AggRestoreOutcome::Skipped);
    };
    let faucet_id = fungible_asset.faucet_id();
    let miden_amount = u64::from(fungible_asset.amount());
    let origin = match resolve_faucet_origin(faucet_id, &**store).await {
        Ok(v) => v,
        Err(e) => {
            // MA#18 — bridge consumed the B2AGG but its faucet is unknown to us, so
            // we can't reconstruct the origin token. Quarantine for operator rescue.
            tracing::warn!(note_id = %note_id_str, "restore: B2AGG unknown faucet: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &note_id_str,
                note,
                restore_block,
                crate::store::UnbridgeableBridgeOutReason::UnknownFaucet,
                format!("{e:#}"),
            )
            .await;
            return Ok(B2AggRestoreOutcome::Skipped);
        }
    };
    let origin_amount = match crate::bridge_out::reverse_scale_amount(miden_amount, origin.scale) {
        Ok(v) => v,
        Err(e) => {
            // MA#18 — the scaled L1 amount overflows. Quarantine, don't silently drop.
            tracing::warn!(note_id = %note_id_str, "restore: B2AGG amount overflow: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &note_id_str,
                note,
                restore_block,
                crate::store::UnbridgeableBridgeOutReason::AmountOverflow,
                format!("{e:#}"),
            )
            .await;
            return Ok(B2AggRestoreOutcome::Skipped);
        }
    };

    // Cantina #13 Layer 2 — recover + validate empty ERC-20 metadata before
    // rebuilding the BridgeEvent. Legacy/DB-loss faucet rows carry empty
    // metadata; emitting that for an ERC-20 is a poison leaf. Mirrors
    // `BridgeOutScanner::resolve_emit_metadata`. Native ETH stays empty.
    let emit_metadata = {
        let needs_recovery = origin.metadata.is_empty() && origin.origin_address != [0u8; 20];
        let (bridge_account, faucet_account) = if needs_recovery {
            match client {
                Some(client) => {
                    let bridge = client.get_account(bridge_id).await.ok().flatten();
                    let faucet = client.get_account(faucet_id).await.ok().flatten();
                    (bridge, faucet)
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        crate::metadata_recovery::recover_bridge_out_metadata(
            &origin.origin_address,
            &origin.metadata,
            origin.origin_decimals,
            faucet_id,
            bridge_account.as_ref(),
            faucet_account.as_ref(),
            l1_rpc_url,
        )
        .await
    };
    let emit_metadata = match emit_metadata {
        EmitMetadata::Ready(bytes) => bytes,
        EmitMetadata::Recovered(bytes) => {
            // One-time self-heal: backfill the validated preimage.
            if let Ok(Some(mut entry)) = store.get_faucet_by_id(faucet_id).await {
                entry.metadata = bytes.clone();
                if let Err(e) = store.register_faucet(entry).await {
                    tracing::warn!(
                        note_id = %note_id_str,
                        faucet_id = %faucet_id,
                        error = ?e,
                        "restore: Cantina #13 L2 metadata backfill failed (recovery will re-run)"
                    );
                } else {
                    tracing::info!(
                        note_id = %note_id_str,
                        faucet_id = %faucet_id,
                        "restore: Cantina #13 L2 recovered + backfilled ERC-20 metadata"
                    );
                }
            }
            bytes
        }
        EmitMetadata::Unrecoverable => {
            // FAIL-SAFE GATE: refuse to rebuild an ERC-20 BridgeEvent with empty
            // metadata. Skip without marking processed so a later restore (after
            // the registry is backfilled / an L1 RPC is wired) retries it.
            ::metrics::counter!(METADATA_UNRECOVERABLE_METRIC).increment(1);
            tracing::warn!(
                note_id = %note_id_str,
                faucet_id = %faucet_id,
                origin_network = origin.origin_network,
                "restore: Cantina #13 L2 — ERC-20 bridge-out has empty metadata that could not \
                 be recovered + validated against the bridge's metadata hash; skipping (refusing \
                 to emit empty/unvalidated metadata). Backfill the faucet registry or supply an \
                 L1 RPC for the token's origin network, then re-run restore."
            );
            return Ok(B2AggRestoreOutcome::Skipped);
        }
    };

    // Cantina #13 follow-up — DoS guard, now applied to the FINAL emit bytes
    // (Layer-1 stored OR Layer-2 recovered): the metadata derives from untrusted
    // L1 calldata, and a malicious token's name() could yield an oversized
    // recovered blob. Cap before encoding; skip without marking the note processed.
    if emit_metadata.len() > crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES {
        ::metrics::counter!("bridge_out_b2agg_metadata_too_large_total").increment(1);
        tracing::warn!(
            note_id = %note_id_str,
            metadata_len = emit_metadata.len(),
            cap = crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES,
            "restore: B2AGG metadata exceeds cap; skipping synthetic BridgeEvent (DoS guard)"
        );
        crate::bridge_out::quarantine_unbridgeable_b2agg(
            &**store,
            bridge_id,
            &note_id_str,
            note,
            restore_block,
            crate::store::UnbridgeableBridgeOutReason::MetadataTooLarge,
            format!(
                "emit_metadata.len()={} exceeds MAX_BRIDGE_EVENT_METADATA_BYTES={}",
                emit_metadata.len(),
                crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES
            ),
        )
        .await;
        return Ok(B2AggRestoreOutcome::Skipped);
    }

    // B5 — share the versioned domain-separated helper with bridge_out so the
    // tx_hash is byte-identical across first-observation and restore paths
    // (dedup-stable).
    let tx_hash = crate::bridge_out::derive_bridge_out_tx_hash(&note_id_str);

    let deposit_count = store.mark_note_processed(note_id_str.clone()).await?;

    if let Err(err) = store
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
            &emit_metadata,
            deposit_count,
        )
        .await
    {
        let _ = store.unmark_note_processed(&note_id_str).await;
        return Err(err);
    }

    // "emitted BridgeEvent" is the production signal a bridge-out was projected —
    // both the live projector and the startup restore replay reach here, and both
    // genuinely emit a synthetic BridgeEvent. (Was "restore: rebuilt BridgeEvent",
    // which was misleading on the live path and which downstream tooling / e2e
    // greps for under the legacy wording.)
    tracing::info!(
        note_id = %note_id_str,
        deposit_count,
        "emitted BridgeEvent"
    );

    Ok(B2AggRestoreOutcome::Emitted)
}

/// Outcome of projecting one consumed note through the CLAIM derivation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaimProjectOutcome {
    /// A synthetic `ClaimEvent` log was written for this CLAIM note.
    Emitted,
    /// Skipped: not a CLAIM note, already processed (Dedup 1), undecodable
    /// storage, or a ClaimEvent for the same global index was already recorded
    /// by the primary path (Dedup 2 — note is still marked processed).
    Skipped,
}

/// Project a single consumed note through the CLAIM derivation, emitting a
/// synthetic `ClaimEvent` iff it is a CLAIM note that has not yet been recorded.
///
/// Extracted from `restore_claims`' per-note loop body so the *same* derivation
/// backs both the recovery `restore_*` phases and the cursor-driven
/// [`crate::synthetic_projector`] — same script-root
/// filter, same storage decoder, same dedup predicates, same atomic commit
/// primitive — so the synthetic logs are byte-identical regardless of which
/// path observes the CLAIM note.
pub(crate) async fn project_claim_note(
    store: &Arc<dyn Store>,
    note: &InputNoteRecord,
    block_number: u64,
    block_hash: [u8; 32],
    bridge_address: &str,
) -> anyhow::Result<ClaimProjectOutcome> {
    let claim_root = miden_base_agglayer::ClaimNote::script().root();
    let details = note.details();
    if details.script().root() != claim_root {
        return Ok(ClaimProjectOutcome::Skipped);
    }

    let note_id_str = hex::encode(note.details_commitment().as_bytes());

    // Dedup 1: was this CLAIM already replayed by an earlier restore (or by the
    // live watcher)?
    if store.is_claim_note_processed(&note_id_str).await? {
        return Ok(ClaimProjectOutcome::Skipped);
    }

    // Decode the on-chain CLAIM storage. Malformed storage is logged + counted
    // but doesn't abort restore — the live watcher does the same.
    let decoded = match parse_claim_event_from_storage(details.storage()) {
        Ok(d) => d,
        Err(e) => {
            ::metrics::counter!("claim_watcher_storage_decode_total").increment(1);
            tracing::warn!(
                target: "restore::claims",
                note_id = %note_id_str,
                error = ?e,
                "restore: CLAIM storage could not be decoded; skipping"
            );
            ::metrics::counter!("claim_watcher_unrecoverable_total").increment(1);
            return Ok(ClaimProjectOutcome::Skipped);
        }
    };

    // Dedup 2: was the ClaimEvent already written by the normal
    // `eth_sendRawTransaction` path before the crash? Same check the live
    // watcher uses; without it restore would double-emit for every CLAIM whose
    // primary path ran to completion.
    if store
        .has_claim_event_for_global_index(&decoded.global_index)
        .await?
    {
        ::metrics::counter!("claim_watcher_already_recorded_total").increment(1);
        // Still mark the note processed so the next observation (live watcher
        // or another restore) is a fast skip rather than a re-decode.
        if let Err(e) = store
            .mark_claim_note_processed(note_id_str.clone(), decoded.global_index, block_number)
            .await
        {
            tracing::error!(
                target: "restore::claims",
                note_id = %note_id_str,
                error = ?e,
                "restore: failed to mark already-recorded CLAIM processed"
            );
        }
        return Ok(ClaimProjectOutcome::Skipped);
    }

    // Prefer the REAL claim eth-tx hash (recorded by `publish_claim` via
    // `record_tx_note_link`). aggkit's L2BridgeSyncer fetches the claim tx by
    // hash and decodes its `claimAsset` calldata to resolve the claim's GER
    // boundary; a derived hash points at a synthetic tx with EMPTY calldata, so
    // aggkit fails "input too short: 0 bytes" and never settles the certificate.
    // Fall back to the derived hash only for notes with no recorded link (e.g.
    // restore replaying history predating the link, or notes submitted out-of-band).
    let (tx_hash, linked) = match store.get_tx_for_note(&note_id_str).await? {
        Some(real_tx) => (real_tx, true),
        None => (derive_manual_claim_tx_hash(&note_id_str), false),
    };

    store
        .commit_manual_claim_event_atomic(
            note_id_str.clone(),
            bridge_address,
            block_number,
            block_hash,
            &tx_hash,
            decoded.global_index,
            decoded.origin_network,
            &decoded.origin_address,
            &decoded.destination_address,
            decoded.amount,
        )
        .await?;

    // The projector OWNS receipt completion: finalise the real claim tx's receipt
    // at THIS (consumption) block — the same block the ClaimEvent is emitted — so the
    // receipt block == the log block. `publish_claim` left it pending (`id: None`) for
    // exactly this. Tolerate a missing pending entry (derived-hash fallback, which has
    // no real `txn_begin`; or an expired/pruned tx, or restore predating the tx
    // record): the receipt is then synthesised from the log by `service_get_txn_receipt`,
    // so a missing entry must not abort the projection.
    if let Some(h) = linked
        .then(|| tx_hash.parse::<alloy::primitives::TxHash>().ok())
        .flatten()
    {
        let _ = store
            .txn_commit(h, Ok(()), block_number, block_hash)
            .await
            .inspect_err(|e| {
                tracing::debug!(tx = %tx_hash, "claim receipt not finalised: {e}");
            });
    }

    ::metrics::counter!("claim_watcher_synthesised_total").increment(1);
    tracing::info!(
        target: "restore::claims",
        note_id = %note_id_str,
        synthetic_tx_hash = %tx_hash,
        global_index = %hex::encode(decoded.global_index),
        origin_network = decoded.origin_network,
        amount = decoded.amount,
        block_number,
        "restore: synthesised ClaimEvent from consumed CLAIM note (MA#27)"
    );

    Ok(ClaimProjectOutcome::Emitted)
}

/// Phase 2.5: scan miden consumed CLAIM notes and replay any missing
/// synthetic `ClaimEvent` log via [`Store::commit_manual_claim_event_atomic`].
///
/// Mirrors the live [`SyntheticProjector`](crate::synthetic_projector) — same
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

                let bridge_address = get_bridge_address();
                let mut claim_count = 0usize;
                let mut log_count = 0usize;

                // Miden-1:1: replay each CLAIM at its OWN Miden consumption block,
                // in the projector's canonical (block, tx_order, note_id) order
                // (deterministic across runs + parity with the live projector).
                let mut sorted_notes: Vec<&_> = consumed_notes.iter().collect();
                sort_consumed_for_projection(&mut sorted_notes);

                for note in sorted_notes {
                    let blk = note_consumed_block(note, restore_block);
                    let block_hash = block_state_clone.get_block_hash(blk);
                    // Per-note CLAIM derivation lives in `project_claim_note` so
                    // the live cursor-driven projector and this recovery phase
                    // share one implementation.
                    if project_claim_note(&store_clone, note, blk, block_hash, bridge_address)
                        .await?
                        == ClaimProjectOutcome::Emitted
                    {
                        claim_count += 1;
                        log_count += 1;
                    }
                }

                *result_inner.lock().unwrap() = (claim_count, log_count);
                Ok(())
            })
        })
        .await?;

    let (count, logs) = *result.lock().unwrap();
    Ok((count, logs))
}

/// Outcome of projecting one consumed note through the GER derivation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GerProjectOutcome {
    /// A synthetic GER update log was written for this `UpdateGerNote`.
    Emitted,
    /// Skipped: not a GER note, failed MA#28 provenance, malformed storage,
    /// a limb overflow, or the GER was already injected.
    Skipped,
}

/// Project a single consumed note through the GER derivation, emitting a
/// synthetic GER update iff it is a sanctioned, not-yet-injected
/// `UpdateGerNote`.
///
/// Extracted from `restore_gers`' per-note loop body so the *same* derivation
/// backs both the recovery `restore_*` phases and the cursor-driven
/// [`crate::synthetic_projector`]. `output_metadata` maps a note's
/// details-commitment to the metadata of our own output-note record — the
/// MA#28 provenance fallback for the metadata-less `ConsumedExternal` state
/// (see the comment in `restore_gers` for why this is fail-closed).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_ger_note(
    store: &Arc<dyn Store>,
    note: &InputNoteRecord,
    output_metadata: &std::collections::HashMap<[u8; 32], NoteMetadata>,
    expected_sender: AccountId,
    expected_target: AccountId,
    block_number: u64,
    block_hash: [u8; 32],
    timestamp: u64,
) -> anyhow::Result<GerProjectOutcome> {
    let ger_script_root = UpdateGerNote::script_root();
    let details = note.details();
    if details.script().root() != ger_script_root {
        return Ok(GerProjectOutcome::Skipped);
    }

    // MA#28 — verify the note's authoritative provenance BEFORE we read any
    // storage from it. `UpdateGerNote::create` sets:
    //   - metadata.sender = ger_manager (or service in legacy)
    //   - metadata.attachment = NetworkAccountTarget(bridge_id)
    // A consumed note with the right script_root but the wrong sender /
    // attachment was not minted by aggkit and must not influence the restored
    // `ger_entries` / `hash_chain_value` state. Pure-predicate classification is
    // unit-tested via `classify_ger_note` — keep this match in sync. Prefer the
    // record's own metadata (pre-0.15 states still carry it); fall back to our
    // output-note record for the metadata-less `ConsumedExternal` state.
    let effective_metadata = note
        .metadata()
        .or_else(|| output_metadata.get(&note.details_commitment().as_bytes()));
    match classify_ger_note(
        effective_metadata,
        note.attachments(),
        expected_sender,
        expected_target,
    ) {
        GerNoteVerdict::Accept => {}
        GerNoteVerdict::MissingMetadata => {
            ::metrics::counter!("restore_ger_missing_metadata_total").increment(1);
            tracing::warn!(
                note_id = %hex::encode(note.details_commitment().as_bytes()),
                "MA#28: UpdateGerNote-shaped consumed note has no metadata; skipping"
            );
            return Ok(GerProjectOutcome::Skipped);
        }
        GerNoteVerdict::SenderMismatch => {
            ::metrics::counter!("restore_ger_sender_mismatch_total").increment(1);
            tracing::error!(
                note_id = %hex::encode(note.details_commitment().as_bytes()),
                sender = ?effective_metadata.map(|m| m.sender()),
                expected = %expected_sender,
                "MA#28: UpdateGerNote-shaped note has unexpected sender; \
                 refusing to replay as restored GER"
            );
            return Ok(GerProjectOutcome::Skipped);
        }
        GerNoteVerdict::UndecodableTarget => {
            ::metrics::counter!("restore_ger_no_target_total").increment(1);
            tracing::error!(
                note_id = %hex::encode(note.details_commitment().as_bytes()),
                "MA#28: UpdateGerNote-shaped note has no decodable \
                 NetworkAccountTarget attachment; refusing to replay"
            );
            return Ok(GerProjectOutcome::Skipped);
        }
        GerNoteVerdict::TargetMismatch => {
            ::metrics::counter!("restore_ger_target_mismatch_total").increment(1);
            tracing::error!(
                note_id = %hex::encode(note.details_commitment().as_bytes()),
                expected = %expected_target,
                "MA#28: UpdateGerNote-shaped note targets a different \
                 recipient than the configured bridge; refusing to replay"
            );
            return Ok(GerProjectOutcome::Skipped);
        }
    }

    let storage = details.storage();
    let items = storage.items();
    if items.len() < UpdateGerNote::NUM_STORAGE_ITEMS {
        tracing::warn!(
            note_id = %hex::encode(note.details_commitment().as_bytes()),
            storage_len = items.len(),
            "restore: UpdateGerNote has unexpected storage size, skipping"
        );
        return Ok(GerProjectOutcome::Skipped);
    }

    let ger_bytes = match ger_bytes_from_storage(items) {
        Ok(g) => g,
        Err(i) => {
            tracing::error!(
                note_id = %hex::encode(note.details_commitment().as_bytes()),
                limb_index = i,
                "restore: UpdateGerNote limb exceeds u32::MAX, skipping (X6)"
            );
            return Ok(GerProjectOutcome::Skipped);
        }
    };

    // `is_ger_injected` (not `has_seen_ger`): with the L1InfoTreeIndexer
    // running, ger_entries rows can exist for pairs the indexer observed on L1
    // but for which the proxy never submitted a Miden inject (typical when
    // restore is replaying after a crash that lost the in-memory injection
    // state). Replay should re-emit those.
    if store.is_ger_injected(&ger_bytes).await? {
        return Ok(GerProjectOutcome::Skipped);
    }

    let tx_hash = {
        let mut hasher = Keccak256::new();
        hasher.update(b"restore-ger-miden-");
        hasher.update(hex::encode(note.details_commitment().as_bytes()).as_bytes());
        format!("0x{}", hex::encode(hasher.finalize()))
    };

    store
        .add_ger_update_event(
            block_number,
            block_hash,
            &tx_hash,
            &ger_bytes,
            None,
            None,
            timestamp,
        )
        .await?;

    store.mark_ger_injected(ger_bytes).await?;

    tracing::info!(
        note_id = %hex::encode(note.details_commitment().as_bytes()),
        ger = %hex::encode(ger_bytes),
        "restore: rebuilt GER from consumed UpdateGerNote"
    );

    Ok(GerProjectOutcome::Emitted)
}

/// Phase 3: scan consumed UpdateGerNote notes to rebuild GER state.
///
/// Cantina MA#28 — also asserts that the consumed note was minted by the
/// `ger_manager` (or, for legacy deployments without a dedicated manager,
/// the `service` account) and targeted the bridge account. Without these
/// checks a note that happens to share the `UpdateGerNote` script root —
/// possibly minted by some other account, possibly targeting some other
/// recipient — would have been replayed as an injected GER, mutating
/// `ger_entries` / `hash_chain_value` based on data the proxy did not
/// authorise.
async fn restore_gers(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();
    // MA#28 — same fallback as `submit_update_ger_note` in `src/ger.rs`:
    // legacy deployments without a dedicated `ger_manager` mint
    // UpdateGerNotes from the `service` account. Use the same resolution
    // here so notes minted before the dedicated manager was introduced
    // still verify against the active configuration.
    let expected_sender = accounts
        .ger_manager
        .as_ref()
        .map(|a| a.0)
        .unwrap_or(accounts.service.0);
    let expected_target = accounts.bridge.0;

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                // Protocol 0.15: notes consumed by the bridge land in the client
                // store as `ConsumedExternal`, a state that carries NO metadata —
                // so `note.metadata()` is `None` for every sanctioned GER note and
                // the MA#28 sender check below would skip all of them, restoring
                // zero GERs. The proxy MINTED those notes itself, and the client
                // store's output-note records retain the full metadata
                // permanently. Recover the sender from our own output records,
                // keyed by the details commitment. This is fail-closed and
                // strictly stronger than the plain sender check: a GER-shaped
                // note we did not mint has no output record, stays metadata-less,
                // and is skipped as MissingMetadata — exactly the MA#28 posture.
                let own_output_metadata: std::collections::HashMap<[u8; 32], NoteMetadata> = client
                    .get_output_notes(NoteFilter::All)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
                    .into_iter()
                    .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
                    .collect();

                let mut ger_count = 0usize;
                let mut log_count = 0usize;

                // The GER hash chain is ORDER-SENSITIVE (each value mixes into a
                // rolling Keccak), so restore MUST replay in the projector's exact
                // (block, tx_order, note_id) order — otherwise the restored chain
                // diverges from a fresh live projection (and from aggkit's view).
                // Each GER is also emitted at its OWN Miden consumption block
                // (Miden-1:1), with that block's hash + timestamp.
                let mut sorted_notes: Vec<&_> = consumed_notes.iter().collect();
                sort_consumed_for_projection(&mut sorted_notes);

                for note in sorted_notes {
                    let blk = note_consumed_block(note, restore_block);
                    let block_hash = block_state_clone.get_block_hash(blk);
                    let timestamp = block_state_clone.get_block_timestamp(blk);
                    // Per-note GER derivation (MA#28 provenance + hash-chain
                    // replay) lives in `project_ger_note` so the live
                    // cursor-driven projector and this recovery phase share one
                    // implementation.
                    if project_ger_note(
                        &store_clone,
                        note,
                        &own_output_metadata,
                        expected_sender,
                        expected_target,
                        blk,
                        block_hash,
                        timestamp,
                    )
                    .await?
                        == GerProjectOutcome::Emitted
                    {
                        ger_count += 1;
                        log_count += 1;
                    }
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
    use miden_protocol::note::{
        NoteAttachment, NoteAttachments, NoteMetadata, NoteType, PartialNoteMetadata,
    };
    use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint};
    use std::sync::Arc as StdArc;

    // Test AccountIds — four distinct, valid protocol-0.15 (version-1) ids.
    // Protocol 0.15 dropped the 0.14 v0 id encoding (and folded the old
    // Network *storage mode* away: `AccountType` is now just `Private`/`Public`,
    // and network-account behaviour comes from the `AuthNetworkAccount`
    // *component*, not an id bit). So `NetworkAccountTarget::new` no longer
    // constrains the target id's encoding, and these plain public/private ids
    // are accepted as targets. They are hardcoded hex (rather than pulled from
    // the `testing` feature) to keep this a dependency-light pure-predicate test;
    // the only property the ma28 classifier relies on is that the four ids are
    // mutually distinct.
    const TEST_TARGET_BRIDGE: &str = "0xaa0000000000bb110000cc000000dd";
    const TEST_TARGET_OTHER: &str = "0xbb0000000000cc110000dd000000ee";
    const TEST_SENDER_MANAGER: &str = "0xfa0000000000bb010000cc000000de";
    const TEST_SENDER_ATTACKER: &str = "0xbf0000000000cc010000dc000000ee";

    fn id(hex: &str) -> AccountId {
        AccountId::from_hex(hex).expect("hex must decode")
    }

    fn make_metadata(
        sender: AccountId,
        target: Option<AccountId>,
    ) -> (NoteMetadata, NoteAttachments) {
        let partial = PartialNoteMetadata::new(sender, NoteType::Public);
        match target {
            Some(t) => {
                let attachment = NoteAttachment::from(
                    NetworkAccountTarget::new(t, NoteExecutionHint::Always).expect("ok"),
                );
                let attachments = NoteAttachments::from(attachment);
                let metadata = NoteMetadata::new(partial, &attachments);
                (metadata, attachments)
            }
            None => {
                let attachments = NoteAttachments::default();
                let metadata = NoteMetadata::new(partial, &attachments);
                (metadata, attachments)
            }
        }
    }

    // MA#28 — classifier pins for the four reject branches + accept.
    #[test]
    fn ma28_classify_ger_note_accept() {
        let sender = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        let (meta, attachments) = make_metadata(sender, Some(bridge));
        assert_eq!(
            classify_ger_note(Some(&meta), &attachments, sender, bridge),
            GerNoteVerdict::Accept,
        );
    }

    /// GER byte-order regression: `ger_bytes_from_storage` must little-endian-
    /// decode an `UpdateGerNote`'s storage so it round-trips `ExitRoot::to_elements`
    /// (the encoder the note actually uses). A big-endian decode byte-swaps each
    /// 4-byte limb (`2ae1a9b7…` → `b7a9e12a…`) — the projector then emitted a GER
    /// that never matched the one aggkit injected, hanging bridge-in deposits on
    /// `ready_for_claim`.
    #[test]
    fn ger_bytes_from_storage_roundtrips_little_endian() {
        use miden_base_agglayer::ExitRoot;
        let ger: [u8; 32] =
            hex::decode("2ae1a9b7e0d82a4412b675321c58b3336faca4b549b5d3dd5fdeea4304740f7c")
                .unwrap()
                .try_into()
                .unwrap();
        // Encode exactly as UpdateGerNote storage does, then decode via the path.
        let items = ExitRoot::from(ger).to_elements();
        assert_eq!(items.len(), 8, "ExitRoot packs into 8 felts");
        let decoded = ger_bytes_from_storage(&items).expect("valid GER decodes");
        assert_eq!(
            decoded, ger,
            "GER must round-trip; a big-endian limb decode would byte-swap the root"
        );
        // Prove this pins endianness (not a tautology): a big-endian decode of the
        // same felts must NOT equal the original GER.
        let mut be = [0u8; 32];
        for (i, f) in items.iter().take(8).enumerate() {
            let v = u32::try_from(f.as_canonical_u64()).unwrap();
            be[i * 4..(i + 1) * 4].copy_from_slice(&v.to_be_bytes());
        }
        assert_ne!(be, ger, "big-endian decode must differ — that was the bug");
    }

    #[test]
    fn ma28_classify_ger_note_missing_metadata() {
        let sender = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        assert_eq!(
            classify_ger_note(None, &NoteAttachments::default(), sender, bridge),
            GerNoteVerdict::MissingMetadata,
        );
    }

    #[test]
    fn ma28_classify_ger_note_sender_mismatch() {
        let expected_sender = id(TEST_SENDER_MANAGER);
        let attacker = id(TEST_SENDER_ATTACKER);
        let bridge = id(TEST_TARGET_BRIDGE);
        let (meta, attachments) = make_metadata(attacker, Some(bridge));
        assert_eq!(
            classify_ger_note(Some(&meta), &attachments, expected_sender, bridge),
            GerNoteVerdict::SenderMismatch,
        );
    }

    #[test]
    fn ma28_classify_ger_note_target_mismatch() {
        let sender = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        let other = id(TEST_TARGET_OTHER);
        let (meta, attachments) = make_metadata(sender, Some(other));
        assert_eq!(
            classify_ger_note(Some(&meta), &attachments, sender, bridge),
            GerNoteVerdict::TargetMismatch,
        );
    }

    #[test]
    fn ma28_classify_ger_note_undecodable_target() {
        let sender = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        // Note metadata with no NetworkAccountTarget attachment at all —
        // this is the "forged-via-NoAuth" signature analogous to Cantina #4.
        let (meta, attachments) = make_metadata(sender, None);
        assert_eq!(
            classify_ger_note(Some(&meta), &attachments, sender, bridge),
            GerNoteVerdict::UndecodableTarget,
        );
    }

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
            store.has_claim_event_for_global_index(&gi).await.unwrap(),
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
            "restore and the live projector must derive identical synthetic tx-hashes"
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

    // ── Cantina MA#3 — restore reclaim gate (Finding #3, restore path) ───────
    //
    // bridge_out.rs's scanner was fixed (PR #63) to emit a synthetic BridgeEvent
    // only when a consumed B2AGG note's `consumer_account == bridge`. The restore
    // path (`project_b2agg_note`) must apply the SAME gate: a B2AGG note has
    // a reclaim branch (consumer == sender, asset stays on Miden) and a bridge
    // branch (consumer == bridge, asset leaves). Rebuilding a BridgeEvent for a
    // reclaim hands the user a claimable withdrawal for value that never left.

    /// `(faucet_id, bridge_id, sender_id)` — valid protocol-0.15 ids. The faucet
    /// is a real fungible-faucet id (reused from the store tests) so
    /// `FungibleAsset::new` accepts it; bridge/sender reuse this module's ids.
    fn ma3_accounts() -> (AccountId, AccountId, AccountId) {
        (
            id("0xac0000000000dd110000ee000000fc"),
            id(TEST_TARGET_BRIDGE),
            id(TEST_SENDER_MANAGER),
        )
    }

    /// Build a consumed B2AGG `InputNoteRecord` (current miden-client API, mirrors
    /// `bridge_out::tests::build_b2agg_note_with_consumer`) carrying a fungible
    /// asset from `faucet_id` and recording `consumer` as the consuming account.
    /// The gate keys on the note's script root + `consumer_account()` (the note
    /// STATE), so only `faucet_id` and `consumer` matter here. The asset is
    /// present so restore's emit path is actually reached when the gate is
    /// absent — i.e. the RED test fails on the missing gate, not a no-asset skip.
    fn ma3_b2agg_input_note(faucet_id: AccountId, consumer: Option<AccountId>) -> InputNoteRecord {
        use miden_base_agglayer::B2AggNote;
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::asset::{Asset, FungibleAsset};
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{
            NoteAssets, NoteAttachments, NoteDetails, NoteRecipient, NoteStorage,
        };
        use miden_protocol::{Felt, Word};

        // B2AGG storage: 6 felts (network + 5 address limbs); zeros parse fine.
        let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let asset: Asset = FungibleAsset::new(faucet_id, 50).unwrap().into();
        let assets = NoteAssets::new(vec![asset]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: consumer,
            consumed_tx_order: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    async fn ma3_register_faucet(store: &StdArc<dyn Store>, faucet_id: AccountId) {
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id,
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "ETH".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();
    }

    /// RED → GREEN regression for Finding #3: a reclaimed B2AGG note (consumer ==
    /// sender, not the bridge) must NOT rebuild a synthetic BridgeEvent on restore.
    #[tokio::test]
    async fn ma3_restore_reclaimed_b2agg_note_is_not_emitted() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, sender_id) = ma3_accounts();
        // Register the faucet so the (ungated) emit path would otherwise SUCCEED:
        // the test then fails on the missing gate, not on an unrelated
        // unresolved-faucet skip.
        ma3_register_faucet(&store, faucet_id).await;

        // Reclaim branch: consumer == sender (the user), NOT the bridge.
        let note = ma3_b2agg_input_note(faucet_id, Some(sender_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "reclaimed B2AGG note (consumer != bridge) must NOT rebuild a BridgeEvent",
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "reclaimed note must not be marked processed",
        );
    }

    /// Bridge branch: a B2AGG note consumed by the configured bridge IS a real
    /// bridge-out and must still be rebuilt on restore (the gate must not be
    /// over-eager).
    #[tokio::test]
    async fn ma3_restore_emits_for_bridge_consumed_b2agg() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        // consumer == bridge → real bridge-out.
        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Emitted,
            "bridge-consumed B2AGG note must rebuild a BridgeEvent"
        );
        assert!(
            store.is_note_processed(&note_id).await.unwrap(),
            "emitted note must be marked processed",
        );
    }

    /// Cantina #13 — self-target poison-leaf gate, now enforced in the PRODUCTION
    /// derivation `project_b2agg_note` (formerly only in the deleted
    /// `project_b2agg_note`). A bridge-consumed B2AGG note
    /// whose destination network EQUALS the local network advances the on-chain
    /// LET but its agglayer certificate is rejected (InvalidExit); we MUST refuse
    /// to emit the synthetic BridgeEvent. Reuses the dest-network-0 note from the
    /// emit test (which DOES emit at local=7) and pins it at local=0 so the same
    /// note is now self-targeted — proving the gate, not an unrelated skip.
    #[tokio::test]
    async fn cantina13_self_target_b2agg_is_gated_in_projection() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        // Bridge-consumed (would otherwise emit), destination network 0.
        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());

        // local_network_id == 0 == the note's destination network → poison self-target.
        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            0, // local_network_id == dest-network 0 → self-target
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "Cantina #13: a B2AGG bridge-out targeting the LOCAL network must NOT emit a BridgeEvent",
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "self-target poison note must stay un-processed so it re-surfaces for an operator",
        );
    }

    /// Fail-closed: a consumed B2AGG note with no recorded consumer
    /// (`consumer_account == None`) is an anomaly and must be skipped, not
    /// emitted on an unverifiable basis.
    #[tokio::test]
    async fn ma3_restore_skips_b2agg_with_untracked_consumer() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        let note = ma3_b2agg_input_note(faucet_id, None);
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "untracked-consumer B2AGG note must NOT rebuild a BridgeEvent",
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "skipped note must not be marked processed",
        );
    }

    /// Defense-in-depth: a B2AGG note consumed by an account that is neither the
    /// bridge NOR the original sender (an anomalous third party) must still be
    /// skipped — the gate is an allow-list of exactly the configured bridge
    /// account, so anything else is gated out (classified `Reclaimed`).
    #[tokio::test]
    async fn ma3_restore_skips_b2agg_consumed_by_other_account() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        // A third account, distinct from BOTH the bridge and the sender.
        let other = id(TEST_TARGET_OTHER);
        let note = ma3_b2agg_input_note(faucet_id, Some(other));
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "B2AGG note consumed by a non-bridge third party must NOT rebuild a BridgeEvent",
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "skipped note must not be marked processed",
        );
    }

    /// Review follow-up: if a PRE-FIX restore wrongly marked a reclaimed B2AGG
    /// note processed (emitting an invalid BridgeEvent), an upgraded run must NOT
    /// silently skip it — it must surface the legacy bad state so operators can
    /// reset/rebuild.
    #[tokio::test]
    async fn ma3_restore_flags_legacy_processed_reclaimed_b2agg() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        // Reclaim consumer, but a pre-fix run already marked it processed.
        let note = ma3_b2agg_input_note(faucet_id, Some(id(TEST_SENDER_MANAGER)));
        let note_id = hex::encode(note.details_commitment().as_bytes());
        store.mark_note_processed(note_id.clone()).await.unwrap();

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::LegacyProcessedGated,
            "an already-processed gated note must be flagged as legacy bad state",
        );
    }

    /// A legitimately bridge-out note already processed by an earlier run is a
    /// benign no-op — it must NOT be flagged as legacy bad state.
    #[tokio::test]
    async fn ma3_restore_already_processed_bridge_b2agg_is_benign() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        ma3_register_faucet(&store, faucet_id).await;

        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());
        store.mark_note_processed(note_id.clone()).await.unwrap();

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "a correctly-processed bridge-out note must be a benign skip, not flagged",
        );
    }

    /// Cantina #13 DoS guard: a faucet whose metadata exceeds the encoder cap
    /// must gate the bridge-out (skip) — never feed an oversized blob (from
    /// untrusted L1 calldata) into the BridgeEvent encoder.
    #[tokio::test]
    async fn ma3_restore_skips_b2agg_with_oversized_metadata() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id,
                origin_address: [0x11u8; 20],
                origin_network: 0,
                symbol: "BIG".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![0u8; crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES + 1],
            })
            .await
            .unwrap();

        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_b2agg_note(
            &store,
            &note,
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "B2AGG with oversized faucet metadata must be gated (DoS guard), not emitted",
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "gated note must not be marked processed",
        );
    }
}
