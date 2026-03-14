//! Bridge-Out (L2 → L1) — Detect B2AGG note consumption and emit BridgeEvent logs.
//!
//! When the bridge account consumes a B2AGG note, assets are burned and a corresponding
//! deposit is recorded on the L2 side. This module scans for consumed B2AGG notes and
//! emits synthetic `BridgeEvent` EVM logs so the bridge-service can index them.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::log_synthesis::LogStore;
use crate::miden_client::{MidenClientLib, SyncListener};
use miden_base_agglayer::B2AggNote;
use miden_client::store::InputNoteRecord;
use miden_client::store::NoteFilter;
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::note::{NoteDetails, NoteStorage};
use parking_lot::RwLock;
use sha3::{Digest, Keccak256};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

const LEAF_TYPE_ASSET: u8 = 0;

// BRIDGE OUT TRACKER
// ================================================================================================

/// Persistent dedup tracker for processed B2AGG notes + monotonic deposit counter.
pub struct BridgeOutTracker {
    processed_notes: RwLock<HashSet<String>>,
    deposit_counter: RwLock<u32>,
    persistence_path: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TrackerState {
    processed_notes: Vec<String>,
    deposit_counter: u32,
}

impl BridgeOutTracker {
    pub fn new(persistence_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let (processed_notes, deposit_counter) = if let Some(ref path) = persistence_path {
            if path.exists() {
                let data = std::fs::read_to_string(path)?;
                let state: TrackerState = serde_json::from_str(&data)?;
                (
                    state.processed_notes.into_iter().collect(),
                    state.deposit_counter,
                )
            } else {
                (HashSet::new(), 0)
            }
        } else {
            (HashSet::new(), 0)
        };
        Ok(Self {
            processed_notes: RwLock::new(processed_notes),
            deposit_counter: RwLock::new(deposit_counter),
            persistence_path,
        })
    }

    pub fn is_processed(&self, note_id: &str) -> bool {
        self.processed_notes.read().contains(note_id)
    }

    /// Mark a note as processed, increment deposit counter, persist.
    /// Returns the deposit count assigned to this note.
    pub fn mark_processed(&self, note_id: String) -> u32 {
        let mut notes = self.processed_notes.write();
        notes.insert(note_id);

        let mut counter = self.deposit_counter.write();
        let deposit_count = *counter;
        *counter += 1;

        drop(notes);
        drop(counter);
        self.persist();
        deposit_count
    }

