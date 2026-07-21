//! Admin RPC endpoint for explicit faucet registration.
//!
//! `admin_registerFaucet` creates a faucet on Miden, registers it in the bridge,
//! and saves its metadata to the Store. This is an alternative to auto-creation
//! during the first claim — useful for pre-staging tokens.

use crate::faucet_ops;
use crate::service_state::ServiceState;
use crate::store::FaucetEntry;
use miden_base_agglayer::{EthAddress, MetadataHash};
use miden_protocol::account::AccountId;
use serde::Deserialize;
use std::sync::{Arc, OnceLock};

// Mirror of the upstream `miden-agglayer` `SolTokenMetadata` struct (its
// `encode_token_metadata` is `pub(crate)` so we can't call it directly). Encoding
// this with `abi_encode_params` reproduces Solidity's `abi.encode(string name,
// string symbol, uint8 decimals)` byte-for-byte, so `keccak256(bytes)` equals the
// faucet's `MetadataHash` (Cantina #13). A plain tuple `.abi_encode()` won't do —
// `u8` doesn't implement `SolValue`, and a dynamic tuple's `abi_encode` would add an
// extra offset word.
alloy_core::sol! {
    struct AdminTokenMetadata {
        string name;
        string symbol;
        uint8 decimals;
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterFaucetParams {
    pub symbol: String,
    pub origin_token_address: String,
    pub origin_network: u32,
    pub origin_decimals: u8,
    /// DEPRECATED / IGNORED. The local faucet decimals are computed as
    /// `min(origin_decimals, `[`faucet_ops::MIDEN_DECIMALS`]` (8))` — capped at 8
    /// (finding #17). The field is retained only for request-shape compatibility;
    /// whatever value a caller sends is discarded. Routability is decided purely
    /// by `origin_decimals` (must be `<= MAX_ORIGIN_DECIMALS (26)`).
    ///
    /// Accepted (via `serde`) for request-shape compatibility but deliberately
    /// never read — the value is discarded in favour of the fixed constant.
    #[serde(default)]
    #[allow(dead_code)]
    pub miden_decimals: u8,
    /// Token display name used when computing the `MetadataHash`. Optional —
    /// defaults to the symbol if not provided.
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn admin_register_faucet(
    state: ServiceState,
    params: RegisterFaucetParams,
) -> anyhow::Result<String> {
    let origin_address = parse_eth_address(&params.origin_token_address)?;

    // Check if already registered FIRST — before any parameter validation.
    // `admin_registerFaucet` is strictly register-if-absent / return-existing-if-present:
    // an existing route for this `(origin_address, origin_network)` is ALWAYS returned
    // idempotently, regardless of the params supplied on the re-register call. Validating
    // before this lookup would break idempotency — an idempotent re-register that happened
    // to carry imperfect decimals would surface a validation error instead of the route
    // that already exists. Validation therefore gates ONLY new-route creation (below).
    // There is deliberately no live "replace" path — swapping a route would DELETE the old
    // (origin_address, origin_network) row and orphan any holder still carrying
    // balances in the old faucet, re-creating the Cantina finding #6 split-brain
    // (their bridge-outs would resolve to an "unknown faucet ID" and quarantine,
    // burning funds on L2 with no L1 claim). Disaster-recovery route repair, if
    // ever needed, is a purpose-built throwaway image, not a standing endpoint.
    if let Some(existing) = state
        .store
        .get_faucet_by_origin(&origin_address, params.origin_network)
        .await?
    {
        let id = existing.faucet_id.to_hex();
        tracing::info!(
            faucet_id = %id,
            "admin_registerFaucet: faucet already exists for this origin"
        );
        return Ok(id);
    }

    // New-route creation path only. The faucet decimals are capped at
    // `MIDEN_DECIMALS` (8): `miden_decimals = min(origin_decimals, 8)` (finding
    // #17). The caller's `params.miden_decimals` is IGNORED (a route can never be
    // created with a caller-chosen decimal count). A low-decimal origin token
    // (e.g. 6-decimal USDC/USDT) routes 1:1 at scale 0; a high-decimal token pins
    // to 8. Routability then reduces to a single check on the origin token: the
    // downscaling factor `scale = origin_decimals - min(origin_decimals, 8)` must
    // fit MAX_SCALING_FACTOR (18, enforced at runtime by
    // `EthAmount::scale_to_token_amount`), i.e. `origin_decimals <= 26`. Reject
    // unclaimable routes up-front so a poisoned entry is never persisted.
    let miden_decimals = params.origin_decimals.min(faucet_ops::MIDEN_DECIMALS);
    // `miden_decimals <= origin_decimals` by construction — this never underflows;
    // it stays only as a defensive invariant guard.
    let scale = params
        .origin_decimals
        .checked_sub(miden_decimals)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "internal invariant violated: miden_decimals ({}) > origin_decimals ({})",
                miden_decimals,
                params.origin_decimals,
            )
        })?;

    if scale > faucet_ops::MAX_SCALING_FACTOR {
        anyhow::bail!(
            "scale ({scale} = origin_decimals {} - capped miden_decimals {}) exceeds the shared limit \
             of {} (route would be unclaimable). origin_decimals must be <= {} (= {} + {}).",
            params.origin_decimals,
            miden_decimals,
            faucet_ops::MAX_SCALING_FACTOR,
            faucet_ops::MAX_ORIGIN_DECIMALS,
            faucet_ops::MIDEN_DECIMALS,
            faucet_ops::MAX_SCALING_FACTOR,
        );
    }

    let accounts = &state.accounts.0;
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;

    // Compute the raw ABI metadata preimage `abi.encode(name, symbol, decimals)` ONCE and
    // reuse it for both the on-Miden `MetadataHash` and the stored `FaucetEntry.metadata`
    // (Cantina #13). Using `abi_encode_params` (not `abi_encode`) matches Solidity's
    // `abi.encode(string, string, uint8)` exactly — a plain `abi_encode` of a dynamic tuple
    // would prepend an extra 32-byte offset word and diverge from the L1 bridge's
    // `getTokenMetadata` encoding. Deriving the hash via `from_abi_encoded(&metadata_bytes)`
    // guarantees `keccak256(stored_metadata) == faucet MetadataHash`, so a later bridge-out
    // emits metadata whose hash matches Miden's bridge state. Callers that skip the `name`
    // field get `name = symbol`.
    use alloy_core::sol_types::SolValue;
    let metadata_name = params.name.clone().unwrap_or_else(|| params.symbol.clone());
    let metadata_bytes = AdminTokenMetadata {
        name: metadata_name.clone(),
        symbol: params.symbol.clone(),
        decimals: params.origin_decimals,
    }
    .abi_encode_params();
    let metadata_hash = MetadataHash::from_abi_encoded(&metadata_bytes);

    // Create, deploy, register in bridge (using OnceLock pattern like publish_claim)
    let result = Arc::new(OnceLock::<AccountId>::new());
    let result_inner = result.clone();
    // Cantina #6 — set once the closure RECOVERED an existing on-chain faucet
    // (and already persisted its local row), so the post-closure create-path
    // registration is skipped.
    let recovered = Arc::new(OnceLock::<()>::new());
    let recovered_inner = recovered.clone();
    let store_for_closure = state.store.clone();
    let symbol_clone = params.symbol.clone();
    // `miden_decimals` was capped to min(origin_decimals, 8) above; reuse it (the
    // caller's params value is deliberately ignored).
    let origin_network = params.origin_network;
    // The admin-supplied metadata IS the authoritative preimage for this token;
    // prefer it over on-chain recovery when importing an existing faucet.
    let metadata_for_recovery = metadata_bytes.clone();

    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                // Cantina #6 — recover an EXISTING on-chain faucet for this origin
                // token before deploying a replacement generation. Mirrors the live
                // claim path: the local row is missing but the faucet may still be
                // registered on the bridge.
                if let Some(bridge_account) = client.get_account(bridge_id).await.ok().flatten()
                    && let Some((existing_id, conversion)) =
                        crate::metadata_recovery::find_registered_faucet_for_origin(
                            bridge_account.storage(),
                            &origin_address,
                            origin_network,
                        )
                {
                    tracing::warn!(
                        faucet_id = %existing_id,
                        origin_network,
                        "admin_registerFaucet: origin token already has a faucet registered on \
                         the bridge but no local row — importing the existing identity instead \
                         of deploying a replacement (Cantina #6)"
                    );
                    match faucet_ops::rebuild_faucet_entry_from_chain(
                        client,
                        &bridge_account,
                        existing_id,
                        &conversion,
                        None,
                    )
                    .await
                    {
                        Ok(mut entry) => {
                            entry.metadata = metadata_for_recovery;
                            store_for_closure.register_faucet(entry).await?;
                            ::metrics::counter!("faucet_recovered_existing_total").increment(1);
                            let _ = result_inner.set(existing_id);
                            let _ = recovered_inner.set(());
                            return Ok(());
                        }
                        Err(e) => {
                            ::metrics::counter!("faucet_recover_existing_failed_total")
                                .increment(1);
                            tracing::warn!(
                                faucet_id = %existing_id,
                                error = ?e,
                                "admin_registerFaucet: failed to import existing faucet identity; \
                                 falling back to deploy (WARNING: may create a second generation)"
                            );
                        }
                    }
                }

                let account = faucet_ops::create_and_register_faucet(
                    client,
                    &symbol_clone,
                    miden_decimals,
                    &origin_address,
                    origin_network,
                    scale,
                    service_id,
                    bridge_id,
                    metadata_hash,
                    false, // admin_registerFaucet: bridge-owned mint/burn (not Miden-native)
                )
                .await?;
                let _ = result_inner.set(account.id());
                Ok(())
            })
        })
        .await?;

    let faucet_id = *result.get().ok_or_else(|| {
        anyhow::anyhow!("admin_registerFaucet: closure completed but result not set")
    })?;

    // Save to store — UNLESS the closure already recovered + persisted an existing
    // faucet identity (Cantina #6), in which case the row is already written. This
    // path is only reached when no route existed for the origin (existing routes
    // returned early above), so a plain insert is correct — there is deliberately no
    // live "replace" path. `miden_decimals` here is the capped local (finding #17),
    // NOT the ignored `params.miden_decimals`.
    if recovered.get().is_none() {
        state
            .store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address,
                origin_network: params.origin_network,
                symbol: params.symbol,
                origin_decimals: params.origin_decimals,
                miden_decimals,
                scale,
                metadata: metadata_bytes,
            })
            .await?;
    }

    let id_hex = faucet_id.to_hex();
    tracing::info!(
        faucet_id = %id_hex,
        "admin_registerFaucet: faucet created and registered"
    );
    Ok(id_hex)
}

