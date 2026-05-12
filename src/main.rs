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

    /// CORS-allowed origins for the JSON-RPC route (R11). Comma-separated list of
    /// scheme://host[:port] entries. The single value `*` enables a permissive
    /// wildcard (DEV ONLY — do not deploy to mainnet). Omit to disable CORS entirely
    /// (the safe production default).
    #[arg(long, env = "CORS_ALLOWED_ORIGINS", value_delimiter = ',')]
    cors_allowed_origins: Option<Vec<String>>,

    /// Admin API key gating the `admin_*` JSON-RPC methods (R1). When unset,
    /// `admin_*` requests are rejected with "admin endpoints disabled". Set this to
    /// a long random token in production (rotate via deploy). Callers must send
    /// `Authorization: Bearer <token>`. Comparison is constant-time.
    #[arg(long, env = "ADMIN_API_KEY")]
    admin_api_key: Option<String>,

    /// Allow-list of EVM signer addresses permitted to submit
    /// `eth_sendRawTransaction` (R2). Comma-separated 0x-prefixed addresses
    /// (case-insensitive). When unset, every well-formed signer is accepted
    /// (legacy open mode — only safe behind a private network boundary).
    #[arg(long, env = "ALLOWED_SIGNERS", value_delimiter = ',')]
    allowed_signers: Option<Vec<alloy::primitives::Address>>,

    /// Per-IP rate limit, sustained requests per second (R13). Default 500.
    #[arg(long, env = "RATE_LIMIT_PER_SECOND", default_value_t = miden_agglayer_service::service::DEFAULT_RATE_LIMIT_PER_SECOND)]
    rate_limit_per_second: u64,

    /// Per-IP rate limit, burst capacity (R13). Default 500.
    #[arg(long, env = "RATE_LIMIT_BURST", default_value_t = miden_agglayer_service::service::DEFAULT_RATE_LIMIT_BURST)]
    rate_limit_burst: u32,

    /// Reject the address-mapper zero-padding fallback (C5). When set,
    /// claims targeting an EVM address with no explicit store mapping are
    /// rejected immediately instead of falling through to the structural
    /// reconstruction. Production posture; default false for backward
    /// compat with aggsender / aggoracle / hardhat dev flows.
    #[arg(long, env = "REJECT_ZERO_PADDING_ADDRESSES", default_value_t = false)]
    reject_zero_padding_addresses: bool,

    /// Production hardening invariant. When set, refuse to start if any of
    /// the following hardening flags are at their fail-open defaults:
    /// - `--admin-api-key` unset (admin endpoints accept any caller)
    /// - `--allowed-signers` unset (any signer can submit txs)
    /// - `--cors-allowed-origins` set to a wildcard `*`
    ///
    /// Operators deploying to mainnet should set `--require-hardening`
    /// (env `REQUIRE_HARDENING`) to make these mistakes startup failures
    /// rather than silent runtime exposures.
    #[arg(long, env = "REQUIRE_HARDENING", default_value_t = false)]
    require_hardening: bool,

    /// API key sent as `authorization: Bearer <key>` on every outbound Miden gRPC call.
    ///
    /// Required when the node sits behind a gateway that rate-limits unauthenticated
    /// traffic (e.g. `miden-testnet.eu-central-8.gateway.fm`). Safe to omit when
    /// targeting the node directly. Redacted in log output.
    #[arg(long, env = "MIDEN_API_KEY")]
    miden_api_key: Option<String>,
}

