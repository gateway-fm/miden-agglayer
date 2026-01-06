use crate::service_state::ServiceState;
use miden_agglayer::*;
use std::str::FromStr;
use url::Url;

mod claim_endpoint;
mod service;
pub mod service_state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::setup_tracing()?;
    let url = Url::from_str("http://localhost:12345")?;
    service::serve(url, ServiceState {}).await?;
    Ok(())
}
