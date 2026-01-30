use anyhow::anyhow;
use miden_client::DebugMode;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::Endpoint;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::env;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type MidenClientLib = miden_client::Client<FilesystemKeyStore>;

type BoxFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
type BoxFutureFactory =
    Box<dyn for<'c> FnOnce(&'c mut MidenClientLib) -> BoxFuture<'c> + Send + 'static>;

struct Request {
    response_sender: oneshot::Sender<anyhow::Result<()>>,
    closure: BoxFutureFactory,
}

pub struct MidenClient {
    keystore: Arc<FilesystemKeyStore>,
    task: std::sync::Mutex<Option<thread::JoinHandle<anyhow::Result<()>>>>,
    sender: mpsc::Sender<Request>,
    done_sender: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<MidenClient>();

impl MidenClient {
    pub fn new(store_dir: Option<PathBuf>, node_url: Option<String>) -> anyhow::Result<Self> {
        let store_dir = store_dir.unwrap_or(Self::default_store_dir());
        let node_endpoint =
            node_url.map(Self::parse_node_url).unwrap_or(Ok(Endpoint::localhost()))?;
        let keystore = Self::create_keystore(store_dir.clone())?;
        let keystore_for_run = keystore.clone();

        let (sender, receiver) = mpsc::channel::<Request>(1);
        let (done_sender, done_receiver) = oneshot::channel::<()>();

        let runtime = tokio::runtime::Runtime::new()?;
        let task = thread::spawn(move || -> anyhow::Result<()> {
            let result = runtime.block_on(tokio::task::LocalSet::new().run_until(Self::run(
                store_dir,
                node_endpoint,
                keystore_for_run,
                receiver,
                done_receiver,
            )));
            if let Err(err) = &result {
                tracing::error!("MidenClient::run stopped: {err:#?}");
            }
            result
        });

        let task = std::sync::Mutex::new(Some(task));
        let done_sender = std::sync::Mutex::new(Some(done_sender));
        Ok(Self { keystore, task, sender, done_sender })
    }

    fn default_store_dir() -> PathBuf {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    }

    fn parse_node_url(node_url: String) -> anyhow::Result<Endpoint> {
        match node_url.as_str() {
            "devnet" => Ok(Endpoint::devnet()),
            "testnet" => Ok(Endpoint::testnet()),
            _ => {
                let endpoint = Endpoint::try_from(node_url.as_str());
                endpoint.map_err(|err| anyhow!(err))
            },
        }
    }

    fn create_keystore(store_dir: PathBuf) -> anyhow::Result<Arc<FilesystemKeyStore>> {
        let keystore_path = store_dir.join("keystore");
        let keystore = FilesystemKeyStore::new(keystore_path)?;
        Ok(Arc::new(keystore))
    }

    pub fn get_keystore(&self) -> Arc<FilesystemKeyStore> {
        self.keystore.clone()
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

    pub fn close(&self) {
        let mut done_sender_guard = self
            .done_sender
            .lock()
            .expect("MidenClient::close has failed to lock the done_sender mutex");
        let Some(done_sender) = done_sender_guard.take() else {
            return;
        };
        _ = done_sender.send(());
    }

    pub fn shutdown(&self) -> anyhow::Result<()> {
        self.close();
        self.join()
    }

    async fn run(
        store_dir: PathBuf,
        node_endpoint: Endpoint,
        keystore: Arc<FilesystemKeyStore>,
        mut receiver: mpsc::Receiver<Request>,
        mut done_receiver: oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
        // node client
        let node_timeout_ms: u64 = 10_000;

        let mut client = ClientBuilder::new()
            .grpc_client(&node_endpoint, Some(node_timeout_ms))
            .sqlite_store(store_dir.join("store.sqlite3"))
            .authenticator(keystore)
            .in_debug_mode(DebugMode::Enabled)
            .build()
            .await?;

        client.sync_state().await?;

        loop {
            tokio::select! {
                receiver_result = receiver.recv() => {
                    let Some(request) = receiver_result else { break };
                    let result = (request.closure)(&mut client).await;
                    request.response_sender.send(result).unwrap_or(());
                },
                _ = &mut done_receiver => {
                    tracing::debug!("MidenClient::run loop done");
                    break;
                }
            }
        }

        Ok(())
    }

    // https://users.rust-lang.org/t/function-that-takes-an-async-closure/61663/2
    pub async fn with<Fn>(&self, closure: Fn) -> anyhow::Result<()>
    where
        Fn: for<'c> FnOnce(
            &'c mut MidenClientLib,
        ) -> Box<dyn Future<Output = anyhow::Result<()>> + 'c>,
        Fn: Send + 'static,
    {
        let (response_sender, response_receiver) = oneshot::channel::<anyhow::Result<()>>();

        let request = Request {
            response_sender,
            closure: Box::new(|client| Box::into_pin(closure(client))),
        };
        if self.sender.send(request).await.is_err() {
            anyhow::bail!("MidenClient::with: failed to queue a request - receiver is closed");
        }

        let Ok(result) = response_receiver.await else {
            anyhow::bail!("MidenClient::with: failed to get a response - receiver is closed");
        };
        result
    }
}
