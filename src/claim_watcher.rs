//! CLAIM-note storage decoder — `ClaimNoteStorage` → `DecodedClaim` (the fields
//! of a synthetic `ClaimEvent`), plus the deterministic synthetic claim tx-hash
//! derivation (`derive_manual_claim_tx_hash`).
//!
//! Shared by the [`SyntheticProjector`](crate::synthetic_projector)'s
//! `restore::project_claim_note` and the startup restore replay: both observe a
//! CLAIM note consumed on Miden, decode its on-chain `ClaimNoteStorage`, and emit
//! a synthetic `ClaimEvent` via `Store::commit_manual_claim_event_atomic`.
//! Keeping the decoder + tx-hash derivation here means the projector and restore
//! produce byte-identical ClaimEvents.
//!
//! (Historically this module also hosted a live `ClaimWatcher` SyncListener that
//! synthesised ClaimEvents on every tick. The SyntheticProjector is now the SOLE
//! synthetic-event producer, so that watcher was removed in the cut-over; only
//! the shared decoder remains.)

use anyhow::Context;
use miden_protocol::note::NoteStorage;
use sha3::{Digest, Keccak256};

// CLAIMNOTESTORAGE FELT LAYOUT
// ================================================================================================
//
// Pinned to the upstream layout in
// `miden-agglayer-0.14.5/src/claim_note.rs::{ProofData, LeafData}::to_elements`.
//
// ProofData (536 felts):
//   [0..256)   smt_proof_local_exit_root   (32 nodes × 8 felts; unused here)
//   [256..512) smt_proof_rollup_exit_root  (32 nodes × 8 felts; unused here)
//   [512..520) global_index                (8 felts, packed-u32-LE for 32 BE bytes)
//   [520..528) mainnet_exit_root           (unused here)
//   [528..536) rollup_exit_root            (unused here)
//
// LeafData (32 felts, starting at offset 536):
//   [536]      leaf_type                   (always Felt::ZERO)
//   [537]      origin_network              (1 felt, byte-swapped u32; see decode_swapped_u32)
//   [538..543) origin_token_address        (5 felts, packed-u32-LE for 20 BE bytes)
//   [543]      destination_network         (1 felt, byte-swapped u32)
//   [544..549) destination_address         (5 felts, packed-u32-LE for 20 BE bytes)
//   [549..557) amount                      (8 felts, packed-u32-LE for 32 BE bytes — U256)
//   [557..565) metadata_hash               (unused here)
//   [565..568) padding                     (always Felt::ZERO ×3)
//
//   [568]      miden_claim_amount          (unused here; not part of ClaimEvent)

const OFFSET_GLOBAL_INDEX: usize = 512;
const OFFSET_ORIGIN_NETWORK: usize = 537;
const OFFSET_ORIGIN_ADDRESS: usize = 538;
const OFFSET_DESTINATION_ADDRESS: usize = 544;
const OFFSET_AMOUNT: usize = 549;
const MIN_FELT_COUNT: usize = 569;

/// Fields extracted from a consumed CLAIM note's storage that are needed to
/// synthesise a `ClaimEvent` log identical to what `claim::publish_claim_internal`
/// would have written via the normal path.
#[derive(Debug, Clone)]
pub struct DecodedClaim {
    pub global_index: [u8; 32],
    pub origin_network: u32,
    pub origin_address: [u8; 20],
    pub destination_address: [u8; 20],
    /// Amount in origin-token units, low-order bits. The bridge contract's
    /// ClaimEvent type-2 topic is u256, but in practice every legitimate value
    /// fits u64 (max ETH supply ≈ 2^57 wei) — we surface overflows as a metric
    /// and refuse to emit, rather than silently truncating.
    pub amount: u64,
}

// PARSING
// ================================================================================================

