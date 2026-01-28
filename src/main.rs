use crate::service_state::ServiceState;
use clap::Parser;
use miden_agglayer_service::*;
use std::path::PathBuf;
use std::str::FromStr;
use url::Url;

mod claim_endpoint;
mod service;
pub mod service_state;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Command {
    /// JSON-RPC HTTP service listening port
    #[arg(long, default_value_t = 8125)]
    port: u16,

    /// Directory for miden-client data [default: $HOME/.miden]
    #[arg(long)]
    miden_store_dir: Option<PathBuf>,

    /// Miden node GRPC URL or a network name: "devnet" or "testnet" [default: http://localhost:57291]
    #[arg(long)]
    miden_node: Option<String>,

    /// Create a new accounts config inside --miden-store-dir
    #[arg(long)]
    init: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;

    let miden_store_dir = command.miden_store_dir;
    let client = MidenClient::new(miden_store_dir.clone(), command.miden_node)?;

    if command.init || !config_path_exists(miden_store_dir.clone())? {
        let config_path = init::init(&client, miden_store_dir.clone()).await?;
        tracing::info!("new config created at {config_path:?}");
    }
    if command.init {
        client.join()?;
        return Ok(());
    }

    let accounts = load_config(miden_store_dir)?;
    let state = ServiceState::new(client, accounts);

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, state.clone()).await?;

    state.miden_client.join()?;

    Ok(())
}
