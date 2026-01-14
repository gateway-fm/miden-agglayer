use miden_client::DebugMode;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::Endpoint;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

pub struct MidenClient {
    client: miden_client::Client<FilesystemKeyStore>,
}

impl MidenClient {
    pub async fn new(store_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let store_dir = store_dir.unwrap_or(Self::default_store_dir());

        // node client
        let node_endpoint = Endpoint::localhost();
        let node_timeout_ms: u64 = 10_000;

        // keystore
        let keystore_path = store_dir.join("keystore");
        let keystore = FilesystemKeyStore::new(keystore_path)?;

        let client = ClientBuilder::new()
            .grpc_client(&node_endpoint, Some(node_timeout_ms))
            .sqlite_store(store_dir.join("store.sqlite3"))
            .authenticator(Arc::new(keystore))
            .in_debug_mode(DebugMode::Enabled)
            .build()
            .await?;

        Ok(MidenClient { client })
    }

    fn default_store_dir() -> PathBuf {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    }

    pub async fn sync(&mut self) -> anyhow::Result<()> {
        _ = self.client.sync_state().await?;
        Ok(())
    }
}
