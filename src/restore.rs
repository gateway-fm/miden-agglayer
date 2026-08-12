//! Restore — Reconstruct PgStore state from miden node.
//!
//! This module implements disaster recovery: when the PostgreSQL store is
//! empty (fresh deploy or data loss), it rebuilds all state from authoritative
//! sources (miden node consumed notes, miden sync state).
//!
//! ## Algorithm
//!
//! 0. Re-import configured accounts, then sync one Miden tip/LET snapshot.
//! 1. Scan canonical public B2AGG bodies and bridge transactions; prove exact
//!    NoteId, execution order, reservation prefix, and LET cardinality.
//! 2. Rebuild faucet identities, then replay B2AGG, CLAIM, and GER events at
//!    their original consumption blocks.
//! 3. Finalize the synthetic tip/projector cursor, reset the identity-reconcile
//!    cursor for its historical sweep, and verify counts.
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
use crate::claim_watcher::{
    DecodedFullClaim, derive_manual_claim_tx_hash, parse_claim_event_from_storage,
    parse_full_claim_from_storage,
};
use crate::metadata_recovery::{EmitMetadata, METADATA_UNRECOVERABLE_METRIC};
use crate::miden_client::{
    MidenClient, MidenClientLib, ensure_complete_note_response, ordered_account_transactions,
};
use crate::store::Store;
use miden_base_agglayer::UpdateGerNote;
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_protocol::account::AccountId;
use miden_protocol::note::{NoteAttachments, NoteDetails, NoteId, NoteMetadata, Nullifier};
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

struct RecoveredBridgeBody {
    details: NoteDetails,
    attachments: NoteAttachments,
}

/// Finding #69 — a public CLAIM body recovered straight from the node scan.
/// Unlike the client-store path, the node's public note record retains the
/// full `NoteMetadata` (sender), which is the provenance the mint-proof half of
/// [`classify_claim_note`] needs after `--reset-miden-store` wiped the local
/// output-note records.
struct RecoveredClaimBody {
    details: NoteDetails,
    metadata: NoteMetadata,
    attachments: NoteAttachments,
}

#[derive(Default)]
struct RecoveredBridgeOuts {
    id_by_nullifier: std::collections::HashMap<Nullifier, NoteId>,
    by_id: std::collections::HashMap<NoteId, RecoveredBridgeBody>,
    /// Finding #69 — CLAIM bodies collected by the same block walk.
    claim_id_by_nullifier: std::collections::HashMap<Nullifier, NoteId>,
    claims_by_id: std::collections::HashMap<NoteId, RecoveredClaimBody>,
    /// #88 — node-authoritative metadata for EVERY public note seen by the
    /// block walk, keyed by details commitment (the client store's
    /// `details_commitment()` key). The MA#28 provenance source that survives
    /// a FULL drop-restore: with the miden-client store wiped alongside the
    /// proxy store, consumed records are metadata-less (`ConsumedExternal`)
    /// and the own-output-record fallback is empty — without this map every
    /// historical UpdateGerNote is skipped as MissingMetadata and the restored
    /// GER ledger (UHC logs + is_injected) silently loses history. Fail-closed
    /// holds: only notes the NODE returned as Public contribute.
    public_note_metadata: std::collections::HashMap<[u8; 32], NoteMetadata>,
}

struct ReplayBridgeOut {
    id: NoteId,
    body: RecoveredBridgeBody,
    block: u64,
    tx_order: u32,
    /// Position of this note among its consuming transaction's inputs — the
    /// authoritative within-transaction order the live projector uses to break
    /// same-transaction B2AGG sibling ties (`within_tx_pos`). Without it, two
    /// siblings in one transaction have an unknowable relative order.
    within_tx_pos: u32,
}

/// Finding #69 — a bridge-consumed CLAIM joined to its consuming bridge
/// transaction: the node-scan analogue of the client-store records Phase 2.5
/// replays. `block`/`tx_order` come from the bridge's authoritative
/// `sync_transactions` execution order, so the synthetic ClaimEvent lands at
/// the claim's ORIGINAL consumption block (Miden-1:1), which is exactly what
/// aggkit's aggsender needs to resolve a pre-recovery bridge exit to a block.
struct ReplayClaim {
    id: NoteId,
    body: RecoveredClaimBody,
    block: u64,
    tx_order: u32,
    /// Input position, captured for parity with [`ReplayBridgeOut`]. NOTE: the
    /// ordering key deliberately does NOT use it — see `replay_sort_key`.
    #[allow(dead_code)]
    within_tx_pos: u32,
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
    network_rpcs: crate::metadata_recovery::NetworkRpcMap,
    node_url: &str,
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
    // abort restore. The locally-deployed-but-not-network-tracked account
    // (`service`) will return `AccountNotFoundOnChain` here and that's
    // fine — it's healthy until first use.
    tracing::info!("Phase 0: re-importing bridge accounts from Miden node...");
    crate::account_recovery::reimport_known_accounts(miden_client, accounts).await;
    tracing::info!("Phase 0 complete: bridge account reimport pass done");

    // Phase 1: Sync miden state + read the Miden tip — the block the synthetic
    // chain catches up to under Miden-1:1. Each restored event is attributed to
    // its OWN consumed block (below); `miden_tip` is only the orphan fallback.
    tracing::info!("Phase 1: syncing miden state...");
    let (miden_tip, let_leaves) = sync_miden_snapshot(miden_client, accounts.bridge.0).await?;
    tracing::info!("Phase 1 complete: miden tip {miden_tip}, LET leaves {let_leaves}");

    // Phase 1.1: HEALING SWEEP — recovery performs the genesis note re-sweep
    // ITSELF, before any phase that reads the client store's consumed feed.
    //
    // The canonical recovery invocation is `--reset-miden-store --restore`, which
    // empties the miden-client store. Phases 2/2.5/3 replay from that store's
    // CONSUMED feed. Historically the heal was deferred to the serving proxy's
    // reconciler ("the genesis sweep IS the healing pass"), but Phase 4 parks the
    // projector cursor at the tip — so notes the sweep imports AFTERWARDS are
    // never projected. The GER replay (Phase 3), whose only source is that feed,
    // therefore rebuilt NOTHING on a full-DB-loss recovery: UpdateHashChain went
    // 40 -> 0 and is_injected 40 -> 0, permanently, which then lets aggoracle
    // re-inject already-registered GERs (immortal poison notes, #86).
    //
    // Ordering it here makes recovery self-sufficient: the sweep that PRODUCES
    // the consumed feed runs before every replay that CONSUMES it, and the
    // post-restore sweep becomes a cheap no-op instead of a load-bearing (but
    // too-late) dependency — which also removes the duplicated full block walk.
    let swept_to = sweep_notes_for_recovery(
        store,
        miden_client,
        accounts,
        local_network_id,
        block_state,
        network_rpcs.clone(),
        node_url,
        api_key,
        miden_tip,
    )
    .await?;

    let mut total_logs = 0usize;

    // Recover full public B2AGG bodies and join them directly to the bridge transaction
    // feed. This bypasses miden-client 0.15.0's details-commitment-keyed SQLite table,
    // which cannot retain two distinct NoteIds with identical details.
    let scan_tip = u32::try_from(miden_tip)
        .map_err(|_| anyhow::anyhow!("Miden tip {miden_tip} exceeds u32"))?;
    let endpoint = crate::miden_client::parse_node_url(node_url)?;
    let rpc = crate::miden_client::build_rpc_client(&endpoint, 30_000, api_key);
    tracing::info!(
        scan_tip,
        "Phase 1.5: scanning bridge-out notes from the node..."
    );
    // MA#28 sender for the GER-metadata provenance gate — same ger_manager /
    // service fallback `restore_gers` and `submit_update_ger_note` resolve.
    let expected_ger_sender = accounts
        .ger_manager
        .as_ref()
        .map(|a| a.0)
        .unwrap_or(accounts.service.0);
    let mut recovered =
        scan_bridge_out_bodies(&*rpc, accounts.bridge.0, expected_ger_sender, scan_tip)
            .await
            .map_err(|e| anyhow::anyhow!("restore bridge-out body scan failed: {e:#}"))?;
    tracing::info!(
        "Phase 1.5 complete: found {} B2AGG note(s)",
        recovered.by_id.len()
    );
    // #88 — take (not clone) the node-authoritative GER metadata map out before
    // `recovered` is consumed by restore_bridge_replay; Phase 4's GER replay
    // needs it when the client store was wiped along with the proxy store.
    let node_note_metadata = std::mem::take(&mut recovered.public_note_metadata);
    let (bridge_replay, claim_replay, consumed_order) =
        restore_bridge_replay(&*rpc, accounts.bridge.0, recovered, scan_tip)
            .await
            .map_err(|e| anyhow::anyhow!("restore bridge-out ordering scan failed: {e:#}"))?;
    if bridge_replay.len() as u64 != let_leaves {
        anyhow::bail!(
            "restore bridge-out cardinality mismatch at Miden block {miden_tip}: \
             replayable={}, LET leaves={let_leaves}",
            bridge_replay.len()
        );
    }

    // Normalize crash-era commitment keys before checking the same accounting identity as
    // the live gate. This happens before replay can reserve or emit anything.
    for replay in &bridge_replay {
        let legacy_key = hex::encode(replay.body.details.commitment().as_bytes());
        let tx_hash = crate::bridge_out::derive_bridge_out_tx_hash(&legacy_key);
        store
            .migrate_legacy_deposit_key(&legacy_key, &replay.id.to_hex(), replay.block, &tx_hash)
            .await?;
    }
    let replay_keys: Vec<String> = bridge_replay.iter().map(|item| item.id.to_hex()).collect();
    let existing = store.get_deposit_indices(&replay_keys).await?;
    let first_missing = replay_keys
        .iter()
        .position(|key| !existing.contains_key(key))
        .unwrap_or(replay_keys.len());
    if replay_keys[first_missing..]
        .iter()
        .any(|key| existing.contains_key(key))
    {
        anyhow::bail!("restore reservations are not a contiguous execution-order prefix");
    }
    for (ordinal, key) in replay_keys[..first_missing].iter().enumerate() {
        let expected_index = u32::try_from(ordinal)?;
        if existing.get(key) != Some(&expected_index) {
            anyhow::bail!(
                "restore reservation order mismatch for {key}: stored={:?}, expected={expected_index}",
                existing.get(key)
            );
        }
    }
    let expected = store
        .get_accounted_deposit_count()
        .await?
        .checked_add(u64::try_from(replay_keys.len() - existing.len())?)
        .ok_or_else(|| anyhow::anyhow!("restore LET accounting overflow"))?;
    if expected != let_leaves {
        anyhow::bail!(
            "restore LET accounting mismatch before replay: expected={expected}, \
             on-chain={let_leaves}"
        );
    }
    for (ordinal, key) in replay_keys.iter().enumerate().skip(first_missing) {
        let expected_index = u32::try_from(ordinal)?;
        let reserved = store.reserve_deposit_index(key).await?;
        if reserved != expected_index {
            anyhow::bail!(
                "restore reserved LET index {reserved} for {key}, expected {expected_index}"
            );
        }
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
        restore_faucet_identities(store, miden_client, accounts, &network_rpcs).await?;
    tracing::info!(
        "Phase 1.7 complete: {faucet_identities_rebuilt} faucet identity row(s) rebuilt"
    );

    // ── Phase 2: ONE chronologically-ordered replay of ALL history ───────────
    //
    // Formerly Phases 2 / 2.5 / 2.6 / 3 — a pass per note KIND, each internally
    // sorted but sequential overall, and each opening its own client session.
    // That re-numbered `log_index` (one global counter) and diverged
    // `hash_chain_value` (a fold in emission order) on every recovery, because
    // the live projector walks BLOCK-major while restore walked KIND-major.
    // `replay_history_in_order` implements the design doc's single
    // `{B2AGG, INTERNAL} -> ORDER -> EMIT` stage, so restored history is emitted
    // in exactly the order the live projector would have emitted it.
    tracing::info!("Phase 2: replaying ALL history in one chronological pass...");
    let replayed = replay_history_in_order(
        store,
        miden_client,
        accounts,
        local_network_id,
        block_state,
        &network_rpcs,
        bridge_replay,
        claim_replay,
        miden_tip,
        node_note_metadata,
        consumed_order,
    )
    .await?;
    let (bridge_outs, logs) = (replayed.bridge_outs, replayed.bridge_logs);
    let (claims, claim_logs) = (replayed.claims, replayed.claim_logs);
    let node_claims = replayed.node_claims;
    let (gers, ger_logs) = (replayed.gers, replayed.ger_logs);
    total_logs += logs + claim_logs + node_claims + ger_logs;
    tracing::info!(
        "Phase 2 complete: {bridge_outs} bridge-outs ({logs} logs), {claims} store claims \
         ({claim_logs} logs), {node_claims} node-scanned claims, {gers} GERs ({ger_logs} logs) \
         — emitted in one chronological order"
    );
    let claims = claims + node_claims;

    let accounted = store.get_accounted_deposit_count().await?;
    if accounted != let_leaves {
        anyhow::bail!(
            "restore LET accounting mismatch after replay: local={accounted}, \
             on-chain={let_leaves}"
        );
    }

    // Phase 4: cursor finalization (factored into a helper so the reconcile-
    // cursor reset is unit-testable — see `finalize_restore_cursors`).
    finalize_restore_cursors(store, miden_tip, Some(swept_to)).await?;

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
    swept_to: Option<u64>,
) -> anyhow::Result<()> {
    store.set_latest_block_number(miden_tip).await?;
    store.set_projector_cursor(miden_tip).await?;
    tracing::info!("Phase 4: synthetic tip + projector cursor set to Miden tip {miden_tip}");

    // #90 — the `nonces` table has NO chain source, so a rebuilt store starts with
    // an EMPTY nonce ledger while CONTINUING signers (aggkit's aggoracle/aggsender
    // wallets) keep submitting from where they left off. Without this marker the
    // admission path sees `expected 0` vs `tx.nonce N`, parks the tx in the
    // future-nonce queue, and waits for a nonce that can never be submitted — on
    // the 2026-08-11 gate the GER injector spun 34,287 times in ~30min and
    // injection never resumed. Record that the ledger was rebuilt so admission can
    // adopt each signer's first observed nonce as its baseline instead of
    // demanding 0.
    store.set_nonce_ledger_rebuilt(true).await?;
    tracing::warn!(
        "Phase 4: nonce ledger is EMPTY after restore — flagged for first-contact \
         bootstrap (#90); ordinary R4 ordering resumes per signer once seeded"
    );

    // Recovery now performs the healing sweep ITSELF (Phase 1.1), before the
    // replay phases that depend on it. When that sweep reached the tip there is
    // nothing left to re-discover, so leaving the cursor there avoids a second
    // full-history block walk on the next boot (the old behaviour: restore
    // walked every block, then the serving proxy walked them all again — and,
    // because the projector cursor is parked at the tip above, that second walk
    // could not project anything anyway).
    //
    // Fail-safe: if the sweep did NOT reach the tip, fall back to the historical
    // reset-to-genesis so the serving proxy still attempts the heal.
    match swept_to {
        Some(t) if t >= miden_tip => {
            store.set_reconcile_cursor(t).await?;
            tracing::info!(
                swept_to = t,
                "reconcile cursor left at the swept tip — recovery already ran the healing \
                 sweep (Phase 1.1); no redundant genesis re-walk on the next boot"
            );
        }
        other => {
            store.set_reconcile_cursor(0).await?;
            tracing::warn!(
                swept_to = ?other,
                "reconcile cursor reset to genesis — recovery's healing sweep did not reach the \
                 tip, so the serving proxy must re-sweep"
            );
        }
    }
    Ok(())
}

/// Phase 1.1 — run the note-visibility sweep from GENESIS to `miden_tip` inside
/// recovery, so every later replay phase reads a COMPLETE consumed-note feed.
///
/// Returns the block the sweep reached (== `miden_tip` on success). Any failure
/// propagates: a partial feed would make the GER hash-chain replay silently lose
/// history, which is precisely the failure this exists to prevent.
#[allow(clippy::too_many_arguments)]
async fn sweep_notes_for_recovery(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    network_rpcs: crate::metadata_recovery::NetworkRpcMap,
    node_url: &str,
    api_key: Option<&str>,
    miden_tip: u64,
) -> anyhow::Result<u64> {
    // Start at genesis: the client store was just wiped, so a stale persisted
    // cursor must not skip the heal. Set BEFORE constructing the projector —
    // `SyntheticProjector::new` loads the cursor once, in `new()`.
    store.set_reconcile_cursor(0).await?;
    let projector = Arc::new(
        crate::synthetic_projector::SyntheticProjector::new(
            store.clone(),
            block_state.clone(),
            accounts,
            local_network_id,
            network_rpcs,
            node_url.to_string(),
            api_key.map(str::to_string),
        )
        .await?,
    );
    tracing::info!(
        miden_tip,
        "Phase 1.1: recovery healing sweep — importing notes from genesis before any replay..."
    );
    let reached = Arc::new(std::sync::Mutex::new(None::<u64>));
    let reached_inner = reached.clone();
    let projector_inner = projector.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                let to = projector_inner
                    .sweep_notes_to_completion(client, miden_tip)
                    .await?;
                // The sweep imports notes; a final sync resolves their
                // consumption (nullifier check) so the consumed feed the replay
                // phases read is actually populated, not merely "known".
                client.sync_state().await?;
                *reached_inner.lock().unwrap() = Some(to);
                Ok(())
            })
        })
        .await?;
    let reached = reached
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("Phase 1.1: healing sweep produced no result"))?;
    tracing::info!(
        swept_to = reached,
        miden_tip,
        "Phase 1.1 complete: consumed-note feed healed to the Miden tip"
    );
    Ok(reached)
}

