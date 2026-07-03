//! Shared faucet operations — creation, bridge registration, metadata parsing.
//!
//! Used by `init.rs` (startup), `claim.rs` (auto-creation on first bridge),
//! and `service_admin.rs` (admin RPC endpoint).

use crate::accounts_config::AccountIdBech32;
use crate::metadata_recovery::{EmitMetadata, FaucetConversion, recover_bridge_out_metadata};
use crate::miden_client::MidenClientLib;
use crate::store::FaucetEntry;
use alloy::primitives::{Address, Bytes};
use miden_base_agglayer::{
    AggLayerFaucet, ConfigAggBridgeNote, ConversionMetadata, EthAddress, MetadataHash,
    create_agglayer_faucet,
};
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
    let max_supply =
        Felt::new(u64::from(FungibleAsset::MAX_AMOUNT)).expect("value is a valid field element");
    let origin_addr = EthAddress::new(*origin_token_address);

    // Protocol 0.15: the faucet no longer stores conversion metadata. `create_agglayer_faucet`
    // is now 5-arg (seed, symbol, decimals, max_supply, bridge_id). The origin token address,
    // network, scale and metadata hash are registered on the bridge's `faucet_metadata_map`
    // via the CONFIG_AGG_BRIDGE note in `register_faucet_in_bridge` below.
    let account = create_agglayer_faucet(
        client.rng().draw_word(),
        symbol,
        miden_decimals,
        max_supply,
        bridge_id,
    );
    client.add_account(&account, false).await?;

    // Deploy
    tracing::info!(
        "deploying {} faucet {} ...",
        symbol,
        AccountIdBech32(account.id())
    );
    let dummy_txn = TransactionRequestBuilder::new().build()?;
    let txn_id = crate::metrics::meter_proof(
        crate::metrics::ProofKind::Faucet,
        crate::miden_client::submit_new_transaction(client, account.id(), dummy_txn),
    )
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
        origin_network,
        scale,
        metadata_hash,
        symbol,
    )
    .await?;

    Ok(account)
}

/// Cantina #6 — rebuild a local [`FaucetEntry`] for an EXISTING on-chain faucet
/// whose local `faucet_registry` row is missing (a `--restore` / fresh-DB
/// bootstrap, or a live claim/admin for a token whose row was lost).
///
/// The faucet account still lives on Miden — its origin identity is in the
/// bridge's `faucet_metadata_map` (`conversion`, already read by the caller) and
/// its symbol + Miden decimals in the faucet account's own storage. We therefore
/// rebuild only the local POINTER; we NEVER re-deploy the account. Its seed is a
/// random word (`create_agglayer_faucet(client.rng().draw_word(), ..)`) and is
/// unrecoverable, so a re-deploy would mint a *second* generation for the same
/// `(origin_address, origin_network)` — exactly the split-brain Cantina #6
/// describes, with the old generation's exits invisible forever.
///
/// Imports the faucet account from the node if it isn't tracked locally, reads
/// its symbol + Miden decimals, derives `origin_decimals = miden_decimals +
/// scale`, and recovers the ABI metadata preimage via the existing Cantina #13
/// L2 helper (empty for native ETH / when unrecoverable — the bridge-out emit
/// path re-recovers + backfills it later).
pub async fn rebuild_faucet_entry_from_chain(
    client: &mut MidenClientLib,
    bridge_account: &Account,
    faucet_id: AccountId,
    conversion: &FaucetConversion,
    l1_rpc_url: Option<&str>,
) -> anyhow::Result<FaucetEntry> {
    // Ensure the faucet account is available locally (best-effort import; if it is
    // already tracked this is a refresh). A prior process's dynamically-created
    // faucets are NOT in `bridge_accounts.toml`, so Phase 0 doesn't reimport them.
    if client.get_account(faucet_id).await.ok().flatten().is_none()
        && let Err(e) = client.import_account_by_id(faucet_id).await
    {
        anyhow::bail!("Cantina #6: cannot import faucet account {faucet_id} from node: {e}");
    }
    let faucet_account = client
        .get_account(faucet_id)
        .await
        .map_err(|e| anyhow::anyhow!("get_account({faucet_id}): {e}"))?
        .ok_or_else(|| anyhow::anyhow!("faucet account {faucet_id} not found after import"))?;

    let faucet = AggLayerFaucet::try_faucet_from_account(&faucet_account)
        .map_err(|e| anyhow::anyhow!("account {faucet_id} is not an AggLayer faucet: {e}"))?;
    let miden_decimals = faucet.decimals();
    let symbol = faucet.symbol().to_string();
    let scale = conversion.scale;
    let origin_decimals = miden_decimals.checked_add(scale).ok_or_else(|| {
        anyhow::anyhow!(
            "faucet {faucet_id}: miden_decimals {miden_decimals} + scale {scale} overflows u8"
        )
    })?;

    // Recover the ABI metadata preimage from authoritative on-chain state (bridge
    // metadata hash + Miden faucet name/symbol, and L1 name()/symbol()/decimals()
    // if an RPC is wired). Empty for native ETH or when unrecoverable — safe: the
    // bridge-out emit site re-recovers or gates (Cantina #13 L2).
    let metadata = match recover_bridge_out_metadata(
        &conversion.origin_address,
        &[],
        origin_decimals,
        faucet_id,
        Some(bridge_account),
        Some(&faucet_account),
        l1_rpc_url,
    )
    .await
    {
        EmitMetadata::Ready(bytes) | EmitMetadata::Recovered(bytes) => bytes,
        EmitMetadata::Unrecoverable => Vec::new(),
    };

    Ok(FaucetEntry {
        faucet_id,
        origin_address: conversion.origin_address,
        origin_network: conversion.origin_network,
        symbol,
        origin_decimals,
        miden_decimals,
        scale,
        metadata,
    })
}