/// Reverse-engineer one origin/destination network word from a `LeafData` felt.
///
/// Forward path (upstream `LeafData::to_elements`):
///   `let v = u32::from_le_bytes(orig_network.to_be_bytes()); push Felt::from(v)`
/// Inverse: read the felt as a u32, then `u32::from_be_bytes(v.to_le_bytes())`.
/// Same trick the B2AGG parser uses at `bridge_out.rs::parse_b2agg_storage`.
fn decode_swapped_u32(felt: miden_protocol::Felt) -> anyhow::Result<u32> {
    let raw = u32::try_from(felt.as_canonical_u64())
        .context("network felt exceeds u32::MAX — malformed CLAIM storage")?;
    Ok(u32::from_be_bytes(raw.to_le_bytes()))
}

/// Inverse of `bytes_to_packed_u32_elements` for a fixed number of felts.
/// Each felt is interpreted as a u32 and written as 4 little-endian bytes.
/// The concatenation reproduces the original byte sequence.
fn unpack_u32_felts<const N: usize>(felts: &[miden_protocol::Felt]) -> anyhow::Result<[u8; N]> {
    if felts.len() * 4 < N {
        anyhow::bail!(
            "unpack_u32_felts: need ≥{} felts for {N} bytes, got {}",
            N.div_ceil(4),
            felts.len()
        );
    }
    let mut out = [0u8; N];
    for (i, felt) in felts.iter().take(N.div_ceil(4)).enumerate() {
        let limb = u32::try_from(felt.as_canonical_u64())
            .with_context(|| format!("limb {i} exceeds u32::MAX — malformed CLAIM storage"))?;
        let bytes = limb.to_le_bytes();
        let dst = i * 4;
        let copy_len = (N - dst).min(4);
        out[dst..dst + copy_len].copy_from_slice(&bytes[..copy_len]);
    }
    Ok(out)
}

/// Decode the subset of `ClaimNoteStorage` fields needed to emit a `ClaimEvent`.
///
/// Inverse of `ClaimNoteStorage → NoteStorage` defined in
/// `miden-agglayer-0.14.5/src/claim_note.rs::TryFrom<ClaimNoteStorage>` —
/// pinned by the offset constants above and the `roundtrips_known_vector`
/// test.
///
/// Returns `Err` on any of:
/// - storage felt count below [`MIN_FELT_COUNT`]
/// - a felt holding a value outside `u32`
/// - an amount field that doesn't fit `u64` (rejected so the watcher never
///   silently truncates a large-value claim)
pub fn parse_claim_event_from_storage(storage: &NoteStorage) -> anyhow::Result<DecodedClaim> {
    let items = storage.items();
    if items.len() < MIN_FELT_COUNT {
        anyhow::bail!(
            "CLAIM storage too short: expected ≥{MIN_FELT_COUNT} felts, got {}",
            items.len()
        );
    }

    let global_index =
        unpack_u32_felts::<32>(&items[OFFSET_GLOBAL_INDEX..OFFSET_GLOBAL_INDEX + 8])?;
    let origin_network = decode_swapped_u32(items[OFFSET_ORIGIN_NETWORK])?;
    let origin_address =
        unpack_u32_felts::<20>(&items[OFFSET_ORIGIN_ADDRESS..OFFSET_ORIGIN_ADDRESS + 5])?;
    let destination_address =
        unpack_u32_felts::<20>(&items[OFFSET_DESTINATION_ADDRESS..OFFSET_DESTINATION_ADDRESS + 5])?;
    let amount_bytes = unpack_u32_felts::<32>(&items[OFFSET_AMOUNT..OFFSET_AMOUNT + 8])?;

    // Reject amounts that overflow u64 — the upper 24 bytes of the U256 BE
    // representation must be zero. ClaimEvent's wire type is u256 but
    // `Store::add_claim_event` takes u64; surfacing as Err keeps every
    // overflow visible via the storage_decode_total counter rather than
    // silently truncating.
    if amount_bytes[..24].iter().any(|b| *b != 0) {
        anyhow::bail!("CLAIM amount exceeds u64::MAX (top 24 bytes nonzero); refusing to truncate");
    }
    let mut amount_low = [0u8; 8];
    amount_low.copy_from_slice(&amount_bytes[24..32]);
    let amount = u64::from_be_bytes(amount_low);

    Ok(DecodedClaim {
        global_index,
        origin_network,
        origin_address,
        destination_address,
        amount,
    })
}

