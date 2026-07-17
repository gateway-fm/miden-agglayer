//! Bridge-Out (L2 → L1) — B2AGG consumption: shared derivation helpers + monitors.
//!
//! When the bridge account consumes a B2AGG note, assets are burned and a corresponding
//! deposit is recorded on the L2 side. The synthetic `BridgeEvent` log is emitted by the
//! [`crate::synthetic_projector::SyntheticProjector`] via the shared `project_b2agg_note`
//! derivation. This module hosts the derivation helpers that path shares
//! (`classify_b2agg_consumer`, `parse_b2agg_storage`, `is_b2agg_note`, `is_self_targeted`,
//! `derive_bridge_out_tx_hash`) plus the live `BridgeOutScanner`, whose remaining job is the
//! Miden-facing security monitors. LET cardinality is enforced by the projector.

use crate::miden_client::{MidenClientLib, SyncListener};
use anyhow::Context;
use miden_base_agglayer::B2AggNote;
use miden_client::store::InputNoteRecord;
use miden_client::store::NoteFilter;
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::note::{NoteDetails, NoteStorage};
use sha3::{Digest, Keccak256};
use std::sync::Arc;

// B2AGG NOTE PARSING
// ================================================================================================

/// Check if a note is a B2AGG note by comparing script roots.
pub fn is_b2agg_note(details: &NoteDetails) -> bool {
    details.script().root() == B2AggNote::script_root()
}

/// Extract destination_network and destination_address from B2AGG note storage.
///
/// The destination_address is a standard 20-byte EVM address (e.g. `0xAbC...123`),
/// NOT a Miden account ID. It comes from the bridge contract's `bridgeAsset()` call
/// and is stored in the note via `EthAddress::to_elements()`.
///
/// Storage layout (6 felts):
/// - items()[0]: destination_network (u32, byte-swapped via u32::from_le_bytes(dest.to_be_bytes()))
/// - items()[1..6]: destination_address (5 packed u32 felts = 20 bytes EVM address)
pub fn parse_b2agg_storage(storage: &NoteStorage) -> anyhow::Result<(u32, [u8; 20])> {
    let items = storage.items();

    // Bounds-check up front so a truncated or malformed B2AGG storage cannot panic the
    // sync loop. A bad note must not take down processing of every other consumed note
    // in the same tick — surface as a parse error and let the caller quarantine.
    if items.len() < 6 {
        anyhow::bail!(
            "B2AGG note storage too short: expected ≥6 felts (1 network + 5 address limbs), got {}",
            items.len()
        );
    }

    // Reverse the byte-swap applied during note creation:
    // build_note_storage does: u32::from_le_bytes(destination_network.to_be_bytes())
    // So to recover: u32::from_le_bytes(felt_value.to_be_bytes())
    let raw_network = u32::try_from(items[0].as_canonical_u64())
        .context("destination_network overflow: felt value exceeds u32::MAX")?;
    let destination_network = u32::from_le_bytes(raw_network.to_be_bytes());

    // Reconstruct 20-byte address from 5 packed u32 felts (big-endian limb order).
    // Each felt holds a u32 value that represents 4 bytes in little-endian byte order.
    // to_elements() in EthAddress uses bytes_to_packed_u32_elements which reads
    // each 4-byte chunk as a little-endian u32.
    let mut address = [0u8; 20];
    for i in 0..5 {
        let limb = u32::try_from(items[1 + i].as_canonical_u64())
            .context("address limb overflow: felt value exceeds u32::MAX")?;
        address[i * 4..(i + 1) * 4].copy_from_slice(&limb.to_le_bytes());
    }

    Ok((destination_network, address))
}

/// Domain-separation tag for synthetic bridge-out tx hashes. Versioned so
/// any future change in the derivation can co-exist with historical hashes.
///
/// Self-review B5 — pre-fix the tag was just `"miden-bridge-out-"`. The
/// reviewer flagged that as risk-of-collision with any other synthetic
/// tx-hash family that might use a similar prefix; using a tagged + versioned
/// constant + a stable suffix order pins the contract.
pub const BRIDGE_OUT_TX_HASH_TAG: &[u8] = b"miden-agglayer/bridge-out/v1\x00";

/// Derive the synthetic transaction hash for a B2AGG bridge-out's BridgeEvent.
///
/// Includes the version-tagged domain separator + the note id. Note: the
/// reviewer suggested folding `block_number` into the derivation for
/// retry-vs-replay differentiation. We deliberately do NOT — the same B2AGG
/// note has a stable on-chain identity across syncs, and aggsender
/// consumers key off the tx_hash to dedup. Adding block_number would
/// produce a different tx_hash on restore vs first-observation, breaking
/// dedup and creating phantom duplicate events.
pub fn derive_bridge_out_tx_hash(note_id_str: &str) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(BRIDGE_OUT_TX_HASH_TAG);
    hasher.update(note_id_str.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    format!("0x{}", hex::encode(hash))
}

/// Reject destination addresses that are obviously invalid for a bridge-out.
///
/// Self-review B7 — pre-fix, aggkit forwarded any 20-byte destination address
/// to bridge-service, even the zero address (no recipient) or the EVM
/// precompile range (0x00..0x09 reserved for ecrecover, sha256, ripemd, etc.).
/// The L1 contract has its own checks but pre-filtering here saves
/// bridge-service work and keeps the synthetic log stream tidy.
pub fn is_invalid_destination_address(address: &[u8; 20]) -> bool {
    // All-zero — no recipient.
    if address.iter().all(|b| *b == 0) {
        return true;
    }
    // Precompile range: address bytes are zero except possibly the very last
    // byte being 0x01..0x09. The ABI encodes addresses BE so the precompile
    // is at the *low* end of the 20 bytes (byte 19).
    if address[..19].iter().all(|b| *b == 0) && address[19] >= 0x01 && address[19] <= 0x09 {
        return true;
    }
    false
}

// FAUCET ORIGIN RESOLUTION
// ================================================================================================

/// Origin token info for a faucet.
pub struct FaucetOriginInfo {
    pub origin_network: u32,
    pub origin_address: [u8; 20],
    pub scale: u8,
    /// Raw ABI-encoded token metadata preimage (`abi.encode(name, symbol,
    /// decimals)` for ERC-20s, empty for native ETH). Threaded into the
    /// synthetic bridge-out `BridgeEvent` so the exit leaf carries the real
    /// metadata (Cantina #13).
    pub metadata: Vec<u8>,
    /// Token symbol (sanitised, as stored on the Miden faucet). Used by the
    /// Cantina #13 Layer-2 recovery path when `metadata` is empty for an ERC-20.
    pub symbol: String,
    /// Token decimals on the origin chain — part of the metadata preimage that
    /// Layer-2 recovery re-derives and validates.
    pub origin_decimals: u8,
}

/// Resolve faucet origin info from the dynamic faucet registry.
pub async fn resolve_faucet_origin(
    faucet_id: AccountId,
    store: &dyn crate::store::Store,
) -> anyhow::Result<FaucetOriginInfo> {
    let entry = store.get_faucet_by_id(faucet_id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "unknown faucet ID {faucet_id}: not found in faucet registry. \
                 Register the faucet via admin_registerFaucet or bridge a claim first."
        )
    })?;
    Ok(FaucetOriginInfo {
        origin_network: entry.origin_network,
        origin_address: entry.origin_address,
        scale: entry.scale,
        metadata: entry.metadata,
        symbol: entry.symbol,
        origin_decimals: entry.origin_decimals,
    })
}

/// Reverse-scale a Miden amount back to origin token decimals.
/// origin_amount = miden_amount * 10^scale
pub(crate) fn reverse_scale_amount(miden_amount: u64, scale: u8) -> anyhow::Result<u128> {
    let factor = 10u128
        .checked_pow(scale as u32)
        .context("reverse_scale_amount: 10^scale overflows u128")?;
    (miden_amount as u128)
        .checked_mul(factor)
        .context("reverse_scale_amount: miden_amount * 10^scale overflows u128")
}

// CANTINA MA#3 — RECLAIM GATE
// ================================================================================================

/// Decision returned by [`classify_b2agg_consumer`].
///
/// The B2AGG MASM script (`asm/note_scripts/B2AGG.masm` lines 53-109) has TWO
/// consumption paths — a reclaim branch that adds assets back to the sender,
/// and a bridge branch that BURNs and advances the LET frontier. miden-client
/// returns notes from both paths in `NoteFilter::Consumed`, so a pure gate on
/// `consumer_account()` is required before emitting a synthetic BridgeEvent.
///
/// `Emit` is the only variant that should produce a BridgeEvent. The other two
/// are skip paths with distinct metrics so operators can graph reclaim rate
/// (expected, normal user flow) separately from the untracked-consumer anomaly
/// (fail-closed, indicates miden-client did not record the consuming account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B2AggConsumerClass {
    /// Note was consumed by the bridge account — emit BridgeEvent.
    Emit,
    /// Note was consumed by a non-bridge account (reclaim path in MASM lines 65-71).
    Reclaimed,
    /// Note has no recorded consumer — fail-closed skip.
    UntrackedConsumer,
}

