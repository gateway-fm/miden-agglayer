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
use miden_protocol::account::{Account, AccountId};
use miden_protocol::{Felt, Word};

/// Metric incremented whenever an ERC-20 bridge-out is gated because its
/// metadata could not be recovered + validated. A non-zero value means real
/// user bridge-outs are being deferred and an operator must backfill the faucet
/// registry (e.g. re-run `admin_registerFaucet` with the correct name) or supply
/// a reachable L1 RPC for the token's origin network.
pub const METADATA_UNRECOVERABLE_METRIC: &str = "bridge_out_metadata_unrecoverable_total";

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