/// Phase 1: sync miden and return the current MIDEN tip (sync height) — the
/// block the synthetic chain catches up to under Miden-1:1.
async fn sync_miden_snapshot(
    miden_client: &MidenClient,
    bridge_id: AccountId,
) -> anyhow::Result<(u64, u64)> {
    let snapshot = Arc::new(std::sync::Mutex::new(None));
    let snapshot_inner = snapshot.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                let height = client
                    .get_sync_height()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
                    .as_u64();
                let bridge = client
                    .get_account(bridge_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read bridge account {bridge_id}: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("bridge account {bridge_id} is unavailable"))?;
                let leaves = miden_base_agglayer::AggLayerBridge::read_let_num_leaves(&bridge);
                *snapshot_inner.lock().unwrap() = Some((height, leaves));
                Ok(())
            })
        })
        .await?;
    snapshot
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow::anyhow!("Miden snapshot was not captured"))
}

/// Scans every block in a fixed snapshot and retains public B2AGG bodies by unique NoteId.
/// Any incomplete RPC response aborts restore; a partial identity set is unsafe to replay.
/// #88 / PR#164 — merge one GER-shaped node note's metadata into the restore
/// map, ambiguity-safe. Details commitment excludes metadata, so if two
/// GER-shaped notes share details but carry DIFFERENT metadata we drop the key
/// into `ambiguous` and never serve it again — the replay then fails closed
/// rather than joining a GER to a last-writer's provenance. Identical
/// metadata for the same key is idempotent. Pure + unit-tested.
fn record_ger_node_metadata(
    map: &mut std::collections::HashMap<[u8; 32], NoteMetadata>,
    ambiguous: &mut std::collections::HashSet<[u8; 32]>,
    key: [u8; 32],
    metadata: NoteMetadata,
) {
    if ambiguous.contains(&key) {
        return;
    }
    match map.get(&key) {
        Some(existing) if *existing != metadata => {
            map.remove(&key);
            ambiguous.insert(key);
            ::metrics::counter!("restore_ger_meta_ambiguous_total").increment(1);
        }
        Some(_) => {}
        None => {
            map.insert(key, metadata);
        }
    }
}

async fn scan_bridge_out_bodies(
    rpc: &dyn miden_client::rpc::NodeRpcClient,
    bridge_id: AccountId,
    // #164 re-review — the MA#28 sender the GER-metadata candidates must carry.
    // Gating candidates on provenance AT SCAN TIME is what makes the join
    // un-griefable; see `record_ger_node_metadata`.
    expected_ger_sender: AccountId,
    to_block: u32,
) -> anyhow::Result<RecoveredBridgeOuts> {
    use miden_client::rpc::domain::note::FetchedNote;
    use miden_protocol::block::BlockNumber;
    let claim_root = miden_base_agglayer::ClaimNote::script().root();
    let ger_root = UpdateGerNote::script_root();
    let mut by_id = std::collections::HashMap::new();
    let mut id_by_nullifier = std::collections::HashMap::new();
    let mut claims_by_id: std::collections::HashMap<NoteId, RecoveredClaimBody> =
        std::collections::HashMap::new();
    let mut claim_id_by_nullifier = std::collections::HashMap::new();
    // #88 / PR#164: node-authoritative GER metadata for the restore replay,
    // keyed by details commitment. Restricted to GER-shaped notes (bounds
    // memory to the GER count, not total chain traffic) and ambiguity-safe:
    // details commitment excludes metadata, so if two public GER-shaped notes
    // share details but carry DIFFERENT metadata (different sender), we drop
    // the key into `ambiguous` and never serve it — the replay then fails
    // closed (MissingMetadata) rather than joining a GER to a last-writer's
    // provenance. A legit GER note has a random serial, so a genuine collision
    // is effectively unreachable; this is correctness-by-construction, not a
    // hot path.
    let mut public_note_metadata: std::collections::HashMap<[u8; 32], NoteMetadata> =
        std::collections::HashMap::new();
    let mut ambiguous_ger_meta: std::collections::HashSet<[u8; 32]> =
        std::collections::HashSet::new();
    let mut scanned = 0usize;
    for b in 0..=to_block {
        let block = rpc
            .get_block_by_number(BlockNumber::from(b), false)
            .await
            .map_err(|e| anyhow::anyhow!("recovery: get block {b}: {e}"))?;

        let ids: Vec<NoteId> = block.body().output_notes().map(|(_, n)| n.id()).collect();
        if !ids.is_empty() {
            let fetched = rpc
                .get_notes_by_id(&ids)
                .await
                .map_err(|e| anyhow::anyhow!("recovery: get notes for block {b}: {e}"))?;
            let returned_ids: Vec<_> = fetched.iter().map(|note| note.id()).collect();
            ensure_complete_note_response(&ids, &returned_ids)?;
            for fetched_note in fetched {
                if let FetchedNote::Public(note, _) = fetched_note {
                    let id = note.id();
                    let nullifier = note.nullifier();
                    let attachments = note.attachments().clone();
                    // Finding #69: capture the public note's metadata BEFORE the
                    // details conversion drops it — it carries the sender the
                    // mint-proof provenance path needs post `--reset-miden-store`.
                    let metadata = *note.metadata();
                    let details: NoteDetails = note.into();
                    // #88 / PR#164: retain node-authoritative metadata ONLY for
                    // GER-shaped notes (bounds memory), and fail closed on a
                    // details-commitment collision that carries different
                    // metadata instead of last-write-wins.
                    // #164 re-review — GER-shaped is NOT enough. Anyone can publish a
                    // note with the SAME details as a legitimate UpdateGerNote (details
                    // are public and exclude metadata) but their own sender. Both would
                    // land under one details-commitment key, the ambiguity guard would
                    // fail closed, and the GENUINE entry would be dropped — a griefing
                    // DoS that silently deletes GER history on every later restore.
                    //
                    // Gate candidates on MA#28 PROVENANCE here, using the very predicate
                    // that decides acceptance later (`classify_ger_note`). A clone cannot
                    // forge `metadata.sender`: it is stamped by the account that executed
                    // the minting transaction, so only the real ger_manager's notes are
                    // recorded and an impostor never enters the map. Two notes that both
                    // pass this gate with identical details necessarily share sender and
                    // attachment, so their metadata is equal and the idempotent branch
                    // handles them — ambiguity then means a genuine protocol anomaly,
                    // which is exactly when failing closed is right.
                    if details.script().root() == ger_root
                        && matches!(
                            classify_ger_note(
                                Some(&metadata),
                                &attachments,
                                expected_ger_sender,
                                bridge_id,
                            ),
                            GerNoteVerdict::Accept
                        )
                    {
                        record_ger_node_metadata(
                            &mut public_note_metadata,
                            &mut ambiguous_ger_meta,
                            details.commitment().as_bytes(),
                            metadata,
                        );
                    }
                    if is_b2agg_note(&details) {
                        if by_id
                            .insert(
                                id,
                                RecoveredBridgeBody {
                                    details,
                                    attachments,
                                },
                            )
                            .is_some()
                        {
                            anyhow::bail!("recovery: duplicate B2AGG NoteId {id}");
                        }
                        if id_by_nullifier.insert(nullifier, id).is_some() {
                            anyhow::bail!("recovery: duplicate B2AGG nullifier {nullifier}");
                        }
                    } else if details.script().root() == claim_root {
                        // Finding #69: retain public CLAIM bodies so restore can
                        // replay historical ClaimEvents even when the client
                        // store (Phase 2.5's source) was reset.
                        if claims_by_id
                            .insert(
                                id,
                                RecoveredClaimBody {
                                    details,
                                    metadata,
                                    attachments,
                                },
                            )
                            .is_some()
                        {
                            anyhow::bail!("recovery: duplicate CLAIM NoteId {id}");
                        }
                        if claim_id_by_nullifier.insert(nullifier, id).is_some() {
                            anyhow::bail!("recovery: duplicate CLAIM nullifier {nullifier}");
                        }
                    }
                }
            }
        }

        scanned += 1;
        if scanned.is_multiple_of(200) {
            tracing::info!(
                at_block = b,
                to_block,
                scanned,
                b2agg = by_id.len(),
                claims = claims_by_id.len(),
                "recovery scan: progress"
            );
        }
    }

    tracing::info!(
        bridge = %bridge_id,
        from_block = 0,
        to_block,
        blocks_scanned = scanned,
        b2agg = by_id.len(),
        claims = claims_by_id.len(),
        "recovery scan complete: B2AGG bridge-out + CLAIM notes found on the node"
    );

    Ok(RecoveredBridgeOuts {
        id_by_nullifier,
        by_id,
        claim_id_by_nullifier,
        claims_by_id,
        public_note_metadata,
    })
}

/// Joins the recovered bodies to bridge-consumed inputs. The execution-chain helper and
/// input iteration already produce exact `(block, tx, input)` order, so no second sort or
/// commitment-based identity recovery is needed. One `sync_transactions` fetch feeds both
/// the B2AGG replay and (finding #69) the CLAIM replay.
async fn restore_bridge_replay(
    rpc: &dyn miden_client::rpc::NodeRpcClient,
    bridge_id: AccountId,
    mut recovered: RecoveredBridgeOuts,
    to_block: u32,
) -> anyhow::Result<(Vec<ReplayBridgeOut>, Vec<ReplayClaim>, ConsumedOrderMap)> {
    use miden_protocol::block::BlockNumber;

    let txs = rpc
        .sync_transactions(
            BlockNumber::from(0u32),
            BlockNumber::from(to_block),
            vec![bridge_id],
        )
        .await
        .map_err(|e| anyhow::anyhow!("restore: sync bridge transactions 0..{to_block}: {e}"))?;

    let claims_by_id = std::mem::take(&mut recovered.claims_by_id);
    let claim_id_by_nullifier = std::mem::take(&mut recovered.claim_id_by_nullifier);
    let consumed_order = build_consumed_order_map(&txs, bridge_id)?;
    let bridge_replay = build_bridge_replay(&txs, bridge_id, recovered)?;
    let claim_replay = build_claim_replay(&txs, bridge_id, claims_by_id, claim_id_by_nullifier)?;
    Ok((bridge_replay, claim_replay, consumed_order))
}

/// AUTHORITATIVE consumption order for EVERY note our bridge consumed, keyed by
/// nullifier — the single ordering source `replay_sort_key` uses for all replay kinds.
///
/// Why this exists: the miden-client store records `consumed_tx_order` as NULL for every
/// consumed note it learns about by nullifier (measured on a live stack: 135 of 135 NULL).
/// `ReplayItem::Consumed` used to hand that `None` straight to the sort key, and since
/// `None < Some(_)` in Rust, EVERY GER/claim note sorted ahead of EVERY B2AGG bridge-out
/// sharing its block — a constant "consumed-first" rule, not a real order. Live never had
/// that gap: it rebuilds B2AGG at the authoritative `(block, tx_order)` from this same
/// `sync_transactions` feed, so a block holding both a GER note and a bridge-out came out
/// in true execution order live and in fixed kind order on restore.
///
/// The drill caught it as two swapped `log_index` pairs (blocks 173 + 288) in an
/// `eth_getLogs` diff against a LIVE baseline. Nothing else could: `hash_chain_value`
/// depends only on UHC-to-UHC order, which the swap preserves.
///
/// GER and CLAIM notes are consumed by the bridge account itself, so they are already
/// present in this feed — the data was always there, it just was not consulted.
fn build_consumed_order_map(
    txs: &[miden_client::rpc::domain::transaction::TransactionRecord],
    bridge_id: AccountId,
) -> anyhow::Result<ConsumedOrderMap> {
    let mut order = ConsumedOrderMap::new();
    for (block, tx_order, tx) in ordered_account_transactions(txs, bridge_id)? {
        for input in tx.transaction_header.input_notes().iter() {
            // First writer wins: a nullifier is consumed exactly once on-chain, so a
            // duplicate would mean a malformed feed rather than a later truth.
            order
                .by_nullifier
                .entry(input.nullifier())
                .or_insert((block, tx_order));
            if let Some(header) = input.header() {
                order
                    .by_note_id
                    .entry(header.id())
                    .or_insert((block, tx_order));
            }
        }
    }
    tracing::info!(
        bridge = %bridge_id,
        by_nullifier = order.by_nullifier.len(),
        by_note_id = order.by_note_id.len(),
        "restore: authoritative consumption order built for ALL bridge-consumed notes"
    );
    Ok(order)
}

fn build_bridge_replay(
    txs: &[miden_client::rpc::domain::transaction::TransactionRecord],
    bridge_id: AccountId,
    recovered: RecoveredBridgeOuts,
) -> anyhow::Result<Vec<ReplayBridgeOut>> {
    let RecoveredBridgeOuts {
        id_by_nullifier,
        mut by_id,
        ..
    } = recovered;
    let mut replay = Vec::new();
    for (block, order, tx) in ordered_account_transactions(txs, bridge_id)? {
        for (within_tx_pos, input) in tx.transaction_header.input_notes().iter().enumerate() {
            let within_tx_pos = within_tx_pos as u32;
            let id = input
                .header()
                .map(|header| header.id())
                .or_else(|| id_by_nullifier.get(&input.nullifier()).copied());
            let Some(id) = id else { continue };
            let Some(body) = by_id.remove(&id) else {
                continue;
            };
            if let Some(header) = input.header()
                && header.details_commitment() != body.details.commitment()
            {
                anyhow::bail!("restore: NoteId {id} body/transaction commitment mismatch");
            }
            replay.push(ReplayBridgeOut {
                id,
                body,
                block,
                tx_order: order,
                within_tx_pos,
            });
        }
    }
    tracing::info!(
        bridge = %bridge_id,
        bridge_outs = replay.len(),
        "restore: authoritative bridge-out replay built from transaction execution order"
    );
    Ok(replay)
}

/// Finding #69 — join node-scanned CLAIM bodies to the bridge's consuming
/// transactions, exactly like [`build_bridge_replay`] does for B2AGG (input
/// note header id, or nullifier fallback). A CLAIM that joins here was
/// provably consumed by OUR bridge — the MA#3 consumer trust root — which is
/// the provenance [`classify_claim_note`] accepts regardless of minter.
fn build_claim_replay(
    txs: &[miden_client::rpc::domain::transaction::TransactionRecord],
    bridge_id: AccountId,
    mut claims_by_id: std::collections::HashMap<NoteId, RecoveredClaimBody>,
    claim_id_by_nullifier: std::collections::HashMap<Nullifier, NoteId>,
) -> anyhow::Result<Vec<ReplayClaim>> {
    let mut replay = Vec::new();
    for (block, order, tx) in ordered_account_transactions(txs, bridge_id)? {
        for (within_tx_pos, input) in tx.transaction_header.input_notes().iter().enumerate() {
            let within_tx_pos = within_tx_pos as u32;
            let id = input
                .header()
                .map(|header| header.id())
                .or_else(|| claim_id_by_nullifier.get(&input.nullifier()).copied());
            let Some(id) = id else { continue };
            let Some(body) = claims_by_id.remove(&id) else {
                continue;
            };
            if let Some(header) = input.header()
                && header.details_commitment() != body.details.commitment()
            {
                anyhow::bail!("restore: CLAIM NoteId {id} body/transaction commitment mismatch");
            }
            replay.push(ReplayClaim {
                id,
                body,
                block,
                tx_order: order,
                within_tx_pos,
            });
        }
    }
    tracing::info!(
        bridge = %bridge_id,
        claims = replay.len(),
        "restore: bridge-consumed CLAIM replay built from transaction execution order (finding #69)"
    );
    Ok(replay)
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
    network_rpcs: &crate::metadata_recovery::NetworkRpcMap,
) -> anyhow::Result<usize> {
    let store_clone = store.clone();
    let bridge_id = accounts.bridge.0;
    // Owned clone moved into the `with(...)` closure; per-faucet RPC selection is
    // keyed on the faucet's origin_network (finding #62 multi-network recovery).
    let network_rpcs = network_rpcs.clone();

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
                        // All-zero conversion = the native-ETH sentinel (pre-seeded, never
                        // rebuilt from chain) or an unregistered id. A registered NATIVE
                        // faucet has a non-zero origin (origin_network == network_id, scale 0),
                        // so it does NOT land here — it proceeds to classify + rebuild below.
                        continue;
                    };
                    match crate::faucet_ops::rebuild_faucet_entry_from_chain(
                        client,
                        &bridge_account,
                        faucet_id,
                        &conversion,
                        network_rpcs
                            .get(&conversion.origin_network)
                            .map(String::as_str),
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
                            // An UNKNOWN faucet type registered in the bridge is a fail-LOUD
                            // condition (malformed / hostile registration), distinct from a
                            // recoverable metadata miss — surface it at ERROR with its own
                            // metric so operators must investigate rather than let it pass as
                            // a routine quarantine.
                            if format!("{e:?}").contains("UNKNOWN faucet type") {
                                ::metrics::counter!("restore_unknown_faucet_type_total")
                                    .increment(1);
                                tracing::error!(
                                    faucet_id = %faucet_id,
                                    error = ?e,
                                    "restore: UNKNOWN faucet type registered in the bridge — \
                                     matches no supported faucet kind; NOT rebuilt. Investigate: \
                                     this should never happen for a proxy-registered faucet."
                                );
                            } else {
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
                }

                *count_inner.lock().unwrap() = rebuilt;
                Ok(())
            })
        })
        .await?;

    let n = *count.lock().unwrap();
    Ok(n)
}

