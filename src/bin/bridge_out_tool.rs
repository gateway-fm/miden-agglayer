//! Bridge-out tool — creates and submits a B2AGG note from a wallet.
//!
//! Usage:
//!   bridge-out-tool --store-dir <path> --node-url <url> \
//!     --wallet-id <hex> --bridge-id <hex> --faucet-id <hex> \
//!     --amount <u64> --dest-address <hex> [--dest-network <u32>]
//!
//! The store-dir should be an existing miden-client data directory
//! (created by `miden-client init --local`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use clap::Parser;
use miden_base_agglayer::{B2AggNote, EthAddress};
use miden_client::DebugMode;
use miden_client::RemoteTransactionProver;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::NoteAssets;
use miden_client::transaction::{TransactionProver, TransactionRequestBuilder};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::AccountId;

#[derive(Parser)]
#[command(version, about = "Create and submit a B2AGG bridge-out note")]
struct Args {
    /// Path to the miden-client store directory (containing store.sqlite3 and keystore/)
    #[arg(long)]
    store_dir: PathBuf,

    /// Miden node gRPC URL (e.g. http://localhost:57291)
    #[arg(long)]
    node_url: String,

    /// Wallet account ID (hex, e.g. 0x...)
    #[arg(long)]
    wallet_id: String,

    /// Bridge account ID (hex, e.g. 0x...)
    #[arg(long)]
    bridge_id: String,

    /// Faucet account ID (hex, e.g. 0x...)
    #[arg(long)]
    faucet_id: String,

    /// Amount to bridge out (in Miden token units)
    #[arg(long)]
    amount: u64,

    /// L1 destination address (hex, e.g. 0xabcd...)
    #[arg(long, default_value = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd")]
    dest_address: String,

    /// Destination network (0 = Ethereum L1)
    #[arg(long, default_value_t = 0)]
    dest_network: u32,

    /// Enable Miden VM debug mode (verbose execution traces). Disable in production.
    #[arg(long, env = "MIDEN_DEBUG")]
    miden_debug: bool,

    /// API key sent as `authorization: Bearer <key>` on every outbound Miden gRPC call.
    /// Needed when `--node-url` points at a gateway that rate-limits unauthenticated
    /// traffic. Safe to omit for direct node access.
    #[arg(long, env = "MIDEN_API_KEY")]
    miden_api_key: Option<String>,

    /// gRPC URL of a remote Miden transaction prover (e.g. `http://miden-prover:50051`).
    /// When set, this binary offloads all proof generation to that endpoint instead
    /// of running an in-process `LocalTransactionProver`. Mirrors the proxy flag of
    /// the same name so the same `MIDEN_PROVER_URL` environment variable applies.
    #[arg(long, env = "MIDEN_PROVER_URL")]
    miden_prover_url: Option<String>,

    /// Per-request timeout for the remote Miden prover, in seconds. Default 120s.
    /// Has no effect when --miden-prover-url is unset.
    #[arg(long, env = "MIDEN_PROVER_TIMEOUT_SECS", default_value_t = 120)]
    miden_prover_timeout_secs: u64,
}

impl std::fmt::Debug for Args {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Args")
            .field("store_dir", &self.store_dir)
            .field("node_url", &self.node_url)
            .field("wallet_id", &self.wallet_id)
            .field("bridge_id", &self.bridge_id)
            .field("faucet_id", &self.faucet_id)
            .field("amount", &self.amount)
            .field("dest_address", &self.dest_address)
            .field("dest_network", &self.dest_network)
            .field("miden_debug", &self.miden_debug)
            .field(
                "miden_api_key",
                &self.miden_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "miden_prover_url",
                &self.miden_prover_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("miden_prover_timeout_secs", &self.miden_prover_timeout_secs)
            .finish()
    }
}