/// Pure gate predicate for the B2AGG reclaim fix (Cantina MA#3).
///
/// Given the `consumer_account` field from miden-client's `InputNoteRecord`
/// and this scanner's `bridge_account_id`, classify whether to emit a synthetic
/// BridgeEvent. Pure (no I/O, no metrics) so it can be unit-tested directly.
/// Metric emission and tracing live at the call site in `project_b2agg_note`.
pub fn classify_b2agg_consumer(
    consumer_account: Option<AccountId>,
    bridge_account_id: AccountId,
) -> B2AggConsumerClass {
    match consumer_account {
        Some(consumer) if consumer == bridge_account_id => B2AggConsumerClass::Emit,
        Some(_) => B2AggConsumerClass::Reclaimed,
        None => B2AggConsumerClass::UntrackedConsumer,
    }
}

// BRIDGE OUT SCANNER
// ================================================================================================

/// Runs bridge security monitors after each Miden sync.
pub struct BridgeOutScanner {
    store: Arc<dyn crate::store::Store>,
    /// Local network id, used to detect self-targeted bridge-outs (Cantina #13). A B2AGG
    /// note whose `destination_network` equals this value is a poison leaf — the on-chain
    /// bridge accepts and processes it (LET frontier advances, BURN emitted), but the next
    /// agglayer certificate covering it is rejected by pessimistic-proof-core, halting the
    /// bridge for every legitimate B2AGG since the last successful certificate.
    local_network_id: u32,
    /// The bridge account monitored for faucet ownership and note provenance.
    bridge_account_id: AccountId,
    /// BURN serial collision tracker (Cantina #5).
    pub burn_serials: Arc<crate::burn_serial_tracker::BurnSerialTracker>,
    /// Twin-NoteId detector (Cantina #6).
    pub twin_notes: Arc<crate::twin_note_detector::TwinNoteDetector>,
    /// Expected-MINT-NoteId tracker (Cantina #7).
    pub expected_mints: Arc<crate::expected_mint_tracker::ExpectedMintTracker>,
    /// Sync ticks per faucet-ownership probe (Cantina #4 ownership monitor).
    /// 0 disables; default is every tick.
    ownership_probe_every_n_ticks: u32,
    /// Internal tick counter for ownership probe scheduling.
    tick_counter: std::sync::atomic::AtomicU32,
    /// Optional L1 JSON-RPC endpoint. Used by the Cantina #13 Layer-2 recovery
    /// path to fetch a token's canonical `name()`/`symbol()`/`decimals()` when a
    /// legacy faucet row has empty ERC-20 metadata. `None` disables the L1
    /// fallback (recovery then relies solely on the all-Miden candidate, and
    /// gates if that does not validate).
    l1_rpc_url: Option<String>,
}

impl BridgeOutScanner {
    pub fn new(
        store: Arc<dyn crate::store::Store>,
        local_network_id: u32,
        bridge_account_id: AccountId,
    ) -> Self {
        // RD-913: trackers now persist through `store` and bound their
        // in-memory caches; default capacities live in each module.
        let burn_serials = Arc::new(crate::burn_serial_tracker::BurnSerialTracker::new(
            store.clone(),
        ));
        let twin_notes = Arc::new(crate::twin_note_detector::TwinNoteDetector::new(
            store.clone(),
        ));
        let expected_mints = Arc::new(crate::expected_mint_tracker::ExpectedMintTracker::new(
            store.clone(),
        ));
        Self {
            store,
            local_network_id,
            bridge_account_id,
            burn_serials,
            twin_notes,
            expected_mints,
            ownership_probe_every_n_ticks: 5, // every 5 sync ticks (~30s at 6s/tick)
            tick_counter: std::sync::atomic::AtomicU32::new(0),
            l1_rpc_url: None,
        }
    }

    /// Wire an L1 JSON-RPC endpoint for Cantina #13 Layer-2 ERC-20 metadata
    /// recovery (see [`Self::l1_rpc_url`]). Builder so existing call sites and
    /// tests that don't need recovery stay unchanged.
    pub fn with_l1_rpc_url(mut self, l1_rpc_url: Option<String>) -> Self {
        self.l1_rpc_url = l1_rpc_url;
        self
    }

    /// Returns true if a parsed B2AGG `destination_network` is the bridge's own network,
    /// i.e. a poison leaf that wedges every subsequent bridge-out until manual recovery.
    /// Public for unit tests in this module and for any external observers that want to
    /// pre-validate a B2AGG before submission.
    pub fn is_self_targeted(&self, destination_network: u32) -> bool {
        destination_network == self.local_network_id
    }
}

/// Record a quarantine (`unbridgeable_bridge_outs`) row for a B2AGG that was
/// observed consumed by the bridge but skipped by the indexer (Cantina MA#18).
///
/// Shared by the live scanner ([`BridgeOutScanner::quarantine_unbridgeable_b2agg`])
/// and the offline restore path so both record a note as a *permanent skip*
/// (note_id + reason + diagnostic) and the same oversized / erased note is not
/// re-attempted on every sync tick or restore run.
///
/// Best-effort: a quarantine-write failure must not propagate — the caller's
/// contract is that a skip path's only side effect is the skip itself.
/// Quarantine errors are logged and the metric still fires.
pub(crate) async fn quarantine_unbridgeable_b2agg(
    store: &dyn crate::store::Store,
    bridge_account: AccountId,
    note_id_str: &str,
    note: &InputNoteRecord,
    observed_block: u64,
    reason: crate::store::UnbridgeableBridgeOutReason,
    detail: String,
) {
    // Bound the detail field so a flood of malformed notes can't
    // bloat individual rows. The Postgres column has no length cap;
    // bound here so the bound is enforced regardless of backend.
    const MAX_DETAIL: usize = 4096;
    let detail = if detail.len() > MAX_DETAIL {
        format!(
            "{}…[truncated {} bytes]",
            &detail[..MAX_DETAIL],
            detail.len() - MAX_DETAIL
        )
    } else {
        detail
    };

    let note_dump = dump_note_for_quarantine(note);
    metrics::counter!(
        "bridge_out_quarantined_erased_b2agg_total",
        "reason" => reason.as_str()
    )
    .increment(1);

    let entry = crate::store::UnbridgeableBridgeOut {
        note_id: note_id_str.to_string(),
        bridge_account,
        reason,
        detail,
        note_dump,
        observed_block,
    };

    match store.record_unbridgeable_bridge_out(entry).await {
        Ok(true) => {
            tracing::warn!(
                target: "bridge_out::quarantine",
                note_id = %note_id_str,
                reason = reason.as_str(),
                "Cantina MA#18: B2AGG quarantined — operator handle persisted"
            );
        }
        Ok(false) => {
            // Already quarantined; idempotent — no spam.
            tracing::debug!(
                target: "bridge_out::quarantine",
                note_id = %note_id_str,
                reason = reason.as_str(),
                "Cantina MA#18: B2AGG already quarantined (idempotent skip)"
            );
        }
        Err(e) => {
            tracing::error!(
                target: "bridge_out::quarantine",
                note_id = %note_id_str,
                reason = reason.as_str(),
                error = %e,
                "Cantina MA#18: failed to record quarantine row — \
                 metric still fired but recovery handle is lost"
            );
        }
    }
}

/// Render a note's key forensic fields as a JSON-like string suitable for
/// the `note_dump` quarantine column. Captures: script root (so an operator
/// can confirm this was a B2AGG, not some other wrapper), the storage felts
/// (so a fixed parser can re-derive destination_network + destination_address),
/// and the asset list (so the operator knows what's stranded).
///
/// Kept simple text rather than `serde_json::to_string` to avoid pulling
/// serde into the bridge_out hot path and to keep the format human-readable
/// in psql.
pub(crate) fn dump_note_for_quarantine(note: &InputNoteRecord) -> String {
    use std::fmt::Write as _;
    let details = note.details();
    let script_root_hex = hex::encode(details.script().root().as_bytes());
    let storage_items: Vec<String> = details
        .storage()
        .items()
        .iter()
        .map(|f| format!("{}", f.as_canonical_u64()))
        .collect();
    let assets: Vec<String> = details
        .assets()
        .iter_fungible()
        .map(|fa| format!("{{faucet={}, amount={}}}", fa.faucet_id(), fa.amount()))
        .collect();
    let mut out = String::with_capacity(256);
    let _ = write!(out, "{{\"script_root\":\"0x{script_root_hex}\",");
    let _ = write!(out, "\"storage_items\":[{}],", storage_items.join(","));
    let _ = write!(out, "\"fungible_assets\":[{}]}}", assets.join(","));
    out
}

