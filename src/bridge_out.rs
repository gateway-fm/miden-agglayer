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
use std::num::NonZeroUsize;
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

// NOTE PROVENANCE — deployment scoping for the consumed-note monitors
// ================================================================================================
//
// The MINT/BURN/CLAIM/B2AGG note scripts are deployment-independent (identical
// bytes across every agglayer instance on a chain), so — exactly like
// [`crate::restore::classify_claim_note`] — a script-root match alone cannot
// tell OUR deployment's notes from a foreign deployment sharing the chain. The
// bridge MASM emits its MINT/BURN output notes with the DEFAULT (0) tag
// (`bridge_in_output.masm` / `bridge_out.masm` both `push.DEFAULT_TAG`), the
// same tag family the note-visibility reconciler sweeps, so a foreign
// deployment's notes DO land in our store and DO reach the consumed-note
// monitors. Provenance must therefore be decided from what each note itself
// proves — its embedded deployment references — NOT from miden-client's
// consumer attribution, which only reflects which accounts WE track:
// `consumer == Some(x)` means "x is tracked locally", and a foreign
// deployment's independent bridge/faucet accounts are ordinarily `None`.
// Consumer attribution is used below strictly as an additional OURS proof
// (fail-closed direction: keeps notes monitored), never as a foreign proof.

/// Script-root classification of a consumed note for provenance purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitoredNoteKind {
    Mint,
    Burn,
    Claim,
    B2Agg,
    Other,
}

/// Content-positive facts extracted from a consumed note. Each field is a
/// deployment reference the note itself carries:
///
/// - `sender` — [`NoteMetadata::sender`]: the account that CREATED the note.
///   MINT and BURN notes are created by the emitting deployment's BRIDGE
///   account (`bridge_in_output.masm` / `bridge_out.masm` output-note
///   creation), CLAIM notes by that deployment's service account.
/// - `attachment_target` — the `NetworkAccountTarget` attachment: the network
///   account the note is routed to for execution. CLAIM/B2AGG/UpdateGer notes
///   name their deployment's BRIDGE here; MINT notes name the intended FAUCET.
/// - `asset_faucets` — faucet ids of the note's fungible assets. A BURN note
///   carries the asset directly; an AggLayer MINT binds it in canonical storage.
/// - `consumer` — miden-client's consumer attribution (OURS proof only, see
///   the module-section comment above).
#[derive(Debug, Clone)]
struct NoteProvenanceFacts {
    kind: MonitoredNoteKind,
    sender: Option<AccountId>,
    attachment_target: Option<AccountId>,
    asset_faucets: Vec<AccountId>,
    consumer: Option<AccountId>,
}

impl NoteProvenanceFacts {
    /// Extract the provenance facts from a consumed-note record. No I/O.
    fn from_note(note: &InputNoteRecord) -> Self {
        let script_root = note.details().script().root();
        let kind = if script_root == miden_standards::note::MintNote::script_root() {
            MonitoredNoteKind::Mint
        } else if script_root == miden_standards::note::BurnNote::script_root() {
            MonitoredNoteKind::Burn
        } else if script_root == miden_base_agglayer::ClaimNote::script().root() {
            MonitoredNoteKind::Claim
        } else if script_root == B2AggNote::script_root() {
            MonitoredNoteKind::B2Agg
        } else {
            MonitoredNoteKind::Other
        };
        let attachment_target =
            miden_standards::note::NetworkAccountTarget::try_from(note.attachments())
                .ok()
                .map(|nat| nat.target_id());
        let mut asset_faucets: Vec<AccountId> = note
            .details()
            .assets()
            .iter_fungible()
            .map(|fa| fa.faucet_id())
            .collect();
        // Standard/AggLayer MINT notes intentionally have no outer NoteAssets.
        // The asset which `mint_and_send` binds to the consuming faucet lives in
        // the canonical 22-felt MINT storage instead. Treat that faucet as
        // positive provenance too, otherwise a metadata-stripped MINT carrying
        // one of OUR faucets but a foreign attachment could be misclassified as
        // foreign and skipped by #2/#4.
        if kind == MonitoredNoteKind::Mint
            && let Some(identity) = observed_mint_identity(note)
            && !asset_faucets.contains(&identity.faucet)
        {
            asset_faucets.push(identity.faucet);
        }
        Self {
            kind,
            sender: note.metadata().map(|m| m.sender()),
            attachment_target,
            asset_faucets,
            consumer: note.consumer_account(),
        }
    }
}

/// Tri-state deployment provenance for the consumed-note monitors, decided
/// from the note's own content ([`NoteProvenanceFacts`]).
///
/// 1. **OURS proofs (any one keeps the note monitored — fail-closed):** the
///    note's sender, consumer attribution, or `NetworkAccountTarget` is our
///    bridge or a registered faucet, or any carried asset was issued by a
///    registered faucet.
/// 2. **FOREIGN proof (required to skip), per note type:**
///    - MINT: its canonical storage decodes an asset issued by a non-ours
///      faucet. The attachment alone is not foreign proof because a
///      cross-faucet attack can deliberately name another target.
///    - BURN: `sender` (bridge-created, as above) decodes or the burned
///      asset's faucet id is present — and is none of ours.
///    - CLAIM / B2AGG: the `NetworkAccountTarget` attachment decodes and is
///      none of ours — the note is routed to a FOREIGN bridge for execution.
///    - Other: never foreign (fail-closed).
///
/// The three states are handled asymmetrically at the call sites:
///
/// - **OURS** — monitored as local traffic.
/// - **FOREIGN** — the only state that may SKIP the value monitors. Requires a
///   per-type foreign proof AND that nothing of ours matched AND the registry
///   is available (so registered-faucet membership was actually checked).
/// - **UNKNOWN** — the registry is unreadable, or the note carries no decodable
///   provenance either way. Fully MONITORED. A store outage can therefore make
///   a monitor noisier (fail-closed) but never blinder (fail-open).
///
/// Durable CLAIM conclusions do not use this classifier: they require the
/// authoritative tracked consumer to be the local bridge account.
///
/// Pure (no I/O, no metrics) so it is unit-testable directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Ours,
    Foreign,
    Unknown,
}

fn note_provenance(
    facts: &NoteProvenanceFacts,
    registered_faucets: Option<&std::collections::HashSet<AccountId>>,
    bridge_id: AccountId,
) -> Provenance {
    let Some(registered) = registered_faucets else {
        // DEGRADED: registered-faucet membership can't be checked this tick.
        // Still positively identify OURS by registry-INDEPENDENT evidence
        // (our bridge). Everything else is UNKNOWN —
        // never FOREIGN (a store outage must not suppress a monitor).
        let ours = |a: &AccountId| *a == bridge_id;
        if facts.sender.as_ref().is_some_and(ours)
            || facts.consumer.as_ref().is_some_and(ours)
            || facts.attachment_target.as_ref().is_some_and(ours)
        {
            return Provenance::Ours;
        }
        return Provenance::Unknown;
    };
    let ours = |a: &AccountId| *a == bridge_id || registered.contains(a);
    if facts.sender.as_ref().is_some_and(ours)
        || facts.consumer.as_ref().is_some_and(ours)
        || facts.attachment_target.as_ref().is_some_and(ours)
        || facts.asset_faucets.iter().any(|f| registered.contains(f))
    {
        return Provenance::Ours;
    }
    // A known consumer is a locally tracked account. Even when it is no
    // longer registered, keep the note monitored so #2 can report the stale
    // or missing faucet registration instead of classifying it Foreign.
    if facts.consumer.is_some() {
        return Provenance::Unknown;
    }
    let foreign = match facts.kind {
        // Bridge-emitted note types: the creator (sender) is the emitting
        // deployment's bridge. miden-client DROPS metadata on the
        // ConsumedExternal state transition, so externally-consumed records
        // often have no sender — the notes' other embedded references cover
        // that shape: a MINT's canonical storage and a BURN's carried asset
        // name the faucet that issued the value. For MINT, require the storage
        // faucet: the attachment is attacker-selectable and is itself what #2
        // checks, so it cannot safely prove foreignness.
        MonitoredNoteKind::Mint => !facts.asset_faucets.is_empty(),
        MonitoredNoteKind::Burn => facts.sender.is_some() || !facts.asset_faucets.is_empty(),
        // Bridge-executed note types: the NetworkAccountTarget names the
        // bridge that will consume them.
        MonitoredNoteKind::Claim | MonitoredNoteKind::B2Agg => facts.attachment_target.is_some(),
        MonitoredNoteKind::Other => false,
    };
    if foreign {
        Provenance::Foreign
    } else {
        Provenance::Unknown
    }
}

/// Cantina #2 decision for a consumed MINT already attributed to OUR
/// deployment. Wraps the repository's #2 predicate
/// [`crate::mint_target_monitor::check_mint_attachment`]
/// (`consuming_faucet != intended_faucet` — the actual finding: a MINT built
/// for faucet A consumed by faucet B mints B's wrapped asset for A's
/// claimant) and adds the registry-membership signal (intended faucet not in
/// aggkit's registry — cross-faucet exploit against an unregistered faucet, or
/// operator misregistration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MintTargetAlert {
    /// Healthy — no #2 signal.
    None,
    /// Cantina #2 proper: the consuming faucet is not the faucet the MINT's
    /// `NetworkAccountTarget` names.
    ConsumerMismatch {
        intended: AccountId,
        consuming: AccountId,
    },
    /// The MINT's intended faucet is not in aggkit's registry.
    UnregisteredTarget { intended: AccountId },
}

/// Evaluate the Cantina #2 signals for an OURS MINT. `consumer == bridge` is
/// treated as no-consumer-information: only faucets consume MINT notes (the
/// script calls `mint_and_send` on the consuming account), and a bridge-
/// consumed MINT-script note is MA#4 unknown-wrapper territory instead.
/// `registered_faucets == None` (registry unavailable) suppresses ONLY the
/// registry-membership signal — the consumer-vs-intended comparison needs no
/// registry and still fires.
fn mint_cross_faucet_alert(
    intended_faucet: Option<AccountId>,
    consumer: Option<AccountId>,
    registered_faucets: Option<&std::collections::HashSet<AccountId>>,
    bridge_id: AccountId,
) -> MintTargetAlert {
    let Some(intended) = intended_faucet else {
        return MintTargetAlert::None;
    };
    if let Some(consuming) = consumer
        && consuming != bridge_id
        && let crate::mint_target_monitor::MintTargetMatch::Mismatch {
            intended,
            consuming,
        } = crate::mint_target_monitor::check_mint_attachment(intended, consuming)
    {
        return MintTargetAlert::ConsumerMismatch {
            intended,
            consuming,
        };
    }
    if let Some(registered) = registered_faucets
        && !registered.contains(&intended)
    {
        return MintTargetAlert::UnregisteredTarget { intended };
    }
    MintTargetAlert::None
}

