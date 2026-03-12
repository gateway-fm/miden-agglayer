use crate::service_state::ServiceState;
use clap::Parser;
use miden_agglayer_service::block_state::BlockState;
use miden_agglayer_service::log_synthesis::LogStore;
use miden_agglayer_service::*;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

mod service;
mod service_get_txn_receipt;
mod service_send_raw_txn;
pub mod service_state;

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

    /// L2 chain ID configured in the AggLayer
    #[arg(long, default_value_t = 2)]
    chain_id: u64,

    /// Create a new accounts config inside --miden-store-dir
    #[arg(long)]
    init: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    tracing::info!("{command:?}");

    let block_num_tracker = Arc::new(BlockNumTracker::new());
    let txn_manager = Arc::new(TxnManager::new());
    let block_state = Arc::new(BlockState::new());
    let log_store = Arc::new(LogStore::new());

    let miden_store_dir = command.miden_store_dir;
    let client = MidenClient::new(
        miden_store_dir.clone(),
        command.miden_node,
        vec![
            txn_manager.clone(),
            block_num_tracker.clone(),
            block_state.clone(),
        ],
    )?;

    if command.init || !config_path_exists(miden_store_dir.clone())? {
        let config_path = init::init(&client, miden_store_dir.clone()).await?;
        tracing::info!("new config created at {config_path:?}");
    }
    if command.init {
        client.shutdown()?;
        return Ok(());
    }

    let accounts = load_config(miden_store_dir.clone())?;
    let claim_persistence_path = miden_store_dir
        .as_ref()
        .map(|d| d.join("claimed_indices.json"));
    let claim_tracker = Arc::new(ClaimTracker::new(claim_persistence_path)?);
    let nonce_tracker = Arc::new(NonceTracker::new());
    let address_persistence_path = miden_store_dir
        .as_ref()
        .map(|d| d.join("address_mappings.json"));
    let address_mapper = Arc::new(AddressMapper::new(address_persistence_path)?);

    let state = ServiceState::new(
        client,
        accounts,
        command.chain_id,
        block_num_tracker,
        txn_manager,
        block_state,
        log_store,
        claim_tracker,
        nonce_tracker,
        address_mapper,
    );

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone()).await?;

    state.miden_client.shutdown()?;

    Ok(())
}
