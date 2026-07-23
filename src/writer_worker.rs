//! RD-940 async writer worker — bounded `tokio::sync::mpsc(64)` between the
//! JSON-RPC request thread and Miden submission.
//!
//! See `docs/design/RD-940-async-writer.md` for the full design.
//!
//! ## Flow
//!
//! ```text
//!   HTTP request thread                       writer worker (single)
//!   ──────────────────────                    ──────────────────────
//!   validate (chain_id, nonce, signer)
//!   decode method → DecodedWriteCall
//!   handle.try_enqueue(WriteJob)  ──mpsc(64)──▶  recv  ──▶  dispatch_job
//!   nonce_increment                                       │  MidenClient::with(...)
//!   return tx_hash                                        │  publish_claim / insert_ger
//!                                                         │  txn_commit (success or error)
//!                                                         │  update inflight state
//! ```
//!
//! ## What lives where
//!
//! - **Request thread** owns validation and the per-signer-lock window (R4 nonce
//!   atomicity). It calls `try_enqueue`, which `try_send`s without awaiting, so
//!   the request future returns the tx-hash in milliseconds.
//! - **Worker task** owns the Miden round-trip and the store receipt write.
//!   Mirrors `L1InfoTreeIndexer::spawn` (`src/l1_info_tree_indexer.rs:124`) —
//!   a single-tokio-task pattern shielded with `tokio::select! biased`
//!   shutdown.
//! - **TTL sweeper task** owns eviction of in-flight entries whose terminal
//!   state has been observed long enough that callers should have polled the
//!   receipt by now.
//!
//! ## Phase 1 scope (this module, current commit)
//!
//! - WriteJob enum + JobState + InFlightEntry + WriterWorkerHandle + Worker
//! - 4 of the 8 Spec-F metrics (queue_depth, inflight_jobs, job_duration,
//!   queue_full_rejections); the remaining metrics land in Phase 5.
//! - Worker is a pure **translator** — calls back into
//!   `service_send_raw_txn::worker_handle_claim_asset` /
//!   `worker_handle_ger_insert`, which run the unchanged Miden + record
//!   pipeline minus the per-signer nonce_increment (request thread handled
//!   that). ClaimGuard stays in `worker_handle_claim_asset` for Phase 1; it
//!   relocates *inside the worker dispatch* in Phase 2.
//!
//! Phases 2–5 build on this skeleton (ClaimGuard move, BlockMonitor, TTL →
//! status:0x0, graceful drain, remaining metrics).

use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, TxHash};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;
use ulid::Ulid;

/// Default writer-worker mpsc capacity. Spec §6 decision #5: at queue cap 64
/// and p50 commit ≈ 10 s, sustainable throughput tops out near 6 jobs/s by
/// design — bali's current load envelope. Override with
/// `AGGLAYER_WRITER_QUEUE_DEPTH`.
pub const DEFAULT_QUEUE_DEPTH: usize = 64;

/// Default per-tx-hash TTL after which an in-flight entry in a terminal state
/// is evicted from the DashMap. Inside aggkit's `WaitTxToBeMined = 2 m`
/// envelope (`fixtures/aggkit-config.toml:43`), with margin. Override with
/// `AGGLAYER_WRITER_TX_TTL` (seconds).
pub const DEFAULT_TX_TTL_SECS: u64 = 5 * 60;

/// Env var consulted by `WriterWorker::parse_queue_depth_env`.
pub const QUEUE_DEPTH_ENV: &str = "AGGLAYER_WRITER_QUEUE_DEPTH";

/// Env var consulted by `WriterWorker::parse_tx_ttl_env`.
pub const TX_TTL_ENV: &str = "AGGLAYER_WRITER_TX_TTL";

/// How often the TTL sweeper task wakes up to evict aged-out terminal entries.
const SWEEPER_INTERVAL: Duration = Duration::from_secs(30);

/// RD-940 Phase 5 — graceful-shutdown queue-depth snapshot location.
///
/// Written by the writer-worker process on clean termination with the
/// number of in-flight jobs still in non-terminal state. Read+reset on the
/// next process boot to feed the `agglayer_writer_dropped_on_restart_total`
/// counter. `/tmp` is appropriate for a k8s `emptyDir` (default behaviour on
/// bali) — survives across container restarts within the same Pod, lost
/// across Pod evictions, which remains useful as an observability signal even though the signed
/// envelope is durably recoverable from the transaction store. SIGKILL leaves the tmpfile absent; combined with
/// pre-kill `agglayer_writer_queue_depth` history this still pinpoints the
/// loss window.
pub const DROP_SNAPSHOT_PATH: &str = "/tmp/agglayer-writer-queue-snapshot";

/// Read + remove the dropped-on-restart snapshot left by the previous
/// shutdown. Returns the residual count (0 if the file is missing or
/// unparsable). Call this **after** `metrics::init_metrics` so the
/// counter increment lands in the registered recorder.
pub fn read_and_clear_drop_snapshot() -> u64 {
    match std::fs::read_to_string(DROP_SNAPSHOT_PATH) {
        Ok(s) => {
            let n: u64 = s.trim().parse().unwrap_or(0);
            let _ = std::fs::remove_file(DROP_SNAPSHOT_PATH);
            n
        }
        Err(_) => 0,
    }
}

/// Write the dropped-on-restart snapshot at graceful shutdown. Tolerates
/// I/O failure — if the write fails, the next boot's read returns 0 and
/// the operator sees "queue depth was high → restart → counter quiet",
/// which the pre-kill queue-depth history covers as the fallback signal.
pub fn write_drop_snapshot(count: u64) {
    let _ = std::fs::write(DROP_SNAPSHOT_PATH, count.to_string());
}

// ─── DecodedWriteCall ───────────────────────────────────────────────────────

/// Method-decoded `eth_sendRawTransaction` payload — the *output* of
/// `service_send_raw_txn`'s request-thread decode step and the input to the
/// mandatory writer-worker enqueue. Keeping this shape separate from the wire
/// representation avoids duplicating selector detection in the worker.
// `claimAssetCall` is much larger than the Ger payload (multiple FixedBytes<32>
// arrays + U256 + addresses, ~1 KB worst case). Box it so the enum variant size
// stays small — clippy::large_enum_variant gates this above 200 bytes and the
// cost of the indirection is negligible against the time it took to decode the
// ABI in the first place.
#[derive(Debug, Clone)]
pub enum DecodedWriteCall {
    Claim {
        params: Box<crate::claim::claimAssetCall>,
    },
    Ger {
        ger_bytes: [u8; 32],
    },
}

impl DecodedWriteCall {
    pub fn kind(&self) -> WriteJobKind {
        match self {
            DecodedWriteCall::Claim { .. } => WriteJobKind::Claim,
            DecodedWriteCall::Ger { .. } => WriteJobKind::GerInsert,
        }
    }

