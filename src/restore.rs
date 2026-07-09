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

/// Provenance verdict for a `ClaimNote`-shaped consumed note — the ClaimEvent
/// analogue of MA#28's [`GerNoteVerdict`] (GER path) and MA#3's
/// [`crate::bridge_out::B2AggConsumerClass`] (B2AGG path).
///
/// Live-proven gap: a read-only reindex of a chain shared with a FOREIGN
/// miden-agglayer deployment projected the foreign deployment's claims into
/// our synthetic_logs, because `project_claim_note` gated only on the
/// ClaimNote script root. The script root is deployment-independent — every
/// agglayer instance on the chain mints notes with the identical script.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimNoteVerdict {
    /// Provably OURS — safe to project a synthetic ClaimEvent.
    Ours,
    /// Not provably ours: consumed by some other account (a foreign
    /// deployment's bridge) and not minted by our service targeting our
    /// bridge. Fail-closed skip.
    Foreign,
}

/// Pure provenance predicate for a `ClaimNote`-shaped consumed note. A claim
/// is OURS iff at least one of two independent proofs holds:
///
/// 1. **Consumer proof (MA#3 trust root):** `consumer == our bridge`. Our
///    bridge network account only consumes notes targeted at it, and its MASM
///    validates the claim proof on consumption — so a bridge-consumed CLAIM is
///    a sanctioned claim through OUR deployment regardless of who minted it.
///    This is the same attribution the projector's spent-before-import
///    recovery derives from the bridge's `sync_transactions` feed.
/// 2. **Mint proof (MA#28 trust root):** the note's (own-output-record
///    recovered) metadata shows `sender == our service` — `create_claim` mints
///    every CLAIM from `accounts.service` — AND its `NetworkAccountTarget`
///    attachment targets OUR bridge.
///
/// A foreign deployment's claim satisfies neither: it targets and is consumed
/// by the FOREIGN bridge account, and its sender is the foreign service.
/// Pure (no I/O, no metrics) so it is unit-testable directly; metric emission
/// and tracing live at the call site in `project_claim_note`.
pub fn classify_claim_note(
    consumer: Option<AccountId>,
    metadata: Option<&NoteMetadata>,
    attachments: &NoteAttachments,
    expected_sender: AccountId,
    bridge_id: AccountId,
) -> ClaimNoteVerdict {
    if consumer == Some(bridge_id) {
        return ClaimNoteVerdict::Ours;
    }
    if let Some(meta) = metadata
        && meta.sender() == expected_sender
        && decode_network_target(attachments) == Some(bridge_id)
    {
        return ClaimNoteVerdict::Ours;
    }
    ClaimNoteVerdict::Foreign
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
    /// Cantina #6 — number of non-ETH faucet `faucet_registry` rows rebuilt from
    /// the bridge's authoritative `faucet_metadata_map` (rows that were missing
    /// on a fresh-DB / `--restore` bootstrap). Rebuilding these BEFORE replaying
    /// bridge-outs is what lets `resolve_faucet_origin` succeed so historical
    /// exits replay instead of being quarantined as `UnknownFaucet`.
    pub faucet_identities_rebuilt: usize,
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
// 8 args: the v0.15.4 merge unions our projector-shared params
// (local_network_id, l1_rpc_url) with the release's PRST-4035 node-scan
// params (node_url, api_key). A config struct here would churn every
// call site for a single-caller function; not worth it.
#[allow(clippy::too_many_arguments)]
pub async fn restore(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    l1_rpc_url: Option<String>,
    node_url: Option<&str>,
    api_key: Option<&str>,
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
    // (`service`) will return `AccountNotFoundOnChain`
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

    // Phase 1.5 (PRST-4035): recover bridge-out notes the local store never
    // recorded (consumed by the bridge via network txs). Tag-scan the node and
    // import them so the Phase 2 NoteFilter::Consumed scan below can see them.
    // Best-effort: a failure must not abort restore.
    if let Some(url) = node_url {
        let from_block: u32 = std::env::var("RECOVER_FROM_BLOCK")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        tracing::info!(
            from_block,
            "Phase 1.5: recovering missed bridge-out notes from the node..."
        );
        match recover_missed_bridge_outs(miden_client, url, api_key, accounts.bridge.0, from_block)
            .await
        {
            Ok(n) => {
                tracing::info!("Phase 1.5 complete: recovered {n} B2AGG note(s) from the node")
            }
            Err(e) => tracing::warn!(
                err = %e,
                "Phase 1.5 recovery scan failed; continuing with local-only restore"
            ),
        }
    } else {
        tracing::warn!("Phase 1.5 skipped: no --miden-node URL available to restore()");
    }

    // Phase 1.7 (Cantina #6): rebuild missing non-ETH faucet identity rows from the
    // bridge's authoritative `faucet_metadata_map` BEFORE replaying bridge-outs.
    // Without this, a faucet whose local row was lost on a fresh-DB bootstrap makes
    // `resolve_faucet_origin` error, so `restore_bridge_outs` (Phase 2) and the live
    // `BridgeOutScanner` both quarantine/skip every historical exit tied to it, and
    // the next claim/admin-register deploys a REPLACEMENT faucet → split-brain
    // (Cantina #6). Best-effort: a per-faucet failure is logged + counted, never
    // aborts restore.
    tracing::info!("Phase 1.7: rebuilding faucet identities from bridge state (Cantina #6)...");
    let faucet_identities_rebuilt =
        restore_faucet_identities(store, miden_client, accounts, l1_rpc_url.clone()).await?;
    tracing::info!(
        "Phase 1.7 complete: {faucet_identities_rebuilt} faucet identity row(s) rebuilt"
    );

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
    let (claims, claim_logs) =
        restore_claims(store, miden_client, accounts, block_state, miden_tip).await?;
    total_logs += claim_logs;
    tracing::info!("Phase 2.5 complete: {claims} claims, {claim_logs} logs");

    // Phase 3: Scan consumed UpdateGerNote notes on Miden
    tracing::info!("Phase 3: scanning consumed UpdateGerNote notes on Miden...");
    let (gers, ger_logs) =
        restore_gers(store, miden_client, accounts, block_state, miden_tip).await?;
    total_logs += ger_logs;
    tracing::info!("Phase 3 complete: {gers} GERs, {ger_logs} logs");

    // Phase 4: cursor finalization (factored into a helper so the reconcile-
    // cursor reset is unit-testable — see `finalize_restore_cursors`).
    finalize_restore_cursors(store, miden_tip).await?;

    // Phase 5: Verify
    tracing::info!("Phase 5: verification");
    tracing::info!("  bridge_outs={bridge_outs}, claims={claims}, gers={gers}, logs={total_logs}");
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: miden_tip,
        bridge_outs_restored: bridge_outs,
        faucet_identities_rebuilt,
        claims_restored: claims,
        gers_restored: gers,
        logs_created: total_logs,
    })
}

