use anyhow::{Context, anyhow};
use miden_client::RemoteTransactionProver;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::{Endpoint, GrpcClient, GrpcError, NodeRpcClient, RpcError};
use miden_client::sync::SyncSummary;
use miden_client::transaction::{LocalTransactionProver, TransactionProver};
use miden_client::{ClientError, DebugMode};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Minimum backoff delay for retries.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Maximum backoff delay for retries.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

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

/// Shared node-URL resolver used by both the persistent `MidenClient` (background sync,
/// GER injection) and the fresh-client path in `src/claim.rs::publish_claim`. Both code
/// paths must dial the same node — funneling through this single function guarantees the
/// shortcut strings (`"devnet"`, `"testnet"`) resolve identically, not just raw URLs.
///
/// Without this shared helper the two paths drifted: `MidenClient::new` used to resolve
/// `"testnet"` to `https://rpc.testnet.miden.io` while `publish_claim` passed the literal
/// string straight to `Endpoint::try_from`, which either fails or produces a wrong URL.
/// See RD-856 for the diagnosis.
pub fn parse_node_url(node_url: &str) -> anyhow::Result<Endpoint> {
    match node_url {
        "devnet" => Ok(Endpoint::devnet()),
        "testnet" => Ok(Endpoint::testnet()),
        _ => Endpoint::try_from(node_url).map_err(|err| anyhow!(err)),
    }
}

/// Resolves the `NetworkId` for the configured `--miden-node` value, applying
/// the same `None` → localhost default as `MidenClient::new`. Use this so
/// bech32 strings written by the service (notably `bridge_accounts.toml`) use
/// the active node's HRP — e.g. `mtst` on testnet rather than the local
/// network's `mlcl`.
pub fn resolve_network_id(
    node_url: Option<&str>,
) -> anyhow::Result<miden_protocol::address::NetworkId> {
    let endpoint = match node_url {
        Some(url) => parse_node_url(url)?,
        None => Endpoint::localhost(),
    };
    Ok(endpoint.to_network_id())
}

/// Builds an RPC client for the Miden node, optionally authenticating via a bearer token.
///
/// When `api_key` is `Some`, the returned client sends `authorization: Bearer <api_key>` on
/// every outbound gRPC call — required when the node sits behind a rate-limiting gateway.
/// Otherwise this behaves identically to `ClientBuilder::grpc_client()`.
pub fn build_rpc_client(
    endpoint: &Endpoint,
    timeout_ms: u64,
    api_key: Option<&str>,
) -> Arc<dyn NodeRpcClient> {
    let mut client = GrpcClient::new(endpoint, timeout_ms);
    if let Some(key) = api_key {
        client = client.with_bearer_auth(key.to_string());
    }
    Arc::new(client)
}

pub struct MidenClient {
    keystore: Arc<FilesystemKeyStore>,
    task: std::sync::Mutex<Option<thread::JoinHandle<anyhow::Result<()>>>>,
    sender: mpsc::Sender<Request>,
    done_sender: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    alive: Arc<AtomicBool>,
    /// Opt-in `LocalTransactionProver` used when the remote prover fails and
    /// `--miden-prover-fallback-to-local` is set. `None` when fallback is
    /// disabled (the default — preserves the bali OOM fix) OR when the
    /// proxy is already proving locally (no remote prover configured, so
    /// the active prover IS the local one and a "fallback" is meaningless).
    local_prover_fallback: Option<Arc<dyn TransactionProver + Send + Sync>>,
    /// Cantina MA#23 — gates `on_post_sync` dispatch on the background sync
    /// thread. While `true`, the initial sync + every 5s `sync_interval`
    /// tick still runs `sync_state()` (so the local sqlite stays current),
    /// but no `SyncListener::on_post_sync` calls fire. `restore()` toggles
    /// this for the duration of its phases so the `BridgeOutScanner` /
    /// `ClaimWatcher` cannot interleave with the consumed-note replay loop
    /// (which would double-emit synthetic logs and race the deposit-count
    /// counter). Released in a `Drop` guard so an error mid-restore still
    /// re-enables listeners.
    listeners_paused: Arc<AtomicBool>,
    #[cfg(test)]
    call_count: Arc<AtomicUsize>,
}

