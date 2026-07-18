//! Cantina #13 — Layer 2: ERC-20 bridge-out metadata RECOVERY.
//!
//! Layer 1 (PR #90) persists the raw ABI metadata preimage
//! (`abi.encode(name, symbol, decimals)`) on every [`FaucetEntry`] at faucet
//! creation and threads it into the synthetic `BridgeEvent` at both emit sites.
//! But faucet rows written BEFORE Layer 1 — and any registry rebuilt after a DB
//! loss — have an EMPTY `metadata`. For an ERC-20 (origin token address is
//! non-zero) emitting an empty metadata is a poison leaf: the downstream exit
//! leaf no longer matches Miden's bridge state and a fresh-destination
//! `_deployWrappedToken(abi.decode(...))` fails.
//!
//! This module recovers the missing preimage from authoritative on-chain state
//! and, crucially, VALIDATES it before use: the bridge account holds the
//! `keccak256(abi.encode(name, symbol, decimals))` for every faucet in its
//! `faucet_metadata_map` storage slot ([`AggLayerBridge::faucet_metadata_map_slot_name`]).
//! We re-derive candidate preimages, hash each, and accept a candidate ONLY when
//! its keccak equals the bridge's stored hash. On any failure we FAIL SAFE — the
//! caller must NOT emit an ERC-20 `BridgeEvent` with empty or unvalidated
//! metadata.
//!
//! [`FaucetEntry`]: crate::store::FaucetEntry
//! [`AggLayerBridge::faucet_metadata_map_slot_name`]: miden_base_agglayer::AggLayerBridge

use miden_base_agglayer::{AggLayerBridge, AggLayerFaucet, MetadataHash};
use miden_protocol::account::{Account, AccountId, AccountStorage, StorageSlotContent};
use miden_protocol::{Felt, Word};

/// Metric incremented whenever an ERC-20 bridge-out is gated because its
/// metadata could not be recovered + validated. A non-zero value means real
/// user bridge-outs are being deferred and an operator must backfill the faucet
/// registry (e.g. re-run `admin_registerFaucet` with the correct name) or supply
/// a reachable L1 RPC for the token's origin network.
pub const METADATA_UNRECOVERABLE_METRIC: &str = "bridge_out_metadata_unrecoverable_total";

/// Maps an `origin_network` id → the RPC URL that serves that network's token
/// contracts (network 0 = L1, network 2 = a second rollup / L2B, …). Cantina #13
/// metadata recovery uses it to fetch ERC-20 `name()`/`symbol()`/`decimals()`
/// from the token's ACTUAL origin chain instead of always dialing L1 — a token
/// whose origin is L2B (origin_network=2) would otherwise be validated against
/// the wrong chain and fail the keccak gate (finding #62). An empty map means "no
/// RPC for any network" (recovery falls back to the all-Miden candidate only),
/// which is exactly the pre-#62 behavior when no `l1_rpc_url` was configured.
pub type NetworkRpcMap = std::collections::HashMap<u32, String>;

/// Native-ETH sentinel: an all-zero origin token address. Native ETH legitimately
/// carries empty metadata and must never be touched by recovery.
const NATIVE_TOKEN_ADDRESS: [u8; 20] = [0u8; 20];

// Mirror of the upstream `miden-agglayer` `SolTokenMetadata` (its
// `encode_token_metadata` is `pub(crate)`), identical to `service_admin`'s
// `AdminTokenMetadata`. Encoding with `abi_encode_params` reproduces Solidity's
// `abi.encode(string name, string symbol, uint8 decimals)` byte-for-byte, so
// `keccak256(bytes)` equals the bridge's stored `MetadataHash` (Cantina #13).
alloy_core::sol! {
    struct RecoveredTokenMetadata {
        string name;
        string symbol;
        uint8 decimals;
    }
}

// L1 ERC-20 metadata view methods, used to recover the canonical preimage when
// the all-Miden state is insufficient (the Miden faucet only stores a *sanitised*
// symbol and sets its token name == symbol, so it cannot reproduce the origin
// `name`/`symbol` the hash was computed over).
alloy_core::sol! {
    #[derive(Debug)]
    interface IERC20Metadata {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
}

/// A candidate `(name, symbol, decimals)` triple from which to re-derive the
/// ABI metadata preimage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataCandidate {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// Outcome of resolving the metadata bytes a bridge-out's synthetic `BridgeEvent`
/// must carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitMetadata {
    /// Use these bytes as-is. Either the Layer-1 stored metadata (already correct)
    /// or the legitimately-empty native-ETH metadata. No backfill needed.
    Ready(Vec<u8>),
    /// Freshly recovered + keccak-validated ERC-20 metadata. The caller MUST emit
    /// these AND backfill them into the faucet registry (one-time self-heal).
    Recovered(Vec<u8>),
    /// Could not recover validated metadata for an ERC-20 with empty metadata.
    /// The caller MUST gate (defer / skip) the bridge-out and NEVER emit empty.
    Unrecoverable,
}

