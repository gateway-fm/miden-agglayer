use crate::service_state::ServiceState;
use clap::Parser;
use miden_agglayer::*;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Command::parse();
    logging::setup_tracing()?;
    let url = Url::from_str(format!("http://0.0.0.0:{}", command.port).as_str())?;
    service::serve(url, ServiceState {}).await?;
    Ok(())
}