    pub fn into_job(self, envelope: TxEnvelope, signer: Address, eth_tx_hash: TxHash) -> WriteJob {
        let job_id = Ulid::new();
        match self {
            DecodedWriteCall::Claim { params } => WriteJob::Claim {
                params,
                envelope,
                signer,
                eth_tx_hash,
                job_id,
            },
            DecodedWriteCall::Ger { ger_bytes } => WriteJob::Ger {
                ger_bytes,
                envelope,
                signer,
                eth_tx_hash,
                job_id,
            },
        }
    }
}

// ─── WriteJob ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteJobKind {
    Claim,
    GerInsert,
}

impl WriteJobKind {
    /// `kind` label value used in Prometheus metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            WriteJobKind::Claim => "claim",
            WriteJobKind::GerInsert => "ger_insert",
        }
    }
}

/// A single unit of work the writer worker pulls off the mpsc and processes.
///
/// Carries the decoded method params + tx-envelope so the worker has
/// everything it needs to (a) submit to Miden, (b) record the receipt, and
/// (c) emit the synthetic log — without re-touching the wire bytes.
#[derive(Debug, Clone)]
pub enum WriteJob {
    Claim {
        // Boxed for the same reason as `DecodedWriteCall::Claim::params`.
        params: Box<crate::claim::claimAssetCall>,
        envelope: TxEnvelope,
        signer: Address,
        eth_tx_hash: TxHash,
        job_id: Ulid,
    },
    Ger {
        ger_bytes: [u8; 32],
        envelope: TxEnvelope,
        signer: Address,
        eth_tx_hash: TxHash,
        job_id: Ulid,
    },
}

impl WriteJob {
    pub fn eth_tx_hash(&self) -> TxHash {
        match self {
            WriteJob::Claim { eth_tx_hash, .. } | WriteJob::Ger { eth_tx_hash, .. } => *eth_tx_hash,
        }
    }

    pub fn signer(&self) -> Address {
        match self {
            WriteJob::Claim { signer, .. } | WriteJob::Ger { signer, .. } => *signer,
        }
    }

    pub fn job_id(&self) -> Ulid {
        match self {
            WriteJob::Claim { job_id, .. } | WriteJob::Ger { job_id, .. } => *job_id,
        }
    }

    pub fn kind(&self) -> WriteJobKind {
        match self {
            WriteJob::Claim { .. } => WriteJobKind::Claim,
            WriteJob::Ger { .. } => WriteJobKind::GerInsert,
        }
    }
}

// ─── In-flight state ─────────────────────────────────────────────────────────

/// State of a single writer-worker job, observable by request-thread reads
/// (`eth_getTransactionByHash`, `eth_getTransactionReceipt`) for the window
/// between enqueue and TTL eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Sitting in the mpsc queue, not yet dequeued.
    Queued,
    /// Pulled off the queue; mid-Miden-submission.
    Submitting,
    /// Worker successfully committed the receipt and emitted the synthetic
    /// log. `block_number` is the synthetic block the success was recorded at.
    Committed { block_number: u64 },
    /// Worker returned an error or its supervised dispatch task panicked. A failure
    /// receipt was best-effort written; the caller's `eth_getTransactionReceipt` returns `status:0x0`.
    Failed,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Committed { .. } | JobState::Failed)
    }
}

/// Per-`TxHash` row in the in-flight `DashMap`. Read on every
/// `eth_getTransactionByHash` / `eth_getTransactionReceipt` for the same
/// hash; the map is `DashMap` rather than `RwLock<HashMap>` because aggkit
/// polls at ~1 Hz × number-of-in-flight-txs, which sits squarely in
/// shard-contention territory under load.
#[derive(Debug, Clone)]
pub struct InFlightEntry {
    pub state: JobState,
    pub eth_tx_hash: TxHash,
    pub signer: Address,
    pub kind: WriteJobKind,
    pub job_id: Ulid,
    pub envelope: TxEnvelope,
    /// Wall-clock instant the entry was first inserted (mpsc enqueue moment).
    pub created_at: Instant,
    /// Wall-clock instant the state transitioned to `Committed` or `Failed`.
    /// `None` until the worker writes a terminal state; used by the sweeper to
    /// time TTL eviction.
    pub terminal_at: Option<Instant>,
}

impl InFlightEntry {
    fn from_job(job: &WriteJob) -> Self {
        Self {
            state: JobState::Queued,
            eth_tx_hash: job.eth_tx_hash(),
            signer: job.signer(),
            kind: job.kind(),
            job_id: job.job_id(),
            envelope: job.envelope().clone(),
            created_at: Instant::now(),
            terminal_at: None,
        }
    }
}