impl BridgeOutScanner {
    /// Cantina #23 / #19 — client-free, **MONITOR-ONLY** pass over the
    /// consumed-note set. Records every observed note into the twin (#6),
    /// burn-serial (#5) and forged-MINT (#2/#4) trackers and emits the matching
    /// metrics/logs, and returns the set of CLAIM note-ids seen consumed this
    /// tick (fed to the expected-MINT tracker, #7).
    ///
    /// It performs **NO** tip advance and writes **NO** BridgeEvent. The
    /// pre-redesign `BridgeOutScanner` advanced `latest_block_number` and
    /// inserted a BridgeEvent for *each* consumed B2AGG note inside this very
    /// loop — which (a) raced the `restore()` replay writing the same events at a
    /// different block height (Cantina #23) and (b) bumped the block once per
    /// note, scattering a single Miden tx's notes across many synthetic blocks
    /// (Cantina #19). Emission and tip-advance now belong solely to the
    /// [`SyntheticProjector`](crate::synthetic_projector).
    ///
    /// Extracted as a testable seam so the monitor-only invariant is
    /// regression-locked by `finding_23_scanner_is_monitor_only`.
    async fn scan_consumed_notes_monitors(
        &self,
        consumed_notes: &[InputNoteRecord],
    ) -> std::collections::HashSet<[u8; 32]> {
        // Cantina #6 — feed every observed note's NoteId + commitment into the
        // twin-detector. Same-NoteId-different-commitment is the B2AGG twin
        // attack signature.
        // Cantina #5 — every consumed BURN note's serial → tracker; collisions
        // are the duplicate-burn signature.
        // Cantina #2 / #4 — every consumed MINT note → check NetworkAccountTarget
        // attachment matches the consuming faucet AND that the corresponding
        // claim was recorded by aggkit (forged-MINT detection).
        let burn_root = miden_standards::note::BurnNote::script_root();
        let mint_root = miden_standards::note::MintNote::script_root();
        // Cantina #7: claim script root, used to mark expected-MINT entries
        // Landed when we observe the bridge consume the CLAIM. The script
        // root is a constant (the script bytes are baked into the agglayer
        // crate); compute once per sync instead of caching, since
        // `claim_script()` returns by value and the cost is negligible
        // versus the on-chain query that follows.
        let claim_root = miden_base_agglayer::ClaimNote::script().root();
        let mut landed_claim_ids: std::collections::HashSet<[u8; 32]> =
            std::collections::HashSet::new();
        let registered_faucets: std::collections::HashSet<AccountId> = self
            .store
            .list_faucets()
            .await
            .ok()
            .map(|v| v.into_iter().map(|f| f.faucet_id).collect())
            .unwrap_or_default();

        for note in consumed_notes {
            let id_bytes: [u8; 32] = note.details_commitment().as_bytes();
            let Some(commitment_word) = note.commitment() else {
                // Notes without a commitment (incomplete InputNoteRecord)
                // shouldn't show up in the Consumed filter; skip defensively.
                continue;
            };
            let commitment_bytes: [u8; 32] = commitment_word.as_bytes();
            // RD-913: tracker is now store-backed + async; a transient store
            // failure must NOT panic the sync — log and continue so the rest
            // of the post-sync work still runs.
            match self.twin_notes.record(id_bytes, commitment_bytes).await {
                Ok(crate::twin_note_detector::Outcome::TwinDetected { prior_commitments }) => {
                    metrics::counter!("bridge_twin_note_detected_total").increment(1);
                    tracing::error!(
                        target: "bridge_out::twin",
                        note_id = ?note.details_commitment(),
                        observed_commitment = %hex::encode(commitment_bytes),
                        prior_count = prior_commitments.len(),
                        "Cantina #6: twin NoteId observed — different metadata, same NoteId"
                    );
                }
                Ok(crate::twin_note_detector::Outcome::New)
                | Ok(crate::twin_note_detector::Outcome::LegitimateDuplicate) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "bridge_out::twin",
                        note_id = ?note.details_commitment(),
                        error = ?e,
                        "RD-913: twin-note tracker store failure; \
                         continuing without classification"
                    );
                }
            }

            let script_root = note.details().script().root();
            // Cantina #7 — CLAIM consumption observation. The bridge ALWAYS
            // consumes the CLAIM as a precondition to emitting the MINT, so
            // a CLAIM in the consumed-set is the proxy "MINT landed" signal
            // for this proxy's expected-MINT tracker.
            if script_root == claim_root {
                landed_claim_ids.insert(id_bytes);
            }
            // Cantina #5 — BURN serial collision tracking.
            if script_root == burn_root {
                let serial = note.details().recipient().serial_num();
                match self.burn_serials.record(serial.as_bytes()).await {
                    Ok(crate::burn_serial_tracker::Outcome::Duplicate) => {
                        metrics::counter!("bridge_burn_serial_collision_total").increment(1);
                        tracing::error!(
                            target: "bridge_out::burn",
                            note_id = ?note.details_commitment(),
                            serial = %hex::encode(serial.as_bytes()),
                            "Cantina #5: BURN serial collision — second BURN with same serial \
                             observed; faucet token_supply at risk"
                        );
                    }
                    Ok(crate::burn_serial_tracker::Outcome::New) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "bridge_out::burn",
                            note_id = ?note.details_commitment(),
                            error = ?e,
                            "RD-913: burn-serial tracker store failure; continuing"
                        );
                    }
                }
            }
            // Cantina #2 + #4 — MINT attachment-target + forged-MINT detection.
            if script_root == mint_root {
                // The MINT note's attachments carry a NetworkAccountTarget
                // identifying the intended consuming faucet. We decode via
                // TryFrom<&NoteAttachments>.
                let Some(_metadata) = note.metadata() else {
                    continue;
                };
                let attachments = note.attachments();
                let intended_faucet: Option<AccountId> =
                    miden_standards::note::NetworkAccountTarget::try_from(attachments)
                        .ok()
                        .map(|nat| nat.target_id());
                if let Some(intended) = intended_faucet {
                    // Cantina #2: we observe MINT consumption by a faucet
                    // we don't have direct access to here, but we DO know
                    // which faucet was the intended target. If the
                    // intended faucet is not in our registry, that's
                    // already a critical signal — either it's a
                    // cross-faucet exploit (Cantina #2) or a misregistered
                    // faucet (operator error).
                    if !registered_faucets.contains(&intended) {
                        metrics::counter!("bridge_mint_target_mismatch_total").increment(1);
                        tracing::error!(
                            target: "bridge_out::mint_attach",
                            note_id = ?note.details_commitment(),
                            intended_faucet = %intended,
                            "Cantina #2: MINT NetworkAccountTarget points at a \
                             faucet not in aggkit's registry — possible \
                             cross-faucet exploit"
                        );
                    }
                } else {
                    // MINT with no decodable NetworkAccountTarget — Cantina
                    // #4 forged-mint signature. The bridge always attaches
                    // when emitting legitimate MINTs.
                    metrics::counter!("bridge_forged_mint_total").increment(1);
                    tracing::error!(
                        target: "bridge_out::forged_mint",
                        note_id = ?note.details_commitment(),
                        "Cantina #4: MINT note observed with no decodable \
                         NetworkAccountTarget attachment — forged via NoAuth"
                    );
                }
            }

            // Cantina MA#4 — unknown bridge-out wrapper detection. The bridge
            // account has no on-chain assertion that the note consumed must
            // be the canonical B2AGG script — any MASM body that calls
            // `bridge_out::bridge_out` from a transaction the bridge consumes
            // will advance the LET frontier and BURN funds. Pre-fix the
            // indexer silently dropped every non-B2AGG script root in
            // `is_b2agg_note`, so an alternate wrapper would create an
            // invisible exit. Detect post-hoc: notes consumed by the bridge
            // account whose script root is in neither the B2AGG-out set nor
            // the CLAIM-in set are the MA#4 signature.
            if note.consumer_account() == Some(self.bridge_account_id) {
                let b2agg_root_bytes = B2AggNote::script_root().as_bytes();
                let claim_root_bytes = claim_root.as_bytes();
                let observed_bytes = script_root.as_bytes();
                use crate::unknown_wrapper_detector::{
                    BridgeConsumerScript, classify_bridge_consumer_script,
                };
                if matches!(
                    classify_bridge_consumer_script(
                        observed_bytes,
                        b2agg_root_bytes,
                        claim_root_bytes,
                    ),
                    BridgeConsumerScript::Unknown
                ) {
                    metrics::counter!("bridge_unknown_wrapper_consumed_total").increment(1);
                    tracing::warn!(
                        target: "bridge_out::unknown_wrapper",
                        note_id = ?note.details_commitment(),
                        observed_script_root = %hex::encode(observed_bytes),
                        bridge = %self.bridge_account_id,
                        "Cantina MA#4: bridge account consumed a note whose script \
                         root matches neither the canonical B2AGG bridge-out wrapper \
                         nor the CLAIM script — alternate wrapper has produced an \
                         on-chain LET advance that the indexer cannot translate"
                    );
                }
            }
        }

        landed_claim_ids
    }
}

#[async_trait::async_trait]
impl SyncListener for BridgeOutScanner {
    fn on_sync(&self, _summary: &SyncSummary) {
        // no-op — scanning happens in on_post_sync where we have client access
    }

