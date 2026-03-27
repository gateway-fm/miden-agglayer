//! Shared faucet operations — creation, bridge registration, metadata parsing.
//!
//! Used by `init.rs` (startup), `claim.rs` (auto-creation on first bridge),
//! and `service_admin.rs` (admin RPC endpoint).

use crate::accounts_config::AccountIdBech32;
use crate::miden_client::MidenClientLib;
use alloy::primitives::{Address, Bytes};
use miden_base_agglayer::{ConfigAggBridgeNote, EthAddressFormat, create_agglayer_faucet};
use miden_client::Felt;
use miden_client::asset::FungibleAsset;
use miden_client::crypto::FeltRng;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::transaction::OutputNote;

/// Create a faucet on Miden, deploy it, and register it in the bridge.
///
/// This is the full lifecycle for adding a new token faucet:
/// 1. Create the faucet account via `create_agglayer_faucet()`
/// 2. Deploy it to the Miden network
/// 3. Register it in the bridge via `ConfigAggBridgeNote` (required for CLAIM FPI validation)
#[allow(clippy::too_many_arguments)]
pub async fn create_and_register_faucet(
    client: &mut MidenClientLib,
    symbol: &str,
    miden_decimals: u8,
    origin_token_address: &[u8; 20],
    origin_network: u32,
    scale: u8,
    service_id: AccountId,
    bridge_id: AccountId,
) -> anyhow::Result<Account> {
    let max_supply = Felt::new(FungibleAsset::MAX_AMOUNT);
    let origin_addr = EthAddressFormat::new(*origin_token_address);

    let account = create_agglayer_faucet(
        client.rng().draw_word(),
        symbol,
        miden_decimals,
        max_supply,
        bridge_id,
        &origin_addr,
        origin_network,
        scale,
    );
    client.add_account(&account, false).await?;

    // Deploy
    tracing::info!(
        "deploying {} faucet {} ...",
        symbol,
        AccountIdBech32(account.id())
    );
    let dummy_txn = TransactionRequestBuilder::new().build()?;
    let txn_id = client
        .submit_new_transaction(account.id(), dummy_txn)
        .await?;
    tracing::info!("deployed {symbol} faucet with txn_id {txn_id}");

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("deploy tx {txn_id} committed");
    }

    // Register in bridge
    register_faucet_in_bridge(client, service_id, bridge_id, account.id(), symbol).await?;

    Ok(account)
}

/// Register a faucet in the bridge's faucet registry via ConfigAggBridgeNote.
///
/// Required for CLAIM note FPI validation: the bridge account must know which
/// faucets are valid sources for claim operations. Idempotent.
pub async fn register_faucet_in_bridge(
    client: &mut MidenClientLib,
    service_id: AccountId,
    bridge_id: AccountId,
    faucet_id: AccountId,
    faucet_name: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "registering {} faucet {} in bridge {}...",
        faucet_name,
        AccountIdBech32(faucet_id),
        AccountIdBech32(bridge_id),
    );

    let note = ConfigAggBridgeNote::create(faucet_id, service_id, bridge_id, client.rng())
        .map_err(|e| anyhow::anyhow!("failed to create ConfigAggBridgeNote: {e}"))?;

    let txn = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(note); 1])
        .build()?;

    let txn_id = client.submit_new_transaction(service_id, txn).await?;
    tracing::info!(
        "registered {} faucet in bridge with txn_id {txn_id}",
        faucet_name,
    );

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("register faucet tx {txn_id} committed");
        // Extra wait for NTX builder to process the config note
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            client.sync_state().await?;
        }
    }
    Ok(())
}