impl WriteJob {
    fn envelope(&self) -> &TxEnvelope {
        match self {
            WriteJob::Claim { envelope, .. } | WriteJob::Ger { envelope, .. } => envelope,
        }
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Reasons `try_enqueue` may fail. Both variants are caller-meaningful; the
/// JSON-RPC dispatcher (`service.rs::eth_sendRawTransaction`) downcasts on
/// this type to emit the right wire error.
#[derive(Debug, thiserror::Error)]
pub enum TryEnqueueError {
    /// mpsc was full at try_send time. Wire response: JSON-RPC `-32005
    /// "writer queue saturated; retry"` (geth's `LimitExceeded`). aggkit's
    /// ethtxmanager retries `-32005` transparently. See Spec E.
    #[error("writer queue saturated; retry")]
    QueueFull,
    /// mpsc receiver has been dropped — worker task exited (graceful shutdown
    /// or a panic that crossed the supervision boundary). Wire response:
    /// JSON-RPC `-32005 "service shutting down"`; aggkit retries.
    #[error("writer worker has shut down")]
    ShutDown,
}

/// Sentinel error attached to the anyhow chain when `service_send_raw_txn`
/// converts `TryEnqueueError::QueueFull` into the function's
/// `anyhow::Result<TxHash>`. The JSON-RPC dispatcher in `service.rs`
/// `.downcast_ref::<WriterQueueSaturatedError>()` on it to switch the wire
/// error code to `-32005` (geth's `LimitExceeded`) instead of the default
/// `ApplicationError(1) = SendRawTransaction`.
#[derive(Debug, thiserror::Error)]
#[error("writer queue saturated; retry")]
pub struct WriterQueueSaturatedError;

#[derive(Debug, thiserror::Error)]
#[error("writer dispatch task panicked: {0}")]
struct WriterDispatchPanic(String);

/// Run one dispatch behind a Tokio task boundary so a panic becomes a normal
/// job failure and cannot terminate the sole writer loop.
async fn supervise_dispatch<F>(future: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    match tokio::spawn(future.in_current_span()).await {
        Ok(result) => result,
        Err(join_err) if join_err.is_panic() => {
            Err(WriterDispatchPanic(join_err.to_string()).into())
        }
        Err(join_err) => Err(anyhow::anyhow!(
            "writer dispatch task was cancelled: {join_err}"
        )),
    }
}

// ─── Public handle ───────────────────────────────────────────────────────────

/// Producer-side handle to the writer worker. Cloneable via `Arc` so every
/// `ServiceState` clone shares the same channel + in-flight cache.
pub struct WriterWorkerHandle {
    sender: mpsc::Sender<WriteJob>,
    inflight: Arc<DashMap<TxHash, InFlightEntry>>,
    queue_depth: usize,
}

/// Keep-alive for a [`WriterWorkerHandle::saturated_for_test`] handle: holds the
/// un-drained receiver (channel stays open) and the owned permit (the sole slot
/// stays reserved, so `available_capacity()` stays 0).
#[cfg(test)]
pub struct SaturatedWriterGuard {
    _rx: mpsc::Receiver<WriteJob>,
    _permit: mpsc::OwnedPermit<WriteJob>,
}

impl WriterWorkerHandle {
    /// Configured mpsc capacity (the static `queue_depth` argument passed to
    /// `WriterWorker::spawn`). Read-only.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Current size of the in-flight DashMap (queued + submitting +
    /// pre-eviction terminal entries).
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Lookup for the JSON-RPC read path (`eth_getTransactionByHash`,
    /// `eth_getTransactionReceipt`). Returns `None` if the hash is unknown or
    /// already TTL-evicted.
    pub fn get_inflight(&self, hash: &TxHash) -> Option<InFlightEntry> {
        self.inflight.get(hash).map(|e| e.value().clone())
    }

    /// Cheap presence check — used by the upcoming Phase 2 tx-hash dedup
    /// early-return in `service_send_raw_txn`. `true` means the worker is
    /// already tracking this hash; the request thread should short-circuit to
    /// `Ok(hash)` without re-validating R4 nonce or re-enqueueing.
    pub fn is_inflight(&self, hash: &TxHash) -> bool {
        self.inflight.contains_key(hash)
    }

    /// Whether this process still has executable work for `(signer, nonce)`.
    /// A durable unlinked row without this live counterpart is a restart orphan
    /// and must block admission of later nonces until its exact hash resumes.
    pub fn has_non_terminal_nonce(&self, signer: &Address, nonce: u64) -> bool {
        self.inflight.iter().any(|entry| {
            entry.signer == *signer
                && !entry.state.is_terminal()
                && crate::store::envelope_nonce(&entry.envelope) == nonce
        })
    }

    /// Number of slots currently available in the mpsc channel
    /// (`Sender::capacity()`). Re-published as the
    /// `agglayer_writer_queue_depth` gauge on each enqueue attempt.
    pub fn available_capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Build a handle whose write channel reports `available_capacity() == 0`
    /// deterministically (no draining worker), by holding the single buffer slot
    /// with an owned permit. Used to test that a durable admission whose enqueue
    /// is rejected under saturation stays automatically recoverable (#156). The
    /// returned guard MUST be held for the test's duration — it pins the
    /// reservation and keeps the channel open.
    #[cfg(test)]
    pub fn saturated_for_test() -> (Self, SaturatedWriterGuard) {
        let (sender, rx) = mpsc::channel::<WriteJob>(1);
        let permit = sender
            .clone()
            .try_reserve_owned()
            .expect("the sole channel slot must be reservable");
        let handle = Self {
            sender,
            inflight: Arc::new(DashMap::new()),
            queue_depth: 1,
        };
        (
            handle,
            SaturatedWriterGuard {
                _rx: rx,
                _permit: permit,
            },
        )
    }

    /// RD-940 Phase 5 — total non-terminal in-flight count (Queued +
    /// Submitting across all signers). Used at graceful shutdown to size
    /// the `dropped_on_restart` tmpfile snapshot.
    pub fn inflight_non_terminal_count(&self) -> usize {
        self.inflight
            .iter()
            .filter(|e| !e.state.is_terminal())
            .count()
    }

    /// Count process-local non-terminal jobs for diagnostics and drain tests.
    /// Transaction-count RPCs use the store's durable pending frontier, which
    /// survives process restart.
    pub fn count_non_terminal_for_signer(&self, signer: &Address) -> usize {
        self.inflight
            .iter()
            .filter(|e| e.signer == *signer && !e.state.is_terminal())
            .count()
    }

    /// Try to push a job onto the writer-worker queue.
    ///
    /// **Non-blocking** — uses `mpsc::Sender::try_send`. On full, returns
    /// `Err(TryEnqueueError::QueueFull)` immediately so the request future
    /// doesn't park; the caller emits JSON-RPC `-32005`.
    ///
    /// Insert into the in-flight map happens **before** the try_send so a
    /// concurrent reader cannot observe an empty map for an enqueued job.
    /// Insert is rolled back on try_send failure so `is_inflight` remains
    /// truthful.
    pub fn try_enqueue(&self, job: WriteJob) -> Result<(), TryEnqueueError> {
        let hash = job.eth_tx_hash();
        let kind = job.kind();
        let entry = InFlightEntry::from_job(&job);

        // The inflight map is the source of truth for "have we accepted this
        // hash?". Insert first, send second; rollback on send failure.
        self.inflight.insert(hash, entry);
        ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);

        match self.sender.try_send(job) {
            Ok(()) => {
                // The depth published here is the *fill level* (cap minus
                // available), the metric all dashboards care about.
                let depth = self.queue_depth.saturating_sub(self.sender.capacity());
                ::metrics::gauge!("agglayer_writer_queue_depth").set(depth as f64);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inflight.remove(&hash);
                ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);
                ::metrics::counter!(
                    "agglayer_writer_queue_full_rejections_total",
                    "kind" => kind.as_str()
                )
                .increment(1);
                Err(TryEnqueueError::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.inflight.remove(&hash);
                ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);
                Err(TryEnqueueError::ShutDown)
            }
        }
    }
}

// ─── Worker task ─────────────────────────────────────────────────────────────

/// The writer-worker task itself. Owns the mpsc receiver and the
/// `ServiceState` clone it dispatches against. Constructed and immediately
/// spawned by `WriterWorker::spawn`; not exposed for direct construction.
pub struct WriterWorker {
    receiver: mpsc::Receiver<WriteJob>,
    inflight: Arc<DashMap<TxHash, InFlightEntry>>,
    service: ServiceState,
    tx_ttl: Duration,
}

impl WriterWorker {
    /// Read `AGGLAYER_WRITER_QUEUE_DEPTH`, falling back to `DEFAULT_QUEUE_DEPTH`
    /// when unset or unparsable. Logs a warning on parse failure so an
    /// operator misconfiguration is loud.
    pub fn parse_queue_depth_env() -> usize {
        match std::env::var(QUEUE_DEPTH_ENV) {
            Ok(v) => match v.parse::<usize>() {
                Ok(n) if n >= 1 => n,
                Ok(n) => {
                    tracing::warn!(
                        env = QUEUE_DEPTH_ENV,
                        value = n,
                        "{QUEUE_DEPTH_ENV} must be ≥ 1; using default {DEFAULT_QUEUE_DEPTH}"
                    );
                    DEFAULT_QUEUE_DEPTH
                }
                Err(e) => {
                    tracing::warn!(
                        env = QUEUE_DEPTH_ENV,
                        value = %v,
                        err = %e,
                        "could not parse {QUEUE_DEPTH_ENV}; using default {DEFAULT_QUEUE_DEPTH}"
                    );
                    DEFAULT_QUEUE_DEPTH
                }
            },
            Err(_) => DEFAULT_QUEUE_DEPTH,
        }
    }

