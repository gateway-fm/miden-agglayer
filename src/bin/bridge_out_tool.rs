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
use miden_client::RemoteTransactionProver;
use miden_client::asset::{Asset, AssetCallbackFlag, FungibleAsset};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::NoteAssets;
use miden_client::transaction::{TransactionProver, TransactionRequestBuilder};
use miden_client::{ClientError, DebugMode};
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

    /// Wallet account ID (hex, e.g. 0x...). Required unless --create-wallet.
    #[arg(long)]
    wallet_id: Option<String>,

    /// Bridge account ID (hex, e.g. 0x...). Required unless --create-wallet.
    #[arg(long)]
    bridge_id: Option<String>,

    /// Faucet account ID (hex, e.g. 0x...). Required unless --create-wallet.
    #[arg(long)]
    faucet_id: Option<String>,

    /// Amount to bridge out (in Miden token units). Ignored with --create-wallet.
    #[arg(long, default_value_t = 0)]
    amount: u64,

    /// Provision mode: create a fresh INDEPENDENT wallet in --store-dir (its own
    /// store.sqlite3 + keystore, separate from the proxy), register its P2ID note
    /// tag, print its id, and exit. Fund this wallet via L1→L2 to its address,
    /// then run bridge-outs against the same --store-dir. This mirrors production,
    /// where the B2AGG wallet is fully independent of the proxy's store.
    #[arg(long)]
    create_wallet: bool,

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

    /// Print the canonical note script roots (b2agg/claim/ger) as ground truth
    /// for external verifiers (e.g. scripts/verify-event-completeness.sh) and
    /// exit. Pure print — needs no node, store, or accounts.
    #[arg(long)]
    print_script_roots: bool,

    /// Injection mode for the reconciler private-note e2e (0.15.5 hotfix):
    /// create + submit a PRIVATE P2ID note from the isolated wallet to itself
    /// (zero assets) with the DEFAULT note tag (0) — the same tag-0 family the
    /// proxy's note-visibility reconciler sweeps via `sync_notes(tags={0})`.
    /// Only the note id + metadata land on-chain (the details stay private),
    /// so the reconciler's `import_notes(NoteFile::NoteId)` fails with
    /// "Incomplete imported note is private" — pre-hotfix that wedged the whole
    /// sweep window forever. Prints the note id + commit block and exits.
    /// Requires --wallet-id.
    #[arg(long)]
    send_private_note: bool,

    /// After submitting the B2AGG, wait until the note is actually CONSUMED
    /// on-chain (polling sync), up to this many seconds. 0 disables the wait
    /// (legacy behaviour: 5 blind sync cycles). Load tests MUST wait: pacing on
    /// tx acceptance alone floods the bridge with in-flight consumptions.
    #[arg(long, env = "B2AGG_WAIT_CONSUMED_SECS", default_value_t = 180)]
    wait_consumed_secs: u64,

    /// Foreign-deployment provision mode (claim-provenance e2e): stand up a
    /// SECOND, fully independent miden-agglayer deployment on the SAME chain —
    /// foreign service + ger_manager wallets, a foreign bridge account
    /// (`create_bridge_account`, deployed via dummy txn like init.rs), and a
    /// foreign ETH faucet registered in the FOREIGN bridge's token registry
    /// (required for its MASM claim path). Keys live in THIS isolated store.
    /// Prints the four account ids and exits. Mirrors the real-testnet
    /// topology where a foreign deployment's claims leaked into our reindex.
    #[arg(long)]
    create_foreign_bridge: bool,

    /// AggLayer network id the foreign bridge is created with (its MASM
    /// rejects claims whose leaf destinationNetwork differs). Use an id our
    /// own stack does NOT serve (default 2; ours is 1) so the foreign-destined
    /// deposit is never auto-claimed by our aggkit.
    #[arg(long, default_value_t = 2)]
    foreign_network_id: u32,

    /// Foreign-claim mode (claim-provenance e2e): read `claimAsset` calldata
    /// from --claim-calldata-file, inject its GER (keccak(mainnetExitRoot ||
    /// rollupExitRoot)) into the FOREIGN bridge via an UpdateGerNote from the
    /// foreign ger_manager, then build the CLAIM note exactly like
    /// `claim.rs::create_claim` (shared `claim_storage_from_call`) targeting
    /// the FOREIGN bridge, submit it from the foreign service account, and
    /// wait for the foreign bridge to consume it. Prints the note id,
    /// details-commitment and canonical global index.
    #[arg(long)]
    submit_foreign_claim: bool,

    /// Hex file (with or without 0x) containing full `claimAsset` calldata
    /// (selector + ABI args). Required with --submit-foreign-claim.
    #[arg(long)]
    claim_calldata_file: Option<PathBuf>,

    /// Foreign bridge account id. Required with --submit-foreign-claim.
    #[arg(long)]
    foreign_bridge_id: Option<String>,

    /// Foreign service account id (mints the CLAIM — the foreign deployment's
    /// `accounts.service` analogue). Required with --submit-foreign-claim.
    #[arg(long)]
    foreign_service_id: Option<String>,

    /// Foreign ger_manager account id (mints the UpdateGerNote). Required
    /// with --submit-foreign-claim.
    #[arg(long)]
    foreign_ger_manager_id: Option<String>,

    /// `origin_token_decimals - miden_decimals` of the faucet the foreign
    /// bridge has registered for the claimed token (10 for the standard
    /// 18→8 ETH faucet created by --create-foreign-bridge).
    #[arg(long, default_value_t = 10)]
    scale_exp: u32,
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
            .field("send_private_note", &self.send_private_note)
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

