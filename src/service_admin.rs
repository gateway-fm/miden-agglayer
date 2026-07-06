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

    let origin_address = parse_eth_address(&params.origin_token_address)?;

    // Check if already registered
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
        state
            .store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address,
                origin_network: params.origin_network,
                symbol: params.symbol,
                origin_decimals: params.origin_decimals,
                miden_decimals: params.miden_decimals,
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