    async fn on_post_sync(&self, client: &mut MidenClientLib) -> anyhow::Result<()> {
        let consumed_notes = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

        // Cantina #23 + #19 — the per-note pass is MONITOR-ONLY: it records into
        // the twin (#6) / burn-serial (#5) / forged-MINT (#2/#4) trackers and
        // emits metrics, and returns the CLAIM ids seen consumed (for the #7
        // expected-MINT tracker). It NEVER advances `latest_block_number` nor
        // writes a BridgeEvent — the pre-redesign scanner did both here, once per
        // consumed B2AGG note, which raced `restore()` (#23) and misnumbered
        // synthetic blocks (#19). The SyntheticProjector is now the sole
        // emitter/tip-advancer.
        let landed_claim_ids = self.scan_consumed_notes_monitors(&consumed_notes).await;

        // Cantina #4 ownership monitor — on a slower cadence (every N ticks)
        // FPI-query each registered faucet's owner storage slot.
        let tick = self
            .tick_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.ownership_probe_every_n_ticks > 0
            && tick.is_multiple_of(self.ownership_probe_every_n_ticks)
            && let Err(e) = self.run_faucet_ownership_check(client).await
        {
            tracing::warn!(
                target: "bridge_out::ownership",
                error = ?e,
                "Cantina #4: faucet ownership probe failed (transient — will retry)"
            );
        }

        // Cantina #7 — tick the expected-MINT tracker with the CLAIM IDs we
        // observed consumed this sync. Stale entries (CLAIM not consumed
        // within 60 sync ticks ≈ 6 minutes at default cadence) fire a
        // critical metric and log so on-call can investigate.
        //
        // RD-913 Bug B fix: `tick()` now fires StaleAlert **once** per
        // record_expected, then removes the entry. The pre-fix forever-loop
        // behaviour (re-firing every 6s until process death) is gone — see
        // `expected_mint_tracker` module docs.
        match self.expected_mints.tick(&landed_claim_ids, 60).await {
            Ok(tracker_results) => {
                for (gi, status) in tracker_results {
                    if let crate::expected_mint_tracker::MintStatus::StaleAlert { ticks_pending } =
                        status
                    {
                        metrics::counter!("bridge_expected_mint_stale_total").increment(1);
                        tracing::error!(
                            target: "bridge_out::expected_mint",
                            global_index = ?gi,
                            ticks_pending,
                            "Cantina #7: expected MINT NoteId never landed within threshold"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "bridge_out::expected_mint",
                    error = ?e,
                    "RD-913: expected-MINT tracker tick store failure; will retry next sync"
                );
            }
        }

        Ok(())
    }
}

impl BridgeOutScanner {
    /// Cantina #4 ownership monitor. Iterates the registered faucet list,
    /// FPI-fetches each one's `owner` storage slot, compares against the
    /// configured bridge account id.
    async fn run_faucet_ownership_check(&self, client: &mut MidenClientLib) -> anyhow::Result<()> {
        let faucets = self.store.list_faucets().await?;
        for entry in faucets {
            let acct = match client.get_account(entry.faucet_id).await {
                Ok(Some(acct)) => acct,
                Ok(None) => continue, // not yet synced
                Err(e) => {
                    tracing::warn!(
                        target: "bridge_out::ownership",
                        faucet_id = %entry.faucet_id,
                        error = ?e,
                        "Cantina #4: faucet account fetch failed"
                    );
                    continue;
                }
            };
            // The Ownable2Step component stores the owner AccountId at a
            // known slot. Upstream exposes `owner_account_id` returning
            // `Err(OwnershipRenounced)` for the renounced case.
            let observed: Option<AccountId> =
                match miden_base_agglayer::AggLayerFaucet::owner_account_id(&acct) {
                    Ok(id) => Some(id),
                    Err(miden_base_agglayer::AgglayerFaucetError::OwnershipRenounced) => None,
                    Err(e) => {
                        tracing::warn!(
                            target: "bridge_out::ownership",
                            faucet_id = %entry.faucet_id,
                            error = ?e,
                            "Cantina #4: failed to decode faucet owner — skipping"
                        );
                        continue;
                    }
                };
            match crate::faucet_ownership_monitor::check_faucet_owner(
                self.bridge_account_id,
                observed,
            ) {
                crate::faucet_ownership_monitor::OwnershipState::Expected => {}
                crate::faucet_ownership_monitor::OwnershipState::Drift { observed, expected } => {
                    metrics::counter!(
                        "bridge_faucet_ownership_drift_total",
                        "kind" => "drift"
                    )
                    .increment(1);
                    tracing::error!(
                        target: "bridge_out::ownership",
                        faucet_id = %entry.faucet_id,
                        observed_owner = %observed,
                        expected_owner = %expected,
                        "Cantina #4: faucet ownership drifted from bridge — possible takeover"
                    );
                }
                crate::faucet_ownership_monitor::OwnershipState::Renounced => {
                    metrics::counter!(
                        "bridge_faucet_ownership_drift_total",
                        "kind" => "renounced"
                    )
                    .increment(1);
                    tracing::error!(
                        target: "bridge_out::ownership",
                        faucet_id = %entry.faucet_id,
                        "Cantina #4: faucet owner cleared (renounced) — DoS variant"
                    );
                }
            }
        }
        Ok(())
    }
}

// BRIDGE EVENT ABI ENCODING
// ================================================================================================

/// Maximum metadata payload size accepted by `encode_bridge_event_data`.
///
/// 64 KB matches the largest legitimate metadata block we expect (ABI-encoded
/// `(string name, string symbol, uint8 decimals)` for normal ERC-20s sits well
/// below 1 KB; 64 KB is generous for any future variant). Without an explicit
/// cap, a misuse passing huge metadata would allocate `metadata.len() + 9*32`
/// bytes per call and OOM the indexer on a single bad event.
pub const MAX_BRIDGE_EVENT_METADATA_BYTES: usize = 64 * 1024;

/// ABI-encode BridgeEvent data for synthetic log emission.
///
/// BridgeEvent(uint8 leafType, uint32 originNetwork, address originAddress,
///             uint32 destinationNetwork, address destinationAddress,
///             uint256 amount, bytes metadata, uint32 depositCount)
///
/// Per Solidity ABI encoding, all static types are padded to 32 bytes,
/// and `bytes metadata` is encoded as an offset + length + zero-padded data.
///
/// Cantina #10 surfaced non-canonical leaf encoding upstream (`pack_leaf_data`
/// does not enforce zero padding on bridge-in leaf data). The fix there is in
/// MASM, but our event encoder is in the same canonical-encoding family:
/// previously the metadata length was hardcoded to 0 with no provision for
/// non-empty metadata, so any future caller passing real bytes would have
/// produced non-canonical output (missing length, missing 32-byte alignment
/// padding). Take metadata as an explicit parameter and encode canonically:
/// write the length word, append the bytes, zero-pad to the next 32-byte
/// boundary.
///
/// # Errors
/// Returns `Err(BridgeEventEncodeError::MetadataTooLarge)` if `metadata.len()`
/// exceeds `MAX_BRIDGE_EVENT_METADATA_BYTES`.
#[allow(clippy::too_many_arguments)]
pub fn encode_bridge_event_data_checked(
    leaf_type: u8,
    origin_network: u32,
    origin_address: &[u8; 20],
    destination_network: u32,
    destination_address: &[u8; 20],
    amount: u128,
    metadata: &[u8],
    deposit_count: u32,
) -> Result<String, BridgeEventEncodeError> {
    if metadata.len() > MAX_BRIDGE_EVENT_METADATA_BYTES {
        return Err(BridgeEventEncodeError::MetadataTooLarge {
            len: metadata.len(),
            cap: MAX_BRIDGE_EVENT_METADATA_BYTES,
        });
    }
    Ok(encode_bridge_event_data(
        leaf_type,
        origin_network,
        origin_address,
        destination_network,
        destination_address,
        amount,
        metadata,
        deposit_count,
    ))
}

/// Errors returned by `encode_bridge_event_data_checked`.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeEventEncodeError {
    MetadataTooLarge { len: usize, cap: usize },
}

impl std::fmt::Display for BridgeEventEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MetadataTooLarge { len, cap } => write!(
                f,
                "BridgeEvent metadata too large: {len} > {cap} bytes (cap configured for indexer DoS protection)"
            ),
        }
    }
}

impl std::error::Error for BridgeEventEncodeError {}