    /// Read `AGGLAYER_WRITER_TX_TTL` (seconds), falling back to
    /// `DEFAULT_TX_TTL_SECS`. Logs a warning on parse failure.
    pub fn parse_tx_ttl_env() -> Duration {
        match std::env::var(TX_TTL_ENV) {
            Ok(v) => match v.parse::<u64>() {
                Ok(n) if n >= 1 => Duration::from_secs(n),
                Ok(n) => {
                    tracing::warn!(
                        env = TX_TTL_ENV,
                        value = n,
                        "{TX_TTL_ENV} must be ≥ 1 second; using default {DEFAULT_TX_TTL_SECS}s"
                    );
                    Duration::from_secs(DEFAULT_TX_TTL_SECS)
                }
                Err(e) => {
                    tracing::warn!(
                        env = TX_TTL_ENV,
                        value = %v,
                        err = %e,
                        "could not parse {TX_TTL_ENV}; using default {DEFAULT_TX_TTL_SECS}s"
                    );
                    Duration::from_secs(DEFAULT_TX_TTL_SECS)
                }
            },
            Err(_) => Duration::from_secs(DEFAULT_TX_TTL_SECS),
        }
    }

    /// Spawn the writer worker and maintenance sweeper. Returns a producer handle
    /// (cloneable via Arc) and a oneshot shutdown channel — send `()` (or
    /// drop the sender) to request a graceful stop. The worker drains its
    /// `recv` loop and exits; the sweeper exits when the inflight map's
    /// last reference drops.
    pub fn spawn(
        service: ServiceState,
        queue_depth: usize,
        tx_ttl: Duration,
    ) -> (WriterWorkerHandle, oneshot::Sender<()>) {
        let (tx, rx) = mpsc::channel::<WriteJob>(queue_depth);
        let inflight = Arc::new(DashMap::<TxHash, InFlightEntry>::new());
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let worker = WriterWorker {
            receiver: rx,
            inflight: inflight.clone(),
            service: service.clone(),
            tx_ttl,
        };

        tokio::spawn(async move {
            worker.run(&mut shutdown_rx).await;
        });

        // Maintenance sweeper. It deliberately NEVER terminal-fails queued or
        // submitting work: the mpsc item / MidenClient closure is owned by a
        // different task and can still execute after a concurrent map update.
        // Publishing status:0x0 from here would therefore race a real Miden
        // side effect. Only the consuming worker may fail a job, after it has
        // either completed dispatch or determined (before dispatch) that the
        // job spent its whole TTL in the queue.
        //
        // This task has only two safe responsibilities:
        //   1. renew nonce reservations while process-local work is live;
        //   2. evict terminal entries older than tx_ttl since `terminal_at`.
        //
        // Iteration pattern: collect the hash+entry snapshots into a Vec
        // first (no `.await` while holding DashMap iter guards), then
        // process outside the iteration. This avoids the deadlock risk
        // where an in-flight reader on the same shard would block waiting
        // for our (suspended) iterator to release.
        let sweeper_inflight = inflight.clone();
        let sweeper_ttl = tx_ttl;
        let sweeper_service = service.clone();
        tokio::spawn(async move {
            let reservation_lease = crate::service_send_raw_txn::reservation_lease();
            let renewal_period = std::cmp::max(Duration::from_secs(1), reservation_lease / 3);
            let mut ticker = tokio::time::interval(std::cmp::min(SWEEPER_INTERVAL, renewal_period));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let now = Instant::now();

                let mut to_renew: Vec<(TxHash, Address, u64)> = Vec::new();
                let mut to_evict: Vec<TxHash> = Vec::new();
                for entry in sweeper_inflight.iter() {
                    match entry.state {
                        JobState::Queued | JobState::Submitting => {
                            to_renew.push((
                                *entry.key(),
                                entry.signer,
                                crate::service_send_raw_txn::envelope_nonce(&entry.envelope),
                            ));
                        }
                        JobState::Committed { .. } | JobState::Failed => {
                            if let Some(t) = entry.terminal_at
                                && now.duration_since(t) > sweeper_ttl
                            {
                                to_evict.push(*entry.key());
                            }
                        }
                    }
                }

                for (hash, signer, nonce) in &to_renew {
                    let signer = format!("{signer:#x}");
                    if let Err(err) = sweeper_service
                        .store
                        .renew_reservation(&signer, *nonce, *hash, reservation_lease)
                        .await
                    {
                        tracing::warn!(
                            target: "writer_worker::lease", %hash, error = %err,
                            "failed to renew durable writer reservation"
                        );
                    }
                }

                if !to_evict.is_empty() {
                    for hash in &to_evict {
                        sweeper_inflight.remove(hash);
                    }
                    tracing::debug!(
                        target: "writer_worker::ttl",
                        evicted = to_evict.len(),
                        inflight = sweeper_inflight.len(),
                        "writer_worker TTL sweeper evicted aged terminal entries"
                    );
                    ::metrics::gauge!("agglayer_writer_inflight_jobs")
                        .set(sweeper_inflight.len() as f64);
                }
            }
        });

        let handle = WriterWorkerHandle {
            sender: tx,
            inflight,
            queue_depth,
        };
        (handle, shutdown_tx)
    }

    async fn run(mut self, shutdown_rx: &mut oneshot::Receiver<()>) {
        tracing::info!(
            target: "writer_worker",
            tx_ttl_secs = self.tx_ttl.as_secs(),
            "writer worker starting"
        );
        loop {
            tokio::select! {
                biased;
                _ = &mut *shutdown_rx => {
                    tracing::info!(target: "writer_worker", "shutdown signal received");
                    break;
                }
                maybe_job = self.receiver.recv() => {
                    let Some(job) = maybe_job else {
                        tracing::info!(
                            target: "writer_worker",
                            "channel closed by all senders; stopping"
                        );
                        break;
                    };
                    self.process(job).await;
                }
            }
        }
        tracing::info!(target: "writer_worker", "writer worker stopped");
    }

