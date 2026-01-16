use anyhow::anyhow;
use miden_client::DebugMode;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::Endpoint;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

pub struct MidenClient {
    task: std::sync::Mutex<Option<thread::JoinHandle<anyhow::Result<()>>>>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<MidenClient>();

impl MidenClient {
    pub fn new(store_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let store_dir = store_dir.unwrap_or(Self::default_store_dir());

        let runtime = tokio::runtime::Runtime::new()?;
        let task = thread::spawn(move || -> anyhow::Result<()> {
            let result =
                runtime.block_on(tokio::task::LocalSet::new().run_until(Self::run(store_dir)));
            if let Err(err) = &result {
                tracing::error!("MidenClient::run stopped: {err}");
            }
            result
        });

        Ok(Self { task: std::sync::Mutex::new(Some(task)) })
    }

    fn default_store_dir() -> PathBuf {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    }

    pub fn join(&self) -> anyhow::Result<()> {
        let mut task_guard =
            self.task.lock().expect("MidenClient::join has failed to lock the task mutex");
        let Some(task) = task_guard.take() else { return Ok(()) };
        match task.join() {
            Ok(run_result) => run_result,
            Err(err) => Err(anyhow!("MidenClient::join error: {err:?}")),
        }
    }

    async fn run(store_dir: PathBuf) -> anyhow::Result<()> {
        // node client
        let node_endpoint = Endpoint::localhost();
        let node_timeout_ms: u64 = 10_000;

        // keystore
        let keystore_path = store_dir.join("keystore");
        let keystore = FilesystemKeyStore::new(keystore_path)?;

        let mut client = ClientBuilder::new()
            .grpc_client(&node_endpoint, Some(node_timeout_ms))
            .sqlite_store(store_dir.join("store.sqlite3"))
            .authenticator(Arc::new(keystore))
            .in_debug_mode(DebugMode::Enabled)
            .build()
            .await?;

        client.sync_state().await?;

        Ok(())
    }

    pub async fn with(&self) -> anyhow::Result<()> {
        todo!()
    }
}