    fn persist(&self) {
        let Some(ref path) = self.persistence_path else {
            return;
        };
        let notes = self.processed_notes.read();
        let counter = *self.deposit_counter.read();
        let state = TrackerState {
            processed_notes: notes.iter().cloned().collect(),
            deposit_counter: counter,
        };
        drop(notes);

        let Ok(data) = serde_json::to_string_pretty(&state) else {
            tracing::error!("BridgeOutTracker: failed to serialize state");
            return;
        };
        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!(
                "BridgeOutTracker: failed to write {}: {e}",
                tmp_path.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::error!(
                "BridgeOutTracker: failed to rename to {}: {e}",
                path.display()
            );
        }
    }
}

impl Default for BridgeOutTracker {
    fn default() -> Self {
        Self::new(None).unwrap()
    }
}

// B2AGG NOTE PARSING
// ================================================================================================

/// Check if a note is a B2AGG note by comparing script roots.
pub fn is_b2agg_note(details: &NoteDetails) -> bool {
    details.script().root() == B2AggNote::script_root()
}

/// Extract destination_network and destination_address from B2AGG note storage.
///
/// Storage layout (6 felts):
/// - items()[0]: destination_network (u32, byte-swapped via u32::from_le_bytes(dest.to_be_bytes()))
/// - items()[1..6]: destination_address (5 packed u32 felts = 20 bytes)
pub fn parse_b2agg_storage(storage: &NoteStorage) -> (u32, [u8; 20]) {
    let items = storage.items();

    // Reverse the byte-swap applied during note creation:
    // build_note_storage does: u32::from_le_bytes(destination_network.to_be_bytes())
    // So to recover: u32::from_le_bytes(felt_value.to_be_bytes())
    let raw_network = items[0].as_int() as u32;
    let destination_network = u32::from_le_bytes(raw_network.to_be_bytes());

    // Reconstruct 20-byte address from 5 packed u32 felts (big-endian limb order).
    // Each felt holds a u32 value that represents 4 bytes in little-endian byte order.
    // to_elements() in EthAddressFormat uses bytes_to_packed_u32_felts which reads
    // each 4-byte chunk as a little-endian u32.
    let mut address = [0u8; 20];
    for i in 0..5 {
        let limb = items[1 + i].as_int() as u32;
        address[i * 4..(i + 1) * 4].copy_from_slice(&limb.to_le_bytes());
    }

    (destination_network, address)
}

// FAUCET ORIGIN RESOLUTION
// ================================================================================================

/// Origin token info for a faucet.
pub struct FaucetOriginInfo {
    pub origin_network: u32,
    pub origin_address: [u8; 20],
    pub scale: u8,
}

/// Resolve faucet origin info by matching against known faucet account IDs.
///
/// Currently hardcoded based on init.rs defaults:
/// - faucet_eth: origin_address=[0;20], origin_network=0, scale=10
/// - faucet_agg: origin_address=[0;20], origin_network=0, scale=0 (default)
pub fn resolve_faucet_origin(faucet_id: AccountId, accounts: &AccountsConfig) -> FaucetOriginInfo {
    if faucet_id == accounts.faucet_eth.0 {
        FaucetOriginInfo {
            origin_network: 0,
            origin_address: [0u8; 20],
            scale: 10,
        }
    } else {
        // Default for faucet_agg and any unknown faucet
        FaucetOriginInfo {
            origin_network: 0,
            origin_address: [0u8; 20],
            scale: 0,
        }
    }
}

/// Reverse-scale a Miden amount back to origin token decimals.
/// origin_amount = miden_amount * 10^scale
fn reverse_scale_amount(miden_amount: u64, scale: u8) -> u128 {
    let factor = 10u128.pow(scale as u32);
    (miden_amount as u128) * factor
}

// BRIDGE OUT SCANNER
// ================================================================================================

/// Scans for consumed B2AGG notes and emits synthetic BridgeEvent logs.
pub struct BridgeOutScanner {
    log_store: Arc<LogStore>,
    block_state: Arc<BlockState>,
    accounts: AccountsConfig,
    tracker: BridgeOutTracker,
    _bridge_account_id: AccountId,
}

impl BridgeOutScanner {
    pub fn new(
        log_store: Arc<LogStore>,
        block_state: Arc<BlockState>,
        accounts: AccountsConfig,
        tracker: BridgeOutTracker,
        bridge_account_id: AccountId,
    ) -> Self {
        Self {
            log_store,
            block_state,
            accounts,
            tracker,
            _bridge_account_id: bridge_account_id,
        }
    }