/// Sync the client to the node tip, surviving transient per-request RPC
/// failures — the fix for the deterministic-in-suite `e2e-claim-provenance`
/// failure (task #26: 7/7 cert runs died at `--create-foreign-bridge` on a
/// single unretried `sync_state()`).
///
/// Why a progress gate instead of a fixed retry count: `sync_state()` loops
/// internally in bounded steps (one gRPC request per step, each under the
/// tool's 10s per-request deadline) and PERSISTS partial progress in the
/// client store — a failed call resumes where it left off, not from genesis.
/// So the correct wait condition is "keep going while the sync height still
/// advances between attempts"; only K consecutive attempts with zero forward
/// progress indicate a genuine stall (node down / unreachable) worth failing
/// on. A fixed count would give up mid-catch-up on a long chain even though
/// every attempt was making progress.
///
/// Errors are printed with `{e:?}` deliberately: `ClientError`'s Display is
/// the bare string "RPC error" (miden-client 0.15 `errors.rs`), which is what
/// left the suite failure undiagnosable — the gRPC status (DeadlineExceeded /
/// ResourceExhausted / Unavailable, each with retry guidance) lives in the
/// source chain that only Debug formatting surfaces.
async fn sync_with_retry(
    client: &mut miden_agglayer_service::miden_client::MidenClientLib,
    label: &str,
) -> anyhow::Result<()> {
    const MAX_STALLED: u32 = 5;
    const RETRY_DELAY_SECS: u64 = 3;
    let mut last_height: Option<u64> = None;
    let mut stalled: u32 = 0;
    loop {
        let err = match client.sync_state().await {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        let height = client.get_sync_height().await.ok().map(|h| h.as_u64());
        let progressed = matches!((last_height, height), (Some(prev), Some(now)) if now > prev);
        // First successfully-read height establishes the BASELINE — that
        // attempt is not a stall datapoint (nothing to compare against). A
        // failed height read PRESERVES the previous baseline (overwriting it
        // with None would blind progress detection on every later attempt)
        // and counts toward the stall window fail-closed.
        let baseline_established = last_height.is_none() && height.is_some();
        if height.is_some() {
            last_height = height;
        }
        if progressed || baseline_established {
            // Forward progress (or first baseline) — this attempt does NOT
            // count toward the stall window; fully reset the counter.
            stalled = 0;
        } else {
            stalled += 1;
        }
        if stalled >= MAX_STALLED {
            return Err(anyhow!(
                "[{label}] sync stalled: {MAX_STALLED} consecutive attempts without \
                 progress (sync height {height:?}); last error: {err:?}"
            ));
        }
        eprintln!(
            "[{label}] sync attempt failed at height {height:?} \
             ({stalled}/{MAX_STALLED} without progress), retrying in {RETRY_DELAY_SECS}s: {err:?}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(RETRY_DELAY_SECS)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.print_script_roots {
        // Keep key=0x<hex> stable — verify-event-completeness.sh parses it.
        println!(
            "b2agg=0x{}",
            hex::encode(miden_base_agglayer::B2AggNote::script_root().as_bytes())
        );
        println!(
            "claim=0x{}",
            hex::encode(miden_base_agglayer::ClaimNote::script().root().as_bytes())
        );
        println!(
            "ger=0x{}",
            hex::encode(miden_base_agglayer::UpdateGerNote::script_root().as_bytes())
        );
        return Ok(());
    }

    let store_path = args.store_dir.join("store.sqlite3");
    let keystore_path = args.store_dir.join("keystore");

    if args.create_wallet || args.create_foreign_bridge {
        // Provision modes: the store/keystore may not exist yet — create them.
        std::fs::create_dir_all(&keystore_path)
            .with_context(|| format!("creating keystore dir {}", keystore_path.display()))?;
    } else {
        if !store_path.exists() {
            return Err(anyhow!("store not found at {}", store_path.display()));
        }
        if !keystore_path.exists() {
            return Err(anyhow!("keystore not found at {}", keystore_path.display()));
        }
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

    // Build miden client from the store (existing for bridge-out, freshly
    // created for --create-wallet).
    let keystore = Arc::new(FilesystemKeyStore::new(keystore_path)?);
    let rpc = miden_agglayer_service::miden_client::build_rpc_client(
        &node_endpoint,
        10_000,
        args.miden_api_key.as_deref(),
    );
    miden_agglayer_service::sqlite_pragmas::open_store_connection(&store_path)
        .with_context(|| format!("failed to configure sqlite store {}", store_path.display()))?;
    let mut builder = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store_path.clone())
        .authenticator(keystore.clone())
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
        .map_err(|e| anyhow!("failed to build miden client: {e:?}"))?;

    // ── Provision mode ────────────────────────────────────────────────────────
    // Create a fully independent bridge-out wallet in THIS store (separate from
    // the proxy's store.sqlite3), print its id, and exit.
    if args.create_wallet {
        println!(
            "[create-wallet] provisioning independent wallet in {}",
            store_path.display()
        );
        sync_with_retry(&mut client, "create-wallet").await?;
        let wallet =
            miden_agglayer_service::init::create_standalone_wallet(&mut client, keystore.clone())
                .await
                .map_err(|e| anyhow!("wallet creation failed: {e:?}"))?;
        // Settle the new account on the node before it can receive deposits.
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
        }
        println!("[create-wallet] wallet-id: {}", wallet.id().to_hex());
        println!("[create-wallet] done");
        return Ok(());
    }

    // ── Foreign-deployment provision mode (claim-provenance e2e) ─────────────
    // Stand up a SECOND independent miden-agglayer deployment on the same
    // chain, mirroring init.rs::add_accounts: service + ger_manager wallets,
    // bridge account, ETH faucet registered in the FOREIGN bridge. All keys
    // live in this isolated store — the proxy's store is never touched.
    if args.create_foreign_bridge {
        use miden_agglayer_service::miden_client::{
            submit_new_transaction, wait_for_transaction_commit,
        };
        use miden_base_agglayer::{MetadataHash, create_bridge_account};
        use miden_client::crypto::FeltRng;

        println!(
            "[foreign-bridge] provisioning independent deployment in {} (network id {})",
            store_path.display(),
            args.foreign_network_id
        );
        sync_with_retry(&mut client, "foreign-bridge").await?;

        let service =
            miden_agglayer_service::init::create_standalone_wallet(&mut client, keystore.clone())
                .await
                .map_err(|e| anyhow!("foreign service wallet creation failed: {e:?}"))?;
        let ger_manager =
            miden_agglayer_service::init::create_standalone_wallet(&mut client, keystore.clone())
                .await
                .map_err(|e| anyhow!("foreign ger_manager wallet creation failed: {e:?}"))?;

        // Deploy ger_manager via dummy txn (mirrors init.rs::deploy_account).
        let dummy = TransactionRequestBuilder::new().build()?;
        let txn_id = submit_new_transaction(&mut client, ger_manager.id(), dummy)
            .await
            .map_err(|e| anyhow!("foreign ger_manager deploy failed: {e:?}"))?;
        wait_for_transaction_commit(&mut client, txn_id, 30, std::time::Duration::from_secs(2))
            .await
            .map_err(|e| anyhow!("foreign ger_manager deploy commit wait failed: {e:?}"))?;

        // Foreign bridge (mirrors init.rs::add_bridge).
        let bridge = create_bridge_account(
            client.rng().draw_word(),
            service.id(),
            ger_manager.id(),
            args.foreign_network_id,
        );
        client
            .add_account(&bridge, false)
            .await
            .map_err(|e| anyhow!("adding foreign bridge account failed: {e:?}"))?;
        let dummy = TransactionRequestBuilder::new().build()?;
        let txn_id = submit_new_transaction(&mut client, bridge.id(), dummy)
            .await
            .map_err(|e| anyhow!("foreign bridge deploy failed: {e:?}"))?;
        wait_for_transaction_commit(&mut client, txn_id, 30, std::time::Duration::from_secs(2))
            .await
            .map_err(|e| anyhow!("foreign bridge deploy commit wait failed: {e:?}"))?;

        // Settle accounts before the faucet registration note targets the bridge
        // (mirrors init.rs's NTX-builder settlement wait).
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
        }

        // Foreign ETH faucet, registered in the FOREIGN bridge's on-chain
        // token registry — without it the foreign bridge's MASM claim path
        // panics on the (origin_token, origin_network) registry lookup and the
        // CLAIM is never consumed. Same parameters as init.rs (18→8, scale 10).
        let faucet = miden_agglayer_service::faucet_ops::create_and_register_faucet(
            &mut client,
            "ETH",
            8,
            &[0u8; 20],
            0,
            10,
            service.id(),
            bridge.id(),
            MetadataHash::from_abi_encoded(&[]),
        )
        .await
        .map_err(|e| anyhow!("foreign faucet creation/registration failed: {e:?}"))?;
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
        }

        // Stable, machine-parseable output — e2e-claim-provenance.sh greps these.
        println!("[foreign-bridge] service-id: {}", service.id().to_hex());
        println!(
            "[foreign-bridge] ger-manager-id: {}",
            ger_manager.id().to_hex()
        );
        println!("[foreign-bridge] bridge-id: {}", bridge.id().to_hex());
        println!("[foreign-bridge] faucet-id: {}", faucet.id().to_hex());
        println!("[foreign-bridge] network-id: {}", args.foreign_network_id);
        println!("[foreign-bridge] done");
        return Ok(());
    }

    // ── Foreign-claim mode (claim-provenance e2e) ─────────────────────────────
    // Drive ONE claim through the FOREIGN bridge: inject its GER, then submit a
    // CLAIM note (built by the same `claim_storage_from_call` the proxy's claim
    // path uses) targeting the foreign bridge, and wait for consumption.
    if args.submit_foreign_claim {
        use alloy_core::sol_types::SolCall;
        use miden_agglayer_service::claim::{claim_storage_from_call, claimAssetCall};
        use miden_agglayer_service::miden_client::submit_new_transaction;
        use miden_base_agglayer::{ClaimNote, ExitRoot, UpdateGerNote};
        use miden_client::store::NoteFilter;

        let bridge_id = parse_account_id(
            args.foreign_bridge_id
                .as_deref()
                .context("--foreign-bridge-id is required with --submit-foreign-claim")?,
        )
        .context("foreign-bridge-id")?;
        let service_id = parse_account_id(
            args.foreign_service_id
                .as_deref()
                .context("--foreign-service-id is required with --submit-foreign-claim")?,
        )
        .context("foreign-service-id")?;
        let ger_manager_id = parse_account_id(
            args.foreign_ger_manager_id
                .as_deref()
                .context("--foreign-ger-manager-id is required with --submit-foreign-claim")?,
        )
        .context("foreign-ger-manager-id")?;
        let calldata_path = args
            .claim_calldata_file
            .as_ref()
            .context("--claim-calldata-file is required with --submit-foreign-claim")?;

        let calldata_hex = std::fs::read_to_string(calldata_path)
            .with_context(|| format!("reading {}", calldata_path.display()))?;
        let calldata = hex::decode(calldata_hex.trim().trim_start_matches("0x"))
            .context("claim calldata is not valid hex")?;
        let call = claimAssetCall::abi_decode(&calldata).context("decoding claimAsset calldata")?;
        println!(
            "[foreign-claim] decoded claimAsset: origin_network={} dest_network={} amount={}",
            call.originNetwork, call.destinationNetwork, call.amount
        );

        sync_with_retry(&mut client, "foreign-claim").await?;

        // Wait for an OUTPUT note (by id) to be consumed on-chain, mirroring
        // the B2AGG wait_consumed loop below.
        async fn wait_output_consumed(
            client: &mut miden_agglayer_service::miden_client::MidenClientLib,
            note_id: miden_protocol::note::NoteId,
            what: &str,
            secs: u64,
        ) -> anyhow::Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Err(e) = client.sync_state().await {
                    eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
                }
                let recs = client
                    .get_output_notes(NoteFilter::Consumed)
                    .await
                    .unwrap_or_default();
                if recs.iter().any(|r| r.id() == note_id) {
                    return Ok(());
                }
            }
            Err(anyhow!("{what} note {note_id} not consumed within {secs}s"))
        }

        // 1. Inject the claim's GER into the FOREIGN bridge (the foreign
        //    ger_manager is its authorized GER sender). The bridge's MASM
        //    claim path recomputes keccak(mainnetExitRoot || rollupExitRoot)
        //    and requires it in the bridge's GER map.
        let ger_bytes = miden_agglayer_service::ger::combined_ger(
            &call.mainnetExitRoot.0,
            &call.rollupExitRoot.0,
        );
        let ger_note = UpdateGerNote::create(
            ExitRoot::new(ger_bytes),
            ger_manager_id,
            bridge_id,
            client.rng(),
        )?;
        let ger_note_id = ger_note.id();
        println!(
            "[foreign-claim] injecting GER 0x{} into foreign bridge (note {ger_note_id})",
            hex::encode(ger_bytes)
        );
        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![ger_note])
            .build()?;
        let txn_id = submit_new_transaction(&mut client, ger_manager_id, tx_request)
            .await
            .map_err(|e| anyhow!("foreign GER inject submit failed: {e:?}"))?;
        println!("[foreign-claim] GER inject transaction submitted: {txn_id}");
        wait_output_consumed(&mut client, ger_note_id, "foreign UpdateGer", 240).await?;
        println!("[foreign-claim] GER injected (UpdateGerNote consumed by foreign bridge)");

        // 2. Build + submit the CLAIM note against the FOREIGN bridge — the
        //    same storage the proxy's create_claim would build, minted by the
        //    FOREIGN service and targeting the FOREIGN bridge.
        let storage = claim_storage_from_call(&call, args.scale_exp)?;
        let note = ClaimNote::create(storage, bridge_id, service_id, client.rng())?;
        let note_id = note.id();
        let note_commitment = hex::encode(
            miden_protocol::note::NoteDetails::from(&note)
                .commitment()
                .as_bytes(),
        );
        // Canonical global index — what a projector would record in a
        // ClaimEvent for this note (mirrors build_canonical_proof_data:
        // mainnet rollup-index bytes zeroed).
        let mut gi = call.globalIndex.to_be_bytes::<32>();
        let mainnet_flag = u32::from_be_bytes([gi[20], gi[21], gi[22], gi[23]]);
        if mainnet_flag == 1 {
            gi[24..28].fill(0);
        }
        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![note])
            .build()?;
        let txn_id = submit_new_transaction(&mut client, service_id, tx_request)
            .await
            .map_err(|e| anyhow!("foreign CLAIM submit failed: {e:?}"))?;
        println!("[foreign-claim] CLAIM transaction submitted: {txn_id}");
        wait_output_consumed(&mut client, note_id, "foreign CLAIM", 300).await?;

        // Stable, machine-parseable output — e2e-claim-provenance.sh greps these.
        println!("[foreign-claim] note-id: {note_id}");
        println!("[foreign-claim] note-commitment: {note_commitment}");
        println!("[foreign-claim] global-index: 0x{}", hex::encode(gi));
        println!("[foreign-claim] done");
        return Ok(());
    }

    // ── Private-note injection mode ───────────────────────────────────────────
    // Reconciler private-note e2e (0.15.5 hotfix): submit a PRIVATE, tag-0,
    // zero-asset P2ID note to the wallet itself. Its id lands on-chain in the
    // tag-0 family the reconciler sweeps, but the details are never published,
    // so the reconciler's import-by-id can never succeed — the exact prod shape
    // that wedged the retroactive-heal sweep pre-hotfix.
    if args.send_private_note {
        use miden_client::crypto::FeltRng;
        use miden_client::note::Note;
        use miden_protocol::note::{NoteType, PartialNoteMetadata};
        use miden_standards::note::P2idNoteStorage;

        let wallet_id = parse_account_id(
            args.wallet_id
                .as_deref()
                .context("--wallet-id is required with --send-private-note")?,
        )
        .context("wallet-id")?;
        println!("[private-note] wallet: {wallet_id}");

        println!("[private-note] syncing state...");
        sync_with_retry(&mut client, "private-note").await?;

        // P2ID recipient targeting the wallet itself; PRIVATE note type; note
        // tag left at the default (0) so `sync_notes(tags={0})` lists it — the
        // same default tag B2AGG bridge-out notes carry (PartialNoteMetadata
        // defaults the tag; B2AggNote::create never overrides it).
        let serial_num = client.rng().draw_word();
        let recipient = P2idNoteStorage::new(wallet_id).into_recipient(serial_num);
        let metadata = PartialNoteMetadata::new(wallet_id, NoteType::Private);
        let note = Note::new(
            NoteAssets::new(vec![]).map_err(|e| anyhow!("note assets: {e}"))?,
            metadata,
            recipient,
        );
        let note_id = note.id();
        let tag: u32 = note.metadata().tag().into();
        println!("[private-note] note created: {note_id} (tag {tag}, type Private)");

        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![note])
            .build()
            .map_err(|e| anyhow!("tx request build failed: {e}"))?;

        // Bounded submit retry — same prover-backpressure rationale as the
        // bridge-out path below.
        const ATTEMPTS: u32 = 4;
        let mut tx_id = None;
        for attempt in 1..=ATTEMPTS {
            println!("[private-note] submitting transaction (attempt {attempt}/{ATTEMPTS})...");
            match client
                .submit_new_transaction(wallet_id, tx_request.clone())
                .await
            {
                Ok(id) => {
                    tx_id = Some(id);
                    break;
                }
                Err(e) if attempt < ATTEMPTS => {
                    eprintln!(
                        "[private-note] submit attempt {attempt} failed: {e:?}; retrying in 10s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                Err(e) => return Err(anyhow!("submit failed after {ATTEMPTS} attempts: {e}")),
            }
        }
        let tx_id = tx_id.expect("loop either sets tx_id or returns");
        println!("[private-note] transaction submitted: {tx_id}");

        // Wait until the note is COMMITTED on-chain and report its block.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut commit_block: Option<u32> = None;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
            let recs = client
                .get_output_notes(miden_client::store::NoteFilter::Committed)
                .await
                .unwrap_or_default();
            if let Some(rec) = recs.iter().find(|r| r.id() == note_id) {
                commit_block = rec
                    .inclusion_proof()
                    .map(|p| p.location().block_num().as_u32());
                break;
            }
        }
        let commit_block =
            commit_block.ok_or_else(|| anyhow!("private note {note_id} not committed in 180s"))?;

        // Stable, machine-parseable output — the e2e script greps these.
        println!("[private-note] note-id: {note_id}");
        println!("[private-note] tag: {tag}");
        println!("[private-note] commit-block: {commit_block}");
        println!("[private-note] done");
        return Ok(());
    }

    // Bridge-out mode: the account ids are required.
    let wallet_id = parse_account_id(
        args.wallet_id
            .as_deref()
            .context("--wallet-id is required unless --create-wallet")?,
    )
    .context("wallet-id")?;
    let bridge_id = parse_account_id(
        args.bridge_id
            .as_deref()
            .context("--bridge-id is required")?,
    )
    .context("bridge-id")?;
    let faucet_id = parse_account_id(
        args.faucet_id
            .as_deref()
            .context("--faucet-id is required")?,
    )
    .context("faucet-id")?;

    println!("[bridge-out] wallet:  {wallet_id}");
    println!("[bridge-out] bridge:  {bridge_id}");
    println!("[bridge-out] faucet:  {faucet_id}");
    println!("[bridge-out] amount:  {}", args.amount);
    println!(
        "[bridge-out] dest:    {} (network {})",
        args.dest_address, args.dest_network
    );

    // Sync state — retry on transient errors (concurrent SQLite access with
    // the running service can cause "failed to convert note record" errors;
    // node RPC under suite load returns transient gRPC failures).
    println!("[bridge-out] syncing state...");
    sync_with_retry(&mut client, "bridge-out").await?;
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
                .filter_map(|(rec, _)| match rec.try_into() {
                    Ok(n) => Some(n),
                    Err(e) => {
                        eprintln!("[bridge-out] SKIPPING unconvertible note record (was silently dropped pre-#128): {e:?}");
                        None
                    }
                })
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
                                    if let Err(e) = client.sync_state().await {
                                        eprintln!(
                                            "[settle] sync failed (non-fatal, retried next tick): {e:?}"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                println!("[bridge-out] consume failed: {e:?}");
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
                        println!("[bridge-out] build consume req failed: {e:?}");
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
        .map_err(|e| anyhow!("failed to get balance: {e:?}"))?;
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

    // Create B2AGG note.
    //
    // Protocol 0.15 introduced per-asset callbacks: the asset vault is keyed by
    // (faucet_id, callback_flag), and AggLayer faucets are registered with
    // callbacks ENABLED. The bridged-in assets therefore sit in the wallet vault
    // under the callbacks-enabled key. `FungibleAsset::new` defaults to
    // callbacks DISABLED, so a default-flag asset addresses a different (empty)
    // vault slot and the bridge-out tx fails with "amount in the vault is less
    // than the amount to remove". Match the vault by enabling callbacks.
    let asset: Asset = FungibleAsset::new(faucet_id, args.amount)
        .map_err(|e| anyhow!("invalid asset: {e}"))?
        .with_callbacks(AssetCallbackFlag::Enabled)
        .into();
    let note_assets = NoteAssets::new(vec![asset]).map_err(|e| anyhow!("note assets: {e}"))?;

    // Re-import bridge account so the NoteScreener has the latest asset tree.
    // Without this, submit_new_transaction fails with FetchAssetWitnessFailed
    // after CLAIM modified the bridge account.
    if let Err(e) = client.import_account_by_id(bridge_id).await {
        eprintln!("[bridge-out] bridge re-import: {e} (may already be tracked)");
    }

    // Create + submit, with a bounded retry. The remote tx-prover applies
    // backpressure with a bounded queue: when the ntx-builder is proving
    // network txs (bridge consumptions) at the same moment, our Prove request
    // can be rejected with RESOURCE_EXHAUSTED ("proof queue is full"), which
    // surfaces as "transaction proving failed". Proving happens BEFORE node
    // submission, so retrying is clean. If the node accepts the tx and only the
    // local store update fails, never resubmit; retry the attached store update.
    const SUBMIT_ATTEMPTS: u32 = 4;
    let mut tx_id = None;
    let mut submitted_note_id = None;
    for attempt in 1..=SUBMIT_ATTEMPTS {
        // Sync right before each attempt to minimize the window where the
        // service's background sync loop can change our shared SQLite state.
        sync_with_retry(&mut client, "bridge-out pre-submit").await?;

        let b2agg = B2AggNote::create(
            args.dest_network,
            l1_dest,
            note_assets.clone(),
            bridge_id,
            wallet_id,
            client.rng(),
        )
        .map_err(|e| anyhow!("B2AGG creation failed: {e:?}"))?;
        let b2agg_note_id = b2agg.id();
        println!("[bridge-out] B2AGG note created: {b2agg_note_id}");

        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![b2agg])
            .build()
            .map_err(|e| anyhow!("tx request build failed: {e}"))?;

        println!("[bridge-out] submitting transaction (attempt {attempt}/{SUBMIT_ATTEMPTS})...");
        match miden_agglayer_service::metrics::meter_proof(
            miden_agglayer_service::metrics::ProofKind::BridgeOut,
            client.submit_new_transaction(wallet_id, tx_request),
        )
        .await
        {
            Ok(id) => {
                tx_id = Some(id);
                submitted_note_id = Some(b2agg_note_id);
                break;
            }
            Err(ClientError::ApplyTransactionAfterSubmitFailed {
                pending_update,
                source,
            }) => {
                let accepted_tx = pending_update.executed_transaction().id();
                let submission_height = pending_update.submission_height();
                eprintln!(
                    "[bridge-out] transaction {accepted_tx} was accepted at block \
                     {submission_height}, but local store update failed: {source:#}; \
                     re-applying the attached update"
                );
                let pending_update = *pending_update;
                let mut last_reapply_err = None;
                for recovery_attempt in 1..=SUBMIT_ATTEMPTS {
                    match client
                        .apply_transaction_update(pending_update.clone())
                        .await
                    {
                        Ok(()) => {
                            println!(
                                "[bridge-out] recovered local store update for accepted transaction {accepted_tx}"
                            );
                            tx_id = Some(accepted_tx);
                            submitted_note_id = Some(b2agg_note_id);
                            break;
                        }
                        Err(reapply_err) => {
                            eprintln!(
                                "[bridge-out] recovery apply attempt {recovery_attempt}/{SUBMIT_ATTEMPTS} failed for transaction {accepted_tx}: {reapply_err:#}"
                            );
                            last_reapply_err = Some(reapply_err);
                            if recovery_attempt < SUBMIT_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            }
                        }
                    }
                }
                if tx_id.is_some() {
                    break;
                }
                let last_reapply_err = last_reapply_err
                    .map(|err| format!("{err:#}"))
                    .unwrap_or_else(|| "unknown recovery error".to_string());
                return Err(anyhow!(
                    "submit accepted transaction {accepted_tx} at block {submission_height}, but local store recovery failed after {SUBMIT_ATTEMPTS} attempts: {last_reapply_err}"
                ));
            }
            Err(e) if attempt < SUBMIT_ATTEMPTS => {
                eprintln!(
                    "[bridge-out] submit attempt {attempt} failed: {e:?}; retrying in 10s \
                     (prover backpressure is the common cause)"
                );
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
            Err(e) => {
                return Err(anyhow!(
                    "submit failed after {SUBMIT_ATTEMPTS} attempts: {e}"
                ));
            }
        }
    }
    let tx_id = tx_id.expect("loop either sets tx_id or returns");

    println!("[bridge-out] transaction submitted: {tx_id}");

    // Wait for the B2AGG to be CONSUMED on-chain (not merely accepted). Pacing
    // on tx acceptance alone lets a load test flood the bridge with in-flight
    // consumptions faster than downstream indexing absorbs them.
    if args.wait_consumed_secs > 0 {
        let note_id = submitted_note_id.expect("tx_id set implies submitted_note_id set");
        println!(
            "[bridge-out] waiting for B2AGG consumption (note {note_id}, up to {}s)...",
            args.wait_consumed_secs
        );
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(args.wait_consumed_secs);
        let mut consumed = false;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
            let recs = client
                .get_output_notes(miden_client::store::NoteFilter::Consumed)
                .await
                .unwrap_or_default();
            if recs.iter().any(|r| r.id() == note_id) {
                consumed = true;
                break;
            }
        }
        if !consumed {
            return Err(anyhow!(
                "B2AGG note {note_id} not consumed within {}s",
                args.wait_consumed_secs
            ));
        }
        println!("[bridge-out] B2AGG consumed");
    } else {
        // Legacy behaviour: a few blind sync cycles.
        println!("[bridge-out] waiting for confirmation...");
        for i in 1..=5 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if let Err(e) = client.sync_state().await {
                eprintln!("[settle] sync failed (non-fatal, retried next tick): {e:?}");
            }
            println!("[bridge-out] sync cycle {i}/5");
        }
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
