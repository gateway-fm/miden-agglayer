//! Bridge-Out (L2 → L1) — Detect B2AGG note consumption and emit BridgeEvent logs.
//!
//! When the bridge account consumes a B2AGG note, assets are burned and a corresponding
//! deposit is recorded on the L2 side. This module scans for consumed B2AGG notes and
//! emits synthetic `BridgeEvent` EVM logs so the bridge-service can index them.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
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

const LEAF_TYPE_ASSET: u8 = 0;

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
pub fn parse_b2agg_storage(storage: &NoteStorage) -> anyhow::Result<(u32, [u8; 20])> {
    let items = storage.items();

    // Reverse the byte-swap applied during note creation:
    // build_note_storage does: u32::from_le_bytes(destination_network.to_be_bytes())
    // So to recover: u32::from_le_bytes(felt_value.to_be_bytes())
    let raw_network = u32::try_from(items[0].as_int())
        .context("destination_network overflow: felt value exceeds u32::MAX")?;
    let destination_network = u32::from_le_bytes(raw_network.to_be_bytes());

    // Reconstruct 20-byte address from 5 packed u32 felts (big-endian limb order).
    // Each felt holds a u32 value that represents 4 bytes in little-endian byte order.
    // to_elements() in EthAddressFormat uses bytes_to_packed_u32_felts which reads
    // each 4-byte chunk as a little-endian u32.
    let mut address = [0u8; 20];
    for i in 0..5 {
        let limb = u32::try_from(items[1 + i].as_int())
            .context("address limb overflow: felt value exceeds u32::MAX")?;
        address[i * 4..(i + 1) * 4].copy_from_slice(&limb.to_le_bytes());
    }

    Ok((destination_network, address))
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
/// - faucet_agg: origin_address=[0;20], origin_network=0, scale=0
pub fn resolve_faucet_origin(
    faucet_id: AccountId,
    accounts: &AccountsConfig,
) -> anyhow::Result<FaucetOriginInfo> {
    if faucet_id == accounts.faucet_eth.0 {
        Ok(FaucetOriginInfo {
            origin_network: 0,
            origin_address: [0u8; 20],
            scale: 10,
        })
    } else if faucet_id == accounts.faucet_agg.0 {
        Ok(FaucetOriginInfo {
            origin_network: 0,
            origin_address: [0u8; 20],
            scale: 0,
        })
    } else {
        anyhow::bail!(
            "unknown faucet ID {faucet_id}: only ETH and AGG faucets are supported. \
             Add new faucets to resolve_faucet_origin before bridging new tokens."
        )
    }
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

// BRIDGE OUT SCANNER
// ================================================================================================

/// Scans for consumed B2AGG notes and emits synthetic BridgeEvent logs.
pub struct BridgeOutScanner {
    store: Arc<dyn crate::store::Store>,
    block_state: Arc<BlockState>,
    accounts: AccountsConfig,
    _bridge_account_id: AccountId,
}

impl BridgeOutScanner {
    pub fn new(
        store: Arc<dyn crate::store::Store>,
        block_state: Arc<BlockState>,
        accounts: AccountsConfig,
        bridge_account_id: AccountId,
    ) -> Self {
        Self {
            store,
            block_state,
            accounts,
            _bridge_account_id: bridge_account_id,
        }
    }

    async fn process_consumed_note(&self, note: &InputNoteRecord, block_number: u64) {
        let note_id_str = note.id().to_string();

        if self
            .store
            .is_note_processed(&note_id_str)
            .await
            .unwrap_or(false)
        {
            return;
        }

        let details = note.details();
        if !is_b2agg_note(details) {
            return;
        }

        // Parse B2AGG storage
        let (destination_network, destination_address) =
            match parse_b2agg_storage(details.storage()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("B2AGG note {note_id_str}: failed to parse storage: {e:#}");
                    return;
                }
            };

        // Get the fungible asset
        let Some(fungible_asset) = details.assets().iter_fungible().next() else {
            tracing::warn!("B2AGG note {note_id_str} has no fungible asset, skipping");
            return;
        };
        let faucet_id = fungible_asset.faucet_id();
        let miden_amount = fungible_asset.amount();

        // Resolve origin info
        let origin = match resolve_faucet_origin(faucet_id, &self.accounts) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("B2AGG note {note_id_str}: {e:#}");
                return;
            }
        };
        let origin_amount = match reverse_scale_amount(miden_amount, origin.scale) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("B2AGG note {note_id_str}: {e:#}");
                return;
            }
        };

        // Generate synthetic tx hash
        let tx_hash = {
            let mut hasher = Keccak256::new();
            hasher.update(b"miden-bridge-out-");
            hasher.update(note_id_str.as_bytes());
            let hash: [u8; 32] = hasher.finalize().into();
            format!("0x{}", hex::encode(hash))
        };

        let block_hash = self.block_state.get_block_hash(block_number);
        let deposit_count = match self.store.mark_note_processed(note_id_str.clone()).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to mark note processed: {e}");
                return;
            }
        };

        // Emit BridgeEvent log
        if let Err(e) = self
            .store
            .add_bridge_event(
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
            )
            .await
        {
            tracing::error!("failed to add bridge event: {e}");
            if let Err(rollback_err) = self.store.unmark_note_processed(&note_id_str).await {
                tracing::error!("failed to roll back processed note marker: {rollback_err}");
            }
            return;
        }

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
            self.process_consumed_note(note, block_number).await;
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
}
