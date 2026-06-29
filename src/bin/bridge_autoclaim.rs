//! bridge-autoclaim — standalone L2→L1 auto-claimer.
//!
//! Discovers our rollup's L2→L1 exits from the proxy's own synthetic
//! `BridgeEvent` (eth_getLogs), gates on the L1 bridge's on-chain `isClaimed`,
//! fetches proofs from the bridge-service `/merkle-proof` (GetClaim path), and
//! submits `claimAsset` on L1 with a sponsor wallet. It never touches
//! `/pending-bridges`. See `src/l2_to_l1_claimer.rs` and the README section
//! "L2->L1 auto-claimer" for the design + decision record.
//!
//! Usage:
//!   SPONSOR_PRIVATE_KEY=0x... \
//!   bridge-autoclaim \
//!     --l2-rpc-url http://localhost:8546 \
//!     --l1-rpc-url http://localhost:8545 \
//!     --bridge-address 0x... \
//!     --bridge-service-url http://localhost:18080 \
//!     --network-id 1
//!
//! The sponsor private key is NEVER a flag — it is read from the environment
//! variable named by `--sponsor-key-env` (default `SPONSOR_PRIVATE_KEY`), which
//! deployment populates from the secret store.

use std::time::Duration;

use clap::Parser;
use miden_agglayer_service::l2_to_l1_claimer::{ClaimerConfig, run};

#[derive(Parser, Debug)]
#[command(version, about = "Standalone L2->L1 auto-claimer (claimAsset sponsor)")]
struct Args {
    /// L2 proxy JSON-RPC URL (source of BridgeEvent via eth_getLogs).
    #[arg(long, env = "L2_RPC_URL")]
    l2_rpc_url: String,

    /// L1 JSON-RPC URL (isClaimed view-calls + claimAsset submission).
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Bridge contract address (L1 claim target + L2 BridgeEvent log filter;
    /// assumes the canonical CDK shared bridge address).
    #[arg(long, env = "BRIDGE_ADDRESS")]
    bridge_address: String,

    /// Bridge-service base URL (for /merkle-proof).
    #[arg(long, env = "BRIDGE_SERVICE_URL")]
    bridge_service_url: String,

    /// Our rollup's agglayer network id (e.g. 1 in kurtosis, 76 on Bali).
    #[arg(long, env = "NETWORK_ID")]
    network_id: u32,

    /// Name of the environment variable holding the sponsor private key. The
    /// key itself is intentionally NOT a flag and is never logged.
    #[arg(long, env = "SPONSOR_KEY_ENV", default_value = "SPONSOR_PRIVATE_KEY")]
    sponsor_key_env: String,

    /// Poll cadence, seconds.
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 10)]
    poll_interval_secs: u64,

    /// Max L2 block span scanned per poll.
    #[arg(long, env = "MAX_RANGE", default_value_t = 10_000)]
    max_range: u64,

    /// Optional explicit start block (overrides the persisted cursor for one boot).
    #[arg(long, env = "START_BLOCK")]
    start_block: Option<u64>,

    /// Path to the sqlite cursor file.
    #[arg(
        long,
        env = "CURSOR_DB",
        default_value = "bridge-autoclaim-cursor.sqlite"
    )]
    cursor_db: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Resolve the sponsor key from the named env var. Never accept it as a flag;
    // never log its value.
    let sponsor_key = std::env::var(&args.sponsor_key_env).map_err(|_| {
        anyhow::anyhow!(
            "sponsor private key env var '{}' is not set (populate it from the secret store)",
            args.sponsor_key_env
        )
    })?;
    if sponsor_key.trim().is_empty() {
        anyhow::bail!(
            "sponsor private key env var '{}' is empty",
            args.sponsor_key_env
        );
    }

    let bridge_address = args
        .bridge_address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bridge-address '{}': {e}", args.bridge_address))?;

    let cfg = ClaimerConfig {
        l2_rpc_url: args.l2_rpc_url,
        l1_rpc_url: args.l1_rpc_url,
        bridge_address,
        bridge_service_url: args.bridge_service_url,
        network_id: args.network_id,
        sponsor_key,
        poll_interval: Duration::from_secs(args.poll_interval_secs),
        max_range: args.max_range,
        start_block: args.start_block,
        cursor_db_path: args.cursor_db,
    };

    run(cfg).await
}
