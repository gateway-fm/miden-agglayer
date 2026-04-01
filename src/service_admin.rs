//! Admin RPC endpoint for explicit faucet registration.
//!
//! `admin_registerFaucet` creates a faucet on Miden, registers it in the bridge,
//! and saves its metadata to the Store. This is an alternative to auto-creation
//! during the first claim — useful for pre-staging tokens.

use crate::faucet_ops;
use crate::service_state::ServiceState;
use crate::store::FaucetEntry;
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

    // Create, deploy, register in bridge (using OnceLock pattern like publish_claim)
    let result = Arc::new(OnceLock::<AccountId>::new());
    let result_inner = result.clone();
    let symbol_clone = params.symbol.clone();

    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                let account = faucet_ops::create_and_register_faucet(
                    client,
                    &symbol_clone,
                    params.miden_decimals,
                    &origin_address,
                    params.origin_network,
                    scale,
                    service_id,
                    bridge_id,
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

    // Save to store
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
        })
        .await?;

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