/// Register a faucet in the bridge's faucet and token registries via ConfigAggBridgeNote.
///
/// Required for CLAIM note FPI validation: the bridge account must know which
/// faucets are valid sources for claim operations, and the on-chain `token_registry_map`
/// must map `hash(origin_token_address)` to the faucet's `AccountId`. Idempotent.
///
/// Protocol 0.15: the full conversion metadata (origin token address, network, scale,
/// `is_native` flag, metadata hash) now lives on the bridge's `faucet_metadata_map` and is
/// supplied here as a [`ConversionMetadata`] struct. Every faucet the service creates is a
/// bridge-owned (mint/burn) faucet for an L1-origin token, so `is_native` is always `false`;
/// Miden-native (lock/unlock) faucets are not created by the proxy.
#[allow(clippy::too_many_arguments)]
pub async fn register_faucet_in_bridge(
    client: &mut MidenClientLib,
    service_id: AccountId,
    bridge_id: AccountId,
    faucet_id: AccountId,
    origin_token_address: &EthAddress,
    origin_network: u32,
    scale: u8,
    metadata_hash: MetadataHash,
    faucet_name: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "registering {} faucet {} in bridge {}...",
        faucet_name,
        AccountIdBech32(faucet_id),
        AccountIdBech32(bridge_id),
    );

    let note = ConfigAggBridgeNote::create(
        ConversionMetadata {
            faucet_account_id: faucet_id,
            origin_token_address: *origin_token_address,
            scale,
            origin_network,
            is_native: false,
            metadata_hash,
        },
        service_id,
        bridge_id,
        client.rng(),
    )
    .map_err(|e| anyhow::anyhow!("failed to create ConfigAggBridgeNote: {e}"))?;

    let txn = TransactionRequestBuilder::new()
        .own_output_notes(vec![note])
        .build()?;

    let txn_id = crate::metrics::meter_proof(
        crate::metrics::ProofKind::Faucet,
        crate::miden_client::submit_new_transaction(client, service_id, txn),
    )
    .await?;
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

