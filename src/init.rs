use crate::accounts_config;
use crate::accounts_config::{AccountIdBech32, AccountsConfig};
use crate::miden_client::MidenClient;
use crate::miden_client::MidenClientLib;
use miden_base_agglayer::{create_agglayer_faucet_builder, create_bridge_account_builder};
use miden_client::Felt;
use miden_client::crypto::FeltRng;
use miden_client::keystore::FilesystemKeyStore;
use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::account::{Account, AccountId};
use miden_standards::account::auth::AuthFalcon512Rpo;
use miden_standards::account::wallets::BasicWallet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[allow(dead_code)]
#[derive(Debug)]
struct Accounts {
    service: Account,
    bridge: Account,
    faucet_eth: Account,
    faucet_agg: Account,
    wallet_hardhat: Account,
    wallet_satoshi: Account,
}

impl From<Accounts> for AccountsConfig {
    fn from(accounts: Accounts) -> Self {
        Self {
            service: AccountIdBech32(accounts.service.id()),
            bridge: AccountIdBech32(accounts.bridge.id()),
            faucet_eth: AccountIdBech32(accounts.faucet_eth.id()),
            faucet_agg: AccountIdBech32(accounts.faucet_agg.id()),
            wallet_hardhat: AccountIdBech32(accounts.wallet_hardhat.id()),
            wallet_satoshi: AccountIdBech32(accounts.wallet_satoshi.id()),
        }
    }
}

fn add_auth_key(keystore: Arc<FilesystemKeyStore>) -> anyhow::Result<AuthFalcon512Rpo> {
    let key_pair = AuthSecretKey::new_falcon512_rpo();
    keystore.add_key(&key_pair)?;
    Ok(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
}

async fn add_bridge(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    let account = create_bridge_account_builder(client.rng().draw_word())
        .with_auth_component(add_auth_key(keystore)?)
        .build()?;
    client.add_account(&account, false).await?;
    Ok(account)
}

async fn add_faucet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
    token_symbol: &str,
    decimals: u8,
    bridge_account_id: AccountId,
) -> anyhow::Result<Account> {
    let max_supply = Felt::new(1000000);
    let builder = create_agglayer_faucet_builder(
        client.rng().draw_word(),
        token_symbol,
        decimals,
        max_supply,
        bridge_account_id,
    );
    let account = builder.with_auth_component(add_auth_key(keystore)?).build()?;
    client.add_account(&account, false).await?;
    Ok(account)
}

async fn add_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    let account = Account::builder(client.rng().draw_word().into())
        .with_component(BasicWallet)
        .with_auth_component(add_auth_key(keystore)?)
        .build()?;
    client.add_account(&account, false).await?;
    Ok(account)
}

async fn add_accounts(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Accounts> {
    let service = add_wallet(client, keystore.clone()).await?;
    let bridge = add_bridge(client, keystore.clone()).await?;
    // TODO: fix decimals
    let faucet_eth = add_faucet(client, keystore.clone(), "ETH", 8u8, bridge.id()).await?;
    let faucet_agg = add_faucet(client, keystore.clone(), "AGG", 8u8, bridge.id()).await?;
    let wallet_hardhat = add_wallet(client, keystore.clone()).await?;
    let wallet_satoshi = add_wallet(client, keystore.clone()).await?;
    Ok(Accounts {
        service,
        bridge,
        faucet_eth,
        faucet_agg,
        wallet_hardhat,
        wallet_satoshi,
    })
}

async fn init_internal(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    client.sync_state().await?;
    let accounts = add_accounts(client, keystore).await?;
    let config = AccountsConfig::from(accounts);
    let config_path = accounts_config::save_config(config, miden_store_dir)?;
    Ok(config_path)
}

pub async fn init(
    client: &MidenClient,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let result = Arc::new(OnceLock::<PathBuf>::new());
    let result_internal = result.clone();
    let keystore = client.get_keystore();

    let future = client.with(|client| {
        Box::new(async move {
            let result = init_internal(client, keystore, miden_store_dir).await?;
            result_internal.set(result).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(result.get().unwrap().clone())
}