/// Phase 4 of [`restore`]: finalize the persisted cursors.
///
/// Miden-1:1 — the synthetic tip == the Miden tip, and the projector cursor is
/// set to the Miden tip so the live projector resumes from there rather than
/// re-scanning the blocks restore just replayed (idempotent dedup would skip
/// them anyway). The restored events already sit at their own Miden blocks.
///
/// The note-reconciler sweep cursor is the OPPOSITE: it is reset to 0. Restore
/// runs against a wiped/rebuilt miden store (`--reset-miden-store --restore` is
/// the canonical recovery invocation), so the client has forgotten every
/// imported note — the genesis re-sweep IS the healing pass that re-discovers
/// externally-created network notes, and it must not be skipped by a stale
/// persisted cursor.
pub(crate) async fn finalize_restore_cursors(
    store: &Arc<dyn Store>,
    miden_tip: u64,
) -> anyhow::Result<()> {
    store.set_latest_block_number(miden_tip).await?;
    store.set_projector_cursor(miden_tip).await?;
    tracing::info!("Phase 4: synthetic tip + projector cursor set to Miden tip {miden_tip}");
    store.set_reconcile_cursor(0).await?;
    tracing::info!(
        "reconcile cursor reset — full-history re-sweep will run (restore rebuilds the miden \
         store; the genesis sweep is the healing pass that re-discovers external notes)"
    );
    Ok(())
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
/// PRST-4035 — recover bridge-out notes the local store never saw.
///
/// The bridge account consumes B2AGG bridge-out notes via NETWORK transactions
/// (executed by the ntx-builder, not the proxy's client). Those consumptions are
/// never recorded in the proxy's local miden-client store, so the
/// `NoteFilter::Consumed` scan that both the live [`crate::bridge_out::BridgeOutScanner`]
/// and [`restore_bridge_outs`] rely on cannot see them — the exit is invisible,
/// aggsender never certifies it, and it can't be claimed on L1.
///
/// The notes are still on the node (public, nullifier committed). This
/// re-discovers them by **block-scanning** the node from `from_block` to the
/// chain tip: for every block it enumerates the notes created in it
/// (`ProvenBlock::body().output_notes()`), fetches their full bodies via
/// `get_notes_by_id` (public notes resolve to `FetchedNote::Public` even after
/// they've been consumed), filters to the B2AGG script root with
/// [`is_b2agg_note`], and imports the matches by id (`NoteFile::NoteId`, which
/// fetches from the node and stores). A follow-up `sync_state()` marks each
/// consumed, so they appear in `NoteFilter::Consumed` for [`restore_bridge_outs`]
/// to rebuild the BridgeEvent from. (A tag-sync on `with_account_target(bridge)`
/// returns zero — the bridge's B2AGG notes don't carry that tag — so the tag
/// path is not usable here.)
///
/// Returns the number of B2AGG notes imported. Best-effort: a scan/RPC failure is
/// surfaced to the caller, which logs and continues with the local-only restore.
async fn recover_missed_bridge_outs(
    miden_client: &MidenClient,
    node_url: &str,
    api_key: Option<&str>,
    bridge_id: AccountId,
    from_block: u32,
) -> anyhow::Result<usize> {
    use miden_client::rpc::domain::note::FetchedNote;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{NoteDetails, NoteFile, NoteId};

    let endpoint = crate::miden_client::parse_node_url(node_url)?;
    let rpc = crate::miden_client::build_rpc_client(&endpoint, 30_000, api_key);

    let (tip_header, _) = rpc
        .get_block_header_by_number(None, false)
        .await
        .map_err(|e| anyhow::anyhow!("recovery: get chain tip: {e}"))?;
    let to_block = tip_header.block_num().as_u32();
    if from_block > to_block {
        tracing::warn!(
            from_block,
            to_block,
            "recovery: from_block is past the chain tip; nothing to scan"
        );
        return Ok(0);
    }

    // Tag-independent block scan. The bridge's B2AGG notes are NOT tagged with the
    // bridge as the target account (a tag-sync on `with_account_target(bridge)`
    // returns zero), so walk every block in `[from_block, to_block]`, enumerate
    // the notes it created (`body().output_notes()`), fetch their full bodies via
    // `get_notes_by_id` (public notes come back as `FetchedNote::Public` even when
    // already consumed), and keep the ones whose script root is the B2AGG script.
    // This is the explorer-style "scan blocks, filter is-b2agg" path. The on-chain
    // `consumer == bridge` gating is enforced downstream by `restore_bridge_outs`
    // once the notes are imported and observed consumed.
    let mut b2agg_ids: Vec<NoteId> = Vec::new();
    let mut scanned = 0usize;
    for b in from_block..=to_block {
        let block = match rpc.get_block_by_number(BlockNumber::from(b), false).await {
            Ok(block) => block,
            Err(e) => {
                tracing::warn!(block = b, err = %e, "recovery: get_block_by_number failed; skipping");
                continue;
            }
        };

        // All notes created in this block (public + private), by id.
        let ids: Vec<NoteId> = block.body().output_notes().map(|(_, n)| n.id()).collect();
        if !ids.is_empty() {
            match rpc.get_notes_by_id(&ids).await {
                Ok(fetched) => {
                    for f in fetched {
                        if let FetchedNote::Public(note, _) = f {
                            let details: NoteDetails = note.clone().into();
                            if is_b2agg_note(&details) {
                                b2agg_ids.push(note.id());
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(block = b, err = %e, "recovery: get_notes_by_id failed; skipping block");
                }
            }
        }

        scanned += 1;
        if scanned.is_multiple_of(200) {
            tracing::info!(
                at_block = b,
                to_block,
                scanned,
                b2agg = b2agg_ids.len(),
                "recovery scan: progress"
            );
        }
    }

    tracing::info!(
        bridge = %bridge_id,
        from_block,
        to_block,
        blocks_scanned = scanned,
        b2agg = b2agg_ids.len(),
        "recovery scan complete: B2AGG bridge-out notes found on the node"
    );

    if b2agg_ids.is_empty() {
        return Ok(0);
    }

    let note_files: Vec<NoteFile> = b2agg_ids.iter().copied().map(NoteFile::NoteId).collect();
    let imported = b2agg_ids.len();

    miden_client
        .with(move |client| {
            Box::new(async move {
                client
                    .import_notes(&note_files)
                    .await
                    .map_err(|e| anyhow::anyhow!("recovery: import_notes: {e}"))?;
                // Mark the freshly-imported notes consumed (their nullifiers are
                // committed) so they land in NoteFilter::Consumed for Phase 2.
                client
                    .sync_state()
                    .await
                    .map_err(|e| anyhow::anyhow!("recovery: sync_state after import: {e}"))?;
                Ok(())
            })
        })
        .await?;

    Ok(imported)
}

/// Phase 1.7 (Cantina #6): rebuild missing non-ETH faucet `faucet_registry` rows
/// from the bridge's authoritative `faucet_metadata_map`.
///
/// Enumerates every faucet registered on the bridge, and for each one WITHOUT a
/// local row, reads its origin identity (address / network / scale) back from the
/// bridge storage and its symbol / Miden-decimals from the faucet account, then
/// `store.register_faucet(...)` the reconstructed row. This is a pure READ of
/// public on-chain state — faucets are bridge-owned (mint/burn), so no signing
/// key is involved and the account is never re-deployed (its random seed is
/// unrecoverable; a re-deploy would strand balances in a second generation).
///
/// Returns the number of rows rebuilt. Best-effort: per-faucet failures are
/// logged + counted and never abort restore.
async fn restore_faucet_identities(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    l1_rpc_url: Option<String>,
) -> anyhow::Result<usize> {
    let store_clone = store.clone();
    let bridge_id = accounts.bridge.0;
    let l1_url = l1_rpc_url;

    let count = Arc::new(std::sync::Mutex::new(0usize));
    let count_inner = count.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                // The bridge account holds the authoritative faucet_metadata_map;
                // Phase 0 reimported it. If it's still unavailable we cannot rebuild.
                let Some(bridge_account) = client.get_account(bridge_id).await.ok().flatten() else {
                    tracing::warn!(
                        bridge = %bridge_id,
                        "Cantina #6: bridge account not available locally; skipping faucet-identity rebuild"
                    );
                    return Ok(());
                };

                let faucet_ids = crate::metadata_recovery::enumerate_registered_faucet_ids(
                    bridge_account.storage(),
                );
                tracing::info!(
                    count = faucet_ids.len(),
                    "Cantina #6: bridge registers {} faucet(s); checking local rows",
                    faucet_ids.len()
                );

                let mut rebuilt = 0usize;
                for faucet_id in faucet_ids {
                    match store_clone.get_faucet_by_id(faucet_id).await {
                        Ok(Some(_)) => continue, // already have a local row
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(faucet_id = %faucet_id, error = ?e,
                                "Cantina #6: get_faucet_by_id failed; skipping");
                            continue;
                        }
                    }
                    let Some(conversion) = crate::metadata_recovery::read_faucet_conversion_metadata(
                        bridge_account.storage(),
                        faucet_id,
                    ) else {
                        continue; // native / unregistered — nothing to rebuild
                    };
                    match crate::faucet_ops::rebuild_faucet_entry_from_chain(
                        client,
                        &bridge_account,
                        faucet_id,
                        &conversion,
                        l1_url.as_deref(),
                    )
                    .await
                    {
                        Ok(entry) => {
                            let (origin_network, scale) = (entry.origin_network, entry.scale);
                            match store_clone.register_faucet(entry).await {
                                Ok(()) => {
                                    rebuilt += 1;
                                    ::metrics::counter!("restore_faucet_identity_rebuilt_total")
                                        .increment(1);
                                    tracing::info!(
                                        faucet_id = %faucet_id,
                                        origin_network,
                                        scale,
                                        "Cantina #6: rebuilt missing faucet_registry row from \
                                         bridge faucet_metadata_map"
                                    );
                                }
                                Err(e) => tracing::warn!(faucet_id = %faucet_id, error = ?e,
                                    "Cantina #6: register_faucet failed during rebuild"),
                            }
                        }
                        Err(e) => {
                            ::metrics::counter!("restore_faucet_identity_rebuild_failed_total")
                                .increment(1);
                            tracing::warn!(
                                faucet_id = %faucet_id,
                                error = ?e,
                                "Cantina #6: could not rebuild faucet row from chain; historical \
                                 bridge-outs for this faucet stay quarantined until it is backfilled"
                            );
                        }
                    }
                }

                *count_inner.lock().unwrap() = rebuilt;
                Ok(())
            })
        })
        .await?;

    let n = *count.lock().unwrap();
    Ok(n)
}

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

    // H1 — atomic B2AGG commit. The legacy two-step mark-processed +
    // emit-bridge-event sequence left a crash window: a process kill between
    // the steps recorded the note as processed (deposit_counter bumped) with
    // NO matching BridgeEvent, silently stranding the exit.
    // `commit_b2agg_event_atomic` folds both into a single DB transaction and
    // is idempotent on retry (reuses the original deposit_count, emits no
    // duplicate log — H3).
    let deposit_count = store
        .commit_b2agg_event_atomic(
            note_id_str.clone(),
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
        )
        .await?;

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
///
/// Provenance gate (live-proven): the note must be provably OURS — see
/// [`classify_claim_note`]. `output_metadata` maps a note's details-commitment
/// to the metadata of our own output-note record, the same MA#28 fallback the
/// GER path uses for the metadata-less `ConsumedExternal` state.
/// `expected_sender` is the account `create_claim` mints from
/// (`accounts.service`); `bridge_id` is our bridge account.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_claim_note(
    store: &Arc<dyn Store>,
    note: &InputNoteRecord,
    output_metadata: &std::collections::HashMap<[u8; 32], NoteMetadata>,
    expected_sender: AccountId,
    bridge_id: AccountId,
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

    // Provenance gate — BEFORE any storage read, dedup mark, or emission
    // (the MA#28 posture). On a chain shared with a foreign miden-agglayer
    // deployment, foreign claims share our ClaimNote script root; projecting
    // them poisons synthetic_logs with ClaimEvents our L1 never saw.
    let effective_metadata = note
        .metadata()
        .or_else(|| output_metadata.get(&note.details_commitment().as_bytes()));
    if classify_claim_note(
        note.consumer_account(),
        effective_metadata,
        note.attachments(),
        expected_sender,
        bridge_id,
    ) == ClaimNoteVerdict::Foreign
    {
        ::metrics::counter!("claim_event_foreign_skipped_total").increment(1);
        tracing::warn!(
            target: "restore::claims",
            note_id = %note_id_str,
            consumer = ?note.consumer_account(),
            sender = ?effective_metadata.map(|m| m.sender()),
            expected_sender = %expected_sender,
            bridge = %bridge_id,
            "CLAIM-shaped note is not provably ours (consumer != our bridge, and \
             sender/target don't verify against our service/bridge) — foreign \
             deployment's claim on a shared chain; skipping ClaimEvent (fail-closed)"
        );
        return Ok(ClaimProjectOutcome::Skipped);
    }

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
    accounts: &AccountsConfig,
    block_state: &Arc<BlockState>,
    restore_block: u64,
) -> anyhow::Result<(usize, usize)> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();
    // Claim provenance gate: `create_claim` mints every CLAIM from the
    // service account, targeting the bridge; the bridge is also the sole
    // legitimate consumer. See `classify_claim_note`.
    let expected_sender = accounts.service.0;
    let bridge_id = accounts.bridge.0;

    let result = Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

                // MA#28-style provenance fallback (same as `restore_gers`):
                // protocol 0.15's `ConsumedExternal` state carries no metadata,
                // but we MINTED our own CLAIM notes, so our output-note records
                // retain the full metadata permanently. A claim-shaped note we
                // did not mint has no output record and no bridge-consumer
                // attribution → skipped as Foreign (fail-closed).
                let own_output_metadata: std::collections::HashMap<[u8; 32], NoteMetadata> = client
                    .get_output_notes(NoteFilter::All)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
                    .into_iter()
                    .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
                    .collect();

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
                    if project_claim_note(
                        &store_clone,
                        note,
                        &own_output_metadata,
                        expected_sender,
                        bridge_id,
                        blk,
                        block_hash,
                        bridge_address,
                    )
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

    // Emit the GER log under the REAL `insertGlobalExitRoot` eth-tx (recovered via
    // the note↔tx link `insert_ger` recorded), falling back to a derived hash only
    // for notes with no recorded link (restore replaying history predating the link,
    // or out-of-band injects).
    let note_commitment = hex::encode(note.details_commitment().as_bytes());
    let (tx_hash, linked) = match store.get_tx_for_note(&note_commitment).await? {
        Some(real_tx) => (real_tx, true),
        None => {
            let mut hasher = Keccak256::new();
            hasher.update(b"restore-ger-miden-");
            hasher.update(note_commitment.as_bytes());
            (format!("0x{}", hex::encode(hasher.finalize())), false)
        }
    };

    store
        .commit_ger_event_atomic(
            block_number,
            block_hash,
            &tx_hash,
            &ger_bytes,
            None,
            None,
            timestamp,
        )
        .await?;

    // The projector OWNS receipt completion: finalise the real insertGlobalExitRoot
    // tx's receipt at THIS (consumption) block — the same block the GER log is emitted
    // — so receipt block == log block. `insert_ger` left it pending (`id: None`).
    // Tolerate a missing pending entry (derived-hash fallback, or restore predating the
    // link): the receipt is then synthesised from the log by `service_get_txn_receipt`,
    // so a missing entry must not abort the projection.
    if let Some(h) = linked
        .then(|| tx_hash.parse::<alloy::primitives::TxHash>().ok())
        .flatten()
    {
        let _ = store
            .txn_commit(h, Ok(()), block_number, block_hash)
            .await
            .inspect_err(|e| {
                tracing::debug!(tx = %tx_hash, "GER receipt not finalised: {e}");
            });
    }

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

    /// Cantina #11 regression lock — sharper than the round-trip above: it uses a
    /// deliberately *non-symmetric* GER (`0x0102…20`, every byte distinct) so that
    /// the little-endian and big-endian decodes are provably different for EVERY
    /// 4-byte limb. The finding described the pre-fix `restore_gers()` decoding the
    /// eight storage felts with `to_be_bytes()`, byte-swapping each limb
    /// (`[a0 a1 a2 a3] → [a3 a2 a1 a0]`) and republishing a GER that never existed
    /// on L1 — hanging bridge-in claim readiness after `--restore`.
    ///
    /// Fixed by `ger_bytes_from_storage` decoding little-endian (matching the
    /// `ExitRoot::to_elements()` packing `UpdateGerNote::create` writes to storage).
    /// This test round-trips through that exact encoder and asserts the decode
    /// returns the IDENTICAL 32 bytes, and that the buggy per-limb byte-swap would
    /// have produced a different value — so a regression back to `to_be_bytes()`
    /// fails here.
    #[test]
    fn finding_11_ger_restore_roundtrip_le_not_be() {
        use miden_base_agglayer::ExitRoot;
        // Non-symmetric bytes32: 0x0102030405...1e1f20 — LE≠BE in every limb.
        let mut ger = [0u8; 32];
        for (i, b) in ger.iter_mut().enumerate() {
            *b = (i as u8) + 1;
        }

        // Encode exactly as `UpdateGerNote::create` stores it.
        let items = ExitRoot::from(ger).to_elements();
        assert_eq!(items.len(), 8, "ExitRoot packs the GER into 8 u32 limbs");

        // The fix: little-endian decode round-trips the original bytes byte-for-byte.
        let decoded = ger_bytes_from_storage(&items).expect("valid GER decodes");
        assert_eq!(
            decoded, ger,
            "restore must return the IDENTICAL 32 GER bytes; a big-endian decode \
             (the pre-fix bug) would byte-swap each limb"
        );

        // The pre-fix behaviour, reconstructed here to prove this test discriminates:
        // decoding the SAME felts big-endian yields the per-limb byte-swap, which is
        // NOT the original GER. A regression to `to_be_bytes()` would make
        // `ger_bytes_from_storage` return exactly `buggy_be`, failing the assert above.
        let mut buggy_be = [0u8; 32];
        for (i, f) in items.iter().take(8).enumerate() {
            let v = u32::try_from(f.as_canonical_u64()).unwrap();
            buggy_be[i * 4..(i + 1) * 4].copy_from_slice(&v.to_be_bytes());
        }
        let mut expected_swap = [0u8; 32];
        for (i, chunk) in ger.chunks_exact(4).enumerate() {
            expected_swap[i * 4..(i + 1) * 4]
                .copy_from_slice(&[chunk[3], chunk[2], chunk[1], chunk[0]]);
        }
        assert_eq!(
            buggy_be, expected_swap,
            "the pre-fix big-endian decode byte-swaps each 4-byte limb",
        );
        assert_ne!(
            buggy_be, ger,
            "the pre-fix decode yields a GER different from the encoded one — \
             that mismatch is exactly what this regression lock catches"
        );
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
            faucet_identities_rebuilt: 0,
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

    /// Regression lock for the prod restart-resync incident: a restore run
    /// rebuilds the miden store, so the client has forgotten every imported
    /// note — the genesis re-sweep IS the healing pass. `restore`'s Phase 4
    /// (`finalize_restore_cursors`) must therefore reset the persisted
    /// note-reconciler sweep cursor to 0, even when a previous deployment
    /// left it deep in history — while the projector cursor jumps to the tip.
    #[tokio::test]
    async fn restore_resets_reconcile_cursor_to_genesis() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        // Simulate a long-running pre-restore deployment: both cursors deep
        // into history.
        store.set_reconcile_cursor(123_456).await.unwrap();
        store.set_projector_cursor(100_000).await.unwrap();

        // Phase 4 of restore() — the exact code path the real restore runs.
        finalize_restore_cursors(&store, 130_000).await.unwrap();

        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            0,
            "restore must reset the reconcile cursor to genesis (full-history heal sweep)"
        );
        assert_eq!(
            store.get_projector_cursor().await.unwrap(),
            130_000,
            "projector cursor resumes at the Miden tip (restore already replayed history)"
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 130_000);
    }

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

        // Reclaim consumer, but a pre-fix run already marked it processed
        // (seeded via the sole processed-set write path).
        let note = ma3_b2agg_input_note(faucet_id, Some(id(TEST_SENDER_MANAGER)));
        let note_id = hex::encode(note.details_commitment().as_bytes());
        store
            .commit_b2agg_event_atomic(
                note_id.clone(),
                get_bridge_address(),
                1,
                [7u8; 32],
                "0xtx-legacy",
                0,
                1,
                &[0u8; 20],
                0,
                &[0u8; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();

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

        // An earlier run committed this note through the atomic write path.
        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        let note_id = hex::encode(note.details_commitment().as_bytes());
        store
            .commit_b2agg_event_atomic(
                note_id.clone(),
                get_bridge_address(),
                1,
                [7u8; 32],
                "0xtx-earlier",
                0,
                1,
                &[0u8; 20],
                0,
                &[0u8; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();

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

    // ── Cantina MA#18 — restore-path quarantine branches ─────────────────────
    //
    // The live scanner's quarantine wiring is pinned in `bridge_out::tests`
    // (`ma18_erased_b2agg_quarantined_on_storage_parse_failure` etc.). The
    // restore path re-implements the same four skip sites inside
    // `project_b2agg_note` (`restore.rs`); each must (a) record an
    // `unbridgeable_bridge_out` row with the matching reason, (b) emit NO
    // synthetic BridgeEvent, and (c) leave the note un-processed so a fixed
    // parser / backfilled registry can re-attempt it.

    /// Build a bridge-consumed B2AGG `InputNoteRecord` with caller-chosen
    /// storage felts and assets — the malformed-shape generator for the MA#18
    /// quarantine branches (`ma3_b2agg_input_note` always builds a WELL-formed
    /// note).
    fn ma18_b2agg_input_note(
        storage_felts: Vec<miden_protocol::Felt>,
        assets: Vec<miden_protocol::asset::Asset>,
        consumer: AccountId,
    ) -> InputNoteRecord {
        use miden_base_agglayer::B2AggNote;
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::Word;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{
            NoteAssets, NoteAttachments, NoteDetails, NoteRecipient, NoteStorage,
        };

        let storage = NoteStorage::new(storage_felts).unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let assets = NoteAssets::new(assets).unwrap();
        let details = NoteDetails::new(assets, recipient);
        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: Some(consumer),
            consumed_tx_order: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    /// Run one note through the restore derivation and assert the MA#18
    /// quarantine contract: Skipped outcome, a quarantine row with `reason`,
    /// no synthetic log, note not marked processed.
    async fn assert_ma18_restore_quarantine(
        store: &StdArc<dyn Store>,
        note: &InputNoteRecord,
        bridge_id: AccountId,
        reason: crate::store::UnbridgeableBridgeOutReason,
    ) {
        let note_id = hex::encode(note.details_commitment().as_bytes());
        let outcome = project_b2agg_note(
            store,
            note,
            bridge_id,
            7, // local_network_id (well-formed test notes target dest-network 0)
            42,
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
            "untranslatable B2AGG must be a quarantine skip, not an emit",
        );
        let row = store
            .get_unbridgeable_bridge_out(&note_id)
            .await
            .unwrap()
            .expect("restore skip must write a quarantine row (MA#18)");
        assert_eq!(row.note_id, note_id);
        assert_eq!(row.bridge_account, bridge_id);
        assert_eq!(row.reason, reason);
        assert_eq!(row.observed_block, 42);
        assert!(!row.detail.is_empty(), "detail must carry the skip cause");
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "quarantined note must stay un-processed for later rescue",
        );
        // No synthetic BridgeEvent was emitted for the quarantined note.
        let logs = store
            .get_logs(
                &crate::log_synthesis::LogFilter {
                    from_block: Some("0x0".into()),
                    to_block: Some("0x64".into()),
                    ..Default::default()
                },
                100,
            )
            .await
            .unwrap();
        assert!(
            logs.is_empty(),
            "quarantine path must emit NO BridgeEvent, got {} log(s)",
            logs.len()
        );
    }

    /// MA#18 (a) restore path — bridge-consumed B2AGG with malformed storage
    /// (1 felt; `parse_b2agg_storage` needs ≥ 6) → `StorageParseFailed`.
    #[tokio::test]
    async fn ma18_restore_quarantines_b2agg_with_malformed_storage() {
        use miden_protocol::Felt;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (_faucet_id, bridge_id, _sender_id) = ma3_accounts();
        let note = ma18_b2agg_input_note(vec![Felt::from(0u32)], vec![], bridge_id);
        assert_ma18_restore_quarantine(
            &store,
            &note,
            bridge_id,
            crate::store::UnbridgeableBridgeOutReason::StorageParseFailed,
        )
        .await;
    }

    /// MA#18 (b) restore path — bridge-consumed B2AGG with valid storage but
    /// NO fungible asset (the bridge consumed an empty note) →
    /// `NoFungibleAsset`.
    #[tokio::test]
    async fn ma18_restore_quarantines_b2agg_with_no_fungible_asset() {
        use miden_protocol::Felt;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (_faucet_id, bridge_id, _sender_id) = ma3_accounts();
        let note = ma18_b2agg_input_note(vec![Felt::from(0u32); 6], vec![], bridge_id);
        assert_ma18_restore_quarantine(
            &store,
            &note,
            bridge_id,
            crate::store::UnbridgeableBridgeOutReason::NoFungibleAsset,
        )
        .await;
    }

    /// MA#18 (c) restore path — well-formed bridge-consumed B2AGG whose faucet
    /// is NOT in the registry → `UnknownFaucet`. (Same note shape as the MA#3
    /// emit test, minus the `ma3_register_faucet` step.)
    #[tokio::test]
    async fn ma18_restore_quarantines_b2agg_with_unknown_faucet() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        // Deliberately NOT registering the faucet.
        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        assert_ma18_restore_quarantine(
            &store,
            &note,
            bridge_id,
            crate::store::UnbridgeableBridgeOutReason::UnknownFaucet,
        )
        .await;
    }

    /// MA#18 (d) restore path — the faucet's registered scale makes
    /// `reverse_scale_amount` overflow u128 (10^39 > u128::MAX) →
    /// `AmountOverflow`.
    #[tokio::test]
    async fn ma18_restore_quarantines_b2agg_amount_overflow() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (faucet_id, bridge_id, _sender_id) = ma3_accounts();
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id,
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "OVF".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 39, // 10^39 overflows u128 in reverse_scale_amount
                metadata: vec![],
            })
            .await
            .unwrap();
        let note = ma3_b2agg_input_note(faucet_id, Some(bridge_id));
        assert_ma18_restore_quarantine(
            &store,
            &note,
            bridge_id,
            crate::store::UnbridgeableBridgeOutReason::AmountOverflow,
        )
        .await;
    }

    // ── Cantina MA#28 — ConsumedExternal output-note-metadata fallback ───────
    //
    // Protocol 0.15 strips metadata from `ConsumedExternal` input-note
    // records, so `project_ger_note` recovers provenance from OUR OWN
    // output-note records (we minted every sanctioned UpdateGerNote). The
    // classifier's four verdicts are pinned above (`ma28_classify_*`); these
    // two tests pin the FALLBACK wiring itself, fail-closed and fail-open.

    /// Build a GER-shaped consumed note in the metadata-less
    /// `ConsumedExternal` state (mirrors `synthetic_projector::tests::ger_note`),
    /// returning the record, its would-be output-record metadata entry, and
    /// the GER bytes its storage encodes.
    fn ma28_consumed_external_ger_note(
        ger_byte: u8,
    ) -> (InputNoteRecord, ([u8; 32], NoteMetadata), [u8; 32]) {
        use miden_base_agglayer::UpdateGerNote;
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{
            NoteAssets, NoteAttachment, NoteDetails, NoteRecipient, NoteStorage,
        };
        use miden_protocol::{Felt, Word};

        // 8 u32 limbs, every byte equal → the decoded GER is [ger_byte; 32]
        // regardless of limb endianness.
        let limb = u32::from_be_bytes([ger_byte; 4]);
        let storage = NoteStorage::new(vec![Felt::from(limb); 8]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), UpdateGerNote::script(), storage);
        let details = NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient);

        // Provenance the fallback must recover: sender = ger manager,
        // attachment = NetworkAccountTarget(bridge).
        let bridge = id(TEST_TARGET_BRIDGE);
        let attachment = NoteAttachment::from(
            NetworkAccountTarget::new(bridge, NoteExecutionHint::Always).expect("nat"),
        );
        let attachments = NoteAttachments::from(attachment);
        let partial = PartialNoteMetadata::new(id(TEST_SENDER_MANAGER), NoteType::Public);
        let metadata = NoteMetadata::new(partial, &attachments);

        // ConsumedExternal: NO metadata on the input-note record itself.
        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: Some(bridge),
            consumed_tx_order: None,
        });
        let record = InputNoteRecord::new(details, attachments, None, state);
        let key = record.details_commitment().as_bytes();
        (record, (key, metadata), [ger_byte; 32])
    }

    /// MA#28 fail-closed — a consumed-external GER-shaped note with NO
    /// matching own-output-note record must be skipped as `MissingMetadata`:
    /// no GER restored, no synthetic log. This is exactly the posture for a
    /// same-script note the proxy did NOT mint.
    #[tokio::test]
    async fn ma28_consumed_external_ger_without_output_record_is_fail_closed_skip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (note, _own_meta, ger_bytes) = ma28_consumed_external_ger_note(0x5A);

        let outcome = project_ger_note(
            &store,
            &note,
            &std::collections::HashMap::new(), // no own output record → fail closed
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            3,
            [3u8; 32],
            1_000,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            GerProjectOutcome::Skipped,
            "GER-shaped note without an own output record must be skipped (MissingMetadata)",
        );
        assert!(
            !store.is_ger_injected(&ger_bytes).await.unwrap(),
            "the unverifiable GER must NOT be marked injected",
        );
        let logs = store
            .get_logs(
                &crate::log_synthesis::LogFilter {
                    from_block: Some("0x0".into()),
                    to_block: Some("0x64".into()),
                    ..Default::default()
                },
                100,
            )
            .await
            .unwrap();
        assert!(
            logs.is_empty(),
            "fail-closed skip must emit NO synthetic log"
        );
    }

    // ── ClaimEvent provenance gate — foreign-deployment claims (live-proven) ─
    //
    // A read-only reindex of the real testnet (which hosts a FOREIGN
    // miden-agglayer deployment on the SAME Miden chain) projected 3
    // ClaimEvents from the foreign deployment's claims into our
    // synthetic_logs: `project_claim_note` gated only on the ClaimNote
    // script root, unlike the GER path's MA#28 sender/target gate and the
    // B2AGG path's MA#3 consumer gate. These tests pin the fix: a
    // CLAIM-shaped consumed note must be provably OURS (consumed by OUR
    // bridge, or minted by OUR service targeting OUR bridge) before a
    // synthetic ClaimEvent is projected.

    /// Build a consumed CLAIM note with a valid `ClaimNoteStorage` (so the
    /// pre-fix pipeline would decode + emit — the test then fails on the
    /// missing provenance gate, not an unrelated decode skip), consumed by
    /// `consumer`, with a per-test `gi_byte` to keep global indexes distinct
    /// across tests (Dedup 2 keys on global_index).
    fn claim_input_note(consumer: Option<AccountId>, gi_byte: u8) -> InputNoteRecord {
        use miden_base_agglayer::{
            ClaimNote, ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData,
            MetadataHash, ProofData, SmtNode,
        };
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteAssets, NoteDetails, NoteRecipient, NoteStorage};
        use miden_protocol::{Felt, Word};

        let mut gi_bytes = [0u8; 32];
        gi_bytes[31] = gi_byte;
        let mut amount_bytes = [0u8; 32];
        amount_bytes[28..32].copy_from_slice(&1_000_000u32.to_be_bytes());

        let claim_storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new(gi_bytes),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network: 7,
                origin_token_address: EthAddress::new([0xAB; 20]),
                destination_network: 1,
                destination_address: EthAddress::new([0xCD; 20]),
                amount: EthAmount::new(amount_bytes),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::ZERO,
        };
        let storage = NoteStorage::try_from(claim_storage).expect("claim storage round-trips");
        let recipient = NoteRecipient::new(Word::default(), ClaimNote::script(), storage);
        let details = NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient);

        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: consumer,
            consumed_tx_order: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    /// RED→GREEN PoC for the live finding: a consumed claim-shaped note whose
    /// consumer is NOT our bridge (a foreign deployment's bridge on the same
    /// chain) and which we did not mint must NOT project a ClaimEvent.
    /// Pre-fix this test fails: the note projects (`Emitted`) because
    /// `project_claim_note` gated only on the ClaimNote script root.
    #[tokio::test]
    async fn finding_claim_provenance_foreign_claim_not_projected() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        // Foreign bridge consumed it; we never minted it (no output record).
        let foreign_bridge = id(TEST_SENDER_ATTACKER);
        let note = claim_input_note(Some(foreign_bridge), 0x71);
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_claim_note(
            &store,
            &note,
            &std::collections::HashMap::new(), // we did not mint it → no output record
            id(TEST_SENDER_MANAGER),           // our service
            id(TEST_TARGET_BRIDGE),            // our bridge
            5,
            [5u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            ClaimProjectOutcome::Skipped,
            "a claim-shaped note consumed by a FOREIGN bridge must not project a ClaimEvent",
        );
        assert!(
            !store.is_claim_note_processed(&note_id).await.unwrap(),
            "foreign claim must not be marked processed",
        );
        let mut gi = [0u8; 32];
        gi[31] = 0x71;
        assert!(
            !store.has_claim_event_for_global_index(&gi).await.unwrap(),
            "no ClaimEvent row may exist for the foreign claim's global index",
        );
    }

    /// Positive counterpart — the SAME claim shape consumed by OUR bridge must
    /// still project (consumer proof, MA#3 trust root). Proves the foreign
    /// skip above is the provenance gate, not an over-eager claim kill-switch.
    #[tokio::test]
    async fn finding_claim_provenance_bridge_consumed_claim_projects() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let note = claim_input_note(Some(id(TEST_TARGET_BRIDGE)), 0x72);
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_claim_note(
            &store,
            &note,
            &std::collections::HashMap::new(),
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            5,
            [5u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            ClaimProjectOutcome::Emitted,
            "a claim consumed by OUR bridge must still project a ClaimEvent",
        );
        assert!(store.is_claim_note_processed(&note_id).await.unwrap());
        let mut gi = [0u8; 32];
        gi[31] = 0x72;
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
    }

    /// Mint-proof fallback — a claim with NO consumer attribution but whose
    /// own-output-record metadata shows OUR service minted it targeting OUR
    /// bridge must project (MA#28 trust root: we created it). This is the
    /// `ConsumedExternal` posture for our own claims when the consumer is
    /// untracked.
    #[tokio::test]
    async fn finding_claim_provenance_minted_by_us_projects_via_output_record() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let note = claim_input_note(None, 0x73);

        // Our own output-note record: sender = service, target = our bridge.
        // The record's attachments must also carry the target — mirror what
        // `ClaimNote::create` produces.
        let (metadata, attachments) =
            make_metadata(id(TEST_SENDER_MANAGER), Some(id(TEST_TARGET_BRIDGE)));
        let note = InputNoteRecord::new(
            note.details().clone(),
            attachments,
            None,
            note.state().clone(),
        );
        let output_metadata =
            std::collections::HashMap::from([(note.details_commitment().as_bytes(), metadata)]);

        let outcome = project_claim_note(
            &store,
            &note,
            &output_metadata,
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            5,
            [5u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            ClaimProjectOutcome::Emitted,
            "our own minted claim (output-record metadata proof) must project",
        );
    }

    /// Fail-closed floor — no consumer attribution AND no mint proof (we have
    /// no output record for it) must skip, even though the storage decodes.
    #[tokio::test]
    async fn finding_claim_provenance_unattributed_claim_is_fail_closed_skip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let note = claim_input_note(None, 0x74);

        let outcome = project_claim_note(
            &store,
            &note,
            &std::collections::HashMap::new(),
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            5,
            [5u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ClaimProjectOutcome::Skipped);
    }

    /// Pure-classifier pins for `classify_claim_note` — both proofs and the
    /// reject branches (mirrors the `ma28_classify_*` pin style).
    #[test]
    fn claim_provenance_classifier_branches() {
        let service = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        let foreign = id(TEST_SENDER_ATTACKER);

        // Consumer proof: consumed by our bridge → Ours (metadata irrelevant).
        assert_eq!(
            classify_claim_note(
                Some(bridge),
                None,
                &NoteAttachments::default(),
                service,
                bridge
            ),
            ClaimNoteVerdict::Ours,
        );
        // Mint proof: sender == service AND target == bridge → Ours.
        let (meta, attachments) = make_metadata(service, Some(bridge));
        assert_eq!(
            classify_claim_note(None, Some(&meta), &attachments, service, bridge),
            ClaimNoteVerdict::Ours,
        );
        // Foreign consumer, no metadata → Foreign.
        assert_eq!(
            classify_claim_note(
                Some(foreign),
                None,
                &NoteAttachments::default(),
                service,
                bridge
            ),
            ClaimNoteVerdict::Foreign,
        );
        // Foreign sender (their service minted it) → Foreign.
        let (foreign_meta, foreign_attachments) = make_metadata(foreign, Some(bridge));
        assert_eq!(
            classify_claim_note(
                None,
                Some(&foreign_meta),
                &foreign_attachments,
                service,
                bridge
            ),
            ClaimNoteVerdict::Foreign,
        );
        // Our sender but a DIFFERENT target (their bridge) → Foreign.
        let (meta2, attachments2) = make_metadata(service, Some(id(TEST_TARGET_OTHER)));
        assert_eq!(
            classify_claim_note(None, Some(&meta2), &attachments2, service, bridge),
            ClaimNoteVerdict::Foreign,
        );
        // No attribution at all → Foreign (fail-closed floor).
        assert_eq!(
            classify_claim_note(None, None, &NoteAttachments::default(), service, bridge),
            ClaimNoteVerdict::Foreign,
        );
    }

    /// MA#28 fail-open counterpart — the SAME consumed-external note, when our
    /// output-note records carry its metadata (we minted it), must verify via
    /// the fallback and restore its GER. Proves the skip above is the metadata
    /// gate and nothing else.
    #[tokio::test]
    async fn ma28_consumed_external_ger_with_output_record_restores() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (note, (key, metadata), ger_bytes) = ma28_consumed_external_ger_note(0x5B);
        let output_metadata = std::collections::HashMap::from([(key, metadata)]);

        let outcome = project_ger_note(
            &store,
            &note,
            &output_metadata,
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            3,
            [3u8; 32],
            1_000,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            GerProjectOutcome::Emitted,
            "sanctioned GER note must restore once the output-record metadata verifies it",
        );
        assert!(
            store.is_ger_injected(&ger_bytes).await.unwrap(),
            "restored GER must be marked injected",
        );
    }
}
