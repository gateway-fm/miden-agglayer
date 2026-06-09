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
}

/// Does an existing route already satisfy the caller's request AND the protocol
/// decimal bounds? Only then is the admin call a no-op (idempotent re-register).
///
/// Cantina MA#17 — a route is "poisoned" (and therefore repairable, NOT a no-op)
/// when its persisted `scale` exceeds `MAX_SCALING_FACTOR` (18) or its
/// `miden_decimals` exceeds `MIDEN_FAUCET_MAX_DECIMALS` (12) — such routes are
/// unclaimable. We also treat a route whose `miden_decimals` differs from the
/// caller's request as repairable, so an operator can re-key a high-decimal token
/// onto a claimable local decimal count.
fn existing_route_is_healthy(existing: &FaucetEntry, requested_miden_decimals: u8) -> bool {
    existing.scale <= faucet_ops::MAX_SCALING_FACTOR
        && existing.miden_decimals <= faucet_ops::MIDEN_FAUCET_MAX_DECIMALS
        && existing.miden_decimals == requested_miden_decimals
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

    // Cantina MA#17 — reject a request that would itself persist an UNCLAIMABLE
    // route. The bridge's asset-conversion path caps the downscale at
    // MAX_SCALING_FACTOR (18) and the local faucet at MIDEN_FAUCET_MAX_DECIMALS
    // (12); registering outside either bound just re-poisons the registry.
    if scale > faucet_ops::MAX_SCALING_FACTOR {
        anyhow::bail!(
            "refusing to register unclaimable route: scale {scale} (= origin_decimals {} - \
             miden_decimals {}) exceeds MAX_SCALING_FACTOR ({}). Choose miden_decimals >= {}.",
            params.origin_decimals,
            params.miden_decimals,
            faucet_ops::MAX_SCALING_FACTOR,
            params
                .origin_decimals
                .saturating_sub(faucet_ops::MAX_SCALING_FACTOR),
        );
    }
    if params.miden_decimals > faucet_ops::MIDEN_FAUCET_MAX_DECIMALS {
        anyhow::bail!(
            "refusing to register unclaimable route: miden_decimals {} exceeds the local faucet \
             maximum MIDEN_FAUCET_MAX_DECIMALS ({})",
            params.miden_decimals,
            faucet_ops::MIDEN_FAUCET_MAX_DECIMALS,
        );
    }

    let origin_address = parse_eth_address(&params.origin_token_address)?;

    // Check if already registered. A HEALTHY existing route (claimable and
    // matching the request) is an idempotent no-op. A POISONED route — Cantina
    // MA#17 — must be REPAIRED: we create a fresh faucet below and atomically
    // REPLACE the route via `replace_faucet_by_origin`, so an operator can fix a
    // 27..=30 decimal token that auto-create persisted with scale > 18.
    let existing = state
        .store
        .get_faucet_by_origin(&origin_address, params.origin_network)
        .await?;
    if let Some(existing) = &existing {
        if existing_route_is_healthy(existing, params.miden_decimals) {
            let id = existing.faucet_id.to_hex();
            tracing::info!(
                faucet_id = %id,
                "admin_registerFaucet: healthy faucet already exists for this origin"
            );
            return Ok(id);
        }
        tracing::warn!(
            faucet_id = %existing.faucet_id.to_hex(),
            existing_miden_decimals = existing.miden_decimals,
            existing_scale = existing.scale,
            "admin_registerFaucet: existing route is poisoned/mismatched — repairing (Cantina MA#17)"
        );
    }

    let accounts = &state.accounts.0;
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;

    // Compute MetadataHash from (name, symbol, origin_decimals) — matches the L1 bridge
    // contract's `keccak256(abi.encode(name, symbol, decimals))`. Callers that skip the
    // `name` field get `name = symbol`.
    let metadata_name = params.name.clone().unwrap_or_else(|| params.symbol.clone());
    let metadata_hash =
        MetadataHash::from_token_info(&metadata_name, &params.symbol, params.origin_decimals);

    // Create, deploy, register in bridge (using OnceLock pattern like publish_claim)
    let result = Arc::new(OnceLock::<AccountId>::new());
    let result_inner = result.clone();
    let symbol_clone = params.symbol.clone();
    let miden_decimals = params.miden_decimals;
    let origin_network = params.origin_network;

    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
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

    let entry = FaucetEntry {
        faucet_id,
        origin_address,
        origin_network: params.origin_network,
        symbol: params.symbol,
        origin_decimals: params.origin_decimals,
        miden_decimals: params.miden_decimals,
        scale,
    };

    // Persist. If a (poisoned/mismatched) route already existed for this origin
    // key we REPLACE it atomically — the registry's UNIQUE(origin_address,
    // origin_network) index would otherwise reject inserting the new faucet_id
    // over the old one (Cantina MA#17). Otherwise this is a fresh registration.
    if existing.is_some() {
        state.store.replace_faucet_by_origin(entry).await?;
    } else {
        state.store.register_faucet(entry).await?;
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

    fn poisoned_entry(
        faucet_id: AccountId,
        origin_address: [u8; 20],
        origin_network: u32,
    ) -> FaucetEntry {
        // 27-decimal token auto-created with the legacy fixed 8 local decimals →
        // scale 19 > MAX_SCALING_FACTOR (18): UNCLAIMABLE.
        FaucetEntry {
            faucet_id,
            origin_address,
            origin_network,
            symbol: "POC".into(),
            origin_decimals: 27,
            miden_decimals: 8,
            scale: 19,
        }
    }

    // Cantina MA#17 — the poisoned route MUST be classified as unhealthy so the
    // admin endpoint takes the repair branch instead of the idempotent early-return.
    #[test]
    fn poisoned_route_is_not_healthy_but_a_valid_route_is() {
        let id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        let poisoned = poisoned_entry(id, [0x42u8; 20], 7);
        // Caller requests a claimable 9-decimal route.
        assert!(
            !existing_route_is_healthy(&poisoned, 9),
            "scale 19 > MAX_SCALING_FACTOR: route is poisoned and must be repairable",
        );

        let healthy = FaucetEntry {
            miden_decimals: 9,
            scale: 18,
            ..poisoned_entry(id, [0x42u8; 20], 7)
        };
        assert!(
            existing_route_is_healthy(&healthy, 9),
            "scale 18 with matching miden_decimals is healthy — idempotent no-op",
        );
        // A healthy-but-mismatched local-decimal request is still treated as repairable.
        assert!(
            !existing_route_is_healthy(&healthy, 10),
            "request for different local decimals should re-key the route",
        );
    }

    // Cantina MA#17 — adapted from the auditor PoC. The PoC asserted the BUGGY
    // behaviour (route NOT repaired, same faucet_id, scale stays 19). The production
    // assertion below proves the route IS repaired: `replace_faucet_by_origin` (the
    // primitive the admin repair branch invokes once a fresh faucet is deployed)
    // overwrites the poisoned entry under the UNIQUE(origin_address, origin_network)
    // index with a NEW faucet_id and a CLAIMABLE scale, leaving exactly one row.
    #[tokio::test]
    async fn poisoned_high_decimal_route_is_repaired_via_origin_replace() -> anyhow::Result<()> {
        let service = create_test_service();
        let origin_network = 7u32;
        let origin_address = [0x42u8; 20];
        let poisoned_faucet_id =
            AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").expect("valid account id");
        // A DISTINCT, valid id standing in for the freshly-deployed repair faucet.
        let repaired_faucet_id =
            AccountId::from_hex("0x3d7c9747558851900f8206226dfbeb").expect("valid account id");

        service
            .store
            .register_faucet(poisoned_entry(
                poisoned_faucet_id,
                origin_address,
                origin_network,
            ))
            .await?;

        // Sanity: the poisoned route exists and is unclaimable.
        let before = service
            .store
            .get_faucet_by_origin(&origin_address, origin_network)
            .await?
            .expect("poisoned route should exist");
        assert_eq!(before.faucet_id, poisoned_faucet_id);
        assert_eq!(before.scale, 19);
        assert!(!existing_route_is_healthy(&before, 9));

        // Repair: derive a claimable local decimal count and REPLACE the route.
        let (miden_decimals, scale) = faucet_ops::derive_faucet_decimals(27)?;
        assert_eq!((miden_decimals, scale), (9, 18));
        service
            .store
            .replace_faucet_by_origin(FaucetEntry {
                faucet_id: repaired_faucet_id,
                origin_address,
                origin_network,
                symbol: "POC".into(),
                origin_decimals: 27,
                miden_decimals,
                scale,
            })
            .await?;

        // The route IS repaired: NEW faucet_id, claimable scale, exactly one row.
        let stored = service
            .store
            .get_faucet_by_origin(&origin_address, origin_network)
            .await?
            .expect("repaired route should exist");
        assert_eq!(
            stored.faucet_id, repaired_faucet_id,
            "origin route must now point at the repaired faucet, not the poisoned one",
        );
        assert_eq!(stored.miden_decimals, 9);
        assert_eq!(
            stored.scale, 18,
            "repaired scale must be within MAX_SCALING_FACTOR"
        );

        let all_for_origin = service
            .store
            .find_faucets_by_origin_address(&origin_address)
            .await?;
        assert_eq!(
            all_for_origin.len(),
            1,
            "replace must not leave the poisoned row behind alongside the repair",
        );

        Ok(())
    }

    // Cantina MA#17 — the admin endpoint must REFUSE to persist an unclaimable
    // route in the first place (scale > MAX_SCALING_FACTOR), bailing BEFORE any
    // Miden client call.
    #[tokio::test]
    async fn admin_register_faucet_rejects_unclaimable_scale() -> anyhow::Result<()> {
        let service = create_test_service();
        let err = admin_register_faucet(
            service.clone(),
            RegisterFaucetParams {
                symbol: "POC".into(),
                origin_token_address: format!("0x{}", hex::encode([0x42u8; 20])),
                origin_network: 7,
                origin_decimals: 27,
                miden_decimals: 8, // scale 19 > 18 — unclaimable
                name: Some("Bad Route".into()),
            },
        )
        .await
        .expect_err("registering scale 19 must be rejected");
        assert!(
            err.to_string().contains("MAX_SCALING_FACTOR"),
            "error should explain the scale bound, got: {err}",
        );
        assert_eq!(
            service.miden_client.test_call_count(),
            0,
            "rejection must happen before any Miden client call",
        );
        Ok(())
    }
}