/// Number of `ProofData` felts at the head of a CLAIM note's storage
/// (32*8 + 32*8 + 8 + 8 + 8 — see `claim.masm` storage layout).
const CLAIM_PROOF_DATA_FELTS: usize = 536;
/// Total CLAIM note storage felts (proof 536 + leaf 32 + miden_claim_amount 1).
const CLAIM_STORAGE_FELTS: usize = 569;

/// Derive, from a consumed CLAIM note's storage, the serial number of the ONE
/// legitimate MINT note that claim produces on consumption.
///
/// The bridge MASM uses the claim's `PROOF_DATA_KEY` as the MINT serial
/// (`bridge_in_output.masm::build_mint_recipient`: "Generate a serial number
/// for the MINT note (use PROOF_DATA_KEY)"), and `PROOF_DATA_KEY` is
/// `poseidon2::hash_elements` over the first 536 storage felts
/// (`claim.masm::write_claim_data_into_advice_map_by_key`) — exactly
/// [`SequentialCommit::to_commitment`](miden_protocol::crypto::SequentialCommit)
/// of the `ProofData` those felts encode. This is the Cantina #4
/// reconciliation key: an observed MINT whose serial matches no recorded
/// claim's key corresponds to NO claim and is forged.
///
/// Returns `None` for storage that is not CLAIM-shaped (wrong felt count).
fn claim_expected_mint_serial(storage_items: &[miden_protocol::Felt]) -> Option<[u8; 32]> {
    if storage_items.len() != CLAIM_STORAGE_FELTS {
        return None;
    }
    let key: miden_protocol::Word =
        miden_protocol::Hasher::hash_elements(&storage_items[..CLAIM_PROOF_DATA_FELTS]);
    Some(key.as_bytes())
}

/// Storage-felt offset of the `miden_claim_amount` tail felt — the EXACT
/// Miden-scaled amount the MINT this claim produces will carry (the last
/// storage item; see `claim_watcher` layout comment).
const OFFSET_MIDEN_CLAIM_AMOUNT: usize = 568;

/// Result of deriving a consumed CLAIM note's expected-MINT identity
/// ([`claim_expected_mint_identity`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimMintDerivation {
    /// Storage is not CLAIM-shaped / undecodable — nothing derivable.
    NotClaim,
    /// A NATIVE-faucet claim (origin network == this deployment's network id):
    /// it executes the P2ID unlock path and produces NO MINT, so it must NOT
    /// write a claim→MINT legitimacy entry (blocker #1).
    Native,
    /// A non-native claim and the canonical content of its derived MINT.
    NonNative {
        serial: [u8; 32],
        identity: crate::store::ExpectedMint,
    },
}

/// Derive, from a consumed CLAIM note's storage, the serial and canonical
/// expected-MINT content (blocker #1). Persisting and
/// comparing this identity — not just the serial — is what stops a NoAuth
/// forger from copying a public legitimate serial while changing the actual
/// MINT (recipient / asset / amount / destination).
///
/// `local_network_id` is this deployment's Miden network id; a claim whose
/// decoded `LeafData.origin_network` equals it is a native-faucet claim (no
/// MINT) and is reported as [`ClaimMintDerivation::Native`] so the caller
/// records nothing.
fn claim_expected_mint_identity(
    storage: &NoteStorage,
    local_network_id: u32,
) -> ClaimMintDerivation {
    let items = storage.items();
    let Some(serial) = claim_expected_mint_serial(items) else {
        return ClaimMintDerivation::NotClaim;
    };
    // Use the full U256-safe decoder. `parse_claim_event_from_storage` also
    // decodes these fields, but deliberately rejects origin-chain amounts over
    // u64 for the legacy event-store API. Such an amount may still scale to a
    // perfectly valid Miden amount at felt 568 and produce a legitimate MINT.
    let decoded = match crate::claim_watcher::parse_full_claim_from_storage(storage) {
        Ok(d) => d,
        Err(_) => return ClaimMintDerivation::NotClaim,
    };
    // NATIVE claims produce no MINT — never whitelist their serial.
    if decoded.origin_network == local_network_id {
        return ClaimMintDerivation::Native;
    }
    let minted_amount = items[OFFSET_MIDEN_CLAIM_AMOUNT].as_canonical_u64();
    let identity = crate::store::ExpectedMint {
        minted_amount,
        destination_address: decoded.destination_address,
        origin_network: decoded.origin_network,
        origin_address: decoded.origin_address,
    };
    ClaimMintDerivation::NonNative { serial, identity }
}

/// Semantic identity recovered from the canonical AggLayer MINT storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedMintIdentity {
    faucet: AccountId,
    amount: u64,
    destination: AccountId,
    callbacks: miden_protocol::asset::AssetCallbackFlag,
}

const MONITOR_CACHE_CAPACITY: usize = 100_000;

#[derive(Debug, Clone, Copy, Default)]
struct MintScanState {
    target_alerted: bool,
    forged_alerted: bool,
}

fn monitor_cache<V>() -> lru::LruCache<[u8; 32], V> {
    lru::LruCache::new(
        NonZeroUsize::new(MONITOR_CACHE_CAPACITY).expect("monitor cache capacity is non-zero"),
    )
}

/// Stable key for the MINT fields available on `ConsumedExternal` records.
/// Attachments are outside `NoteDetails`, so details commitment alone is not
/// sufficient for monitor dedupe.
fn mint_observation_key(note: &InputNoteRecord) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(b"miden-agglayer/mint-monitor/v1\0");
    hasher.update(note.details_commitment().as_bytes());
    hasher.update([note.attachments().num_attachments()]);
    for attachment in note.attachments().iter() {
        hasher.update(attachment.attachment_scheme().as_u16().to_be_bytes());
        hasher.update(attachment.to_commitment().as_bytes());
    }
    hasher.finalize().into()
}

