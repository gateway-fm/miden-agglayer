//! Genesis input accounts for the e2e chain (node v0.17).
//!
//! Since node v0.17 `miden-validator genesis` no longer invents the native fee
//! faucet and its pre-funded operator: it takes them as ACCOUNT FILES
//! (`--native-faucet`, `--funding-account`) and imports them as-is. These are
//! the two accounts the 0.16 genesis used to generate, rebuilt with the recipe
//! upstream's own harness uses (`bin/large-account-benchmark/src/accounts.rs`):
//!
//! - `native_faucet.mac` — a public, keyed [`FungibleFaucet`] whose recorded
//!   token supply equals what the operator holds (genesis adds wallet
//!   allocations on top, never subtracts).
//! - `faucet_operator.mac` — a public [`BasicWallet`] holding that whole supply,
//!   with its signing key. The name is kept from 0.16 so the fee-funder sidecar
//!   (`bridge-out-tool --fund-fee-asset --faucet-operator-mac`) is unchanged.
//!
//! Genesis "deploys" accounts without a transaction, so both carry nonce 1 —
//! genesis rejects a nonce-0 import as undeployed.
//!
//! The fee faucet carries allow-all send/receive policies behind the V2
//! ASSET CALLBACKS (`TokenPolicyManagerV2`, miden-standards 0.17.1), like the
//! live testnet fee faucet: every fee asset an account receives then runs the
//! faucet's `invoke_receive_policy_v2`. Without callbacks the e2e never
//! executes that path — and a proxy built on miden-standards 0.17.0 (no V2
//! procedures) fails it with "procedure with root digest 0xefc0d6dc…ace2 could
//! not be found" (testnet bring-up, proxy v0.17.1 on node v0.17.1).

use std::path::Path;

use anyhow::{Context, anyhow};
use miden_client::account::AccountFile;
use miden_protocol::ONE;
use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
use miden_protocol::account::{Account, AccountBuilder, AccountType};
use miden_protocol::asset::{Asset, AssetAmount, AssetVault, FungibleAsset, TokenSymbol};
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::{FungibleFaucet, TokenName};
use miden_standards::account::policies::{
    BurnPolicy, MintPolicy, TokenPolicyManager, TokenPolicyManagerV2, TransferPolicy,
};
use miden_standards::account::wallets::create_basic_wallet;

pub const NATIVE_FAUCET_FILE: &str = "native_faucet.mac";
pub const FAUCET_OPERATOR_FILE: &str = "faucet_operator.mac";

const SYMBOL: &str = "MIDEN";
const DECIMALS: u8 = 6;
/// What the operator holds: ample for every e2e funding cascade (a 4096-txn
/// budget at base fee 7 is under a million units).
pub const OPERATOR_SUPPLY: u64 = 1_000_000_000_000_000;

/// The two genesis input accounts with their signing keys.
pub struct GenesisAccounts {
    pub native_faucet: AccountFile,
    pub faucet_operator: AccountFile,
}

fn fresh_key() -> (AuthSecretKey, Approver, [u8; 32]) {
    let key = AuthSecretKey::new_falcon512_poseidon2();
    let commitment = key.public_key().to_commitment();
    // The fresh key's commitment doubles as the account seed: random per run,
    // no extra RNG dependency.
    let seed = miden_protocol::Word::from(commitment).as_bytes();
    (
        key,
        Approver::new(commitment, AuthScheme::Falcon512Poseidon2),
        seed,
    )
}

/// Builds the native fee faucet and the operator wallet that holds its supply.
pub fn build() -> anyhow::Result<GenesisAccounts> {
    let supply = AssetAmount::new(OPERATOR_SUPPLY)?;

    let (faucet_key, faucet_approver, faucet_seed) = fresh_key();
    let faucet = FungibleFaucet::builder()
        .name(TokenName::new(SYMBOL).map_err(|e| anyhow!("native faucet name: {e:?}"))?)
        .symbol(TokenSymbol::new(SYMBOL).map_err(|e| anyhow!("native faucet symbol: {e:?}"))?)
        .decimals(DECIMALS)
        .max_supply(supply)
        .token_supply(supply)
        .build()
        .map_err(|e| anyhow!("native faucet component: {e:?}"))?;
    let mut faucet_account = AccountBuilder::new(faucet_seed)
        .account_type(AccountType::Public)
        .with_component(AuthSingleSig::new(faucet_approver))
        .with_component(faucet)
        .with_components(TokenPolicyManagerV2::new(
            TokenPolicyManager::builder()
                .active_mint_policy(MintPolicy::allow_all())
                .active_burn_policy(BurnPolicy::allow_all())
                .active_send_policy(TransferPolicy::allow_all())
                .active_receive_policy(TransferPolicy::allow_all())
                .build(),
        ))
        .build()
        .context("native faucet account")?;
    faucet_account.set_nonce(ONE)?;

    let (operator_key, operator_approver, operator_seed) = fresh_key();
    let mut wallet = create_basic_wallet(operator_seed, operator_approver, AccountType::Public)
        .context("faucet operator wallet")?;
    // Nonce 1 first: that also drops the creation seed, which a deployed
    // (nonce > 0) account must not carry.
    wallet.set_nonce(ONE)?;
    // Pre-fund by rebuilding with a vault that already holds the supply —
    // `Account::vault_mut` is test-only, `Account::new` validates the result.
    let (id, _empty_vault, storage, code, nonce, seed) = wallet.into_parts();
    let supply_asset = Asset::from(FungibleAsset::new(faucet_account.id(), OPERATOR_SUPPLY)?);
    let vault = AssetVault::new(&[supply_asset]).context("operator vault")?;
    let operator = Account::new(id, vault, storage, code, nonce, seed)
        .context("pre-funded faucet operator")?;

    Ok(GenesisAccounts {
        native_faucet: AccountFile::new(faucet_account, vec![faucet_key]),
        faucet_operator: AccountFile::new(operator, vec![operator_key]),
    })
}