/// Re-derive the ABI metadata preimage `abi.encode(name, symbol, decimals)`.
///
/// Uses `abi_encode_params` (not `abi_encode`) to match Solidity's
/// `abi.encode(string, string, uint8)` exactly — the same approach Layer 1's
/// admin path uses in `service_admin::AdminTokenMetadata`.
pub fn rederive_token_metadata(name: &str, symbol: &str, decimals: u8) -> Vec<u8> {
    use alloy_core::sol_types::SolValue;
    RecoveredTokenMetadata {
        name: name.to_string(),
        symbol: symbol.to_string(),
        decimals,
    }
    .abi_encode_params()
}

/// `keccak256(metadata_bytes)` using the bridge's own hashing (so the comparison
/// is byte-identical to how the stored `MetadataHash` was computed).
fn keccak_metadata(metadata_bytes: &[u8]) -> [u8; 32] {
    *MetadataHash::from_abi_encoded(metadata_bytes).as_bytes()
}

/// PURE core of Cantina #13 Layer 2. Decide what metadata bytes a bridge-out
/// should emit, given the faucet's stored metadata, the origin token address, a
/// set of recovery candidates, and the bridge's authoritative metadata hash.
///
/// No I/O — fully unit-testable. The caller is responsible for gathering the
/// candidates (Miden faucet account, L1 RPC) and the `expected_hash` (bridge
/// account), then acting on the returned variant (emit / emit+backfill / gate).
///
/// Decision order:
/// 1. Non-empty stored metadata → [`EmitMetadata::Ready`] (Layer-1 happy path).
/// 2. Native ETH (zero origin address) → [`EmitMetadata::Ready`] with empty bytes.
/// 3. ERC-20 with empty stored metadata → try each candidate; the FIRST whose
///    `keccak256(abi.encode(...))` equals `expected_hash` yields
///    [`EmitMetadata::Recovered`]. No match (or no hash) → [`EmitMetadata::Unrecoverable`].
pub fn resolve_emit_metadata(
    origin_address: &[u8; 20],
    stored_metadata: &[u8],
    candidates: &[MetadataCandidate],
    expected_hash: Option<[u8; 32]>,
) -> EmitMetadata {
    // 1. Layer-1 happy path: metadata already persisted and (by construction)
    //    keccak-consistent with the faucet's hash.
    if !stored_metadata.is_empty() {
        return EmitMetadata::Ready(stored_metadata.to_vec());
    }

    // 2. Native ETH legitimately has empty metadata — never recover, never gate.
    if *origin_address == NATIVE_TOKEN_ADDRESS {
        return EmitMetadata::Ready(Vec::new());
    }

    // 3. ERC-20 with empty metadata: recovery is mandatory and gated on a match
    //    against the bridge's authoritative hash. Without that hash we cannot
    //    validate anything — fail safe.
    let Some(expected) = expected_hash else {
        return EmitMetadata::Unrecoverable;
    };

    for candidate in candidates {
        let bytes = rederive_token_metadata(&candidate.name, &candidate.symbol, candidate.decimals);
        if keccak_metadata(&bytes) == expected {
            return EmitMetadata::Recovered(bytes);
        }
    }

    EmitMetadata::Unrecoverable
}

