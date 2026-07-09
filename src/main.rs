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

    /// Bind address for the JSON-RPC HTTP service (audit H2/C2). Default
    /// `0.0.0.0` (all interfaces) for backward compat. Set to `127.0.0.1` to
    /// restrict to loopback — the recommended production posture when the
    /// service sits behind a reverse proxy / sidecar that owns authn.
    #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0", value_parser = parse_bind_addr)]
    bind: String,

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

    /// Escape hatch: reset the persisted note-reconciler sweep cursor to 0 at
    /// boot so the reconciler re-walks the ENTIRE Miden history looking for
    /// externally-created network notes that sync missed. Use for deliberate
    /// full-history audits (e.g. after a proxy/node upgrade that may have
    /// changed note visibility). Idempotent per boot but expensive: on a long
    /// chain the sweep takes hours and loads the node — remove the flag after
    /// the audit boot. Normal restarts resume from the persisted cursor and
    /// do NOT need this. (`--restore` / `--reset-miden-store` already reset
    /// the cursor themselves — the wiped miden store makes the genesis
    /// re-sweep the healing pass.)
    #[arg(long, env = "RESWEEP_FROM_GENESIS")]
    resweep_from_genesis: bool,

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
    /// (case-insensitive). When unset, NO signer is accepted (audit C2 —
    /// fail-closed default; previously the default was open to any signer).
    /// To explicitly restore legacy open mode (ONLY safe behind a private
    /// network boundary / loopback bind), set `--insecure-allow-any-signer`.
    #[arg(long, env = "ALLOWED_SIGNERS", value_delimiter = ',')]
    allowed_signers: Option<Vec<alloy::primitives::Address>>,

    /// DANGEROUS: accept `eth_sendRawTransaction` from ANY signer (audit C2).
    /// Explicit opt-in for the legacy open mode that was the pre-C2 default.
    /// Refused by `--require-hardening`. Only safe with `--bind 127.0.0.1`
    /// and/or a network-level boundary.
    #[arg(long, env = "INSECURE_ALLOW_ANY_SIGNER", default_value_t = false)]
    insecure_allow_any_signer: bool,

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

    /// Hard read-only guarantee for recovery drills / cold reindexes against
    /// production networks. When set, EVERY transaction submission is refused
    /// at the single chokepoint all chain mutations funnel through
    /// (`miden_client::submit_new_transaction` + the CLAIM hot path's
    /// pre-submit check): the call returns an error, ERROR-logs, and
    /// increments `readonly_submissions_refused_total`. The proxy reads
    /// history (sync, sweep, reconcile) but can never send a transaction.
    #[arg(long, env = "AGGLAYER_READ_ONLY", default_value_t = false)]
    read_only: bool,
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
            "  - --allowed-signers is unset (eth_sendRawTransaction would reject \
             every signer — audit C2 fail-closed default). Set ALLOWED_SIGNERS \
             to a comma-separated allow-list."
                .to_string(),
        );
    }
    if command.insecure_allow_any_signer {
        reasons.push(
            "  - --insecure-allow-any-signer is set (eth_sendRawTransaction accepts \
             ANY signer — audit C2 legacy open mode). This is incompatible with \
             --require-hardening; remove it and use --allowed-signers instead."
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
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// clap value parser for `--bind`: validate the value as a bare IP address
/// (`0.0.0.0`, `127.0.0.1`, `::1`, …) at the CLI boundary. The service port is
/// a *separate* `--port` arg, so a `host:port` form (`127.0.0.1:8546`) or a
/// bare IPv6 literal that only fails later at URL construction is rejected here
/// with a clear message instead of blowing up deep in startup.
fn parse_bind_addr(s: &str) -> Result<String, String> {
    s.parse::<std::net::IpAddr>()
        .map(|_| s.to_string())
        .map_err(|_| {
            format!(
                "`{s}` is not a valid IP address (expected e.g. `0.0.0.0`, `127.0.0.1`, or `::1`; \
             the listening port is set separately via --port, not appended here)"
            )
        })
}

/// Build the JSON-RPC service URL from a validated bind host + port. IPv6
/// literals are bracketed (`::1` → `http://[::1]:8546`); without brackets the
/// colons in the address collide with the port separator and the URL is
/// invalid (`http://::1:8546`).
fn build_service_url(bind: &str, port: u16) -> Result<Url, url::ParseError> {
    let host = if bind.contains(':') {
        format!("[{bind}]")
    } else {
        bind.to_string()
    };
    Url::from_str(&format!("http://{host}:{port}"))
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("port", &self.port)
            .field("bind", &self.bind)
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
            .field("resweep_from_genesis", &self.resweep_from_genesis)
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
            .field("insecure_allow_any_signer", &self.insecure_allow_any_signer)
            .field("require_hardening", &self.require_hardening)
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
            .field("read_only", &self.read_only)
            .finish()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    // Install the process-wide Prometheus recorder FIRST — before any thread
    // or runtime that can emit a metric exists. `metrics` resolves the global
    // recorder per macro call, so this single install covers the MidenClient's
    // dedicated second runtime/thread, the writer worker, and the L1 indexer;
    // but anything emitted BEFORE this line would go to the no-op recorder and
    // silently vanish from /metrics (which is exactly what happened when the
    // install lived after client construction — init/restore/early-sync
    // emissions never reached the served registry).
    let metrics_handle = miden_agglayer_service::metrics::install_prometheus_recorder()?;
    miden_agglayer_service::bridge_address::init_bridge_address(command.bridge_address.clone());
    // Install the read-only switch BEFORE any client / submit path exists so
    // the guarantee holds from the very first instruction that could mutate
    // chain state (including the init phase's deploy transactions).
    miden_agglayer_service::miden_client::init_read_only(command.read_only);
    if command.read_only {
        tracing::warn!(
            "READ-ONLY mode active (--read-only / AGGLAYER_READ_ONLY): every transaction \
             submission will be refused at the submit chokepoint"
        );
    }
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

        // 0.15.3: the bridge account stores its AggLayer network id (set at
        // creation). Same u32 validation as the service path below (the MASM
        // only reads the low 32 bits, so a value that doesn't fit u32 would be
        // silently truncated).
        let init_network_id = u32::try_from(command.network_id).map_err(|_| {
            anyhow::anyhow!(
                "network_id {} does not fit in u32 (AggLayer network ids are u32)",
                command.network_id
            )
        })?;
        let config_path = init::init(
            &init_client,
            init_net_id,
            init_network_id,
            miden_store_dir.clone(),
        )
        .await?;
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

    // Reset the persisted note-reconciler sweep cursor BEFORE the
    // SyntheticProjector is constructed (it loads the cursor in `new()`):
    //   * `--reset-miden-store` wiped the miden-client sqlite above — the
    //     client has forgotten every imported note, so the genesis re-sweep
    //     IS the healing pass and must not be skipped by a stale cursor.
    //     (`--restore` resets it too, inside `restore()` Phase 4, and then
    //     exits — the next boot picks up the 0.)
    //   * `--resweep-from-genesis` is the operator escape hatch for
    //     deliberate full-history audits (e.g. after upgrades).
    if command.reset_miden_store || command.resweep_from_genesis {
        let reason = if command.reset_miden_store {
            "--reset-miden-store (miden store wiped; genesis sweep is the healing pass)"
        } else {
            "--resweep-from-genesis (operator-requested full-history audit)"
        };
        store.set_reconcile_cursor(0).await?;
        tracing::warn!(
            reason,
            "reconcile cursor reset — full-history re-sweep will run"
        );
    }

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
                    metadata: vec![],
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

    // Synthetic-indexer redesign — the SyntheticProjector is the SOLE
    // synthetic-event producer and the SINGLE owner of the synthetic tip
    // (Finding #5 eliminated by construction). The legacy writer paths only
    // submit to Miden; the projector re-derives every BridgeEvent / ClaimEvent /
    // GER log from the consumed Miden notes and advances the tip itself
    // (Miden-1:1). The BridgeOutScanner remains a sync listener purely for its
    // Miden-facing monitors (Cantina #9 LET-divergence, ownership probe).

    let bridge_out_scanner = Arc::new(
        BridgeOutScanner::new(
            store.clone(),
            bridge_out_local_network_id,
            accounts.0.bridge.0,
        )
        // Copilot #16 fail-closed: register the known-LOCAL non-faucet accounts
        // (service, ger_manager) so the #5/#6 consumer-provenance predicate
        // does NOT mislabel a note one of OUR local flows consumed as
        // "foreign" and suppress the alert. ger_manager is optional — include
        // it only when configured. (wallet_hardhat was removed entirely in
        // #125 — bridge-out wallets are now isolated per-store and never
        // consume claim/mint notes, so they don't belong in this set.)
        .with_local_accounts(
            [
                Some(accounts.0.service.0),
                accounts.0.ger_manager.as_ref().map(|g| g.0),
            ]
            .into_iter()
            .flatten(),
        )
        // Cantina #13 Layer 2 — wire the L1 RPC so legacy ERC-20 faucet rows with
        // empty metadata can be recovered + validated before a bridge-out emits.
        .with_l1_rpc_url(command.l1_rpc_url.clone()),
    );
    // Cantina #7: clone the tracker handle now so we can plumb it into
    // ServiceState below — `bridge_out_scanner` is moved into the listener
    // vec a few lines down.
    let expected_mints_handle = bridge_out_scanner.expected_mints.clone();

    let sync_listener = Arc::new(StoreSyncListener::new(store.clone(), block_state.clone()));

    // Register the projector LAST so it observes the same consumed-note feed the
    // monitors saw this tick, then advances the synthetic tip itself (no race —
    // it is the only writer of `latest_block_number`, Finding #5).
    let projector = Arc::new(
        miden_agglayer_service::synthetic_projector::SyntheticProjector::new(
            store.clone(),
            block_state.clone(),
            &accounts.0,
            local_network_id_u32,
            command.l1_rpc_url.clone(),
            command.miden_node.clone(),
            command.miden_api_key.clone(),
        )
        .await?,
    );
    tracing::info!(
        "SyntheticProjector registered: the SOLE synthetic-event producer and the SINGLE owner of \
         the synthetic tip. SINGLE-PROCESS ONLY — multiple replicas are NOT supported."
    );
    let sync_listeners: Vec<Arc<dyn miden_agglayer_service::miden_client::SyncListener>> = vec![
        sync_listener,
        block_state.clone(),
        bridge_out_scanner,
        projector,
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
    // locally-deployed-but-not-yet-network-tracked accounts (e.g. service)
    // — those are healthy until first use, at which point
    // their initial `submit_new_transaction` deploys them on-chain.
    // Run restore if requested
    if command.restore {
        let result = miden_agglayer_service::restore::restore(
            &store,
            &client,
            &accounts.0,
            local_network_id_u32,
            &block_state,
            command.l1_rpc_url.clone(),
            command.miden_node.as_deref(),
            command.miden_api_key.as_deref(),
        )
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
    state.allow_any_signer = command.insecure_allow_any_signer;
    state.rate_limit_per_second = command.rate_limit_per_second;
    state.rate_limit_burst = command.rate_limit_burst;
    state.reject_zero_padding_addresses = command.reject_zero_padding_addresses;
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

    // (Metrics recorder + `init_metrics` are installed at the very top of
    // main, before any metric-emitting thread exists — see
    // `metrics::install_prometheus_recorder`.)

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

    let url = build_service_url(&command.bind, command.port)?;
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
            bind: "0.0.0.0".into(),
            miden_store_dir: None,
            miden_node: None,
            chain_id: 1,
            network_id: 1,
            init: false,
            database_url: None,
            restore: false,
            reset_miden_store: false,
            unlock_miden_accounts: false,
            resweep_from_genesis: false,
            bridge_address: miden_agglayer_service::bridge_address::DEFAULT_BRIDGE_ADDRESS
                .to_string(),
            l1_rpc_url: None,
            ger_l1_address: None,
            l1_indexer_from_block: None,
            miden_debug: false,
            cors_allowed_origins: cors,
            admin_api_key: admin,
            allowed_signers: signers,
            insecure_allow_any_signer: false,
            rate_limit_per_second: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: miden_agglayer_service::service::DEFAULT_RATE_LIMIT_BURST,
            reject_zero_padding_addresses: false,
            require_hardening: require,
            miden_api_key: None,
            miden_prover_url: prover_url,
            miden_prover_timeout_secs: 120,
            miden_prover_fallback_to_local: false,
            enable_writer_worker: false,
            read_only: false,
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

    /// Audit C2 — `--insecure-allow-any-signer` (the legacy open-mode opt-in)
    /// is refused under `--require-hardening`. Starting from the all-set config
    /// that otherwise passes, flipping only the insecure opt-in must trip the
    /// gate with exactly one reason naming the flag.
    #[test]
    fn hardening_refuses_insecure_allow_any_signer() {
        let mut c = cmd(
            true,
            Some("strong-admin-key".into()),
            Some(vec![alloy::primitives::Address::ZERO]),
            Some(vec!["https://app.example.com".into()]),
        );
        c.insecure_allow_any_signer = true;
        let reasons = check_hardening_invariants(&c).unwrap_err();
        assert_eq!(
            reasons.len(),
            1,
            "only the insecure opt-in should trip the gate: {reasons:?}"
        );
        assert!(reasons[0].contains("--insecure-allow-any-signer"));
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
}

#[cfg(test)]
mod bind_tests {
    use super::*;
    use clap::Parser;

    /// A bad `--bind` value (host:port form, not a bare IP) is rejected at the
    /// CLI by the value parser instead of failing late at URL construction.
    #[test]
    fn bad_bind_value_rejected_at_cli() {
        // host:port is not a bare IpAddr → clap error.
        let err = Command::try_parse_from(["prog", "--bind", "127.0.0.1:8546"]);
        assert!(err.is_err(), "host:port bind must be rejected at the CLI");

        // Outright garbage is rejected too.
        assert!(
            Command::try_parse_from(["prog", "--bind", "not-an-ip"]).is_err(),
            "non-IP bind must be rejected at the CLI"
        );
    }

    /// The value parser accepts the valid bare-IP forms it documents.
    #[test]
    fn good_bind_values_accepted() {
        assert_eq!(parse_bind_addr("0.0.0.0").unwrap(), "0.0.0.0");
        assert_eq!(parse_bind_addr("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(parse_bind_addr("::1").unwrap(), "::1");
        assert!(parse_bind_addr("127.0.0.1:8546").is_err());
    }

    /// An IPv6 bind (`::1`) produces a valid, bracketed URL — the unbracketed
    /// `http://::1:8546` would be an invalid URL.
    #[test]
    fn ipv6_bind_builds_valid_bracketed_url() {
        let url = build_service_url("::1", 8546).expect("::1 must build a valid URL");
        assert_eq!(url.as_str(), "http://[::1]:8546/");
        assert_eq!(url.host_str(), Some("::1"));
        assert_eq!(url.port(), Some(8546));
    }

    /// IPv4 bind is left unbracketed.
    #[test]
    fn ipv4_bind_builds_plain_url() {
        let url = build_service_url("127.0.0.1", 8546).expect("valid IPv4 URL");
        assert_eq!(url.as_str(), "http://127.0.0.1:8546/");
        assert_eq!(url.port(), Some(8546));
    }
}