/// Writes both account files into `dir` (refusing to overwrite either).
pub fn write(dir: &Path) -> anyhow::Result<(Account, Account)> {
    let accounts = build()?;
    for (name, file) in [
        (NATIVE_FAUCET_FILE, &accounts.native_faucet),
        (FAUCET_OPERATOR_FILE, &accounts.faucet_operator),
    ] {
        let path = dir.join(name);
        anyhow::ensure!(
            !path.exists(),
            "{} already exists — refusing to overwrite",
            path.display()
        );
        file.write(&path)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok((
        accounts.native_faucet.account().clone(),
        accounts.faucet_operator.account().clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the invariants `miden-validator genesis` (node v0.17,
    /// `crates/store/src/genesis/config::into_state`) enforces on the imports,
    /// so a protocol bump that breaks one fails here, not at stack bring-up.
    #[test]
    fn genesis_inputs_satisfy_the_v0_17_genesis_contract() {
        let accounts = build().unwrap();
        let faucet = accounts.native_faucet.account();
        let operator = accounts.faucet_operator.account();

        // NativeFaucetNotFungible
        let decoded = FungibleFaucet::try_from(faucet).expect("native faucet must be fungible");
        // FundingAccountNotPublic
        assert_eq!(operator.id().account_type(), AccountType::Public);
        // UndeployedAccount
        assert_eq!(faucet.nonce(), ONE);
        assert_eq!(operator.nonce(), ONE);
        // Recorded supply == what the operator actually holds.
        assert_eq!(decoded.token_supply().as_u64(), OPERATOR_SUPPLY);
        let held: u64 = operator
            .vault()
            .assets()
            .filter_map(|a| a.as_fungible())
            .filter(|f| f.faucet_id() == faucet.id())
            .map(|f| f.amount().as_u64())
            .sum();
        assert_eq!(held, OPERATOR_SUPPLY);
        // Both carry a signing key (the operator's is what the fee-funder spends with).
        assert_eq!(accounts.native_faucet.auth_secret_keys().len(), 1);
        assert_eq!(accounts.faucet_operator.auth_secret_keys().len(), 1);
    }

    /// The fee faucet must wire the V2 transfer callbacks, as the testnet fee
    /// faucet does — the receive root is pinned to the exact digest the
    /// testnet bring-up failed on, so the e2e keeps exercising that path.
    #[test]
    fn native_faucet_wires_the_v2_asset_callbacks_like_testnet() {
        use miden_protocol::asset::AssetCallbacks;
        let accounts = build().unwrap();
        let storage = accounts.native_faucet.account().storage();
        let on_receive = storage
            .get_item(AssetCallbacks::on_before_asset_added_to_account_slot())
            .unwrap();
        let on_send = storage
            .get_item(AssetCallbacks::on_before_asset_added_to_note_slot())
            .unwrap();
        assert_eq!(
            on_receive,
            TokenPolicyManagerV2::invoke_receive_policy_root().as_word()
        );
        assert_eq!(
            on_receive.to_hex(),
            "0xefc0d6dc729c05e93107be949d6cca2e20daf02e20b923934c1c0420a466ace2"
        );
        assert_eq!(
            on_send,
            TokenPolicyManagerV2::invoke_send_policy_root().as_word()
        );
    }

    #[test]
    fn written_files_round_trip_and_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let (faucet, operator) = write(dir.path()).unwrap();
        let back = AccountFile::read(dir.path().join(FAUCET_OPERATOR_FILE)).unwrap();
        assert_eq!(back.account().id(), operator.id());
        let back = AccountFile::read(dir.path().join(NATIVE_FAUCET_FILE)).unwrap();
        assert_eq!(back.account().id(), faucet.id());
        assert!(
            write(dir.path()).is_err(),
            "a second write must refuse to overwrite"
        );
    }
}
