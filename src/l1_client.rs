//! L1Client trait — abstracts all L1 (Ethereum) interactions.
//!
//! Call sites that previously created ad-hoc alloy providers from a raw URL
//! now go through this trait, enabling mock/test implementations and clean
//! separation of concerns.
//!
//! ## Not yet behind the trait
//!
//! - `claim_settler.rs` — has its own wallet-based signing flow; would need
//!   a separate `L1Signer` trait or similar abstraction.
//! - `restore.rs` — runs at startup before ServiceState exists; still uses
//!   `ger::fetch_l1_exit_roots()` and raw providers directly.

use alloy::primitives::{Address, Bytes};
use alloy::rpc::types::{Filter, Log};

/// Trait for L1 (Ethereum) interactions used by the service layer.
#[async_trait::async_trait]
pub trait L1Client: Send + Sync {
    /// Forward an `eth_call` to L1.
    async fn eth_call(&self, to: Address, data: Bytes) -> anyhow::Result<Bytes>;

    /// Forward a raw transaction (hex-encoded) to L1.
    async fn send_raw_transaction(&self, raw_tx_hex: &str) -> anyhow::Result<String>;

    /// Fetch the latest mainnet and rollup exit roots from the L1 GER contract.
    async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])>;

    /// Get the latest block number on L1.
    async fn get_block_number(&self) -> anyhow::Result<u64>;

    /// Get logs matching a filter from L1.
    async fn get_logs(&self, filter: &Filter) -> anyhow::Result<Vec<Log>>;
}

/// Production L1 client backed by an alloy HTTP provider.
pub struct AlloyL1Client {
    rpc_url: String,
    ger_address: String,
}

impl AlloyL1Client {
    pub fn new(rpc_url: String, ger_address: String) -> Self {
        Self {
            rpc_url,
            ger_address,
        }
    }
}

#[async_trait::async_trait]
impl L1Client for AlloyL1Client {
    async fn eth_call(&self, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy_rpc_types_eth::TransactionRequest;

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.parse()?);
        let result = provider
            .call(TransactionRequest::default().to(to).input(data.into()))
            .await?;
        Ok(result)
    }

    async fn send_raw_transaction(&self, raw_tx_hex: &str) -> anyhow::Result<String> {
        use alloy::providers::{Provider, ProviderBuilder};

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.parse()?);
        let result = provider
            .raw_request::<_, String>("eth_sendRawTransaction".into(), [raw_tx_hex])
            .await?;
        Ok(result)
    }

    async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])> {
        crate::ger::fetch_l1_exit_roots(&self.rpc_url, &self.ger_address).await
    }

    async fn get_block_number(&self) -> anyhow::Result<u64> {
        use alloy::providers::{Provider, ProviderBuilder};

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.parse()?);
        Ok(provider.get_block_number().await?)
    }

    async fn get_logs(&self, filter: &Filter) -> anyhow::Result<Vec<Log>> {
        use alloy::providers::{Provider, ProviderBuilder};

        let provider = ProviderBuilder::new().connect_http(self.rpc_url.parse()?);
        Ok(provider.get_logs(filter).await?)
    }
}

/// No-op L1 client for when L1 is not configured.
///
/// All methods return an error. Use `Option<Arc<dyn L1Client>>` when you want
/// to conditionally skip L1 calls; use `NoOpL1Client` when you need a concrete
/// type that satisfies the trait bound but L1 is known to be unavailable.
pub struct NoOpL1Client;

#[async_trait::async_trait]
impl L1Client for NoOpL1Client {
    async fn eth_call(&self, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
        anyhow::bail!("L1 not configured")
    }

    async fn send_raw_transaction(&self, _raw_tx_hex: &str) -> anyhow::Result<String> {
        anyhow::bail!("L1 not configured")
    }

    async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])> {
        anyhow::bail!("L1 not configured")
    }

    async fn get_block_number(&self) -> anyhow::Result<u64> {
        anyhow::bail!("L1 not configured")
    }

    async fn get_logs(&self, _filter: &Filter) -> anyhow::Result<Vec<Log>> {
        anyhow::bail!("L1 not configured")
    }
}