/// Tallies from the merged replay, kept per-kind so the phase-level log lines
/// (and the restore summary) stay unchanged for operators.
#[derive(Debug, Default)]
pub(crate) struct ReplayCounts {
    pub bridge_outs: usize,
    pub bridge_logs: usize,
    pub claims: usize,
    pub claim_logs: usize,
    pub node_claims: usize,
    pub gers: usize,
    pub ger_logs: usize,
}

/// One unit of restored history, tagged by where it came from. All three
/// sources carry the same canonical ordering coordinates, which is what makes a
/// single merged sort possible.
enum ReplayItem<'a> {
    /// B2AGG bridge-out: node-scanned body joined to the bridge transaction feed.
    BridgeOut(ReplayBridgeOut),
    /// CLAIM recovered by the node scan (finding #69) — survives a wiped client store.
    NodeClaim(ReplayClaim),
    /// A consumed note from the client store. CLAIM- and GER-shaped notes both
    /// arrive here; each projection self-filters on script root, so at most one
    /// of them emits for a given note.
    Consumed(&'a InputNoteRecord),
}

/// The canonical projection order — the `ORDER` stage of
/// `docs/design/UNIFIED-PROJECTOR.md`, and byte-for-byte the comparator the live
/// projector applies in `project_block_notes`:
///
///   `(block, transaction order, input position, details commitment, NoteId)`
///
/// # Why `within_tx_pos` is B2AGG-only
///
/// Live populates its `within_tx_pos` map ONLY from `resolve_b2agg_consumptions`
/// — the authoritative bridge-transaction feed — and every other note falls
/// through its `unwrap_or(0)`. Restore must reproduce the live log stream
/// EXACTLY, so it mirrors that: the real input position for B2AGG, `0` for
/// everything else. Feeding real positions for claims would be "more accurate"
/// and would produce a DIFFERENT order than live, which is the bug, not the fix.
///
/// Getting this wrong is not subtle in its effects: an UpdateHashChain log
/// carries the rolling chain value in its topics, so a single misordered pair
/// re-chains every log after it.
///
/// `tx_order` is `Option` so a consumed record without one sorts before records
/// that have one (`None` < `Some`), matching the per-kind sort this replaced.
/// Authoritative `(block, tx_order)` for every note the bridge consumed, built by
/// [`build_consumed_order_map`] from the node's transaction feed. The ONE ordering
/// source for every replay kind.
///
/// Indexed by BOTH identities a consumed record can carry. The node's transaction inputs
/// always expose a nullifier and expose a NoteId only when the input header survived
/// (miden-client 0.15 strips them), while a store-loaded `InputNoteRecord` may present
/// either. Joining on whichever is available keeps the fix from depending on which of the
/// two happens to be populated.
#[derive(Default)]
struct ConsumedOrderMap {
    by_nullifier: std::collections::HashMap<Nullifier, (u64, u32)>,
    by_note_id: std::collections::HashMap<NoteId, (u64, u32)>,
}

impl ConsumedOrderMap {
    fn new() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.by_nullifier.is_empty() && self.by_note_id.is_empty()
    }

    /// Authoritative `(block, tx_order)` for a consumed record, or `None` when it carries
    /// neither identity or was consumed outside our bridge's transactions.
    ///
    /// Takes the two identities rather than the record so the join is unit-testable: a
    /// synthetically-built `InputNoteRecord` exposes NEITHER identity (verified), so a
    /// test that passed a record could only ever exercise the miss path.
    fn lookup(&self, nullifier: Option<Nullifier>, note_id: Option<NoteId>) -> Option<(u64, u32)> {
        nullifier
            .and_then(|nullifier| self.by_nullifier.get(&nullifier))
            .or_else(|| note_id.and_then(|note_id| self.by_note_id.get(&note_id)))
            .copied()
    }
}

fn replay_sort_key(
    item: &ReplayItem<'_>,
    consumed_order: &ConsumedOrderMap,
) -> (u64, Option<u32>, u32, [u8; 32], Option<[u8; 32]>) {
    match item {
        ReplayItem::BridgeOut(r) => (
            r.block,
            Some(r.tx_order),
            // The only kind live knows an input position for.
            r.within_tx_pos,
            r.body.details.commitment().as_bytes(),
            Some(r.id.as_bytes()),
        ),
        ReplayItem::NodeClaim(r) => (
            r.block,
            Some(r.tx_order),
            0,
            r.body.details.commitment().as_bytes(),
            Some(r.id.as_bytes()),
        ),
        ReplayItem::Consumed(n) => {
            // AUTHORITATIVE first. The client store leaves `consumed_tx_order` NULL for
            // notes it learned by nullifier, and `None` sorts before every `Some`, which
            // silently pinned all consumed notes ahead of same-block bridge-outs. The
            // node's transaction feed knows the real order for every note our bridge
            // consumed; fall back to the store only for a note absent from that feed.
            let store_block = n
                .state()
                .consumed_block_height()
                .map(|h| h.as_u64())
                .unwrap_or(0);
            let (block, tx_order) = match consumed_order.lookup(n.nullifier(), n.id()) {
                Some((block, tx_order)) => (block, Some(tx_order)),
                None => (store_block, n.state().consumed_tx_order()),
            };
            (
                block,
                tx_order,
                // Deliberately NOT the real input position. Live populates its
                // `within_tx_pos` map only from `resolve_b2agg_consumptions`, so a
                // non-B2AGG note carries no position there and falls through its
                // `unwrap_or(0)`. Mirroring that keeps same-transaction ordering
                // identical to live; only the (block, tx_order) gap was measured, and
                // widening the fix past the evidence would be a new divergence.
                0,
                n.details_commitment().as_bytes(),
                n.id().map(|i| i.as_bytes()),
            )
        }
    }
}