/// Validate the `--require-hardening` invariants. Returns a list of
/// reason strings naming each unsatisfied flag. Empty list = pass.
fn check_hardening_invariants(command: &Command) -> Result<(), Vec<String>> {
    if !command.require_hardening {
        return Ok(());
    }
    let mut reasons = Vec::new();
    if command.admin_api_key.is_none() {
        reasons.push(
            "  - --admin-api-key is unset (admin_* methods would be open). \
             Set ADMIN_API_KEY to a long random token."
                .to_string(),
        );
    }
    if command
        .allowed_signers
        .as_ref()
        .is_none_or(|v| v.is_empty())
    {
        reasons.push(
            "  - --allowed-signers is unset (eth_sendRawTransaction would accept \
             any signer). Set ALLOWED_SIGNERS to a comma-separated allow-list."
                .to_string(),
        );
    }
    if let Some(origins) = command.cors_allowed_origins.as_ref()
        && origins.iter().any(|o| o == "*")
    {
        reasons.push(
            "  - --cors-allowed-origins contains a wildcard `*` (browsers from \
             any origin can hit state-mutating endpoints). Use an explicit \
             origin list."
                .to_string(),
        );
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
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
            .field(
                "admin_api_key",
                &self.admin_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("allowed_signers", &self.allowed_signers)
            .field("require_hardening", &self.require_hardening)
            .field(
                "miden_api_key",
                &self.miden_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    miden_agglayer_service::bridge_address::init_bridge_address(command.bridge_address.clone());
    tracing::info!("{command:?}");

    // Hardening startup invariants — fail loud on fail-open production
    // configurations. Reviewer-flagged (R1+R2+R11). Without this, an
    // operator can launch the proxy with all three hardening flags at
    // their fail-open defaults and the only signal is a faint info-level
    // log line. Loud startup failure is the right escalation.
    if let Err(reasons) = check_hardening_invariants(&command) {
        anyhow::bail!(
            "--require-hardening is set but the following invariants are not satisfied:\n{}\n\
             Either set the listed flags or drop --require-hardening for dev mode.",
            reasons.join("\n")
        );
    }

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
            command.miden_api_key.clone(),
            sync_listeners,
            command.miden_debug,
        )?;

        // Resolve the NetworkId from the same `--miden-node` flag MidenClient uses,
        // so the bech32 strings written to bridge_accounts.toml use the active
        // node's HRP (e.g. `mtst` on testnet). Without this, every saved id
        // would be encoded with the local-network HRP (`mlcl`) regardless of
        // which network the agglayer is actually deployed against.
        let init_net_id = miden_agglayer_service::miden_client::resolve_network_id(
            command.miden_node.as_deref(),
        )?;

        let config_path =
            init::init(&init_client, init_net_id, miden_store_dir.clone()).await?;
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

    // Cantina #13 — BridgeOutScanner needs to know our local network id so it can
    // refuse to emit synthetic BridgeEvents for self-targeted poison leaves.
    let bridge_out_local_network_id = u32::try_from(command.network_id).map_err(|_| {
        anyhow::anyhow!(
            "--network-id ({}) does not fit in u32; B2AGG destination_network is u32-sized",
            command.network_id
        )
    })?;
    let bridge_out_scanner = Arc::new(BridgeOutScanner::new(
        store.clone(),
        block_state.clone(),
        bridge_out_local_network_id,
        accounts.0.bridge.0,
    ));
    // Cantina #7: clone the tracker handle now so we can plumb it into
    // ServiceState below — `bridge_out_scanner` is moved into the listener
    // vec a few lines down.
    let expected_mints_handle = bridge_out_scanner.expected_mints.clone();

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> =
        vec![sync_listener, block_state.clone(), bridge_out_scanner];

    let client = MidenClient::new(
        miden_store_dir.clone(),
        command.miden_node.clone(),
        command.miden_api_key.clone(),
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
    state.cors_allowed_origins = command.cors_allowed_origins;
    state.admin_api_key = command.admin_api_key;
    state.allowed_signers = command.allowed_signers;
    state.rate_limit_per_second = command.rate_limit_per_second;
    state.rate_limit_burst = command.rate_limit_burst;
    state.reject_zero_padding_addresses = command.reject_zero_padding_addresses;
    // Cantina #7: share the BridgeOutScanner's expected-MINT tracker so
    // `publish_claim_internal` can record the CLAIM NoteId and the scanner
    // can mark it Landed once it sees the bridge consume it.
    state.expected_mints = expected_mints_handle;
    state.miden_store_dir = miden_store_dir.clone().unwrap_or_default();
    // The fresh `MidenClient` built per `publish_claim` in `src/claim.rs` must connect to
    // the SAME node URL as `MidenClient::new` — that's what `command.miden_node` feeds.
    // This used to read an independent `MIDEN_NODE_URL` env var with a `http://miden-node:57291`
    // fallback; when the env var wasn't set by the deployment, fresh-client builds
    // unconditionally failed with `dns error: Name or service not known` on the fallback
    // hostname, while the persistent sync loop (which reads `command.miden_node`) kept
    // working fine. Claims silently never landed. See RD-856.
    state.miden_node_url = command
        .miden_node
        .clone()
        .unwrap_or_else(|| "http://localhost:57291".to_string());
    tracing::info!(
        miden_node_url = %state.miden_node_url,
        "fresh-client `publish_claim` path will dial this Miden node URL"
    );
    state.miden_api_key = command.miden_api_key;

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

#[cfg(test)]
mod hardening_tests {
    use super::*;

    /// Test fixture: build a Command with all the boring fields set to
    /// minimum-valid defaults, then mutate the hardening fields per test.
    fn cmd(
        require: bool,
        admin: Option<String>,
        signers: Option<Vec<alloy::primitives::Address>>,
        cors: Option<Vec<String>>,
    ) -> Command {
        Command {
            port: 8546,
            miden_store_dir: None,
            miden_node: None,
            chain_id: 1,
            network_id: 1,
            init: false,
            database_url: None,
            restore: false,
            reset_miden_store: false,
            unlock_miden_accounts: false,
            bridge_address: miden_agglayer_service::bridge_address::DEFAULT_BRIDGE_ADDRESS
                .to_string(),
            l1_rpc_url: None,
            ger_l1_address: None,
            miden_debug: false,
            cors_allowed_origins: cors,
            admin_api_key: admin,
            allowed_signers: signers,
            rate_limit_per_second: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_BURST,
            reject_zero_padding_addresses: false,
            require_hardening: require,
            miden_api_key: None,
        }
    }

    /// When --require-hardening is false, no invariant is enforced.
    #[test]
    fn hardening_disabled_passes_with_open_defaults() {
        let c = cmd(false, None, None, None);
        assert!(check_hardening_invariants(&c).is_ok());
    }

    /// All three flags missing → all three reasons reported.
    #[test]
    fn hardening_enabled_lists_every_unsatisfied_invariant() {
        let c = cmd(true, None, None, None);
        let reasons = check_hardening_invariants(&c).unwrap_err();
        assert_eq!(reasons.len(), 2, "admin + signers missing; cors absent OK");
        assert!(reasons[0].contains("--admin-api-key"));
        assert!(reasons[1].contains("--allowed-signers"));
    }

    /// Wildcard CORS triggers the third reason.
    #[test]
    fn hardening_flags_wildcard_cors() {
        let c = cmd(
            true,
            Some("k".into()),
            Some(vec![alloy::primitives::Address::ZERO]),
            Some(vec!["*".into()]),
        );
        let reasons = check_hardening_invariants(&c).unwrap_err();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("wildcard `*`"));
    }

    /// All flags set correctly → pass even with hardening enabled.
    #[test]
    fn hardening_all_set_passes() {
        let c = cmd(
            true,
            Some("strong-admin-key".into()),
            Some(vec![alloy::primitives::Address::ZERO]),
            Some(vec!["https://app.example.com".into()]),
        );
        assert!(check_hardening_invariants(&c).is_ok());
    }
}