/// Read a faucet's authoritative `metadata_hash` from the bridge account's
/// `faucet_metadata_map` storage slot.
///
/// The map is keyed per-faucet with a 4-felt sub-key `[subkey, 0, faucet_suffix,
/// faucet_prefix]`; sub-keys 2 and 3 hold the low/high words of the keccak hash,
/// each a 4-felt little-endian-packed-u32 value (mirrors
/// `MetadataHash::to_elements` / `bytes_to_packed_u32_elements`, and the
/// reconstruction in `AggLayerBridge::cgi_chain_hash`).
///
/// Returns `None` if the slot read fails or the stored hash is all-zero (an
/// unregistered / uninitialised faucet — an absent map key yields the default
/// `Word`).
pub fn read_faucet_metadata_hash(
    bridge_account: &Account,
    faucet_id: AccountId,
) -> Option<[u8; 32]> {
    let slot = AggLayerBridge::faucet_metadata_map_slot_name();
    let suffix = faucet_id.suffix();
    let prefix = faucet_id.prefix().as_felt();
    let zero = Felt::from(0u32);

    // sub-key 2 → METADATA_HASH_LO, sub-key 3 → METADATA_HASH_HI
    let key_lo = Word::new([Felt::from(2u32), zero, suffix, prefix]);
    let key_hi = Word::new([Felt::from(3u32), zero, suffix, prefix]);

    let storage = bridge_account.storage();
    let lo = storage.get_map_item(slot, key_lo).ok()?;
    let hi = storage.get_map_item(slot, key_hi).ok()?;

    let mut bytes = [0u8; 32];
    for (i, felt) in lo
        .as_elements()
        .iter()
        .chain(hi.as_elements().iter())
        .enumerate()
    {
        let limb = u32::try_from(felt.as_canonical_u64()).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&limb.to_le_bytes());
    }

    if bytes == [0u8; 32] {
        // No metadata hash registered for this faucet — treat as unreadable so
        // the caller fails safe rather than "validating" against a zero hash.
        return None;
    }
    Some(bytes)
}

// CANTINA #6 — NON-ETH FAUCET IDENTITY READBACK
// ================================================================================================
//
// The bridge's `faucet_metadata_map` is the authoritative on-chain record of
// every registered faucet's origin-chain identity. On a `--restore` / fresh-DB
// bootstrap the local `faucet_registry` may be missing a faucet's row entirely
// (Cantina #6); the account still exists on Miden and is still bridge-out-valid,
// so we rebuild the local POINTER from this map rather than (re)deploying a
// second generation the registry cannot model.
//
// The map is written by `bridge_config.masm::store_faucet_metadata` with a
// per-faucet 4-felt sub-key `[subkey, 0, faucet_suffix, faucet_prefix]`:
//   - sub-key 0 → VALUE `[addr0, addr1, addr2, addr3]`        (origin address bytes 0..16)
//   - sub-key 1 → VALUE `[addr4, origin_network, scale, 0]`   (bytes 16..20 + network + scale)
//   - sub-key 2/3 → the keccak metadata hash (see `read_faucet_metadata_hash`).
// `origin_network` is stored RAW (no byte-swap); each address limb is a
// little-endian-packed u32 (`bytes_to_packed_u32_elements`), decoded exactly as
// `read_faucet_metadata_hash` decodes the hash limbs.

/// A faucet's origin-chain identity, read back from the bridge's
/// `faucet_metadata_map` — the authoritative source used to rebuild a missing
/// local `faucet_registry` row on restore, and to recover an existing faucet on
/// the live claim/admin path instead of deploying a replacement generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetConversion {
    pub origin_address: [u8; 20],
    pub origin_network: u32,
    pub scale: u8,
}

/// Read a single faucet's conversion metadata (origin token address, origin
/// network, decimal scale) from the bridge account's `faucet_metadata_map`,
/// keyed by `faucet_id` (sub-keys 0 and 1).
///
/// Returns `None` when the readback is the default all-zero word (the faucet is
/// not registered on the bridge — native ETH is never rebuilt via this path) or
/// a limb is out of `u32`/`u8` range.
pub fn read_faucet_conversion_metadata(
    bridge_storage: &AccountStorage,
    faucet_id: AccountId,
) -> Option<FaucetConversion> {
    let slot = AggLayerBridge::faucet_metadata_map_slot_name();
    let suffix = faucet_id.suffix();
    let prefix = faucet_id.prefix().as_felt();
    let zero = Felt::from(0u32);

    // sub-key 0 → origin address bytes 0..16; sub-key 1 → bytes 16..20 + network + scale
    let key_lo = Word::new([Felt::from(0u32), zero, suffix, prefix]);
    let key_hi = Word::new([Felt::from(1u32), zero, suffix, prefix]);

    let lo = bridge_storage.get_map_item(slot, key_lo).ok()?;
    let hi = bridge_storage.get_map_item(slot, key_hi).ok()?;
    let lo_elems = lo.as_elements();
    let hi_elems = hi.as_elements();

    let mut origin_address = [0u8; 20];
    // limbs 0..4 (sub-key 0) → bytes 0..16
    for (i, felt) in lo_elems.iter().enumerate() {
        let limb = u32::try_from(felt.as_canonical_u64()).ok()?;
        origin_address[i * 4..i * 4 + 4].copy_from_slice(&limb.to_le_bytes());
    }
    // limb 4 (sub-key 1, element 0) → bytes 16..20
    let addr4 = u32::try_from(hi_elems[0].as_canonical_u64()).ok()?;
    origin_address[16..20].copy_from_slice(&addr4.to_le_bytes());

    let origin_network = u32::try_from(hi_elems[1].as_canonical_u64()).ok()?;
    let scale = u8::try_from(hi_elems[2].as_canonical_u64()).ok()?;

    // An absent map key defaults to the zero word; an all-zero readback means the
    // faucet is not registered (or is the native-ETH sentinel, which is
    // pre-seeded and never rebuilt from chain).
    if origin_address == [0u8; 20] && origin_network == 0 && scale == 0 {
        return None;
    }
    Some(FaucetConversion {
        origin_address,
        origin_network,
        scale,
    })
}