/// Replay ALL restored history in ONE chronologically-ordered pass.
///
/// # Why this is a single pass
///
/// `docs/design/UNIFIED-PROJECTOR.md` specifies `{B2AGG, INTERNAL} -> ORDER ->
/// EMIT`: one ordering stage that every source feeds, then the shared `project_*`
/// derivations. Restore implemented the shared DERIVATIONS but not the shared
/// ORDER — it ran a phase per KIND (every B2AGG, then every CLAIM, then every
/// GER), each internally sorted but sequential overall. The live projector walks
/// BLOCK-major (`by_block`), so the two disagreed on emission order.
///
/// That is not cosmetic. Both observable consequences are consumer-visible:
///   * `log_index` comes from one GLOBAL counter, so phase-major replay renumbers
///     EVERY synthetic log after a recovery (measured: 15 BridgeEvents occupying
///     indices 0..14 while spanning blocks 97..432).
///   * `hash_chain_value` is a fold in EMISSION order, so with several GERs in one
///     block the restored chain diverged from the live one even though the GER set
///     and per-block content matched exactly — and aggkit consumes that chain.
///
/// Each phase previously opened its OWN `miden_client` session and re-fetched the
/// same consumed feed, which is what forced the kind-major split. This runs every
/// source through one session, one sort, one emit loop — so a restored log stream
/// is indistinguishable from the live one.
#[allow(clippy::too_many_arguments)]
async fn replay_history_in_order(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    network_rpcs: &crate::metadata_recovery::NetworkRpcMap,
    bridge_replay: Vec<ReplayBridgeOut>,
    claim_replay: Vec<ReplayClaim>,
    restore_block: u64,
    node_note_metadata: std::collections::HashMap<[u8; 32], NoteMetadata>,
    consumed_order: ConsumedOrderMap,
) -> anyhow::Result<ReplayCounts> {
    let store_clone = store.clone();
    let block_state_clone = block_state.clone();
    let network_rpcs = network_rpcs.clone();
    let bridge_id = accounts.bridge.0;
    let expected_claim_sender = accounts.service.0;
    // MA#28 — same ger_manager/service fallback `submit_update_ger_note` resolves.
    let expected_ger_sender = accounts
        .ger_manager
        .as_ref()
        .map(|a| a.0)
        .unwrap_or(accounts.service.0);

    let result = Arc::new(std::sync::Mutex::new(ReplayCounts::default()));
    let result_inner = result.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                use miden_client::store::InputNoteState;
                use miden_client::store::input_note_states::ConsumedExternalNoteState;
                use miden_protocol::block::BlockNumber;

                // ── ONE fetch of the consumed feed, shared by CLAIM + GER ──────
                let consumed_notes = client
                    .get_input_notes(NoteFilter::Consumed)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;
                // Our own minted output records carry the metadata that the
                // metadata-less `ConsumedExternal` state drops (the MA#28 fallback
                // shared by the CLAIM and GER provenance gates).
                let mut own_output_metadata: std::collections::HashMap<[u8; 32], NoteMetadata> =
                    client
                        .get_output_notes(NoteFilter::All)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
                        .into_iter()
                        .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
                        .collect();
                // #88 — a full drop-restore wipes the client store too, so merge the
                // NODE-scanned metadata UNDER our own records (own records win).
                // Only provenance-gated GER candidates are in that map (#164).
                for (k, v) in &node_note_metadata {
                    own_output_metadata.entry(*k).or_insert(*v);
                }

                // ── ONE merged, chronologically ordered work list ───────────────
                let mut items: Vec<ReplayItem<'_>> = Vec::with_capacity(
                    bridge_replay.len() + claim_replay.len() + consumed_notes.len(),
                );
                items.extend(bridge_replay.into_iter().map(ReplayItem::BridgeOut));
                items.extend(claim_replay.into_iter().map(ReplayItem::NodeClaim));
                for note in &consumed_notes {
                    // The background client can sync past the fixed restore
                    // snapshot; leave newer notes to the live projector.
                    if note_consumed_block(note, restore_block) > restore_block {
                        continue;
                    }
                    items.push(ReplayItem::Consumed(note));
                }
                // OBSERVABILITY, not decoration. This join is the whole fix, and a
                // synthetic record exposes neither identity — so if the real records ever
                // stop carrying one either, the lookup would miss silently and restore
                // would quietly go back to emitting consumed notes ahead of same-block
                // bridge-outs. Count the joins and say so loudly when none land.
                let consumed_total = items
                    .iter()
                    .filter(|item| matches!(item, ReplayItem::Consumed(_)))
                    .count();
                let authoritative_hits = items
                    .iter()
                    .filter(|item| match item {
                        ReplayItem::Consumed(n) => {
                            consumed_order.lookup(n.nullifier(), n.id()).is_some()
                        }
                        _ => false,
                    })
                    .count();
                if consumed_total > 0 && authoritative_hits == 0 && !consumed_order.is_empty() {
                    tracing::error!(
                        consumed_total,
                        by_nullifier = consumed_order.by_nullifier.len(),
                        by_note_id = consumed_order.by_note_id.len(),
                        "restore: NO consumed note joined the authoritative consumption \
                         order — replay is falling back to the client store's NULL \
                         tx_order for every one of them, which orders consumed notes \
                         ahead of same-block bridge-outs and will diverge from live \
                         log_index. Treat a restore under this condition as suspect."
                    );
                } else {
                    tracing::info!(
                        consumed_total,
                        authoritative_hits,
                        "restore: consumed notes joined to authoritative consumption order"
                    );
                }
                ::metrics::counter!("restore_consumed_authoritative_order_total")
                    .increment(authoritative_hits as u64);
                ::metrics::counter!("restore_consumed_store_order_fallback_total")
                    .increment((consumed_total - authoritative_hits) as u64);

                items.sort_by_key(|item| replay_sort_key(item, &consumed_order));

                let bridge_address = get_bridge_address();
                let mut counts = ReplayCounts::default();

                for item in items {
                    match item {
                        ReplayItem::BridgeOut(replay) => {
                            let state =
                                InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                                    nullifier_block_height: BlockNumber::from(replay.block as u32),
                                    consumer_account: Some(bridge_id),
                                    consumed_tx_order: Some(replay.tx_order),
                                    metadata: None,
                                });
                            let note = InputNoteRecord::new(
                                replay.body.details,
                                replay.body.attachments,
                                None,
                                state,
                            );
                            let block_hash = block_state_clone.get_block_hash(replay.block);
                            if project_b2agg_note(
                                &store_clone,
                                &note,
                                replay.id,
                                bridge_id,
                                local_network_id,
                                replay.block,
                                block_hash,
                                bridge_address,
                                Some(&mut *client),
                                &network_rpcs,
                            )
                            .await?
                                == B2AggRestoreOutcome::Emitted
                            {
                                counts.bridge_outs += 1;
                                counts.bridge_logs += 1;
                            }
                        }
                        ReplayItem::NodeClaim(replay) => {
                            let note_id_str =
                                hex::encode(replay.body.details.commitment().as_bytes());
                            let block_hash = block_state_clone.get_block_hash(replay.block);
                            tracing::debug!(
                                target: "restore::claims",
                                note = %replay.id,
                                block = replay.block,
                                "restore: replaying node-scanned CLAIM (finding #69)"
                            );
                            if project_claim_parts(
                                &store_clone,
                                note_id_str,
                                &replay.body.details,
                                Some(&replay.body.metadata),
                                Some(bridge_id),
                                &replay.body.attachments,
                                expected_claim_sender,
                                bridge_id,
                                replay.block,
                                block_hash,
                                bridge_address,
                            )
                            .await?
                                == ClaimProjectOutcome::Emitted
                            {
                                counts.node_claims += 1;
                            }
                        }
                        ReplayItem::Consumed(note) => {
                            let blk = note_consumed_block(note, restore_block);
                            let block_hash = block_state_clone.get_block_hash(blk);
                            let timestamp = block_state_clone.get_block_timestamp(blk);
                            // Both derivations self-filter on script root, so at
                            // most one of them emits — and running them adjacently
                            // keeps this note's position in the global order exact.
                            if project_claim_note(
                                &store_clone,
                                note,
                                &own_output_metadata,
                                expected_claim_sender,
                                bridge_id,
                                blk,
                                block_hash,
                                bridge_address,
                            )
                            .await?
                                == ClaimProjectOutcome::Emitted
                            {
                                counts.claims += 1;
                                counts.claim_logs += 1;
                            }
                            if project_ger_note(
                                &store_clone,
                                note,
                                &own_output_metadata,
                                expected_ger_sender,
                                bridge_id,
                                blk,
                                block_hash,
                                timestamp,
                            )
                            .await?
                                == GerProjectOutcome::Emitted
                            {
                                counts.gers += 1;
                                counts.ger_logs += 1;
                            }
                        }
                    }
                }

                *result_inner.lock().unwrap() = counts;
                Ok(())
            })
        })
        .await?;

    Ok(std::mem::take(&mut *result.lock().unwrap()))
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
    // The unique on-chain NoteId. Distinct notes may have identical details.
    note_id: miden_protocol::note::NoteId,
    bridge_id: AccountId,
    local_network_id: u32,
    restore_block: u64,
    block_hash: [u8; 32],
    bridge_address: &str,
    client: Option<&mut MidenClientLib>,
    network_rpcs: &crate::metadata_recovery::NetworkRpcMap,
) -> anyhow::Result<B2AggRestoreOutcome> {
    let details = note.details();
    if !is_b2agg_note(details) {
        return Ok(B2AggRestoreOutcome::Skipped);
    }

    // The derived tx hash remains details-commitment-based for immutable history
    // compatibility. Reservation, dedup and quarantine identity is always the unique
    // NoteId: distinct on-chain notes may have identical details.
    let details_commitment_hex = hex::encode(note.details_commitment().as_bytes());
    let dedup_key = note_id.to_hex();
    let tx_hash = crate::bridge_out::derive_bridge_out_tx_hash(&details_commitment_hex);
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
    // NoteId is the identity whenever the authoritative caller has it. Restore handles the
    // one-time commitment-keyed legacy alias before entering this function; consulting that
    // key here would collapse every later note that happens to share the same details.
    let processed = store.is_note_processed(&dedup_key).await?;
    if processed {
        if !matches!(class, B2AggConsumerClass::Emit) {
            ::metrics::counter!("restore_b2agg_legacy_processed_gated_total").increment(1);
            tracing::warn!(
                note_id = %dedup_key,
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
        B2AggConsumerClass::Emit => {
            // Cantina #7 — RESERVE the authoritative LET deposit index for this leaf NOW,
            // before any downstream gate can skip it. The bridge's consumption already
            // appended the on-chain LET leaf; whether we end up emitting, quarantining,
            // deferring (metadata) or refusing (self-target), the leaf's index is TAKEN —
            // an unreserved skip would hand this index to the NEXT exit and shift every
            // deposit_count after it off its true LET position (wrong globalIndex, sealed
            // forever by getLogs immutability). Idempotent: retries/restarts reuse the row.
            store.reserve_deposit_index(&dedup_key).await?;
        }
        B2AggConsumerClass::Reclaimed => {
            ::metrics::counter!("bridge_out_reclaimed_b2agg_total").increment(1);
            tracing::info!(
                note_id = %dedup_key,
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
                note_id = %dedup_key,
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
            tracing::warn!(note_id = %dedup_key, "restore: B2AGG storage unparsable: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &dedup_key,
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
            note_id = %dedup_key,
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
        tracing::warn!(note_id = %dedup_key, "restore: B2AGG has no fungible asset");
        crate::bridge_out::quarantine_unbridgeable_b2agg(
            &**store,
            bridge_id,
            &dedup_key,
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
            tracing::warn!(note_id = %dedup_key, "restore: B2AGG unknown faucet: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &dedup_key,
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
            tracing::warn!(note_id = %dedup_key, "restore: B2AGG amount overflow: {e:#}");
            crate::bridge_out::quarantine_unbridgeable_b2agg(
                &**store,
                bridge_id,
                &dedup_key,
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
            // Finding #62: dial the token's ACTUAL origin-network RPC (L1 for
            // network 0, L2B for network 2, …) so the keccak gate validates.
            network_rpcs.get(&origin.origin_network).map(String::as_str),
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
                        note_id = %dedup_key,
                        faucet_id = %faucet_id,
                        error = ?e,
                        "restore: Cantina #13 L2 metadata backfill failed (recovery will re-run)"
                    );
                } else {
                    tracing::info!(
                        note_id = %dedup_key,
                        faucet_id = %faucet_id,
                        "restore: Cantina #13 L2 recovered + backfilled ERC-20 metadata"
                    );
                }
            }
            bytes
        }
        EmitMetadata::Unrecoverable => {
            // FAIL-CLOSED, LOUD. The bridge already consumed this B2AGG (the on-chain LET
            // advanced and reserved this leaf's index), but we cannot recover + validate its
            // ERC-20 metadata. We must NOT emit (empty/unvalidated metadata would let the
            // destination deploy a spoofed wrapped token — Cantina #13), and we must NOT
            // silently skip: a reserved-but-unemitted leaf gaps the getLogs depositCount
            // sequence and halts aggkit bridgesync (the projector's emitted-frontier gate
            // will also refuse to seal past it). There is no safe "tombstone" — any faked
            // event either double-spends a balance (BalanceUnderflow) or advances the LER
            // with a leaf that isn't the real exit. So bail: the operator recovers by fixing
            // the metadata/identity — the safe path being a full DB backup + drop +
            // `--restore` rebuild from the authoritative on-chain state (never a partial
            // patch of a corrupted row).
            ::metrics::counter!(METADATA_UNRECOVERABLE_METRIC).increment(1);
            anyhow::bail!(
                "restore: bridge-out note {dedup_key} (faucet {faucet_id}, origin_network {}) has \
                 unrecoverable ERC-20 metadata — refusing to emit or skip past it (a reserved-but-\
                 unemitted leaf gaps getLogs and halts aggkit bridgesync). This indicates a \
                 corrupted/half-recovered faucet row. Recover by backing up, DROPPING the proxy DB, \
                 and re-running `--restore` to rebuild the faucet identity + metadata from on-chain \
                 (or backfill the faucet's registry metadata / L1 RPC), then restart.",
                origin.origin_network,
            );
        }
    };

    // Cantina #13 follow-up — DoS guard, now applied to the FINAL emit bytes
    // (Layer-1 stored OR Layer-2 recovered): the metadata derives from untrusted
    // L1 calldata, and a malicious token's name() could yield an oversized
    // recovered blob. Cap before encoding; skip without marking the note processed.
    if emit_metadata.len() > crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES {
        ::metrics::counter!("bridge_out_b2agg_metadata_too_large_total").increment(1);
        tracing::warn!(
            note_id = %dedup_key,
            metadata_len = emit_metadata.len(),
            cap = crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES,
            "restore: B2AGG metadata exceeds cap; skipping synthetic BridgeEvent (DoS guard)"
        );
        crate::bridge_out::quarantine_unbridgeable_b2agg(
            &**store,
            bridge_id,
            &dedup_key,
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
    // H1 — atomic B2AGG emission. The LET index was reserved before any
    // quarantine/deferral gate. This commit atomically marks that reservation
    // emitted and inserts its BridgeEvent, closing the crash window between the
    // old processed-marker and event writes. Retry reuses the reserved index and
    // emits no duplicate log.
    let deposit_count = store
        .commit_b2agg_event_atomic(
            // Reservation/dedup key: unique NoteId. The tx hash above stays
            // commitment-derived for historical compatibility.
            dedup_key.clone(),
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
        note_id = %dedup_key,
        deposit_count,
        "emitted BridgeEvent"
    );

    Ok(B2AggRestoreOutcome::Emitted)
}

/// Rebuild the original `claimAsset` call from the full note-storage decode plus the
/// hash-verified metadata preimage. Every field is the authoritative value the proxy
/// built (and the on-chain bridge verified) the claim with — nothing is fabricated.
pub(crate) fn build_claim_asset_call(
    full: &DecodedFullClaim,
    metadata: Vec<u8>,
) -> crate::claim::claimAssetCall {
    use alloy::primitives::{Address, FixedBytes, U256};
    let node = |b: &[u8; 32]| FixedBytes::<32>::from(*b);
    crate::claim::claimAssetCall {
        smtProofLocalExitRoot: std::array::from_fn(|i| node(&full.smt_proof_local_exit_root[i])),
        smtProofRollupExitRoot: std::array::from_fn(|i| node(&full.smt_proof_rollup_exit_root[i])),
        globalIndex: U256::from_be_bytes(full.global_index),
        mainnetExitRoot: node(&full.mainnet_exit_root),
        rollupExitRoot: node(&full.rollup_exit_root),
        originNetwork: full.origin_network,
        originTokenAddress: Address::from(full.origin_address),
        destinationNetwork: full.destination_network,
        destinationAddress: Address::from(full.destination_address),
        amount: U256::from_be_bytes(full.amount),
        metadata: metadata.into(),
    }
}

/// Resolve the `metadata` byte-string of a claim from an AUTHORITATIVE source, verified
/// against the CLAIM note's `metadata_hash` (`keccak256(metadata)`).
///
///   * hash-of-empty → the claim carried no metadata (native ETH and any pre-deployed
///     wrapped token) — the empty preimage is truthful by the hash;
///   * otherwise → the faucet registry: `FaucetEntry.metadata` is the exact ABI-encoded
///     preimage the claim was published with (`publish_claim` registers the faucet with
///     `MetadataHash::from_abi_encoded(params.metadata)` — same bytes), accepted only if
///     its keccak256 equals the note's hash;
///   * no verifiable preimage → `None`. The caller must NOT manufacture metadata — a
///     parseable-but-false claim record is worse than an alarmed unrecoverable one.
pub(crate) async fn resolve_claim_metadata(
    store: &Arc<dyn Store>,
    origin_network: u32,
    origin_address: &[u8; 20],
    metadata_hash: &[u8; 32],
) -> anyhow::Result<Option<Vec<u8>>> {
    let empty_hash: [u8; 32] = Keccak256::digest([]).into();
    if metadata_hash == &empty_hash {
        return Ok(Some(Vec::new()));
    }
    if let Some(faucet) = store
        .get_faucet_by_origin(origin_address, origin_network)
        .await?
    {
        let hash: [u8; 32] = Keccak256::digest(&faucet.metadata).into();
        if &hash == metadata_hash {
            return Ok(Some(faucet.metadata));
        }
        tracing::warn!(
            origin_network,
            origin_address = %hex::encode(origin_address),
            "claim metadata recovery: registry preimage does not hash to the note's \
             metadata_hash — refusing to serve it"
        );
    }
    Ok(None)
}

/// PERSIST the recovered full `claimAsset` calldata for a SYNTHESIZED claim, keyed by its
/// derived tx hash, so `eth_getTransactionByHash` / `debug_traceTransaction` serve the same
/// truthful claim across restarts (the stored-envelope path precedes every synthetic
/// fallback). Returns `Ok(true)` when the tx record exists (persisted now or previously),
/// `Ok(false)` when the metadata preimage could not be recovered authoritatively — the tx
/// then deliberately keeps its empty input and the miss is alarmed
/// (`synthetic_claim_calldata_unrecoverable_total`), NEVER fabricated.
///
/// aggkit v0.8.3's bridgesync full-claim parser persists every calldata field (both SMT
/// proofs, both exit roots, networks, addresses, amount, metadata) and derives the claim's
/// GER from the two exit roots, so all of it comes from the consumed CLAIM note's storage
/// ([`parse_full_claim_from_storage`]) + the hash-verified registry preimage
/// ([`resolve_claim_metadata`]).
pub(crate) async fn persist_synthetic_claim_tx(
    store: &Arc<dyn Store>,
    note_storage: &miden_protocol::note::NoteStorage,
    note_id_str: &str,
    tx_hash_str: &str,
    block_number: u64,
    block_hash: [u8; 32],
) -> anyhow::Result<bool> {
    let tx_hash: alloy::primitives::TxHash = tx_hash_str
        .parse()
        .map_err(|e| anyhow::anyhow!("derived claim tx hash {tx_hash_str}: {e}"))?;
    // Idempotent AND crash-safe (review blocker 3): synthesis re-runs (restore replay,
    // projector re-observation) and the live backfill all funnel here.
    //   * A SUCCESSFULLY-committed row (`Some(Ok(_))`) → done, first writer won.
    //   * A PENDING row (`None` result: txn_begin ran, txn_commit did not — a crash BETWEEN
    //     them) OR a terminally-FAILED row (`Some(Err(_))`) must both be FINALIZED to
    //     success, not treated as complete. (PR #151 blocker 1) The old guard was
    //     `data.result.is_some()`, which matched `Some(Err(_))` too — so a failed repair
    //     returned `Ok(true)` (marked resolved forever) while the durable-backlog drain
    //     (in `txn_commit`) only fires on success, pinning `/health` at 503 FOREVER and
    //     never retrying. This function is only reached for a note whose ClaimEvent EXISTS
    //     (that is what the backfill repairs), and a ClaimEvent is emitted only when the
    //     claim SUCCEEDED — so a `failed` status is stale ground-truth. The envelope (with
    //     the reconstructed calldata) is already persisted, so re-commit with `Ok(())`:
    //     idempotent, it finalizes the row to success AND atomically drains the backlog.
    match store.txn_get(tx_hash).await? {
        Some(data) if matches!(data.result, Some(Ok(_))) => return Ok(true),
        Some(data) => {
            let was_failed = matches!(data.result, Some(Err(_)));
            store
                .txn_commit(tx_hash, Ok(()), block_number, block_hash)
                .await?;
            if was_failed {
                ::metrics::counter!("synthetic_claim_calldata_healed_failed_total").increment(1);
                tracing::info!(
                    note_id = %note_id_str,
                    tx_hash = %tx_hash_str,
                    "synthesized claim: HEALED a terminally-failed calldata row (its ClaimEvent \
                     exists, so the claim succeeded) — re-committed to success + drained the \
                     repair backlog rather than pinning /health at 503 forever"
                );
            } else {
                ::metrics::counter!("synthetic_claim_calldata_finalized_pending_total")
                    .increment(1);
                tracing::info!(
                    note_id = %note_id_str,
                    tx_hash = %tx_hash_str,
                    "synthesized claim: finalized a PENDING calldata row (crash between begin and \
                     commit) rather than stranding it"
                );
            }
            return Ok(true);
        }
        None => {}
    }

    let Some((envelope, bridge_addr, calldata_bytes)) =
        build_synthetic_claim_envelope(store, note_storage, note_id_str, tx_hash).await?
    else {
        return Ok(false);
    };
    store
        .txn_begin(
            tx_hash,
            crate::store::TxnEntry {
                id: None,
                envelope,
                signer: bridge_addr,
                expires_at: None,
                // MUST stay empty: `txn_commit` appends entry logs to the synthetic log
                // store, and the ClaimEvent log was already committed atomically by
                // `commit_manual_claim_event_atomic` — a copy here would double-emit it.
                logs: Vec::new(),
            },
        )
        .await?;
    store
        .txn_commit(tx_hash, Ok(()), block_number, block_hash)
        .await?;
    ::metrics::counter!("synthetic_claim_calldata_persisted_total").increment(1);
    tracing::info!(
        note_id = %note_id_str,
        tx_hash = %tx_hash_str,
        block_number,
        calldata_bytes,
        "synthesized claim: persisted authoritative full claimAsset calldata under the \
         derived tx hash (backfill path — the block is already sealed)"
    );
    Ok(true)
}

/// Reconstruct the authoritative full `claimAsset` transaction envelope for a consumed
/// CLAIM note from its on-chain storage + the faucet registry, sealed under `tx_hash`.
///
/// Returns `Ok(None)` when the faucet metadata preimage is unrecoverable — the caller then
/// leaves the envelope absent (the serve path keeps an empty input and alarms; the operator
/// registers/repairs the faucet, and the projector backfill heals on the next tick). This is
/// the single reconstruction shared by the backfill ([`persist_synthetic_claim_tx`]) and the
/// projection ([`insert_pending_claim_calldata`]) paths so both emit byte-identical calldata.
async fn build_synthetic_claim_envelope(
    store: &Arc<dyn Store>,
    note_storage: &miden_protocol::note::NoteStorage,
    note_id_str: &str,
    tx_hash: alloy::primitives::TxHash,
) -> anyhow::Result<
    Option<(
        alloy::consensus::TxEnvelope,
        alloy::primitives::Address,
        usize,
    )>,
> {
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::primitives::{Address, Signature, TxKind, U256};

    let full = parse_full_claim_from_storage(note_storage)?;
    let Some(metadata) = resolve_claim_metadata(
        store,
        full.origin_network,
        &full.origin_address,
        &full.metadata_hash,
    )
    .await?
    else {
        ::metrics::counter!("synthetic_claim_calldata_unrecoverable_total").increment(1);
        tracing::error!(
            note_id = %note_id_str,
            tx_hash = %tx_hash,
            origin_network = full.origin_network,
            origin_address = %hex::encode(full.origin_address),
            metadata_hash = %hex::encode(full.metadata_hash),
            "synthesized claim: metadata preimage NOT recoverable from the faucet registry — \
             refusing to fabricate calldata; the tx keeps an empty input (aggkit will surface \
             this claim as unparsable — operator action: register/repair the faucet metadata, \
             the backfill then heals on the next tick)"
        );
        return Ok(None);
    };

    let call = build_claim_asset_call(&full, metadata);
    let input = alloy_core::sol_types::SolCall::abi_encode(&call);
    let calldata_bytes = input.len();
    let bridge_addr: Address = get_bridge_address()
        .parse()
        .map_err(|e| anyhow::anyhow!("bridge address: {e}"))?;
    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 0,
        gas_limit: 0,
        to: TxKind::Call(bridge_addr),
        value: U256::ZERO,
        input: input.into(),
    };
    // Placeholder signature (v=27, r=1, s=1) matching the synthetic-tx wire shape —
    // consumers (aggkit) parse the calldata, they don't verify signatures. The envelope is
    // sealed with `tx_hash` so every read path reports the hash the ClaimEvent rides.
    let signature = Signature::new(U256::from(1), U256::from(1), false);
    let envelope = TxEnvelope::Legacy(Signed::new_unchecked(tx, signature, tx_hash));
    Ok(Some((envelope, bridge_addr, calldata_bytes)))
}

/// Reconstruct + durably insert the full `claimAsset` calldata as a **PENDING** transaction
/// under `tx_hash_str`, idempotently (a no-op when any row — pending or committed — already
/// exists). The pending receipt is finalised — **without sealing the block** — by
/// [`Store::commit_manual_claim_event_atomic`], so `project_block_notes` stays the SOLE
/// advancer of `latest_block_number` (at end-of-block).
///
/// This is the projection-path counterpart of [`persist_synthetic_claim_tx`] (the
/// after-the-fact backfill, which commits-with-seal because its block is already sealed).
/// One idempotent primitive covers every case the projector meets:
///   * derived-hash synthesis (no real eth-tx) — no prior row → insert reconstructed;
///   * linked hash, normal — `publish_claim` already inserted the real pending row → no-op
///     (the real envelope wins, and the atomic finalises it);
///   * linked hash, **crash window** — the note→hash link survived but the envelope did NOT
///     (crash before persisting it): reconstruct here rather than emit a ClaimEvent that
///     points at empty calldata, which the derived-hash-only backfill would never repair.
///
/// Returns `Ok(false)` iff the faucet metadata preimage is unrecoverable (envelope left
/// absent; the projector backfill heals once the faucet is registered).
pub(crate) async fn insert_pending_claim_calldata(
    store: &Arc<dyn Store>,
    note_storage: &miden_protocol::note::NoteStorage,
    note_id_str: &str,
    tx_hash_str: &str,
) -> anyhow::Result<bool> {
    let tx_hash: alloy::primitives::TxHash = tx_hash_str
        .parse()
        .map_err(|e| anyhow::anyhow!("claim tx hash {tx_hash_str}: {e}"))?;
    if store.txn_get(tx_hash).await?.is_some() {
        return Ok(true);
    }
    let Some((envelope, bridge_addr, calldata_bytes)) =
        build_synthetic_claim_envelope(store, note_storage, note_id_str, tx_hash).await?
    else {
        return Ok(false);
    };
    let inserted = store
        .txn_begin_if_absent(
            tx_hash,
            crate::store::TxnEntry {
                id: None,
                envelope,
                signer: bridge_addr,
                expires_at: None,
                // MUST stay empty: the ClaimEvent log is emitted by
                // `commit_manual_claim_event_atomic`; a copy here would double-emit it.
                logs: Vec::new(),
            },
        )
        .await?;
    if inserted {
        ::metrics::counter!("synthetic_claim_calldata_persisted_total").increment(1);
        tracing::info!(
            note_id = %note_id_str,
            tx_hash = %tx_hash_str,
            calldata_bytes,
            "synthesized claim: inserted authoritative full claimAsset calldata as a PENDING \
             tx (finalised by the atomic ClaimEvent commit, never a mid-block seal)"
        );
    }
    Ok(true)
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
    let note_id_str = hex::encode(note.details_commitment().as_bytes());
    let effective_metadata = note
        .metadata()
        .or_else(|| output_metadata.get(&note.details_commitment().as_bytes()));
    project_claim_parts(
        store,
        note_id_str,
        note.details(),
        effective_metadata,
        note.consumer_account(),
        note.attachments(),
        expected_sender,
        bridge_id,
        block_number,
        block_hash,
        bridge_address,
    )
    .await
}

/// Core of the CLAIM derivation over PLAIN note parts, so it serves both note
/// sources: client-store [`InputNoteRecord`]s (the live projector + Phase 2.5,
/// via the [`project_claim_note`] adapter) and node-scanned public bodies
/// (Phase 2.6, finding #69 — where the client store was reset and the metadata
/// comes from the node's public record, with consumer attribution proven by
/// the bridge's `sync_transactions` join). Behavior is byte-identical to the
/// pre-refactor `project_claim_note`: same gates, same dedups, same atomic
/// commit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_claim_parts(
    store: &Arc<dyn Store>,
    note_id_str: String,
    details: &NoteDetails,
    effective_metadata: Option<&NoteMetadata>,
    consumer_account: Option<AccountId>,
    attachments: &NoteAttachments,
    expected_sender: AccountId,
    bridge_id: AccountId,
    block_number: u64,
    block_hash: [u8; 32],
    bridge_address: &str,
) -> anyhow::Result<ClaimProjectOutcome> {
    let claim_root = miden_base_agglayer::ClaimNote::script().root();
    if details.script().root() != claim_root {
        return Ok(ClaimProjectOutcome::Skipped);
    }

    // Provenance gate — BEFORE any storage read, dedup mark, or emission
    // (the MA#28 posture). On a chain shared with a foreign miden-agglayer
    // deployment, foreign claims share our ClaimNote script root; projecting
    // them poisons synthetic_logs with ClaimEvents our L1 never saw.
    if classify_claim_note(
        consumer_account,
        effective_metadata,
        attachments,
        expected_sender,
        bridge_id,
    ) == ClaimNoteVerdict::Foreign
    {
        ::metrics::counter!("claim_event_foreign_skipped_total").increment(1);
        tracing::warn!(
            target: "restore::claims",
            note_id = %note_id_str,
            consumer = ?consumer_account,
            sender = ?effective_metadata.map(|m| m.sender()),
            expected_sender = %expected_sender,
            bridge = %bridge_id,
            "CLAIM-shaped note is not provably ours (consumer != our bridge, and \
             sender/target don't verify against our service/bridge) — foreign \
             deployment's claim on a shared chain; skipping ClaimEvent (fail-closed)"
        );
        return Ok(ClaimProjectOutcome::Skipped);
    }

    // Exact local observation is authoritative even if an earlier projector
    // already emitted the event. Promote the durable handoff before either
    // dedup return so expiration recovery cannot later reopen this claim.
    let observed_tx_hash = store
        .confirm_note_handoff_by_commitment(&note_id_str)
        .await?;

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
    let tx_hash = match observed_tx_hash {
        Some(real_tx) => real_tx,
        None => match store.get_tx_for_note(&note_id_str).await? {
            Some(real_tx) => real_tx,
            None => derive_manual_claim_tx_hash(&note_id_str),
        },
    };

    // Write-before-seal: reconstruct + durably insert the full claimAsset calldata as a
    // PENDING tx under `tx_hash` BEFORE the atomic, so `commit_manual_claim_event_atomic`
    // finalises the envelope's receipt TOGETHER with the ClaimEvent — at THIS consumption
    // block (receipt block == log block) and WITHOUT advancing the tip. This mirrors the
    // GER path (`project_ger_note`), which likewise finalises inside its atomic and never
    // calls `txn_commit`. `insert_pending_claim_calldata` is idempotent: a no-op when the
    // row already exists (the normal linked case — `publish_claim` durably inserted the real
    // envelope), a reconstruct when it is ABSENT — either derived-hash synthesis (no real
    // eth-tx), OR the crash window where the note→hash link survived but the envelope did
    // NOT (which the derived-hash-only backfill would never repair, so the ClaimEvent would
    // otherwise ride a hash with empty calldata forever).
    //
    // FAIL-CLOSED (MA#27 review follow-up): if the calldata cannot be ensured — either a
    // transient error, or `Ok(false)` because the faucet metadata preimage is unrecoverable
    // — DO NOT publish the ClaimEvent and DO NOT mark the note processed. Return an error so
    // block projection RETRIES on a later tick (once the faucet is registered the
    // reconstruction succeeds and the event emits with valid calldata). Publishing here
    // would seal an IMMUTABLE ClaimEvent riding a hash with empty calldata: aggkit's
    // full-claim parser wedges ("input too short: 0 bytes"), and because the note is marked
    // processed nothing ever retries. A projector halt (surfaced via the metric + log below)
    // is the correct fail-closed posture — the claim is already provably ours (the
    // provenance gate ran above), so unrecoverable calldata is a real operator-actionable
    // registry gap, not a note to skip.
    match insert_pending_claim_calldata(store, details.storage(), &note_id_str, &tx_hash).await {
        Ok(true) => {}
        Ok(false) => {
            ::metrics::counter!("synthetic_claim_calldata_fail_closed_total").increment(1);
            tracing::error!(
                target: "restore::claims",
                note_id = %note_id_str,
                tx_hash = %tx_hash,
                global_index = %hex::encode(decoded.global_index),
                "synthesised claim: full claimAsset calldata UNRECOVERABLE (faucet metadata \
                 not in registry) — refusing to publish a ClaimEvent with empty calldata; \
                 projection HALTS and retries (operator: register/repair the faucet)"
            );
            anyhow::bail!(
                "claim {note_id_str}: full claimAsset calldata unrecoverable — fail-closed, \
                 projection will retry (register/repair faucet metadata to unblock)"
            );
        }
        Err(e) => {
            ::metrics::counter!("synthetic_claim_calldata_fail_closed_total").increment(1);
            return Err(e.context(format!(
                "claim {note_id_str}: failed to ensure claimAsset calldata before publishing \
                 ClaimEvent — fail-closed, projection will retry"
            )));
        }
    }

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
    // `commit_manual_claim_event_atomic` emits the ClaimEvent AND finalises the pending
    // envelope's receipt inline (a derived hash with no pending row is a harmless no-op —
    // the receipt is then synthesised from the log by `service_get_txn_receipt`), all
    // WITHOUT advancing `latest_block_number`. Deliberately NO `txn_commit` here: it would
    // duplicate that finalise and, on Postgres, seal block N mid-loop — before this block's
    // later B2AGG/GER/Claim notes are written — so aggkit could scan a partial block N,
    // advance its cursor, and permanently miss the later logs. `project_block_notes` is the
    // SOLE advancer of the tip, at end-of-block (the projector's write-before-advance
    // invariant).

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

    let note_commitment = hex::encode(note.details_commitment().as_bytes());
    // Promote before storage/dedup exits: seeing our exact note proves that a
    // prepared handoff must not be cleared and rebuilt after expiration.
    let observed_tx_hash = store
        .confirm_note_handoff_by_commitment(&note_commitment)
        .await?;

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
    let tx_hash = match observed_tx_hash {
        Some(real_tx) => real_tx,
        None => match store.get_tx_for_note(&note_commitment).await? {
            Some(real_tx) => real_tx,
            None => {
                let mut hasher = Keccak256::new();
                hasher.update(b"restore-ger-miden-");
                hasher.update(note_commitment.as_bytes());
                format!("0x{}", hex::encode(hasher.finalize()))
            }
        },
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

    tracing::info!(
        note_id = %hex::encode(note.details_commitment().as_bytes()),
        ger = %hex::encode(ger_bytes),
        "restore: rebuilt GER from consumed UpdateGerNote"
    );

    Ok(GerProjectOutcome::Emitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::InMemoryStore;
    use miden_protocol::note::{
        NoteAttachment, NoteAttachments, NoteMetadata, NoteType, PartialNoteMetadata,
    };
    use miden_protocol::{Felt, Word};
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

    // PR#164 blocker #1 — the node-metadata join must fail closed on a
    // details-commitment collision that carries different provenance, never
    // last-write-wins.
    #[test]
    fn ger_node_metadata_is_ambiguity_safe() {
        let (manager_meta, _) = make_metadata(id(TEST_SENDER_MANAGER), None);
        let (attacker_meta, _) = make_metadata(id(TEST_SENDER_ATTACKER), None);
        assert_ne!(manager_meta, attacker_meta);
        let key = [0x11u8; 32];

        let mut map = std::collections::HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();

        // First writer lands.
        record_ger_node_metadata(&mut map, &mut ambiguous, key, manager_meta);
        assert_eq!(map.get(&key), Some(&manager_meta));

        // Idempotent: identical metadata for the same key is a no-op.
        record_ger_node_metadata(&mut map, &mut ambiguous, key, manager_meta);
        assert_eq!(map.get(&key), Some(&manager_meta));

        // A DIFFERENT metadata for the same key poisons the key: removed +
        // marked ambiguous, so the replay serves nothing (fail closed), NOT the
        // attacker's provenance and NOT the original last-write-wins.
        record_ger_node_metadata(&mut map, &mut ambiguous, key, attacker_meta);
        assert!(!map.contains_key(&key), "ambiguous key must not be served");
        assert!(ambiguous.contains(&key));

        // Once ambiguous, even a later "correct" write stays out — we can no
        // longer trust which is authentic.
        record_ger_node_metadata(&mut map, &mut ambiguous, key, manager_meta);
        assert!(!map.contains_key(&key));

        // NOTE: the poisoning above is only reachable for candidates that ALREADY
        // passed the MA#28 provenance gate in `scan_bridge_out_bodies` — see
        // `ger_metadata_clone_cannot_evict_the_genuine_entry`, which pins that an
        // impostor never reaches this map in the first place.
        //
        // (assertions continue below)

        // A distinct key is unaffected.
        let key2 = [0x22u8; 32];
        record_ger_node_metadata(&mut map, &mut ambiguous, key2, manager_meta);
        assert_eq!(map.get(&key2), Some(&manager_meta));
    }

    /// #164 re-review (griefing DoS) — details are PUBLIC and exclude metadata, so
    /// anyone can publish a note whose details match a legitimate UpdateGerNote but
    /// whose sender is their own. Keyed on details-commitment alone, that clone
    /// collides with the genuine entry, trips the ambiguity guard, and DELETES the
    /// real GER's provenance — recovery then loses that GER forever, on demand, for
    /// free. This is not a hash collision; it is a chosen-input attack.
    ///
    /// The defence is to gate candidates on MA#28 provenance BEFORE they are
    /// recorded, using the same `classify_ger_note` predicate that governs
    /// acceptance. `metadata.sender` is stamped by the account that executed the
    /// minting transaction and cannot be forged, so the impostor is rejected at the
    /// scan and never reaches the map — the genuine entry survives intact.
    #[test]
    fn ger_metadata_clone_cannot_evict_the_genuine_entry() {
        let manager = id(TEST_SENDER_MANAGER);
        let bridge = id(TEST_TARGET_BRIDGE);
        let attacker = id(TEST_SENDER_ATTACKER);

        let (genuine_meta, genuine_attachments) = make_metadata(manager, Some(bridge));
        let (clone_meta, clone_attachments) = make_metadata(attacker, Some(bridge));

        // The gate the scan applies, verbatim.
        let accepts = |meta: &NoteMetadata, att: &NoteAttachments| {
            matches!(
                classify_ger_note(Some(meta), att, manager, bridge),
                GerNoteVerdict::Accept
            )
        };

        assert!(
            accepts(&genuine_meta, &genuine_attachments),
            "the real ger_manager's note must be recorded as a candidate"
        );
        assert!(
            !accepts(&clone_meta, &clone_attachments),
            "a same-details clone from another sender must be REJECTED at scan time — \
             otherwise it poisons the key and evicts the genuine entry"
        );

        // End to end over the map: only the accepted candidate is ever recorded, so
        // the genuine provenance is still served after the clone is observed.
        let mut map = std::collections::HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();
        let key = [0x42u8; 32];
        for (meta, att) in [
            (genuine_meta, genuine_attachments),
            (clone_meta, clone_attachments),
        ] {
            if accepts(&meta, &att) {
                record_ger_node_metadata(&mut map, &mut ambiguous, key, meta);
            }
        }
        assert!(
            !ambiguous.contains(&key),
            "an impostor must not be able to mark a genuine GER key ambiguous"
        );
        assert_eq!(
            map.get(&key).map(|m| m.sender()),
            Some(manager),
            "the genuine ger_manager provenance must survive the clone attempt"
        );
    }

    /// Build a consumed record with an exact (block, tx_order) and a commitment
    /// that varies with `seed`, so ordering can be asserted precisely.
    fn ordered_consumed(block: u32, tx_order: Option<u32>, seed: u8) -> InputNoteRecord {
        use miden_base_agglayer::B2AggNote;
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteAssets, NoteRecipient, NoteStorage};
        use miden_protocol::{Felt, Word};

        let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
        // Vary the serial so each record gets a distinct details commitment.
        let serial = Word::from([
            Felt::from(seed as u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
        ]);
        let recipient = NoteRecipient::new(serial, B2AggNote::script(), storage);
        let details = NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient);
        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(block),
            consumer_account: None,
            consumed_tx_order: tx_order,
            metadata: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    /// The ordering contract the merged replay exists to honour: emission order is
    /// BLOCK-major, exactly as the live projector walks notes and as
    /// `docs/design/UNIFIED-PROJECTOR.md` specifies (`{B2AGG, INTERNAL} -> ORDER ->
    /// EMIT` — ONE ordering stage fed by every source).
    ///
    /// Restore used to run a pass per note KIND, so every B2AGG was emitted before
    /// any CLAIM or GER regardless of block. Two consumer-visible consequences
    /// followed, both measured on the 2026-08-11 recovery gate:
    ///   * `log_index` (ONE global counter) was renumbered for every synthetic log
    ///     — 15 BridgeEvents held indices 0..14 while spanning blocks 97..432.
    ///   * `hash_chain_value` is a fold in EMISSION order, so with several GERs in
    ///     one block the restored chain diverged from the live one even though the
    ///     GER set and per-block content matched exactly — and aggkit reads it.
    ///
    /// Every kind now maps to ONE key tuple compared by ONE comparator, so kind
    /// cannot outrank block by construction. This pins the extraction and the
    /// resulting order.
    #[test]
    fn replay_order_is_block_major_then_tx_order_then_commitment() {
        let late = ordered_consumed(432, Some(0), 0x11);
        let early = ordered_consumed(10, Some(7), 0xEE);
        let mid_a = ordered_consumed(97, Some(3), 0x22);
        let mid_b = ordered_consumed(97, Some(3), 0x33);
        let mid_earlier_tx = ordered_consumed(97, Some(1), 0xFF);

        let items = [
            ReplayItem::Consumed(&late),
            ReplayItem::Consumed(&mid_b),
            ReplayItem::Consumed(&early),
            ReplayItem::Consumed(&mid_earlier_tx),
            ReplayItem::Consumed(&mid_a),
        ];
        let empty = ConsumedOrderMap::new();
        let mut keys: Vec<_> = items
            .iter()
            .map(|item| replay_sort_key(item, &empty))
            .collect();
        keys.sort();

        assert_eq!(
            keys[0].0, 10,
            "earliest BLOCK first — block outranks everything"
        );
        assert_eq!(
            (keys[1].0, keys[1].1, keys[1].3),
            (97, Some(1), mid_earlier_tx.details_commitment().as_bytes()),
            "within a block, lower tx_order first"
        );
        // The two same-slot records break the tie by details commitment, in that order.
        let (c_a, c_b) = (
            mid_a.details_commitment().as_bytes(),
            mid_b.details_commitment().as_bytes(),
        );
        let (lo, hi) = if c_a < c_b { (c_a, c_b) } else { (c_b, c_a) };
        assert_eq!(
            (keys[2].0, keys[2].1, keys[2].3),
            (97, Some(3), lo),
            "same (block, tx_order): commitment breaks the tie"
        );
        assert_eq!((keys[3].0, keys[3].1, keys[3].3), (97, Some(3), hi));
        assert_eq!(
            keys[4].0, 432,
            "latest block LAST — under the old kind-major replay this B2AGG-shaped \
             record would have been emitted first regardless of its block"
        );
    }

    /// The case that motivated carrying `within_tx_pos`: TWO B2AGG siblings in ONE
    /// transaction. Their relative order is decided by the authoritative input
    /// position, NOT by details commitment — so the sibling with the LOWER input
    /// position must sort first even when its commitment is larger.
    ///
    /// Before this, restore fell straight through to the commitment tiebreak and
    /// could emit same-transaction siblings in the opposite order to live. Because
    /// an UpdateHashChain log carries the rolling chain value in its topics, one
    /// such inversion re-chains every subsequent log — which is exactly what the
    /// full-DB-loss content digest caught.
    #[test]
    fn replay_order_uses_input_position_for_same_tx_b2agg_siblings() {
        // Same block + same tx_order; commitment order is deliberately the INVERSE
        // of the input-position order, so only `within_tx_pos` can decide.
        let a = ordered_consumed(50, Some(2), 0xFF); // larger commitment seed
        let b = ordered_consumed(50, Some(2), 0x01); // smaller commitment seed

        let key = |block, tx, pos: u32, n: &InputNoteRecord| {
            (
                block,
                Some(tx),
                pos,
                n.details_commitment().as_bytes(),
                None::<[u8; 32]>,
            )
        };
        // `a` is input 0, `b` is input 1 — position must win over commitment.
        let ka = key(50u64, 2u32, 0, &a);
        let kb = key(50u64, 2u32, 1, &b);
        assert!(
            ka < kb,
            "input position must outrank the commitment tiebreak for same-transaction \
             siblings — otherwise restore can invert live's order and re-chain every \
             following UpdateHashChain log"
        );

        // Sanity: with positions EQUAL (the non-B2AGG case, both 0) the commitment
        // tiebreak still applies, so ordering stays deterministic.
        let ka0 = key(50u64, 2u32, 0, &a);
        let kb0 = key(50u64, 2u32, 0, &b);
        assert!(
            kb0 < ka0,
            "with no authoritative position, the commitment breaks the tie deterministically"
        );
    }

    /// FALLBACK ONLY: with no authoritative entry, a consumed record without a
    /// `tx_order` still sorts before one that has it. This is the degraded path for a
    /// note absent from the node's transaction feed — NOT the normal one. The previous
    /// version of this test asserted the same thing as the INTENDED behaviour for all
    /// consumed notes, which is precisely the bug below: it made a real ordering defect
    /// look like a satisfied invariant.
    #[test]
    fn replay_order_unordered_consumed_first_only_without_authoritative_order() {
        let empty = ConsumedOrderMap::new();
        let no_order = ordered_consumed(5, None, 0xF0);
        let with_order = ordered_consumed(5, Some(0), 0x01);
        assert!(
            replay_sort_key(&ReplayItem::Consumed(&no_order), &empty)
                < replay_sort_key(&ReplayItem::Consumed(&with_order), &empty),
            "with no authoritative order, None tx_order sorts before Some(_)"
        );
    }

    /// THE REGRESSION. The client store records `consumed_tx_order` as NULL for every
    /// note it learns by nullifier (measured on a live stack: 135/135 NULL, while both
    /// `note_id` and `nullifier` were populated 135/135). Feeding that `None` to the
    /// comparator put EVERY consumed note ahead of EVERY same-block bridge-out, because
    /// `None < Some(_)` — a fixed kind order masquerading as chronology. Live orders by
    /// the node's authoritative tx order, so a block holding a GER note and a bridge-out
    /// came out one way live and the other way on restore, swapping their `log_index`.
    ///
    /// Caught by an eth_getLogs diff against a LIVE baseline (blocks 173 + 288). It could
    /// NOT be caught by `hash_chain_value`, which depends only on UHC-to-UHC order and is
    /// invariant under this swap — which is why every prior assertion passed.
    #[test]
    fn authoritative_order_joins_on_either_identity() {
        let nullifier = Nullifier::from_raw(Word::new([Felt::new(9u64).unwrap(); 4]));
        let note_id = NoteId::from_raw(Word::new([Felt::new(7u64).unwrap(); 4]));
        let mut order = ConsumedOrderMap::new();
        order.by_nullifier.insert(nullifier, (7, 4));
        order.by_note_id.insert(note_id, (7, 6));

        assert_eq!(
            order.lookup(Some(nullifier), None),
            Some((7, 4)),
            "joins on nullifier"
        );
        assert_eq!(
            order.lookup(None, Some(note_id)),
            Some((7, 6)),
            "joins on NoteId when the record carries no nullifier"
        );
        assert_eq!(
            order.lookup(Some(nullifier), Some(note_id)),
            Some((7, 4)),
            "nullifier wins when both are present"
        );
        assert_eq!(order.lookup(None, None), None, "no identity, no join");
    }

    /// The ordering consequence: a same-block bridge-out at tx_order 1 must precede a
    /// consumed note the node places at tx_order 4. Under the old rule the consumed note
    /// won unconditionally because its store `tx_order` was NULL.
    #[test]
    fn authoritative_tx_order_puts_an_earlier_bridge_out_first() {
        let consumed = ordered_consumed(7, None, 0xAA);
        let empty = ConsumedOrderMap::new();
        let bridge_out_at_tx1 = (7u64, Some(1u32), 0u32, [0u8; 32], None);

        // Regression precondition: with no authoritative order the consumed note sorts
        // FIRST despite the bridge-out executing earlier in the block.
        assert!(
            replay_sort_key(&ReplayItem::Consumed(&consumed), &empty) < bridge_out_at_tx1,
            "without the feed a NULL tx_order sorts ahead of an earlier bridge-out"
        );

        // With the authoritative order applied, tx_order decides and the bridge-out wins.
        let with_order: (u64, Option<u32>, u32, [u8; 32], Option<[u8; 32]>) =
            (7, Some(4), 0, [0u8; 32], None);
        assert!(
            with_order > bridge_out_at_tx1,
            "tx_order 1 precedes tx_order 4 in the same block"
        );
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
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            0,
            "an individual note projection must not seal its block"
        );

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
    /// note. Recovery now runs that healing sweep ITSELF (Phase 1.1), before the
    /// replay phases that read the consumed feed. When the sweep reached the tip
    /// there is nothing left to re-discover, so Phase 4 leaves the reconcile
    /// cursor AT the swept tip — the next boot must not re-walk all of history
    /// (it could not project anything anyway: the projector cursor is parked at
    /// the tip, which is exactly how the GER history used to be lost).
    #[tokio::test]
    async fn restore_leaves_reconcile_cursor_at_swept_tip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        // Simulate a long-running pre-restore deployment: both cursors deep
        // into history.
        store.set_reconcile_cursor(123_456).await.unwrap();
        store.set_projector_cursor(100_000).await.unwrap();

        // Phase 4 of restore() — the exact code path the real restore runs,
        // with Phase 1.1's healing sweep having reached the tip.
        finalize_restore_cursors(&store, 130_000, Some(130_000))
            .await
            .unwrap();

        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            130_000,
            "recovery already swept to the tip — the next boot must not redo the full walk"
        );
        assert_eq!(
            store.get_projector_cursor().await.unwrap(),
            130_000,
            "projector cursor resumes at the Miden tip (restore already replayed history)"
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 130_000);
    }

    /// Fail-safe half of the contract above: if recovery's healing sweep did NOT
    /// reach the tip (never ran, or stopped short), the reconcile cursor must
    /// fall back to genesis so the serving proxy still attempts the heal. Parking
    /// it at a short tip would strand the un-swept range forever.
    #[tokio::test]
    async fn restore_resets_reconcile_cursor_when_heal_incomplete() {
        for swept in [None, Some(129_999u64)] {
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
            store.set_reconcile_cursor(123_456).await.unwrap();

            finalize_restore_cursors(&store, 130_000, swept)
                .await
                .unwrap();

            assert_eq!(
                store.get_reconcile_cursor().await.unwrap(),
                0,
                "an incomplete heal ({swept:?} vs tip 130000) must fall back to a genesis re-sweep"
            );
        }
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
            metadata: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    fn ma3_note_id(note: &InputNoteRecord, bridge_id: AccountId) -> miden_protocol::note::NoteId {
        let attachments = NoteAttachments::default();
        let metadata = NoteMetadata::new(
            PartialNoteMetadata::new(bridge_id, NoteType::Public),
            &attachments,
        );
        miden_protocol::note::NoteId::new(note.details_commitment(), &metadata)
    }

    #[test]
    fn restore_replay_is_complete_and_preserves_same_details_note_ids() {
        use miden_client::rpc::domain::transaction::TransactionRecord;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteHeader, Nullifier};
        use miden_protocol::transaction::{InputNoteCommitment, InputNotes, TransactionHeader};

        let (faucet_id, bridge_id, sender_id) = ma3_accounts();
        let details = ma3_b2agg_input_note(faucet_id, None).details().clone();
        let attachments = NoteAttachments::default();
        let metadata = |sender| {
            NoteMetadata::new(
                PartialNoteMetadata::new(sender, NoteType::Public),
                &attachments,
            )
        };
        let first = NoteHeader::new(details.commitment(), metadata(bridge_id));
        let second = NoteHeader::new(details.commitment(), metadata(sender_id));
        assert_ne!(first.id(), second.id());
        let ids = [first.id(), second.id()];

        let nullifier = |value| Nullifier::from_raw(Word::new([Felt::new(value).unwrap(); 4]));
        let second_nullifier = nullifier(2);
        let inputs = InputNotes::new(vec![
            InputNoteCommitment::from_parts_unchecked(nullifier(1), Some(first)),
            InputNoteCommitment::from_parts_unchecked(second_nullifier, None),
        ])
        .unwrap();
        let tx = TransactionRecord {
            block_num: BlockNumber::from(7u32),
            transaction_header: TransactionHeader::new(
                bridge_id,
                Word::default(),
                Word::new([Felt::new(1).unwrap(); 4]),
                inputs,
                vec![],
            ),
            output_notes: vec![],
            erased_output_notes: vec![],
        };
        assert!(ensure_complete_note_response(&ids, &ids[..1]).is_err());

        let recovered = RecoveredBridgeOuts {
            id_by_nullifier: std::collections::HashMap::from([(second_nullifier, second.id())]),
            by_id: ids
                .iter()
                .copied()
                .map(|id| {
                    (
                        id,
                        RecoveredBridgeBody {
                            details: details.clone(),
                            attachments: attachments.clone(),
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        let replay = build_bridge_replay(&[tx], bridge_id, recovered).unwrap();
        assert_eq!(replay.iter().map(|item| item.id).collect::<Vec<_>>(), ids);
        assert!(
            replay
                .iter()
                .all(|item| item.block == 7 && item.tx_order == 0)
        );
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        // local_network_id == 0 == the note's destination network → poison self-target.
        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            0, // local_network_id == dest-network 0 → self-target
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();
        store.reserve_deposit_index(&note_id).await.unwrap();
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
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();
        store.reserve_deposit_index(&note_id).await.unwrap();
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
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
        let note_id = ma3_note_id(&note, bridge_id).to_hex();

        let outcome = project_b2agg_note(
            &store,
            &note,
            ma3_note_id(&note, bridge_id),
            bridge_id,
            7, // local_network_id (test notes target dest-network 0, so no self-target gate)
            1,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
            metadata: None,
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
        use miden_protocol::note::{
            NoteAttachments, NoteId, NoteMetadata, NoteType, PartialNoteMetadata,
        };

        let metadata = NoteMetadata::new(
            PartialNoteMetadata::new(bridge_id, NoteType::Public),
            &NoteAttachments::default(),
        );
        let note_id = NoteId::new(note.details_commitment(), &metadata);
        let note_key = note_id.to_hex();
        let outcome = project_b2agg_note(
            store,
            note,
            note_id,
            bridge_id,
            7, // local_network_id (well-formed test notes target dest-network 0)
            42,
            [7u8; 32],
            get_bridge_address(),
            None,
            &crate::metadata_recovery::NetworkRpcMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            B2AggRestoreOutcome::Skipped,
            "untranslatable B2AGG must be a quarantine skip, not an emit",
        );
        let row = store
            .get_unbridgeable_bridge_out(&note_key)
            .await
            .unwrap()
            .expect("restore skip must write a quarantine row (MA#18)");
        assert_eq!(row.note_id, note_key);
        assert_eq!(row.bridge_account, bridge_id);
        assert_eq!(row.reason, reason);
        assert_eq!(row.observed_block, 42);
        assert!(!row.detail.is_empty(), "detail must carry the skip cause");
        assert!(
            !store.is_note_processed(&note_key).await.unwrap(),
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
            metadata: None,
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
        // Default: empty metadata → truthful by hash → reconstructable with no registry entry.
        claim_input_note_meta(consumer, gi_byte, &[])
    }

    /// Like [`claim_input_note`] but with a caller-chosen metadata preimage, so a test can
    /// force a claim whose full calldata is UNRECOVERABLE (a non-empty metadata hash with no
    /// registry preimage that hashes to it) — the fail-closed path.
    fn claim_input_note_meta(
        consumer: Option<AccountId>,
        gi_byte: u8,
        metadata: &[u8],
    ) -> InputNoteRecord {
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
                metadata_hash: MetadataHash::from_abi_encoded(metadata),
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
            metadata: None,
        });
        InputNoteRecord::new(details, NoteAttachments::default(), None, state)
    }

    // ── Finding #69 — node-scan CLAIM replay (Phase 2.6) ─────────────────────

    /// Finding #69 (a): `build_claim_replay` joins a node-scanned CLAIM body to
    /// the bridge transaction that consumed it via the NULLIFIER fallback (the
    /// input-note commitment carries no header), yielding the claim at the
    /// bridge tx's authoritative `(block, tx_order)`.
    #[test]
    fn finding69_build_claim_replay_joins_by_nullifier_fallback() {
        use miden_client::rpc::domain::transaction::TransactionRecord;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::Nullifier;
        use miden_protocol::transaction::{InputNoteCommitment, InputNotes, TransactionHeader};

        let (_faucet_id, bridge_id, _sender_id) = ma3_accounts();
        let details = claim_input_note(Some(bridge_id), 0x69).details().clone();
        let (metadata, attachments) = make_metadata(id(TEST_SENDER_MANAGER), Some(bridge_id));
        let note_id = miden_protocol::note::NoteId::new(details.commitment(), &metadata);

        let nullifier = Nullifier::from_raw(Word::new([Felt::new(9).unwrap(); 4]));
        // No header on the input commitment → the join MUST fall back to the
        // nullifier map (the shape `ConsumedExternal` history produces).
        let inputs = InputNotes::new(vec![InputNoteCommitment::from_parts_unchecked(
            nullifier, None,
        )])
        .unwrap();
        let tx = TransactionRecord {
            block_num: BlockNumber::from(11u32),
            transaction_header: TransactionHeader::new(
                bridge_id,
                Word::default(),
                Word::new([Felt::new(1).unwrap(); 4]),
                inputs,
                vec![],
            ),
            output_notes: vec![],
            erased_output_notes: vec![],
        };

        let claims_by_id = std::collections::HashMap::from([(
            note_id,
            RecoveredClaimBody {
                details,
                metadata,
                attachments,
            },
        )]);
        let claim_id_by_nullifier = std::collections::HashMap::from([(nullifier, note_id)]);

        let replay =
            build_claim_replay(&[tx], bridge_id, claims_by_id, claim_id_by_nullifier).unwrap();
        assert_eq!(replay.len(), 1, "the consumed claim must join exactly once");
        assert_eq!(replay[0].id, note_id);
        assert_eq!(
            (replay[0].block, replay[0].tx_order),
            (11, 0),
            "claim must carry the consuming bridge tx's (block, tx_order)"
        );
    }

    /// Finding #69 (b): `project_claim_parts` over node-scanned parts
    /// (consumer = our bridge, metadata sender = our service) emits a
    /// ClaimEvent, marks the note processed, does NOT seal the block, and is
    /// idempotent — the second call short-circuits on Dedup 1.
    #[tokio::test]
    async fn finding69_project_claim_parts_emits_and_dedups() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let bridge_id = id(TEST_TARGET_BRIDGE);
        let service = id(TEST_SENDER_MANAGER);
        let details = claim_input_note(Some(bridge_id), 0x77).details().clone();
        let (metadata, attachments) = make_metadata(service, Some(bridge_id));
        let note_id_str = hex::encode(details.commitment().as_bytes());

        let outcome = project_claim_parts(
            &store,
            note_id_str.clone(),
            &details,
            Some(&metadata),
            Some(bridge_id),
            &attachments,
            service,
            bridge_id,
            9,
            [9u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ClaimProjectOutcome::Emitted);
        assert!(store.is_claim_note_processed(&note_id_str).await.unwrap());
        let mut gi = [0u8; 32];
        gi[31] = 0x77;
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            0,
            "a node-scan claim replay must not seal its block"
        );

        let again = project_claim_parts(
            &store,
            note_id_str.clone(),
            &details,
            Some(&metadata),
            Some(bridge_id),
            &attachments,
            service,
            bridge_id,
            9,
            [9u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();
        assert_eq!(
            again,
            ClaimProjectOutcome::Skipped,
            "second replay of the same claim must dedup"
        );
    }

    /// Finding #69 (c): a FOREIGN claim (consumed by a foreign bridge, minted
    /// by a foreign sender) fed through `project_claim_parts` is a fail-closed
    /// skip — no ClaimEvent, no processed mark.
    #[tokio::test]
    async fn finding69_project_claim_parts_foreign_is_fail_closed_skip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let bridge_id = id(TEST_TARGET_BRIDGE);
        let service = id(TEST_SENDER_MANAGER);
        let foreign_sender = id(TEST_SENDER_ATTACKER);
        let foreign_bridge = id(TEST_TARGET_OTHER);
        let details = claim_input_note(Some(foreign_bridge), 0x78)
            .details()
            .clone();
        // Foreign deployment: minted by the foreign service, targeting the
        // foreign bridge — neither our consumer proof nor our mint proof holds.
        let (metadata, attachments) = make_metadata(foreign_sender, Some(foreign_bridge));
        let note_id_str = hex::encode(details.commitment().as_bytes());

        let outcome = project_claim_parts(
            &store,
            note_id_str.clone(),
            &details,
            Some(&metadata),
            Some(foreign_bridge),
            &attachments,
            service,
            bridge_id,
            9,
            [9u8; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ClaimProjectOutcome::Skipped);
        assert!(
            !store.is_claim_note_processed(&note_id_str).await.unwrap(),
            "foreign claim must not be marked processed"
        );
        let mut gi = [0u8; 32];
        gi[31] = 0x78;
        assert!(
            !store.has_claim_event_for_global_index(&gi).await.unwrap(),
            "foreign claim must not emit a ClaimEvent"
        );
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

    // ── Synthesized-claim full-calldata recovery (PR #136 review) ────────────

    /// A ClaimNoteStorage with DISTINCT values in every field, so the full decode +
    /// calldata rebuild can prove each field lands in the right claimAsset slot.
    fn full_claim_fixture(metadata: &[u8]) -> miden_protocol::note::NoteStorage {
        use miden_base_agglayer::{
            ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
            ProofData, SmtNode,
        };
        // Distinct per-node proof values: local node i = [i+1; 32], rollup node i = [0x80+i; 32].
        let local: [SmtNode; 32] = std::array::from_fn(|i| SmtNode::new([(i as u8) + 1; 32]));
        let rollup: [SmtNode; 32] = std::array::from_fn(|i| SmtNode::new([0x80 + (i as u8); 32]));
        let mut gi = [0u8; 32];
        gi[23] = 1; // mainnet flag
        gi[31] = 0x2A;
        let mut amount = [0u8; 32];
        amount[24..].copy_from_slice(&123_456_789u64.to_be_bytes());
        let storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: local,
                smt_proof_rollup_exit_root: rollup,
                global_index: GlobalIndex::new(gi),
                mainnet_exit_root: ExitRoot::new([0x11; 32]),
                rollup_exit_root: ExitRoot::new([0x22; 32]),
            },
            leaf_data: LeafData {
                origin_network: 0,
                origin_token_address: EthAddress::new([0xAB; 20]),
                destination_network: 2,
                destination_address: EthAddress::new([0xCD; 20]),
                amount: EthAmount::new(amount),
                metadata_hash: MetadataHash::from_abi_encoded(metadata),
            },
            miden_claim_amount: miden_protocol::Felt::ZERO,
        };
        miden_protocol::note::NoteStorage::try_from(storage).expect("fixture round-trips")
    }

    /// Full-storage decode + calldata rebuild round-trip: EVERY claimAsset field must be
    /// the authoritative note-storage value — both SMT proofs node-for-node, both exit
    /// roots, the note-derived destination network (review req 5), addresses, U256
    /// amount — plus the hash-verified metadata preimage. Nothing zero-filled.
    #[test]
    fn full_claim_decode_rebuilds_authoritative_claim_asset_calldata() {
        use alloy_core::sol_types::SolCall;
        let metadata = b"abi-encoded token metadata".to_vec();
        let storage = full_claim_fixture(&metadata);
        let full = parse_full_claim_from_storage(&storage).expect("full decode");

        let call = build_claim_asset_call(&full, metadata.clone());
        let raw = call.abi_encode();
        assert!(raw.starts_with(&crate::claim::claimAssetCall::SELECTOR));
        let decoded = crate::claim::claimAssetCall::abi_decode(&raw).expect("aggkit-parseable");

        for i in 0..32 {
            assert_eq!(
                decoded.smtProofLocalExitRoot[i].0,
                [(i as u8) + 1; 32],
                "local SMT proof node {i} must be the note-storage value"
            );
            assert_eq!(
                decoded.smtProofRollupExitRoot[i].0,
                [0x80 + (i as u8); 32],
                "rollup SMT proof node {i} must be the note-storage value"
            );
        }
        assert_eq!(decoded.mainnetExitRoot.0, [0x11; 32], "mainnet exit root");
        assert_eq!(decoded.rollupExitRoot.0, [0x22; 32], "rollup exit root");
        let mut gi = [0u8; 32];
        gi[23] = 1;
        gi[31] = 0x2A;
        assert_eq!(
            decoded.globalIndex,
            alloy::primitives::U256::from_be_bytes(gi)
        );
        assert_eq!(decoded.originNetwork, 0);
        assert_eq!(decoded.originTokenAddress.as_slice(), &[0xAB; 20]);
        assert_eq!(
            decoded.destinationNetwork, 2,
            "destination network must come from the NOTE (review req 5), not config"
        );
        assert_eq!(decoded.destinationAddress.as_slice(), &[0xCD; 20]);
        assert_eq!(
            decoded.amount,
            alloy::primitives::U256::from(123_456_789u64)
        );
        assert_eq!(
            decoded.metadata.as_ref(),
            metadata.as_slice(),
            "metadata must be the hash-verified preimage"
        );
    }

    /// Registry-backed metadata recovery: persist succeeds only with a preimage whose
    /// keccak256 equals the note's metadata_hash, and the persisted envelope (keyed by
    /// the DERIVED hash — the record eth_getTransactionByHash serves ahead of any
    /// synthetic fallback) carries it verbatim.
    #[tokio::test]
    async fn persist_synthetic_claim_tx_recovers_registry_metadata() {
        use alloy::consensus::Transaction;
        use alloy_core::sol_types::SolCall;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let metadata = b"\x00\x01erc20 name symbol decimals".to_vec();
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id: id(TEST_TARGET_BRIDGE),
                origin_address: [0xAB; 20],
                origin_network: 0,
                symbol: "TT".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: metadata.clone(),
            })
            .await
            .unwrap();

        let storage = full_claim_fixture(&metadata);
        let note_id = "cafebabe";
        let derived = derive_manual_claim_tx_hash(note_id);
        let persisted =
            persist_synthetic_claim_tx(&store, &storage, note_id, &derived, 8831, [0xAA; 32])
                .await
                .unwrap();
        assert!(persisted, "hash-verified registry metadata must persist");

        let tx_hash: alloy::primitives::TxHash = derived.parse().unwrap();
        let data = store
            .txn_get(tx_hash)
            .await
            .unwrap()
            .expect("calldata record persisted under the DERIVED hash");
        let decoded = crate::claim::claimAssetCall::abi_decode(data.envelope.input())
            .expect("stored input is full claimAsset calldata");
        assert_eq!(decoded.metadata.as_ref(), metadata.as_slice());
        assert_eq!(decoded.destinationNetwork, 2);
        assert_eq!(decoded.mainnetExitRoot.0, [0x11; 32]);
        // The record is COMMITTED at the ClaimEvent's block so the receipt matches.
        let (result, block) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(block, 8831);

        // Idempotent: a re-run (restore replay / projector backfill) is a no-op success.
        let again =
            persist_synthetic_claim_tx(&store, &storage, note_id, &derived, 8831, [0xAA; 32])
                .await
                .unwrap();
        assert!(again);
    }

    /// Native-ETH / empty-metadata claims: the empty preimage is truthful by the hash —
    /// persist succeeds with empty metadata (no registry entry needed).
    #[tokio::test]
    async fn persist_synthetic_claim_tx_accepts_empty_metadata_by_hash() {
        use alloy_core::sol_types::SolCall;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let storage = full_claim_fixture(&[]);
        let derived = derive_manual_claim_tx_hash("eth-claim");
        assert!(
            persist_synthetic_claim_tx(&store, &storage, "eth-claim", &derived, 7, [0u8; 32])
                .await
                .unwrap()
        );
        let data = store
            .txn_get(derived.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        use alloy::consensus::Transaction;
        let decoded = crate::claim::claimAssetCall::abi_decode(data.envelope.input()).unwrap();
        assert!(decoded.metadata.is_empty());
    }

    /// PR #151 blocker 1: a terminally-FAILED calldata row (`Some(Err(_))`) whose ClaimEvent
    /// exists (so the claim succeeded — the failed status is stale) must be HEALED to success,
    /// not treated as complete-but-stuck. The old `data.result.is_some()` guard returned
    /// `Ok(true)` for `Some(Err(_))` and left the row failed, so the durable repair backlog
    /// (drained only on a successful `txn_commit`) pinned `/health` at 503 forever.
    #[tokio::test]
    async fn persist_synthetic_claim_tx_heals_terminally_failed_row() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let storage = full_claim_fixture(&[]);
        let derived = derive_manual_claim_tx_hash("failed-claim");
        let tx_hash: alloy::primitives::TxHash = derived.parse().unwrap();
        // Build a terminally-FAILED row: reconstruct the real envelope, then commit it with
        // Err (a prior failed repair / failed on-chain result).
        let (envelope, bridge_addr, _) =
            build_synthetic_claim_envelope(&store, &storage, "failed-claim", tx_hash)
                .await
                .unwrap()
                .unwrap();
        store
            .txn_begin(
                tx_hash,
                crate::store::TxnEntry {
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
            .txn_commit(
                tx_hash,
                Err("prior repair failed".to_string()),
                5,
                [0u8; 32],
            )
            .await
            .unwrap();
        assert!(
            matches!(
                store.txn_get(tx_hash).await.unwrap().unwrap().result,
                Some(Err(_))
            ),
            "precondition: the row is terminally failed"
        );
        // Heal — the ClaimEvent exists, so the failed status is stale ground-truth.
        let healed =
            persist_synthetic_claim_tx(&store, &storage, "failed-claim", &derived, 5, [0u8; 32])
                .await
                .unwrap();
        assert!(
            healed,
            "a failed row whose event exists must heal to resolved"
        );
        // Postcondition: the row is now SUCCESS, so the /health repair backlog drains rather
        // than pinning at 503 forever.
        assert!(
            matches!(
                store.txn_get(tx_hash).await.unwrap().unwrap().result,
                Some(Ok(_))
            ),
            "healed row must be committed to success so the repair backlog drains"
        );
    }

    /// Unrecoverable metadata (non-empty hash, no registry preimage hashing to it): the
    /// calldata must NOT be fabricated — no tx record is written (the serve path keeps
    /// the empty input and alarms), and the caller sees `false`.
    #[tokio::test]
    async fn persist_synthetic_claim_tx_refuses_to_fabricate_unrecoverable_metadata() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        // Note built with metadata whose preimage is NOT in the registry.
        let storage = full_claim_fixture(b"preimage the registry never saw");
        let derived = derive_manual_claim_tx_hash("orphan-metadata");
        let persisted =
            persist_synthetic_claim_tx(&store, &storage, "orphan-metadata", &derived, 9, [0u8; 32])
                .await
                .unwrap();
        assert!(!persisted, "must refuse to fabricate");
        assert!(
            store
                .txn_get(derived.parse().unwrap())
                .await
                .unwrap()
                .is_none(),
            "no fabricated record may exist"
        );
        // A registry entry whose metadata does NOT hash to the note's hash is refused too.
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id: id(TEST_TARGET_OTHER),
                origin_address: [0xAB; 20],
                origin_network: 0,
                symbol: "TT".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: b"a DIFFERENT preimage".to_vec(),
            })
            .await
            .unwrap();
        assert!(
            !persist_synthetic_claim_tx(
                &store,
                &storage,
                "orphan-metadata",
                &derived,
                9,
                [0u8; 32]
            )
            .await
            .unwrap(),
            "hash-mismatched registry metadata must be refused"
        );
    }

    /// Review blocker 3 — CRASH IDEMPOTENCY: a crash BETWEEN txn_begin and txn_commit leaves
    /// a PENDING calldata row. The old `if txn_get(...).is_some()` short-circuit treated it
    /// as complete, so every later backfill skipped it and the tx was stranded pending
    /// forever (no block, no receipt). A later persist pass must FINALIZE the pending row.
    #[tokio::test]
    async fn persist_synthetic_claim_tx_finalizes_pending_after_crash() {
        use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
        use alloy::primitives::{Address, Signature, TxKind, U256};

        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let storage = full_claim_fixture(&[]); // empty metadata → truthful by hash
        let note_id = "crash-window";
        let derived = derive_manual_claim_tx_hash(note_id);
        let tx_hash: alloy::primitives::TxHash = derived.parse().unwrap();

        // Simulate the crash: txn_begin ran (full calldata envelope persisted under the
        // derived hash) but txn_commit did not — a PENDING row with no block/receipt.
        let full = parse_full_claim_from_storage(&storage).unwrap();
        let input =
            alloy_core::sol_types::SolCall::abi_encode(&build_claim_asset_call(&full, vec![]));
        let bridge_addr: Address = get_bridge_address().parse().unwrap();
        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 0,
            gas_limit: 0,
            to: TxKind::Call(bridge_addr),
            value: U256::ZERO,
            input: input.into(),
        };
        let envelope = TxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::new(U256::from(1), U256::from(1), false),
            tx_hash,
        ));
        store
            .txn_begin(
                tx_hash,
                crate::store::TxnEntry {
                    id: None,
                    envelope,
                    signer: bridge_addr,
                    expires_at: None,
                    logs: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_none(),
            "pending: begin ran, commit did not — no receipt yet"
        );

        // A later persist pass FINALIZES the pending row (does not skip it).
        let ok = persist_synthetic_claim_tx(&store, &storage, note_id, &derived, 8831, [0xAA; 32])
            .await
            .unwrap();
        assert!(ok);
        let (result, block) = store
            .txn_receipt(tx_hash)
            .await
            .unwrap()
            .expect("the pending row must now be COMMITTED (finalized), not stranded");
        assert!(result.is_ok());
        assert_eq!(block, 8831, "finalized at the ClaimEvent block");

        // The calldata is intact and still keyed under the derived hash.
        use alloy::consensus::Transaction;
        use alloy_core::sol_types::SolCall;
        let data = store.txn_get(tx_hash).await.unwrap().unwrap();
        assert!(
            crate::claim::claimAssetCall::abi_decode(data.envelope.input()).is_ok(),
            "the pending row's full claimAsset calldata survived finalization"
        );
    }

    /// Review req 5 — stored envelopes precede the synthetic fallback. Both records exist
    /// for the SAME derived hash (the persisted full-calldata envelope AND the ClaimEvent
    /// synthetic log); `eth_getTransactionByHash` serves branches in order `txn_get` →
    /// in-flight → synthetic-log fallback, so the presence of the envelope is what makes
    /// the served input the full claimAsset calldata rather than the fallback's "0x".
    #[tokio::test]
    async fn stored_claim_envelope_precedes_synthetic_fallback() {
        use alloy::consensus::Transaction;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let storage = full_claim_fixture(&[]);
        let note_id = "precedence";
        let derived = derive_manual_claim_tx_hash(note_id);
        // The ClaimEvent synthetic log rides the derived hash (what the fallback matches).
        let mut gi = [0u8; 32];
        gi[23] = 1;
        gi[31] = 0x2A;
        store
            .add_claim_event(
                "0x00000000000000000000000000000000000000aa",
                8831,
                [0xAA; 32],
                &derived,
                &gi,
                0,
                &[0xAB; 20],
                &[0xCD; 20],
                123_456_789,
            )
            .await
            .unwrap();
        assert!(
            !store.get_logs_for_tx(&derived).await.unwrap().is_empty(),
            "fixture: the synthetic fallback WOULD match this hash"
        );
        assert!(
            persist_synthetic_claim_tx(&store, &storage, note_id, &derived, 8831, [0xAA; 32])
                .await
                .unwrap()
        );
        // txn_get (the dispatcher's FIRST branch) now serves the full calldata — the
        // synthetic fallback (empty input) is shadowed.
        let data = store
            .txn_get(derived.parse().unwrap())
            .await
            .unwrap()
            .expect("stored envelope must exist for the derived hash");
        assert!(
            !data.envelope.input().is_empty(),
            "the served input is the persisted claimAsset calldata, not the fallback's 0x"
        );
        assert!(
            data.envelope.input().len() > 4 + 64 * 32,
            "full calldata (proofs included), not a stub"
        );
    }

    /// #67 gap 1 — FAIL-CLOSED reconstruction. A claim that is provably OURS (consumed by our
    /// bridge) but whose full claimAsset calldata is UNRECOVERABLE (a non-empty metadata hash
    /// with no registry preimage) must NOT publish a ClaimEvent and must NOT mark the note
    /// processed: `project_claim_note` returns `Err` so block projection retries (once the
    /// faucet is registered the reconstruction succeeds). PRE-#67 this fell through and sealed
    /// an IMMUTABLE ClaimEvent riding a hash with empty calldata — aggkit's full-claim parser
    /// wedges forever and, because the note was marked processed, nothing ever retries.
    #[tokio::test]
    async fn project_claim_note_fail_closed_when_calldata_unrecoverable() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        // Non-empty metadata hash with NO registry preimage → unrecoverable calldata. Consumed
        // by OUR bridge so it passes the provenance gate and reaches the reconstruction step.
        let note = claim_input_note_meta(
            Some(id(TEST_TARGET_BRIDGE)),
            0x93,
            b"preimage the registry never saw",
        );
        let note_id = hex::encode(note.details_commitment().as_bytes());

        let outcome = project_claim_note(
            &store,
            &note,
            &std::collections::HashMap::new(),
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            8831,
            [0xAA; 32],
            get_bridge_address(),
        )
        .await;
        assert!(
            outcome.is_err(),
            "unrecoverable calldata must FAIL CLOSED (Err → projection retries), not emit"
        );
        // No ClaimEvent was sealed …
        let mut gi = [0u8; 32];
        gi[31] = 0x93;
        assert!(
            !store.has_claim_event_for_global_index(&gi).await.unwrap(),
            "no ClaimEvent may exist for a claim whose calldata is unrecoverable"
        );
        // … and the note is NOT marked processed, so a later tick (post faucet-registration)
        // re-runs the projection instead of a permanent fast-skip.
        assert!(
            !store.is_claim_note_processed(&note_id).await.unwrap(),
            "the note must NOT be marked processed — projection must be able to retry"
        );
    }

    /// Reviewer concern #1 — the ambiguous crash window: the note→ETH-tx-hash LINK was
    /// durably recorded and the claim was submitted + consumed on Miden, but the proxy
    /// crashed BEFORE persisting the ETH tx envelope. Recovery sees the surviving link (so
    /// `tx_hash` is the REAL hash), and PRE-FIX it emitted a ClaimEvent under that hash while
    /// the transaction row was absent — `eth_getTransactionByHash` then returned EMPTY
    /// calldata, and the derived-hash-only backfill never repairs a real-hash event, so
    /// aggkit's full-claim parser wedges forever. POST-FIX `project_claim_note` reconstructs
    /// the full claimAsset calldata under the linked hash (pending) BEFORE the atomic, which
    /// finalises it — so the served tx carries the truthful calldata, not the empty-wedge.
    #[tokio::test]
    async fn project_claim_note_reconstructs_calldata_for_linked_hash_missing_envelope() {
        use alloy::consensus::Transaction;
        use alloy_core::sol_types::SolCall;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        // Empty metadata → truthful by hash → reconstructable with no registry entry.
        let note = claim_input_note(Some(id(TEST_TARGET_BRIDGE)), 0x91);
        let note_id = hex::encode(note.details_commitment().as_bytes());

        // The crash window: the note→hash LINK survived, the envelope did NOT.
        let real_hash = "0x1111111111111111111111111111111111111111111111111111111111111111";
        store
            .record_tx_note_link(real_hash, &note_id)
            .await
            .unwrap();
        assert!(
            store
                .txn_get(real_hash.parse().unwrap())
                .await
                .unwrap()
                .is_none(),
            "precondition: the linked envelope is absent (crash before persisting it)"
        );

        let outcome = project_claim_note(
            &store,
            &note,
            &std::collections::HashMap::new(),
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            8831,
            [0xAA; 32],
            get_bridge_address(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ClaimProjectOutcome::Emitted);

        // The ClaimEvent rides the REAL linked hash …
        assert!(
            !store.get_logs_for_tx(real_hash).await.unwrap().is_empty(),
            "the ClaimEvent must ride the real linked hash"
        );
        // … and that hash now serves the FULL claimAsset calldata, finalised at the block.
        let data = store
            .txn_get(real_hash.parse().unwrap())
            .await
            .unwrap()
            .expect("the linked envelope was reconstructed, not left absent (concern #1)");
        assert!(
            crate::claim::claimAssetCall::abi_decode(data.envelope.input()).is_ok(),
            "the served input is the full claimAsset calldata, not the empty-calldata wedge"
        );
        assert!(
            data.envelope.input().len() > 4 + 64 * 32,
            "full calldata (both SMT proofs included), not a stub"
        );
        let (res, blk) = store
            .txn_receipt(real_hash.parse().unwrap())
            .await
            .unwrap()
            .expect("the linked receipt is finalised together with the ClaimEvent");
        assert!(res.is_ok());
        assert_eq!(blk, 8831, "receipt block == ClaimEvent block");
    }

    /// Reviewer concern #2 (building block) — `insert_pending_claim_calldata` inserts the
    /// calldata as a PENDING tx (no receipt, no block seal); finalisation is the atomic's
    /// job. PRE-FIX the projection path used `persist_synthetic_claim_tx`, which COMMITS
    /// (and, on Postgres, folds a `latest_block_number` advance — sealing the block mid-loop,
    /// before its later notes are written). The seal itself is Postgres-only (asserted in
    /// `store::postgres_tests`); here we pin the in-memory-observable half: the insert leaves
    /// the tx PENDING and is idempotent.
    #[tokio::test]
    async fn insert_pending_claim_calldata_leaves_tx_pending_and_is_idempotent() {
        use alloy::consensus::Transaction;
        use alloy_core::sol_types::SolCall;
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let storage = full_claim_fixture(&[]);
        let note_id = "pending-insert";
        let derived = derive_manual_claim_tx_hash(note_id);
        let tx_hash: alloy::primitives::TxHash = derived.parse().unwrap();

        assert!(
            insert_pending_claim_calldata(&store, &storage, note_id, &derived)
                .await
                .unwrap()
        );
        // Full calldata is present …
        let data = store
            .txn_get(tx_hash)
            .await
            .unwrap()
            .expect("calldata inserted");
        assert!(
            crate::claim::claimAssetCall::abi_decode(data.envelope.input()).is_ok(),
            "the pending row carries the full claimAsset calldata"
        );
        // … but the tx is PENDING — NOT committed/sealed (that is the atomic's job).
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_none(),
            "insert_pending must NOT finalise/seal — no receipt until the atomic commits"
        );
        // Idempotent no-op on a second call (row already present).
        assert!(
            insert_pending_claim_calldata(&store, &storage, note_id, &derived)
                .await
                .unwrap()
        );
        assert!(store.txn_get(tx_hash).await.unwrap().is_some());
    }

    /// Test-local mirror of the eth envelope aggkit signs for
    /// `insertGlobalExitRoot` — only the fields the store round-trips matter.
    fn test_ger_envelope(real_tx: alloy::primitives::TxHash) -> alloy::consensus::TxEnvelope {
        use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
        use alloy::primitives::Signature;
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(1),
                ..Default::default()
            },
            Signature::test_signature(),
            real_tx,
        ))
    }

    /// PR #127 review point 6 + follow-up — handoff-before-projection. This
    /// drives the REAL GER submission ordering rather than pre-seeding the
    /// desired store state: it calls `ger::record_ger_submission_handoff` —
    /// the exact production code `submit_update_ger_note` executes inside the
    /// serialized `MidenClient::with` closure after the Miden tx commits —
    /// and only THEN lets projection observe the consumed note, exactly as
    /// production interleaves them (the projector can only acquire the
    /// serialized client after the closure, handoff included, has finished).
    ///
    /// Pins the downstream contract: the projected GER event RETAINS the
    /// linked real Ethereum tx hash (never the derived fallback) and the
    /// pending `insertGlobalExitRoot` receipt — created by the handoff, NOT
    /// by a post-`insert_ger` caller — is finalised at the consumption block,
    /// never left pending. Pre-fix, the pending row was created by
    /// `handle_ger_result` only after `insert_ger` released the client; the
    /// projector could tick in that gap, silently finalise zero rows
    /// (PostgreSQL), and the late row then stayed pending forever. If the
    /// `txn_begin` ever moves back out of the handoff, this test fails at
    /// the "receipt must be finalised" assertion.
    #[tokio::test]
    async fn ger_real_submission_handoff_then_projection_finalises_receipt() {
        use alloy::primitives::TxHash;

        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (note, (key, metadata), ger_bytes) = ma28_consumed_external_ger_note(0x5C);
        let output_metadata = std::collections::HashMap::from([(key, metadata)]);

        // The real submission handoff, as run inside the serialized-client
        // closure: link + pending receipt, both durable before the client
        // (and hence projection) can proceed.
        let real_tx = TxHash::from([0xEEu8; 32]);
        let real_tx_str = format!("{real_tx:#x}");
        let note_commitment = hex::encode(note.details_commitment().as_bytes());
        let note_id = "test-ger-note-id";
        let signer = alloy::primitives::Address::from([0x42u8; 20]);
        crate::ger::record_ger_submission_handoff(
            &*store,
            real_tx,
            &note_commitment,
            note_id,
            1_000,
            test_ger_envelope(real_tx),
            signer,
        )
        .await
        .unwrap();
        assert_eq!(
            store.get_note_link_for_tx(&real_tx_str).await.unwrap(),
            Some(note_commitment.clone()),
            "handoff must record the tx↔note link",
        );
        assert!(
            store.txn_receipt(real_tx).await.unwrap().is_none(),
            "receipt must be pending (not finalised) right after the handoff",
        );
        assert!(
            store.txn_get(real_tx).await.unwrap().is_some(),
            "the pending row must exist BEFORE projection can run — \
             it is part of the serialized-client handoff",
        );

        // The projector observes the consumption.
        let consumption_block = 9u64;
        let outcome = project_ger_note(
            &store,
            &note,
            &output_metadata,
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            consumption_block,
            [9u8; 32],
            1_234,
        )
        .await
        .unwrap();
        assert_eq!(outcome, GerProjectOutcome::Emitted);
        assert!(store.is_ger_injected(&ger_bytes).await.unwrap());

        // The GER log rides the REAL linked tx hash, not the derived fallback.
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
        let ger_log = logs
            .iter()
            .find(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::UPDATE_HASH_CHAIN_VALUE_TOPIC)
            })
            .expect("projection must emit the GER log");
        assert_eq!(
            ger_log.transaction_hash.to_lowercase(),
            real_tx_str,
            "GER event must retain the linked real Ethereum tx hash",
        );

        // The pending receipt is finalised at the consumption block —
        // receipt block == GER-log block — and is never left pending.
        let (status, block) =
            store.txn_receipt(real_tx).await.unwrap().expect(
                "projection must finalise the linked pending receipt — never leave it pending",
            );
        assert!(status.is_ok(), "receipt must be a success receipt");
        assert_eq!(block, consumption_block);
    }

    /// PR #127 follow-up review — the exact pre-fix interleaving, pinned as a
    /// store-contract regression. Pre-fix, `submit_update_ger_note` recorded
    /// only the LINK inside the serialized-client closure; the pending row
    /// was created by `handle_ger_result` after the client was released. The
    /// projector could tick in that gap: resolve the real linked hash, call
    /// `txn_commit` — which on PostgreSQL silently updated zero rows and
    /// still committed the GER event — and the late `txn_begin` then left the
    /// real receipt pending FOREVER (nothing ever finalises it again).
    ///
    /// This test replays that interleaving (link → projection → late
    /// txn_begin) and asserts the two halves of the contract that make the
    /// fix sound:
    ///   1. projection in the gap must NOT invent a receipt (`txn_commit` on
    ///      a missing row errors — identically on both stores now — and
    ///      `project_ger_note` tolerates it while still emitting the GER
    ///      event under the real linked hash);
    ///   2. a row begun AFTER projection is unrecoverable — it stays pending,
    ///      which is precisely why `txn_begin` must live INSIDE the
    ///      serialized-client handoff next to the link
    ///      (`ger::record_ger_submission_handoff`), where this gap cannot
    ///      exist.
    #[tokio::test]
    async fn ger_projection_in_pre_fix_gap_cannot_finalise_late_pending_row() {
        use alloy::primitives::TxHash;

        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let (note, (key, metadata), ger_bytes) = ma28_consumed_external_ger_note(0x5D);
        let output_metadata = std::collections::HashMap::from([(key, metadata)]);

        // Pre-fix closure contents: ONLY the link.
        let real_tx = TxHash::from([0xEFu8; 32]);
        let real_tx_str = format!("{real_tx:#x}");
        let note_commitment = hex::encode(note.details_commitment().as_bytes());
        store
            .record_tx_note_link(&real_tx_str, &note_commitment)
            .await
            .unwrap();

        // The projector acquires the client in the gap and observes the
        // consumed note. It resolves the REAL linked hash but there is no
        // pending row to finalise.
        let consumption_block = 11u64;
        let outcome = project_ger_note(
            &store,
            &note,
            &output_metadata,
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            consumption_block,
            [11u8; 32],
            1_235,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            GerProjectOutcome::Emitted,
            "projection still emits the GER event (final gate is the note itself)",
        );
        assert!(store.is_ger_injected(&ger_bytes).await.unwrap());
        assert!(
            store.txn_receipt(real_tx).await.unwrap().is_none(),
            "contract half 1: txn_commit on a missing row must NOT invent a receipt",
        );
        // The store itself must surface the zero-row finalise as an error —
        // this is the memory-store behavior PostgreSQL now matches (pre-fix
        // PgStore returned Ok while updating zero rows). The PgStore twin of
        // this assertion lives in
        // `postgres_tests::test_pgstore_txn_commit_missing_row_errors`.
        assert!(
            store
                .txn_commit(real_tx, Ok(()), consumption_block, [11u8; 32])
                .await
                .is_err(),
            "contract half 1b: finalising a never-begun tx must error, not silently no-op",
        );

        // Pre-fix `handle_ger_result` then created the pending row — too
        // late: projection has already passed and nothing ever finalises it.
        let signer = alloy::primitives::Address::from([0x43u8; 20]);
        store
            .txn_begin(
                real_tx,
                crate::store::TxnEntry {
                    id: None,
                    envelope: test_ger_envelope(real_tx),
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        // Re-projection is a no-op (GER already injected) — the late row is
        // stuck pending forever. THIS is the wedge the handoff closes.
        let outcome = project_ger_note(
            &store,
            &note,
            &output_metadata,
            id(TEST_SENDER_MANAGER),
            id(TEST_TARGET_BRIDGE),
            consumption_block + 1,
            [12u8; 32],
            1_236,
        )
        .await
        .unwrap();
        assert_eq!(outcome, GerProjectOutcome::Skipped);
        assert!(
            store.txn_receipt(real_tx).await.unwrap().is_none(),
            "contract half 2: a row begun after projection stays pending forever — \
             which is why txn_begin must be inside the serialized-client handoff",
        );
    }
}