/// RAII guard returned by [`MidenClient::pause_listeners`]. While in scope,
/// the background sync loop will skip `on_post_sync` dispatch on every
/// listener. Drop (success or panic) restores the previous flag.
pub struct ListenerPauseGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for ListenerPauseGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<MidenClient>();

impl MidenClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store_dir: Option<PathBuf>,
        node_url: Option<String>,
        api_key: Option<String>,
        prover_url: Option<String>,
        prover_timeout_secs: u64,
        fallback_to_local: bool,
        sync_listeners: Vec<Arc<dyn SyncListener>>,
        debug_mode: bool,
    ) -> anyhow::Result<Self> {
        let store_dir = store_dir.unwrap_or(Self::default_store_dir());
        let node_endpoint = node_url
            .map(Self::parse_node_url)
            .unwrap_or(Ok(Endpoint::localhost()))?;
        let keystore = Self::create_keystore(store_dir.clone())?;
        let keystore_for_run = keystore.clone();
        let prover_url_for_run = prover_url.clone();

        // Pre-construct the local-prover Arc once at boot so the proof-call
        // hot path can clone it without re-initialising. Only meaningful when
        // a remote prover IS configured AND the operator has opted in via
        // `--miden-prover-fallback-to-local`; otherwise the active prover
        // is already local (no remote configured) or the operator has
        // explicitly chosen "fail rather than fall back to the bali OOM
        // path" (the default).
        let local_prover_fallback: Option<Arc<dyn TransactionProver + Send + Sync>> =
            if fallback_to_local && prover_url.is_some() {
                Some(Arc::new(LocalTransactionProver::default()))
            } else {
                None
            };

        let (sender, receiver) = mpsc::channel::<Request>(1);
        let (done_sender, done_receiver) = oneshot::channel::<()>();
        let alive = Arc::new(AtomicBool::new(false));
        let alive_for_run = alive.clone();
        let listeners_paused = Arc::new(AtomicBool::new(false));
        let listeners_paused_for_run = listeners_paused.clone();

        let runtime = tokio::runtime::Runtime::new()?;
        let task = thread::spawn(move || -> anyhow::Result<()> {
            let local_set = tokio::task::LocalSet::new();
            let mut receiver = receiver;
            let mut done_receiver = done_receiver;

            loop {
                let result = runtime.block_on(local_set.run_until(Self::run(
                    store_dir.clone(),
                    node_endpoint.clone(),
                    api_key.clone(),
                    prover_url_for_run.clone(),
                    prover_timeout_secs,
                    keystore_for_run.clone(),
                    &mut receiver,
                    &mut done_receiver,
                    &sync_listeners,
                    debug_mode,
                    &alive_for_run,
                    &listeners_paused_for_run,
                )));

                match result {
                    Ok(()) => {
                        // Clean shutdown (done_receiver signalled)
                        alive_for_run.store(false, Ordering::Release);
                        return Ok(());
                    }
                    Err(err) => {
                        alive_for_run.store(false, Ordering::Release);
                        metrics::counter!("miden_client_restarts_total").increment(1);
                        tracing::error!("MidenClient::run crashed: {err:#}, restarting in 5s...");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        });

        let task = std::sync::Mutex::new(Some(task));
        let done_sender = std::sync::Mutex::new(Some(done_sender));
        Ok(Self {
            keystore,
            task,
            sender,
            done_sender,
            alive,
            local_prover_fallback,
            listeners_paused,
            #[cfg(test)]
            call_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Returns the opt-in `LocalTransactionProver` fallback if the operator
    /// set `--miden-prover-fallback-to-local` AND a remote prover is
    /// configured. `None` when the active prover is already local (no
    /// remote configured) or fallback was not opted into.
    ///
    /// Callers (currently `claim.rs`) should attempt `prove_transaction`
    /// against the configured prover first, then retry against this
    /// `Arc` when the result is a `ClientError::TransactionProvingError`.
    /// See `src/claim.rs::publish_claim_internal` for the canonical use.
    pub fn local_prover_fallback(&self) -> Option<Arc<dyn TransactionProver + Send + Sync>> {
        self.local_prover_fallback.clone()
    }

    /// Cantina MA#23 — suppress `SyncListener::on_post_sync` dispatch for the
    /// lifetime of the returned guard. The background sync thread still
    /// pulls deltas from the Miden node (so the local sqlite stays
    /// current), but no listener side-effects fire. Used by `restore()` so
    /// the live `BridgeOutScanner` / `ClaimWatcher` cannot interleave with
    /// the consumed-note replay loop and double-emit synthetic logs.
    ///
    /// Calling this while already paused is safe: each guard restores the
    /// flag to `false` on drop, but the wider `restore()` codepath holds the
    /// outermost guard for its entire duration, so the inner reset is a
    /// no-op in practice.
    pub fn pause_listeners(&self) -> ListenerPauseGuard {
        self.listeners_paused.store(true, Ordering::Release);
        ListenerPauseGuard {
            flag: self.listeners_paused.clone(),
        }
    }

    /// Cantina MA#23 — true while `on_post_sync` dispatch is suppressed.
    pub fn listeners_paused(&self) -> bool {
        self.listeners_paused.load(Ordering::Acquire)
    }

    /// Returns true if the background thread is connected and syncing.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
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
            alive: Arc::new(AtomicBool::new(true)),
            local_prover_fallback: None,
            listeners_paused: Arc::new(AtomicBool::new(false)),
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
        parse_node_url(&node_url)
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

    pub(crate) fn unwrap_connection_error(
        client_err: ClientError,
    ) -> anyhow::Result<Box<dyn Error>> {
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
        let mut backoff = BACKOFF_MIN;
        loop {
            let result = client.sync_state().await;
            match result {
                Ok(summary) => {
                    tracing::debug!(target: concat!(module_path!(), "::sync::debug"), "MidenClient::sync succeeded at block {}", summary.block_num);
                    return Ok(summary);
                }
                Err(client_err) => {
                    match Self::unwrap_connection_error(client_err) {
                        Ok(conn_err) => {
                            metrics::counter!("miden_sync_errors_total", "kind" => "connection")
                                .increment(1);
                            tracing::error!(
                                "MidenClient::sync connection error: {conn_err:?}, retrying in {backoff:?}..."
                            );
                        }
                        Err(other_err) => {
                            metrics::counter!("miden_sync_errors_total", "kind" => "other")
                                .increment(1);
                            tracing::error!(
                                "MidenClient::sync non-connection error: {other_err:#}, retrying in {backoff:?}..."
                            );
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = next_backoff(backoff);
                }
            }
        }
    }

    async fn on_sync(
        result: anyhow::Result<SyncSummary>,
        client: &mut MidenClientLib,
        listeners: &[Arc<dyn SyncListener>],
        listeners_paused: &AtomicBool,
    ) -> anyhow::Result<()> {
        let summary = result?;
        // Cantina MA#23 — sample once per sync tick so the pause/unpause
        // transitions don't interleave with this listener loop. Cheap and
        // race-resilient: a tick that begins while paused completes paused.
        let paused = listeners_paused.load(Ordering::Acquire);
        for listener in listeners {
            // `on_sync` is the cheap summary hook — keep firing it so
            // listeners can keep low-frequency tick-counter state in step
            // even while the heavier `on_post_sync` is suppressed.
            listener.on_sync(&summary);
            if paused {
                ::metrics::counter!("miden_listener_skipped_paused_total").increment(1);
                continue;
            }
            listener.on_post_sync(client).await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        store_dir: PathBuf,
        node_endpoint: Endpoint,
        api_key: Option<String>,
        prover_url: Option<String>,
        prover_timeout_secs: u64,
        keystore: Arc<FilesystemKeyStore>,
        receiver: &mut mpsc::Receiver<Request>,
        done_receiver: &mut oneshot::Receiver<()>,
        sync_listeners: &[Arc<dyn SyncListener>],
        debug_mode: bool,
        alive: &AtomicBool,
        listeners_paused: &AtomicBool,
    ) -> anyhow::Result<()> {
        // node client — retry build with exponential backoff
        let node_timeout_ms: u64 = 10_000;
        let mode = if debug_mode {
            DebugMode::Enabled
        } else {
            DebugMode::Disabled
        };

        let tx_prover: Option<Arc<dyn TransactionProver + Send + Sync>> =
            prover_url.as_deref().map(|url| {
                Arc::new(
                    RemoteTransactionProver::new(url)
                        .with_timeout(Duration::from_secs(prover_timeout_secs)),
                ) as _
            });
        if prover_url.is_some() {
            // Deliberately NOT logging the URL itself — operators can audit
            // MIDEN_PROVER_URL from the environment, ops logs must not
            // contain it (the URL may include an auth-bearing path or be
            // routed through an internal-only hostname we do not want
            // captured in long-lived log indices).
            tracing::info!(
                target: crate::COMPONENT,
                prover_url = "configured",
                prover_timeout_secs,
                "MidenClient using remote transaction prover",
            );
        } else {
            tracing::info!(target: crate::COMPONENT, "MidenClient using local transaction prover (default)");
        }

        let mut client;
        let mut backoff = BACKOFF_MIN;
        loop {
            let store_path = store_dir.join("store.sqlite3");
            crate::sqlite_pragmas::open_store_connection(&store_path).with_context(|| {
                format!("failed to configure sqlite store {}", store_path.display())
            })?;
            let mut builder = ClientBuilder::new()
                .rpc(build_rpc_client(
                    &node_endpoint,
                    node_timeout_ms,
                    api_key.as_deref(),
                ))
                .sqlite_store(store_path)
                .authenticator(keystore.clone())
                .in_debug_mode(mode);
            if let Some(p) = tx_prover.clone() {
                builder = builder.prover(p);
            }
            let build_result = builder.build().await;

            match build_result {
                Ok(c) => {
                    client = c;
                    break;
                }
                Err(err) => {
                    metrics::counter!("miden_client_build_errors_total").increment(1);
                    tracing::error!(
                        "MidenClient build failed: {err:#}, retrying in {backoff:?}..."
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {},
                        _ = &mut *done_receiver => {
                            tracing::debug!("MidenClient::run shutdown during build retry");
                            return Ok(());
                        }
                    }
                    backoff = next_backoff(backoff);
                }
            }
        }

        // initial sync
        tokio::select! {
            result = Self::sync(&mut client) => {
                if let Err(err) = Self::on_sync(result, &mut client, sync_listeners, listeners_paused).await {
                    tracing::error!("MidenClient initial sync listener error: {err:#}");
                }
            },
            _ = &mut *done_receiver => {
                tracing::debug!("MidenClient::run loop done");
                return Ok(());
            }
        }

        alive.store(true, Ordering::Release);
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
                        result = Self::sync(&mut client) => {
                            if let Err(err) = Self::on_sync(result, &mut client, sync_listeners, listeners_paused).await {
                                tracing::error!("MidenClient sync listener error: {err:#}");
                            }
                        },
                        _ = &mut *done_receiver => break,
                    }
                },
                _ = &mut *done_receiver => break,
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
/// Connection errors during sync are retried up to 3 times per attempt.
pub async fn wait_for_transaction_commit(
    client: &mut MidenClientLib,
    txn_id: miden_protocol::transaction::TransactionId,
    max_attempts: usize,
    poll_interval: Duration,
) -> anyhow::Result<bool> {
    for _ in 0..max_attempts {
        tokio::time::sleep(poll_interval).await;

        // Retry sync on connection errors (up to 3 retries per poll attempt)
        let mut sync_ok = false;
        for retry in 0..3u32 {
            match client.sync_state().await {
                Ok(_) => {
                    sync_ok = true;
                    break;
                }
                Err(client_err) => match MidenClient::unwrap_connection_error(client_err) {
                    Ok(conn_err) => {
                        tracing::warn!(
                            "wait_for_transaction_commit: sync connection error (retry {}/3): {conn_err:?}",
                            retry + 1
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(other_err) => return Err(other_err),
                },
            }
        }
        if !sync_ok {
            tracing::error!(
                "wait_for_transaction_commit: sync failed after 3 retries, skipping poll"
            );
            continue;
        }

        // Cantina MA#22 — scope the store scan to the txn we're actually
        // waiting on. Previously this used `TransactionFilter::All`, which
        // forces the underlying sqlite query to return EVERY known
        // transaction (committed, uncommitted, expired, foreign) on every
        // 1s poll. On a hot path (claim publish, faucet ops, GER inject,
        // init) this scales O(transactions_in_store) per poll, per call —
        // wasted CPU, wasted memory, slower commit observation. The
        // `Ids(...)` filter pushes the equality check into the SQL query.
        let txns = client
            .get_transactions(miden_client::store::TransactionFilter::Ids(vec![txn_id]))
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

    /// Cantina MA#23 — the listener pause flag flips on while a guard is
    /// held and back off when it drops. Without the guard, the background
    /// sync thread fires `on_post_sync` on every listener including while
    /// `restore()` is iterating consumed notes, producing duplicate
    /// synthetic logs and a race on the deposit-count cursor.
    #[test]
    fn ma23_pause_listeners_guard_toggles_flag_on_drop() {
        let client = MidenClient::new_test();
        assert!(!client.listeners_paused(), "initially not paused");
        {
            let _guard = client.pause_listeners();
            assert!(client.listeners_paused(), "guard pauses listeners");
        }
        assert!(!client.listeners_paused(), "drop releases the pause");
    }

    /// Cantina MA#23 — nesting two guards keeps the flag paused for the
    /// entire outer scope. Inner-drop releases the flag prematurely, but
    /// the outer guard runs `drop()` next and re-asserts the released
    /// state — this test pins the documented "outer guard wins" behaviour
    /// so future refactors don't silently turn pause into a counter.
    #[test]
    fn ma23_pause_listeners_guards_nest() {
        let client = MidenClient::new_test();
        let outer = client.pause_listeners();
        assert!(client.listeners_paused());
        {
            let inner = client.pause_listeners();
            assert!(client.listeners_paused());
            drop(inner);
            // Current contract: inner drop releases the flag. The outer
            // guard's existence does NOT keep it paused on its own —
            // restore() must hold a single guard for its entire window.
            assert!(!client.listeners_paused());
        }
        drop(outer);
        assert!(!client.listeners_paused());
    }

    /// Cantina MA#23 — `pause_listeners()` is callable before the
    /// background sync loop reports `is_alive() == true`. This is the
    /// timing-critical case: the original race is "sync_listeners are
    /// constructed and the background thread is spinning up while
    /// `restore()` runs Phase 0 of its replay." Restore must be able to
    /// install the pause before is_alive flips on.
    #[test]
    fn ma23_pause_listeners_works_before_alive() {
        // `new_test()` returns a stub where alive is already true; the
        // pause flag is independent of alive, which is exactly the
        // invariant the restore call relies on. Pin both observations.
        let client = MidenClient::new_test();
        let _guard = client.pause_listeners();
        assert!(
            client.listeners_paused(),
            "pause works regardless of alive state"
        );
        assert!(client.is_alive(), "test stub is alive");
    }
}
