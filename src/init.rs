use crate::accounts_config;
use crate::accounts_config::{AccountIdBech32, AccountsConfig};
use crate::faucet_ops;
use crate::miden_client::MidenClient;
use crate::miden_client::MidenClientLib;
use miden_base_agglayer::create_bridge_account;
use miden_client::crypto::FeltRng;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::NoteType;
use miden_protocol::transaction::OutputNote;
use miden_standards::account::auth::AuthSingleSig;
use miden_standards::account::wallets::BasicWallet;
use miden_standards::note::P2idNote;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[derive(Debug)]
struct Accounts {
    service: Account,
    bridge: Account,
    faucet_eth: Account,
    faucet_agg: Account,
    wallet_hardhat: Account,
    ger_manager: Account,
}

impl From<Accounts> for AccountsConfig {
    fn from(accounts: Accounts) -> Self {
        Self {
            service: AccountIdBech32(accounts.service.id()),
            bridge: AccountIdBech32(accounts.bridge.id()),
            faucet_eth: Some(AccountIdBech32(accounts.faucet_eth.id())),
            faucet_agg: Some(AccountIdBech32(accounts.faucet_agg.id())),
            wallet_hardhat: AccountIdBech32(accounts.wallet_hardhat.id()),
            ger_manager: Some(AccountIdBech32(accounts.ger_manager.id())),
        }
    }
}

fn create_auth_component() -> anyhow::Result<(AuthSingleSig, AuthSecretKey)> {
    let key_pair = AuthSecretKey::new_falcon512_rpo();
    let auth_component = AuthSingleSig::new(
        key_pair.public_key().to_commitment(),
        AuthScheme::Falcon512Rpo,
    );
    Ok((auth_component, key_pair))
}

async fn deploy_account(
    client: &mut MidenClientLib,
    account_id: AccountId,
    name: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "deploying {} account {} ...",
        name,
        AccountIdBech32(account_id)
    );
    let dummy_txn = TransactionRequestBuilder::new().build()?;
    let txn_id = client.submit_new_transaction(account_id, dummy_txn).await?;
    tracing::info!("deployed {name} account with txn_id {txn_id}");

    // Wait for the transaction to be committed (like ajl test's wait_for_tx)
    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("deploy tx {txn_id} committed");
    }
    Ok(())
}

async fn add_bridge(
    client: &mut MidenClientLib,
    _keystore: Arc<FilesystemKeyStore>,
    service_id: AccountId,
    ger_manager_id: AccountId,
) -> anyhow::Result<Account> {
    let account = create_bridge_account(client.rng().draw_word(), service_id, ger_manager_id);
    client.add_account(&account, false).await?;

    deploy_account(client, account.id(), "bridge").await?;

    Ok(account)
}

#[allow(clippy::too_many_arguments)]
async fn add_faucet(
    client: &mut MidenClientLib,
    token_symbol: &str,
    decimals: u8,
    origin_token_address: &[u8; 20],
    origin_network: u32,
    scale: u8,
    service_id: AccountId,
    bridge_account_id: AccountId,
) -> anyhow::Result<Account> {
    faucet_ops::create_and_register_faucet(
        client,
        token_symbol,
        decimals,
        origin_token_address,
        origin_network,
        scale,
        service_id,
        bridge_account_id,
    )
    .await
}

async fn add_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    let (auth_component, key_pair) = create_auth_component()?;
    let account = Account::builder(client.rng().draw_word().into())
        .with_component(BasicWallet)
        .with_auth_component(auth_component)
        .build()?;
    keystore.add_key(&key_pair, account.id()).await?;
    client.add_account(&account, false).await?;
    Ok(account)
}

/// Create a Network (public) wallet account for GER injection.
/// Must be public so import_account_by_id can refresh its state after
/// the NoteScreener bypass skips apply_transaction.
async fn add_public_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    use miden_protocol::account::AccountStorageMode;
    let (auth_component, key_pair) = create_auth_component()?;
    let account = Account::builder(client.rng().draw_word().into())
        .storage_mode(AccountStorageMode::Network)
        .with_component(BasicWallet)
        .with_auth_component(auth_component)
        .build()?;
    keystore.add_key(&key_pair, account.id()).await?;
    client.add_account(&account, false).await?;
    Ok(account)
}

async fn add_accounts(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Accounts> {
    let service = add_wallet(client, keystore.clone()).await?;
    let ger_manager = add_wallet(client, keystore.clone()).await?;
    deploy_account(client, ger_manager.id(), "ger_manager").await?;
    let bridge = add_bridge(client, keystore.clone(), service.id(), ger_manager.id()).await?;
    // ETH: 18 origin decimals, 8 miden decimals → scale=10
    let faucet_eth = add_faucet(
        client,
        "ETH",
        8,
        &[0u8; 20],
        0,
        10,
        service.id(),
        bridge.id(),
    )
    .await?;
    // AGG: 8 origin decimals, 8 miden decimals → scale=0
    let faucet_agg = add_faucet(
        client,
        "AGG",
        8,
        &[0u8; 20],
        0,
        0,
        service.id(),
        bridge.id(),
    )
    .await?;
    let wallet_hardhat = add_wallet(client, keystore.clone()).await?;
    Ok(Accounts {
        service,
        bridge,
        faucet_eth,
        faucet_agg,
        wallet_hardhat,
        ger_manager,
    })
}

async fn register_p2id_script(
    client: &mut MidenClientLib,
    sender: AccountId,
) -> anyhow::Result<()> {
    tracing::info!("registering P2ID script...");
    // dummy note to register its script on the node
    let note = P2idNote::create(
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

    // Wait for the NTX builder to process account creation transactions
    // before submitting notes that target those accounts.
    tracing::info!("waiting for account transactions to settle...");
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        client.sync_state().await?;
    }
    tracing::info!("account settlement complete");

    // Register the P2ID note tag for wallet_hardhat so sync discovers incoming P2ID notes.
    // The faucet's MASM `note_tag::create_account_target` takes the top 14 bits of the
    // account_id_prefix's high 32 bits: (prefix >> 32) & 0xFFFC0000
    {
        use miden_protocol::note::NoteTag;
        let wallet_id = accounts.wallet_hardhat.id();
        let prefix_u64 = wallet_id.prefix().as_felt().as_int();
        let hi32 = (prefix_u64 >> 32) as u32;
        let p2id_tag_value = hi32 & 0xFFFC0000u32; // top 14 bits
        let raw_tag = NoteTag::from(p2id_tag_value);
        tracing::info!(
            raw_tag = %u32::from(raw_tag),
            wallet = %AccountIdBech32(wallet_id),
            "registering P2ID note tag for wallet"
        );
        client.add_note_tag(raw_tag).await?;
    }

    // Faucet bridge registration is handled in create_and_register_faucet (via add_faucet)

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