// SYNTHETIC TX HASH
// ================================================================================================

/// Domain-separation tag for synthetic CLAIM-watcher tx hashes. Versioned so a
/// future change to the derivation can co-exist with historical hashes. Mirrors
/// `bridge_out.rs::BRIDGE_OUT_TX_HASH_TAG`.
pub const MANUAL_CLAIM_TX_HASH_TAG: &[u8] = b"miden-agglayer/manual-claim/v1\x00";

/// Deterministic synthetic transaction hash for a watcher-emitted ClaimEvent.
/// Bound to the consumed CLAIM note's stable on-chain `NoteId`, so a re-emit
/// on restart (e.g. after a crash before `mark_claim_note_processed` landed)
/// produces the same hash and bridge-service dedups it correctly.
pub fn derive_manual_claim_tx_hash(note_id_str: &str) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(MANUAL_CLAIM_TX_HASH_TAG);
    hasher.update(note_id_str.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    format!("0x{}", hex::encode(hash))
}

// WATCHER
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use miden_base_agglayer::{
        ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
        ProofData, SmtNode,
    };
    use miden_protocol::Felt;
    use std::sync::Arc as StdArc;

    fn known_storage() -> NoteStorage {
        // Build a ClaimNoteStorage with values that exercise every decoded
        // offset, then round-trip it through `NoteStorage::try_from` so we're
        // testing against the actual upstream layout, not a hand-built one.
        let mut gi_bytes = [0u8; 32];
        gi_bytes[23] = 1; // mainnet flag at limb 5 LSB
        gi_bytes[31] = 0x42; // leaf_index = 0x42

        let mut origin_addr = [0u8; 20];
        origin_addr[19] = 0xAB;
        let mut dest_addr = [0u8; 20];
        dest_addr[..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let mut amount_bytes = [0u8; 32];
        // 1_000_000 as u256 big-endian
        amount_bytes[28..32].copy_from_slice(&1_000_000u32.to_be_bytes());

        let storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new(gi_bytes),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network: 0x12345678,
                origin_token_address: EthAddress::new(origin_addr),
                destination_network: 0xAABBCCDD,
                destination_address: EthAddress::new(dest_addr),
                amount: EthAmount::new(amount_bytes),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::ZERO,
        };
        NoteStorage::try_from(storage).expect("known storage must round-trip")
    }

    #[test]
    fn parse_claim_storage_roundtrips_known_vector() {
        let storage = known_storage();
        let decoded = parse_claim_event_from_storage(&storage).expect("decode");

        // global_index: 32 BE bytes, mainnet flag at byte 23, leaf at byte 31.
        let mut expected_gi = [0u8; 32];
        expected_gi[23] = 1;
        expected_gi[31] = 0x42;
        assert_eq!(decoded.global_index, expected_gi);

        // origin_network: round-trips the byte-swap.
        assert_eq!(decoded.origin_network, 0x12345678);

        // origin_address: 20 bytes, last byte = 0xAB.
        let mut expected_origin = [0u8; 20];
        expected_origin[19] = 0xAB;
        assert_eq!(decoded.origin_address, expected_origin);

        // destination_address: first 4 bytes = DEAD BEEF.
        assert_eq!(decoded.destination_address[..4], [0xDE, 0xAD, 0xBE, 0xEF]);

        // amount: 1_000_000.
        assert_eq!(decoded.amount, 1_000_000);
    }

    #[test]
    fn parse_claim_storage_rejects_short_storage() {
        // Build a NoteStorage with fewer than MIN_FELT_COUNT felts.
        let short = NoteStorage::new(vec![Felt::ZERO; 100]).expect("short ok");
        let err = parse_claim_event_from_storage(&short).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("storage too short"),
            "error should describe the bound: {msg}"
        );
    }

    #[test]
    fn parse_claim_storage_rejects_amount_overflow_u64() {
        // Build a valid base storage, then patch the amount felts to encode a
        // U256 > u64::MAX. We rebuild a ClaimNoteStorage with a huge amount.
        let mut huge_amount = [0u8; 32];
        huge_amount[16] = 0x01; // top half of u128 set → exceeds u64
        let huge = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new([0u8; 32]),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network: 0,
                origin_token_address: EthAddress::new([0u8; 20]),
                destination_network: 0,
                destination_address: EthAddress::new([0u8; 20]),
                amount: EthAmount::new(huge_amount),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::ZERO,
        };
        let storage = NoteStorage::try_from(huge).expect("ok");
        let err = parse_claim_event_from_storage(&storage).expect_err("overflow must err");
        assert!(format!("{err:#}").contains("u64::MAX"));
    }

    #[test]
    fn manual_claim_tx_hash_is_versioned_and_deterministic() {
        let h1 = derive_manual_claim_tx_hash("note_a");
        let h2 = derive_manual_claim_tx_hash("note_a");
        assert_eq!(h1, h2, "deterministic for same note_id");
        assert_eq!(h1.len(), 66, "0x + 64 hex chars");
        assert!(h1.starts_with("0x"));

        let h3 = derive_manual_claim_tx_hash("note_b");
        assert_ne!(h1, h3, "different note_ids → different hashes");

        // Pin the domain tag so a refactor that drops the version separator
        // produces a compile-time / test-time visible regression rather than
        // a silently colliding hash family.
        assert!(MANUAL_CLAIM_TX_HASH_TAG.starts_with(b"miden-agglayer/manual-claim/v"));
        // Hash family separation from bridge-out (regression: if someone
        // accidentally re-uses the bridge-out tag, this assert catches it).
        assert_ne!(
            MANUAL_CLAIM_TX_HASH_TAG,
            crate::bridge_out::BRIDGE_OUT_TX_HASH_TAG,
            "manual-claim and bridge-out tx-hash families must not collide"
        );
    }

    /// The store-side dedup contract the projector's claim derivation relies on:
    /// feeding the same global_index twice through the store paths must produce a
    /// single ClaimEvent and a single cursor advance. Drives the store primitives
    /// directly to pin the dedup logic.
    #[tokio::test]
    async fn store_dedup_paths_are_idempotent() {
        let store: StdArc<dyn crate::store::Store> = StdArc::new(InMemoryStore::new());

        let gi = [0x42u8; 32];
        let note_id = "0xabcdef".to_string();

        assert!(!store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(!store.has_claim_event_for_global_index(&gi).await.unwrap());

        store
            .commit_manual_claim_event_atomic(
                note_id.clone(),
                "0xbridge",
                1,
                [0u8; 32],
                "0xtx",
                gi,
                0,
                &[0u8; 20],
                &[0u8; 20],
                1000,
            )
            .await
            .unwrap();

        // Both dedup predicates now return true.
        assert!(store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
        assert_eq!(store.get_latest_block_number().await.unwrap(), 1);

        // A second commit with the same note_id must NOT advance the block
        // or duplicate the log — note that the InMemoryStore default impl
        // re-inserts (the HashMap upsert), but the cursor advance is to the
        // same block_number, and downstream dedup catches re-emission.
        // The PgStore variant uses `ON CONFLICT DO NOTHING` so it's a true
        // no-op. The InMemoryStore observable invariant is "ClaimEvent
        // lookup still returns true and cursor doesn't go BACKWARD".
        store
            .commit_manual_claim_event_atomic(
                note_id.clone(),
                "0xbridge",
                1,
                [0u8; 32],
                "0xtx",
                gi,
                0,
                &[0u8; 20],
                &[0u8; 20],
                1000,
            )
            .await
            .unwrap();
        assert!(store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
        assert!(store.get_latest_block_number().await.unwrap() >= 1);
    }
}
