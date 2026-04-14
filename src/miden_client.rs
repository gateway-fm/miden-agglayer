use anyhow::anyhow;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::{Endpoint, GrpcError, RpcError};
use miden_client::sync::SyncSummary;
use miden_client::{ClientError, DebugMode};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
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

#[async_trait::async_trait]
pub trait SyncListener: Send + Sync {
    fn on_sync(&self, summary: &SyncSummary);
    async fn on_post_sync(&self, _client: &mut MidenClientLib) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct MidenClient {
    keystore: Arc<FilesystemKeyStore>,
    task: std::sync::Mutex<Option<thread::JoinHandle<anyhow::Result<()>>>>,
    sender: mpsc::Sender<Request>,
    done_sender: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    call_count: Arc<AtomicUsize>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<MidenClient>();

impl MidenClient {
    pub fn new(
        store_dir: Option<PathBuf>,
        node_url: Option<String>,
        sync_listeners: Vec<Arc<dyn SyncListener>>,
        debug_mode: bool,
    ) -> anyhow::Result<Self> {
        let store_dir = store_dir.unwrap_or(Self::default_store_dir());
        let node_endpoint = node_url
            .map(Self::parse_node_url)
            .unwrap_or(Ok(Endpoint::localhost()))?;
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
                sync_listeners,
                debug_mode,
            )));
            if let Err(err) = &result {
                tracing::error!("MidenClient::run stopped: {err:#?}");
            }
            result
        });

        let task = std::sync::Mutex::new(Some(task));
        let done_sender = std::sync::Mutex::new(Some(done_sender));
        Ok(Self {
            keystore,
            task,
            sender,
            done_sender,
            #[cfg(test)]
            call_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    #[cfg(test)]
    pub fn new_test() -> Self {
        Self::new_test_with_response(Ok(()))
    }

    /// Creates a test stub that returns the given response for every `.with()` call.
    /// Also tracks how many times `.with()` was called.
    #[cfg(test)]
    pub fn new_test_with_response(response: anyhow::Result<()>) -> Self {
        let store_dir = tempfile::tempdir().unwrap().keep();
        let keystore_path = store_dir.join("keystore");
        std::fs::create_dir_all(&keystore_path).unwrap();
        let keystore = FilesystemKeyStore::new(keystore_path).unwrap();
        let keystore = Arc::new(keystore);
        let (sender, mut receiver) = mpsc::channel::<Request>(1);
        let (done_sender, _done_receiver) = oneshot::channel::<()>();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // Convert the response into a reusable error message (if error) for the background thread
        let response_err_msg = match &response {
            Ok(()) => None,
            Err(e) => Some(format!("{e:#}")),
        };

        thread::spawn(move || {
            while let Some(req) = receiver.blocking_recv() {
                call_count_clone.fetch_add(1, Ordering::SeqCst);
                let result = match &response_err_msg {
                    None => Ok(()),
                    Some(msg) => Err(anyhow!(msg.clone())),
                };
                let _ = req.response_sender.send(result);
            }
        });

        Self {
            keystore,
            task: std::sync::Mutex::new(None),
            sender,
            done_sender: std::sync::Mutex::new(Some(done_sender)),
            call_count,
        }
    }

    /// Returns the number of times `.with()` was called on this test stub.
    #[cfg(test)]
    pub fn test_call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Returns true if `.with()` was called at least once.
    #[cfg(test)]
    pub fn test_was_called(&self) -> bool {
        self.test_call_count() > 0
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
            }
        }
    }

    fn create_keystore(store_dir: PathBuf) -> anyhow::Result<Arc<FilesystemKeyStore>> {
        let keystore_path = store_dir.join("keystore");
        if !keystore_path.exists() {
            std::fs::create_dir_all(&keystore_path)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&keystore_path, perms)?;
        }
        let keystore = FilesystemKeyStore::new(keystore_path)?;
        Ok(Arc::new(keystore))
    }

    pub fn get_keystore(&self) -> Arc<FilesystemKeyStore> {
        self.keystore.clone()
    }

    pub fn join(&self) -> anyhow::Result<()> {
        let mut task_guard = self
            .task
            .lock()
            .expect("MidenClient::join has failed to lock the task mutex");
        let Some(task) = task_guard.take() else {
            return Ok(());
        };
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

    fn unwrap_connection_error(client_err: ClientError) -> anyhow::Result<Box<dyn Error>> {
        match client_err {
            ClientError::RpcError(RpcError::ConnectionError(err)) => Ok(err),
            ClientError::RpcError(RpcError::RequestError {
                error_kind: GrpcError::Unavailable,
                ..
            }) => Ok(Box::new(GrpcError::Unavailable)),
            _ => Err(client_err.into()),
        }
    }

    async fn sync(client: &mut MidenClientLib) -> anyhow::Result<SyncSummary> {
        loop {
            let result = client.sync_state().await;
            match result {
                Ok(summary) => {
                    tracing::debug!(target: concat!(module_path!(), "::sync::debug"), "MidenClient::sync succeeded at block {}", summary.block_num);
                    return Ok(summary);
                }
                Err(client_err) => {
                    let err = Self::unwrap_connection_error(client_err)?;
                    tracing::error!(
                        "MidenClient::sync failed to connect to the node: {err:?}, retrying in 5 seconds..."
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn on_sync(
        result: anyhow::Result<SyncSummary>,
        client: &mut MidenClientLib,
        listeners: &[Arc<dyn SyncListener>],
    ) -> anyhow::Result<()> {
        let summary = result?;
        for listener in listeners {
            listener.on_sync(&summary);
            listener.on_post_sync(client).await?;
        }
        Ok(())
    }

    async fn run(
        store_dir: PathBuf,
        node_endpoint: Endpoint,
        keystore: Arc<FilesystemKeyStore>,
        mut receiver: mpsc::Receiver<Request>,
        mut done_receiver: oneshot::Receiver<()>,
        sync_listeners: Vec<Arc<dyn SyncListener>>,
        debug_mode: bool,
    ) -> anyhow::Result<()> {
        // node client
        let node_timeout_ms: u64 = 10_000;
        let mode = if debug_mode {
            DebugMode::Enabled
        } else {
            DebugMode::Disabled
        };

        let mut client = ClientBuilder::new()
            .grpc_client(&node_endpoint, Some(node_timeout_ms))
            .sqlite_store(store_dir.join("store.sqlite3"))
            .authenticator(keystore)
            .in_debug_mode(mode)
            .build()
            .await?;

        // initial sync
        tokio::select! {
            result = Self::sync(&mut client) => Self::on_sync(result, &mut client, &sync_listeners).await?,
            _ = &mut done_receiver => {
                tracing::debug!("MidenClient::run loop done");
                return Ok(());
            }
        }
        let mut sync_interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                receiver_result = receiver.recv() => {
                    let Some(request) = receiver_result else { break };
                    let result = (request.closure)(&mut client).await;
                    request.response_sender.send(result).unwrap_or(());
                },
                _ = sync_interval.tick() => {
                    tokio::select! {
                        result = Self::sync(&mut client) => Self::on_sync(result, &mut client, &sync_listeners).await?,
                        _ = &mut done_receiver => break,
                    }
                },
                _ = &mut done_receiver => break,
            }
        }

        tracing::debug!("MidenClient::run loop done");
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

/// Poll until a transaction is committed on the Miden node.
///
/// Returns `true` if committed within the given number of attempts.
pub async fn wait_for_transaction_commit(
    client: &mut MidenClientLib,
    txn_id: miden_protocol::transaction::TransactionId,
    max_attempts: usize,
    poll_interval: Duration,
) -> anyhow::Result<bool> {
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;
        client.sync_state().await?;
        let txns = client
            .get_transactions(miden_client::store::TransactionFilter::All)
            .await?;
        if txns.iter().any(|t| {
            t.id == txn_id
                && matches!(
                    t.status,
                    miden_client::transaction::TransactionStatus::Committed { .. }
                )
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_miden_client_test_tracks_calls() {
        let client = MidenClient::new_test();
        assert_eq!(client.test_call_count(), 0);
        assert!(!client.test_was_called());

        let res = client.with(|_client| Box::new(async move { Ok(()) })).await;

        assert!(res.is_ok());
        assert_eq!(client.test_call_count(), 1);
        assert!(client.test_was_called());

        // Second call increments
        client
            .with(|_client| Box::new(async move { Ok(()) }))
            .await
            .unwrap();
        assert_eq!(client.test_call_count(), 2);
    }

    #[tokio::test]
    async fn test_miden_client_test_with_error_response() {
        let client = MidenClient::new_test_with_response(Err(anyhow!("simulated failure")));

        let res = client.with(|_client| Box::new(async move { Ok(()) })).await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("simulated failure"));
        assert_eq!(client.test_call_count(), 1);
    }
}
