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
use miden_base_agglayer::{
    AggLayerBridge, AggLayerFaucet, AgglayerBridgeError, AgglayerFaucetError, BridgeRoles, ExitRoot,
};
use miden_client::note::NoteScriptRoot;
use miden_protocol::account::{
    Account, AccountBuilder, AccountId, AssetCallbackFlag, StorageMapKey,
};
use miden_protocol::asset::TokenSymbol;
use miden_protocol::crypto::hash::poseidon2::Poseidon2;
use miden_protocol::{Felt, Word};
use miden_standards::account::access::{
    Authority, Ownable2Step, Pausable, PausableManager, RoleBasedAccessControl, RoleConfig,
};
use miden_standards::account::auth::NetworkAccount;
use miden_standards::account::faucets::FungibleFaucet;
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
    // A faucet that configures a transfer policy MUST carry AssetCallbackFlag::
    // Enabled (miden-protocol AccountBuilder doc; the flag is immutable, baked
    // into the id). The upstream AggLayerFaucet::account_builder omits it, so it
    // builds a Disabled faucet — which the proxy's own Cantina #4 detector
    // rejects (a legit wrapped-faucet MINT must have Enabled callbacks; live at
    // base fee 7, the deposit's MINT tripped "asset_callbacks: expected Enabled,
    // observed Disabled"). Derive it the way miden-standards' faucet helper does.
    let asset_callbacks = AssetCallbackFlag::from(token_policy_manager.has_transfer_policy());
    let builder = NetworkAccount::builder(seed.into(), allowed, fee_policy_manager)
        .map_err(|e| anyhow!("faucet note allowlist: {e:?}"))?
        .with_asset_callbacks(asset_callbacks)
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

// ── Readers that do not gate on the code commitment ────────────────────────────
//
// Upstream's `AggLayerBridge::is_ger_registered`, `AggLayerFaucet::
// try_faucet_from_account` and `AggLayerFaucet::owner_account_id` first assert
// `BRIDGE_CODE_COMMITMENT == account.code().commitment()` (resp. the faucet's).
// The P2ID-fundable variants carry one more code component, so that assertion
// fails ("the code commitment of the provided account does not match …") and
// the proxy could no longer read its own bridge's GER map — the live deposit
// stalled on exactly that. Storage is identical (`BasicWallet` has none), so
// these replicas keep upstream's STORAGE checks and formulas and drop only the
// code gate. They return upstream's error types so call sites are unchanged.

/// `AggLayerBridge::is_ger_registered` without the code-commitment gate.
pub fn is_ger_registered(
    ger: ExitRoot,
    bridge_account: &Account,
) -> Result<bool, AgglayerBridgeError> {
    // Same key derivation as upstream: poseidon2::merge(GER_LOWER, GER_UPPER).
    let elements = ger.to_elements();
    let ger_lower: Word = elements[0..4]
        .try_into()
        .expect("an exit root is eight field elements");
    let ger_upper: Word = elements[4..8]
        .try_into()
        .expect("an exit root is eight field elements");
    let ger_hash = Poseidon2::merge(&[ger_lower, ger_upper]);
    let stored = bridge_account
        .storage()
        .get_map_item(
            AggLayerBridge::ger_map_slot_name(),
            StorageMapKey::from_raw(ger_hash),
        )
        .map_err(|_| AgglayerBridgeError::StorageSlotsMismatch)?;
    let registered: Word = [Felt::ONE, Felt::ZERO, Felt::ZERO, Felt::ZERO].into();
    Ok(stored == registered)
}

/// `AggLayerFaucet::owner_account_id` without the code-commitment gate: the
/// AggLayer-owned faucet's `Ownable2Step` slot IS the discriminator — a
/// Miden-native operator faucet has none and fails here, as upstream's does.
pub fn owner_account_id(faucet_account: &Account) -> Result<AccountId, AgglayerFaucetError> {
    let ownership = Ownable2Step::try_from_storage(faucet_account.storage())
        .map_err(AgglayerFaucetError::Ownable2StepError)?;
    ownership
        .owner()
        .ok_or(AgglayerFaucetError::OwnershipRenounced)
}

/// `AggLayerFaucet::try_faucet_from_account` without the code-commitment gate:
/// requires the AggLayer ownership slot (see [`owner_account_id`]) and decodes
/// the standard fungible-faucet metadata.
pub fn try_faucet_from_account(
    faucet_account: &Account,
) -> Result<FungibleFaucet, AgglayerFaucetError> {
    Ownable2Step::try_from_storage(faucet_account.storage())
        .map_err(|_| AgglayerFaucetError::StorageSlotsMismatch)?;
    FungibleFaucet::try_from(faucet_account.storage())
        .map_err(AgglayerFaucetError::FungibleFaucetError)
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

    #[test]
    fn faucet_is_asset_callback_enabled() {
        use miden_protocol::account::{AccountId, AssetCallbackFlag};
        use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE as X;
        let id = AccountId::try_from(X).unwrap();
        let faucet = faucet_account_builder(
            [7u8; 32],
            "ETH",
            8,
            Felt::new(1_000_000).unwrap(),
            Felt::new(0).unwrap(),
            id,
            id,
            id,
        )
        .unwrap()
        .build()
        .unwrap();
        // The wrapped faucet configures a transfer policy, so its immutable
        // AssetCallbackFlag must be Enabled — the invariant the proxy's Cantina
        // #4 MINT detector enforces (upstream's builder leaves it Disabled).
        assert_eq!(
            faucet.id().asset_callback_flag(),
            AssetCallbackFlag::Enabled
        );
    }
}
