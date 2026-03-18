use clap::Parser;
use miden_agglayer_service::block_state::BlockState;
use miden_agglayer_service::bridge_out::BridgeOutScanner;
use miden_agglayer_service::l1_client::{AlloyL1Client, L1Client};
use miden_agglayer_service::service;
use miden_agglayer_service::service_state::ServiceState;
use miden_agglayer_service::store::memory::InMemoryStore;
use miden_agglayer_service::store::StoreSyncListener;
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

    /// PostgreSQL connection URL (enables PgStore instead of InMemoryStore)
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Restore mode: reconstruct store state from miden node + L1, then exit
    #[arg(long)]
    restore: bool,

    /// L1 bridge contract address (for restore + ClaimSettler)
    #[arg(long, env = "BRIDGE_ADDRESS")]
    bridge_address: Option<String>,

    /// L1 GER contract address
    #[arg(long, env = "L1_GER_ADDRESS", default_value = "0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674")]
    l1_ger_address: String,

    /// L1 RollupManager contract address (eth_call forwarding)
    #[arg(long, env = "ROLLUP_MANAGER_ADDRESS", default_value = "0x6c6c009cc348976db4a908c92b24433d4f6eda43")]
    rollup_manager_address: String,

    /// L1 Rollup contract address (eth_call forwarding)
    #[arg(long, env = "ROLLUP_ADDRESS", default_value = "0x414e9e227e4b589af92200508af5399576530e4e")]
    rollup_address: String,

    /// L1 block to start scanning from during restore
    #[arg(long, env = "L1_FROM_BLOCK", default_value_t = 0)]
    l1_from_block: u64,
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

    let bridge_account_id = accounts.0.bridge.0;
    let bridge_out_scanner = Arc::new(BridgeOutScanner::new(
        store.clone(),
        block_state.clone(),
        accounts.0.clone(),
        bridge_account_id,
    ));

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> = vec![
        sync_listener,
        block_state.clone(),
        bridge_out_scanner,
    ];

    let client = MidenClient::new(miden_store_dir.clone(), command.miden_node, sync_listeners)?;

    // Run restore if requested
    if command.restore {
        let l1_rpc = command.l1_rpc_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("--restore requires --l1-rpc-url"))?;
        let bridge_addr = command.bridge_address.as_deref()
            .ok_or_else(|| anyhow::anyhow!("--restore requires --bridge-address"))?;

        let result = miden_agglayer_service::restore::restore(
            &store,
            &client,
            &accounts.0,
            &block_state,
            l1_rpc,
            bridge_addr,
            command.l1_from_block,
        )
        .await?;

        tracing::info!(
            "Restore complete: block={}, claims={}, bridge_outs={}, gers={}, logs={}",
            result.block_number,
            result.claims_restored,
            result.bridge_outs_restored,
            result.gers_restored,
            result.logs_created,
        );

        client.shutdown()?;
        return Ok(());
    }

    let l1_client: Option<Arc<dyn L1Client>> = command.l1_rpc_url.map(|url| {
        Arc::new(AlloyL1Client::new(url, command.l1_ger_address.clone())) as Arc<dyn L1Client>
    });
    let state = ServiceState::new(
        client,
        accounts,
        command.chain_id,
        command.network_id,
        store,
        block_state,
        l1_client,
        command.rollup_manager_address,
        command.rollup_address,
    );

    // Optionally spawn the ClaimSettler background task
    if std::env::var("CLAIM_SETTLER_ENABLED").unwrap_or_default() == "true" {
        let bridge_service_url = std::env::var("BRIDGE_SERVICE_URL")
            .unwrap_or_else(|_| "http://bridge-service:8080".to_string());
        let l1_rpc_url = std::env::var("L1_RPC_URL")
            .unwrap_or_else(|_| "http://localhost:8545".to_string());
        let bridge_address: alloy::primitives::Address =
            std::env::var("BRIDGE_ADDRESS")
                .unwrap_or_default()
                .parse()
                .expect("BRIDGE_ADDRESS must be a valid address for ClaimSettler");
        let private_key_hex = std::env::var("CLAIM_SETTLER_PRIVATE_KEY")
            .expect("CLAIM_SETTLER_PRIVATE_KEY must be set when CLAIM_SETTLER_ENABLED=true");
        let signer: alloy::signers::local::PrivateKeySigner = private_key_hex
            .parse()
            .expect("CLAIM_SETTLER_PRIVATE_KEY must be a valid hex private key");

        let watch_addresses: Vec<alloy::primitives::Address> =
            match std::env::var("CLAIM_SETTLER_WATCH_ADDRESSES") {
                Ok(val) if !val.is_empty() => val
                    .split(',')
                    .map(|s| s.trim().parse().expect("invalid watch address"))
                    .collect(),
                _ => {
                    vec![alloy::signers::Signer::address(&signer)]
                }
            };

        let persistence_path = miden_store_dir
            .as_ref()
            .map(|d: &PathBuf| d.join("claim_settler_tracker.json"));

        let settler_config = miden_agglayer_service::claim_settler::ClaimSettlerConfig {
            bridge_service_url,
            l1_rpc_url,
            bridge_address,
            signer,
            watch_addresses,
            persistence_path,
        };
        let settler = miden_agglayer_service::claim_settler::ClaimSettler::new(settler_config)?;
        tokio::spawn(settler.run());
        tracing::info!("ClaimSettler background task spawned");
    }

    // Initialize metrics
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install metrics recorder");
    miden_agglayer_service::metrics::init_metrics();

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone(), metrics_handle).await?;

    state.miden_client.shutdown()?;

    Ok(())
}
