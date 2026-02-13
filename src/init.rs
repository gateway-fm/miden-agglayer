use crate::accounts_config;
use crate::accounts_config::{AccountIdBech32, AccountsConfig};
use crate::miden_client::MidenClient;
use crate::miden_client::MidenClientLib;
use miden_base_agglayer::{create_agglayer_faucet_builder, create_bridge_account_builder};
use miden_client::Felt;
use miden_client::crypto::FeltRng;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::account::{Account, AccountComponent, AccountId, AccountStorageMode};
use miden_protocol::note::NoteType;
use miden_protocol::transaction::OutputNote;
use miden_standards::account::auth::AuthFalcon512Rpo;
use miden_standards::account::auth::NoAuth;
use miden_standards::account::wallets::BasicWallet;
use miden_standards::note::create_p2id_note;
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
}

impl From<Accounts> for AccountsConfig {
    fn from(accounts: Accounts) -> Self {
        Self {
            service: AccountIdBech32(accounts.service.id()),
            bridge: AccountIdBech32(accounts.bridge.id()),
            faucet_eth: AccountIdBech32(accounts.faucet_eth.id()),
            faucet_agg: AccountIdBech32(accounts.faucet_agg.id()),
            wallet_hardhat: AccountIdBech32(accounts.wallet_hardhat.id()),
        }
    }
}

fn add_auth_key(keystore: Arc<FilesystemKeyStore>) -> anyhow::Result<AuthFalcon512Rpo> {
    let key_pair = AuthSecretKey::new_falcon512_rpo();
    keystore.add_key(&key_pair)?;
    Ok(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
}

async fn deploy_account(
    client: &mut MidenClientLib,
    account_id: AccountId,
    name: &str,
) -> anyhow::Result<()> {
    tracing::info!("deploying {} account {} ...", name, AccountIdBech32(account_id));
    let dummy_txn = TransactionRequestBuilder::new().build()?;
    let txn_id = client.submit_new_transaction(account_id, dummy_txn).await?;
    tracing::info!("deployed {name} account with txn_id {txn_id}");
    Ok(())
}

async fn add_bridge(
    client: &mut MidenClientLib,
    _keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    let account = create_bridge_account_builder(client.rng().draw_word())
        .with_auth_component(AccountComponent::from(NoAuth))
        .build()?;
    client.add_account(&account, false).await?;

    deploy_account(client, account.id(), "bridge").await?;

    Ok(account)
}

async fn add_faucet(
    client: &mut MidenClientLib,
    _keystore: Arc<FilesystemKeyStore>,
    token_symbol: &str,
    decimals: u8,
    bridge_account_id: AccountId,
) -> anyhow::Result<Account> {
    let max_supply = Felt::try_from(0xffffffff00000000u64).unwrap();
    let builder = create_agglayer_faucet_builder(
        client.rng().draw_word(),
        token_symbol,
        decimals,
        max_supply,
        bridge_account_id,
    );
    let builder = builder.with_auth_component(AccountComponent::from(NoAuth));
    let builder = builder.storage_mode(AccountStorageMode::Public);
    let builder = builder.with_component(BasicWallet);
    let account = builder.build()?;
    client.add_account(&account, false).await?;

    deploy_account(client, account.id(), format!("{token_symbol} faucet").as_str()).await?;

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
    let faucet_eth = add_faucet(client, keystore.clone(), "ETH", 8u8, bridge.id()).await?;
    let faucet_agg = add_faucet(client, keystore.clone(), "AGG", 8u8, bridge.id()).await?;
    let wallet_hardhat = add_wallet(client, keystore.clone()).await?;
    Ok(Accounts {
        service,
        bridge,
        faucet_eth,
        faucet_agg,
        wallet_hardhat,
    })
}

async fn register_p2id_script(
    client: &mut MidenClientLib,
    sender: AccountId,
) -> anyhow::Result<()> {
    tracing::info!("registering P2ID script...");
    // dummy note to register its script on the node
    let note = create_p2id_note(
        sender,
        /* target = */ sender,
        /* assets = */ vec![],
        NoteType::Public,
        /* attachment = */ Default::default(),
        client.rng(),
    )?;

    let txn = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(note); 1])
        .build()?;

    let txn_id = client.submit_new_transaction(sender, txn).await?;
    tracing::info!("registered P2ID script with txn_id {txn_id}");
    Ok(())
}

async fn init_internal(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    client.sync_state().await?;
    let accounts = add_accounts(client, keystore).await?;
    register_p2id_script(client, accounts.service.id()).await?;
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
