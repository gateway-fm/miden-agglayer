//! Zero-fee `FeePolicyManager` construction for the proxy's on-chain accounts.
//!
//! Protocol 0.16.0-rc introduces transaction fees: every `NetworkAccount`
//! (bridge, faucets) must be built with a [`FeePolicyManager`] naming the
//! chain's fee faucet and an active fee policy. This deployment charges ZERO
//! fees for every note script the account accepts — mirroring the upstream
//! testing `zero_fee_policy_manager` recipe, but against the REAL fee faucet
//! id read from the chain's block-header fee parameters (never a mock
//! constant: a wrong fee asset id would make every fee check fail on-chain).

use std::collections::BTreeSet;

use miden_client::note::NoteScriptRoot;
use miden_protocol::account::AccountId;
use miden_protocol::asset::AssetAmount;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_standards::account::fees::{BasicConstantFeePolicy, FeePolicyManager};

use crate::miden_client::MidenClientLib;

/// A `FeePolicyManager` charging a ZERO fee for each of `allowed` note
/// scripts, denominated in the chain fee faucet's asset.
pub fn zero_fee_policy_manager_for(
    allowed: BTreeSet<NoteScriptRoot>,
    fee_faucet_id: AccountId,
) -> FeePolicyManager {
    let mut policy = BasicConstantFeePolicy::new();
    for note_script in allowed {
        policy = policy.with_fee(note_script, AssetAmount::ZERO);
    }
    FeePolicyManager::builder()
        .fee_faucet_id(fee_faucet_id)
        .active_fee_policy(policy.into())
        .build()
}

/// The fee faucet a block header commits to. Since protocol 0.17 the fee asset
/// lives in the `ProtocolConfig` (header carries only its commitment); the
/// client stores the config by commitment when it syncs past it.
pub async fn fee_faucet_id_for_header(
    client: &MidenClientLib,
    header: &BlockHeader,
) -> anyhow::Result<AccountId> {
    let config = client
        .get_protocol_config(header.protocol_config_commitment())
        .await?;
    Ok(config.fee_asset_id().faucet_id())
}

/// The chain's fee faucet id (constant for the chain's lifetime — the genesis
/// header is authoritative; the current sync-height header is the fallback
/// when genesis is not stored locally).
pub async fn fee_faucet_id_from_chain(client: &MidenClientLib) -> anyhow::Result<AccountId> {
    for block in [BlockNumber::GENESIS, client.get_sync_height().await?] {
        if let Some((header, _)) = client.get_block_header_by_num(block).await? {
            return fee_faucet_id_for_header(client, &header).await;
        }
    }
    anyhow::bail!(
        "no block header available locally (genesis or sync height) — cannot determine the \
         chain fee faucet id; sync the client before creating accounts"
    )
}
