use crate::service_state::ServiceState;
use clap::Parser;
use miden_agglayer::miden_client::MidenClient;
use miden_agglayer::*;
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
    #[arg(short, long, default_value_t = 8125)]
    port: u16,

    /// Directory for miden-client data [default: $HOME/.miden]
    #[arg(short, long)]
    miden_store_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;

    let mut client = MidenClient::new(command.miden_store_dir).await?;
    client.sync().await?;

    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, ServiceState {}).await?;
    Ok(())
}