/// Maximum decimals a local Miden faucet may declare. Mirrors
/// [`miden_standards::account::faucets::FungibleFaucet::MAX_DECIMALS`], which the
/// fungible-faucet builder enforces — `create_agglayer_faucet` panics/errors for
/// any `decimals` above this. Used to bound the auto-derived `miden_decimals`.
pub const MAX_MIDEN_DECIMALS: u8 = miden_standards::account::faucets::FungibleFaucet::MAX_DECIMALS;

/// Maximum decimal downscaling factor `scale = origin_decimals - miden_decimals`
/// supported by the bridge stack. This is `MAX_SCALING_FACTOR` in the agglayer
/// `asset_conversion.masm`, enforced at runtime by
/// [`miden_base_agglayer::EthAmount::scale_to_token_amount`], which returns
/// `EthAmountError::ScaleTooLarge` for `scale > 18`. The upstream constant is
/// not `pub`, so it is mirrored here; keep in sync with the MASM.
pub const MAX_SCALING_FACTOR: u8 = 18;

/// Maximum decimals an ERC-20 may legitimately declare AND still be supportable
/// as a bridged token. A route for an origin token with `d` decimals is
/// satisfiable iff there is a local decimal count `m` with `m <=
/// MAX_MIDEN_DECIMALS` (12) and `d - m <= MAX_SCALING_FACTOR` (18); the largest
/// such `d` is `12 + 18 = 30` (via `m = 12`, `scale = 18`). Values above 30 are
/// genuinely unsupportable (and also pathological — `10u256.pow(decimals)` would
/// overflow during scaling). Self-review X3 — without this bound the
/// `parse_token_metadata` happy path accepts `decimals = 255` from a malicious
/// or buggy ERC-20, which then overflows U256 arithmetic in the claim path.
///
/// NB: 30 (not 26) is the correct hard cap under the *dynamic* decimal
/// derivation in [`derive_miden_decimals`]. 26 was only correct under the older
/// fixed `miden_decimals = min(origin, 8)` scheme, where `d = 27..30` yielded
/// `scale = 19..22 > 18` and thus an unclaimable route. The dynamic derivation
/// bumps `miden_decimals` up as needed so those tokens are supportable.
pub const MAX_TOKEN_DECIMALS: u8 = MAX_MIDEN_DECIMALS + MAX_SCALING_FACTOR;