fn parse_account_id(s: &str) -> anyhow::Result<AccountId> {
    // Try hex first
    if let Ok(id) = AccountId::from_hex(s) {
        return Ok(id);
    }
    // Try bech32
    if let Ok((_, id)) = AccountId::from_bech32(s) {
        return Ok(id);
    }
    Err(anyhow!("cannot parse account ID: {s}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let wallet_id = parse_account_id(&args.wallet_id).context("wallet-id")?;
    let bridge_id = parse_account_id(&args.bridge_id).context("bridge-id")?;
    let faucet_id = parse_account_id(&args.faucet_id).context("faucet-id")?;

    println!("[bridge-out] wallet:  {wallet_id}");
    println!("[bridge-out] bridge:  {bridge_id}");
    println!("[bridge-out] faucet:  {faucet_id}");
    println!("[bridge-out] amount:  {}", args.amount);
    println!(
        "[bridge-out] dest:    {} (network {})",
        args.dest_address, args.dest_network
    );

    let store_path = args.store_dir.join("store.sqlite3");
    let keystore_path = args.store_dir.join("keystore");

    if !store_path.exists() {
        return Err(anyhow!("store not found at {}", store_path.display()));
    }
    if !keystore_path.exists() {
        return Err(anyhow!("keystore not found at {}", keystore_path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&keystore_path, perms)?;
    }

    // Parse node endpoint via the shared resolver so the `devnet` / `testnet` shortcuts
    // work the same way they do for the main service. Using `Endpoint::try_from` directly
    // would silently reject those shortcuts (the same asymmetry that caused RD-856 on the
    // fresh-client path in `src/claim.rs::publish_claim`).
    let node_endpoint = miden_agglayer_service::miden_client::parse_node_url(&args.node_url)
        .map_err(|e| anyhow!("invalid node URL {}: {e}", args.node_url))?;

    let mode = if args.miden_debug {
        DebugMode::Enabled
    } else {
        DebugMode::Disabled
    };

    // Build miden client from existing store
    let keystore = FilesystemKeyStore::new(keystore_path)?;
    let rpc = miden_agglayer_service::miden_client::build_rpc_client(
        &node_endpoint,
        10_000,
        args.miden_api_key.as_deref(),
    );
    let mut builder = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store_path)
        .authenticator(Arc::new(keystore))
        .in_debug_mode(mode);
    if let Some(prover_url) = args.miden_prover_url.as_deref() {
        // Mirrors the main service wiring: when a remote prover URL is set,
        // offload all proving to it (avoiding the bali OOM cause of in-process
        // LocalTransactionProver). Without this the tool would silently still
        // prove locally even with `MIDEN_PROVER_URL` exported.
        let tx_prover: Arc<dyn TransactionProver + Send + Sync> =
            Arc::new(RemoteTransactionProver::new(prover_url).with_timeout(
                std::time::Duration::from_secs(args.miden_prover_timeout_secs),
            ));
        builder = builder.prover(tx_prover);
        println!(
            "[bridge-out] using remote transaction prover (timeout {}s)",
            args.miden_prover_timeout_secs,
        );
    }
    let mut client = builder
        .build()
        .await
        .map_err(|e| anyhow!("failed to build miden client: {e}"))?;

    // Sync state — retry on transient errors (concurrent SQLite access with
    // the running service can cause "failed to convert note record" errors).
    println!("[bridge-out] syncing state...");
    for sync_attempt in 0..5u32 {
        match client.sync_state().await {
            Ok(_) => break,
            Err(e) if sync_attempt < 4 => {
                eprintln!(
                    "[bridge-out] sync attempt {} failed: {e}, retrying...",
                    sync_attempt + 1
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(anyhow!("sync failed after 5 attempts: {e}")),
        }
    }
    println!("[bridge-out] sync complete");

    // Try to consume any Expected/Committed notes for the wallet
    {
        use miden_client::store::NoteFilter;
        let expected = client
            .get_input_notes(NoteFilter::Expected)
            .await
            .unwrap_or_default();
        let committed = client
            .get_input_notes(NoteFilter::Committed)
            .await
            .unwrap_or_default();
        println!(
            "[bridge-out] notes: {} expected, {} committed",
            expected.len(),
            committed.len()
        );

        let consumable = client
            .get_consumable_notes(Some(wallet_id))
            .await
            .unwrap_or_default();
        if !consumable.is_empty() {
            println!("[bridge-out] consuming {} notes...", consumable.len());
            let notes: Vec<miden_protocol::note::Note> = consumable
                .into_iter()
                .filter_map(|(rec, _)| rec.try_into().ok())
                .collect();
            if !notes.is_empty() {
                match TransactionRequestBuilder::new().build_consume_notes(notes) {
                    Ok(req) => {
                        match miden_agglayer_service::metrics::meter_proof(
                            miden_agglayer_service::metrics::ProofKind::BridgeOut,
                            client.submit_new_transaction(wallet_id, req),
                        )
                        .await
                        {
                            Ok(tx) => {
                                println!("[bridge-out] consumed notes: {tx}");
                                for _ in 0..10 {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    client.sync_state().await.ok();
                                }
                            }
                            Err(e) => {
                                println!("[bridge-out] consume failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        // Pre-prove build failure — no histogram (duration is
                        // meaningless before any proving started). Record the
                        // distinct `build_failed` outcome so dashboards can
                        // split prover failures from request-construction
                        // failures.
                        miden_agglayer_service::metrics::record_proof_outcome(
                            miden_agglayer_service::metrics::ProofKind::BridgeOut,
                            miden_agglayer_service::metrics::ProofOutcome::BuildFailed,
                        );
                        println!("[bridge-out] build consume req failed: {e}");
                    }
                }
            }
        }
    }

    // Check wallet balance
    let balance = client
        .account_reader(wallet_id)
        .get_balance(faucet_id)
        .await
        .map_err(|e| anyhow!("failed to get balance: {e}"))?;
    println!("[bridge-out] wallet balance: {balance}");

    if balance < args.amount {
        return Err(anyhow!(
            "insufficient balance: have {balance}, need {}",
            args.amount
        ));
    }

    // Parse destination address
    let l1_dest = EthAddress::from_hex(&args.dest_address)
        .map_err(|e| anyhow!("invalid dest address: {e}"))?;

    // Create B2AGG note
    let asset: Asset = FungibleAsset::new(faucet_id, args.amount)
        .map_err(|e| anyhow!("invalid asset: {e}"))?
        .into();
    let note_assets = NoteAssets::new(vec![asset]).map_err(|e| anyhow!("note assets: {e}"))?;

    let b2agg = B2AggNote::create(
        args.dest_network,
        l1_dest,
        note_assets,
        bridge_id,
        wallet_id,
        client.rng(),
    )
    .map_err(|e| anyhow!("B2AGG creation failed: {e}"))?;

    println!("[bridge-out] B2AGG note created");

    // Re-import bridge account so the NoteScreener has the latest asset tree.
    // Without this, submit_new_transaction fails with FetchAssetWitnessFailed
    // after CLAIM modified the bridge account.
    if let Err(e) = client.import_account_by_id(bridge_id).await {
        eprintln!("[bridge-out] bridge re-import: {e} (may already be tracked)");
    }

    // Final sync right before submit to minimize the window where the service's
    // background sync loop can change our shared SQLite state.
    client
        .sync_state()
        .await
        .map_err(|e| anyhow!("pre-submit sync failed: {e}"))?;

    // Submit transaction
    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![b2agg])
        .build()
        .map_err(|e| anyhow!("tx request build failed: {e}"))?;

    println!("[bridge-out] submitting transaction...");
    let tx_id = miden_agglayer_service::metrics::meter_proof(
        miden_agglayer_service::metrics::ProofKind::BridgeOut,
        client.submit_new_transaction(wallet_id, tx_request),
    )
    .await
    .map_err(|e| anyhow!("submit failed: {e}"))?;

    println!("[bridge-out] transaction submitted: {tx_id}");

    // Wait a couple of sync cycles for the note to propagate
    println!("[bridge-out] waiting for confirmation...");
    for i in 1..=5 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        client.sync_state().await.ok();
        println!("[bridge-out] sync cycle {i}/5");
    }

    let new_balance = client
        .account_reader(wallet_id)
        .get_balance(faucet_id)
        .await
        .unwrap_or(0);
    println!("[bridge-out] wallet balance after: {new_balance}");
    println!("[bridge-out] done");

    Ok(())
}