    async fn process(&self, job: WriteJob) {
        let hash = job.eth_tx_hash();
        let kind = job.kind();
        let job_id = job.job_id();
        let signer = job.signer();
        let started = Instant::now();

        // RD-940 Phase 5 — one tracing span per job, fields per Spec F §4:
        // tx_hash, job_id, kind, signer, queue_wait_ms, miden_submit_ms,
        // commit_ms. `queue_wait_ms` is measured from the inflight entry's
        // `created_at` (set in `try_enqueue`); the remaining elapsed
        // measurements are recorded inline below.
        let queue_wait = self
            .inflight
            .get(&hash)
            .map(|e| e.created_at.elapsed())
            .unwrap_or_default();
        let queue_wait_ms = queue_wait.as_millis() as u64;
        let span = tracing::info_span!(
            target: "writer_worker::job",
            "writer_job",
            %hash,
            %job_id,
            kind = kind.as_str(),
            signer = %signer,
            queue_wait_ms,
        );
        let _entered = span.enter();

        // The consuming worker is the only task allowed to expire queued
        // work. At this point the item has been removed from mpsc and no
        // dispatch future has been created, so a terminal failure cannot race
        // a later Miden side effect from the same job.
        if queue_wait >= self.tx_ttl {
            let err = anyhow::anyhow!(
                "writer_worker: TTL expired in queue before dispatch (>{}s)",
                self.tx_ttl.as_secs()
            );
            if preserve_pending_after_handoff(&self.service.store, hash).await {
                self.inflight.remove(&hash);
                tracing::warn!(
                    target: "writer_worker",
                    %hash, kind = kind.as_str(), %job_id, signer = %signer,
                    queue_wait_ms,
                    "queued retry expired after an existing durable note handoff; \
                     leaving receipt pending"
                );
                ::metrics::histogram!(
                    "agglayer_writer_job_duration_seconds",
                    "kind" => kind.as_str(),
                    "outcome" => "pending",
                )
                .record(started.elapsed().as_secs_f64());
                ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);
                return;
            }
            if let Some(mut entry) = self.inflight.get_mut(&hash) {
                entry.state = JobState::Failed;
                entry.terminal_at = Some(Instant::now());
            }
            tracing::warn!(
                target: "writer_worker",
                %hash, kind = kind.as_str(), %job_id, signer = %signer,
                queue_wait_ms,
                "writer job expired in queue before dispatch; writing failure receipt"
            );
            if let Err(store_err) = write_failure_receipt(&self.service, hash, &err).await {
                tracing::error!(
                    target: "writer_worker",
                    %hash,
                    error = format!("{store_err:#}"),
                    "writer_worker: failed to write queue-expiry receipt; \
                     eth_getTransactionReceipt will return null"
                );
            }
            ::metrics::counter!(
                "agglayer_writer_job_failures_total",
                "kind" => kind.as_str(),
                "reason" => "ttl",
            )
            .increment(1);
            ::metrics::histogram!(
                "agglayer_writer_job_duration_seconds",
                "kind" => kind.as_str(),
                "outcome" => "failed",
            )
            .record(started.elapsed().as_secs_f64());
            ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);
            return;
        }

        // Transition Queued → Submitting.
        if let Some(mut entry) = self.inflight.get_mut(&hash) {
            entry.state = JobState::Submitting;
        }

        let outcome_label;
        let dispatch_service = self.service.clone();
        let result =
            supervise_dispatch(async move { dispatch_job(&dispatch_service, job).await }).await;
        match result {
            Ok(()) => {
                // Best-effort: read the freshly-bumped tip to attribute the
                // metric to the right block number. Failure to read it is
                // not fatal — the receipt was already written by the
                // dispatch path.
                let block = self
                    .service
                    .store
                    .get_latest_block_number()
                    .await
                    .unwrap_or(0);
                // RD-940 Phase 3 — keep the BlockMonitor tip mirror fresh
                // so the next eth_blockNumber hot-read returns the right
                // value without a store round-trip.
                if block > 0 {
                    self.service.block_monitor.record_tip(block);
                }
                if let Some(mut entry) = self.inflight.get_mut(&hash) {
                    entry.state = JobState::Committed {
                        block_number: block,
                    };
                    entry.terminal_at = Some(Instant::now());
                }
                outcome_label = "committed";
                tracing::info!(
                    target: "writer_worker",
                    %hash, kind = kind.as_str(), %job_id, signer = %signer,
                    block,
                    elapsed_secs = started.elapsed().as_secs_f64(),
                    "writer_worker: job committed"
                );
            }
            Err(err) => {
                if preserve_pending_after_handoff(&self.service.store, hash).await {
                    self.inflight.remove(&hash);
                    outcome_label = "pending";
                    tracing::warn!(
                        target: "writer_worker",
                        %hash, kind = kind.as_str(), %job_id, signer = %signer,
                        elapsed_secs = started.elapsed().as_secs_f64(),
                        error = format!("{err:#}"),
                        "writer job errored after durable note handoff; leaving receipt pending"
                    );
                } else {
                    if let Some(mut entry) = self.inflight.get_mut(&hash) {
                        entry.state = JobState::Failed;
                        entry.terminal_at = Some(Instant::now());
                    }
                    outcome_label = "failed";
                    tracing::error!(
                        target: "writer_worker",
                        %hash, kind = kind.as_str(), %job_id, signer = %signer,
                        elapsed_secs = started.elapsed().as_secs_f64(),
                        error = format!("{err:#}"),
                        "writer_worker: job failed; writing failure receipt"
                    );
                    // Best-effort: write the failure receipt so
                    // `eth_getTransactionReceipt` transitions
                    // `null → status:0x0` and aggkit's ethtxmanager moves the
                    // tx to Failed instead of polling forever.
                    if let Err(store_err) = write_failure_receipt(&self.service, hash, &err).await {
                        tracing::error!(
                            target: "writer_worker",
                            %hash,
                            error = format!("{store_err:#}"),
                            "writer_worker: failed to write failure receipt; \
                             eth_getTransactionReceipt will return null"
                        );
                    }
                    let reason = if err.downcast_ref::<WriterDispatchPanic>().is_some() {
                        "panic"
                    } else {
                        "miden"
                    };
                    ::metrics::counter!(
                        "agglayer_writer_job_failures_total",
                        "kind" => kind.as_str(),
                        "reason" => reason,
                    )
                    .increment(1);
                }
            }
        }

        let elapsed = started.elapsed().as_secs_f64();
        ::metrics::histogram!(
            "agglayer_writer_job_duration_seconds",
            "kind" => kind.as_str(),
            "outcome" => outcome_label,
        )
        .record(elapsed);
        ::metrics::gauge!("agglayer_writer_inflight_jobs").set(self.inflight.len() as f64);
    }
}

/// Dispatch a `WriteJob` to the matching Phase-1 translator in
/// `service_send_raw_txn`. Each variant calls the corresponding publish/insert
/// handler without a per-signer `nonce_increment`: the request thread durably
/// admitted the envelope and advanced the nonce before enqueue.
async fn dispatch_job(service: &ServiceState, job: WriteJob) -> anyhow::Result<()> {
    match job {
        WriteJob::Claim {
            params,
            envelope,
            signer,
            eth_tx_hash,
            ..
        } => {
            crate::service_send_raw_txn::worker_handle_claim_asset(
                service,
                *params,
                eth_tx_hash,
                envelope,
                signer,
            )
            .await
        }
        WriteJob::Ger {
            ger_bytes,
            envelope,
            signer,
            eth_tx_hash,
            ..
        } => {
            crate::service_send_raw_txn::worker_handle_ger_insert(
                service,
                ger_bytes,
                eth_tx_hash,
                envelope,
                signer,
            )
            .await
        }
    }
}

