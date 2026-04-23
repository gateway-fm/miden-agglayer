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

#[derive(Parser)]
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

    /// Big hammer recovery: wipe the miden-client sqlite store
    /// (`store.sqlite3` + WAL/SHM) before starting so the proxy re-syncs from
    /// the node. Keystore and `bridge_accounts.toml` are preserved.
    ///
    /// Combine with `--restore` to also rebuild the proxy store (PgStore /
    /// InMemoryStore) from on-chain notes in the same startup.
    #[arg(long)]
    reset_miden_store: bool,

    /// Surgical recovery: clear the `locked` flag on every account row in the
    /// miden-client sqlite, then exit. Use when `--reset-miden-store` would
    /// be overkill (i.e. the only symptom is a stale lock). Operator must
    /// restart the proxy afterwards.
    #[arg(long)]
    unlock_miden_accounts: bool,

    /// L1 bridge contract address used for synthetic log emission
    #[arg(
        long,
        env = "BRIDGE_ADDRESS",
        default_value = miden_agglayer_service::bridge_address::DEFAULT_BRIDGE_ADDRESS
    )]
    bridge_address: String,

    /// L1 RPC URL for resolving exit roots (enables full GER resolution)
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: Option<String>,

    /// L1 GER contract address for exit root resolution
    #[arg(long, env = "GER_L1_ADDRESS")]
    ger_l1_address: Option<String>,

    /// Enable Miden VM debug mode (verbose execution traces). Disable in production.
    #[arg(long, env = "MIDEN_DEBUG")]
    miden_debug: bool,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("port", &self.port)
            .field("miden_store_dir", &self.miden_store_dir)
            .field("miden_node", &self.miden_node)
            .field("chain_id", &self.chain_id)
            .field("network_id", &self.network_id)
            .field("init", &self.init)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("restore", &self.restore)
            .field("reset_miden_store", &self.reset_miden_store)
            .field("unlock_miden_accounts", &self.unlock_miden_accounts)
            .field("bridge_address", &self.bridge_address)
            .field(
                "l1_rpc_url",
                &self.l1_rpc_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("ger_l1_address", &self.ger_l1_address)
            .field("miden_debug", &self.miden_debug)
            .finish()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    miden_agglayer_service::bridge_address::init_bridge_address(command.bridge_address.clone());
    tracing::info!("{command:?}");

    let miden_store_dir = command.miden_store_dir;

    // Resolve the effective store directory for recovery flags (which need a
    // concrete path even when the user didn't pass `--miden-store-dir`).
    let effective_store_dir = miden_store_dir.clone().unwrap_or_else(|| {
        let base = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join(".miden")
    });

    // Surgical recovery: clear stale `locked` flags in miden-client's sqlite
    // and exit. Operator restarts the proxy afterwards.
    if command.unlock_miden_accounts {
        let cleared = miden_agglayer_service::recovery::unlock_miden_accounts(&effective_store_dir)
            .context("failed to clear locked flags in miden-client sqlite")?;
        tracing::info!(
            "unlock_miden_accounts: cleared {cleared} locked row(s); restart the proxy to pick up the change"
        );
        return Ok(());
    }

    // Big hammer recovery: wipe miden-client sqlite so startup re-syncs from
    // the node. Must happen before ClientBuilder opens the sqlite file.
    if command.reset_miden_store {
        let removed = miden_agglayer_service::recovery::reset_miden_store(&effective_store_dir)
            .context("failed to reset miden-client sqlite store")?;
        tracing::warn!(
            "reset_miden_store: removed {removed} sqlite file(s) from {}; \
             keystore and bridge_accounts.toml preserved",
            effective_store_dir.display()
        );
    }

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
            command.miden_debug,
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

    // Seed faucet registry if empty (first startup or InMemoryStore).
    // Only ETH is seeded by default; ERC-20s are auto-created by claim.rs::find_or_create_faucet
    // on first bridge. The AGG genesis placeholder was dropped in the 0.14.x migration — its
    // placeholder origin collided with ETH in the new on-chain token_registry_map.
    if store.list_faucets().await?.is_empty() {
        use miden_agglayer_service::store::FaucetEntry;
        if let Some(faucet_eth) = &accounts.0.faucet_eth {
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
            tracing::info!("seeded faucet registry with default ETH faucet");
        }
    }

    let bridge_out_scanner = Arc::new(BridgeOutScanner::new(store.clone(), block_state.clone()));

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> =
        vec![sync_listener, block_state.clone(), bridge_out_scanner];

    let client = MidenClient::new(
        miden_store_dir.clone(),
        command.miden_node,
        sync_listeners,
        command.miden_debug,
    )?;

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

    let mut state = ServiceState::new(
        client,
        accounts,
        command.chain_id,
        command.network_id,
        store,
        block_state,
    );
    state.l1_rpc_url = command.l1_rpc_url;
    state.ger_l1_address = command.ger_l1_address;
    state.miden_store_dir = miden_store_dir.clone().unwrap_or_default();
    // miden_node was moved into MidenClient::new, re-read from env
    state.miden_node_url =
        std::env::var("MIDEN_NODE_URL").unwrap_or_else(|_| "http://miden-node:57291".to_string());

    // Initialize metrics
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .context("failed to install metrics recorder")?;
    miden_agglayer_service::metrics::init_metrics();

    // Startup diagnostic: once the initial sync completes, check whether any
    // managed account is marked `locked` in miden-client's local state. A
    // stale lock is a symptom of a previous crash or commitment divergence and
    // will otherwise surface later as opaque "transaction conflicts with
    // current mempool state" errors on the first tx submission.
    //
    // Runs in the background so it doesn't delay `service::serve`. Worst case,
    // a locked account is flagged a few seconds into the proxy's lifetime
    // instead of strictly before it serves traffic.
    {
        let diag_client = state.miden_client.clone();
        let diag_accounts = state.accounts.0.clone();
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            while !diag_client.is_alive() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if !diag_client.is_alive() {
                tracing::warn!(
                    "startup diagnostic: miden-client not alive within 120s — skipping lock-status check"
                );
                return;
            }
            match miden_agglayer_service::recovery::detect_locked_accounts(
                &diag_client,
                &diag_accounts,
            )
            .await
            {
                Ok(locked) if !locked.is_empty() => {
                    tracing::error!(
                        "startup diagnostic: {} managed account(s) are LOCKED in miden-client: {:?}. \
                         This usually means local state diverged from the node. \
                         Recovery: restart with --unlock-miden-accounts (surgical) or \
                         --reset-miden-store --restore (full resync).",
                        locked.len(),
                        locked
                    );
                    ::metrics::counter!("miden_locked_accounts_detected_total")
                        .increment(locked.len() as u64);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!("startup diagnostic: lock-status check failed: {err:#}");
                }
            }
        });
    }

    // Observability heartbeat: emit one INFO line every HEARTBEAT_INTERVAL so operators
    // tailing logs can distinguish a healthy-idle service from a hung one. Without this,
    // a successfully-syncing service produces zero output — sync-success logs are at
    // DEBUG level (target `miden_agglayer_service::miden_client::sync::debug`) and every
    // other `info!` is event-driven. See `logging.rs` for how to opt into the per-sync
    // debug line instead of / in addition to this heartbeat.
    {
        const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
        let hb_client = state.miden_client.clone();
        let hb_store = state.store.clone();
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            // Consume the immediate first tick; first heartbeat fires after one interval
            // so it does not overlap with the "Service started" startup line.
            interval.tick().await;
            loop {
                interval.tick().await;
                let uptime_secs = started.elapsed().as_secs();
                let miden_client_alive = hb_client.is_alive();
                let latest_block = match hb_store.get_latest_block_number().await {
                    Ok(n) => n.to_string(),
                    Err(err) => format!("<err: {err:#}>"),
                };
                tracing::info!(
                    target: "miden_agglayer_service::heartbeat",
                    uptime_secs,
                    miden_client_alive,
                    latest_block,
                    "heartbeat"
                );
            }
        });
    }

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone(), metrics_handle).await?;

    state.miden_client.shutdown()?;

    Ok(())
}