/// Enumerate every faucet_id registered in the bridge's `faucet_metadata_map`.
///
/// Each registered faucet has exactly one sub-key-`0` entry keyed
/// `[0,0,suffix,prefix]`; we recover the `AccountId` from the `(suffix, prefix)`
/// limbs of those keys. Relies on the bridge account being fully synced locally
/// (restore Phase 0 reimports it) so the map leaves are present — the same
/// assumption `read_faucet_metadata_hash` already makes for Cantina #13.
pub fn enumerate_registered_faucet_ids(bridge_storage: &AccountStorage) -> Vec<AccountId> {
    let slot = AggLayerBridge::faucet_metadata_map_slot_name();
    let Some(storage_slot) = bridge_storage.get(slot) else {
        return Vec::new();
    };
    let StorageSlotContent::Map(map) = storage_slot.content() else {
        return Vec::new();
    };
    let zero = Felt::from(0u32);
    let mut ids = Vec::new();
    for (key, _value) in map.entries() {
        let elems = Word::from(*key);
        let e = elems.as_elements();
        // One sub-key-0 row per faucet; the other sub-keys (1/2/3) would yield the
        // same (suffix, prefix), so filter to sub-key 0 to enumerate each once.
        if e[0] != zero {
            continue;
        }
        let (suffix, prefix) = (e[2], e[3]);
        if let Ok(id) = AccountId::try_from_elements(suffix, prefix) {
            ids.push(id);
        }
    }
    ids
}

/// Find the faucet already registered on the bridge for a given origin token
/// `(address, network)` pair, if any (Cantina #6 recover-existing). Used by the
/// live claim/admin path to import an existing faucet identity instead of
/// deploying a replacement generation the `(origin_address, origin_network)`
/// unique registry can't model.
pub fn find_registered_faucet_for_origin(
    bridge_storage: &AccountStorage,
    origin_address: &[u8; 20],
    origin_network: u32,
) -> Option<(AccountId, FaucetConversion)> {
    for faucet_id in enumerate_registered_faucet_ids(bridge_storage) {
        if let Some(conv) = read_faucet_conversion_metadata(bridge_storage, faucet_id)
            && &conv.origin_address == origin_address
            && conv.origin_network == origin_network
        {
            return Some((faucet_id, conv));
        }
    }
    None
}

/// Build the all-Miden recovery candidate from the faucet account.
///
/// NOTE: the AggLayer faucet sets its token name == symbol and stores a
/// *sanitised* symbol, so this candidate only validates for tokens whose origin
/// `name == symbol` and whose symbol survived sanitisation unchanged. It is tried
/// first because it needs no RPC; the keccak gate makes trying it safe.
fn miden_faucet_candidate(
    faucet_account: &Account,
    origin_decimals: u8,
) -> Option<MetadataCandidate> {
    let faucet = AggLayerFaucet::try_faucet_from_account(faucet_account).ok()?;
    Some(MetadataCandidate {
        name: faucet.token_name().as_str().to_string(),
        symbol: faucet.symbol().to_string(),
        decimals: origin_decimals,
    })
}

