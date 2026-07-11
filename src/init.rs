use crate::accounts_config;
use crate::accounts_config::{AccountIdBech32, AccountsConfig};
use crate::faucet_ops;
use crate::miden_client::MidenClient;
use crate::miden_client::MidenClientLib;
use miden_base_agglayer::{MetadataHash, create_bridge_account};
use miden_client::crypto::FeltRng;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
use miden_protocol::account::{Account, AccountId, AccountType};
use miden_protocol::address::NetworkId;
use miden_protocol::note::NoteType;
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
    ger_manager: Account,
}

impl From<Accounts> for AccountsConfig {
    fn from(accounts: Accounts) -> Self {
        Self {
            service: AccountIdBech32(accounts.service.id()),
            bridge: AccountIdBech32(accounts.bridge.id()),
            faucet_eth: Some(AccountIdBech32(accounts.faucet_eth.id())),
            // AGG genesis faucet removed during 0.14.x migration: it registered under origin
            // [0u8; 20] which collides with ETH in the new on-chain token_registry_map. Any
            // additional token (POL, USDC, …) is auto-created by find_or_create_faucet in
            // claim.rs on first bridge.
            faucet_agg: None,
            ger_manager: Some(AccountIdBech32(accounts.ger_manager.id())),
        }
    }
}

fn create_auth_component(
    client: &mut MidenClientLib,
) -> anyhow::Result<(AuthSingleSig, AuthSecretKey)> {
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let auth_component = AuthSingleSig::new(
        key_pair.public_key().to_commitment(),
        AuthScheme::Falcon512Poseidon2,
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
    let txn_id = crate::metrics::meter_proof(
        crate::metrics::ProofKind::Init,
        crate::miden_client::submit_new_transaction(client, account_id, dummy_txn),
    )
    .await?;
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
    network_id: u32,
) -> anyhow::Result<Account> {
    // 0.15.3: the AggLayer network id is written into a bridge storage slot at
    // creation (was a hardcoded MASM constant pre-0.15.3). Must match the id the
    // L1 RollupManager assigns this rollup, or claims fail destination-network
    // checks on both ends.
    let account = create_bridge_account(
        client.rng().draw_word(),
        service_id,
        ger_manager_id,
        network_id,
    );
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
    metadata_hash: MetadataHash,
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
        metadata_hash,
        false, // proxy-created faucet: bridge-owned mint/burn (not Miden-native)
    )
    .await
}

async fn add_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    // Public storage mode is REQUIRED for the proxy's infrastructure accounts
    // (service, ger_manager) so a missing local sqlite row can
    // be recovered via `Client::import_account_by_id` from the live Miden
    // node. Private accounts (the AccountBuilder default) cannot be
    // recovered — their full state lives ONLY in the proxy's sqlite. The
    // regression that put bali in this state was commit dbe5c2d (Apr 2026),
    // which folded `add_public_wallet` into `add_wallet` during the 0.14.x
    // migration and dropped the explicit storage_mode call. Bali ran with
    // Private accounts for ~20 days until the proxy's sqlite lost the
    // ger_manager row, after which every aggoracle GER push rejected with
    // `AccountDataNotFound` and `--reset-miden-store --restore` could not
    // bring it back.
    //
    // We use `Public` rather than `Network` because the latter is
    // testnet/devnet-only on current upstream — local miden-node builds
    // (and any production node not running with network-tx enabled) reject
    // Network deployments with `Network transactions may not be submitted
    // by users yet`. Public gives us the recovery property (state on-chain,
    // import-by-id works) without the network-tx-builder semantics, which
    // the proxy doesn't use anyway.
    let (auth_component, key_pair) = create_auth_component(client)?;
    let account = Account::builder(client.rng().draw_word().into())
        .account_type(AccountType::Public)
        .with_component(BasicWallet)
        .with_auth_component(auth_component)
        .build()?;
    keystore.add_key(&key_pair, account.id()).await?;
    client.add_account(&account, false).await?;
    Ok(account)
}

