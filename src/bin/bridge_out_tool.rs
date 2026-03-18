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
use miden_base_agglayer::{B2AggNote, EthAddressFormat};
use miden_client::DebugMode;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::NoteAssets;
use miden_client::rpc::Endpoint;
use miden_client::transaction::{OutputNote, TransactionRequestBuilder};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::AccountId;

#[derive(Parser, Debug)]
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

    // Parse node endpoint
    let node_endpoint =
        Endpoint::try_from(args.node_url.as_str()).map_err(|e| anyhow!("invalid node URL: {e}"))?;

    // Build miden client from existing store
    let keystore = FilesystemKeyStore::new(keystore_path)?;
    let mut client = ClientBuilder::new()
        .grpc_client(&node_endpoint, Some(10_000))
        .sqlite_store(store_path)
        .authenticator(Arc::new(keystore))
        .in_debug_mode(DebugMode::Enabled)
        .build()
        .await
        .map_err(|e| anyhow!("failed to build miden client: {e}"))?;

    // Sync state
    println!("[bridge-out] syncing state...");
    client
        .sync_state()
        .await
        .map_err(|e| anyhow!("sync failed: {e}"))?;
    println!("[bridge-out] sync complete");

    // Try to consume any Expected/Committed notes for the wallet
    {
        use miden_client::store::NoteFilter;
        // Check for Expected notes (synced but not yet consumed)
        let expected = client.get_input_notes(NoteFilter::Expected).await.unwrap_or_default();
        let committed = client.get_input_notes(NoteFilter::Committed).await.unwrap_or_default();
        println!("[bridge-out] notes: {} expected, {} committed", expected.len(), committed.len());

        // Try consuming committed notes first (standard path)
        let consumable = client.get_consumable_notes(Some(wallet_id)).await.unwrap_or_default();
        if !consumable.is_empty() {
            println!("[bridge-out] consuming {} notes...", consumable.len());
            let notes: Vec<miden_protocol::note::Note> = consumable
                .into_iter()
                .filter_map(|(rec, _)| rec.try_into().ok())
                .collect();
            if !notes.is_empty() {
                match TransactionRequestBuilder::new()
                    .build_consume_notes(notes)
                {
                    Ok(req) => {
                        match client.submit_new_transaction(wallet_id, req).await {
                            Ok(tx) => {
                                println!("[bridge-out] consumed notes: {tx}");
                                // Wait for commit
                                for _ in 0..10 {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    client.sync_state().await.ok();
                                }
                            }
                            Err(e) => println!("[bridge-out] consume failed: {e}"),
                        }
                    }
                    Err(e) => println!("[bridge-out] build consume req failed: {e}"),
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
    let l1_dest = EthAddressFormat::from_hex(&args.dest_address)
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

    // Submit transaction
    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(b2agg)])
        .build()
        .map_err(|e| anyhow!("tx request build failed: {e}"))?;

    println!("[bridge-out] submitting transaction...");
    let tx_id = client
        .submit_new_transaction(wallet_id, tx_request)
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