/// Fetch the canonical recovery candidate from L1 via ERC-20 `name()`,
/// `symbol()`, `decimals()` on the origin token contract.
///
/// This reproduces the exact preimage the bridge hashed at registration (the
/// metadata originally flowed from L1), so it is the authoritative recovery
/// source. If the L1 RPC points at the wrong origin network the calls return
/// values that won't match the stored hash and the keccak gate rejects them.
async fn fetch_l1_token_candidate(
    l1_rpc_url: &str,
    origin_address: &[u8; 20],
) -> anyhow::Result<MetadataCandidate> {
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::sol_types::SolCall;
    use alloy_rpc_types_eth::TransactionRequest;

    let provider = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let addr = alloy::primitives::Address::from(*origin_address);

    let name = {
        let call = IERC20Metadata::nameCall {};
        let res = provider
            .call(
                TransactionRequest::default()
                    .to(addr)
                    .input(call.abi_encode().into()),
            )
            .await?;
        IERC20Metadata::nameCall::abi_decode_returns(&res)?
    };
    let symbol = {
        let call = IERC20Metadata::symbolCall {};
        let res = provider
            .call(
                TransactionRequest::default()
                    .to(addr)
                    .input(call.abi_encode().into()),
            )
            .await?;
        IERC20Metadata::symbolCall::abi_decode_returns(&res)?
    };
    let decimals = {
        let call = IERC20Metadata::decimalsCall {};
        let res = provider
            .call(
                TransactionRequest::default()
                    .to(addr)
                    .input(call.abi_encode().into()),
            )
            .await?;
        IERC20Metadata::decimalsCall::abi_decode_returns(&res)?
    };

    Ok(MetadataCandidate {
        name,
        symbol,
        decimals,
    })
}