/// Best-effort failure receipt writer. Records a pending tx row (if one
/// doesn't already exist, e.g. because the failure happened before
/// `record_local_pending_tx` ran), then `txn_commit` with `Err`. The
/// downstream effect is that `eth_getTransactionReceipt(hash)` transitions
/// from JSON `null` (not-yet-mined) to `status:0x0` (failed) — the contract
/// aggkit's ethtxmanager needs to move the tx out of its retry loop.
async fn write_failure_receipt(
    service: &ServiceState,
    hash: TxHash,
    err: &anyhow::Error,
) -> anyhow::Result<()> {
    if preserve_pending_after_handoff(&service.store, hash).await {
        return Ok(());
    }
    let reason = format!("writer_worker: {err:#}");

    // Snapshot the current tip so the receipt is attributable to a block
    // (even if it's not the block the success-path would have used). Failure
    // to read the tip falls back to 0; the receipt is still recoverable.
    let block_num = service.store.get_latest_block_number().await.unwrap_or(0);
    let block_hash = service.block_state.get_block_hash(block_num);

    // txn_commit asserts a prior txn_begin row. If the failure happened before
    // the dispatcher reached the `record_local_pending_tx` step, no row
    // exists — txn_commit would error. We don't have the original envelope
    // here (it's been moved into the dispatch), but the inflight cache does;
    // for Phase 1 we accept that "no prior begin" is best-effort recoverable
    // by the future txn_commit_pending sweep at sync time. Phase 4 lands the
    // queue-age expiry and completed dispatch failures use this same path.
    service
        .store
        .txn_commit(hash, Err(reason), block_num, block_hash)
        .await
}