#[derive(Debug, Deserialize)]
pub struct RegisterNativeFaucetParams {
    /// The EXISTING, externally-deployed Miden faucet account id (hex) to allowlist as
    /// native. The proxy does NOT create it — an external party (e.g. the bridge-out
    /// app's `--create-native-faucet`) deploys + mints it first.
    pub faucet_id: String,
    /// The 20-byte origin token address the bridge records for this native faucet (its
    /// canonical L1/agglayer-side representation).
    pub origin_token_address: String,
    pub symbol: String,
    pub decimals: u8,
    #[serde(default)]
    pub name: Option<String>,
}

/// Authoritative token metadata read from a deployed Miden faucet account
/// (`token_name` / `symbol` / `decimals`). This is the ONLY source from which the
/// registered metadata-hash preimage can be reconstructed after database loss, so
/// it — not caller-supplied params — is what `admin_registerNativeFaucet` persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthoritativeFaucetMetadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// Resolved native-faucet metadata to persist/emit — always the deployed faucet
/// account's authoritative values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedNativeMetadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// Issue #149 — validate caller-supplied native-faucet metadata against the
/// deployed Miden faucet account's authoritative values and RESOLVE the metadata
/// to persist/emit from that authoritative state.
///
/// The proxy persists + emits a metadata hash whose preimage is
/// `abi.encode(name, symbol, decimals)`; after database loss, recovery
/// reconstructs that preimage ONLY from the deployed faucet account
/// (`metadata_recovery::miden_faucet_candidate`). If the registered preimage came
/// from caller-supplied params that differ from the deployed faucet, the hash's
/// preimage is unreconstructable and its poison leaf halts restore fail-closed.
///
/// Rules (each mismatch rejected INDEPENDENTLY, before any state change):
/// - `symbol` must equal the faucet's actual symbol.
/// - `decimals` must equal the faucet's actual decimals.
/// - `name`, if supplied, must equal the faucet's actual token name EXACTLY — a
///   custom `name != symbol` is valid and preserved, never normalized to symbol.
/// - `name` omitted → resolve to the faucet's actual token name (so an omitted
///   name always succeeds with the authoritative name).
///
/// Pure (no I/O) so the whole decision is unit-testable; the caller supplies the
/// authoritative triple read from the faucet account.
pub(crate) fn resolve_native_faucet_metadata(
    requested_name: Option<&str>,
    requested_symbol: &str,
    requested_decimals: u8,
    authoritative: &AuthoritativeFaucetMetadata,
) -> anyhow::Result<ResolvedNativeMetadata> {
    if requested_symbol != authoritative.symbol {
        anyhow::bail!(
            "admin_registerNativeFaucet: symbol mismatch — requested {requested_symbol:?}, \
             deployed faucet account has {:?}; the deployed faucet is authoritative. No \
             registry row was written; re-register with the faucet's actual symbol.",
            authoritative.symbol
        );
    }
    if requested_decimals != authoritative.decimals {
        anyhow::bail!(
            "admin_registerNativeFaucet: decimals mismatch — requested {requested_decimals}, \
             deployed faucet account has {}; the deployed faucet is authoritative. No \
             registry row was written; re-register with the faucet's actual decimals.",
            authoritative.decimals
        );
    }
    if let Some(name) = requested_name
        && name != authoritative.name
    {
        anyhow::bail!(
            "admin_registerNativeFaucet: name mismatch — requested {name:?}, deployed faucet \
             account has {:?}; a custom name must match the on-chain token name exactly and \
             is never normalized. No registry row was written; re-register with the faucet's \
             actual name or omit `name` to adopt it.",
            authoritative.name
        );
    }
    // Omitted name resolves to the authoritative name (which may legitimately differ
    // from the symbol — a valid custom name is preserved, never collapsed to symbol).
    Ok(ResolvedNativeMetadata {
        name: authoritative.name.clone(),
        symbol: authoritative.symbol.clone(),
        decimals: authoritative.decimals,
    })
}