/// Orchestrate Cantina #13 Layer 2 recovery for one bridge-out.
///
/// Combines the authoritative bridge hash, the (cheap, no-RPC) Miden faucet
/// candidate, and — only if needed and configured — an L1 candidate, then defers
/// the keccak-gated decision to [`resolve_emit_metadata`]. Performs I/O only when
/// recovery is actually required (ERC-20 + empty stored metadata); the happy and
/// native paths return immediately without touching the accounts.
///
/// `bridge_account` / `faucet_account` are the already-fetched accounts (the
/// caller holds the live Miden client). When recovery is required but either is
/// missing (e.g. no client available), the function fails safe with
/// [`EmitMetadata::Unrecoverable`].
#[allow(clippy::too_many_arguments)]
pub async fn recover_bridge_out_metadata(
    origin_address: &[u8; 20],
    stored_metadata: &[u8],
    origin_decimals: u8,
    faucet_id: AccountId,
    bridge_account: Option<&Account>,
    faucet_account: Option<&Account>,
    l1_rpc_url: Option<&str>,
) -> EmitMetadata {
    // Fast paths: no recovery (and no account reads) needed.
    if !stored_metadata.is_empty() {
        return EmitMetadata::Ready(stored_metadata.to_vec());
    }
    if *origin_address == NATIVE_TOKEN_ADDRESS {
        return EmitMetadata::Ready(Vec::new());
    }

    // ERC-20 with empty metadata: recover + validate.
    let Some(expected) = bridge_account.and_then(|b| read_faucet_metadata_hash(b, faucet_id))
    else {
        // Unreadable / missing bridge hash → cannot validate anything.
        return EmitMetadata::Unrecoverable;
    };

    // Candidate 1 — all-Miden (no RPC). Try it first.
    let mut candidates: Vec<MetadataCandidate> = Vec::new();
    if let Some(c) = faucet_account.and_then(|fa| miden_faucet_candidate(fa, origin_decimals)) {
        candidates.push(c);
    }
    if let EmitMetadata::Recovered(bytes) =
        resolve_emit_metadata(origin_address, stored_metadata, &candidates, Some(expected))
    {
        return EmitMetadata::Recovered(bytes);
    }

    // Candidate 2 — authoritative L1 ERC-20 metadata (only if an L1 RPC is wired).
    if let Some(url) = l1_rpc_url {
        match fetch_l1_token_candidate(url, origin_address).await {
            Ok(c) => candidates.push(c),
            Err(e) => {
                tracing::warn!(
                    target: "bridge_out::metadata_recovery",
                    faucet_id = %faucet_id,
                    error = ?e,
                    "Cantina #13 L2: L1 token metadata fetch failed; cannot recover ERC-20 metadata"
                );
            }
        }
    }

    resolve_emit_metadata(origin_address, stored_metadata, &candidates, Some(expected))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: [u8; 20] = [0x11u8; 20];

    fn hash_of(name: &str, symbol: &str, decimals: u8) -> [u8; 32] {
        keccak_metadata(&rederive_token_metadata(name, symbol, decimals))
    }

    // ── Cantina #6 — non-ETH faucet identity readback ────────────────────────
    //
    // These tests prove the on-chain read mechanism the restore-rebuild and
    // live recover-existing paths depend on: given a bridge account whose
    // `faucet_metadata_map` is populated exactly as `bridge_config.masm`
    // writes it, we can read a faucet's origin (address/network/scale) back and
    // enumerate/match registered faucets — WITHOUT a Miden node. The full
    // rebuild (importing the faucet account for symbol/decimals) is exercised
    // by `scripts/e2e-cantina6-faucet-identity-restore.sh`.
    mod finding_6 {
        use super::*;
        use crate::bridge_out::resolve_faucet_origin;
        use crate::store::memory::InMemoryStore;
        use crate::store::{FaucetEntry, Store};
        use miden_base_agglayer::EthAddress;
        use miden_protocol::account::{
            AccountId, AccountStorage, StorageMap, StorageMapKey, StorageSlot,
        };
        use std::sync::Arc as StdArc;

        // A valid v1 public account id used as the faucet id under test. The
        // readback/enumeration logic is agnostic to account TYPE — it only
        // round-trips the (suffix, prefix) felts — so a regular id suffices.
        const FAUCET_HEX: &str = "0xac0000000000dd110000ee000000fc";

        fn faucet_id() -> AccountId {
            AccountId::from_hex(FAUCET_HEX).unwrap()
        }

        /// Fabricate a bridge `AccountStorage` whose `faucet_metadata_map` holds
        /// the sub-key-0 / sub-key-1 rows for one faucet, byte-identical to how
        /// `bridge_config.masm::store_faucet_metadata` writes them:
        ///   sub-key 0 → `[addr0, addr1, addr2, addr3]`
        ///   sub-key 1 → `[addr4, origin_network, scale, 0]`
        fn fabricate_bridge_storage(
            faucet: AccountId,
            origin_address: [u8; 20],
            origin_network: u32,
            scale: u8,
        ) -> AccountStorage {
            let suffix = faucet.suffix();
            let prefix = faucet.prefix().as_felt();
            let zero = Felt::from(0u32);

            let addr = EthAddress::new(origin_address).to_elements(); // 5 felts
            let key0 = StorageMapKey::new(Word::new([Felt::from(0u32), zero, suffix, prefix]));
            let val0 = Word::new([addr[0], addr[1], addr[2], addr[3]]);
            let key1 = StorageMapKey::new(Word::new([Felt::from(1u32), zero, suffix, prefix]));
            let val1 = Word::new([addr[4], Felt::from(origin_network), Felt::from(scale), zero]);

            let mut map = StorageMap::new();
            map.insert(key0, val0).unwrap();
            map.insert(key1, val1).unwrap();

            let slot =
                StorageSlot::with_map(AggLayerBridge::faucet_metadata_map_slot_name().clone(), map);
            AccountStorage::new(vec![slot]).unwrap()
        }

        /// Reads origin address / network / scale back from the bridge map for a
        /// known faucet id (the exact decode the restore-rebuild relies on).
        #[test]
        fn finding_6_reads_conversion_metadata_from_bridge_map() {
            let faucet = faucet_id();
            let storage = fabricate_bridge_storage(faucet, USDC, 3, 10);

            let conv = read_faucet_conversion_metadata(&storage, faucet)
                .expect("faucet is registered on the bridge — must decode");
            assert_eq!(conv.origin_address, USDC);
            assert_eq!(conv.origin_network, 3);
            assert_eq!(conv.scale, 10);

            // A faucet NOT in the map decodes to None (absent → zero word).
            let other = AccountId::from_hex("0xaa0000000000bb110000cc000000fd").unwrap();
            assert!(read_faucet_conversion_metadata(&storage, other).is_none());
        }

        /// Enumerate + match: the recover-existing lookup finds the faucet for an
        /// origin (address, network) pair, and returns None for an unregistered
        /// origin (the case where the live path WOULD deploy).
        #[test]
        fn finding_6_find_registered_faucet_for_origin_recovers_existing_generation() {
            let faucet = faucet_id();
            let storage = fabricate_bridge_storage(faucet, USDC, 0, 8);

            assert_eq!(enumerate_registered_faucet_ids(&storage), vec![faucet]);

            let (found, conv) = find_registered_faucet_for_origin(&storage, &USDC, 0)
                .expect("existing on-chain faucet must be recovered, not re-deployed");
            assert_eq!(found, faucet);
            assert_eq!(conv.scale, 8);

            // Same address, DIFFERENT network → not a match (no recovery).
            assert!(find_registered_faucet_for_origin(&storage, &USDC, 1).is_none());
            // Unknown token → not a match.
            assert!(find_registered_faucet_for_origin(&storage, &[0x22u8; 20], 0).is_none());
        }

        /// End-to-end PoC of the restore-rebuild gap-closure at the store +
        /// decode layer: with the local row REMOVED, `resolve_faucet_origin`
        /// errors (pre-fix: the historical bridge-out is quarantined as
        /// UnknownFaucet). After rebuilding the row from the authoritative
        /// bridge `faucet_metadata_map`, it succeeds with the correct origin —
        /// so the historical exit replays instead of being skipped.
        #[tokio::test]
        async fn finding_6_restore_rebuild_makes_resolve_faucet_origin_succeed() {
            let faucet = faucet_id();
            let bridge_storage = fabricate_bridge_storage(faucet, USDC, 6, 10);
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

            // Pre-fix: local row missing → resolve errors (this is exactly what
            // makes `restore_bridge_outs` / `BridgeOutScanner` skip the exit).
            let err = resolve_faucet_origin(faucet, &*store)
                .await
                .err()
                .expect("missing local row must error before rebuild");
            assert!(
                format!("{err:#}").contains("unknown faucet"),
                "unexpected error: {err:#}"
            );

            // Rebuild the row from authoritative bridge state (the address /
            // network / scale come straight from the on-chain map; symbol /
            // decimals would come from the faucet account on the live path).
            let conv = read_faucet_conversion_metadata(&bridge_storage, faucet).unwrap();
            let miden_decimals = 8u8;
            store
                .register_faucet(FaucetEntry {
                    faucet_id: faucet,
                    origin_address: conv.origin_address,
                    origin_network: conv.origin_network,
                    symbol: "USDC".into(),
                    origin_decimals: miden_decimals + conv.scale,
                    miden_decimals,
                    scale: conv.scale,
                    metadata: vec![],
                })
                .await
                .unwrap();

            // Post-fix: resolve succeeds with the reconstructed origin.
            let origin = resolve_faucet_origin(faucet, &*store)
                .await
                .expect("rebuilt row must resolve");
            assert_eq!(origin.origin_address, USDC);
            assert_eq!(origin.origin_network, 6);
            assert_eq!(origin.scale, 10);
        }

        // ── Finding #62 — multi-network restore-metadata ─────────────────────

        /// The per-network RPC selection (the exact `network_rpcs.get(&n)` used at
        /// the two restore recovery sites) routes an L2B-origin (network 2) token
        /// to the L2B RPC and an L1 (network 0) token to the L1 RPC — and yields
        /// None for an unmapped network (recovery then relies on the all-Miden
        /// candidate only, the pre-#62 behavior). This is what stops an L2B token
        /// being validated against the wrong (L1) chain and failing the keccak gate.
        #[test]
        fn finding_62_network_rpc_map_selects_rpc_by_origin_network() {
            let mut rpcs = NetworkRpcMap::new();
            rpcs.insert(0, "http://l1:8545".to_string());
            rpcs.insert(2, "http://anvil-l2b:8545".to_string());

            assert_eq!(rpcs.get(&0).map(String::as_str), Some("http://l1:8545"));
            assert_eq!(
                rpcs.get(&2).map(String::as_str),
                Some("http://anvil-l2b:8545")
            );
            // An unmapped network (e.g. a third rollup we weren't configured for)
            // selects no RPC — recovery falls back to the all-Miden candidate.
            assert_eq!(rpcs.get(&3).map(String::as_str), None);
            // Empty map == pre-#62 "no L1 RPC configured": every lookup is None.
            assert_eq!(NetworkRpcMap::new().get(&0).map(String::as_str), None);
        }

        /// A restore rebuild for an L2B-origin faucet (origin_network=2) must carry
        /// the correct `symbol` and `origin_network` into the rebuilt row. The
        /// symbol comes from the Miden faucet account (network-independent), so it
        /// survives restore regardless of which chain the metadata PREIMAGE is
        /// recovered from — proving the registry row itself is network-agnostic
        /// and only the metadata-preimage RPC (tested above) was single-network.
        #[tokio::test]
        async fn finding_62_restore_rebuilds_l2b_origin_row_with_symbol() {
            let faucet = faucet_id();
            // origin_network = 2 (L2B), a token that would previously be validated
            // against L1 and lose its metadata.
            let bridge_storage = fabricate_bridge_storage(faucet, USDC, 2, 4);
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

            let conv = read_faucet_conversion_metadata(&bridge_storage, faucet).unwrap();
            assert_eq!(conv.origin_network, 2, "L2B network must decode faithfully");
            let miden_decimals = 6u8;
            store
                .register_faucet(FaucetEntry {
                    faucet_id: faucet,
                    origin_address: conv.origin_address,
                    origin_network: conv.origin_network,
                    symbol: "MOP".into(),
                    origin_decimals: miden_decimals + conv.scale,
                    miden_decimals,
                    scale: conv.scale,
                    metadata: vec![],
                })
                .await
                .unwrap();

            let origin = resolve_faucet_origin(faucet, &*store)
                .await
                .expect("rebuilt L2B row must resolve");
            assert_eq!(origin.origin_network, 2);
            assert_eq!(origin.origin_address, USDC);
            let row = store.get_faucet_by_id(faucet).await.unwrap().unwrap();
            assert_eq!(row.symbol, "MOP", "L2B token symbol must survive restore");
            assert_eq!(row.origin_network, 2);
        }
    }

    /// (i) RED→GREEN: a faucet row with EMPTY metadata for an ERC-20, plus a
    /// candidate whose `abi.encode(...)` keccak matches the (mocked) bridge hash,
    /// must RECOVER the correct preimage. The first (non-matching) candidate is
    /// skipped and the matching one wins, proving the fallback chain.
    #[test]
    fn recovers_erc20_metadata_on_keccak_match() {
        // The authoritative bridge hash was computed over the ORIGIN name.
        let expected = hash_of("USD Coin", "USDC", 6);

        // Candidate 0 mimics the all-Miden source (name == symbol) — does NOT
        // match. Candidate 1 mimics the L1 source (real name) — matches.
        let candidates = vec![
            MetadataCandidate {
                name: "USDC".into(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            MetadataCandidate {
                name: "USD Coin".into(),
                symbol: "USDC".into(),
                decimals: 6,
            },
        ];

        let out = resolve_emit_metadata(&USDC, &[], &candidates, Some(expected));

        let expected_bytes = rederive_token_metadata("USD Coin", "USDC", 6);
        assert_eq!(
            out,
            EmitMetadata::Recovered(expected_bytes),
            "matching candidate must yield Recovered with the exact preimage bytes"
        );
    }

    /// (ii) keccak MISMATCH (no candidate hashes to the bridge value) must GATE:
    /// return Unrecoverable so the caller never emits empty/unvalidated metadata.
    #[test]
    fn gates_erc20_on_keccak_mismatch() {
        let expected = hash_of("USD Coin", "USDC", 6);
        let candidates = vec![
            MetadataCandidate {
                name: "Wrong".into(),
                symbol: "WRG".into(),
                decimals: 18,
            },
            MetadataCandidate {
                name: "USDC".into(),
                symbol: "USDC".into(),
                decimals: 6,
            },
        ];

        let out = resolve_emit_metadata(&USDC, &[], &candidates, Some(expected));
        assert_eq!(
            out,
            EmitMetadata::Unrecoverable,
            "no candidate matches → must gate"
        );
    }

    /// (ii-b) ERC-20 with empty metadata and NO bridge hash available (unreadable)
    /// must also gate — we can validate nothing.
    #[test]
    fn gates_erc20_when_hash_unavailable() {
        let candidates = vec![MetadataCandidate {
            name: "USD Coin".into(),
            symbol: "USDC".into(),
            decimals: 6,
        }];
        let out = resolve_emit_metadata(&USDC, &[], &candidates, None);
        assert_eq!(
            out,
            EmitMetadata::Unrecoverable,
            "missing bridge hash → must gate"
        );
    }

    /// (iii) Native ETH (zero origin address) with empty metadata must STILL emit
    /// empty — unchanged behaviour, never recovered, never gated, even if a bogus
    /// hash/candidates are supplied.
    #[test]
    fn native_eth_empty_metadata_emits_empty_unchanged() {
        let out = resolve_emit_metadata(
            &NATIVE_TOKEN_ADDRESS,
            &[],
            &[MetadataCandidate {
                name: "x".into(),
                symbol: "y".into(),
                decimals: 1,
            }],
            Some([0xabu8; 32]),
        );
        assert_eq!(
            out,
            EmitMetadata::Ready(Vec::new()),
            "native ETH empty metadata stays empty"
        );
    }

    /// Layer-1 happy path: non-empty stored metadata is emitted as-is, no
    /// recovery attempted (works for ERC-20 and native alike).
    #[test]
    fn present_metadata_emitted_as_ready() {
        let stored = rederive_token_metadata("USD Coin", "USDC", 6);
        let out = resolve_emit_metadata(&USDC, &stored, &[], None);
        assert_eq!(out, EmitMetadata::Ready(stored));
    }
}
