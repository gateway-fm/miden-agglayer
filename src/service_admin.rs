//! Admin RPC endpoint for explicit faucet registration.
//!
//! `admin_registerFaucet` creates a faucet on Miden, registers it in the bridge,
//! and saves its metadata to the Store. This is an alternative to auto-creation
//! during the first claim — useful for pre-staging tokens.

use crate::faucet_ops;
use crate::service_state::ServiceState;
use crate::store::FaucetEntry;
use miden_base_agglayer::MetadataHash;
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

/// `admin_recoverUnbridgeableBridgeOuts` — Cantina MA#18 recovery entrypoint.
///
/// Sweeps the `unbridgeable_bridge_outs` quarantine table and re-emits the
/// synthetic `BridgeEvent` for every row whose blocker is now resolved (e.g. an
/// `unknown_faucet` note whose faucet has since been registered via
/// `admin_registerFaucet`). Recovered rows advance `deposit_count` (closing the
/// Cantina #9 LET divergence) and are deleted; rows that stay blocked (truly
/// erased storage, faucet still unknown, self-targeted poison) are left in place
/// as the durable operator handle. Idempotent — safe to call repeatedly.
///
/// Returns the sweep summary `{attempted, recovered, stale_cleared,
/// still_blocked}`.
pub async fn admin_recover_unbridgeable_bridge_outs(
    state: ServiceState,
) -> anyhow::Result<serde_json::Value> {
    let summary = crate::bridge_out_recovery::recover_all_unbridgeable_bridge_outs(
        &state.store,
        &state.block_state,
        crate::bridge_address::get_bridge_address(),
        state.network_id,
    )
    .await?;
    tracing::info!(
        attempted = summary.attempted,
        recovered = summary.recovered,
        stale_cleared = summary.stale_cleared,
        still_blocked = summary.still_blocked,
        "admin_recoverUnbridgeableBridgeOuts: MA#18 recovery sweep complete"
    );
    Ok(serde_json::json!({
        "attempted": summary.attempted,
        "recovered": summary.recovered,
        "stale_cleared": summary.stale_cleared,
        "still_blocked": summary.still_blocked,
    }))
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