    fn process_consumed_note(&self, note: &InputNoteRecord, block_number: u64) {
        let note_id_str = note.id().to_string();

        if self.tracker.is_processed(&note_id_str) {
            return;
        }

        let details = note.details();
        if !is_b2agg_note(details) {
            return;
        }

        // Parse B2AGG storage
        let (destination_network, destination_address) = parse_b2agg_storage(details.storage());

        // Get the fungible asset
        let Some(fungible_asset) = details.assets().iter_fungible().next() else {
            tracing::warn!("B2AGG note {note_id_str} has no fungible asset, skipping");
            return;
        };
        let faucet_id = fungible_asset.faucet_id();
        let miden_amount = fungible_asset.amount();

        // Resolve origin info
        let origin = resolve_faucet_origin(faucet_id, &self.accounts);
        let origin_amount = reverse_scale_amount(miden_amount, origin.scale);

        // Generate synthetic tx hash
        let tx_hash = {
            let mut hasher = Keccak256::new();
            hasher.update(b"miden-bridge-out-");
            hasher.update(note_id_str.as_bytes());
            let hash: [u8; 32] = hasher.finalize().into();
            format!("0x{}", hex::encode(hash))
        };

        let block_hash = self.block_state.get_block_hash(block_number);
        let deposit_count = self.tracker.mark_processed(note_id_str.clone());

        // Emit BridgeEvent log
        self.log_store.add_bridge_event(
            get_bridge_address(),
            block_number,
            block_hash,
            &tx_hash,
            LEAF_TYPE_ASSET,
            origin.origin_network,
            &origin.origin_address,
            destination_network,
            &destination_address,
            origin_amount,
            deposit_count,
        );

        tracing::info!(
            note_id = %note_id_str,
            synthetic_tx_hash = %tx_hash,
            deposit_count,
            destination_network,
            amount = origin_amount,
            block_number,
            "emitted BridgeEvent for consumed B2AGG note"
        );
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

        // Store events at current_block + 1 so they appear in a block the bridge-service
        // hasn't synced yet. With forceSyncChunk=true, the bridge never re-queries old
        // blocks, so events at the current block are missed if the bridge already synced it.
        let block_number = self.block_state.current_block_number() + 1;

        for note in &consumed_notes {
            self.process_consumed_note(note, block_number);
        }

        Ok(())
    }
}

// BRIDGE EVENT ABI ENCODING
// ================================================================================================

/// ABI-encode BridgeEvent data for synthetic log emission.
///
/// BridgeEvent(uint8 leafType, uint32 originNetwork, address originAddress,
///             uint32 destinationNetwork, address destinationAddress,
///             uint256 amount, bytes metadata, uint32 depositCount)
///
/// Per Solidity ABI encoding, all static types are padded to 32 bytes,
/// and `bytes metadata` is encoded as an offset + length + data.
pub fn encode_bridge_event_data(
    leaf_type: u8,
    origin_network: u32,
    origin_address: &[u8; 20],
    destination_network: u32,
    destination_address: &[u8; 20],
    amount: u128,
    deposit_count: u32,
) -> String {
    // 8 words of 32 bytes each for the static parts + dynamic metadata
    let mut data = Vec::with_capacity(9 * 32);

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

    // amount (uint256 — u128 in high bytes of 32-byte slot)
    data.extend_from_slice(&[0u8; 16]);
    data.extend_from_slice(&amount.to_be_bytes());

    // metadata (bytes) — offset pointer (points to word 8 = 0x100 = 256)
    data.extend_from_slice(&[0u8; 31]);
    data.push(0); // offset = 8 * 32 = 256
    // Fix: offset is after all 8 params. Params: leafType, originNetwork, originAddress,
    // destinationNetwork, destinationAddress, amount, metadata_offset, depositCount = 8 words
    // So metadata starts at byte 8*32 = 256 = 0x100
    // Overwrite the metadata offset
    let offset_pos = 6 * 32; // 7th parameter (0-indexed: 6)
    data[offset_pos..offset_pos + 32].fill(0);
    let offset: u32 = 8 * 32; // 256
    data[offset_pos + 28..offset_pos + 32].copy_from_slice(&offset.to_be_bytes());

    // depositCount (uint32 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 28]);
    data.extend_from_slice(&deposit_count.to_be_bytes());

    // metadata dynamic part: length (0) + no data (empty metadata)
    data.extend_from_slice(&[0u8; 32]); // length = 0

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
            0,           // deposit_count
        );
        // 9 words (8 params + 1 metadata length) = 288 bytes = 576 hex chars + "0x" prefix
        assert_eq!(data.len(), 2 + 9 * 32 * 2);
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
        assert_eq!(reverse_scale_amount(1000, 0), 1000);
        // ETH: scale=10
        assert_eq!(reverse_scale_amount(1000, 10), 10_000_000_000_000);
        // 1 unit with scale=18
        assert_eq!(reverse_scale_amount(1, 18), 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_bridge_out_tracker_dedup() {
        let tracker = BridgeOutTracker::new(None).unwrap();
        assert!(!tracker.is_processed("note1"));
        let count = tracker.mark_processed("note1".to_string());
        assert_eq!(count, 0);
        assert!(tracker.is_processed("note1"));
        let count2 = tracker.mark_processed("note2".to_string());
        assert_eq!(count2, 1);
    }

    #[test]
    fn test_bridge_out_tracker_persistence() {
        let dir = std::env::temp_dir().join(format!(
            "bridge_out_tracker_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge_out_tracker.json");

        {
            let tracker = BridgeOutTracker::new(Some(path.clone())).unwrap();
            tracker.mark_processed("note_a".to_string());
            tracker.mark_processed("note_b".to_string());
        }

        let tracker = BridgeOutTracker::new(Some(path.clone())).unwrap();
        assert!(tracker.is_processed("note_a"));
        assert!(tracker.is_processed("note_b"));
        assert!(!tracker.is_processed("note_c"));
        assert_eq!(*tracker.deposit_counter.read(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
