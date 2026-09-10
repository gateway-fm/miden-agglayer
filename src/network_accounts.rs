//! P2ID-fundable network accounts (#201).
//!
//! On a fee-charging chain every account pays its own transaction fees from
//! its vault — including the bridge and the faucets, which are network
//! accounts (`AuthNetworkAccount`, executed by the ntx-builder). A brand-new
//! account can only fill its vault by CONSUMING a note in its first
//! transaction, and a network account only accepts allowlisted note scripts.
//! The upstream `AggLayerBridge` / `AggLayerFaucet` builders allowlist just
//! their own feature notes — every one of which carries a `NetworkAccountTarget`
//! and therefore cannot be created until the target is on-chain (the sender
//! FPIs into it) — and they carry no `receive_asset`, so a plain P2ID aborts
//! in the kernel. That left no way to bring such an account to life after
//! genesis (proven live: `procedure root … not in the account procedure index
//! map`, then `before_foreign_load … account not found at block N`).
//!
//! Miden's own post-genesis network account (the testnet network-monitor's
//! counter) shows the intended recipe: build the network account WITH a
//! [`BasicWallet`] component and allowlist the P2ID script at zero fee. A P2ID
//! carries no attachment, so anyone can create one for a not-yet-created
//! target, and the account's creation transaction consumes it — funding the
//! vault, paying the kernel fee from it, and deploying the account in one go.
//! Afterwards the zero per-note policy means ordinary notes (CLAIM, B2AGG,
//! UpdateGer, MINT) need no sponsorship: the vault pays, and top-ups are just
//! more P2IDs from `service`.
//!
//! Safety: adding `BasicWallet` also adds its send procedures, but a network
//! account only runs allowlisted note scripts and the default tx-script
//! allowlist holds nothing but the network builder's expiration script — so
//! the only path into those procedures is a note script, and the P2ID script
//! calls `receive_asset` alone. The agglayer note scripts are unchanged.
//!
//! These builders mirror the upstream ones component-for-component; the only
//! differences are the extended allowlist (+P2ID, scheduled at zero fee) and
//! the trailing `BasicWallet`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::anyhow;
use miden_base_agglayer::{AggLayerBridge, AggLayerFaucet, BridgeRoles};
use miden_client::Felt;
use miden_client::note::NoteScriptRoot;
use miden_protocol::account::{AccountBuilder, AccountId};
use miden_protocol::asset::TokenSymbol;
use miden_standards::account::access::{
    Authority, Ownable2Step, Pausable, PausableManager, RoleBasedAccessControl, RoleConfig,
};
use miden_standards::account::auth::NetworkAccount;
use miden_standards::account::fees::ConstantFeeManager;
use miden_standards::account::policies::{
    BurnPolicy, MintPolicy, TokenPolicyManager, TransferPolicy,
};
use miden_standards::account::wallets::BasicWallet;
use miden_standards::note::P2idNote;

use crate::fee_policy::zero_fee_policy_manager_for;

/// The upstream allowlist plus the P2ID script, so the account can be funded
/// (and later topped up) with a plain P2ID of the fee asset.
pub fn p2id_fundable(allowed: BTreeSet<NoteScriptRoot>) -> BTreeSet<NoteScriptRoot> {
    let mut allowed = allowed;
    allowed.insert(P2idNote::script_root());
    allowed
}

/// `AggLayerBridge::account_builder`, P2ID-fundable. Component-for-component
/// the upstream builder, with the extended allowlist and a trailing
/// `BasicWallet`.
pub fn bridge_account_builder(
    seed: impl Into<[u8; 32]>,
    bridge_admin: AccountId,
    roles: BridgeRoles,
    network_id: u32,
    fee_faucet_id: AccountId,
) -> anyhow::Result<AccountBuilder> {
    let allowed = p2id_fundable(AggLayerBridge::allowed_notes());
    let fee_policy_manager = zero_fee_policy_manager_for(allowed.clone(), fee_faucet_id);
    let rbac = RoleBasedAccessControl::builder()
        .role(RoleConfig::new(RoleBasedAccessControl::admin_role()).with_member(bridge_admin))
        .roles(roles)
        .build()
        .map_err(|e| anyhow!("bridge RBAC roles: {e:?}"))?;
    let builder = NetworkAccount::builder(seed.into(), allowed, fee_policy_manager)
        .map_err(|e| anyhow!("bridge note allowlist: {e:?}"))?
        .with_component(AggLayerBridge::new(network_id))
        .with_component(rbac)
        .with_component(Authority::RbacControlled {
            procedure_roles: AggLayerBridge::procedure_roles(),
        })
        .with_component(Pausable::unpaused())
        .with_component(PausableManager)
        .with_component(ConstantFeeManager::for_basic_constant_fee_policy())
        .with_component(BasicWallet);
    Ok(builder)
}

/// `AggLayerFaucet::account_builder`, P2ID-fundable. Component-for-component
/// the upstream builder, with the extended allowlist and a trailing
/// `BasicWallet`.
#[allow(clippy::too_many_arguments)]
pub fn faucet_account_builder(
    seed: impl Into<[u8; 32]>,
    token_symbol: &str,
    decimals: u8,
    max_supply: Felt,
    initial_supply: Felt,
    faucet_admin: AccountId,
    bridge_account_id: AccountId,
    fee_faucet_id: AccountId,
) -> anyhow::Result<AccountBuilder> {
    let allowed = p2id_fundable(AggLayerFaucet::allowed_notes());
    let fee_policy_manager = zero_fee_policy_manager_for(allowed.clone(), fee_faucet_id);
    let symbol = TokenSymbol::new(token_symbol)
        .map_err(|e| anyhow!("faucet token symbol {token_symbol:?}: {e:?}"))?;
    let faucet = AggLayerFaucet::new(symbol, decimals, max_supply, initial_supply)
        .map_err(|e| anyhow!("faucet metadata for {token_symbol}: {e:?}"))?;
    let rbac = RoleBasedAccessControl::with_admins([faucet_admin])
        .map_err(|e| anyhow!("faucet RBAC admin: {e:?}"))?;
    let token_policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::owner_only())
        .active_burn_policy(BurnPolicy::owner_only())
        .active_send_policy(TransferPolicy::allow_all())
        .active_receive_policy(TransferPolicy::allow_all())
        .build();
    let builder = NetworkAccount::builder(seed.into(), allowed, fee_policy_manager)
        .map_err(|e| anyhow!("faucet note allowlist: {e:?}"))?
        .with_component(faucet)
        .with_component(Ownable2Step::new(bridge_account_id))
        .with_component(rbac)
        .with_component(Authority::RbacControlled {
            procedure_roles: BTreeMap::new(),
        })
        .with_components(token_policy_manager)
        .with_component(ConstantFeeManager::for_basic_constant_fee_policy())
        .with_component(BasicWallet);
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2id_is_added_to_the_allowlists() {
        let bridge = p2id_fundable(AggLayerBridge::allowed_notes());
        assert!(bridge.contains(&P2idNote::script_root()));
        assert!(bridge.is_superset(&AggLayerBridge::allowed_notes()));
        let faucet = p2id_fundable(AggLayerFaucet::allowed_notes());
        assert!(faucet.contains(&P2idNote::script_root()));
        assert!(faucet.is_superset(&AggLayerFaucet::allowed_notes()));
    }
}
