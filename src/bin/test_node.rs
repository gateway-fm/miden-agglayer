/*
use std::path::PathBuf;
use std::time::Duration;
use std::{env, fs, process};

use node_builder::{DEFAULT_BATCH_INTERVAL, DEFAULT_BLOCK_INTERVAL, DEFAULT_RPC_PORT, NodeBuilder};

fn default_data_dir() -> PathBuf {
    let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
    let base_dir = env::home_dir().unwrap_or(current_dir);
    base_dir.join(".miden").join("node-data")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = env::args().nth(1).map(PathBuf::from).unwrap_or(default_data_dir());
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir)?;
    }
    fs::create_dir_all(&data_dir)?;

    let builder = NodeBuilder::new(data_dir)
        .with_rpc_port(DEFAULT_RPC_PORT)
        .with_block_interval(Duration::from_millis(DEFAULT_BLOCK_INTERVAL))
        .with_batch_interval(Duration::from_millis(DEFAULT_BATCH_INTERVAL));

    let handle = builder.start().await?;
    println!("Node started successfully with PID: {}", process::id());

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    handle.stop().await?;
    println!("Node stopped successfully");

    Ok(())
}
*/

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // TODO: fix node-builder on miden-client after updating it to 0.13.2
    // cargo update -p miden-node-block-producer -p miden-testing
    // cargo build -p node-builder
    Ok(())
}
