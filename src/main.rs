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

    /// Operator override for the L1 InfoTree indexer's start block. When
    /// set, the indexer ignores the persisted cursor and forces a forward
    /// walk from this L1 block on the next boot — used to back-fill
    /// historic `UpdateL1InfoTree` events whose `ger_entries` rows landed
    /// with NULL `(M, R)` (the STATE C orphans pattern seen on bali, 27
    /// rows from proxy blocks 95k-130k). After the back-fill completes
    /// and the cursor advances forward, remove the flag for subsequent
    /// boots — it serves no purpose once the cursor has moved past it.
    #[arg(long, env = "L1_INDEXER_FROM_BLOCK")]
    l1_indexer_from_block: Option<u64>,

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

    /// Disable the built-in Hardhat default-account alias (Cantina MA#8).
    /// When set, the special-case remap of the well-known Hardhat address
    /// (`0xf39f...2266`) to `wallet_hardhat` is refused. Production
    /// deployments MUST set this flag — otherwise an L1 deposit targeting
    /// the Hardhat default-account address would be silently routed into
    /// the operator's `wallet_hardhat` account. Enforced as a
    /// `--require-hardening` invariant.
    #[arg(long, env = "DISABLE_HARDHAT_ALIAS", default_value_t = false)]
    disable_hardhat_alias: bool,

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

    /// gRPC URL of a remote Miden transaction prover (e.g. `http://miden-prover:50051`).
    /// When set, all transaction proving for the persistent MidenClient (CLAIM / GER
    /// insert / faucet ops) is offloaded to this endpoint. When unset, proving stays
    /// in-process via the default LocalTransactionProver — the historical behaviour
    /// and the bali OOM cause.
    #[arg(long, env = "MIDEN_PROVER_URL")]
    miden_prover_url: Option<String>,

    /// Per-request timeout for the remote Miden prover, in seconds. Default 120s.
    /// Has no effect when --miden-prover-url is unset.
    #[arg(long, env = "MIDEN_PROVER_TIMEOUT_SECS", default_value_t = 120)]
    miden_prover_timeout_secs: u64,

    /// When the remote prover fails (timeout / connection error), retry the proof
    /// against an in-process LocalTransactionProver. Trades OOM safety for availability.
    /// Default OFF — preserves the bali OOM fix as the default behaviour.
    #[arg(long, env = "MIDEN_PROVER_FALLBACK_TO_LOCAL", default_value_t = false)]
    miden_prover_fallback_to_local: bool,

    /// RD-940 async writer-worker dispatch toggle. When `false` (the default
    /// during the RD-940 rollout up to Phase 7), `eth_sendRawTransaction` runs
    /// the existing synchronous handler unchanged. When `true`, requests are
    /// validated on the request thread and Miden submission is enqueued to the
    /// single writer-worker task — see `docs/design/RD-940-async-writer.md`.
    /// The flag is plumbed end-to-end starting at Phase 0; the actual fork on
    /// it lands in Phase 1.
    #[arg(long, env = "AGGLAYER_ENABLE_WRITER_WORKER", default_value_t = false)]
    enable_writer_worker: bool,
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
    if command.miden_prover_url.is_none() {
        reasons.push(
            "  - --require-hardening: --miden-prover-url must be set \
             (local prover is the documented OOM cause). Set \
             MIDEN_PROVER_URL to the gRPC URL of a remote Miden \
             transaction prover."
                .to_string(),
        );
    }
    // Cantina MA#8 — the Hardhat default-account alias is unsafe in
    // production. With `--require-hardening` set, the operator MUST
    // also pass `--disable-hardhat-alias` (env `DISABLE_HARDHAT_ALIAS`).
    if !command.disable_hardhat_alias {
        reasons.push(
            "  - --disable-hardhat-alias is unset (Cantina MA#8: the well-known \
             Hardhat default-account address `0xf39f...2266` would be silently \
             remapped to `wallet_hardhat` on every claim). Set \
             DISABLE_HARDHAT_ALIAS=true to refuse the remap in production."
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
            .field("disable_hardhat_alias", &self.disable_hardhat_alias)
            .field(
                "miden_api_key",
                &self.miden_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "miden_prover_url",
                &self.miden_prover_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("miden_prover_timeout_secs", &self.miden_prover_timeout_secs)
            .field(
                "miden_prover_fallback_to_local",
                &self.miden_prover_fallback_to_local,
            )
            .field("enable_writer_worker", &self.enable_writer_worker)
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

    // Startup probe — when --require-hardening is set AND a remote prover is
    // configured, dial the gRPC endpoint once at boot so a misconfigured
    // prover URL fails loudly here instead of surfacing as a stalled CLAIM
    // five minutes later. Read-only TCP/HTTP2 connect; NOT a prove() call.
    //
    // Skipped when --require-hardening is false so dev/local boots remain
    // tolerant of an offline prover.
    if command.require_hardening
        && let Some(prover_url) = command.miden_prover_url.as_deref()
    {
        let endpoint = ::tonic::transport::Endpoint::from_shared(prover_url.to_string())
            .context("invalid --miden-prover-url for startup probe")?
            .timeout(std::time::Duration::from_secs(5))
            .connect_timeout(std::time::Duration::from_secs(5));
        endpoint
            .connect()
            .await
            .context("remote prover unreachable (startup probe)")?;
        tracing::info!("remote prover reachable");
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
            command.miden_prover_url.clone(),
            command.miden_prover_timeout_secs,
            command.miden_prover_fallback_to_local,
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

        let config_path = init::init(&init_client, init_net_id, miden_store_dir.clone()).await?;
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
            // Run embedded SQL migrations BEFORE the connection pool opens.
            // This replaces the `agglayer-migrate` one-shot service that
            // hardcoded the migration list in docker-compose.e2e.yml — new
            // migrations are now part of the deploy artifact (compiled into
            // the binary via `include_str!`) so the proxy and its schema
            // can't drift out of sync.
            let report = miden_agglayer_service::store::migrator::run_migrations(_db_url)
                .await
                .context("running embedded DB migrations on startup")?;
            tracing::info!(
                applied = report.applied.len(),
                already_present = report.already_present.len(),
                "DB migrations complete"
            );

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
                    // Native ETH: on-chain MetadataHash is keccak256("") so the
                    // preimage is empty. Cantina MA#13.
                    metadata: Vec::new(),
                })
                .await?;
            tracing::info!("seeded faucet registry with default ETH faucet");
        }
    }

    // Cantina #13 / RD-703 — narrow the operator-supplied `--network-id`
    // (parsed as `u64` for CLI backward compat) to `u32` exactly once, here
    // at startup. The Solidity bridge contract types `originNetwork`,
    // `destinationNetwork`, and the on-chain `networkID()` return as
    // `uint32`, and BridgeOutScanner / ServiceState / claimAsset comparisons
    // are all `u32`. A `u64` value that doesn't fit `u32` is operator
    // misconfiguration — fail loudly here rather than silently truncate at
    // a later use site (the prior `as u32` cast in `service_send_raw_txn.rs`
    // would spuriously accept claims targeting `network_id & 0xFFFFFFFF`).
    let local_network_id_u32 = u32::try_from(command.network_id).map_err(|_| {
        anyhow::anyhow!(
            "--network-id ({}) does not fit in u32; bridge destinationNetwork / originNetwork are u32-sized",
            command.network_id
        )
    })?;
    let bridge_out_local_network_id = local_network_id_u32;
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

    // CLAIM-side chain-tail watcher: synthesises missing ClaimEvent logs for
    // CLAIMs the normal eth_sendRawTransaction path didn't fully record
    // (crash recovery + foreign CLAIM observations). Must run AFTER
    // BridgeOutScanner so the two listeners don't both try to claim the same
    // (latest + 1) slot in the same sync tick — BridgeOutScanner consumes
    // the slot for any B2AGG it processes; ClaimWatcher takes the next slot.
    let claim_watcher = Arc::new(miden_agglayer_service::claim_watcher::ClaimWatcher::new(
        store.clone(),
        block_state.clone(),
    ));

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> = vec![
        sync_listener,
        block_state.clone(),
        bridge_out_scanner,
        claim_watcher,
    ];

    let client = MidenClient::new(
        miden_store_dir.clone(),
        command.miden_node.clone(),
        command.miden_api_key.clone(),
        command.miden_prover_url.clone(),
        command.miden_prover_timeout_secs,
        command.miden_prover_fallback_to_local,
        sync_listeners,
        command.miden_debug,
    )?;

    // Self-heal is RUNTIME-only, not startup-only. See `src/account_recovery.rs`
    // — when a Miden submission inside `insert_ger` or `publish_claim` returns
    // an `AccountDataNotFound` or `IncorrectAccountInitialCommitment` error,
    // the caller reimports the affected account from the live Miden node and
    // retries once. We deliberately do NOT brick the proxy at startup over
    // locally-deployed-but-not-yet-network-tracked accounts (e.g. service,
    // wallet_hardhat) — those are healthy until first use, at which point
    // their initial `submit_new_transaction` deploys them on-chain.
    // Run restore if requested
    if command.restore {
        let result =
            miden_agglayer_service::restore::restore(&store, &client, &accounts.0, &block_state)
                .await?;

        tracing::info!(
            "Restore complete: block={}, bridge_outs={}, claims={}, gers={}, logs={}",
            result.block_number,
            result.bridge_outs_restored,
            result.claims_restored,
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
        local_network_id_u32,
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
    state.reject_hardhat_alias = command.disable_hardhat_alias;
    // Cantina #7: share the BridgeOutScanner's expected-MINT tracker so
    // `publish_claim_internal` can record the CLAIM NoteId and the scanner
    // can mark it Landed once it sees the bridge consume it.
    state.expected_mints = expected_mints_handle;
    state.miden_store_dir = miden_store_dir.clone().unwrap_or_default();
    state.miden_api_key = command.miden_api_key;
    state.enable_writer_worker = command.enable_writer_worker;

    // RD-940 — spawn the writer worker if the flag is set. The worker is a
    // single tokio task with a bounded mpsc queue between it and
    // `eth_sendRawTransaction`; see `docs/design/RD-940-async-writer.md`.
    //
    // The handle is plumbed into `ServiceState` (a `Clone` struct) BEFORE
    // `service::serve` clones the state per request, so every dispatcher sees
    // the same writer channel and inflight DashMap. Phase 5: the oneshot
    // shutdown sender is held in a local so we can fire it on graceful
    // SIGTERM and drain the queue before the process exits.
    let writer_shutdown: Option<tokio::sync::oneshot::Sender<()>> = if command.enable_writer_worker
    {
        let queue_depth =
            miden_agglayer_service::writer_worker::WriterWorker::parse_queue_depth_env();
        let tx_ttl = miden_agglayer_service::writer_worker::WriterWorker::parse_tx_ttl_env();
        let (handle, writer_shutdown_tx) =
            miden_agglayer_service::writer_worker::WriterWorker::spawn(
                state.clone(),
                queue_depth,
                tx_ttl,
            );
        tracing::info!(
            queue_depth,
            tx_ttl_secs = tx_ttl.as_secs(),
            "RD-940 writer worker spawned"
        );
        state.writer_handle = Some(Arc::new(handle));
        Some(writer_shutdown_tx)
    } else {
        tracing::info!(
            "RD-940 writer worker disabled (enable_writer_worker=false); \
             eth_sendRawTransaction runs the legacy synchronous handler"
        );
        None
    };

    // L1 InfoTree indexer — eliminates the RD-862 GER decomposition race by
    // proactively indexing every (mainnet, rollup) pair as L1 emits it,
    // instead of trying to recover the pair from a racing view call after the
    // GER lands on L2. Idempotent UPSERT: no-op if both code paths populate
    // the same ger_entries row. See `l1_info_tree_indexer.rs` for the full
    // race analysis and store-ordering guarantees.
    if let (Some(l1_rpc_url), Some(ger_addr_str)) =
        (state.l1_rpc_url.clone(), state.ger_l1_address.clone())
    {
        match ger_addr_str.parse::<alloy::primitives::Address>() {
            Ok(ger_addr) => {
                let mut indexer =
                    miden_agglayer_service::l1_info_tree_indexer::L1InfoTreeIndexer::new(
                        l1_rpc_url,
                        ger_addr,
                        state.store.clone(),
                    );
                if let Some(from_block) = command.l1_indexer_from_block {
                    indexer = indexer.with_from_block_override(from_block);
                }
                match indexer.spawn() {
                    Ok(shutdown_tx) => {
                        // The indexer runs for the lifetime of the tokio
                        // runtime; when `main` returns, the runtime tears
                        // down and the task stops with it. Leak the shutdown
                        // sender deliberately rather than store it on the
                        // (Clone) ServiceState — Sender is not Clone, and
                        // there is no graceful-shutdown path here that would
                        // benefit from holding it.
                        std::mem::forget(shutdown_tx);
                        tracing::info!("L1InfoTreeIndexer spawned");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to spawn L1InfoTreeIndexer");
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    address = %ger_addr_str,
                    error = %e,
                    "invalid --ger-l1-address; L1InfoTreeIndexer not started"
                );
            }
        }
    } else {
        tracing::warn!(
            "L1 RPC URL or GER contract address missing; L1InfoTreeIndexer disabled. \
             Without it, GER orphan resolution falls back to the racing view-call path \
             in service_send_raw_txn.rs and may produce orphan GERs under deposit load."
        );
    }

    // Initialize metrics.
    //
    // Histograms registered with `metrics-exporter-prometheus` default to the
    // Prometheus *summary* representation (a fixed set of quantiles), which
    // loses p95/p99 fidelity for low-volume metrics and — more importantly —
    // can't be aggregated across replicas in PromQL. For the two latency
    // metrics we actually care about (proof generation, JSON-RPC requests)
    // we install explicit bucket sets so they're emitted as real
    // `*_bucket{le="…"}` series. The proof buckets span 100ms (local prover
    // warm) → 5min (remote prover under load) because bali has empirically
    // seen 60–120s p99 proves; the RPC buckets are typical hot-path latencies
    // (1ms → 5s). Both metric names match the `histogram!()` call-site
    // strings in `metrics.rs` / `service.rs` exactly — using `Matcher::Full`
    // means a typo here silently falls back to summary, so add a test if
    // either name changes.
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("miden_proof_duration_seconds".to_string()),
            &[
                0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0,
            ],
        )
        .context("set_buckets_for_metric (miden_proof_duration_seconds) failed")?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("rpc_request_duration_seconds".to_string()),
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ],
        )
        .context("set_buckets_for_metric (rpc_request_duration_seconds) failed")?
        .install_recorder()
        .context("failed to install metrics recorder")?;
    miden_agglayer_service::metrics::init_metrics();

    // RD-940 Phase 5 — read the previous process's graceful-shutdown
    // snapshot. A non-zero value means in-flight WriteJobs whose hashes
    // had already been returned to callers were dropped on the last
    // restart. Page hard on this counter: every increment is real
    // unrecovered work and callers MUST re-submit. Must run AFTER
    // `init_metrics` so the recorder is registered.
    let dropped = miden_agglayer_service::writer_worker::read_and_clear_drop_snapshot();
    if dropped > 0 {
        tracing::error!(
            count = dropped,
            "RD-940 dropped_on_restart: previous shutdown left {dropped} in-flight job(s). \
             Their tx hashes were returned to callers but the work is unrecoverable in v1. \
             Callers MUST re-submit. Hard-page on this counter."
        );
        ::metrics::counter!("agglayer_writer_dropped_on_restart_total").increment(dropped);
    }

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

    // RD-940 Phase 5 — graceful drain. When `service::serve` returns
    // (SIGTERM or upstream error), signal the writer worker to stop
    // accepting new jobs, give it a short window to finish work that's
    // already mid-Miden-roundtrip, then snapshot any residual non-terminal
    // count to the dropped_on_restart tmpfile for the next boot. The
    // budget (20 s) sits inside aggkit's `WaitTxToBeMined = 2 m` so even
    // a partial drain doesn't leave aggkit wedged.
    if let Some(shutdown_tx) = writer_shutdown {
        let _ = shutdown_tx.send(());
        tracing::info!("RD-940 writer worker: drain signal sent");
        // Light wait — the worker exits its recv loop on the next
        // iteration. We don't await a JoinHandle (Phase 1 didn't expose
        // one). A short sleep gives the worker time to flip terminal_at
        // on anything currently mid-dispatch.
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        let residual = state
            .writer_handle
            .as_ref()
            .map(|h| h.inflight_non_terminal_count())
            .unwrap_or(0);
        if residual > 0 {
            tracing::warn!(
                residual,
                "RD-940 graceful drain: {residual} job(s) still in non-terminal state; \
                 writing snapshot to {} for next-boot dropped_on_restart accounting",
                miden_agglayer_service::writer_worker::DROP_SNAPSHOT_PATH
            );
            miden_agglayer_service::writer_worker::write_drop_snapshot(residual as u64);
            ::metrics::counter!(
                "agglayer_writer_drain_outcome_total",
                "outcome" => "partial",
            )
            .increment(1);
        } else {
            tracing::info!("RD-940 graceful drain: queue empty, clean shutdown");
            ::metrics::counter!(
                "agglayer_writer_drain_outcome_total",
                "outcome" => "clean",
            )
            .increment(1);
        }
    }

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
        cmd_with_prover(
            require,
            admin,
            signers,
            cors,
            Some("http://prover:50051".into()),
        )
    }

    /// Like [`cmd`] but leaves the prover-url tunable so the
    /// `--miden-prover-url`-must-be-set hardening reason can be exercised.
    fn cmd_with_prover(
        require: bool,
        admin: Option<String>,
        signers: Option<Vec<alloy::primitives::Address>>,
        cors: Option<Vec<String>>,
        prover_url: Option<String>,
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
            l1_indexer_from_block: None,
            miden_debug: false,
            cors_allowed_origins: cors,
            admin_api_key: admin,
            allowed_signers: signers,
            rate_limit_per_second: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_BURST,
            reject_zero_padding_addresses: false,
            require_hardening: require,
            // Cantina MA#8 — tests default the alias to disabled so the
            // pre-existing hardening tests below only flex the flag they
            // care about. The dedicated `hardening_flags_hardhat_alias`
            // test below pins the new invariant in isolation.
            disable_hardhat_alias: true,
            miden_api_key: None,
            miden_prover_url: prover_url,
            miden_prover_timeout_secs: 120,
            miden_prover_fallback_to_local: false,
            enable_writer_worker: false,
        }
    }

    /// When --require-hardening is false, no invariant is enforced.
    #[test]
    fn hardening_disabled_passes_with_open_defaults() {
        let mut c = cmd(false, None, None, None);
        c.disable_hardhat_alias = false;
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

    /// When hardening is enabled and the remote prover is unset, the gate
    /// must reject — local proving is the documented bali OOM cause.
    #[test]
    fn hardening_flags_missing_prover_url() {
        let c = cmd_with_prover(
            true,
            Some("k".into()),
            Some(vec![alloy::primitives::Address::ZERO]),
            None,
            None,
        );
        let reasons = check_hardening_invariants(&c).unwrap_err();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("--miden-prover-url"));
    }

    /// Cantina MA#8 — `--require-hardening` MUST require
    /// `--disable-hardhat-alias` to be set. Otherwise an operator
    /// thinks they're running a hardened build but the Hardhat default
    /// address is still being remapped to `wallet_hardhat` on every
    /// claim.
    #[test]
    fn hardening_flags_hardhat_alias_when_unset() {
        let mut c = cmd(
            true,
            Some("strong-admin-key".into()),
            Some(vec![alloy::primitives::Address::ZERO]),
            Some(vec!["https://app.example.com".into()]),
        );
        c.disable_hardhat_alias = false;
        let reasons = check_hardening_invariants(&c).unwrap_err();
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].contains("--disable-hardhat-alias"),
            "expected MA#8 invariant, got: {}",
            reasons[0]
        );
    }

    /// Cantina MA#8 — when `--require-hardening` is OFF, the hardhat
    /// invariant is not enforced. Operators can run dev-mode with the
    /// alias on (current default) or off.
    #[test]
    fn hardening_disabled_does_not_enforce_hardhat_alias() {
        let mut c = cmd(false, None, None, None);
        c.disable_hardhat_alias = false;
        assert!(check_hardening_invariants(&c).is_ok());
    }
}