/// `admin_registerNativeFaucet` — allowlist an EXTERNALLY-deployed Miden-ORIGINATED
/// (native lock/unlock) faucet on the bridge. Faucet bridging is a PERMISSIONED
/// ALLOWLIST: only the bridge admin can register, and the admin IS this proxy's
/// `service` account — so the external party deploys the faucet, the PROXY registers it.
///
/// This is the REQUEST side (mirrors `ger.rs::insert_ger`): it sends the admin
/// `ConfigAggBridgeNote` with `is_native = true` and persists the proxy-store row. The
/// faucet-registry DISCOVERY module is the decoupled read side — it reconciles entries
/// registered by anyone (including a different admin, adopted with a `warn!`) and after
/// a restart, independent of and order-agnostic to this request.
///
/// Native means the token ORIGINATES on this Miden network, so the origin network is
/// this proxy's CONFIGURED `network_id` (never hardcoded 1): `is_native` is derivable as
/// `origin_network == service.network_id`. There is no L1<->Miden decimal scaling for a
/// native token (`scale = 0`, `origin_decimals == miden_decimals`).
pub async fn admin_register_native_faucet(
    state: ServiceState,
    params: RegisterNativeFaucetParams,
) -> anyhow::Result<String> {
    let faucet_id = AccountId::from_hex(&params.faucet_id)
        .map_err(|e| anyhow::anyhow!("bad faucet_id {}: {e:?}", params.faucet_id))?;
    let origin_address = parse_eth_address(&params.origin_token_address)?;
    let origin_network = state.network_id;
    let scale = 0u8;

    // Idempotent: an existing native route for this (origin_address, origin_network) is
    // returned as-is (register-if-absent), matching admin_registerFaucet's contract.
    if let Some(existing) = state
        .store
        .get_faucet_by_origin(&origin_address, origin_network)
        .await?
    {
        // Return the EXISTING route's faucet_id, not the caller-supplied one: the origin
        // is already bound to `existing.faucet_id`, and echoing the caller's (possibly
        // different) id would falsely imply THAT faucet is the registered route.
        tracing::info!(
            origin_network,
            existing_faucet_id = %existing.faucet_id.to_hex(),
            requested_faucet_id = %faucet_id.to_hex(),
            "admin_registerNativeFaucet: a route already exists for this origin — returning the existing route's faucet"
        );
        return Ok(existing.faucet_id.to_hex());
    }

    // #149 — read the deployed faucet account's AUTHORITATIVE metadata BEFORE any
    // state change. The persisted + emitted metadata-hash preimage
    // (`abi.encode(name, symbol, decimals)`) must be reconstructable from chain
    // state after database loss: recovery derives its only candidate from the
    // faucet account (`metadata_recovery::miden_faucet_candidate`). So the
    // registered preimage MUST come from the faucet account, not caller-supplied
    // params — otherwise the hash is unrecoverable and its poison leaf halts
    // restore. `with()` returning before this populates the slot (or erroring) is
    // fail-closed: we bail before touching the bridge or the registry.
    let authoritative = Arc::new(std::sync::Mutex::new(None::<AuthoritativeFaucetMetadata>));
    let authoritative_read = authoritative.clone();
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                // Native faucets are externally deployed and NOT in
                // bridge_accounts.toml, so import them on demand before reading.
                if client.get_account(faucet_id).await.ok().flatten().is_none()
                    && let Err(e) = client.import_account_by_id(faucet_id).await
                {
                    anyhow::bail!(
                        "admin_registerNativeFaucet: cannot import faucet account \
                         {faucet_id} from node: {e}"
                    );
                }
                let faucet_account = client
                    .get_account(faucet_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("get_account({faucet_id}): {e}"))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "admin_registerNativeFaucet: faucet account {faucet_id} not \
                             found after import"
                        )
                    })?;
                // Both supported kinds expose the standard FungibleFaucet metadata, but
                // admin_registerNativeFaucet is for operator-owned NATIVE faucets only.
                // Guard against an operator accidentally reclassifying an already-deployed
                // AggLayer-owned (bridge mint/burn) faucet as native (PR #150 review).
                let (kind, faucet) = faucet_ops::classify_faucet_account(&faucet_account)?;
                if kind != faucet_ops::FaucetKind::NativeFungible {
                    anyhow::bail!(
                        "admin_registerNativeFaucet: faucet account {faucet_id} is an \
                         AggLayer-owned (bridge mint/burn) faucet, not an operator-owned \
                         native FungibleFaucet — refusing to register it as native; no \
                         registry row written"
                    );
                }
                *authoritative_read.lock().unwrap() = Some(AuthoritativeFaucetMetadata {
                    name: faucet.token_name().as_str().to_string(),
                    symbol: faucet.symbol().to_string(),
                    decimals: faucet.decimals(),
                });
                Ok(())
            })
        })
        .await?;
    let authoritative = authoritative.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!(
            "admin_registerNativeFaucet: could not read faucet account {faucet_id} \
             metadata (no authoritative token name/symbol/decimals); refusing to register \
             — no registry row written"
        )
    })?;

    // #149 — validate the caller-supplied values against authoritative chain state
    // (symbol, decimals, and name each independently) and RESOLVE the metadata to
    // persist/emit from the faucet account. Any mismatch bails here, before the
    // bridge ConfigAggBridgeNote or the registry row — no partial state change.
    let resolved = resolve_native_faucet_metadata(
        params.name.as_deref(),
        &params.symbol,
        params.decimals,
        &authoritative,
    )?;

    // abi.encode(name, symbol, decimals) — same preimage the bridge/L2 wrapped-token
    // metadata hashes to (Cantina #13); reused for the on-Miden MetadataHash + the
    // stored FaucetEntry.metadata so a later bridge-out emits matching metadata. Built
    // from the AUTHORITATIVE (resolved) values so the preimage is always recoverable.
    use alloy_core::sol_types::SolValue;
    let metadata_bytes = AdminTokenMetadata {
        name: resolved.name.clone(),
        symbol: resolved.symbol.clone(),
        decimals: resolved.decimals,
    }
    .abi_encode_params();
    let metadata_hash = MetadataHash::from_abi_encoded(&metadata_bytes);

    let accounts = &state.accounts.0;
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;
    let symbol_clone = resolved.symbol.clone();

    // REQUEST: admin ConfigAggBridgeNote registering the EXISTING faucet as native
    // (is_native = true). Register only — the faucet was deployed externally.
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                let origin_addr = EthAddress::new(origin_address);
                faucet_ops::register_faucet_in_bridge(
                    client,
                    service_id,
                    bridge_id,
                    faucet_id,
                    &origin_addr,
                    origin_network,
                    scale,
                    metadata_hash,
                    &symbol_clone,
                    true, // is_native — Miden-ORIGINATED lock/unlock faucet
                )
                .await
            })
        })
        .await?;

    // Persist the proxy-store row (origin_network == the configured net id => is_native
    // is derivable; no separate column). All fields come from the authoritative
    // (resolved) metadata, never caller-supplied.
    state
        .store
        .register_faucet(FaucetEntry {
            faucet_id,
            origin_address,
            origin_network,
            symbol: resolved.symbol.clone(),
            origin_decimals: resolved.decimals, // native: no L1<->Miden scaling
            miden_decimals: resolved.decimals,
            scale,
            metadata: metadata_bytes,
        })
        .await?;

    let id_hex = faucet_id.to_hex();
    tracing::info!(
        faucet_id = %id_hex,
        origin_network,
        "admin_registerNativeFaucet: native faucet allowlisted on the bridge"
    );
    Ok(id_hex)
}

