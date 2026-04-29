//! Shared faucet operations — creation, bridge registration, metadata parsing.
//!
//! Used by `init.rs` (startup), `claim.rs` (auto-creation on first bridge),
//! and `service_admin.rs` (admin RPC endpoint).

use crate::accounts_config::AccountIdBech32;
use crate::miden_client::MidenClientLib;
use alloy::primitives::{Address, Bytes};
use miden_base_agglayer::{ConfigAggBridgeNote, EthAddress, MetadataHash, create_agglayer_faucet};
use miden_client::Felt;
use miden_client::asset::FungibleAsset;
use miden_client::crypto::FeltRng;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::{Account, AccountId};

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
    metadata_hash: MetadataHash,
) -> anyhow::Result<Account> {
    let max_supply = Felt::new(FungibleAsset::MAX_AMOUNT);
    let origin_addr = EthAddress::new(*origin_token_address);

    let account = create_agglayer_faucet(
        client.rng().draw_word(),
        symbol,
        miden_decimals,
        max_supply,
        bridge_id,
        &origin_addr,
        origin_network,
        scale,
        metadata_hash,
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
    register_faucet_in_bridge(
        client,
        service_id,
        bridge_id,
        account.id(),
        &origin_addr,
        symbol,
    )
    .await?;

    Ok(account)
}

/// Register a faucet in the bridge's faucet and token registries via ConfigAggBridgeNote.
///
/// Required for CLAIM note FPI validation: the bridge account must know which
/// faucets are valid sources for claim operations, and the on-chain `token_registry_map`
/// must map `hash(origin_token_address)` to the faucet's `AccountId`. Idempotent.
pub async fn register_faucet_in_bridge(
    client: &mut MidenClientLib,
    service_id: AccountId,
    bridge_id: AccountId,
    faucet_id: AccountId,
    origin_token_address: &EthAddress,
    faucet_name: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "registering {} faucet {} in bridge {}...",
        faucet_name,
        AccountIdBech32(faucet_id),
        AccountIdBech32(bridge_id),
    );

    let note = ConfigAggBridgeNote::create(
        faucet_id,
        origin_token_address,
        service_id,
        bridge_id,
        client.rng(),
    )
    .map_err(|e| anyhow::anyhow!("failed to create ConfigAggBridgeNote: {e}"))?;

    let txn = TransactionRequestBuilder::new()
        .own_output_notes(vec![note])
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

/// Maximum decimals an ERC-20 may legitimately declare. Real-world tokens use
/// 0..30; values above 30 are pathological and would cause `10u256.pow(decimals)`
/// to overflow during scaling. Self-review X3 — without this bound the
/// `parse_token_metadata` happy path accepts `decimals = 255` from a malicious
/// or buggy ERC-20, which then overflows U256 arithmetic in the claim path.
pub const MAX_TOKEN_DECIMALS: u8 = 30;

/// Maximum byte length for an ABI-decoded token symbol/name. Token symbols are
/// always short (1-12 chars in practice). Cap at 64 bytes so a malicious
/// metadata claiming `length = 1 GB` cannot trigger a huge allocation before
/// failing TokenSymbol validation. Self-review X4.
pub const MAX_DECODED_STRING_BYTES: usize = 64;

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
    // X3 — reject pathological decimals that would overflow `10^decimals` in
    // U256 amount scaling. The bridge contract's own metadata typically caps
    // at 18 (ETH); 30 is generous headroom for any future variant.
    if decimals > MAX_TOKEN_DECIMALS {
        anyhow::bail!(
            "token decimals out of range: {decimals} > {MAX_TOKEN_DECIMALS} (would overflow U256 scaling)"
        );
    }
    // The decimals field is u8 in the ABI; the high 31 bytes of the word must be
    // zero. A non-zero value there indicates a malformed metadata that misbeds
    // the decimals into a wider integer slot — refuse rather than silently
    // truncating.
    if data[64..95].iter().any(|b| *b != 0) {
        anyhow::bail!(
            "token decimals word non-canonical: high bytes are non-zero (malformed ABI)"
        );
    }

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
///
/// X4 hardening: bound everything with `checked_add` so a malicious metadata
/// that reports `len = u64::MAX - 31` cannot wrap into a small size that
/// passes the bounds check, and cap the maximum decoded length at
/// `MAX_DECODED_STRING_BYTES` so a 1 GB declared length doesn't trigger a
/// huge allocation before TokenSymbol validation rejects it.
fn abi_decode_string(data: &[u8], offset: usize) -> anyhow::Result<String> {
    let header_end = offset
        .checked_add(32)
        .ok_or_else(|| anyhow::anyhow!("string offset {offset} overflows usize"))?;
    if header_end > data.len() {
        anyhow::bail!(
            "string offset {offset} out of bounds (data len {})",
            data.len()
        );
    }
    let len = u256_from_be_slice(&data[offset..header_end]);
    if len > MAX_DECODED_STRING_BYTES {
        anyhow::bail!(
            "decoded string length {len} exceeds cap {MAX_DECODED_STRING_BYTES} bytes"
        );
    }
    let str_start = header_end;
    let str_end = str_start
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("string end overflow: start {str_start} + len {len}"))?;
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

    /// Self-review X3 — repro+regression. Pre-fix `parse_token_metadata`
    /// accepted any u8 value for `decimals`, including `255`. The downstream
    /// claim path then computes `10u256.pow(decimals)` for amount scaling,
    /// which overflows for `decimals > ~77`. Cap at MAX_TOKEN_DECIMALS = 30
    /// (well above any real-world token).
    #[test]
    fn x3_decimals_above_30_rejected() {
        let metadata = bytes_with_decimals(255);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let err = parse_token_metadata(&metadata, &addr).unwrap_err();
        assert!(
            err.to_string().contains("decimals out of range"),
            "unexpected: {err}"
        );

        // Boundary: exactly MAX is accepted.
        let metadata = bytes_with_decimals(MAX_TOKEN_DECIMALS);
        assert!(parse_token_metadata(&metadata, &addr).is_ok());

        // Off-by-one above MAX is rejected.
        let metadata = bytes_with_decimals(MAX_TOKEN_DECIMALS + 1);
        assert!(parse_token_metadata(&metadata, &addr).is_err());
    }

    /// Self-review X3 — non-canonical decimals word (high bytes non-zero)
    /// must be refused. The ABI declares `uint8 decimals` so the high 31
    /// bytes of the 32-byte slot must be zero. A misformed sender that
    /// embeds a wider integer would silently truncate to its low byte
    /// without this check.
    #[test]
    fn x3_non_canonical_decimals_word_rejected() {
        // Build metadata with decimals=6 in the low byte but non-zero
        // garbage in the high bytes of the third word.
        let mut data = vec![0u8; 224];
        data[31] = 0x60; // name offset = 0x60
        data[63] = 0xa0; // symbol offset = 0xa0
        data[64] = 0xff; // GARBAGE in high byte of decimals word
        data[95] = 6; // legitimate-looking decimals
        // name = "X" at offset 0x60
        data[127] = 1; // name length = 1
        data[128] = b'X';
        // symbol = "USDT" at offset 0xa0
        data[191] = 4;
        data[192..196].copy_from_slice(b"USDT");
        let metadata = Bytes::from(data);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let err = parse_token_metadata(&metadata, &addr).unwrap_err();
        assert!(
            err.to_string().contains("non-canonical"),
            "unexpected: {err}"
        );
    }

    /// Self-review X4 — repro+regression. Pre-fix `abi_decode_string`
    /// accepted any u64 length without bounds. A malicious metadata
    /// declaring `length = 1 GB` would either allocate 1 GB (DoS) or
    /// fail bounds check after the allocation. Cap at
    /// MAX_DECODED_STRING_BYTES = 64 so the allocation refuses BEFORE
    /// the slice is copied.
    #[test]
    fn x4_oversized_string_length_rejected() {
        // Build metadata where the symbol's declared length is 1024
        // (above MAX_DECODED_STRING_BYTES = 64).
        let mut data = vec![0u8; 256];
        data[31] = 0x60; // name offset
        data[63] = 0xa0; // symbol offset = 0xa0
        data[95] = 6; // decimals
        // name = "X" at 0x60
        data[127] = 1;
        data[128] = b'X';
        // symbol length = 1024 at 0xa0
        data[160 + 30] = 0x04;
        data[160 + 31] = 0x00; // 1024 = 0x0400
        let metadata = Bytes::from(data);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let err = parse_token_metadata(&metadata, &addr).unwrap_err();
        assert!(
            err.to_string().contains("exceeds cap"),
            "unexpected: {err}"
        );
    }

    /// Self-review X4 — overflow in `offset + len` arithmetic. Pre-fix
    /// `offset + len` could wrap to a small value if `len = u64::MAX - 31`,
    /// passing the bounds check. Post-fix `checked_add` short-circuits.
    #[test]
    fn x4_overflow_in_offset_plus_len_rejected() {
        // Test the helper directly because crafting a 32-byte length word
        // that lands at usize::MAX is awkward via the parse_token_metadata
        // entry. The function is private so we round-trip through the
        // public surface: a length word of max (u64) immediately fails
        // the cap check before any arithmetic.
        let mut data = vec![0u8; 256];
        data[31] = 0x60;
        data[63] = 0xa0;
        data[95] = 6;
        data[127] = 1;
        data[128] = b'X';
        // symbol length = u64::MAX
        for b in data[160 + 24..160 + 32].iter_mut() {
            *b = 0xff;
        }
        let metadata = Bytes::from(data);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        assert!(parse_token_metadata(&metadata, &addr).is_err());
    }

    /// Helper for X3 tests: build a minimal valid ABI metadata with a
    /// chosen decimals byte. Other fields are kept canonical.
    fn bytes_with_decimals(decimals: u8) -> Bytes {
        let mut data = vec![0u8; 224];
        data[31] = 0x60; // name offset
        data[63] = 0xa0; // symbol offset
        data[95] = decimals;
        // name = "T"
        data[127] = 1;
        data[128] = b'T';
        // symbol = "TKN"
        data[191] = 3;
        data[192..195].copy_from_slice(b"TKN");
        Bytes::from(data)
    }
}