/// Derive the local Miden faucet decimals for an origin token declaring
/// `origin_decimals`, honouring BOTH bridge-stack limits:
/// - the local faucet's decimals must be `<= MAX_MIDEN_DECIMALS` (12), and
/// - the downscaling factor `scale = origin_decimals - miden_decimals` must be
///   `<= MAX_SCALING_FACTOR` (18).
///
/// The formula keeps the historical default of 8 whenever that satisfies both
/// limits, and only bumps `miden_decimals` up as far as needed to keep `scale
/// <= 18`:
///
/// ```text
/// miden_decimals = origin_decimals.min(8).max(origin_decimals.saturating_sub(18))
/// ```
///
/// Worked examples: d=6→6 (scale 0), d=18→8 (scale 10), d=26→8 (scale 18),
/// d=27→9 (scale 18), d=30→12 (scale 18), d=31→13 (would exceed
/// MAX_MIDEN_DECIMALS → rejected).
///
/// Returns an error when the token is genuinely unsupportable — i.e. the
/// smallest `miden_decimals` that keeps `scale <= 18` still exceeds
/// `MAX_MIDEN_DECIMALS` (equivalently `origin_decimals > 30`). Callers MUST
/// reject up-front rather than persist an unclaimable route (finding #17).
pub fn derive_miden_decimals(origin_decimals: u8) -> anyhow::Result<u8> {
    let miden_decimals = origin_decimals
        .min(8)
        .max(origin_decimals.saturating_sub(MAX_SCALING_FACTOR));
    if miden_decimals > MAX_MIDEN_DECIMALS {
        anyhow::bail!(
            "origin token with {origin_decimals} decimals is unsupportable: the smallest local \
             faucet decimals that keeps scale <= {MAX_SCALING_FACTOR} is {miden_decimals}, which \
             exceeds the faucet limit of {MAX_MIDEN_DECIMALS} (max supportable origin decimals is \
             {MAX_TOKEN_DECIMALS})"
        );
    }
    Ok(miden_decimals)
}

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
    // X3 / finding #17 — reject decimals no local route can satisfy. Above
    // MAX_TOKEN_DECIMALS (= MAX_MIDEN_DECIMALS + MAX_SCALING_FACTOR = 30) there is
    // no `miden_decimals <= 12` with `scale <= 18`, so the token is
    // unsupportable; such values would also overflow `10^decimals` in U256 amount
    // scaling. Tokens with 27..30 decimals ARE supportable via the dynamic
    // derivation in `derive_miden_decimals` and are accepted here.
    if decimals > MAX_TOKEN_DECIMALS {
        anyhow::bail!(
            "token decimals out of range: {decimals} > {MAX_TOKEN_DECIMALS} (no local faucet route \
             with miden_decimals <= {MAX_MIDEN_DECIMALS} and scale <= {MAX_SCALING_FACTOR} exists)"
        );
    }
    // The decimals field is u8 in the ABI; the high 31 bytes of the word must be
    // zero. A non-zero value there indicates a malformed metadata that misbeds
    // the decimals into a wider integer slot — refuse rather than silently
    // truncating.
    if data[64..95].iter().any(|b| *b != 0) {
        anyhow::bail!("token decimals word non-canonical: high bytes are non-zero (malformed ABI)");
    }

    // Read symbol offset (second word) and decode the string
    let symbol_offset = u256_from_be_slice(&data[32..64]);
    let raw_symbol = abi_decode_string(data, symbol_offset)?;

    if raw_symbol.is_empty() {
        anyhow::bail!("parsed empty symbol from metadata for token {token_address}");
    }

    // X5 — sanitise the L1 symbol for Miden's TokenSymbol constraints.
    // Miden requires uppercase A-Z, max 6 chars, no digits or punctuation.
    // L1 ERC-20s routinely use lowercase ("usdt"), digits ("1INCH"), or
    // mixed case ("USDC.e"). Pre-fix the raw symbol flowed straight to
    // `create_agglayer_faucet`, which panics on invalid TokenSymbol —
    // failing the auto-create and (combined with RD-860 swallow)
    // permanently dropping every claim of that token. Sanitise to a
    // deterministic Miden-compatible value while preserving the raw
    // symbol via tracing for operator visibility.
    let symbol = sanitise_token_symbol(&raw_symbol, token_address);

    Ok((symbol, decimals))
}

/// Maximum length of a Miden TokenSymbol. The upstream type accepts ≤ 6
/// uppercase ASCII letters.
pub const MIDEN_TOKEN_SYMBOL_MAX: usize = 6;