/// Register the P2ID note tag for `wallet_id` so `sync_state` discovers incoming
/// P2ID (bridged-in) notes. The faucet's MASM `note_tag::create_account_target`
/// takes the top 14 bits of the account_id_prefix's high 32 bits:
/// `(prefix >> 32) & 0xFFFC0000`.
pub(crate) async fn register_wallet_p2id_tag(
    client: &mut MidenClientLib,
    wallet_id: AccountId,
) -> anyhow::Result<()> {
    use miden_protocol::note::NoteTag;
    let prefix_u64 = wallet_id.prefix().as_felt().as_canonical_u64();
    let hi32 = (prefix_u64 >> 32) as u32;
    let p2id_tag_value = hi32 & 0xFFFC0000u32; // top 14 bits
    let raw_tag = NoteTag::from(p2id_tag_value);
    tracing::info!(
        raw_tag = %u32::from(raw_tag),
        wallet = %AccountIdBech32(wallet_id),
        "registering P2ID note tag for wallet"
    );
    client.add_note_tag(raw_tag).await?;
    Ok(())
}

/// Create a standalone `Public` `BasicWallet` in `client`'s store and register
/// its P2ID note tag. Used by `bridge-out-tool --create-wallet` to stand up a
/// fully INDEPENDENT bridge-out wallet whose sqlite store is SEPARATE from the
/// proxy's — mirroring production, where the B2AGG (bridge-out) wallet is an
/// independent wallet the proxy never shares `store.sqlite3` with. The caller
/// is responsible for syncing afterwards to settle the account on the node.
pub async fn create_standalone_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
) -> anyhow::Result<Account> {
    let account = add_wallet(client, keystore).await?;
    register_wallet_p2id_tag(client, account.id()).await?;
    Ok(account)
}

async fn add_accounts(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
    network_id: u32,
) -> anyhow::Result<Accounts> {
    let service = add_wallet(client, keystore.clone()).await?;
    let ger_manager = add_wallet(client, keystore.clone()).await?;
    deploy_account(client, ger_manager.id(), "ger_manager").await?;
    let bridge = add_bridge(
        client,
        keystore.clone(),
        service.id(),
        ger_manager.id(),
        network_id,
    )
    .await?;
    // ETH: 18 origin decimals → 8 miden decimals (scale=10). Native ETH has empty metadata on
    // the L1 bridge, so the faucet's stored metadata_hash is keccak256("") — matches any
    // CLAIM leaf_data.metadata_hash for ETH deposits.
    let faucet_eth = add_faucet(
        client,
        "ETH",
        8,
        &[0u8; 20],
        0,
        10,
        service.id(),
        bridge.id(),
        MetadataHash::from_abi_encoded(&[]),
    )
    .await?;
    Ok(Accounts {
        service,
        bridge,
        faucet_eth,
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
        .own_output_notes(vec![note])
        .build()?;

    let txn_id = crate::metrics::meter_proof(
        crate::metrics::ProofKind::Init,
        crate::miden_client::submit_new_transaction(client, sender, txn),
    )
    .await?;
    tracing::info!("registered P2ID script with txn_id {txn_id}");
    Ok(())
}

async fn init_internal(
    client: &mut MidenClientLib,
    keystore: Arc<FilesystemKeyStore>,
    net_id: NetworkId,
    network_id: u32,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    client.sync_state().await?;
    let accounts = add_accounts(client, keystore, network_id).await?;

    // Wait for the NTX builder to process account creation transactions
    // before submitting notes that target those accounts.
    tracing::info!("waiting for account transactions to settle...");
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        client.sync_state().await?;
    }
    tracing::info!("account settlement complete");

    // Faucet bridge registration is handled in create_and_register_faucet (via add_faucet)

    register_p2id_script(client, accounts.service.id()).await?;

    let config = AccountsConfig::from(accounts);
    let config_path = accounts_config::save_config(config, &net_id, miden_store_dir)?;
    Ok(config_path)
}

pub async fn init(
    client: &MidenClient,
    net_id: NetworkId,
    network_id: u32,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let result = Arc::new(OnceLock::<PathBuf>::new());
    let result_internal = result.clone();
    let keystore = client.get_keystore();

    let future = client.with(move |client| {
        Box::new(async move {
            let result =
                init_internal(client, keystore, net_id, network_id, miden_store_dir).await?;
            result_internal.set(result).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(result.get().unwrap().clone())
}