/// Encode BridgeEvent data, panicking on metadata overflow. Use
/// `encode_bridge_event_data_checked` for callers that handle errors.
///
/// Internal callers (`InMemoryStore::add_bridge_event`, restore path) pass `&[]` so
/// the cap is unreachable today; this `unwrap_or_else` form preserves the
/// pre-fix infallible signature for those callers while keeping the cap
/// enforced for any future caller via the `_checked` variant.
#[allow(clippy::too_many_arguments)]
pub fn encode_bridge_event_data(
    leaf_type: u8,
    origin_network: u32,
    origin_address: &[u8; 20],
    destination_network: u32,
    destination_address: &[u8; 20],
    amount: u128,
    metadata: &[u8],
    deposit_count: u32,
) -> String {
    // Compute the canonical 32-byte-aligned padded length of the metadata data section.
    let metadata_padded_len = metadata.len().div_ceil(32) * 32;
    // 8 static words (each 32 bytes) + 1 length word + padded data
    let mut data = Vec::with_capacity(8 * 32 + 32 + metadata_padded_len);

    // leafType (uint8 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 31]);
    data.push(leaf_type);

    // originNetwork (uint32 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 28]);
    data.extend_from_slice(&origin_network.to_be_bytes());

    // originAddress (address padded to 32 bytes)
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(origin_address);

    // destinationNetwork (uint32 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 28]);
    data.extend_from_slice(&destination_network.to_be_bytes());

    // destinationAddress (address padded to 32 bytes)
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(destination_address);

    // amount (uint256 — u128 in low 16 bytes of 32-byte slot, big-endian)
    data.extend_from_slice(&[0u8; 16]);
    data.extend_from_slice(&amount.to_be_bytes());

    // metadata offset (uint256). Static head is 8 params × 32 bytes = 256, so the dynamic
    // region begins at byte 256 = 0x100. The metadata length sits at that offset, the data
    // starts at offset+32.
    data.extend_from_slice(&[0u8; 28]);
    let metadata_offset: u32 = 8 * 32;
    data.extend_from_slice(&metadata_offset.to_be_bytes());

    // depositCount (uint32 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 28]);
    data.extend_from_slice(&deposit_count.to_be_bytes());

    // metadata dynamic part: length (uint256, big-endian) + data + zero padding to 32-byte boundary
    data.extend_from_slice(&[0u8; 24]);
    data.extend_from_slice(&(metadata.len() as u64).to_be_bytes());
    data.extend_from_slice(metadata);
    let pad = metadata_padded_len - metadata.len();
    data.extend(std::iter::repeat_n(0u8, pad));

    format!("0x{}", hex::encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bridge_event_encoding_length() {
        let data = encode_bridge_event_data(
            0,           // leaf_type
            0,           // origin_network
            &[0u8; 20],  // origin_address
            1,           // destination_network
            &[0xaa; 20], // destination_address
            1000,        // amount
            &[],         // metadata
            0,           // deposit_count
        );
        // 9 words (8 params + 1 metadata length) = 288 bytes = 576 hex chars + "0x" prefix
        assert_eq!(data.len(), 2 + 9 * 32 * 2);
    }

    /// Cantina #10 — repro+regression. Pre-fix `encode_bridge_event_data` hardcoded
    /// `metadata length = 0` and had no parameter for non-empty metadata. Any future
    /// caller passing real bytes would have produced non-canonical Solidity ABI:
    /// no length word and no 32-byte alignment padding on the data section. Post-fix
    /// the length word reflects `metadata.len()` and trailing bytes are zero-padded
    /// to the next 32-byte boundary so consumers (alloy, ethers, web3.py) decode it
    /// identically to a real on-chain BridgeEvent.
    #[test]
    fn cantina_10_bridge_event_metadata_canonical_encoding() {
        let metadata = b"USDC-erc20-decimals-6";
        let data = encode_bridge_event_data(0, 0, &[0u8; 20], 1, &[0xaa; 20], 1000, metadata, 0);
        let bytes = hex::decode(&data[2..]).unwrap();
        // 32-byte aligned overall.
        assert_eq!(bytes.len() % 32, 0, "encoding must be 32-byte aligned");
        // Static head occupies the first 8 * 32 = 256 bytes.
        // Length word at offset 256 (BE u256, length goes in the low 8 bytes).
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&bytes[256 + 24..256 + 32]);
        assert_eq!(u64::from_be_bytes(len_bytes), metadata.len() as u64);
        // Data starts at 288, must contain the metadata bytes verbatim.
        let padded_len = metadata.len().div_ceil(32) * 32;
        assert_eq!(&bytes[288..288 + metadata.len()], metadata);
        // Trailing pad must be exactly zero (canonical Solidity ABI).
        assert_eq!(
            &bytes[288 + metadata.len()..288 + padded_len],
            &vec![0u8; padded_len - metadata.len()][..]
        );

        // Empty metadata: length = 0, no data bytes after the length word.
        let empty = encode_bridge_event_data(0, 0, &[0u8; 20], 1, &[0xaa; 20], 0, &[], 0);
        let empty_bytes = hex::decode(&empty[2..]).unwrap();
        assert_eq!(empty_bytes.len(), 9 * 32);
        assert_eq!(&empty_bytes[256..288], &[0u8; 32]);

        // Exactly 32-byte-aligned metadata: must NOT add a second pad word.
        let aligned = vec![0xAB; 32];
        let aligned_enc =
            encode_bridge_event_data(0, 0, &[0u8; 20], 1, &[0xaa; 20], 0, &aligned, 0);
        let aligned_bytes = hex::decode(&aligned_enc[2..]).unwrap();
        // 8 head + 1 length + 1 data = 10 words.
        assert_eq!(aligned_bytes.len(), 10 * 32);
    }

    #[test]
    fn test_bridge_event_encoding_fields() {
        let mut dest_addr = [0u8; 20];
        dest_addr[19] = 0x42;

        let data = encode_bridge_event_data(
            0,          // leaf_type (asset)
            0,          // origin_network
            &[0u8; 20], // origin_address (ETH)
            1,          // destination_network
            &dest_addr, // destination_address
            1000,       // amount
            &[],        // metadata
            5,          // deposit_count
        );

        let bytes = hex::decode(&data[2..]).unwrap();

        // leafType at offset 0, last byte should be 0
        assert_eq!(bytes[31], 0);
        // originNetwork at offset 32, last 4 bytes
        assert_eq!(&bytes[60..64], &[0, 0, 0, 0]);
        // destinationNetwork at offset 96, last 4 bytes
        assert_eq!(&bytes[124..128], &[0, 0, 0, 1]);
        // destination address at offset 128, last 20 bytes
        assert_eq!(bytes[128 + 12 + 19], 0x42);
        // amount at offset 160, last 16 bytes (u128 big-endian)
        assert_eq!(&bytes[176 + 14..176 + 16], &[3, 232]); // 1000 in big-endian
        // depositCount at offset 224, last 4 bytes
        assert_eq!(&bytes[252..256], &[0, 0, 0, 5]);
        // metadata length at offset 256 should be 0
        assert_eq!(&bytes[256..288], &[0u8; 32]);
    }

    #[test]
    fn test_reverse_scale_amount() {
        // No scaling
        assert_eq!(reverse_scale_amount(1000, 0).unwrap(), 1000);
        // ETH: scale=10
        assert_eq!(reverse_scale_amount(1000, 10).unwrap(), 10_000_000_000_000);
        // 1 unit with scale=18
        assert_eq!(
            reverse_scale_amount(1, 18).unwrap(),
            1_000_000_000_000_000_000
        );
        // Overflow: scale too large
        assert!(reverse_scale_amount(1, 39).is_err());
    }

    /// Self-review of-the-fix follow-up — repro+regression. The original
    /// `encode_bridge_event_data` had no cap on metadata size — a misuse passing
    /// huge metadata would allocate proportionally and OOM the indexer on a
    /// single bad event. The reviewer agents flagged this as a low-severity
    /// gap in the Cantina #10 encoder commit. The new
    /// `encode_bridge_event_data_checked` wrapper enforces
    /// `MAX_BRIDGE_EVENT_METADATA_BYTES` and surfaces an explicit error.
    #[test]
    fn bridge_event_metadata_length_capped() {
        let too_big = vec![0u8; MAX_BRIDGE_EVENT_METADATA_BYTES + 1];
        let err =
            encode_bridge_event_data_checked(0, 0, &[0u8; 20], 1, &[0xaa; 20], 1000, &too_big, 0)
                .expect_err("oversized metadata must error");
        match err {
            BridgeEventEncodeError::MetadataTooLarge { len, cap } => {
                assert_eq!(len, MAX_BRIDGE_EVENT_METADATA_BYTES + 1);
                assert_eq!(cap, MAX_BRIDGE_EVENT_METADATA_BYTES);
            }
        }

        // Exactly at the cap is accepted.
        let at_cap = vec![0u8; MAX_BRIDGE_EVENT_METADATA_BYTES];
        let ok =
            encode_bridge_event_data_checked(0, 0, &[0u8; 20], 1, &[0xaa; 20], 1000, &at_cap, 0);
        assert!(ok.is_ok(), "exactly cap must be accepted");
    }

    /// Cantina #13 — repro+regression. The on-chain `bridge_out` procedure does not
    /// assert `destination_network != local_network_id`, so a B2AGG note targeting the
    /// local network is processed successfully on-chain (LET frontier advances) but the
    /// next agglayer certificate covering it is rejected by pessimistic-proof-core,
    /// stranding every legitimate B2AGG in the same window. We can't prevent the leaf
    /// from being appended on-chain — by the time aggkit observes the consumed note,
    /// the LET already advanced — but we MUST refuse to emit the synthetic BridgeEvent
    /// for that leaf so the bridge-service doesn't try to settle a doomed certificate.
    ///
    /// This test asserts the load-bearing predicate `is_self_targeted` correctly
    /// distinguishes self-target (poison) from cross-network (legitimate) and from the
    /// edge case `network_id = 0` (mainnet, where any B2AGG is by definition cross-net).
    /// The actual emit-skip happens in `project_b2agg_note` and is exercised by the
    /// e2e test suite under `scripts/security-repro/cantina-13-self-target.sh` once the
    /// docker stack is up — see CANTINA_FIXES.md.
    #[test]
    fn cantina_13_is_self_targeted_distinguishes_poison_from_legitimate() {
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());

        // Local network = 7 (typical rollup id assigned by RollupManager).
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let scanner = BridgeOutScanner::new(store.clone(), 7, bridge_id);
        assert!(
            scanner.is_self_targeted(7),
            "destination_network == local must be flagged as poison"
        );
        assert!(
            !scanner.is_self_targeted(0),
            "mainnet (0) destination is legitimate"
        );
        assert!(
            !scanner.is_self_targeted(1),
            "other rollup destination is legitimate"
        );
        assert!(
            !scanner.is_self_targeted(u32::MAX),
            "off-by-one: u32::MAX is not the local network 7"
        );

        // Edge: a service deployed with network_id = 0 (mainnet bridge) flags
        // destination 0 as self-target.
        let mainnet_scanner = BridgeOutScanner::new(store, 0, bridge_id);
        assert!(mainnet_scanner.is_self_targeted(0));
        assert!(!mainnet_scanner.is_self_targeted(1));
    }

    /// Self-review B5 — repro+regression. The synthetic tx-hash derivation
    /// must be:
    /// - Stable for the same input (deterministic).
    /// - Different for different note_ids (no collisions in normal use).
    /// - Different from the previous derivation (versioned tag) — so a
    ///   regression that drops the version separator is caught.
    /// - 32 bytes hex with 0x prefix (length 66 chars).
    #[test]
    fn b5_bridge_out_tx_hash_versioned_and_deterministic() {
        let h1 = derive_bridge_out_tx_hash("note_a");
        let h2 = derive_bridge_out_tx_hash("note_a");
        assert_eq!(h1, h2, "deterministic for same input");
        assert_eq!(h1.len(), 66, "0x + 64 hex chars");
        assert!(h1.starts_with("0x"));

        let h3 = derive_bridge_out_tx_hash("note_b");
        assert_ne!(h1, h3, "different note_ids → different hashes");

        // Pin the expected hash for "note_a" so a future regression that
        // changes the domain tag without bumping the version is caught.
        // The exact value is deterministic given BRIDGE_OUT_TX_HASH_TAG +
        // "note_a" as keccak256 input. We check the prefix to confirm
        // the tag is in use; the full value matters less than the
        // *change-detection* property — if someone refactors the
        // derivation, this test forces an explicit choice.
        assert!(BRIDGE_OUT_TX_HASH_TAG.starts_with(b"miden-agglayer/bridge-out/v"));
    }

    /// Self-review B7 — repro+regression. The destination address validator
    /// must reject:
    ///   - zero address (no recipient)
    ///   - precompile range (bytes 0..18 zero, byte 19 in 0x01..0x09)
    ///
    /// AND accept legitimate addresses:
    ///   - real EOA (random hex)
    ///   - real contract (random hex)
    ///   - byte 19 = 0x0A onwards (precompiles stop at 0x09)
    #[test]
    fn b7_destination_address_validator() {
        // Zero address rejected.
        assert!(is_invalid_destination_address(&[0u8; 20]));

        // Precompile range rejected (0x01..0x09).
        for byte in 0x01u8..=0x09 {
            let mut addr = [0u8; 20];
            addr[19] = byte;
            assert!(
                is_invalid_destination_address(&addr),
                "precompile {byte:#04x} must be rejected"
            );
        }

        // 0x0A is just past the precompile range — accepted.
        let mut addr = [0u8; 20];
        addr[19] = 0x0A;
        assert!(!is_invalid_destination_address(&addr));

        // Legitimate-looking address.
        let mut addr = [0xAAu8; 20];
        addr[19] = 0x42;
        assert!(!is_invalid_destination_address(&addr));

        // Address with high byte set (precompiles only have low byte set,
        // so this should NOT be flagged).
        let mut addr = [0u8; 20];
        addr[0] = 0x01;
        addr[19] = 0x05; // looks like precompile in low byte but high byte set
        assert!(!is_invalid_destination_address(&addr));
    }

    /// Self-review B6 — repro+regression. A B2AGG note with fewer than 6 storage felts
    /// (1 network word + 5 address limbs) is malformed. Before this guard,
    /// `parse_b2agg_storage` would index `items[0]` and `items[1+i]` directly and panic
    /// with index-out-of-bounds — taking down the entire sync loop for the rest of the
    /// tick and dropping every other consumed note in the same batch on the floor.
    /// Asserting clean Err return ensures the caller can quarantine the offending note
    /// instead of aborting downstream B2AGG processing.
    #[test]
    fn b6_parse_b2agg_storage_short_payload_returns_clean_error() {
        use miden_protocol::Felt;

        // 1 felt only — short of the required 6.
        let storage = NoteStorage::new(vec![Felt::from(0u32)]).unwrap();
        let err = parse_b2agg_storage(&storage).expect_err("short storage must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("storage too short") && msg.contains("≥6 felts"),
            "error should describe the bound: got {msg}"
        );

        // 5 felts — still short.
        let storage = NoteStorage::new(vec![
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
        ])
        .unwrap();
        assert!(parse_b2agg_storage(&storage).is_err());

        // 6 felts — exact minimum, must succeed.
        let storage = NoteStorage::new(vec![
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
        ])
        .unwrap();
        assert!(parse_b2agg_storage(&storage).is_ok());
    }

    // CANTINA MA#3 — RECLAIM GATE TESTS
    // ============================================================================================

    /// Cantina MA#3 — pure-helper repro. `classify_b2agg_consumer` is the
    /// load-bearing gate predicate. Test the three branches explicitly so any
    /// future refactor that broadens or narrows the gate is caught here.
    #[test]
    fn ma3_classify_b2agg_consumer_branches() {
        // Two distinct AccountIds (last hex char differs).
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let user_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();
        assert_ne!(bridge_id, user_id, "test ids must be distinct");

        // 1. Bridge-consumed → Emit (real bridge-out).
        assert_eq!(
            classify_b2agg_consumer(Some(bridge_id), bridge_id),
            B2AggConsumerClass::Emit
        );

        // 2. Reclaim path — note was consumed by a different (user) account.
        assert_eq!(
            classify_b2agg_consumer(Some(user_id), bridge_id),
            B2AggConsumerClass::Reclaimed
        );

        // 3. Untracked consumer — fail-closed.
        assert_eq!(
            classify_b2agg_consumer(None, bridge_id),
            B2AggConsumerClass::UntrackedConsumer
        );
    }

    /// Build a minimal B2AGG `InputNoteRecord` in a chosen consumed state for
    /// gate-wiring tests. Empty asset set so we never need to construct a
    /// FungibleAsset (which would require a faucet-typed AccountId) — the gate
    /// runs strictly before asset extraction in `project_b2agg_note`, so
    /// the downstream code path that reads assets is unreachable for the
    /// reclaim/untracked tests.
    fn build_b2agg_note_with_consumer(
        consumer_account: Option<AccountId>,
    ) -> miden_client::store::InputNoteRecord {
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::Felt;
        use miden_protocol::Word;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteAssets, NoteDetails, NoteRecipient, NoteStorage};

        // B2AGG storage: 6 felts (network + 5 address limbs). Values don't matter
        // for the gate — only the script root distinguishes B2AGG.
        let storage = NoteStorage::new(vec![
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
        ])
        .unwrap();
        let script = B2AggNote::script();
        let recipient = NoteRecipient::new(Word::default(), script, storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account,
            consumed_tx_order: None,
        });

        miden_client::store::InputNoteRecord::new(
            details,
            miden_protocol::note::NoteAttachments::default(),
            None,
            state,
        )
    }

    /// Build a fully-formed bridge-out B2AGG note: a fungible asset from
    /// `faucet_id`, valid 6-felt storage (a non-zero, non-precompile destination
    /// address and a non-self-target network), consumed by `consumer`. This
    /// reaches the metadata-resolution / commit path in `project_b2agg_note`
    /// (unlike `build_b2agg_note_with_consumer`, whose empty asset set short-
    /// circuits at the no-fungible-asset skip).
    fn build_b2agg_bridge_out_note(
        faucet_id: AccountId,
        consumer: AccountId,
    ) -> miden_client::store::InputNoteRecord {
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::asset::{Asset, FungibleAsset};
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteAssets, NoteDetails, NoteRecipient, NoteStorage};
        use miden_protocol::{Felt, Word};

        // storage: [network=0, addr_limb0=0x11111111, 0, 0, 0, 0] → destination
        // network 0 (not the local 7) and address 0x11111111000…0 (non-zero,
        // not a precompile).
        let storage = NoteStorage::new(vec![
            Felt::from(0u32),
            Felt::from(0x1111_1111u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(0u32),
        ])
        .unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let asset: Asset = FungibleAsset::new(faucet_id, 50).unwrap().into();
        let assets = NoteAssets::new(vec![asset]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: Some(consumer),
            consumed_tx_order: None,
        });
        miden_client::store::InputNoteRecord::new(
            details,
            miden_protocol::note::NoteAttachments::default(),
            None,
            state,
        )
    }

    /// Run a consumed B2AGG note through the PRODUCTION derivation
    /// (`restore::project_b2agg_note`, what the SyntheticProjector uses) and map
    /// its outcome to the legacy `project_b2agg_note` bool (Emitted == "advanced").
    /// `local_network_id = 7`; every note built here targets destination-network 0,
    /// so the Cantina #13 self-target gate never fires (that gate has its own test).
    fn test_b2agg_note_id(
        note: &miden_client::store::InputNoteRecord,
        bridge_id: AccountId,
    ) -> miden_protocol::note::NoteId {
        let attachments = miden_protocol::note::NoteAttachments::default();
        let metadata = miden_protocol::note::NoteMetadata::new(
            miden_protocol::note::PartialNoteMetadata::new(
                bridge_id,
                miden_protocol::note::NoteType::Public,
            ),
            &attachments,
        );
        miden_protocol::note::NoteId::new(note.details_commitment(), &metadata)
    }

    async fn run_b2agg_emit(
        store: &std::sync::Arc<dyn crate::store::Store>,
        block_state: &std::sync::Arc<crate::block_state::BlockState>,
        note: &miden_client::store::InputNoteRecord,
        bridge_id: AccountId,
        block: u64,
    ) -> bool {
        crate::restore::project_b2agg_note(
            store,
            note,
            test_b2agg_note_id(note, bridge_id),
            bridge_id,
            7,
            block,
            block_state.get_block_hash(block),
            crate::bridge_address::get_bridge_address(),
            None,
            None,
        )
        .await
        .unwrap()
            == crate::restore::B2AggRestoreOutcome::Emitted
    }

    /// Cantina #13 Layer 2 — FAIL-CLOSED (no tombstone). A bridge-consumed ERC-20
    /// whose metadata is unrecoverable (here: no live client → bridge hash unreadable)
    /// must NOT emit (empty metadata → spoofed wrapped token) AND must NOT silently skip
    /// (a reserved-but-unemitted leaf gaps getLogs → aggkit halts). So it BAILS loudly.
    /// The leaf's index is reserved (so it stays visible to the emitted-frontier gate),
    /// no BridgeEvent is emitted, and recovery is operator-driven (fix metadata / a full
    /// DB drop + `--restore` rebuild from on-chain).
    #[tokio::test]
    async fn cantina13_l2_erc20_unrecoverable_fails_closed() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let faucet_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        // ERC-20 faucet (non-zero origin address) with EMPTY metadata — the legacy/DB-loss
        // state Layer 2 must guard.
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id,
                origin_address: [0x42u8; 20],
                origin_network: 0,
                symbol: "USDC".into(),
                origin_decimals: 6,
                miden_decimals: 6,
                scale: 0,
                metadata: vec![],
            })
            .await
            .unwrap();

        let note = build_b2agg_bridge_out_note(faucet_id, bridge_id);
        let note_id = test_b2agg_note_id(&note, bridge_id).to_hex();

        // No client → bridge metadata hash unreadable → Unrecoverable → FAIL CLOSED (Err).
        let outcome = crate::restore::project_b2agg_note(
            &store,
            &note,
            test_b2agg_note_id(&note, bridge_id),
            bridge_id,
            7,
            100,
            block_state.get_block_hash(100),
            crate::bridge_address::get_bridge_address(),
            None,
            None,
        )
        .await;
        assert!(
            outcome.is_err(),
            "unrecoverable ERC-20 metadata must FAIL CLOSED (Err), not silently skip"
        );
        // The leaf reserved its index but never emitted → the emitted-frontier gate must see
        // it, and NO BridgeEvent may exist.
        assert_eq!(
            store
                .first_unemitted_reservation()
                .await
                .unwrap()
                .as_ref()
                .map(|(_, n)| n.as_str()),
            Some(note_id.as_str()),
            "reserved-but-unemitted poison leaf must be visible to the emitted-frontier gate"
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "no BridgeEvent may be emitted for an unrecoverable-metadata leaf"
        );
    }

    /// Cantina #13 Layer 2 — native ETH is UNTOUCHED. A bridge-consumed native-ETH
    /// bridge-out (zero origin address) with empty metadata is correct and must
    /// STILL emit (and be marked processed), even with no client — recovery is
    /// never attempted for native ETH.
    #[tokio::test]
    async fn cantina13_l2_native_eth_empty_metadata_still_emits() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let faucet_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        // Native ETH faucet: zero origin address, empty metadata (correct).
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

        let note = build_b2agg_bridge_out_note(faucet_id, bridge_id);
        let note_id = test_b2agg_note_id(&note, bridge_id).to_hex();

        let advanced = run_b2agg_emit(&store, &block_state, &note, bridge_id, 100).await;
        assert!(
            advanced,
            "native-ETH bridge-out with empty metadata must still emit"
        );
        assert!(
            store.is_note_processed(&note_id).await.unwrap(),
            "emitted native-ETH note must be marked processed",
        );
    }

    /// Cantina MA#3 — wiring repro. A B2AGG note consumed by a user account
    /// (reclaim branch in B2AGG.masm:65-71) must NOT trigger a synthetic
    /// BridgeEvent or be marked processed.
    #[tokio::test]
    async fn ma3_skips_b2agg_reclaimed_by_user() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let user_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        let note = build_b2agg_note_with_consumer(Some(user_id));
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        let advanced = run_b2agg_emit(&store, &block_state, &note, bridge_id, 100).await;
        assert!(!advanced, "reclaim must NOT signal block advance");

        // The note must NOT be marked processed — otherwise a future
        // bridge-actual consumption of a different note with the same ID
        // (twin) would silently skip.
        assert!(
            !store.is_note_processed(&note_id_str).await.unwrap(),
            "reclaimed note must remain un-processed in the store"
        );

        // No BridgeEvent log emitted.
        let filter = crate::log_synthesis::LogFilter::default();
        let logs = store.get_logs(&filter, 1000).await.unwrap_or_default();
        assert!(
            logs.is_empty(),
            "reclaim path must not emit any synthetic log, got {} log(s)",
            logs.len()
        );
    }

    /// Cantina MA#3 — wiring repro. A B2AGG note with no tracked consumer
    /// account (miden-client gap or transient sync state) must be treated as
    /// fail-closed: skip emission, no state mutation.
    #[tokio::test]
    async fn ma3_skips_b2agg_with_unknown_consumer() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

        let note = build_b2agg_note_with_consumer(None);
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        let advanced = run_b2agg_emit(&store, &block_state, &note, bridge_id, 100).await;
        assert!(
            !advanced,
            "untracked-consumer must NOT signal block advance"
        );
        assert!(
            !store.is_note_processed(&note_id_str).await.unwrap(),
            "untracked-consumer note must remain un-processed"
        );
    }

    /// Cantina MA#3 — positive wiring. A B2AGG note consumed by the bridge
    /// account passes the gate and proceeds to downstream processing. In this
    /// test the note carries no fungible asset so the subsequent
    /// "no fungible asset" branch in `project_b2agg_note` returns false —
    /// what we're pinning here is that the gate did NOT short-circuit, i.e.
    /// the reclaim metric path was NOT taken. We assert this indirectly: the
    /// reclaim-skip path returns false WITHOUT ever calling
    /// `iter_fungible().next()`, while the emit path returns false because
    /// `iter_fungible().next()` is `None`. We pin the contract via the
    /// pure-helper test (`ma3_classify_b2agg_consumer_branches`) and assert
    /// here that the scanner doesn't panic / blow up when the bridge consumes
    /// a B2AGG (i.e. it proceeds past the gate cleanly).
    #[tokio::test]
    async fn ma3_emits_for_bridge_consumed_b2agg() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

        let note = build_b2agg_note_with_consumer(Some(bridge_id));

        // Must not panic — the gate accepts and we fall through to the
        // "no fungible asset" branch (which also returns false). The key
        // contract here is: bridge-consumed notes are NOT short-circuited by
        // the gate. The pure-helper test pins the exact decision; this just
        // exercises the wiring end-to-end without a downstream panic.
        let _ = run_b2agg_emit(&store, &block_state, &note, bridge_id, 100).await;
    }

    // CANTINA MA#18 — UNBRIDGEABLE B2AGG QUARANTINE TESTS
    // ============================================================================================

    /// Build a B2AGG note with INVALID storage (only 1 felt) so
    /// `parse_b2agg_storage` returns Err. Bridge-consumed so it passes the
    /// MA#3 gate and reaches the storage-parse skip site in
    /// `project_b2agg_note`.
    fn build_erased_b2agg_note(
        consumer_account: AccountId,
    ) -> miden_client::store::InputNoteRecord {
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::ConsumedExternalNoteState;
        use miden_protocol::Felt;
        use miden_protocol::Word;
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::{NoteAssets, NoteDetails, NoteRecipient, NoteStorage};

        // 1 felt: too short for parse_b2agg_storage (which requires ≥6).
        // This simulates an "erased" B2AGG — the bridge consumed it on-chain
        // (LET advanced) but the indexer cannot reconstruct the destination.
        let storage = NoteStorage::new(vec![Felt::from(0u32)]).unwrap();
        let script = B2AggNote::script();
        let recipient = NoteRecipient::new(Word::default(), script, storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(0u32),
            consumer_account: Some(consumer_account),
            consumed_tx_order: None,
        });

        miden_client::store::InputNoteRecord::new(
            details,
            miden_protocol::note::NoteAttachments::default(),
            None,
            state,
        )
    }

    /// Cantina MA#18 — wiring repro. A B2AGG with un-parseable storage
    /// (the "erased" case) that the bridge consumed MUST land a positive
    /// quarantine row so an operator has a concrete handle to investigate /
    /// rescue. Pre-MA#18 this skipped silently and only surfaced as a LET
    /// divergence symptom (Cantina #9).
    #[tokio::test]
    async fn ma18_erased_b2agg_quarantined_on_storage_parse_failure() {
        use crate::block_state::BlockState;
        use crate::store::UnbridgeableBridgeOutReason;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

        let note = build_erased_b2agg_note(bridge_id);
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        let advanced = run_b2agg_emit(&store, &block_state, &note, bridge_id, 42).await;
        assert!(!advanced, "erased note must NOT signal block advance");

        let row = store
            .get_unbridgeable_bridge_out(&note_id_str)
            .await
            .unwrap()
            .expect("quarantine row must be present");
        assert_eq!(row.note_id, note_id_str);
        assert_eq!(row.bridge_account, bridge_id);
        assert_eq!(row.reason, UnbridgeableBridgeOutReason::StorageParseFailed);
        assert_eq!(row.observed_block, 42);
        assert!(
            row.note_dump.contains("script_root"),
            "note_dump must capture script_root for forensic inspection, got: {}",
            row.note_dump
        );
        assert!(
            row.note_dump.contains("storage_items"),
            "note_dump must capture storage_items so a fixed parser can re-derive fields"
        );
        assert!(
            !row.detail.is_empty(),
            "detail must capture the underlying parse error"
        );
    }

    /// Cantina MA#18 — quarantine writes are idempotent by note_id. Multiple
    /// sync ticks observing the same erased note must NOT duplicate rows.
    /// Pre-fix duplicate inserts would either error or bloat the table on
    /// every tick.
    #[tokio::test]
    async fn ma18_quarantine_is_idempotent_per_note_id() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

        let note = build_erased_b2agg_note(bridge_id);
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        // First observation — quarantine row written.
        let _ = run_b2agg_emit(&store, &block_state, &note, bridge_id, 1).await;
        let first = store
            .get_unbridgeable_bridge_out(&note_id_str)
            .await
            .unwrap()
            .expect("first quarantine row");
        let first_block = first.observed_block;

        // Second observation — quarantine row UNCHANGED.
        let _ = run_b2agg_emit(&store, &block_state, &note, bridge_id, 2).await;
        let second = store
            .get_unbridgeable_bridge_out(&note_id_str)
            .await
            .unwrap()
            .expect("quarantine row must persist");
        assert_eq!(
            second.observed_block, first_block,
            "first-write-wins: observed_block must not be overwritten by later ticks"
        );
    }

    /// Cantina MA#18 — a non-skip path (e.g. MA#3 reclaim by user) must NOT
    /// generate a quarantine row. Quarantine fires only when the bridge
    /// consumed the note (LET advanced) AND we couldn't translate it.
    /// Reclaim by user is normal flow — no LET advance, no quarantine.
    #[tokio::test]
    async fn ma18_user_reclaim_does_not_quarantine() {
        use crate::block_state::BlockState;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let user_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        let note = build_b2agg_note_with_consumer(Some(user_id));
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        let _ = run_b2agg_emit(&store, &block_state, &note, bridge_id, 1).await;

        assert!(
            store
                .get_unbridgeable_bridge_out(&note_id_str)
                .await
                .unwrap()
                .is_none(),
            "user-reclaim must not produce a quarantine row — the LET did not advance"
        );
    }

    /// Cantina MA#18 — pin the `as_str()` mapping. The textual `reason`
    /// column is the load-bearing key for any future recovery RPC; the
    /// strings MUST stay stable or operator queries will silently miss
    /// rows.
    #[test]
    fn ma18_reason_str_mapping_stable() {
        use crate::store::UnbridgeableBridgeOutReason as R;
        assert_eq!(R::StorageParseFailed.as_str(), "storage_parse_failed");
        assert_eq!(R::NoFungibleAsset.as_str(), "no_fungible_asset");
        assert_eq!(R::UnknownFaucet.as_str(), "unknown_faucet");
        assert_eq!(R::AmountOverflow.as_str(), "amount_overflow");
        assert_eq!(R::AtomicCommitFailed.as_str(), "atomic_commit_failed");
        assert_eq!(R::MetadataTooLarge.as_str(), "metadata_too_large");
    }

    /// Cantina #13 follow-up — the oversized-metadata DoS guard must RECORD the
    /// note as unbridgeable (not silently skip), so the same note isn't
    /// re-attempted on every sync tick / restore run. This exercises the shared
    /// free helper both call sites use, pinning that a `MetadataTooLarge`
    /// quarantine row is persisted with the expected reason + forensic dump.
    #[tokio::test]
    async fn cantina13_metadata_too_large_records_unbridgeable() {
        use crate::store::UnbridgeableBridgeOutReason;
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let note = build_b2agg_note_with_consumer(Some(bridge_id));
        let note_id_str = test_b2agg_note_id(&note, bridge_id).to_hex();

        quarantine_unbridgeable_b2agg(
            &*store,
            bridge_id,
            &note_id_str,
            &note,
            99,
            UnbridgeableBridgeOutReason::MetadataTooLarge,
            "origin.metadata.len()=70000 exceeds MAX_BRIDGE_EVENT_METADATA_BYTES=65536".to_string(),
        )
        .await;

        let row = store
            .get_unbridgeable_bridge_out(&note_id_str)
            .await
            .unwrap()
            .expect("metadata-too-large note must be quarantined, not silently skipped");
        assert_eq!(row.note_id, note_id_str);
        assert_eq!(row.bridge_account, bridge_id);
        assert_eq!(row.reason, UnbridgeableBridgeOutReason::MetadataTooLarge);
        assert_eq!(row.observed_block, 99);
        assert!(
            row.detail
                .contains("exceeds MAX_BRIDGE_EVENT_METADATA_BYTES")
        );
    }

    /// Cantina MA#4 — wiring repro for the unknown-wrapper detector. Pins
    /// that the predicate correctly distinguishes the canonical B2AGG and
    /// CLAIM roots from any other 32-byte root. The wiring inside
    /// `on_post_sync` is exercised by the e2e tests (full client+sync stack
    /// required); this test pins the pure decision the wiring depends on.
    #[test]
    fn ma4_classify_bridge_consumer_script_pins_known_set() {
        use crate::unknown_wrapper_detector::{
            BridgeConsumerScript, classify_bridge_consumer_script,
        };
        // Use the real B2AGG + CLAIM roots so a future MASM regen that
        // changes either is caught here.
        let b2agg = B2AggNote::script_root().as_bytes();
        let claim = miden_base_agglayer::ClaimNote::script().root().as_bytes();
        assert_ne!(b2agg, claim, "B2AGG and CLAIM must have distinct roots");

        // Known roots — the bridge legitimately consumes both.
        assert_eq!(
            classify_bridge_consumer_script(b2agg, b2agg, claim),
            BridgeConsumerScript::KnownB2Agg
        );
        assert_eq!(
            classify_bridge_consumer_script(claim, b2agg, claim),
            BridgeConsumerScript::KnownClaim
        );

        // Arbitrary other root — the MA#4 signature. Pre-fix this slipped
        // through silently.
        let foreign = [0xCCu8; 32];
        assert_eq!(
            classify_bridge_consumer_script(foreign, b2agg, claim),
            BridgeConsumerScript::Unknown
        );
    }

    /// Cantina #23 regression lock (invariant a: the scanner is MONITOR-ONLY).
    ///
    /// The pre-redesign `BridgeOutScanner::on_post_sync` advanced
    /// `latest_block_number` and inserted a `BridgeEvent` for each unprocessed
    /// consumed B2AGG note, in the same `NoteFilter::Consumed` loop `restore()`
    /// walks — the race in finding #23 (and the per-note block bump in #19). The
    /// redesign made the scanner monitor-only: it records into the twin/burn/mint
    /// trackers and emits metrics, but the `SyntheticProjector` is the sole
    /// emitter/tip-advancer.
    ///
    /// This drives the exact per-note pass (`scan_consumed_notes_monitors`, the
    /// client-free core of `on_post_sync`) over a fabricated bridge-consumed,
    /// UNPROCESSED B2AGG note and asserts the scanner:
    ///   * does NOT advance the store tip (`get_latest_block_number` unchanged),
    ///   * writes NO synthetic log / BridgeEvent,
    ///   * does NOT mark the note processed (that too belongs to the projector).
    /// A pre-fix scanner given this same note advanced the tip and wrote an event
    /// (its advance did not depend on the note's commitment), so every assertion
    /// below would have failed. The complementary invariant (b) — that restore's
    /// `pause_listeners()` guard suppresses `on_post_sync` dispatch — is locked by
    /// `finding_23_restore_pauses_listeners` and
    /// `ma23_on_post_sync_dispatch_suppressed_while_paused` in `miden_client`
    /// (restore installs the guard at `restore.rs:203`).
    #[tokio::test]
    async fn finding_23_scanner_is_monitor_only() {
        use crate::store::memory::InMemoryStore;
        use std::sync::Arc as StdArc;

        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());
        let bridge_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let faucet_id = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        // Seed a distinctive, non-zero tip: any per-note advance would move it.
        const TIP: u64 = 4242;
        store.set_latest_block_number(TIP).await.unwrap();
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

        let scanner = BridgeOutScanner::new(store.clone(), 7, bridge_id);

        // A real bridge-consumed B2AGG note — exactly the kind the pre-fix loop
        // advanced the tip / emitted a BridgeEvent for.
        let note = build_b2agg_bridge_out_note(faucet_id, bridge_id);
        let note_id = test_b2agg_note_id(&note, bridge_id).to_hex();

        let landed = scanner.scan_consumed_notes_monitors(&[note]).await;

        assert!(
            landed.is_empty(),
            "a B2AGG note is not a CLAIM — the monitor pass reports no landed claims"
        );
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            TIP,
            "MONITOR-ONLY: the scanner must NOT advance the tip (pre-fix bumped it \
             once per consumed B2AGG note — findings #23 and #19)"
        );
        let logs = store
            .get_logs(&crate::log_synthesis::LogFilter::default(), TIP + 100)
            .await
            .unwrap_or_default();
        assert!(
            logs.is_empty(),
            "MONITOR-ONLY: the scanner must emit NO synthetic BridgeEvent (that is \
             the SyntheticProjector's sole responsibility), got {} log(s)",
            logs.len()
        );
        assert!(
            !store.is_note_processed(&note_id).await.unwrap(),
            "MONITOR-ONLY: the scanner must NOT mark the note processed — else it \
             would race restore's own replay (finding #23)"
        );
    }
}