fn parse_eth_address(s: &str) -> anyhow::Result<[u8; 20]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s)?;
    if bytes.len() != 20 {
        anyhow::bail!(
            "invalid ETH address: expected 20 bytes, got {}",
            bytes.len()
        );
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FaucetEntry;
    use crate::test_helpers::create_test_service;
    use miden_protocol::account::AccountId;

    // ── #149: pure native-faucet metadata validation/resolution ──────────────
    // The deployed faucet account is authoritative; caller-supplied metadata is
    // validated against it and the persisted preimage is resolved FROM it.

    fn authoritative(name: &str, symbol: &str, decimals: u8) -> AuthoritativeFaucetMetadata {
        AuthoritativeFaucetMetadata {
            name: name.into(),
            symbol: symbol.into(),
            decimals,
        }
    }

    /// Exact match (name == symbol) resolves to the authoritative triple.
    #[test]
    fn native_metadata_exact_match_resolves() {
        let auth = authoritative("MDN", "MDN", 8);
        let resolved =
            resolve_native_faucet_metadata(Some("MDN"), "MDN", 8, &auth).expect("match succeeds");
        assert_eq!(resolved.name, "MDN");
        assert_eq!(resolved.symbol, "MDN");
        assert_eq!(resolved.decimals, 8);
    }

    /// A wrong symbol is rejected independently with a specific error.
    #[test]
    fn native_metadata_symbol_mismatch_rejected() {
        let auth = authoritative("MDN", "MDN", 8);
        let err = resolve_native_faucet_metadata(Some("MDN"), "WRONG", 8, &auth).unwrap_err();
        assert!(
            err.to_string().contains("symbol mismatch"),
            "unexpected error: {err}"
        );
    }

    /// Wrong decimals are rejected independently.
    #[test]
    fn native_metadata_decimals_mismatch_rejected() {
        let auth = authoritative("MDN", "MDN", 8);
        let err = resolve_native_faucet_metadata(Some("MDN"), "MDN", 6, &auth).unwrap_err();
        assert!(
            err.to_string().contains("decimals mismatch"),
            "unexpected error: {err}"
        );
    }

    /// A supplied name that differs from the on-chain token name is rejected —
    /// a custom name must match exactly, never normalized.
    #[test]
    fn native_metadata_name_mismatch_rejected() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let err =
            resolve_native_faucet_metadata(Some("Something Else"), "MDN", 8, &auth).unwrap_err();
        assert!(
            err.to_string().contains("name mismatch"),
            "unexpected error: {err}"
        );
    }

    /// An omitted name adopts the authoritative token name, and a custom
    /// `name != symbol` is preserved exactly (never collapsed to the symbol) — so
    /// the resolved preimage keccak-matches the deployed faucet's stored hash and
    /// survives database-loss restore.
    #[test]
    fn native_metadata_omitted_name_adopts_custom_name() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let resolved = resolve_native_faucet_metadata(None, "MDN", 8, &auth)
            .expect("omitted name succeeds by adopting the authoritative name");
        assert_eq!(resolved.name, "Wrapped Midnight");
        assert_ne!(
            resolved.name, resolved.symbol,
            "a custom name must be preserved, not normalized to the symbol"
        );
        assert_eq!(resolved.symbol, "MDN");
        // The persisted preimage is abi.encode(resolved.name, symbol, decimals);
        // recovery's miden_faucet_candidate rebuilds the SAME triple from the
        // faucet account's token_name(), so the keccak hash matches on restore.
        use alloy_core::sol_types::SolValue;
        let bytes = AdminTokenMetadata {
            name: resolved.name.clone(),
            symbol: resolved.symbol.clone(),
            decimals: resolved.decimals,
        }
        .abi_encode_params();
        let from_authoritative = AdminTokenMetadata {
            name: auth.name.clone(),
            symbol: auth.symbol.clone(),
            decimals: auth.decimals,
        }
        .abi_encode_params();
        assert_eq!(
            bytes, from_authoritative,
            "resolved preimage must equal the authoritative preimage"
        );
    }

    /// A matching explicit custom name succeeds and is preserved.
    #[test]
    fn native_metadata_matching_custom_name_succeeds() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let resolved = resolve_native_faucet_metadata(Some("Wrapped Midnight"), "MDN", 8, &auth)
            .expect("matching custom name succeeds");
        assert_eq!(resolved.name, "Wrapped Midnight");
    }

    fn native_params(name: Option<&str>) -> RegisterNativeFaucetParams {
        RegisterNativeFaucetParams {
            // A valid protocol-0.15 faucet account id.
            faucet_id: "0xac0000000000dd110000ee000000fc".into(),
            origin_token_address: "0x000000000000000000000000000000000000dEaD".into(),
            symbol: "MDN".into(),
            decimals: 8,
            name: name.map(str::to_string),
        }
    }

    /// #149 fail-closed: when the authoritative faucet metadata cannot be read
    /// (the test client never yields an account), registration bails BEFORE the
    /// bridge ConfigAggBridgeNote and BEFORE any registry write — no partial
    /// state. `test_call_count() == 1` proves only the read was attempted and the
    /// bridge-register `with()` was never reached.
    #[tokio::test]
    async fn native_registration_fails_closed_when_metadata_unreadable() {
        let service = create_test_service();
        let origin = parse_eth_address("0x000000000000000000000000000000000000dEaD").unwrap();

        let err = admin_register_native_faucet(service.clone(), native_params(Some("MDN")))
            .await
            .expect_err("must fail closed when the faucet account metadata is unreadable");
        assert!(
            err.to_string().contains("could not read faucet account"),
            "unexpected error: {err}"
        );
        // No registry row written.
        assert!(
            service
                .store
                .get_faucet_by_origin(&origin, service.network_id)
                .await
                .unwrap()
                .is_none(),
            "a failed registration must leave no registry row"
        );
        // Only the authoritative-read `with()` was attempted; the bridge-register
        // `with()` (which would emit the ConfigAggBridgeNote) was never reached.
        assert_eq!(
            service.miden_client.test_call_count(),
            1,
            "bridge ConfigAggBridgeNote must not be emitted on a failed registration"
        );
    }

    const ORIGIN_HEX: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
    // A valid protocol-0.15 account id, reused as the poisoned faucet's id.
    const POISON_FAUCET_HEX: &str = "0xac0000000000dd110000ee000000fc";

    /// Seed a poisoned route: a 27-decimal token registered under the OLD fixed
    /// scheme (`miden = 8`, `scale = 19 > MAX_SCALING_FACTOR`) — unclaimable.
    async fn seed_poisoned_route(service: &ServiceState) {
        let origin = parse_eth_address(ORIGIN_HEX).unwrap();
        service
            .store
            .register_faucet(FaucetEntry {
                faucet_id: AccountId::from_hex(POISON_FAUCET_HEX).unwrap(),
                origin_address: origin,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 27,
                miden_decimals: 8,
                scale: 19,
                metadata: Vec::new(),
            })
            .await
            .unwrap();
    }

    fn repair_params() -> RegisterFaucetParams {
        RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27,
            // Ignored under the cap-at-8 scheme (faucet decimals = min(origin, 8));
            // left non-8 here to prove the param has no effect.
            miden_decimals: 9,
            name: None,
        }
    }

    /// An existing route short-circuits: `admin_registerFaucet` is strictly
    /// register-if-absent / return-existing-if-present. It returns the existing
    /// faucet_id and never touches Miden — there is deliberately no live "replace"
    /// path (removing one avoids re-creating the finding #6 split-brain by
    /// orphaning holders of the old faucet). DR repair is a throwaway image, not a
    /// standing endpoint.
    #[tokio::test]
    async fn existing_route_is_idempotent() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        let id = admin_register_faucet(service.clone(), repair_params())
            .await
            .unwrap();
        assert_eq!(id, AccountId::from_hex(POISON_FAUCET_HEX).unwrap().to_hex());
        // No faucet deploy attempted — the existing route was returned as-is.
        assert_eq!(service.miden_client.test_call_count(), 0);
    }

    /// Idempotency must win over validation: an existing route is ALWAYS returned,
    /// even when the re-register call carries params that would FAIL new-route
    /// validation (origin_decimals = 27 → capped scale `27 - min(27,8) = 19 >
    /// MAX_SCALING_FACTOR`). Because the existence check runs before any
    /// validation, the poisoned-but-existing route is returned as-is instead of
    /// surfacing a spurious validation error — which would otherwise break the
    /// register-if-absent / return-existing contract.
    #[tokio::test]
    async fn existing_route_returned_even_when_params_would_fail_validation() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        // These params would be rejected on a fresh origin (capped scale 27 -
        // min(27,8) = 19 > 18), but the origin already exists, so validation must
        // never run.
        let bad_params = RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27, // capped scale 27 - min(27,8) = 19 > MAX_SCALING_FACTOR
            miden_decimals: 7,   // ignored
            name: None,
        };
        let id = admin_register_faucet(service.clone(), bad_params)
            .await
            .expect("existing route must be returned without validation");
        assert_eq!(id, AccountId::from_hex(POISON_FAUCET_HEX).unwrap().to_hex());
        // Never touched Miden — no create/deploy on the existing-route path.
        assert_eq!(service.miden_client.test_call_count(), 0);
    }

    /// Finding #17 — an unsatisfiable route is rejected up-front and never
    /// persisted. Under the cap-at-8 scheme `origin_decimals = 27` yields the
    /// capped scale `27 - min(27,8) = 19 > MAX_SCALING_FACTOR` (18), i.e.
    /// origin_decimals > MAX_ORIGIN_DECIMALS (26). The `miden_decimals` param is
    /// ignored.
    #[tokio::test]
    async fn rejects_route_exceeding_scaling_factor() {
        let service = create_test_service();
        let params = RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27, // capped scale 27 - min(27,8) = 19 > 18
            miden_decimals: 7,   // ignored
            name: None,
        };
        let err = admin_register_faucet(service.clone(), params)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("exceeds the shared limit"),
            "unexpected error: {err}"
        );
        // Nothing persisted.
        let origin = parse_eth_address(ORIGIN_HEX).unwrap();
        assert!(
            service
                .store
                .get_faucet_by_origin(&origin, 0)
                .await
                .unwrap()
                .is_none()
        );
    }
}
