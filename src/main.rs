use clap::Parser;
use miden_agglayer_service::block_state::BlockState;
use miden_agglayer_service::bridge_out::{BridgeOutScanner, BridgeOutTracker};
use miden_agglayer_service::log_synthesis::LogStore;
use miden_agglayer_service::service;
use miden_agglayer_service::service_state::ServiceState;
use miden_agglayer_service::*;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Command {
    /// JSON-RPC HTTP service listening port
    #[arg(long, default_value_t = 8546)]
    port: u16,

    /// Directory for miden-client data [default: $HOME/.miden]
    #[arg(long)]
    miden_store_dir: Option<PathBuf>,

    /// Miden node GRPC URL or a network name: "devnet" or "testnet" [default: http://localhost:57291]
    #[arg(long)]
    miden_node: Option<String>,

    /// L2 chain ID configured in the AggLayer (EVM chain ID for eth_chainId)
    #[arg(long, default_value_t = 2, env = "CHAIN_ID")]
    chain_id: u64,

    /// Rollup network ID assigned by the RollupManager (used by bridge's networkID())
    /// This is NOT the same as chain_id — first rollup in RollupManager gets network ID 1.
    #[arg(long, default_value_t = 1, env = "NETWORK_ID")]
    network_id: u64,

    /// Create a new accounts config inside --miden-store-dir
    #[arg(long)]
    init: bool,

    /// L1 RPC URL for reading exit roots during GER injection
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    tracing::info!("{command:?}");

    let miden_store_dir = command.miden_store_dir;
    let needs_init = command.init || !config_path_exists(miden_store_dir.clone())?;

    // Phase 1: Run init if needed (with a minimal client, no BridgeOutScanner)
    if needs_init {
        let block_num_tracker = Arc::new(BlockNumTracker::new());
        let block_state = Arc::new(BlockState::new());
        let log_store = Arc::new(LogStore::new());
        let txn_manager = Arc::new(TxnManager::new(log_store.clone(), block_state.clone()));
        let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> =
            vec![txn_manager, block_num_tracker, block_state];

        let init_client = MidenClient::new(
            miden_store_dir.clone(),
            command.miden_node.clone(),
            sync_listeners,
        )?;

        let config_path = init::init(&init_client, miden_store_dir.clone()).await?;
        tracing::info!("new config created at {config_path:?}");

        init_client.shutdown()?;

        if command.init {
            return Ok(());
        }
    }

    // Phase 2: Load config (always exists at this point) and create full client
    let block_num_tracker = Arc::new(BlockNumTracker::new());
    let block_state = Arc::new(BlockState::new());
    let log_store = Arc::new(LogStore::new());
    let txn_manager = Arc::new(TxnManager::new(log_store.clone(), block_state.clone()));

    let accounts = load_config(miden_store_dir.clone())?;

    let bridge_out_persistence_path = miden_store_dir
        .as_ref()
        .map(|d: &PathBuf| d.join("bridge_out_tracker.json"));
    let bridge_out_tracker = BridgeOutTracker::new(bridge_out_persistence_path)?;
    let bridge_account_id = accounts.0.bridge.0;
    let bridge_out_scanner = Arc::new(BridgeOutScanner::new(
        log_store.clone(),
        block_state.clone(),
        accounts.0.clone(),
        bridge_out_tracker,
        bridge_account_id,
    ));

    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> = vec![
        txn_manager.clone(),
        block_num_tracker.clone(),
        block_state.clone(),
        bridge_out_scanner,
    ];

    let client = MidenClient::new(miden_store_dir.clone(), command.miden_node, sync_listeners)?;

    let claim_persistence_path = miden_store_dir
        .as_ref()
        .map(|d: &PathBuf| d.join("claimed_indices.json"));
    let claim_tracker = Arc::new(ClaimTracker::new(claim_persistence_path)?);
    let nonce_tracker = Arc::new(NonceTracker::new());
    let address_persistence_path = miden_store_dir
        .as_ref()
        .map(|d: &PathBuf| d.join("address_mappings.json"));
    let address_mapper = Arc::new(AddressMapper::new(address_persistence_path)?);

    let state = ServiceState::new(
        client,
        accounts,
        command.chain_id,
        command.network_id,
        block_num_tracker,
        txn_manager,
        block_state,
        log_store,
        claim_tracker,
        nonce_tracker,
        address_mapper,
        command.l1_rpc_url,
    );

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone()).await?;

    state.miden_client.shutdown()?;

    Ok(())
}
