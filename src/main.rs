use anyhow::Context;
use clap::Parser;
use miden_agglayer_service::block_state::BlockState;
use miden_agglayer_service::bridge_out::BridgeOutScanner;
use miden_agglayer_service::service;
use miden_agglayer_service::service_state::ServiceState;
use miden_agglayer_service::store::StoreSyncListener;
use miden_agglayer_service::store::memory::InMemoryStore;
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

    /// PostgreSQL connection URL (enables PgStore instead of InMemoryStore)
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Restore mode: reconstruct store state from miden node, then exit
    #[arg(long)]
    restore: bool,

    /// L1 bridge contract address used for synthetic log emission
    #[arg(
        long,
        env = "BRIDGE_ADDRESS",
        default_value = miden_agglayer_service::bridge_address::DEFAULT_BRIDGE_ADDRESS
    )]
    bridge_address: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    miden_agglayer_service::bridge_address::init_bridge_address(command.bridge_address.clone());
    tracing::info!("{command:?}");

    let miden_store_dir = command.miden_store_dir;
    let needs_init = command.init || !config_path_exists(miden_store_dir.clone())?;

    // Phase 1: Run init if needed (with a minimal client, no BridgeOutScanner)
    if needs_init {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let block_state = Arc::new(BlockState::new());
        let sync_listener = Arc::new(StoreSyncListener::new(store, block_state.clone()));

        let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> =
            vec![sync_listener, block_state];

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

    // Phase 2: Create the store
    let store: Arc<dyn Store> = if let Some(_db_url) = &command.database_url {
        #[cfg(feature = "postgres")]
        {
            let pg = miden_agglayer_service::store::postgres::PgStore::new(_db_url).await?;
            Arc::new(pg)
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = _db_url;
            anyhow::bail!(
                "--database-url requires the 'postgres' feature. \
                 Rebuild with: cargo build --features postgres"
            );
        }
    } else {
        Arc::new(InMemoryStore::new())
    };

    // Phase 3: Load config and create full client
    let block_state = Arc::new(BlockState::new());

    let accounts = load_config(miden_store_dir.clone())?;

    // Seed faucet registry if empty (first startup or InMemoryStore)
    if store.list_faucets().await?.is_empty() {
        use miden_agglayer_service::store::FaucetEntry;
        if let (Some(faucet_eth), Some(faucet_agg)) =
            (&accounts.0.faucet_eth, &accounts.0.faucet_agg)
        {
            store
                .register_faucet(FaucetEntry {
                    faucet_id: faucet_eth.0,
                    origin_address: [0u8; 20],
                    origin_network: 0,
                    symbol: "ETH".into(),
                    origin_decimals: 18,
                    miden_decimals: 8,
                    scale: 10,
                })
                .await?;
            store
                .register_faucet(FaucetEntry {
                    faucet_id: faucet_agg.0,
                    origin_address: [0u8; 20],
                    origin_network: 0,
                    symbol: "AGG".into(),
                    origin_decimals: 8,
                    miden_decimals: 8,
                    scale: 0,
                })
                .await?;
            tracing::info!("seeded faucet registry with default ETH and AGG faucets");
        }
    }

    let bridge_out_scanner = Arc::new(BridgeOutScanner::new(store.clone(), block_state.clone()));

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> =
        vec![sync_listener, block_state.clone(), bridge_out_scanner];

    let client = MidenClient::new(miden_store_dir.clone(), command.miden_node, sync_listeners)?;

    // Run restore if requested
    if command.restore {
        let result =
            miden_agglayer_service::restore::restore(&store, &client, &accounts.0, &block_state)
                .await?;

        tracing::info!(
            "Restore complete: block={}, bridge_outs={}, gers={}, logs={}",
            result.block_number,
            result.bridge_outs_restored,
            result.gers_restored,
            result.logs_created,
        );

        client.shutdown()?;
        return Ok(());
    }

    let state = ServiceState::new(
        client,
        accounts,
        command.chain_id,
        command.network_id,
        store,
        block_state,
    );

    // Initialize metrics
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install metrics recorder")?;
    miden_agglayer_service::metrics::init_metrics();

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone(), metrics_handle).await?;

    state.miden_client.shutdown()?;

    Ok(())
}