/// Sanitise a raw L1 token symbol into a Miden-compatible TokenSymbol.
///
/// Maps any character that is NOT an uppercase A-Z to nothing (drop), keeps
/// at most `MIDEN_TOKEN_SYMBOL_MAX` characters, and uppercases the rest.
/// If the result is empty, falls back to a deterministic identifier
/// derived from the first 4 hex chars of the token address (so
/// non-letterful symbols like "🪙" or "$$$" still produce a stable
/// non-clashing fallback).
pub fn sanitise_token_symbol(raw: &str, token_address: &Address) -> String {
    let upper: String = raw
        .chars()
        .filter_map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_uppercase() {
                Some(u)
            } else {
                None
            }
        })
        .take(MIDEN_TOKEN_SYMBOL_MAX)
        .collect();
    if upper.is_empty() {
        // Fallback: T + first 4 hex chars of the token address.
        let addr_hex = format!("{token_address:x}");
        // Address is hex32 (lowercase, no 0x). Take first 4 chars and uppercase.
        let suffix: String = addr_hex
            .chars()
            .take(4)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        format!("T{suffix}")
    } else {
        if upper != raw {
            tracing::warn!(
                target: "faucet",
                raw_symbol = %raw,
                sanitised = %upper,
                token_address = %token_address,
                "X5: L1 token symbol sanitised to fit Miden's TokenSymbol constraints"
            );
        }
        upper
    }
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
        anyhow::bail!("decoded string length {len} exceeds cap {MAX_DECODED_STRING_BYTES} bytes");
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
    /// bytes of the 32-byte slot must be zero. A malformed sender that
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
        assert!(err.to_string().contains("exceeds cap"), "unexpected: {err}");
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

    /// Self-review X5 — repro+regression. Pre-fix, the raw L1 symbol was
    /// passed straight to `create_agglayer_faucet`, which calls Miden's
    /// `TokenSymbol::new(&str)` — that helper panics or returns Err on
    /// any non-uppercase / non-A-Z / >6-char input. Real-world tokens
    /// like "usdt", "1INCH", "USDC.e", "stETH" therefore failed
    /// auto-create. Combined with RD-860 swallow, those claims would
    /// silently drop forever.
    ///
    /// Tests pin the sanitiser's behaviour over a representative set:
    /// - lowercase preserved letters → uppercased
    /// - digits/punctuation dropped
    /// - multi-letter prefixes capped at MIDEN_TOKEN_SYMBOL_MAX
    /// - empty after stripping → fallback to T + first 4 hex of address
    /// - already-canonical symbols pass through unchanged (no warning
    ///   trigger)
    #[test]
    fn x5_sanitise_token_symbol() {
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

        // Lowercase preserved letters.
        assert_eq!(sanitise_token_symbol("usdt", &addr), "USDT");

        // Digits dropped.
        assert_eq!(sanitise_token_symbol("1INCH", &addr), "INCH");

        // Punctuation dropped.
        assert_eq!(sanitise_token_symbol("USDC.e", &addr), "USDCE");

        // Mixed case — uppercased.
        assert_eq!(sanitise_token_symbol("stETH", &addr), "STETH");

        // Already canonical — passes through.
        assert_eq!(sanitise_token_symbol("ETH", &addr), "ETH");

        // Truncated to MIDEN_TOKEN_SYMBOL_MAX.
        assert_eq!(sanitise_token_symbol("VERYLONG", &addr), "VERYLO");

        // All-letter cap exact.
        assert_eq!(sanitise_token_symbol("ABCDEF", &addr), "ABCDEF");
        assert_eq!(sanitise_token_symbol("ABCDEFG", &addr), "ABCDEF");

        // Empty after stripping → fallback.
        let result = sanitise_token_symbol("$$$", &addr);
        assert!(result.starts_with('T'));
        assert_eq!(result.len(), 5); // T + 4 hex chars
        assert!(
            result
                .chars()
                .skip(1)
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );

        // Non-ASCII Unicode dropped, falls back.
        let result = sanitise_token_symbol("🪙", &addr);
        assert!(result.starts_with('T'));
    }

    /// Self-review X5 — when invoked through `parse_token_metadata`, the
    /// returned symbol must be Miden-compatible regardless of L1 input.
    #[test]
    fn x5_parse_token_metadata_returns_sanitised_symbol() {
        // Build metadata with a lowercase symbol "usdt".
        let mut data = vec![0u8; 224];
        data[31] = 0x60; // name offset
        data[63] = 0xa0; // symbol offset
        data[95] = 6; // decimals
        // name = "X" at 0x60
        data[127] = 1;
        data[128] = b'X';
        // symbol = "usdt" at 0xa0
        data[191] = 4;
        data[192..196].copy_from_slice(b"usdt");
        let metadata = Bytes::from(data);
        let addr = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let (symbol, _decimals) = parse_token_metadata(&metadata, &addr).unwrap();
        assert_eq!(symbol, "USDT");
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