/// Parse token metadata from a `claimAsset` call's metadata field.
///
/// The bridge contract sends `abi.encode(string name, string symbol, uint8 decimals)`
/// for ERC-20 tokens on the first bridge. Returns `(symbol, origin_decimals)`.
pub fn parse_token_metadata(
    metadata: &Bytes,
    token_address: &Address,
) -> anyhow::Result<(String, u8)> {
    if metadata.is_empty() {
        if token_address.is_zero() {
            // Native ETH — shouldn't normally reach here since ETH is pre-registered
            return Ok(("ETH".to_string(), 18));
        }
        anyhow::bail!(
            "empty metadata for non-zero token address {token_address}: \
             cannot determine token symbol and decimals for auto-creation"
        );
    }

    // ABI decode: (string name, string symbol, uint8 decimals)
    // Layout: [name_offset(32)][symbol_offset(32)][decimals(32)][name_data...][symbol_data...]
    let data = metadata.as_ref();
    if data.len() < 96 {
        anyhow::bail!(
            "metadata too short ({} bytes) to contain ABI-encoded token info",
            data.len()
        );
    }

    // Read decimals from the third 32-byte word
    let decimals = data[95]; // last byte of third word

    // Read symbol offset (second word) and decode the string
    let symbol_offset = u256_from_be_slice(&data[32..64]);
    let symbol = abi_decode_string(data, symbol_offset)?;

    if symbol.is_empty() {
        anyhow::bail!("parsed empty symbol from metadata for token {token_address}");
    }

    Ok((symbol, decimals))
}

/// Read a big-endian u256 as a usize offset.
fn u256_from_be_slice(slice: &[u8]) -> usize {
    // Only care about the last 8 bytes for realistic offsets
    let mut bytes = [0u8; 8];
    let start = slice.len().saturating_sub(8);
    bytes.copy_from_slice(&slice[start..]);
    u64::from_be_bytes(bytes) as usize
}

/// ABI-decode a dynamic string at the given byte offset in `data`.
fn abi_decode_string(data: &[u8], offset: usize) -> anyhow::Result<String> {
    if offset + 32 > data.len() {
        anyhow::bail!(
            "string offset {offset} out of bounds (data len {})",
            data.len()
        );
    }
    let len = u256_from_be_slice(&data[offset..offset + 32]);
    let str_start = offset + 32;
    let str_end = str_start + len;
    if str_end > data.len() {
        anyhow::bail!(
            "string data [{str_start}..{str_end}) out of bounds (data len {})",
            data.len()
        );
    }
    String::from_utf8(data[str_start..str_end].to_vec())
        .map_err(|e| anyhow::anyhow!("invalid UTF-8 in token symbol: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn parse_empty_metadata_eth() {
        let metadata = Bytes::from(vec![]);
        let (symbol, decimals) = parse_token_metadata(&metadata, &Address::ZERO).unwrap();
        assert_eq!(symbol, "ETH");
        assert_eq!(decimals, 18);
    }

    #[test]
    fn parse_empty_metadata_non_zero_fails() {
        let metadata = Bytes::from(vec![]);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        assert!(parse_token_metadata(&metadata, &addr).is_err());
    }

    #[test]
    fn parse_abi_encoded_metadata() {
        // ABI encode: ("Tether USD", "USDT", 6)
        // name offset = 0x60, symbol offset = 0xa0, decimals = 6
        let mut data = Vec::new();

        // Word 0: name offset (0x60 = 96)
        data.extend_from_slice(&[0u8; 31]);
        data.push(0x60);

        // Word 1: symbol offset (0xa0 = 160)
        data.extend_from_slice(&[0u8; 31]);
        data.push(0xa0);

        // Word 2: decimals (6)
        data.extend_from_slice(&[0u8; 31]);
        data.push(6);

        // Word 3 (offset 96): name length (10 = "Tether USD")
        data.extend_from_slice(&[0u8; 31]);
        data.push(10);

        // Word 4 (offset 128): name data "Tether USD" + padding
        let name = b"Tether USD";
        data.extend_from_slice(name);
        data.resize(data.len() + (32 - name.len()), 0);

        // Word 5 (offset 160): symbol length (4 = "USDT")
        data.extend_from_slice(&[0u8; 31]);
        data.push(4);

        // Word 6 (offset 192): symbol data "USDT" + padding
        let sym = b"USDT";
        data.extend_from_slice(sym);
        data.resize(data.len() + (32 - sym.len()), 0);

        let metadata = Bytes::from(data);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let (symbol, decimals) = parse_token_metadata(&metadata, &addr).unwrap();
        assert_eq!(symbol, "USDT");
        assert_eq!(decimals, 6);
    }
}