/// Fail closed when the store cannot disprove an exact note handoff. Once a
/// handoff exists, only commit/observation or expiration reconciliation may
/// transition the receipt; worker errors must never publish status 0.
async fn preserve_pending_after_handoff(
    store: &std::sync::Arc<dyn crate::store::Store>,
    hash: TxHash,
) -> bool {
    let tx_key = format!("{hash:#x}");
    match store.get_note_handoff_for_tx(&tx_key).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                target: "writer_worker",
                %hash,
                error = %err,
                "could not classify note handoff; conservatively keeping receipt pending"
            );
            true
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::{SignableTransaction, TxLegacy};
    use alloy::primitives::U256;
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;

    /// Build a throwaway signed legacy tx envelope for tests that need an
    /// `InFlightEntry` shaped object. Hash is determined by the wallet +
    /// nonce so each call gives a distinct hash.
    fn fake_envelope(nonce: u64) -> (TxEnvelope, Address) {
        let signer = PrivateKeySigner::random();
        let from = signer.address();
        let tx = TxLegacy {
            chain_id: Some(2),
            nonce,
            gas_price: 0,
            gas_limit: 21_000,
            to: alloy::primitives::TxKind::Call(from),
            value: U256::ZERO,
            input: alloy::primitives::Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let signed = tx.into_signed(signature);
        let env: TxEnvelope = signed.into();
        (env, from)
    }

    fn fake_ger_job(nonce: u64) -> WriteJob {
        let (env, signer) = fake_envelope(nonce);
        // Hash derived from envelope; recompute via the same path as
        // service_send_raw_txn to keep round-trip parity.
        let hash = *match &env {
            TxEnvelope::Legacy(signed) => signed.hash(),
            _ => unreachable!(),
        };
        WriteJob::Ger {
            ger_bytes: [0u8; 32],
            envelope: env,
            signer,
            eth_tx_hash: hash,
            job_id: Ulid::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_panic_is_contained_and_next_task_still_runs() {
        let err = supervise_dispatch(async {
            panic!("injected writer dispatch panic");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await
        .expect_err("the panic must be converted into a job error");
        assert!(err.downcast_ref::<WriterDispatchPanic>().is_some());

        supervise_dispatch(async { Ok(()) })
            .await
            .expect("the supervisor must remain usable after a panic");
    }

    #[tokio::test]
    async fn durable_note_handoff_preserves_pending_worker_outcome() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let hash = TxHash::from([0xabu8; 32]);
        assert!(!preserve_pending_after_handoff(&store, hash).await);
        store
            .prepare_note_handoff(&format!("{hash:#x}"), "commitment", "note-id", 10)
            .await
            .unwrap();
        assert!(preserve_pending_after_handoff(&store, hash).await);
    }

    /// Regression: expiry authority belongs to the worker that dequeued the
    /// item. An over-age queued job is failed before `dispatch_job` is ever
    /// constructed, so no Miden request can execute after status:0x0.
    #[tokio::test]
    async fn over_age_queued_job_expires_before_miden_dispatch() {
        let service = crate::test_helpers::create_test_service();
        let miden_client = service.miden_client.clone();
        let nonce = service.store.nonce_get("queue-expiry-test").await.unwrap();
        let mut job = fake_ger_job(nonce);
        if let WriteJob::Ger { ger_bytes, .. } = &mut job {
            *ger_bytes = [0x42; 32];
        }
        let hash = job.eth_tx_hash();

        service
            .store
            .txn_begin(
                hash,
                crate::store::TxnEntry {
                    id: None,
                    envelope: job.envelope().clone(),
                    signer: job.signer(),
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();

        let (_sender, receiver) = mpsc::channel(1);
        let inflight = Arc::new(DashMap::new());
        let mut entry = InFlightEntry::from_job(&job);
        entry.created_at = Instant::now() - Duration::from_secs(2);
        inflight.insert(hash, entry);
        let worker = WriterWorker {
            receiver,
            inflight: inflight.clone(),
            service,
            tx_ttl: Duration::from_secs(1),
        };

        worker.process(job).await;

        assert_eq!(
            miden_client.test_call_count(),
            0,
            "queue-age expiry must happen before dispatch reaches MidenClient::with"
        );
        assert_eq!(inflight.get(&hash).unwrap().state, JobState::Failed);
        let receipt = worker
            .service
            .store
            .txn_receipt(hash)
            .await
            .unwrap()
            .expect("queue expiry writes a terminal receipt");
        assert!(receipt.0.is_err());
    }

    /// Smoke test: parse_*_env returns defaults when env unset.
    #[test]
    fn env_parsers_default_when_unset() {
        // SAFETY: tests run sequentially in `#[test]` for this module unless
        // explicitly marked otherwise; we don't run these alongside ones
        // that set the env. Best-effort cleanup is up to the test author.
        unsafe {
            std::env::remove_var(QUEUE_DEPTH_ENV);
            std::env::remove_var(TX_TTL_ENV);
        }
        assert_eq!(WriterWorker::parse_queue_depth_env(), DEFAULT_QUEUE_DEPTH);
        assert_eq!(
            WriterWorker::parse_tx_ttl_env(),
            Duration::from_secs(DEFAULT_TX_TTL_SECS)
        );
    }

    /// Spec A — `WriteJob` fields are preserved through Clone (sanity-check
    /// the Phase-1.5 wire shape that Phase 1.5 / v1.5 will bincode-serialize).
    #[test]
    fn write_job_clone_round_trip() {
        let job = fake_ger_job(7);
        let cloned = job.clone();
        assert_eq!(job.eth_tx_hash(), cloned.eth_tx_hash());
        assert_eq!(job.signer(), cloned.signer());
        assert_eq!(job.job_id(), cloned.job_id());
        assert_eq!(job.kind(), cloned.kind());
    }

    /// WriteJobKind labels are stable across releases — the Prometheus label
    /// space is part of the alert contract.
    #[test]
    fn kind_labels_are_stable() {
        assert_eq!(WriteJobKind::Claim.as_str(), "claim");
        assert_eq!(WriteJobKind::GerInsert.as_str(), "ger_insert");
    }

    /// JobState terminality tracks the receipt contract: only Committed and
    /// Failed write a non-null `eth_getTransactionReceipt`.
    #[test]
    fn job_state_terminal_classification() {
        assert!(!JobState::Queued.is_terminal());
        assert!(!JobState::Submitting.is_terminal());
        assert!(JobState::Committed { block_number: 1 }.is_terminal());
        assert!(JobState::Failed.is_terminal());
    }

    /// `DecodedWriteCall::Ger` carries the combined `ger_bytes` through
    /// `into_job` into `WriteJob::Ger`. The decomposed mainnet/rollup exit roots
    /// are no longer threaded — the `SyntheticProjector` re-derives them from the
    /// consumed `UpdateGerNote`, so only the combined hash needs to reach Miden.
    #[test]
    fn decoded_ger_into_job_preserves_ger_bytes() {
        let decoded = DecodedWriteCall::Ger {
            ger_bytes: [3u8; 32],
        };
        let (env, addr) = fake_envelope(0);
        let hash = *match &env {
            TxEnvelope::Legacy(signed) => signed.hash(),
            _ => unreachable!(),
        };
        let job = decoded.into_job(env, addr, hash);
        match job {
            WriteJob::Ger { ger_bytes, .. } => {
                assert_eq!(ger_bytes, [3u8; 32]);
            }
            _ => panic!("expected WriteJob::Ger"),
        }
    }

    /// `InFlightEntry::from_job` snapshots the envelope so subsequent
    /// `eth_getTransactionByHash` reads can return the pending wire shape
    /// without keeping the original `WriteJob` alive.
    #[test]
    fn inflight_entry_captures_envelope() {
        let job = fake_ger_job(0);
        let captured_hash = job.eth_tx_hash();
        let entry = InFlightEntry::from_job(&job);
        assert_eq!(entry.state, JobState::Queued);
        assert_eq!(entry.eth_tx_hash, captured_hash);
        assert_eq!(entry.kind, WriteJobKind::GerInsert);
        assert!(entry.terminal_at.is_none());
    }

    /// `WriterQueueSaturatedError` is the downcast sentinel used by
    /// `service.rs::eth_sendRawTransaction` to switch the JSON-RPC error code
    /// to `-32005`. Verify it round-trips through `anyhow::Error` so the
    /// `.downcast_ref::<WriterQueueSaturatedError>()` lookup works.
    #[test]
    fn writer_queue_saturated_error_downcasts_through_anyhow() {
        let err: anyhow::Error = WriterQueueSaturatedError.into();
        assert!(err.downcast_ref::<WriterQueueSaturatedError>().is_some());
    }

    // ── Integration tests against the real worker task ─────────────────
    //
    // These spawn a `WriterWorker` against a `create_test_service()`
    // `ServiceState`. The GER variant goes through the full dispatch +
    // Miden round-trip (the test MidenClient stub records the call and
    // returns a successful submission), the Claim variant exercises the
    // zero-amount short-circuit so we don't need to seed faucets / GERs.

    use crate::test_helpers::create_test_service;
    use crate::writer_worker::DecodedWriteCall;
    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::consensus::{Signed, TxEnvelope};
    use alloy::eips::{Decodable2718, Encodable2718};
    use alloy::primitives::{FixedBytes, Signature, TxHash};
    use alloy_core::sol_types::SolCall;

    /// Build a legacy tx envelope mirroring the helper in
    /// `service_send_raw_txn::tests` so the hash matches what
    /// `eth_sendRawTransaction` would compute on the wire.
    fn encode_legacy_envelope(input: Vec<u8>) -> (TxEnvelope, Address, TxHash) {
        let txn = alloy::consensus::TxLegacy {
            input: input.into(),
            chain_id: Some(1),
            ..Default::default()
        };
        let signature = Signature::test_signature();
        let signed = Signed::new_unchecked(txn, signature, TxHash::default());
        let envelope = TxEnvelope::Legacy(signed);
        let signer = envelope.recover_signer().expect("recover signer");
        // Re-encode + decode so the hash matches what unwrap_txn_envelope sees.
        let mut buf = Vec::new();
        envelope.encode_2718(&mut buf);
        let mut slice = buf.as_slice();
        let env2 = TxEnvelope::decode_2718(&mut slice).expect("decode round-trip");
        let hash = match &env2 {
            TxEnvelope::Legacy(s) => *s.hash(),
            _ => unreachable!(),
        };
        (env2, signer, hash)
    }

    /// End-to-end: enqueue a GER WriteJob, let the worker dispatch it, and
    /// assert the store reflects the commit. Mirrors the legacy-path
    /// `test_insert_global_exit_root_stores_ger_and_emits_log` test, but
    /// via the writer-worker.
    #[tokio::test]
    async fn worker_dispatches_ger_job_end_to_end() {
        let service = create_test_service();
        let store = service.store.clone();
        let ger_bytes = [0xCDu8; 32];

        let (handle, _shutdown) = WriterWorker::spawn(service.clone(), 8, Duration::from_secs(60));

        let calldata = crate::ger::insertGlobalExitRootCall {
            root: FixedBytes::from(ger_bytes),
        }
        .abi_encode();
        let (env, signer, hash) = encode_legacy_envelope(calldata);
        let job = DecodedWriteCall::Ger { ger_bytes }.into_job(env, signer, hash);

        handle.try_enqueue(job).expect("enqueue must succeed");

        // The dispatch is async; poll until the in-flight entry transitions
        // to a terminal state (worker has finished). Bound the wait so a
        // genuinely-stuck worker fails the test instead of hanging CI.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(entry) = handle.get_inflight(&hash)
                && entry.state.is_terminal()
            {
                assert_eq!(
                    entry.state,
                    JobState::Committed { block_number: 0 },
                    "the worker JOB reaches Committed at the current tip (test store starts at \
                     0) — a lifecycle marker, not a receipt write. The eth receipt itself is \
                     recorded PENDING; the SyntheticProjector finalises it (and emits the GER \
                     log) when it observes the UpdateGerNote consumed, so receipt-block == \
                     log-block"
                );
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "worker did not reach terminal state within 10s; inflight = {:?}",
                    handle.get_inflight(&hash)
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The SyntheticProjector — not the writer worker / insert_ger — marks the
        // GER seen + injected and emits the synthetic log when it observes the
        // UpdateGerNote consumed. The worker's job here is solely to submit the
        // note and record the injection-tx receipt (asserted above).
        assert!(
            !store.is_ger_injected(&ger_bytes).await.unwrap(),
            "insert_ger must NOT mark injected — the projector does on consumption"
        );
    }

    /// Backpressure path: with a `mpsc(1)` channel, the first try_enqueue
    /// is accepted (sits in the channel), the second hits `QueueFull` and
    /// is converted to `WriterQueueSaturatedError`. The inflight map must
    /// be rolled back on the failed enqueue so a later legitimate retry of
    /// the same hash isn't masked as "already accepted".
    #[tokio::test]
    async fn try_enqueue_returns_queue_full_when_channel_at_cap() {
        let service = create_test_service();
        // Tiny channel so we can saturate predictably; no real worker
        // spawned — we just want to drive the `try_send` codepath.
        let (sender, _receiver) = mpsc::channel::<WriteJob>(1);
        let inflight = Arc::new(DashMap::new());
        let handle = WriterWorkerHandle {
            sender,
            inflight,
            queue_depth: 1,
        };
        // mute warnings about unused `service` — we needed it to ensure the
        // test compiles against the same Send/Sync bounds as the real path.
        let _ = service;

        let job_a = fake_ger_job(0);
        handle
            .try_enqueue(job_a)
            .expect("first enqueue takes the slot");
        assert_eq!(handle.inflight_len(), 1);

        let job_b = fake_ger_job(1);
        let hash_b = job_b.eth_tx_hash();
        match handle.try_enqueue(job_b) {
            Err(TryEnqueueError::QueueFull) => {}
            other => panic!("expected QueueFull, got {other:?}"),
        }
        // Rollback proof: the hash that failed to enqueue must NOT be left
        // in the inflight map as a phantom "accepted" entry.
        assert!(!handle.is_inflight(&hash_b));
        // The accepted entry stays.
        assert_eq!(handle.inflight_len(), 1);
    }

    /// Shutdown drains cleanly: send a job, fire the shutdown signal, and
    /// verify the worker exits within a small bound. Phase 5 lands the
    /// fuller graceful-drain semantics (-32005 mid-shutdown, fixed drain
    /// budget); this is the minimum-viable shape.
    #[tokio::test]
    async fn worker_shuts_down_on_signal() {
        let service = create_test_service();
        let (handle, shutdown) = WriterWorker::spawn(service, 8, Duration::from_secs(60));
        // Don't enqueue anything — just verify shutdown path is reachable.
        assert_eq!(handle.inflight_len(), 0);
        drop(shutdown);
        // No deterministic way to await the task exit without exposing the
        // JoinHandle (Phase 5 will). A short sleep gives tokio time to
        // process the dropped oneshot; the test passes if nothing panics.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// RD-940 Spec D — the in-flight pending-tx JSON must conform to
    /// geth's wire shape: `blockHash`, `blockNumber`, `transactionIndex`
    /// are the ONLY fields permitted to be JSON `null`. Every other
    /// numeric field must be a hex string (Go's `hexutil.Uint{,64}` /
    /// `hexutil.Big` value-type unmarshallers panic on `null`).
    #[tokio::test]
    async fn build_inflight_pending_tx_json_emits_geth_wire_shape() {
        // Build an in-flight entry directly so the test doesn't need a
        // full worker.
        let (env, signer) = fake_envelope(5);
        let hash = match &env {
            TxEnvelope::Legacy(s) => *s.hash(),
            _ => unreachable!(),
        };
        let entry = InFlightEntry {
            state: JobState::Submitting,
            eth_tx_hash: hash,
            signer,
            kind: WriteJobKind::Claim,
            job_id: Ulid::new(),
            envelope: env,
            created_at: Instant::now(),
            terminal_at: None,
        };
        let chain_id = 2u64;
        let json = crate::service_helpers::build_inflight_pending_tx_json(&entry, chain_id);

        // The three load-bearing nulls — pending shape requires these.
        assert!(json.get("blockHash").unwrap().is_null());
        assert!(json.get("blockNumber").unwrap().is_null());
        assert!(json.get("transactionIndex").unwrap().is_null());

        // Every other field that aggkit's monitor reads MUST be a string,
        // never null. (Go's hexutil.* value types panic on null.)
        for required_string_field in &[
            "type", "nonce", "gasPrice", "gas", "value", "input", "v", "r", "s", "hash", "from",
            "chainId",
        ] {
            assert!(
                json.get(*required_string_field).unwrap().is_string(),
                "field {required_string_field} MUST be a hex string, not null/absent"
            );
        }

        // Hash + from + chainId must reflect the entry we built.
        assert_eq!(
            json["hash"].as_str().unwrap(),
            format!("{:#x}", hash),
            "hash must echo the tx_hash"
        );
        assert_eq!(
            json["from"].as_str().unwrap(),
            format!("{:#x}", signer),
            "from must be the recovered signer captured at enqueue"
        );
        assert_eq!(
            json["chainId"].as_str().unwrap(),
            format!("0x{chain_id:x}"),
            "chainId must echo the service chain_id"
        );
        // Nonce 5 from `fake_envelope(5)` round-trips.
        assert_eq!(json["nonce"].as_str().unwrap(), "0x5");
    }

    /// RD-940 Decision 4 (Phase 2): `count_non_terminal_for_signer` must
    /// count Queued + Submitting entries for the matching signer, and
    /// **not** count Committed / Failed entries (those are already
    /// reflected in `store.nonce_get`).
    #[tokio::test]
    async fn count_non_terminal_for_signer_filters_correctly() {
        // No real worker spawned; we just need a handle to mutate the
        // inflight map directly.
        let (sender, _receiver) = mpsc::channel::<WriteJob>(8);
        let inflight = Arc::new(DashMap::new());
        let handle = WriterWorkerHandle {
            sender,
            inflight: inflight.clone(),
            queue_depth: 8,
        };

        let signer_a = Address::from([0x11u8; 20]);
        let signer_b = Address::from([0x22u8; 20]);

        // Helper to drop an entry directly into the map without going
        // through try_enqueue (which also sends on the channel).
        let insert = |hash: TxHash, signer: Address, state: JobState| {
            inflight.insert(
                hash,
                InFlightEntry {
                    state,
                    eth_tx_hash: hash,
                    signer,
                    kind: WriteJobKind::Claim,
                    job_id: Ulid::new(),
                    envelope: fake_envelope(0).0,
                    created_at: Instant::now(),
                    terminal_at: None,
                },
            );
        };

        // Signer A: 2 Queued + 1 Submitting + 1 Committed + 1 Failed = 3 non-terminal.
        insert(TxHash::from([0xA1u8; 32]), signer_a, JobState::Queued);
        insert(TxHash::from([0xA2u8; 32]), signer_a, JobState::Queued);
        insert(TxHash::from([0xA3u8; 32]), signer_a, JobState::Submitting);
        insert(
            TxHash::from([0xA4u8; 32]),
            signer_a,
            JobState::Committed { block_number: 1 },
        );
        insert(TxHash::from([0xA5u8; 32]), signer_a, JobState::Failed);

        // Signer B: 1 Queued = 1 non-terminal. (Cross-signer noise.)
        insert(TxHash::from([0xB1u8; 32]), signer_b, JobState::Queued);

        assert_eq!(handle.count_non_terminal_for_signer(&signer_a), 3);
        assert_eq!(handle.count_non_terminal_for_signer(&signer_b), 1);
        assert_eq!(
            handle.count_non_terminal_for_signer(&Address::from([0xFFu8; 20])),
            0,
            "unknown signer must return 0"
        );
    }
}
