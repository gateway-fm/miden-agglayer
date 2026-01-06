use miden_agglayer::*;
use std::str::FromStr;
use url::Url;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::setup_tracing()?;
    let url = Url::from_str("http://localhost:12345")?;
    tracing::info!(target: "miden-agglayer", address = %url, "Service started");
    Ok(())
}
