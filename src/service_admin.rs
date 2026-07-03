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
    pub miden_decimals: u8,
    /// Token display name used when computing the `MetadataHash`. Optional —
    /// defaults to the symbol if not provided.
    #[serde(default)]
    pub name: Option<String>,
    /// When `true`, an existing route for this `(origin_address, origin_network)`
    /// is REPAIRED: a fresh faucet is deployed on Miden and the registry row is
    /// replaced (finding #17 — poisoned/unclaimable routes can otherwise never be
    /// fixed because the first-match path returns the stale `faucet_id`). Defaults
    /// to `false`, so a normal call is still idempotent and never silently
    /// redeploys.
    #[serde(default)]
    pub replace: bool,
}

pub async fn admin_register_faucet(
    state: ServiceState,
    params: RegisterFaucetParams,
) -> anyhow::Result<String> {
    let scale = params
        .origin_decimals
        .checked_sub(params.miden_decimals)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "origin_decimals ({}) must be >= miden_decimals ({})",
                params.origin_decimals,
                params.miden_decimals
            )
        })?;

    // Finding #17 — reject unclaimable routes up-front so a poisoned entry is
    // never persisted. Both bridge-stack limits must hold: the local faucet
    // decimals must fit MAX_MIDEN_DECIMALS and the downscaling factor must fit
    // MAX_SCALING_FACTOR (enforced by `EthAmount::scale_to_token_amount`).
    if params.miden_decimals > faucet_ops::MAX_MIDEN_DECIMALS {
        anyhow::bail!(
            "miden_decimals ({}) exceeds the faucet limit of {} decimals",
            params.miden_decimals,
            faucet_ops::MAX_MIDEN_DECIMALS,
        );
    }
    if scale > faucet_ops::MAX_SCALING_FACTOR {
        anyhow::bail!(
            "scale ({scale} = origin_decimals {} - miden_decimals {}) exceeds the shared limit of \
             {} (route would be unclaimable). Raise miden_decimals so scale <= {}.",
            params.origin_decimals,
            params.miden_decimals,
            faucet_ops::MAX_SCALING_FACTOR,
            faucet_ops::MAX_SCALING_FACTOR,
        );
    }

    let origin_address = parse_eth_address(&params.origin_token_address)?;

    // Check if already registered. Without `replace`, stay idempotent and return
    // the existing faucet_id. With `replace = true`, fall through to deploy a
    // fresh faucet and swap the registry row (repairs a poisoned route).
    if let Some(existing) = state
        .store
        .get_faucet_by_origin(&origin_address, params.origin_network)
        .await?
    {
        if !params.replace {
            let id = existing.faucet_id.to_hex();
            tracing::info!(
                faucet_id = %id,
                "admin_registerFaucet: faucet already exists for this origin"
            );
            return Ok(id);
        }
        tracing::warn!(
            existing_faucet_id = %existing.faucet_id.to_hex(),
            existing_scale = existing.scale,
            "admin_registerFaucet: replace=true — deploying a fresh faucet and replacing the \
             existing route for this origin"
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
    let miden_decimals = params.miden_decimals;
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
    // faucet identity (Cantina #6), in which case the row is already written.
    if recovered.get().is_none() {
        // On repair (`replace`), swap the row for this origin — the freshly-deployed
        // faucet has a new faucet_id, so a plain upsert-by-faucet_id would collide
        // with the (origin_address, origin_network) unique index.
        let entry = FaucetEntry {
            faucet_id,
            origin_address,
            origin_network: params.origin_network,
            symbol: params.symbol,
            origin_decimals: params.origin_decimals,
            miden_decimals: params.miden_decimals,
            scale,
            metadata: metadata_bytes,
        };
        if params.replace {
            state.store.replace_faucet(entry).await?;
        } else {
            state.store.register_faucet(entry).await?;
        }
    }

    let id_hex = faucet_id.to_hex();
    tracing::info!(
        faucet_id = %id_hex,
        "admin_registerFaucet: faucet created and registered"
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

    fn repair_params(replace: bool) -> RegisterFaucetParams {
        RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27,
            // Fixed derivation for d=27 → scale 18 (valid route).
            miden_decimals: 9,
            name: None,
            replace,
        }
    }

    /// Finding #17 — WITHOUT `replace`, an existing route short-circuits:
    /// `admin_registerFaucet` returns the stale faucet_id and never touches Miden.
    /// This is the pre-fix "cannot be repaired" behaviour, retained as the default
    /// idempotent path.
    #[tokio::test]
    async fn existing_route_is_idempotent_without_replace() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        let id = admin_register_faucet(service.clone(), repair_params(false))
            .await
            .unwrap();
        assert_eq!(id, AccountId::from_hex(POISON_FAUCET_HEX).unwrap().to_hex());
        // No faucet deploy attempted — the stale route was returned as-is.
        assert_eq!(service.miden_client.test_call_count(), 0);
    }

    /// Finding #17 (fixed) — WITH `replace = true`, the poisoned route NO LONGER
    /// short-circuits: the repair path reaches the Miden deploy step, proving the
    /// route can now be repaired. (The in-test Miden stub does not run the deploy
    /// closure, so the call itself does not complete a real redeploy; the
    /// observable is that the deploy path is now reachable — the pre-fix code
    /// returned the stale id here with zero Miden calls.)
    #[tokio::test]
    async fn poisoned_route_can_be_repaired_with_replace() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        let _ = admin_register_faucet(service.clone(), repair_params(true)).await;
        assert!(
            service.miden_client.test_call_count() >= 1,
            "replace=true must reach the Miden deploy path (pre-fix: 0 calls)"
        );
    }

    /// Finding #17 — an unsatisfiable route is rejected up-front and never
    /// persisted, whether or not `replace` is set. Here `scale = 27 - 7 = 20 >
    /// MAX_SCALING_FACTOR`.
    #[tokio::test]
    async fn rejects_route_exceeding_scaling_factor() {
        let service = create_test_service();
        let params = RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27,
            miden_decimals: 7, // scale 20 > 18
            name: None,
            replace: false,
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