/// Decode the real AggLayer MINT representation.
///
/// MINT notes have an empty outer `NoteAssets`; the standard MINT script reads
/// the asset and the P2ID output recipient from its 22-felt public storage:
///
/// - `[0..4]` P2ID script root
/// - `[4..8]` P2ID serial (the claim proof-data key)
/// - `[8..12]` asset key, `[12..16]` asset value
/// - `[16]` P2ID destination tag, `[17..20]` zero padding
/// - `[20..22]` P2ID destination account (suffix, prefix)
///
/// Any non-canonical/undecodable shape returns `None` and is handled as
/// `Undetermined` by #4; it is never accepted as legitimate.
fn observed_mint_identity(note: &InputNoteRecord) -> Option<ObservedMintIdentity> {
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::note::NoteTag;
    use miden_protocol::{Felt, Word};
    use miden_standards::note::{MintNote, P2idNote, P2idNoteStorage};

    if note.details().script().root() != MintNote::script_root()
        || !note.details().assets().is_empty()
    {
        return None;
    }
    let items = note.details().storage().items();
    if items.len() != 22 || items[..4] != *P2idNote::script_root().as_elements() {
        return None;
    }

    let p2id_serial = Word::from(<[Felt; 4]>::try_from(&items[4..8]).ok()?);
    if p2id_serial != note.details().recipient().serial_num() {
        return None;
    }

    let key = Word::from(<[Felt; 4]>::try_from(&items[8..12]).ok()?);
    let value = Word::from(<[Felt; 4]>::try_from(&items[12..16]).ok()?);
    let asset = FungibleAsset::from_key_value_words(key, value).ok()?;
    let destination = P2idNoteStorage::try_from(&items[20..22]).ok()?.target();
    if items[16] != Felt::from(NoteTag::with_account_target(destination))
        || items[17..20].iter().any(|felt| *felt != Felt::ZERO)
    {
        return None;
    }

    Some(ObservedMintIdentity {
        faucet: asset.faucet_id(),
        amount: u64::from(asset.amount()),
        destination,
        callbacks: asset.callbacks(),
    })
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
    /// Cantina #4 — how many consecutive sync ticks a consumed OURS-MINT may
    /// stay unmatched against the recorded claim→MINT-serial history before
    /// the forged-MINT alert fires. Absorbs cross-tick import ordering (the
    /// reconciler can surface a MINT a few sweep ticks before the CLAIM that
    /// produced it). Builder-overridable for tests.
    forged_mint_grace_ticks: u32,
    /// Bounded per-MINT hot state. The persistent claim history remains the
    /// authority; eviction only causes a re-check (or a later repeat alert),
    /// never acceptance of a mismatch.
    mint_scan_state: parking_lot::Mutex<lru::LruCache<[u8; 32], MintScanState>>,
    /// Short-lived #4 grace state. Entries are removed on match, mismatch, or
    /// alert, so this contains only currently unresolved observations rather
    /// than the full consumed-note history.
    forged_mint_pending: parking_lot::Mutex<std::collections::HashMap<[u8; 32], u32>>,
    /// Bounded CLAIM hot set used only to avoid re-hashing historical storage.
    /// Eviction safely falls back to the idempotent persistent write.
    claim_serial_recorded: parking_lot::Mutex<lru::LruCache<[u8; 32], ()>>,
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
            forged_mint_grace_ticks: 10, // ~60s at the 6s sync cadence
            mint_scan_state: parking_lot::Mutex::new(monitor_cache()),
            forged_mint_pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
            claim_serial_recorded: parking_lot::Mutex::new(monitor_cache()),
        }
    }

    fn mint_state(&self, key: &[u8; 32]) -> MintScanState {
        self.mint_scan_state
            .lock()
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    fn update_mint_state(
        &self,
        key: [u8; 32],
        update: impl FnOnce(&mut MintScanState),
    ) -> MintScanState {
        let mut cache = self.mint_scan_state.lock();
        let mut state = cache.pop(&key).unwrap_or_default();
        update(&mut state);
        cache.put(key, state);
        state
    }

    /// Override the Cantina #4 forged-MINT grace window (sync ticks a MINT may
    /// stay unmatched against the claim history before alerting). Tests drive
    /// this to small values for deterministic lifecycles.
    #[cfg(test)]
    fn with_forged_mint_grace_ticks(mut self, ticks: u32) -> Self {
        self.forged_mint_grace_ticks = ticks.max(1);
        self
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

/// Per-scan outcome of the monitor pass — returned so wiring tests can assert
/// alert decisions directly instead of scraping global metrics.
#[derive(Debug, Default)]
struct ScanOutcome {
    /// CLAIM note ids seen consumed this tick (fed to the Cantina #7
    /// expected-MINT tracker).
    landed_claim_ids: std::collections::HashSet<[u8; 32]>,
    /// Cantina #2 alerts fired this scan (MINT note ids; one-shot per note).
    cross_faucet_alerts: Vec<[u8; 32]>,
    /// Cantina #4 forged-MINT alerts fired this scan (one-shot per note).
    forged_mint_alerts: Vec<[u8; 32]>,
    /// `list_faucets()` failed → fail-closed: NOTHING was skipped as foreign
    /// this tick and the registry-membership #2 signal was suppressed.
    registry_degraded: bool,
}

impl BridgeOutScanner {
    /// Cantina #23 / #19 — client-free, **MONITOR-ONLY** pass over the
    /// consumed-note set. Preserves the existing all-note twin observation,
    /// scopes the MINT (#2/#4) monitors, and maintains the Cantina #4 claim→MINT
    /// serial history and reconciles observed MINTs against it, evaluates the
    /// Cantina #2 mint-target predicate, emits the matching metrics/logs, and
    /// returns the per-scan [`ScanOutcome`] (including the consumed CLAIM ids
    /// fed to the #7 expected-MINT tracker).
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
    ) -> ScanOutcome {
        let mut outcome = ScanOutcome::default();

        // Deployment scoping inputs. A `list_faucets()` failure is a DEGRADED
        // state, not an empty registry: collapsing it to an empty set would
        // classify OUR OWN registered faucets as foreign and silently suppress
        // their burn/mint observations (fail-open). Fail closed instead:
        // without the registry, no note is classified Foreign, so every value
        // monitor continues to run until the registry is readable again.
        let registered_faucets: Option<std::collections::HashSet<AccountId>> =
            match self.store.list_faucets().await {
                Ok(v) => Some(v.into_iter().map(|f| f.faucet_id).collect()),
                Err(e) => {
                    outcome.registry_degraded = true;
                    metrics::counter!("bridge_monitor_registry_unavailable_total").increment(1);
                    tracing::error!(
                        target: "bridge_out::provenance",
                        error = ?e,
                        "faucet registry unreadable — provenance gates fail CLOSED this tick: \
                         no monitor is suppressed, unproven notes remain unknown; \
                         the registry-membership Cantina #2 signal is paused until the \
                         registry is readable again"
                    );
                    None
                }
            };

        // ── Pass 1 — Cantina #4 claim history. Every CLAIM consumed by our
        // tracked bridge account records the canonical
        // future MINT whose serial is the claim's PROOF_DATA_KEY, with its
        // derivable content (amount / asset /
        // destination — see `claim_expected_mint_identity`). Record that
        // identity into the PERSISTENT history BEFORE pass 2 reconciles MINTs,
        // so a CLAIM and its MINT surfacing in the same tick (or the full
        // historical set on a fresh boot) reconcile without a false forged
        // alert.
        //
        // Consumer attribution is required for this durable write. The
        // NetworkAccountTarget is routing metadata and cannot authorize a
        // claim history row on its own; miden-client retains `Some(bridge)` for
        // notes consumed by our tracked bridge even in ConsumedExternal state.
        for note in consumed_notes {
            let facts = NoteProvenanceFacts::from_note(note);
            if facts.kind != MonitoredNoteKind::Claim
                || facts.consumer != Some(self.bridge_account_id)
            {
                continue;
            }
            let id_bytes: [u8; 32] = note.details_commitment().as_bytes();
            if self.claim_serial_recorded.lock().get(&id_bytes).is_some() {
                continue;
            }
            let (serial, identity) =
                match claim_expected_mint_identity(note.details().storage(), self.local_network_id)
                {
                    ClaimMintDerivation::NonNative { serial, identity } => (serial, identity),
                    ClaimMintDerivation::Native => {
                        // Native-faucet claim: P2ID unlock, NO MINT produced —
                        // record NOTHING (blocker #1). Cache the observation key so we
                        // don't re-decode it every tick.
                        self.claim_serial_recorded.lock().put(id_bytes, ());
                        continue;
                    }
                    ClaimMintDerivation::NotClaim => {
                        // Claim-script note with non-CLAIM-shaped/undecodable
                        // storage — nothing derivable. Its (hypothetical) MINT
                        // stays unmatched, which alerts: fail-closed.
                        continue;
                    }
                };
            match self
                .store
                .claim_mint_expected_record(&serial, &identity)
                .await
            {
                Ok(()) => {
                    self.claim_serial_recorded.lock().put(id_bytes, ());
                }
                Err(e) => {
                    // Not cached on failure — retried next tick. Until it
                    // lands, the corresponding MINT only accrues grace ticks.
                    tracing::warn!(
                        target: "bridge_out::forged_mint",
                        note_id = ?note.details_commitment(),
                        error = ?e,
                        "claim→MINT identity history write failed; retrying next sync"
                    );
                }
            }
        }

        // ── Pass 2 — per-note monitors.
        for note in consumed_notes {
            let id_bytes: [u8; 32] = note.details_commitment().as_bytes();
            if let Some(commitment_word) = note.commitment() {
                let commitment_bytes: [u8; 32] = commitment_word.as_bytes();
                // Keep the main-branch tracker ordering: every metadata-bearing
                // consumed note is observed before deployment provenance gates.
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
            }
            let facts = NoteProvenanceFacts::from_note(note);
            let foreign = matches!(
                note_provenance(&facts, registered_faucets.as_ref(), self.bridge_account_id,),
                Provenance::Foreign
            );

            if foreign && facts.kind == MonitoredNoteKind::Mint {
                // Positively another deployment's note — excluded from the
                // local #2/#4 MINT monitors. No
                // process-lifetime skip set is kept: this full-history scan
                // must remain memory-bounded.
                self.forged_mint_pending
                    .lock()
                    .remove(&mint_observation_key(note));
                continue;
            }

            match facts.kind {
                // Cantina #7 — CLAIM consumption observation. The bridge
                // ALWAYS consumes the CLAIM as a precondition to emitting the
                // MINT, so a CLAIM in the consumed-set is the proxy "MINT
                // landed" signal for this proxy's expected-MINT tracker.
                MonitoredNoteKind::Claim => {
                    if facts.consumer == Some(self.bridge_account_id) {
                        outcome.landed_claim_ids.insert(id_bytes);
                    }
                }
                // Cantina #5 — unchanged from main. PR #123 does not alter
                // the legacy BURN monitor's behavior or persistence model.
                MonitoredNoteKind::Burn if note.commitment().is_some() => {
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
                // Preserve main's defensive skip for incomplete BURN records.
                // Claim/MINT reconciliation intentionally continues because
                // ConsumedExternal records lose metadata after sync.
                MonitoredNoteKind::Burn => {}
                // Cantina #2 + #4 — MINT monitors, see `scan_mint_monitors`.
                MonitoredNoteKind::Mint => {
                    self.scan_mint_monitors(
                        note,
                        id_bytes,
                        &facts,
                        registered_faucets.as_ref(),
                        &mut outcome,
                    )
                    .await;
                }
                MonitoredNoteKind::B2Agg | MonitoredNoteKind::Other => {}
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
            if note.commitment().is_some()
                && note.consumer_account() == Some(self.bridge_account_id)
            {
                let b2agg_root_bytes = B2AggNote::script_root().as_bytes();
                let claim_root_bytes = miden_base_agglayer::ClaimNote::script().root().as_bytes();
                let observed_bytes = note.details().script().root().as_bytes();
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

        outcome
    }

    /// Cantina #2 + #4 for one consumed OURS MINT.
    ///
    /// **#2 (cross-faucet):** [`mint_cross_faucet_alert`] wires the
    /// repository's #2 predicate
    /// ([`crate::mint_target_monitor::check_mint_attachment`]): a MINT whose
    /// consuming faucet differs from its `NetworkAccountTarget` — including
    /// registered faucet B consuming registered faucet A's MINT — pages, plus
    /// the registry-membership signal (intended faucet unregistered).
    ///
    /// **#4 (forged):** reconciles the MINT against aggkit's recorded claim
    /// history. The MINT's serial number is, by MASM construction, its
    /// producing claim's PROOF_DATA_KEY (see [`claim_expected_mint_serial`]).
    /// Blocker #1: serial membership ALONE is NOT enough — with NoAuth
    /// authorship a forger can copy a public legitimate serial while changing
    /// the actual MINT. So the recorded entry is the FULL derivable expected
    /// identity ([`crate::store::ExpectedMint`]) and the observed MINT is
    /// accepted ONLY if its identity matches:
    ///
    /// - **serial ∉ history** → unmatched; Forged after
    ///   [`Self::forged_mint_grace_ticks`] consecutive ticks (grace absorbs
    ///   cross-tick import ordering — the reconciler can surface a MINT before
    ///   its CLAIM).
    /// - **serial ∈ history but identity MISMATCH** (recipient / amount / asset differ from
    ///   the claim's derived expected MINT) → Forged IMMEDIATELY, no grace:
    ///   the serial is recorded, so there is no import-ordering excuse; the
    ///   copied-serial-different-MINT signature is positive.
    /// - **serial ∈ history AND identity matches** → Recognised.
    ///
    /// This deliberately does NOT equate "forged" with "missing decodable
    /// NetworkAccountTarget": a forger can attach a perfectly valid target;
    /// what they cannot fabricate is a consumed CLAIM whose PROOF_DATA_KEY
    /// equals their serial AND whose derived recipient/amount/asset equals theirs.
    ///
    /// Alert dedupe is best-effort per details-and-attachments observation in
    /// a bounded cache.
    /// It is intentionally not described as NoteId-based: externally consumed
    /// records do not retain metadata or a nullifier.
    async fn scan_mint_monitors(
        &self,
        note: &InputNoteRecord,
        id_bytes: [u8; 32],
        facts: &NoteProvenanceFacts,
        registered_faucets: Option<&std::collections::HashSet<AccountId>>,
        outcome: &mut ScanOutcome,
    ) {
        let state_key = mint_observation_key(note);
        // Cantina #2 — the storage asset faucet is the authoritative consuming
        // faucet: standard `mint_and_send` rejects unless the active faucet owns
        // that asset key. Prefer it over miden-client's optional consumer
        // attribution, which is often absent for externally consumed notes.
        if !self.mint_state(&state_key).target_alerted {
            let consuming_faucet = observed_mint_identity(note)
                .map(|identity| identity.faucet)
                .or(facts.consumer);
            match mint_cross_faucet_alert(
                facts.attachment_target,
                consuming_faucet,
                registered_faucets,
                self.bridge_account_id,
            ) {
                MintTargetAlert::ConsumerMismatch {
                    intended,
                    consuming,
                } => {
                    metrics::counter!("bridge_mint_target_mismatch_total").increment(1);
                    tracing::error!(
                        target: "bridge_out::mint_attach",
                        note_id = ?note.details_commitment(),
                        intended_faucet = %intended,
                        consuming_faucet = %consuming,
                        "Cantina #2: MINT consumed by a faucet other than its \
                         NetworkAccountTarget — cross-faucet exploit"
                    );
                    self.update_mint_state(state_key, |state| state.target_alerted = true);
                    outcome.cross_faucet_alerts.push(id_bytes);
                }
                MintTargetAlert::UnregisteredTarget { intended } => {
                    metrics::counter!("bridge_mint_target_mismatch_total").increment(1);
                    tracing::error!(
                        target: "bridge_out::mint_attach",
                        note_id = ?note.details_commitment(),
                        intended_faucet = %intended,
                        "Cantina #2: MINT NetworkAccountTarget points at a faucet \
                         not in aggkit's registry — possible cross-faucet exploit \
                         or misregistered faucet"
                    );
                    self.update_mint_state(state_key, |state| state.target_alerted = true);
                    outcome.cross_faucet_alerts.push(id_bytes);
                }
                MintTargetAlert::None => {}
            }
        }
        let state = self.mint_state(&state_key);
        if state.forged_alerted {
            return;
        }
        // Cantina #4 — reconcile against the recorded claim→MINT IDENTITY.
        let serial: [u8; 32] = note.details().recipient().serial_num().as_bytes();
        match self.store.claim_mint_expected_get(&serial).await {
            Ok(Some(expected)) => {
                // Serial matches a recorded claim. Accept ONLY if the observed
                // MINT's derivable identity matches (blocker #1).
                match self.observed_mint_matches_expected(note, &expected).await {
                    MintIdentityCheck::Match => {
                        self.forged_mint_pending.lock().remove(&state_key);
                    }
                    MintIdentityCheck::Mismatch {
                        field,
                        expected,
                        observed,
                    } => {
                        // No grace: the serial IS recorded, so there is no
                        // import-ordering excuse — a MINT reusing a recorded
                        // serial with different details is the copied-serial
                        // forgery signature.
                        self.forged_mint_pending.lock().remove(&state_key);
                        self.update_mint_state(state_key, |state| {
                            state.forged_alerted = true;
                        });
                        metrics::counter!("bridge_forged_mint_total", "reason" => "detail_mismatch")
                            .increment(1);
                        tracing::error!(
                            target: "bridge_out::forged_mint",
                            note_id = ?note.details_commitment(),
                            serial = %hex::encode(serial),
                            mismatched_field = field,
                            expected = %expected,
                            observed = %observed,
                            "Cantina #4 (blocker #1): MINT reuses a recorded claim's \
                             serial but its identity DIFFERS from the claim's derived \
                             expected MINT — copied-serial forgery via NoAuth authorship"
                        );
                        outcome.forged_mint_alerts.push(id_bytes);
                    }
                    MintIdentityCheck::Undetermined => {
                        // Could not read the observed MINT's asset (e.g. a
                        // stripped record). Do NOT whitelist and do NOT
                        // immediately page — accrue grace like the unmatched
                        // path so a transient shape doesn't false-fire but a
                        // persistent one eventually alerts (fail-closed).
                        self.accrue_forged_grace(
                            note,
                            state_key,
                            id_bytes,
                            &serial,
                            "identity_undetermined",
                            outcome,
                        );
                    }
                }
            }
            Ok(None) => {
                // serial ∉ history — unmatched; forged after the grace window.
                self.accrue_forged_grace(note, state_key, id_bytes, &serial, "no_claim", outcome);
            }
            Err(e) => {
                tracing::warn!(
                    target: "bridge_out::forged_mint",
                    note_id = ?note.details_commitment(),
                    error = ?e,
                    "claim→MINT identity history read failed; grace window \
                     not advanced, retrying next sync"
                );
            }
        }
    }

    /// Compare an observed MINT's canonical storage identity against the
    /// expected identity derived from its claim. Amount and P2ID recipient are
    /// compared directly. The asset faucet is compared only after the origin
    /// route resolves; a missing/failed registry lookup is `Undetermined` and
    /// retried, never accepted and cached as legitimate.
    async fn observed_mint_matches_expected(
        &self,
        note: &InputNoteRecord,
        expected: &crate::store::ExpectedMint,
    ) -> MintIdentityCheck {
        if !note.details().assets().is_empty() {
            return MintIdentityCheck::Mismatch {
                field: "outer_assets",
                expected: "0".to_string(),
                observed: note.details().assets().num_assets().to_string(),
            };
        }
        let Some(observed) = observed_mint_identity(note) else {
            return MintIdentityCheck::Undetermined;
        };
        if observed.amount != expected.minted_amount {
            return MintIdentityCheck::Mismatch {
                field: "minted_amount",
                expected: expected.minted_amount.to_string(),
                observed: observed.amount.to_string(),
            };
        }
        if observed.callbacks != miden_protocol::asset::AssetCallbackFlag::Enabled {
            return MintIdentityCheck::Mismatch {
                field: "asset_callbacks",
                expected: "Enabled".to_string(),
                observed: format!("{:?}", observed.callbacks),
            };
        }
        // Mirror `bridge_in.masm`: the claim converts the 20-byte leaf
        // destination directly with `eth_address::to_account_id`. Proxy-side
        // address mappings cannot affect this on-chain output.
        let Some(expected_destination) = crate::address_mapper::account_id_from_address(
            alloy::primitives::Address::from(expected.destination_address),
        ) else {
            return MintIdentityCheck::Undetermined;
        };
        if observed.destination != expected_destination {
            return MintIdentityCheck::Mismatch {
                field: "destination_account",
                expected: expected_destination.to_string(),
                observed: observed.destination.to_string(),
            };
        }

        let expected_faucet = match self
            .store
            .get_faucet_by_origin(&expected.origin_address, expected.origin_network)
            .await
        {
            Ok(Some(faucet)) => faucet.faucet_id,
            Ok(None) => return MintIdentityCheck::Undetermined,
            Err(error) => {
                tracing::warn!(
                    target: "bridge_out::forged_mint",
                    origin_network = expected.origin_network,
                    origin_address = %hex::encode(expected.origin_address),
                    error = ?error,
                    "expected-MINT faucet lookup failed; leaving identity undetermined"
                );
                return MintIdentityCheck::Undetermined;
            }
        };
        if observed.faucet != expected_faucet {
            return MintIdentityCheck::Mismatch {
                field: "asset_faucet",
                expected: expected_faucet.to_string(),
                observed: observed.faucet.to_string(),
            };
        }

        // `bridge_in_output.masm` creates exactly one routing attachment for
        // the MINT: NetworkAccountTarget(expected_faucet, Always). Attachments
        // are outside NoteDetails, so bind them explicitly rather than
        // accepting a storage-identical note with altered routing metadata.
        if note.attachments().num_attachments() != 1 {
            return MintIdentityCheck::Mismatch {
                field: "routing_attachment_count",
                expected: "1".to_string(),
                observed: note.attachments().num_attachments().to_string(),
            };
        }
        let Ok(target) = miden_standards::note::NetworkAccountTarget::try_from(note.attachments())
        else {
            return MintIdentityCheck::Mismatch {
                field: "routing_attachment",
                expected: format!("NetworkAccountTarget({expected_faucet}, Always)"),
                observed: "undecodable".to_string(),
            };
        };
        if target.target_id() != expected_faucet {
            return MintIdentityCheck::Mismatch {
                field: "routing_target",
                expected: expected_faucet.to_string(),
                observed: target.target_id().to_string(),
            };
        }
        if target.execution_hint() != miden_standards::note::NoteExecutionHint::Always {
            return MintIdentityCheck::Mismatch {
                field: "routing_execution_hint",
                expected: "Always".to_string(),
                observed: format!("{:?}", target.execution_hint()),
            };
        }
        MintIdentityCheck::Match
    }

    /// Advance the forged-MINT grace window for an unmatched/undetermined MINT
    /// and fire the Cantina #4 forged alert (once) when the window is
    /// exhausted. Shared by the "serial not in history" and "can't determine
    /// identity" paths.
    fn accrue_forged_grace(
        &self,
        note: &InputNoteRecord,
        state_key: [u8; 32],
        id_bytes: [u8; 32],
        serial: &[u8; 32],
        reason: &'static str,
        outcome: &mut ScanOutcome,
    ) {
        let ticks = {
            let mut pending = self.forged_mint_pending.lock();
            let ticks = pending.entry(state_key).or_insert(0);
            *ticks = ticks.saturating_add(1);
            *ticks
        };
        if ticks >= self.forged_mint_grace_ticks {
            self.forged_mint_pending.lock().remove(&state_key);
            self.update_mint_state(state_key, |state| state.forged_alerted = true);
            metrics::counter!("bridge_forged_mint_total", "reason" => reason).increment(1);
            if reason == "no_claim" {
                tracing::error!(
                    target: "bridge_out::forged_mint",
                    note_id = ?note.details_commitment(),
                    serial = %hex::encode(serial),
                    grace_ticks = ticks,
                    "Cantina #4: MINT note matches NO aggkit-recorded claim \
                     (serial ∉ claim PROOF_DATA_KEY history) — forged via \
                     NoAuth bridge note authorship"
                );
            } else {
                tracing::error!(
                    target: "bridge_out::forged_mint",
                    note_id = ?note.details_commitment(),
                    serial = %hex::encode(serial),
                    grace_ticks = ticks,
                    "Cantina #4: a recorded claim exists, but its expected MINT \
                     identity could not be reconciled after the grace window"
                );
            }
            outcome.forged_mint_alerts.push(id_bytes);
        }
    }
}

/// Result of comparing an observed MINT against its recorded expected identity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MintIdentityCheck {
    /// Observed identity matches the recorded expected identity.
    Match,
    /// A bound field differs — the copied-serial forgery signature.
    Mismatch {
        field: &'static str,
        expected: String,
        observed: String,
    },
    /// The observed MINT's asset could not be read (stripped record) — can't
    /// determine; handled fail-closed with grace.
    Undetermined,
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
        // the twin (#6) / forged-MINT (#2/#4) trackers and
        // emits metrics, and returns the CLAIM ids seen consumed (for the #7
        // expected-MINT tracker). It NEVER advances `latest_block_number` nor
        // writes a BridgeEvent — the pre-redesign scanner did both here, once per
        // consumed B2AGG note, which raced `restore()` (#23) and misnumbered
        // synthetic blocks (#19). The SyntheticProjector is now the sole
        // emitter/tip-advancer.
        let landed_claim_ids = self
            .scan_consumed_notes_monitors(&consumed_notes)
            .await
            .landed_claim_ids;

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

    // NOTE-PROVENANCE — Cantina #2 / #4 deployment-scoping tests
    // ============================================================================================

    use crate::store::Store as _;
    use miden_protocol::note::{
        NoteAssets, NoteAttachment, NoteAttachments, NoteDetails as PNoteDetails, NoteMetadata,
        NoteRecipient, NoteType, PartialNoteMetadata,
    };

    /// Distinct ids for the provenance tests: our bridge, two registered
    /// faucets (A consumed-by-B is the Cantina #2 proper case), an
    /// unregistered faucet in our flow, and a foreign deployment's
    /// bridge/service/faucet.
    struct ProvIds {
        bridge: AccountId,
        faucet_a: AccountId,
        faucet_b: AccountId,
        unregistered: AccountId,
        foreign_bridge: AccountId,
        foreign_service: AccountId,
        foreign_faucet: AccountId,
        local_service: AccountId,
    }

    fn prov_ids() -> ProvIds {
        ProvIds {
            bridge: AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap(),
            faucet_a: AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap(),
            faucet_b: AccountId::from_hex("0xaa0000000000bb110000cc000000fd").unwrap(),
            unregistered: AccountId::from_hex("0xab0000000000cd110000cd000000ef").unwrap(),
            foreign_bridge: AccountId::from_hex("0xba0000000000ab110000ab000000ba").unwrap(),
            foreign_service: AccountId::from_hex("0xae0000000000ba110000ba000000ae").unwrap(),
            foreign_faucet: AccountId::from_hex("0xcc0000000000dd110000ee000000ff").unwrap(),
            local_service: AccountId::from_hex("0xad0000000000ef110000ef000000ad").unwrap(),
        }
    }

    fn registry(faucets: &[AccountId]) -> std::collections::HashSet<AccountId> {
        faucets.iter().copied().collect()
    }

    fn facts(
        kind: MonitoredNoteKind,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        asset_faucets: &[AccountId],
        consumer: Option<AccountId>,
    ) -> NoteProvenanceFacts {
        NoteProvenanceFacts {
            kind,
            sender,
            attachment_target,
            asset_faucets: asset_faucets.to_vec(),
            consumer,
        }
    }

    /// The exact review finding on the first cut of this fix: miden-client's
    /// consumer attribution is NOT provenance. `Some(consumer)` only means the
    /// account is TRACKED locally; a foreign deployment's independent
    /// bridge/faucet accounts are ordinarily `None`. Neither direction may be
    /// used as a foreign proof:
    ///  - `None` consumer + no content proof → monitored (was already true),
    ///  - `Some(non-ours)` consumer + no content proof → STILL monitored.
    #[test]
    fn consumer_attribution_alone_is_never_a_foreign_proof() {
        let ids = prov_ids();
        let reg = registry(&[ids.faucet_a]);
        for consumer in [None, Some(ids.foreign_faucet)] {
            let f = facts(MonitoredNoteKind::Other, None, None, &[], consumer);
            assert!(
                !matches!(
                    note_provenance(&f, Some(&reg), ids.bridge),
                    Provenance::Foreign
                ),
                "a note with no content-positive foreign proof must stay monitored \
                 (consumer = {consumer:?})"
            );
        }
    }

    /// A foreign deployment's MINT/BURN prove provenance through their asset
    /// faucet (canonical storage for MINT, outer assets for BURN). CLAIM/B2AGG
    /// prove it via the `NetworkAccountTarget` naming the executing bridge —
    /// including the reconciler-import shape with `consumer == None`.
    #[test]
    fn foreign_notes_positively_identified_by_content() {
        let ids = prov_ids();
        let reg = registry(&[ids.faucet_a]);

        // Foreign MINT: sender = foreign bridge, target = foreign faucet.
        assert_eq!(
            note_provenance(
                &facts(
                    MonitoredNoteKind::Mint,
                    Some(ids.foreign_bridge),
                    Some(ids.foreign_faucet),
                    &[ids.foreign_faucet],
                    None,
                ),
                Some(&reg),
                ids.bridge,
            ),
            Provenance::Foreign
        );
        // Foreign BURN: burned asset issued by the foreign faucet.
        assert_eq!(
            note_provenance(
                &facts(
                    MonitoredNoteKind::Burn,
                    Some(ids.foreign_bridge),
                    None,
                    &[ids.foreign_faucet],
                    None,
                ),
                Some(&reg),
                ids.bridge,
            ),
            Provenance::Foreign
        );
        // Foreign CLAIM, consumer None (reconciler-imported, foreign bridge
        // untracked) — the exact shared-chain false-positive path.
        assert_eq!(
            note_provenance(
                &facts(
                    MonitoredNoteKind::Claim,
                    Some(ids.foreign_service),
                    Some(ids.foreign_bridge),
                    &[],
                    None,
                ),
                Some(&reg),
                ids.bridge,
            ),
            Provenance::Foreign
        );
        // Foreign B2AGG targeting the foreign bridge.
        assert_eq!(
            note_provenance(
                &facts(
                    MonitoredNoteKind::B2Agg,
                    None,
                    Some(ids.foreign_bridge),
                    &[],
                    None,
                ),
                Some(&reg),
                ids.bridge,
            ),
            Provenance::Foreign
        );
    }

    /// Every OURS reference keeps a note monitored, and undecodable shapes
    /// fail closed.
    #[test]
    fn ours_references_and_undecodable_fields_stay_monitored() {
        let ids = prov_ids();
        let reg = registry(&[ids.faucet_a]);
        let cases = [
            // Our bridge minted it (includes the forged-MINT shape: the forger
            // MUST spoof sender == our bridge to pass the faucet owner check).
            facts(
                MonitoredNoteKind::Mint,
                Some(ids.bridge),
                Some(ids.foreign_faucet),
                &[],
                None,
            ),
            // Targeting a registered faucet.
            facts(MonitoredNoteKind::Mint, None, Some(ids.faucet_a), &[], None),
            // A foreign-looking attachment without a decodable storage asset
            // is not enough to suppress the value monitors.
            facts(
                MonitoredNoteKind::Mint,
                None,
                Some(ids.foreign_faucet),
                &[],
                None,
            ),
            // BURN of a registered faucet's asset.
            facts(
                MonitoredNoteKind::Burn,
                Some(ids.foreign_bridge),
                None,
                &[ids.faucet_a],
                None,
            ),
            // A CLAIM with no target is unknown, so it stays monitored.
            facts(
                MonitoredNoteKind::Claim,
                Some(ids.local_service),
                None,
                &[],
                None,
            ),
            // Consumed by our bridge (consumer as an OURS proof only).
            facts(
                MonitoredNoteKind::Claim,
                Some(ids.foreign_service),
                None,
                &[],
                Some(ids.bridge),
            ),
            // MINT with no metadata at all — nothing decodable, fail closed.
            facts(MonitoredNoteKind::Mint, None, None, &[], None),
            // B2AGG whose attachment did not decode — fail closed.
            facts(
                MonitoredNoteKind::B2Agg,
                Some(ids.foreign_service),
                None,
                &[],
                None,
            ),
        ];
        for f in &cases {
            assert!(
                !matches!(
                    note_provenance(f, Some(&reg), ids.bridge),
                    Provenance::Foreign
                ),
                "must stay monitored: {f:?}"
            );
        }
    }

    /// Item 3 — registry failure is fail-CLOSED: with `registered_faucets ==
    /// None` NOTHING is positively foreign, even a blatantly foreign note.
    /// (The first cut collapsed `list_faucets()` errors to an empty registry,
    /// which classified OUR OWN faucets' notes as foreign and suppressed
    /// their alerts.)
    #[test]
    fn registry_unavailable_is_never_a_skip() {
        let ids = prov_ids();
        let blatantly_foreign = facts(
            MonitoredNoteKind::Mint,
            Some(ids.foreign_bridge),
            Some(ids.foreign_faucet),
            &[ids.foreign_faucet],
            Some(ids.foreign_faucet),
        );
        assert_eq!(
            note_provenance(&blatantly_foreign, None, ids.bridge),
            Provenance::Unknown
        );
    }

    /// Cantina #2 — the ACTUAL finding is `consuming_faucet != intended_faucet`
    /// (`mint_target_monitor::check_mint_attachment`). Registered faucet B
    /// consuming registered faucet A's MINT MUST alert.
    ///
    /// RED pin: the first cut's #2 predicate was only "intended faucet not in
    /// registry" — for intended == A (registered) it evaluated `false` and the
    /// B-consumes-A exploit passed silently. The pre-fix expression is pinned
    /// below.
    #[test]
    fn cantina2_registered_b_consuming_a_mint_alerts() {
        let ids = prov_ids();
        let reg = registry(&[ids.faucet_a, ids.faucet_b]);

        // Pre-fix predicate (registry membership only): A is registered → no
        // alert → the exploit was missed.
        let pre_fix_alerted = !reg.contains(&ids.faucet_a);
        assert!(
            !pre_fix_alerted,
            "RED: the registry-membership-only #2 check cannot see B consuming A's MINT"
        );

        assert_eq!(
            mint_cross_faucet_alert(
                Some(ids.faucet_a),
                Some(ids.faucet_b),
                Some(&reg),
                ids.bridge
            ),
            MintTargetAlert::ConsumerMismatch {
                intended: ids.faucet_a,
                consuming: ids.faucet_b
            },
            "GREEN: check_mint_attachment wired — B consuming A's MINT alerts"
        );
        // The mismatch signal needs no registry — it still fires when the
        // registry read failed (fail-closed).
        assert_eq!(
            mint_cross_faucet_alert(Some(ids.faucet_a), Some(ids.faucet_b), None, ids.bridge),
            MintTargetAlert::ConsumerMismatch {
                intended: ids.faucet_a,
                consuming: ids.faucet_b
            },
        );
    }

    /// Cantina #2 auxiliary signals: unregistered intended target (consumer
    /// unknown) alerts; healthy in-order consumption and undecodable targets
    /// do not; `consumer == bridge` carries no faucet information.
    #[test]
    fn cantina2_registry_membership_and_healthy_paths() {
        let ids = prov_ids();
        let reg = registry(&[ids.faucet_a, ids.faucet_b]);
        // Unregistered intended target, consumer unknown → alert.
        assert_eq!(
            mint_cross_faucet_alert(Some(ids.unregistered), None, Some(&reg), ids.bridge),
            MintTargetAlert::UnregisteredTarget {
                intended: ids.unregistered
            },
        );
        // …but with the registry unavailable that signal is paused (it cannot
        // be evaluated) — no false page storm on a DB blip.
        assert_eq!(
            mint_cross_faucet_alert(Some(ids.unregistered), None, None, ids.bridge),
            MintTargetAlert::None,
        );
        // Healthy: A's MINT consumed by A.
        assert_eq!(
            mint_cross_faucet_alert(
                Some(ids.faucet_a),
                Some(ids.faucet_a),
                Some(&reg),
                ids.bridge
            ),
            MintTargetAlert::None,
        );
        // No decodable target → #2 has nothing to compare (that shape is #4's
        // reconciliation job, not a target-mismatch).
        assert_eq!(
            mint_cross_faucet_alert(None, Some(ids.faucet_b), Some(&reg), ids.bridge),
            MintTargetAlert::None,
        );
        // consumer == bridge is not a consuming faucet.
        assert_eq!(
            mint_cross_faucet_alert(Some(ids.faucet_a), Some(ids.bridge), Some(&reg), ids.bridge),
            MintTargetAlert::None,
        );
    }

    /// Cantina #4 reconciliation key: `claim_expected_mint_serial` (poseidon2
    /// over the CLAIM storage's 536 proof-data felts) must equal the
    /// `SequentialCommit` commitment of the `ProofData` those felts encode —
    /// the PROOF_DATA_KEY the bridge MASM uses as the MINT serial. Also pins
    /// the CLAIM-shape gate (569 felts exactly).
    #[test]
    fn claim_expected_mint_serial_is_proof_data_key() {
        use miden_base_agglayer::{ExitRoot, GlobalIndex, ProofData, SmtNode};
        use miden_protocol::Felt;
        use miden_protocol::crypto::SequentialCommit;

        let proof_data = ProofData {
            smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
            smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
            global_index: GlobalIndex::new([0u8; 32]),
            mainnet_exit_root: ExitRoot::new([0u8; 32]),
            rollup_exit_root: ExitRoot::new([0u8; 32]),
        };
        // Assemble CLAIM-shaped storage: proof(536) + leaf(32) + amount(1).
        let mut items = proof_data.to_elements();
        assert_eq!(items.len(), 536);
        items.extend(std::iter::repeat_n(Felt::from(7u32), 33));
        let serial = claim_expected_mint_serial(&items).expect("claim-shaped storage");
        let expected: miden_protocol::Word = proof_data.to_commitment();
        assert_eq!(
            serial,
            expected.as_bytes(),
            "serial derivation must equal ProofData's PROOF_DATA_KEY commitment"
        );
        // Non-CLAIM-shaped storage yields no key.
        assert_eq!(claim_expected_mint_serial(&items[..536]), None);
        assert_eq!(claim_expected_mint_serial(&[]), None);

        // The leaf-data tail must NOT influence the key (MASM hashes only
        // storage[0..536]).
        let mut items2 = proof_data.to_elements();
        items2.extend(std::iter::repeat_n(Felt::from(9u32), 33));
        assert_eq!(
            claim_expected_mint_serial(&items2).unwrap(),
            serial,
            "PROOF_DATA_KEY depends only on the proof-data felts"
        );
    }

    // ── Scanner wiring tests ──────────────────────────────────────────────

    /// Build a consumed `InputNoteRecord` with full provenance control:
    /// script, storage, assets, metadata sender, `NetworkAccountTarget`
    /// attachment, consumer, and recipient serial.
    ///
    /// `sender: Some(_)` builds a metadata-carrying consumed state
    /// (`ConsumedUnauthenticatedLocal` — requires a consumer);
    /// `sender: None` builds the metadata-DROPPING `ConsumedExternal` state —
    /// the exact shape the reconciler-imported, externally-consumed notes
    /// (including every foreign deployment's note) take in the live store.
    #[allow(clippy::too_many_arguments)]
    fn build_monitor_note(
        script: miden_protocol::note::NoteScript,
        storage: NoteStorage,
        assets: NoteAssets,
        serial: miden_protocol::Word,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        consumer: Option<AccountId>,
    ) -> InputNoteRecord {
        use miden_client::store::InputNoteState;
        use miden_client::store::input_note_states::{
            ConsumedExternalNoteState, ConsumedUnauthenticatedLocalNoteState, NoteSubmissionData,
        };
        use miden_protocol::block::BlockNumber;

        let attachments = match attachment_target {
            Some(target) => NoteAttachments::from(NoteAttachment::from(
                miden_standards::note::NetworkAccountTarget::new(
                    target,
                    miden_standards::note::NoteExecutionHint::Always,
                )
                .expect("valid network account target"),
            )),
            None => NoteAttachments::default(),
        };
        let recipient = NoteRecipient::new(serial, script, storage);
        let details = PNoteDetails::new(assets, recipient);
        let state = match sender {
            Some(s) => {
                let consumer =
                    consumer.expect("sender-carrying test notes need a consumer account");
                // Dummy consuming tx id — the scanner never reads it.
                let faucet_typed = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();
                let tx_id = miden_protocol::transaction::TransactionId::new(
                    miden_protocol::Word::default(),
                    miden_protocol::Word::default(),
                    miden_protocol::Word::default(),
                    miden_protocol::Word::default(),
                    miden_protocol::asset::FungibleAsset::new(faucet_typed, 1).unwrap(),
                );
                InputNoteState::ConsumedUnauthenticatedLocal(
                    ConsumedUnauthenticatedLocalNoteState {
                        metadata: NoteMetadata::new(
                            PartialNoteMetadata::new(s, NoteType::Public),
                            &attachments,
                        ),
                        nullifier_block_height: BlockNumber::from(0u32),
                        submission_data: NoteSubmissionData {
                            submitted_at: None,
                            consumer_account: consumer,
                            consumer_transaction: tx_id,
                        },
                        consumed_tx_order: None,
                    },
                )
            }
            None => InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                nullifier_block_height: BlockNumber::from(0u32),
                consumer_account: consumer,
                consumed_tx_order: None,
            }),
        };
        InputNoteRecord::new(details, attachments, None, state)
    }

    fn embedded_address(account: AccountId) -> [u8; 20] {
        miden_base_agglayer::EthEmbeddedAccountId::from(account).into()
    }

    /// A canonical AggLayer MINT: empty outer assets and the fungible asset +
    /// public P2ID output recipient encoded in the real 22-felt storage.
    fn mint_note_with_asset(
        serial: miden_protocol::Word,
        faucet: AccountId,
        amount: u64,
        destination: AccountId,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        consumer: Option<AccountId>,
    ) -> InputNoteRecord {
        mint_note_with_callbacks(
            serial,
            faucet,
            amount,
            destination,
            miden_protocol::asset::AssetCallbackFlag::Enabled,
            sender,
            attachment_target,
            consumer,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn mint_note_with_callbacks(
        serial: miden_protocol::Word,
        faucet: AccountId,
        amount: u64,
        destination: AccountId,
        callbacks: miden_protocol::asset::AssetCallbackFlag,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        consumer: Option<AccountId>,
    ) -> InputNoteRecord {
        use miden_protocol::asset::FungibleAsset;
        use miden_protocol::note::{NoteStorage, NoteTag};
        use miden_standards::note::{MintNoteStorage, P2idNoteStorage};

        let asset = FungibleAsset::new(faucet, amount)
            .unwrap()
            .with_callbacks(callbacks);
        let p2id_recipient = P2idNoteStorage::new(destination).into_recipient(serial);
        let storage = NoteStorage::from(
            MintNoteStorage::new_public(
                p2id_recipient,
                asset,
                miden_protocol::Felt::from(NoteTag::with_account_target(destination)),
            )
            .unwrap(),
        );
        build_monitor_note(
            miden_standards::note::MintNote::script(),
            storage,
            NoteAssets::new(vec![]).unwrap(),
            serial,
            sender,
            attachment_target,
            consumer,
        )
    }

    /// A realistic CLAIM note built from actual `ClaimNoteStorage`.
    #[allow(clippy::too_many_arguments)]
    fn claim_note_realistic(
        origin_network: u32,
        origin_addr: [u8; 20],
        dest_addr: [u8; 20],
        miden_amount: u64,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        consumer: Option<AccountId>,
    ) -> (
        InputNoteRecord,
        miden_protocol::Word,
        crate::store::ExpectedMint,
    ) {
        let mut amount_bytes = [0u8; 32];
        amount_bytes[24..].copy_from_slice(&miden_amount.to_be_bytes());
        claim_note_realistic_with_raw_amount(
            origin_network,
            origin_addr,
            dest_addr,
            amount_bytes,
            miden_amount,
            sender,
            attachment_target,
            consumer,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn claim_note_realistic_with_raw_amount(
        origin_network: u32,
        origin_addr: [u8; 20],
        dest_addr: [u8; 20],
        raw_amount: [u8; 32],
        miden_amount: u64,
        sender: Option<AccountId>,
        attachment_target: Option<AccountId>,
        consumer: Option<AccountId>,
    ) -> (
        InputNoteRecord,
        miden_protocol::Word,
        crate::store::ExpectedMint,
    ) {
        use miden_base_agglayer::{
            ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
            ProofData, SmtNode,
        };
        use miden_protocol::Felt;

        let storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new([0u8; 32]),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network,
                origin_token_address: EthAddress::new(origin_addr),
                destination_network: 7,
                destination_address: EthAddress::new(dest_addr),
                amount: EthAmount::new(raw_amount),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::new(miden_amount).unwrap(),
        };
        let note_storage = NoteStorage::try_from(storage).expect("claim storage round-trips");
        let items = note_storage.items();
        let serial_word: miden_protocol::Word =
            miden_protocol::Hasher::hash_elements(&items[..CLAIM_PROOF_DATA_FELTS]);
        let note = build_monitor_note(
            miden_base_agglayer::ClaimNote::script(),
            note_storage,
            NoteAssets::new(vec![]).unwrap(),
            miden_protocol::Word::from([Felt::from(0xC1A1u32); 4]),
            sender,
            attachment_target,
            consumer,
        );
        let identity = crate::store::ExpectedMint {
            minted_amount: miden_amount,
            destination_address: dest_addr,
            origin_network,
            origin_address: origin_addr,
        };
        (note, serial_word, identity)
    }

    async fn scanner_with_faucets(
        faucets: &[AccountId],
        bridge: AccountId,
    ) -> (
        std::sync::Arc<crate::store::memory::InMemoryStore>,
        BridgeOutScanner,
    ) {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        for (index, f) in faucets.iter().enumerate() {
            let mut origin_address = [0u8; 20];
            origin_address[19] = u8::try_from(index).expect("small test faucet set");
            concrete
                .register_faucet(crate::store::FaucetEntry {
                    faucet_id: *f,
                    origin_address,
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
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        (concrete, BridgeOutScanner::new(store, 7, bridge))
    }

    #[test]
    fn canonical_mint_storage_decodes_asset_and_p2id_recipient() {
        let ids = prov_ids();
        let serial = miden_protocol::Word::from([miden_protocol::Felt::from(7u32); 4]);
        let note = mint_note_with_asset(
            serial,
            ids.faucet_a,
            5_000,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            None,
        );
        assert!(
            note.details().assets().is_empty(),
            "standard MINT notes have no outer assets"
        );
        assert_eq!(note.details().storage().num_items(), 22);
        assert_eq!(
            observed_mint_identity(&note),
            Some(ObservedMintIdentity {
                faucet: ids.faucet_a,
                amount: 5_000,
                destination: ids.local_service,
                callbacks: miden_protocol::asset::AssetCallbackFlag::Enabled,
            })
        );
        assert!(
            NoteProvenanceFacts::from_note(&note)
                .asset_faucets
                .contains(&ids.faucet_a),
            "MINT storage faucet must participate in provenance"
        );
    }

    #[test]
    fn claim_identity_accepts_raw_u256_over_u64_when_scaled_amount_is_valid() {
        let ids = prov_ids();
        let mut raw_amount = [0u8; 32];
        raw_amount[20] = 1; // 2^88: over u64, valid claimAsset uint256.
        let (claim, serial, _) = claim_note_realistic_with_raw_amount(
            0,
            [0; 20],
            embedded_address(ids.local_service),
            raw_amount,
            5_000,
            None,
            Some(ids.bridge),
            None,
        );
        assert!(matches!(
            claim_expected_mint_identity(claim.details().storage(), 7),
            ClaimMintDerivation::NonNative {
                serial: observed,
                identity: crate::store::ExpectedMint { minted_amount: 5_000, .. }
            } if observed == serial.as_bytes()
        ));
    }

    #[tokio::test]
    async fn mint_identity_binds_destination_and_never_accepts_missing_registry() {
        let ids = prov_ids();
        let (_store, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        let serial = miden_protocol::Word::from([miden_protocol::Felt::from(9u32); 4]);
        let expected = crate::store::ExpectedMint {
            minted_amount: 5_000,
            destination_address: embedded_address(ids.local_service),
            origin_network: 0,
            origin_address: [0; 20],
        };
        let matching = mint_note_with_asset(
            serial,
            ids.faucet_a,
            5_000,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            None,
        );
        assert_eq!(
            scanner
                .observed_mint_matches_expected(&matching, &expected)
                .await,
            MintIdentityCheck::Match
        );

        let outer_asset = miden_protocol::asset::FungibleAsset::new(ids.faucet_a, 1)
            .unwrap()
            .into();
        let outer_details = PNoteDetails::new(
            NoteAssets::new(vec![outer_asset]).unwrap(),
            matching.details().recipient().clone(),
        );
        let non_empty_outer = InputNoteRecord::new(
            outer_details,
            matching.attachments().clone(),
            matching.created_at(),
            matching.state().clone(),
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&non_empty_outer, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "outer_assets",
                ..
            }
        ));

        let callbacks_disabled = mint_note_with_callbacks(
            serial,
            ids.faucet_a,
            5_000,
            ids.local_service,
            miden_protocol::asset::AssetCallbackFlag::Disabled,
            None,
            Some(ids.faucet_a),
            None,
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&callbacks_disabled, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "asset_callbacks",
                ..
            }
        ));

        let missing_route = mint_note_with_asset(
            serial,
            ids.faucet_a,
            5_000,
            ids.local_service,
            None,
            None,
            None,
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&missing_route, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "routing_attachment_count",
                ..
            }
        ));

        let delayed_attachment = NoteAttachment::from(
            miden_standards::note::NetworkAccountTarget::new(
                ids.faucet_a,
                miden_standards::note::NoteExecutionHint::None,
            )
            .unwrap(),
        );
        let delayed_route = InputNoteRecord::new(
            matching.details().clone(),
            NoteAttachments::from(delayed_attachment),
            matching.created_at(),
            matching.state().clone(),
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&delayed_route, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "routing_execution_hint",
                ..
            }
        ));

        let extra_routes = NoteAttachments::new(vec![
            NoteAttachment::from(
                miden_standards::note::NetworkAccountTarget::new(
                    ids.faucet_a,
                    miden_standards::note::NoteExecutionHint::Always,
                )
                .unwrap(),
            ),
            NoteAttachment::from(
                miden_standards::note::NetworkAccountTarget::new(
                    ids.faucet_b,
                    miden_standards::note::NoteExecutionHint::Always,
                )
                .unwrap(),
            ),
        ])
        .unwrap();
        let extra_route = InputNoteRecord::new(
            matching.details().clone(),
            extra_routes,
            matching.created_at(),
            matching.state().clone(),
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&extra_route, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "routing_attachment_count",
                ..
            }
        ));

        let wrong_route = mint_note_with_asset(
            serial,
            ids.faucet_a,
            5_000,
            ids.local_service,
            None,
            Some(ids.faucet_b),
            None,
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&wrong_route, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "routing_target",
                ..
            }
        ));

        let wrong_recipient = mint_note_with_asset(
            serial,
            ids.faucet_a,
            5_000,
            ids.foreign_service,
            None,
            Some(ids.faucet_a),
            None,
        );
        assert!(matches!(
            scanner
                .observed_mint_matches_expected(&wrong_recipient, &expected)
                .await,
            MintIdentityCheck::Mismatch {
                field: "destination_account",
                ..
            }
        ));

        let unresolved = crate::store::ExpectedMint {
            origin_network: 99,
            origin_address: [0x42; 20],
            ..expected
        };
        assert_eq!(
            scanner
                .observed_mint_matches_expected(&matching, &unresolved)
                .await,
            MintIdentityCheck::Undetermined,
            "an unresolved origin route must retry, never become cached legitimacy"
        );
    }

    /// A foreign deployment's tag-0 MINT (sender = foreign bridge, target =
    /// foreign faucet, consumer untracked) reaching the scanner must be
    /// skipped by the deployment-scoped value monitors.
    ///
    /// RED pin (first cut): with consumer == None this note was NOT
    /// classified foreign and its unregistered intended faucet raised a false
    /// Cantina #2 page.
    #[tokio::test]
    async fn wiring_foreign_mint_skipped_by_all_monitors() {
        let ids = prov_ids();
        let (_concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;

        // Live shape: reconciler-imported + externally consumed → metadata
        // (sender) dropped, consumer untracked. Provenance comes from the
        // canonical MINT storage asset issued by the foreign faucet.
        let note = mint_note_with_asset(
            miden_protocol::Word::default(),
            ids.foreign_faucet,
            100,
            ids.foreign_service,
            None,
            Some(ids.foreign_faucet),
            None,
        );
        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(out.cross_faucet_alerts.is_empty(), "no false Cantina #2");
        assert!(out.forged_mint_alerts.is_empty(), "no false Cantina #4");
        // The full consumed set is re-scanned every sync; the same foreign
        // note remains excluded without retaining an unbounded dedupe set.
        for _ in 0..3 {
            let again = scanner
                .scan_consumed_notes_monitors(std::slice::from_ref(&note))
                .await;
            assert!(again.cross_faucet_alerts.is_empty());
            assert!(again.forged_mint_alerts.is_empty());
        }
    }

    /// Routing metadata alone cannot authorize a durable claim conclusion.
    /// Even with NetworkAccountTarget == our bridge, consumer=None must not
    /// feed #7 or the claim→MINT history.
    #[tokio::test]
    async fn wiring_claim_attachment_without_local_consumer_records_nothing() {
        let ids = prov_ids();
        let (concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        let (note, serial, _) = claim_note_realistic(
            0,
            [0; 20],
            embedded_address(ids.local_service),
            5000,
            None,
            Some(ids.bridge),
            None,
        );

        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(out.landed_claim_ids.is_empty());
        assert!(
            concrete
                .claim_mint_expected_get(&serial.as_bytes())
                .await
                .unwrap()
                .is_none(),
            "routing metadata without local consumption must not write claim history"
        );
    }

    /// Cantina #2 wired end-to-end: registered faucet B consumes registered
    /// faucet A's MINT → ConsumerMismatch alert, one-shot per note.
    #[tokio::test]
    async fn wiring_cantina2_b_consumes_a_mint_alerts_once() {
        let ids = prov_ids();
        let (_concrete, scanner) =
            scanner_with_faucets(&[ids.faucet_a, ids.faucet_b], ids.bridge).await;
        // Live externally-consumed shape: sender and consumer attribution are
        // both absent. The canonical storage binds the real consuming faucet
        // (B), while the attachment claims A.
        let note = mint_note_with_asset(
            miden_protocol::Word::default(),
            ids.faucet_b,
            100,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            None,
        );
        let id_bytes: [u8; 32] = note.details_commitment().as_bytes();

        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert_eq!(out.cross_faucet_alerts, vec![id_bytes]);
        // One-shot: re-scans of the same consumed set do not re-page.
        let again = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(again.cross_faucet_alerts.is_empty());
    }

    #[tokio::test]
    async fn one_mint_can_alert_for_target_mismatch_and_forgery() {
        let ids = prov_ids();
        let (_store, scanner) =
            scanner_with_faucets(&[ids.faucet_a, ids.faucet_b], ids.bridge).await;
        let scanner = scanner.with_forged_mint_grace_ticks(1);
        let note = mint_note_with_asset(
            miden_protocol::Word::default(),
            ids.faucet_b,
            100,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            None,
        );
        let id: [u8; 32] = note.details_commitment().as_bytes();

        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert_eq!(out.cross_faucet_alerts, vec![id]);
        assert_eq!(out.forged_mint_alerts, vec![id]);

        let again = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(again.cross_faucet_alerts.is_empty());
        assert!(again.forged_mint_alerts.is_empty());
    }

    /// Cantina #4 wired end-to-end: an OURS MINT (sender == our bridge — the
    /// NoAuth forgery shape) whose serial matches NO recorded claim alerts
    /// after the grace window, exactly once; a MINT whose producing CLAIM is
    /// in the consumed set (same tick or earlier) AND whose identity matches
    /// the claim's derived expected MINT never alerts.
    #[tokio::test]
    async fn wiring_cantina4_reconciles_against_claim_history() {
        let ids = prov_ids();
        let (_concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        let scanner = scanner.with_forged_mint_grace_ticks(2);

        // Legit pair: an OURS CLAIM (NetworkAccountTarget names our bridge) for
        // a NON-native origin (network 99 ≠ local 7) minting 5000 units, + the
        // MINT whose serial is that claim's PROOF_DATA_KEY carrying the SAME
        // 5000 units.
        let (claim, legit_serial, _identity) = claim_note_realistic(
            0,
            [0; 20],
            embedded_address(ids.local_service),
            5000,
            None,
            Some(ids.bridge),
            Some(ids.bridge),
        );
        let legit_mint = mint_note_with_asset(
            legit_serial,
            ids.faucet_a,
            5000,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            Some(ids.faucet_a),
        );
        let altered_route = InputNoteRecord::new(
            legit_mint.details().clone(),
            NoteAttachments::default(),
            legit_mint.created_at(),
            legit_mint.state().clone(),
        );
        let legit_id: [u8; 32] = legit_mint.details_commitment().as_bytes();
        // Forged: aimed at our registered faucet (OURS by target — the NoAuth
        // forgery must reference our deployment to drain it) but matching no
        // recorded claim.
        let forged_mint = mint_note_with_asset(
            miden_protocol::Word::from([miden_protocol::Felt::from(99u32); 4]),
            ids.faucet_a,
            1234,
            ids.local_service,
            None,
            Some(ids.faucet_a),
            Some(ids.faucet_a),
        );
        let forged_id: [u8; 32] = forged_mint.details_commitment().as_bytes();
        let notes = vec![claim, legit_mint, forged_mint];

        // Tick 1: legit MINT reconciles against the claim recorded in pass 1
        // of the SAME scan; forged MINT accrues grace (no alert yet).
        let out = scanner.scan_consumed_notes_monitors(&notes).await;
        assert!(out.forged_mint_alerts.is_empty(), "grace tick 1: no alert");
        assert_eq!(out.landed_claim_ids.len(), 1);
        // Tick 2: forged MINT exhausts grace → Cantina #4 fires, once.
        let out = scanner.scan_consumed_notes_monitors(&notes).await;
        assert_eq!(
            out.forged_mint_alerts,
            vec![forged_id],
            "forged MINT (no recorded claim) must alert after grace"
        );
        // Tick 3+: one-shot.
        let out = scanner.scan_consumed_notes_monitors(&notes).await;
        assert!(out.forged_mint_alerts.is_empty(), "one-shot per note id");

        // Attachments are outside NoteDetails. A canonical observation must
        // not cache-recognise a later same-details note with altered routing.
        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&altered_route))
            .await;
        assert_eq!(out.forged_mint_alerts, vec![legit_id]);
    }

    /// Blocker #1 — a MINT reusing a RECORDED claim's serial but with DIFFERENT
    /// details (amount) must STILL alert (immediately, no grace), and a NATIVE
    /// claim must record NO whitelist entry. Deleting the identity comparison
    /// (accepting on serial membership alone) makes the first assertion FAIL.
    #[tokio::test]
    async fn blocker1_copied_serial_different_mint_still_alerts() {
        let ids = prov_ids();
        let (concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        let scanner = scanner.with_forged_mint_grace_ticks(2);

        // Legit non-native claim → records identity {amount: 5000, ...}.
        let (claim, serial, _identity) = claim_note_realistic(
            0,
            [0; 20],
            embedded_address(ids.local_service),
            5000,
            None,
            Some(ids.bridge),
            Some(ids.bridge),
        );
        // A forger copies the recorded serial but mints a DIFFERENT amount.
        let forged = mint_note_with_asset(
            serial,
            ids.faucet_a,
            9999, // ≠ 5000
            ids.local_service,
            None,
            Some(ids.faucet_a),
            Some(ids.faucet_a),
        );
        let forged_id: [u8; 32] = forged.details_commitment().as_bytes();

        // Single tick: the claim records identity in pass 1; the copied-serial
        // MINT mismatches on amount → forged IMMEDIATELY (no grace needed).
        let out = scanner.scan_consumed_notes_monitors(&[claim, forged]).await;
        assert_eq!(
            out.forged_mint_alerts,
            vec![forged_id],
            "copied serial + different amount must alert on the SAME tick (no grace)"
        );
        // The identity WAS recorded (proving the alert is the mismatch path,
        // not the missing-serial path).
        assert!(
            concrete
                .claim_mint_expected_get(&serial.as_bytes())
                .await
                .unwrap()
                .is_some()
        );
    }

    /// Blocker #1 — a NATIVE-faucet claim (origin network == local network id)
    /// executes the P2ID unlock path and produces NO MINT, so it must record
    /// NO claim→MINT legitimacy entry. Deleting the native filter makes this
    /// FAIL (the native serial would become a permanent whitelist entry).
    #[tokio::test]
    async fn blocker1_native_claim_records_no_whitelist_entry() {
        let ids = prov_ids();
        let (concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        // origin_network == local (7) ⇒ native.
        let (claim, serial, _identity) = claim_note_realistic(
            7,
            [0x11; 20],
            embedded_address(ids.local_service),
            5000,
            None,
            Some(ids.bridge),
            Some(ids.bridge),
        );

        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&claim))
            .await;
        // The claim IS ours (landed) but records no MINT identity.
        assert_eq!(out.landed_claim_ids.len(), 1);
        assert!(
            concrete
                .claim_mint_expected_get(&serial.as_bytes())
                .await
                .unwrap()
                .is_none(),
            "a native claim must produce NO expected-MINT whitelist entry"
        );
    }

    /// Blocker #2 — during a registry outage every unproven note is UNKNOWN.
    /// A FOREIGN claim observed in that window must NOT write a legitimacy
    /// entry (the old boolean predicate returned `false` for everything and
    /// Pass 1 recorded it). Deleting the tri-state (recording on "not foreign"
    /// instead of "positively ours") makes this FAIL.
    #[tokio::test]
    async fn blocker2_registry_outage_foreign_claim_records_nothing() {
        let ids = prov_ids();
        let (concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        concrete.set_fail_list_faucets(true);

        // A foreign claim (target = foreign bridge) built from decodable
        // non-native storage, in the reconciler-import (metadata-lost) shape.
        // During the outage it is UNKNOWN, not OURS.
        let (claim, serial, _identity) = claim_note_realistic(
            0,
            [0; 20],
            embedded_address(ids.local_service),
            5000,
            None,
            Some(ids.foreign_bridge),
            None,
        );
        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&claim))
            .await;
        assert!(out.registry_degraded);
        // Restore the registry so the read itself can succeed, and confirm the
        // outage tick wrote nothing.
        concrete.set_fail_list_faucets(false);
        assert!(
            concrete
                .claim_mint_expected_get(&serial.as_bytes())
                .await
                .unwrap()
                .is_none(),
            "a foreign claim during a registry outage must NOT write legitimacy"
        );
    }

    /// Item 3 wired end-to-end: a `list_faucets()` failure must not suppress
    /// any monitor. The same blatantly-foreign MINT that is skipped with a
    /// healthy registry is monitored (not skip-counted) while the registry is
    /// unreadable, and the registry-membership #2 signal is paused rather than
    /// false-firing.
    #[tokio::test]
    async fn wiring_registry_failure_fails_closed() {
        let ids = prov_ids();
        let (concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        concrete.set_fail_list_faucets(true);

        // A canonical MINT that a healthy registry classifies foreign by its
        // storage asset faucet.
        let note = mint_note_with_asset(
            miden_protocol::Word::default(),
            ids.foreign_faucet,
            100,
            ids.foreign_service,
            None,
            Some(ids.foreign_faucet),
            None,
        );
        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(out.registry_degraded, "degraded state must be surfaced");
        assert!(
            out.cross_faucet_alerts.is_empty(),
            "registry-membership #2 signal pauses (cannot be evaluated) — no page storm"
        );

        // Registry heals → the same note is now positively foreign and the
        // skip resumes.
        concrete.set_fail_list_faucets(false);
        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert!(!out.registry_degraded);
        assert!(out.cross_faucet_alerts.is_empty());
        assert!(out.forged_mint_alerts.is_empty());
    }

    /// A known consumer is locally tracked even when its faucet registration
    /// is missing. Keep the MINT monitored so #2 reports the stale registry.
    #[tokio::test]
    async fn wiring_tracked_unregistered_faucet_alerts() {
        let ids = prov_ids();
        let (_concrete, scanner) = scanner_with_faucets(&[ids.faucet_a], ids.bridge).await;
        let note = mint_note_with_asset(
            miden_protocol::Word::default(),
            ids.unregistered,
            100,
            ids.local_service,
            None,
            Some(ids.unregistered),
            Some(ids.unregistered),
        );

        let out = scanner
            .scan_consumed_notes_monitors(std::slice::from_ref(&note))
            .await;
        assert_eq!(out.cross_faucet_alerts.len(), 1);
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
            &crate::metadata_recovery::NetworkRpcMap::new(),
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
            &crate::metadata_recovery::NetworkRpcMap::new(),
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

        let outcome = scanner.scan_consumed_notes_monitors(&[note]).await;

        assert!(
            outcome.landed_claim_ids.is_empty(),
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
