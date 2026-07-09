//! Synthetic projector — the (future) sole owner of the synthetic EVM chain.
//!
//! See `docs/SYNTHETIC-INDEXER-REDESIGN.md`. The projector follows the Miden
//! chain block-by-block on a persisted cursor and, for each Miden block `N`,
//! derives the synthetic events of the notes *consumed at block N*
//! (`nullifier_block_height == N`) in a deterministic order, emitting exactly
//! one synthetic block `N`. A single ordered projector means **no ad-hoc
//! block-number reservation and no race** (Finding #5 eliminated by
//! construction): catch-up (cursor → tip) *is* recovery *is* the normal loop.
//!
//! ## ⚠️ SINGLE-PROCESS ONLY — multiple replicas are NOT supported ⚠️
//!
//! The cursor and the synthetic tip are owned by exactly one in-process
//! projector. Running two projectors (two replicas) against the same store
//! would double-advance the tip and interleave log emission non-
//! deterministically. The deployment MUST guarantee a single projector
//! instance; this is a hard invariant from the design doc, asserted loudly at
//! the cut-over phase.
//!
//! ## The sole synthetic-event producer
//!
//! The projector is ALWAYS registered as a [`SyncListener`] and is the **only**
//! synthetic-event producer and the **only** advancer of `latest_block_number`.
//! The legacy writer paths now only submit to Miden (the user's B2AGG note, the
//! ger_manager UpdateGerNote, the CLAIM note); they emit no synthetic logs and
//! never touch the tip. The projector re-derives every BridgeEvent / ClaimEvent /
//! GER `UpdateHashChainValue` log from the consumed Miden notes.
//!
//! The projector writes into the store exactly the way `restore` does — through
//! the shared `project_b2agg_note` / `project_claim_note` / `project_ger_note`
//! derivations — and is idempotent via the existing `is_*_processed` /
//! `is_ger_injected` dedup keys.
//!
//! ## Determinism + numbering contract (Miden-1:1)
//!
//! Synthetic block N == Miden block N. Every synthetic log derived from notes
//! consumed at Miden block N is written at synthetic block N, and the tip is
//! advanced to N once, **after** the block (write-before-advance) — including for
//! EMPTY Miden blocks, so the synthetic chain mirrors Miden block-for-block and
//! `eth_blockNumber` tracks the Miden tip. Within a Miden block, consumed notes
//! are ordered by `(consumed_tx_order, note_id)` before deriving, so re-running
//! the projector over the same chain yields byte-identical synthetic blocks
//! (numbers, hashes, log order, log indices). Because the projector is the sole
//! assigner of the synthetic tip, there is no `get_latest()+1` reservation race —
//! Finding #5 is eliminated by construction.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::bridge_out::{B2AggConsumerClass, classify_b2agg_consumer, is_b2agg_note};
use crate::miden_client::{MidenClientLib, SyncListener};
use crate::restore::{
    B2AggRestoreOutcome, ClaimProjectOutcome, GerProjectOutcome, project_b2agg_note,
    project_claim_note, project_ger_note,
};
use crate::store::Store;
use miden_client::rpc::NodeRpcClient;
use miden_client::rpc::domain::note::FetchedNote;
use miden_client::rpc::domain::transaction::TransactionRecord;
use miden_client::store::input_note_states::ConsumedExternalNoteState;
use miden_client::store::{InputNoteRecord, InputNoteState, NoteFilter};
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{
    NoteAttachments, NoteDetails, NoteFile, NoteId, NoteMetadata, NoteTag, Nullifier,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Blocks per `sync_notes` window walked by the note-visibility reconciler.
/// Env-tunable via `RECONCILE_CHUNK`. Raised from the historical 200 with
/// testnet evidence (see `note_probe --bench-sweep`): the node happily serves
/// 1000- and 2000-block spans in near-constant time (~240ms for 200 blocks vs
/// ~310ms for 1000 — the call is latency-dominated), so a 1000-block window is
/// ~5x the blocks/s per request AND 5x fewer requests for the same sweep rate,
/// which is what keeps the concurrent catch-up under the public node's rate
/// limiter (sustained ~45 req/s of 200-block windows tripped it; 1000-block
/// windows finish the same span in a fifth of the requests).
const RECONCILE_CHUNK: u64 = 1_000;

/// Concurrent in-flight `sync_notes` window fetches during catch-up.
/// Env-tunable via `RECONCILE_CONCURRENCY`, hard-capped at
/// [`RECONCILE_CONCURRENCY_MAX`] to stay a polite RPC citizen.
const RECONCILE_CONCURRENCY_DEFAULT: usize = 8;
const RECONCILE_CONCURRENCY_MAX: usize = 16;

/// Per-tick time budget for the reconciler's catch-up loop, in milliseconds.
/// Env-tunable via `RECONCILE_TICK_BUDGET_MS`. Projection runs AFTER the
/// reconciler inside `tick`, so the budget bounds how long a deep catch-up can
/// starve projection (and the 5s sync cadence): at least one window batch is
/// always processed per tick (guaranteed progress), then the loop stops once
/// the budget is spent. When the sweep is caught up the budget is irrelevant —
/// the single near-tip window completes in one iteration exactly as before.
const RECONCILE_TICK_BUDGET_MS_DEFAULT: u64 = 2_000;

/// Parse a `u64` tuning knob from the environment, falling back to `default`
/// on absence or garbage (never panics at boot for a bad env var).
fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            tracing::warn!(var = name, value = %v, default, "unparsable env override — using default");
            default
        }),
        Err(_) => default,
    }
}

/// Thin seam over the node's `sync_notes` window fetch, so the catch-up driver
/// (window batching, concurrent fetch, strict-order low-water-mark cursor
/// advancement, tick budget) is unit-testable without a live node. The live
/// implementation is [`RpcReconcileFetcher`]; tests inject failing/slow fakes.
#[async_trait::async_trait]
pub(crate) trait ReconcileFetcher: Send + Sync {
    /// Ids of every tag-0 note committed in Miden blocks `[from, to]`.
    async fn sync_note_ids(&self, from: u64, to: u64) -> anyhow::Result<Vec<NoteId>>;
}

/// The live [`ReconcileFetcher`]: one `sync_notes` call over the window. The
/// underlying tonic client takes `&self`, clones its multiplexed HTTP/2
/// channel per call and is `Send + Sync`, so a single `Arc` handle is safe to
/// share across the concurrent window-fetch tasks.
struct RpcReconcileFetcher(Arc<dyn NodeRpcClient>);

#[async_trait::async_trait]
impl ReconcileFetcher for RpcReconcileFetcher {
    async fn sync_note_ids(&self, from: u64, to: u64) -> anyhow::Result<Vec<NoteId>> {
        let tags: BTreeSet<NoteTag> = BTreeSet::from([NoteTag::from(0u32)]);
        let blocks = self
            .0
            .sync_notes((from as u32).into(), (to as u32).into(), &tags)
            .await
            .map_err(|e| anyhow::anyhow!("sync_notes({from}..{to}): {e}"))?;
        Ok(blocks
            .iter()
            .flat_map(|b| b.notes.keys().copied())
            .collect())
    }
}

/// True iff `e` is miden-client's un-recoverable "note is private" import
/// rejection. A private note imported by [`NoteId`] lacks the details the
/// client needs, so `import_notes` fails (prod 0.15.5: "Incomplete imported
/// note is private"); a private *historical* note never becomes importable.
/// The note-visibility reconciler skips such notes instead of failing the
/// whole batch — otherwise it retries the same block window forever and the
/// retroactive-heal sweep freezes. Safe to skip: bridge exits (B2AGG) are
/// PUBLIC notes, so a private note is never a real exit. Matched on the
/// rendered error text (the client surfaces this opaquely by the time it
/// reaches us — matching on rendered text is the most reliable signal here).
fn is_private_note_import_error<E: std::fmt::Display + ?Sized>(e: &E) -> bool {
    format!("{e}").to_lowercase().contains("is private")
}

/// The synthetic projector. Owns the cursor (last projected Miden block height)
/// and, when registered as the live [`SyncListener`], is the **sole** assigner
/// of the synthetic tip (`Store::latest_block_number`) — so there is no
/// reservation race (Finding #5 eliminated by construction).
pub struct SyntheticProjector {
    store: Arc<dyn Store>,
    block_state: Arc<BlockState>,
    /// Bridge account id — the sole legitimate consumer of a bridge-out B2AGG
    /// note (MA#3) and the expected GER target (MA#28).
    bridge_id: AccountId,
    /// This rollup's AggLayer network id. Threaded into `project_b2agg_note` for
    /// the Cantina #13 self-target poison-leaf gate: a B2AGG bridge-out whose
    /// destination IS this network must NOT emit a synthetic BridgeEvent.
    local_network_id: u32,
    /// Expected GER sender (ger_manager, or service for legacy deployments).
    expected_ger_sender: AccountId,
    /// Expected CLAIM minter (`accounts.service` — the account `create_claim`
    /// mints every ClaimNote from). Together with `bridge_id` this backs the
    /// claim provenance gate: on a chain shared with a FOREIGN miden-agglayer
    /// deployment, foreign claims share our ClaimNote script root and must
    /// not be projected (see `restore::classify_claim_note`).
    expected_claim_sender: AccountId,
    /// L1 JSON-RPC endpoint for the Cantina #13 Layer-2 ERC-20 metadata
    /// recovery path (mirrors `BridgeOutScanner::l1_rpc_url`). Threaded into
    /// `project_b2agg_note` so legacy/DB-loss faucet rows with empty ERC-20
    /// metadata recover + validate instead of being skipped. `None` disables
    /// the L1 fallback (recovery then relies solely on the all-Miden candidate).
    l1_rpc_url: Option<String>,
    /// Last projected Miden block height — an in-memory cache of the persisted
    /// `Store::get_projector_cursor`. The projector is the single owner of this
    /// cursor (SINGLE-PROCESS ONLY) and persists every advance in `tick`.
    cursor: AtomicU64,
    /// Node RPC handle for the note-visibility reconciler. Externally-created
    /// public network notes (tag 0, e.g. B2AGG bridge-outs from an independent
    /// wallet) that are committed AND consumed between two of our sync points
    /// are NEVER delivered by tag/interest-based `sync_state` — the exits then
    /// silently vanish from the synthetic event stream (observed live: 15/26
    /// bridge-outs missing under load; the LET-divergence watchdog's exact
    /// signature). The reconciler walks blocks via `sync_notes` and imports
    /// unknown notes so consumption is re-discovered and projected. `None`
    /// disables reconciliation (unit tests).
    node_rpc: Option<Arc<dyn NodeRpcClient>>,
    /// Last Miden block swept by the reconciler — an in-memory cache of the
    /// persisted `Store::get_reconcile_cursor` (migration 010), mirroring the
    /// projection `cursor` above. Loaded in `new()` and persisted write-behind
    /// AFTER each sweep window completes, so the durable cursor never runs
    /// ahead of work actually done (a crash mid-window redoes that window —
    /// safe, the sweep is idempotent: known ids are skipped).
    ///
    /// History: this used to be memory-only (hardcoded to 0 at boot), so EVERY
    /// container restart re-walked the sweep from genesis — ~3h of resync and
    /// node load per restart on prod history. The very first boot (no
    /// persisted value → 0) still sweeps from genesis: that is the designed
    /// first-boot heal. Recovery flows (`--restore`, `--reset-miden-store`)
    /// and the `--resweep-from-genesis` escape hatch reset the persisted
    /// value to 0 deliberately.
    reconcile_cursor: AtomicU64,
    /// Reconciler sweep-window size in blocks (`RECONCILE_CHUNK` env override,
    /// default [`RECONCILE_CHUNK`]).
    reconcile_chunk: u64,
    /// Concurrent in-flight window fetches during catch-up
    /// (`RECONCILE_CONCURRENCY` env override, default
    /// [`RECONCILE_CONCURRENCY_DEFAULT`], capped at
    /// [`RECONCILE_CONCURRENCY_MAX`]).
    reconcile_concurrency: usize,
    /// Per-tick catch-up time budget (`RECONCILE_TICK_BUDGET_MS` env override,
    /// default [`RECONCILE_TICK_BUDGET_MS_DEFAULT`]).
    reconcile_budget: Duration,
    /// Note ids already projected (or attempted) by the late-consumption sweep,
    /// so the per-tick sweep doesn't re-issue `is_note_processed` store queries
    /// for the whole consumed set every 5s.
    swept: std::sync::Mutex<HashSet<[u8; 32]>>,
    /// Spent-before-import recovery queue. External B2AGG notes that were
    /// ALREADY CONSUMED when the reconciler imported them are silently dropped
    /// by miden-client 0.15 (`import_note_records_by_proof` applies
    /// `consumed_externally` to a fresh Expected-state record; the transition
    /// fails and the record is never persisted — observed live: import returns
    /// Ok, note absent from store, zero errors). Their BridgeEvents then never
    /// materialize. [`Self::recover_spent_before_import`] rebuilds such notes
    /// in-memory (full body via `get_notes_by_id`, spend block via the
    /// nullifier feed, consumer attribution via the bridge's transaction feed —
    /// the MA#3 gate) and queues them here; `tick` projects them through the
    /// SAME `project_b2agg_note` derivation and removes them only after the
    /// tick completes (mid-tick failure retries; `is_note_processed` dedups).
    direct_recovered: std::sync::Mutex<Vec<InputNoteRecord>>,
}

impl SyntheticProjector {
    /// Build a projector from the account configuration. The starting cursor is
    /// **loaded from the store** (`Store::get_projector_cursor`, 0 for a fresh
    /// chain), so a restart resumes catch-up from the last persisted block
    /// rather than re-scanning from genesis. `tick` persists each advance.
    pub async fn new(
        store: Arc<dyn Store>,
        block_state: Arc<BlockState>,
        accounts: &AccountsConfig,
        local_network_id: u32,
        l1_rpc_url: Option<String>,
        node_url: Option<String>,
        node_api_key: Option<String>,
    ) -> anyhow::Result<Self> {
        // Build a dedicated RPC handle for the note reconciler (the live
        // MidenClientLib does not expose its RPC client). Same URL resolution
        // as MidenClient itself.
        let node_rpc = match node_url.as_deref() {
            Some(url) => {
                let endpoint = crate::miden_client::parse_node_url(url)?;
                Some(crate::miden_client::build_rpc_client(
                    &endpoint,
                    10_000,
                    node_api_key.as_deref(),
                ))
            }
            None => None,
        };
        // MA#28 — same fallback as `restore_gers` / `submit_update_ger_note`:
        // legacy deployments without a dedicated ger_manager mint GER notes
        // from the service account.
        let expected_ger_sender = accounts
            .ger_manager
            .as_ref()
            .map(|a| a.0)
            .unwrap_or(accounts.service.0);
        let start_cursor = store.get_projector_cursor().await?;
        // Same pattern for the reconciler's sweep cursor (migration 010): a
        // restart resumes the sweep after the last persisted window instead of
        // re-walking from genesis (prod: ~3h resync per restart pre-fix). 0
        // (fresh deployment or a recovery-flow reset) means the full-history
        // heal sweep runs — grep target for the e2e restart regression check
        // (scripts/e2e-reconciler-cursor-persistence.sh).
        let start_reconcile = store.get_reconcile_cursor().await?;
        let reconcile_chunk = env_u64("RECONCILE_CHUNK", RECONCILE_CHUNK).max(1);
        let reconcile_concurrency = (env_u64(
            "RECONCILE_CONCURRENCY",
            RECONCILE_CONCURRENCY_DEFAULT as u64,
        ) as usize)
            .clamp(1, RECONCILE_CONCURRENCY_MAX);
        let reconcile_budget = Duration::from_millis(env_u64(
            "RECONCILE_TICK_BUDGET_MS",
            RECONCILE_TICK_BUDGET_MS_DEFAULT,
        ));
        tracing::info!(
            reconcile_cursor = start_reconcile,
            chunk = reconcile_chunk,
            concurrency = reconcile_concurrency,
            budget_ms = reconcile_budget.as_millis() as u64,
            "note reconciler: sweep cursor loaded — next sweep window starts at block {}",
            start_reconcile + 1
        );
        Ok(Self {
            store,
            block_state,
            bridge_id: accounts.bridge.0,
            local_network_id,
            expected_ger_sender,
            expected_claim_sender: accounts.service.0,
            l1_rpc_url,
            cursor: AtomicU64::new(start_cursor),
            node_rpc,
            reconcile_cursor: AtomicU64::new(start_reconcile),
            reconcile_chunk,
            reconcile_concurrency,
            reconcile_budget,
            swept: std::sync::Mutex::new(HashSet::new()),
            direct_recovered: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The next sweep window `[from, to]` the note-visibility reconciler will
    /// walk, or `None` when the sweep has caught up to `tip`. Factored out of
    /// [`Self::reconcile_notes`] so the restart-resume contract is directly
    /// unit-testable: `from` is `reconcile_cursor + 1` — persisted-cursor + 1
    /// after a restart, NOT 1.
    fn next_reconcile_window(&self, tip: u64) -> Option<(u64, u64)> {
        let from = self.reconcile_cursor.load(Ordering::Acquire) + 1;
        if from > tip {
            return None;
        }
        Some((from, (from + self.reconcile_chunk - 1).min(tip)))
    }

    /// The next batch of up to `reconcile_concurrency` sweep windows —
    /// [`Self::next_reconcile_window`] extended forward without moving the
    /// cursor. Empty when the sweep has caught up to `tip`.
    fn plan_reconcile_windows(&self, tip: u64) -> Vec<(u64, u64)> {
        let mut windows = Vec::new();
        let Some((mut from, mut to)) = self.next_reconcile_window(tip) else {
            return windows;
        };
        loop {
            windows.push((from, to));
            if to >= tip || windows.len() >= self.reconcile_concurrency {
                return windows;
            }
            from = to + 1;
            to = (from + self.reconcile_chunk - 1).min(tip);
        }
    }

    /// Note-visibility reconciler (completeness guarantee for externally-created
    /// network notes). Walks `sync_notes` in `reconcile_chunk`-block windows and
    /// imports any tag-0 note the local store doesn't know. The next
    /// `sync_state` then discovers the (possibly historical) consumption via the
    /// nullifier check, and the late-consumption sweep in `tick` projects it.
    /// Non-B2AGG imports (MINTs to external wallets, etc.) are harmless: every
    /// `project_*` derivation gates on script root + consumer.
    ///
    /// Catch-up throughput: when the sweep is behind the tip, this processes
    /// MULTIPLE windows per tick — window fetches issued concurrently
    /// (`RECONCILE_CONCURRENCY` in flight), batches repeated until the
    /// `RECONCILE_TICK_BUDGET_MS` budget is spent — instead of the historical
    /// one-window-per-5s-tick cadence (a hard ~40 blocks/s ceiling that made
    /// full-history sweeps take 3+ hours regardless of node speed).
    async fn reconcile_notes(
        &self,
        client: &mut MidenClientLib,
        rpc: &Arc<dyn NodeRpcClient>,
        tip: u64,
    ) -> anyhow::Result<()> {
        let fetcher: Arc<dyn ReconcileFetcher> = Arc::new(RpcReconcileFetcher(Arc::clone(rpc)));
        self.reconcile_notes_with(Some(client), Some(rpc.as_ref()), &fetcher, tip)
            .await
    }

    /// Catch-up driver behind [`Self::reconcile_notes`], with the window fetch
    /// abstracted behind [`ReconcileFetcher`] so the ordering/budget contract is
    /// unit-testable. `client`/`rpc` are only touched when a window actually
    /// has candidate notes (tests drive empty/failed windows with `None`).
    ///
    /// ORDERING SAFETY (low-water mark): window results are processed strictly
    /// in ascending block order, and the persisted cursor advances to a
    /// window's `to` only after that window — and therefore every window below
    /// it — completed successfully. A failed fetch aborts the batch at the
    /// failed window: earlier windows keep their advancement, later (already
    /// fetched) results are DISCARDED and re-fetched next tick, so the cursor
    /// can never advance past a gap. Same write-behind persist-then-cache
    /// ordering per window as before: the durable cursor never runs ahead of
    /// work actually done, and a crash mid-window redoes that window (the
    /// sweep is idempotent — known ids are skipped).
    async fn reconcile_notes_with(
        &self,
        mut client: Option<&mut MidenClientLib>,
        rpc: Option<&dyn NodeRpcClient>,
        fetcher: &Arc<dyn ReconcileFetcher>,
        tip: u64,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + self.reconcile_budget;
        loop {
            let windows = self.plan_reconcile_windows(tip);
            if windows.is_empty() {
                return Ok(());
            }
            // Caught-up fast path: a single (near-tip) window is fetched inline
            // — identical behavior and cost to the historical per-tick sweep.
            let mut results: Vec<(u64, u64, anyhow::Result<Vec<NoteId>>)> =
                if let [(from, to)] = windows[..] {
                    vec![(from, to, fetcher.sync_note_ids(from, to).await)]
                } else {
                    let mut set = tokio::task::JoinSet::new();
                    for (from, to) in windows {
                        let fetcher = Arc::clone(fetcher);
                        set.spawn(async move { (from, to, fetcher.sync_note_ids(from, to).await) });
                    }
                    let mut out = Vec::with_capacity(set.len());
                    while let Some(joined) = set.join_next().await {
                        out.push(joined.map_err(|e| {
                            anyhow::anyhow!("reconcile window-fetch task panicked: {e}")
                        })?);
                    }
                    out
                };
            results.sort_unstable_by_key(|(from, _, _)| *from);
            for (from, to, fetched) in results {
                let candidates = fetched.map_err(|e| {
                    // Low-water mark: never advance past a failed window. The
                    // windows before this one already advanced the cursor;
                    // everything from here on is re-fetched next tick.
                    anyhow::anyhow!(
                        "reconcile window {from}..{to} failed — cursor held at {} (retry next tick): {e:#}",
                        self.reconcile_cursor.load(Ordering::Acquire)
                    )
                })?;
                if !candidates.is_empty() {
                    let client = client.as_deref_mut().ok_or_else(|| {
                        anyhow::anyhow!(
                            "reconcile window {from}..{to}: candidate notes but no client handle"
                        )
                    })?;
                    let rpc = rpc.ok_or_else(|| {
                        anyhow::anyhow!(
                            "reconcile window {from}..{to}: candidate notes but no rpc handle"
                        )
                    })?;
                    self.import_reconcile_window(client, rpc, from, to, &candidates)
                        .await?;
                }
                // Persist write-behind AFTER the window's work completed, and
                // BEFORE updating the in-memory cache (same ordering guarantee
                // as the projection cursor in `tick`): the durable cursor never
                // runs ahead of work actually done. A persist failure fails
                // this tick (warn + retry in `tick`), leaving the in-memory
                // cursor un-advanced so the window is re-swept.
                self.store.set_reconcile_cursor(to).await?;
                self.reconcile_cursor.store(to, Ordering::Release);
                metrics::gauge!("synthetic_reconciler_cursor").set(to as f64);
            }
            // Budget check AFTER the batch: at least one batch always runs
            // (guaranteed progress even under a zero/tiny budget), and
            // projection — which runs after reconcile in `tick` — is never
            // starved by a deep catch-up.
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    /// Import the unknown notes of ONE sweep window `[from, to]` given the
    /// window's candidate note ids. This is the historical per-window body of
    /// `reconcile_notes`, moved verbatim: unknown-ids diff, atomic batch import
    /// with the private-note per-note skip fallback (0.15.5 wedge hotfix), and
    /// the spent-before-import recovery re-query. Runs SEQUENTIALLY per window
    /// on the single client — only the window fetches are concurrent.
    async fn import_reconcile_window(
        &self,
        client: &mut MidenClientLib,
        rpc: &dyn NodeRpcClient,
        from: u64,
        to: u64,
        candidates: &[NoteId],
    ) -> anyhow::Result<()> {
        {
            let known: HashSet<NoteId> = client
                .get_input_notes(NoteFilter::List(candidates.to_vec()))
                .await
                .map_err(|e| anyhow::anyhow!("get_input_notes(List): {e}"))?
                .into_iter()
                .filter_map(|rec| rec.id())
                .collect();
            let unknown_ids: Vec<NoteId> = candidates
                .iter()
                .filter(|id| !known.contains(id))
                .copied()
                .collect();
            if !unknown_ids.is_empty() {
                // HOTFIX (0.15.5 reconciler wedge): fast path is ONE atomic
                // batch import; only fall back to the slower per-note import if
                // the batch fails because a note is private. miden-client
                // rejects an import with "Incomplete imported note is private",
                // and a private *historical* note never becomes importable — so
                // an atomic batch containing one private note fails on every
                // tick and freezes the sweep on the same block window forever
                // (the retry path treats it as transient, but it is not). The
                // per-note retry skips just the private notes: bridge exits
                // (B2AGG) are PUBLIC notes, so a private note is never a real
                // exit — skipping it cannot drop an exit. The
                // `synthetic_reconciler_private_skipped_total` metric keeps the
                // skips auditable.
                let files: Vec<NoteFile> =
                    unknown_ids.iter().map(|id| NoteFile::NoteId(*id)).collect();
                let (attempted, skipped_private): (Vec<NoteId>, usize) = match client
                    .import_notes(&files)
                    .await
                {
                    // Common case: whole batch imported in one call.
                    Ok(_) => (unknown_ids.clone(), 0),
                    // Batch poisoned by >=1 private note — retry per-note,
                    // skipping the private ones so the sweep can advance.
                    Err(e) if is_private_note_import_error(&e) => {
                        let mut ok: Vec<NoteId> = Vec::with_capacity(unknown_ids.len());
                        let mut skipped = 0usize;
                        for id in &unknown_ids {
                            let file = NoteFile::NoteId(*id);
                            match client.import_notes(std::slice::from_ref(&file)).await {
                                Ok(_) => ok.push(*id),
                                Err(e) if is_private_note_import_error(&e) => {
                                    skipped += 1;
                                    metrics::counter!("synthetic_reconciler_private_skipped_total")
                                        .increment(1);
                                    tracing::warn!(
                                        note_id = %id,
                                        from,
                                        to,
                                        "note reconciler: skipping un-importable private \
                                         network note"
                                    );
                                }
                                // Any other per-note failure stays fatal.
                                Err(e) => {
                                    return Err(anyhow::anyhow!(
                                        "import_notes(1, note_id={id}, window={from}..{to}): {e}"
                                    ));
                                }
                            }
                        }
                        (ok, skipped)
                    }
                    // Non-private batch failure is still fatal (stays loud).
                    Err(e) => {
                        let n = unknown_ids.len();
                        return Err(anyhow::anyhow!("import_notes({n}): {e}"));
                    }
                };
                let imported = attempted.len();
                if imported > 0 {
                    metrics::counter!("synthetic_reconciler_notes_imported_total")
                        .increment(imported as u64);
                }
                tracing::info!(
                    imported,
                    skipped_private,
                    from,
                    to,
                    "note reconciler: imported network notes missed by sync"
                );
                // Spent-before-import recovery: `import_notes` returns Ok even
                // for notes it silently DROPPED because they were already
                // consumed at import time (miden-client 0.15 bug — see the
                // `direct_recovered` field docs). Re-query which of the
                // attempted ids actually landed; the rest must be projected
                // directly from node data or their BridgeEvents are lost.
                if !attempted.is_empty() {
                    let landed: HashSet<NoteId> = client
                        .get_input_notes(NoteFilter::List(attempted.clone()))
                        .await
                        .map_err(|e| anyhow::anyhow!("get_input_notes(List) post-import: {e}"))?
                        .into_iter()
                        .filter_map(|rec| rec.id())
                        .collect();
                    let missing: Vec<NoteId> = attempted
                        .into_iter()
                        .filter(|id| !landed.contains(id))
                        .collect();
                    if !missing.is_empty() {
                        metrics::counter!("synthetic_reconciler_import_dropped_total")
                            .increment(missing.len() as u64);
                        tracing::warn!(
                            dropped = missing.len(),
                            from,
                            to,
                            "note reconciler: import silently dropped consumed notes; \
                             attempting direct projection recovery"
                        );
                        self.recover_spent_before_import(rpc, &missing).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Direct-projection recovery for notes that were already CONSUMED when the
    /// reconciler imported them (and were therefore silently dropped by
    /// miden-client's import — see the [`Self::direct_recovered`] field docs).
    /// The node still serves everything needed (`sync_notes` / `get_notes_by_id`
    /// both return consumed notes), so bypass the client store:
    ///
    /// 1. Fetch the full public note bodies via `get_notes_by_id`. Private notes
    ///    cannot be reconstructed and B2AGG bridge-outs are public network
    ///    notes; non-B2AGG public notes (e.g. MINTs to external wallets) derive
    ///    no synthetic event, and CLAIM/GER notes are created by our own
    ///    service so they always reach the store through the normal path.
    /// 2. Resolve each note's spend block from the node's nullifier feed.
    /// 3. **MA#3 reclaim gate** — a B2AGG can be consumed by the bridge (real
    ///    exit → must emit) or reclaimed by its sender (asset stayed → must NOT
    ///    emit), and the nullifier alone doesn't say who consumed. The bridge's
    ///    LET frontier map stores only the O(log n) Merkle frontier nodes
    ///    (overwritten as leaves append), so a direct "leaf present in LET"
    ///    check is not implementable. Instead we use a strictly precise gate:
    ///    the node's per-account `sync_transactions` feed for the BRIDGE
    ///    account, whose transaction headers commit to the nullifiers of the
    ///    notes each transaction consumed. "A bridge-executed transaction
    ///    consumed this nullifier" is exactly the condition
    ///    `classify_b2agg_consumer == Emit` encodes (consumer == bridge), from
    ///    the same trust root as every other projector input (the node RPC).
    ///    Anything else is treated as reclaim/unknown and skipped fail-closed
    ///    with a WARN + metric.
    /// 4. Queue an in-memory `ConsumedExternal` record (consumer = bridge) that
    ///    `tick` runs through the SAME `project_b2agg_note` derivation as every
    ///    other note (store dedup via `is_note_processed` keeps it idempotent).
    async fn recover_spent_before_import(
        &self,
        rpc: &dyn NodeRpcClient,
        missing: &[NoteId],
    ) -> anyhow::Result<()> {
        struct Candidate {
            id: NoteId,
            details: NoteDetails,
            attachments: NoteAttachments,
            nullifier: Nullifier,
        }

        let fetched = rpc
            .get_notes_by_id(missing)
            .await
            .map_err(|e| anyhow::anyhow!("get_notes_by_id({}): {e}", missing.len()))?;

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut min_inclusion = BlockNumber::from(u32::MAX);
        for f in fetched {
            let id = f.id();
            let FetchedNote::Public(note, inclusion_proof) = f else {
                tracing::debug!(
                    note_id = %id.to_hex(),
                    "spent-before-import recovery: skipping private note (not reconstructable)"
                );
                continue;
            };
            let inclusion = inclusion_proof.location().block_num();
            let nullifier = note.nullifier();
            let attachments = note.attachments().clone();
            let details: NoteDetails = note.into();
            if !is_b2agg_note(&details) {
                tracing::debug!(
                    note_id = %id.to_hex(),
                    "spent-before-import recovery: skipping non-B2AGG note (no synthetic event)"
                );
                continue;
            }
            min_inclusion = min_inclusion.min(inclusion);
            candidates.push(Candidate {
                id,
                details,
                attachments,
                nullifier,
            });
        }
        if candidates.is_empty() {
            return Ok(());
        }

        // Spend blocks: nullifier consumption can only happen at or after the
        // note's inclusion block, so search from the batch minimum.
        let nullifiers: BTreeSet<Nullifier> = candidates.iter().map(|c| c.nullifier).collect();
        let heights = rpc
            .get_nullifier_commit_heights(nullifiers, min_inclusion)
            .await
            .map_err(|e| anyhow::anyhow!("get_nullifier_commit_heights: {e}"))?;

        // MA#3 gate data: every transaction the BRIDGE executed across the
        // spend-block range, with the nullifiers each one consumed.
        let spend_blocks: Vec<BlockNumber> = heights.values().flatten().copied().collect();
        let consumed_by_bridge: HashMap<Nullifier, (u64, u32, u32)> =
            match (spend_blocks.iter().min(), spend_blocks.iter().max()) {
                (Some(min_h), Some(max_h)) => {
                    let txs = rpc
                        .sync_transactions(*min_h, *max_h, vec![self.bridge_id])
                        .await
                        .map_err(|e| anyhow::anyhow!("sync_transactions({min_h}..{max_h}): {e}"))?;
                    bridge_consumed_nullifiers(&txs, self.bridge_id)
                }
                _ => HashMap::new(),
            };

        let mut recovered: Vec<InputNoteRecord> = Vec::new();
        for c in candidates {
            let Some(spend_block) = heights.get(&c.nullifier).copied().flatten() else {
                // Import dropped the note but its nullifier is NOT consumed —
                // outside the known drop mode (unconsumed notes persist fine).
                // Surface loudly; a restart re-sweeps from genesis and retries.
                metrics::counter!("synthetic_reconciler_missing_not_consumed_total").increment(1);
                tracing::error!(
                    note_id = %c.id.to_hex(),
                    "spent-before-import recovery: note missing from store but its \
                     nullifier is unspent — unexpected import drop mode; skipping \
                     (restart re-sweeps and retries)"
                );
                continue;
            };
            match consumed_by_bridge.get(&c.nullifier) {
                Some((block, tx_order, _within_tx_pos)) => {
                    debug_assert_eq!(*block, spend_block.as_u64());
                    let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                        nullifier_block_height: spend_block,
                        consumer_account: Some(self.bridge_id),
                        consumed_tx_order: Some(*tx_order),
                    });
                    let record = InputNoteRecord::new(c.details, c.attachments, None, state);
                    metrics::counter!("synthetic_reconciler_direct_recovered_total").increment(1);
                    tracing::info!(
                        note_id = %c.id.to_hex(),
                        spend_block = spend_block.as_u64(),
                        tx_order,
                        "spent-before-import recovery: bridge-consumed B2AGG verified via \
                         bridge transaction feed; queued for direct projection"
                    );
                    recovered.push(record);
                }
                None => {
                    // Fail-closed (MA#3): the note IS consumed, but no
                    // bridge-executed transaction at the spend block consumed
                    // its nullifier — sender reclaim or unknown consumer.
                    // Emitting would hand out a withdrawal for value that
                    // never left Miden, so skip and surface.
                    metrics::counter!("synthetic_reconciler_unverified_consumption_total")
                        .increment(1);
                    tracing::warn!(
                        note_id = %c.id.to_hex(),
                        spend_block = spend_block.as_u64(),
                        bridge = %self.bridge_id,
                        "spent-before-import recovery: consumed B2AGG was NOT consumed by \
                         any bridge transaction at its spend block — treating as \
                         reclaim/unknown consumer; skipping BridgeEvent (fail-closed, MA#3)"
                    );
                }
            }
        }
        if !recovered.is_empty() {
            let mut queue = self
                .direct_recovered
                .lock()
                .expect("direct-recovered queue poisoned");
            // Defensive dedup: a record already queued (not yet drained by
            // `tick`) must not be double-projected in one block.
            let queued: HashSet<[u8; 32]> = queue
                .iter()
                .map(|n| n.details_commitment().as_bytes())
                .collect();
            queue.extend(
                recovered
                    .into_iter()
                    .filter(|n| !queued.contains(&n.details_commitment().as_bytes())),
            );
        }
        Ok(())
    }

    /// Merge the spent-before-import recovery queue snapshot into `tick`'s
    /// per-block projection buckets. A note whose spend block is still ahead of
    /// the cursor projects at its real Miden block (Miden-1:1); a note whose
    /// spend block the cursor already passed projects into the FIRST block of
    /// this tick's window, exactly like the late-consumption sweep (sealed
    /// blocks + forward-only getLogs consumers). Notes whose spend block is
    /// beyond `tip` stay queued for a future tick. Returns the ids that were
    /// bucketed (to be removed from the queue once the tick completes).
    fn bucket_direct_notes<'a>(
        direct: &'a [InputNoteRecord],
        by_block: &mut HashMap<u64, Vec<&'a InputNoteRecord>>,
        cursor: u64,
        tip: u64,
    ) -> Vec<[u8; 32]> {
        let mut done = Vec::new();
        for note in direct {
            let Some(h) = note.state().consumed_block_height().map(|h| h.as_u64()) else {
                // Defensive: the queue only ever holds ConsumedExternal records.
                continue;
            };
            let target = h.max(cursor + 1);
            if target > tip {
                tracing::debug!(
                    spend_block = h,
                    tip,
                    "direct projection: spend block ahead of sync tip — deferring to next tick"
                );
                continue;
            }
            by_block.entry(target).or_default().push(note);
            done.push(note.details_commitment().as_bytes());
        }
        if !done.is_empty() {
            tracing::info!(
                recovered = done.len(),
                "direct projection: projecting spent-before-import notes this tick"
            );
        }
        done
    }

    /// Remove successfully-bucketed direct-recovery records from the queue —
    /// called only AFTER the tick completed, so a mid-tick failure retries them
    /// (idempotently, via `is_note_processed`) instead of dropping them.
    fn complete_direct_notes(&self, done: &[[u8; 32]]) {
        if done.is_empty() {
            return;
        }
        let done: HashSet<[u8; 32]> = done.iter().copied().collect();
        self.direct_recovered
            .lock()
            .expect("direct-recovered queue poisoned")
            .retain(|n| !done.contains(&n.details_commitment().as_bytes()));
    }

    /// The current cursor (last projected Miden block height).
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// Test-only override of the reconciler catch-up knobs (the live values
    /// come from the environment in [`Self::new`]).
    #[cfg(test)]
    fn with_reconcile_tuning(mut self, chunk: u64, concurrency: usize, budget: Duration) -> Self {
        self.reconcile_chunk = chunk;
        self.reconcile_concurrency = concurrency;
        self.reconcile_budget = budget;
        self
    }

    /// Project the notes consumed at one Miden block (`miden_block`) into the
    /// single synthetic block `miden_block` (**Miden-1:1**): every synthetic log
    /// derived from this block's notes is written at synthetic block == the Miden
    /// block, and the tip is advanced to `miden_block` once, AFTER the block
    /// (write-before-advance) — even when the block produced no logs, so the
    /// synthetic chain mirrors Miden block-for-block.
    ///
    /// Determinism: within the Miden block, consumed notes are ordered by
    /// `(consumed_tx_order, note_id_hex)` before deriving, so re-running over the
    /// same chain yields byte-identical synthetic blocks. Idempotent: the
    /// `project_*` derivations short-circuit on the existing dedup keys.
    ///
    /// `client` (the live `&mut MidenClientLib`) is threaded through to
    /// `project_b2agg_note` for the Cantina #13 Layer-2 ERC-20 metadata
    /// recovery (`None` in unit tests, where the in-memory feed is supplied
    /// directly).
    /// Filter `consumed` to the notes consumed at `miden_block`, then project
    /// them. Used by `project_block` (live single-block) and the unit tests;
    /// `tick` pre-groups the feed once and calls [`Self::project_block_notes`]
    /// directly to avoid an O(blocks × notes) per-block re-scan during catch-up.
    async fn project_notes(
        &self,
        consumed: &[InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        client: Option<&mut MidenClientLib>,
        within_tx: Option<&HashMap<Nullifier, (u32, u32)>>,
    ) -> anyhow::Result<usize> {
        let block_notes: Vec<&InputNoteRecord> = consumed
            .iter()
            .filter(|n| n.state().consumed_block_height().map(|h| h.as_u64()) == Some(miden_block))
            .collect();
        self.project_block_notes(
            &block_notes,
            output_metadata,
            miden_block,
            client,
            within_tx,
        )
        .await
    }

    /// Project the already-filtered notes consumed at `miden_block` into the
    /// single synthetic block `miden_block` (Miden-1:1), advancing the tip once
    /// after the block (write-before-advance), even when there are zero notes.
    ///
    /// `within_tx` (Cantina #7): optional map `nullifier → (per-block bridge-tx
    /// order, within-tx input position)` from [`bridge_consumed_nullifiers`],
    /// supplied by `tick` when a block contains several B2AGG notes consumed by
    /// the SAME transaction. Their `consumed_tx_order` ties; the within-tx
    /// position is the on-chain LET append order, so it must break the tie
    /// ahead of the (arbitrary) details-commitment.
    async fn project_block_notes(
        &self,
        block_notes: &[&InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        mut client: Option<&mut MidenClientLib>,
        within_tx: Option<&HashMap<Nullifier, (u32, u32)>>,
    ) -> anyhow::Result<usize> {
        let mut notes: Vec<&InputNoteRecord> = block_notes.to_vec();

        // Determinism: order intra-block events by (consumed_tx_order,
        // within-tx position, note-id). `consumed_tx_order` is the per-account
        // position of the consuming transaction within the block; the within-tx
        // position (when supplied — see `within_tx` above) is the on-chain
        // consumption order INSIDE one transaction; the 32-byte
        // details-commitment stays as the final deterministic tie-breaker.
        // Compare the commitment bytes directly — identical ordering to the old
        // hex-string compare, but no per-comparison allocation (matters when
        // many notes share a block).
        let within_key = |n: &&InputNoteRecord| -> (u32, u32) {
            n.nullifier()
                .and_then(|nf| within_tx.and_then(|m| m.get(&nf).copied()))
                .unwrap_or((u32::MAX, u32::MAX))
        };
        notes.sort_by(|a, b| {
            a.state()
                .consumed_tx_order()
                .cmp(&b.state().consumed_tx_order())
                .then_with(|| within_key(a).cmp(&within_key(b)))
                .then_with(|| {
                    a.details_commitment()
                        .as_bytes()
                        .cmp(&b.details_commitment().as_bytes())
                })
        });

        let bridge_address = get_bridge_address();

        // Miden-1:1 numbering: synthetic block N == Miden block N. Every synthetic
        // log for this Miden block is written AT block `miden_block`; the tip is
        // advanced exactly ONCE, after the whole block (below). The projector is
        // the SOLE advancer of `latest_block_number` — nothing else may touch it.
        let block_hash = self.block_state.get_block_hash(miden_block);
        let timestamp = self.block_state.get_block_timestamp(miden_block);

        let mut logs = 0usize;
        for note in notes {
            // A consumed note matches at most one of the three script roots, so
            // trying all three derivations emits at most one synthetic log per
            // note — the three restore derivations unified into one per-note loop.
            if project_b2agg_note(
                &self.store,
                note,
                self.bridge_id,
                self.local_network_id,
                miden_block,
                block_hash,
                bridge_address,
                // Cantina #13 recovery context: the live client + the projector's
                // L1 RPC, so legacy/empty-metadata ERC-20 bridge-outs recover.
                client.as_deref_mut(),
                self.l1_rpc_url.as_deref(),
            )
            .await?
                == B2AggRestoreOutcome::Emitted
            {
                logs += 1;
                continue;
            }

            if project_claim_note(
                &self.store,
                note,
                output_metadata,
                self.expected_claim_sender,
                self.bridge_id,
                miden_block,
                block_hash,
                bridge_address,
            )
            .await?
                == ClaimProjectOutcome::Emitted
            {
                logs += 1;
                continue;
            }

            if project_ger_note(
                &self.store,
                note,
                output_metadata,
                self.expected_ger_sender,
                self.bridge_id,
                miden_block,
                block_hash,
                timestamp,
            )
            .await?
                == GerProjectOutcome::Emitted
            {
                logs += 1;
                continue;
            }
        }

        // Write-before-advance: every synthetic log for `miden_block` is now in the
        // DB, so it is safe to advance the synthetic tip to == the Miden block.
        // Runs for EMPTY Miden blocks too (advance the tip even with 0 logs), so the
        // synthetic chain mirrors Miden block-for-block (eth_blockNumber == Miden tip).
        self.store.set_latest_block_number(miden_block).await?;

        Ok(logs)
    }

    /// Project one Miden block `miden_block` from the live client: fetch the
    /// consumed-note feed + our own output-note metadata (the MA#28 GER
    /// provenance fallback) through the passed `&mut MidenClientLib`, then run
    /// the deterministic [`Self::project_notes`] core. Returns the number of
    /// synthetic logs written.
    ///
    /// Fetching through the *passed* client (not `MidenClient::with`) is
    /// mandatory: `on_post_sync` already holds the client borrow inside the sync
    /// loop, and re-entering via `with` would deadlock the request queue.
    pub async fn project_block(
        &self,
        client: &mut MidenClientLib,
        miden_block: u64,
    ) -> anyhow::Result<usize> {
        // There is no server-side block-range filter for notes yet (see the
        // restore module TODOs), so pull the full consumed set and filter by
        // nullifier_block_height in `project_notes`.
        let consumed = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

        // Protocol 0.15: notes consumed by the bridge land as `ConsumedExternal`,
        // which carries NO metadata — so the MA#28 sender check in
        // `project_ger_note` needs the metadata from our own output-note records
        // (we minted those notes; the client store retains them permanently).
        let output_metadata: HashMap<[u8; 32], NoteMetadata> = client
            .get_output_notes(NoteFilter::All)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
            .into_iter()
            .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
            .collect();

        self.project_notes(&consumed, &output_metadata, miden_block, Some(client), None)
            .await
    }

    /// Process every Miden block from `cursor + 1` to the current Miden tip in
    /// order, projecting each one and advancing the cursor. Returns the new
    /// cursor (== the projected Miden tip).
    ///
    /// This is the normal projector loop; catch-up after a restart is the same
    /// code path (the cursor simply starts further behind the tip).
    pub async fn tick(&self, client: &mut MidenClientLib) -> anyhow::Result<u64> {
        let tip = client
            .get_sync_height()
            .await
            .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
            .as_u64();
        let mut cursor = self.cursor.load(Ordering::Acquire);
        // Reconcile BEFORE the early-return: the reconciler must run even on
        // ticks where the projector is already at the tip, or imports stall
        // whenever Miden block production pauses. Failures are transient —
        // warn and retry next tick, never block projection.
        if let Some(rpc) = self.node_rpc.clone()
            && let Err(e) = self.reconcile_notes(client, &rpc, tip).await
        {
            tracing::warn!(
                error = %format!("{e:#}"),
                "note reconciler failed (transient — will retry next tick)"
            );
        }
        if cursor >= tip {
            return Ok(cursor);
        }
        // Perf-critical: fetch the consumed-note feed + output-note metadata ONCE
        // per tick, NOT once per block. There is no server-side block-range filter
        // for notes, so a per-block fetch makes tick O(blocks × notes); the
        // projector then never catches up to the Miden tip (observed in e2e as the
        // bridge-in deposit never becoming claimable, because its GER injection
        // never gets projected). The feeds are owned, so `project_notes` filters
        // them per block by `nullifier_block_height` without re-fetching.
        let consumed = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;
        let output_metadata: HashMap<[u8; 32], NoteMetadata> = client
            .get_output_notes(NoteFilter::All)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
            .into_iter()
            .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
            .collect();
        // Spent-before-import recovery queue snapshot: records fabricated by
        // `recover_spent_before_import` are NOT in the client store, so they
        // never appear in the `consumed` feed above — merge them into the
        // buckets below. The queue itself is only pruned after the tick
        // completes (`complete_direct_notes`), so a mid-tick failure retries.
        let direct_notes: Vec<InputNoteRecord> = self
            .direct_recovered
            .lock()
            .expect("direct-recovered queue poisoned")
            .clone();
        // Pre-group the consumed feed by Miden block ONCE (not once per block): a
        // per-block re-scan of the full feed makes catch-up O(blocks_behind ×
        // total_consumed). Each block then projects from its precomputed bucket.
        let mut by_block: HashMap<u64, Vec<&InputNoteRecord>> = HashMap::new();
        for note in &consumed {
            if let Some(h) = note.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push(note);
            }
        }
        // Late-consumption sweep (completeness): notes whose consumption block
        // the cursor already passed — discovered late (imported by the
        // reconciler, or delivered late by sync). Their original synthetic
        // block is sealed and downstream getLogs consumers only read forward,
        // so project them into the FIRST block of this tick's window. The
        // `swept` cache keeps this O(new notes); `project_*`'s own
        // `is_note_processed` dedup keeps it idempotent (a note that was
        // projected on time is attempted once here, then cached).
        let late_ids: Vec<[u8; 32]> = {
            let swept = self.swept.lock().expect("swept cache poisoned");
            let late: Vec<&InputNoteRecord> = consumed
                .iter()
                .filter(|n| {
                    n.state()
                        .consumed_block_height()
                        .map(|h| h.as_u64() <= cursor)
                        .unwrap_or(false)
                })
                .filter(|n| !swept.contains(&n.details_commitment().as_bytes()))
                .collect();
            let ids = late
                .iter()
                .map(|n| n.details_commitment().as_bytes())
                .collect();
            if !late.is_empty() {
                tracing::info!(
                    late = late.len(),
                    first_block = cursor + 1,
                    "late-consumption sweep: projecting notes discovered after their block"
                );
                by_block.entry(cursor + 1).or_default().extend(late);
            }
            ids
        };
        // Direct projection of spent-before-import recoveries (same sealed-block
        // rules as the late sweep; see `bucket_direct_notes`).
        let direct_done = Self::bucket_direct_notes(&direct_notes, &mut by_block, cursor, tip);
        // ── Cantina #7 PoC: LET assignment gate ────────────────────────────
        // `depositCount` is INFERRED from consumed-note enumeration; the bridge
        // account's Local Exit Tree is the authority. The Cantina #9 monitor
        // already compares the two AFTER emission (alarm-only); this gate runs
        // the same comparison BEFORE any index is handed out and HALTS
        // projection on mismatch — a stall is recoverable, a wrongly-numbered
        // BridgeEvent is poison (unclaimable exit + every later index shifted).
        //
        // Invariant: every on-chain LET leaf corresponds to exactly one
        // bridge-consumed B2AGG note (only the bridge's consumption appends a
        // leaf; reclaims never touch bridge storage). Emitted notes advanced
        // `deposit_counter`; visible-but-pending ones (this tick's batch,
        // quarantined, deferred) will. So alignment reduces to:
        //     count(visible bridge-consumed B2AGG) == read_let_num_leaves()
        // computed from the SAME synced client-store snapshot as the consumed
        // feed above — no extra RPC, no assignment-vs-feed race.
        //
        // `direct_recovered` notes are fabricated outside the client store
        // (spent-before-import), so they are counted in explicitly.
        if let Some(bridge_account) = client
            .get_account(self.bridge_id)
            .await
            .map_err(|e| anyhow::anyhow!("LET gate: get_account({}): {e}", self.bridge_id))?
        {
            let on_chain =
                miden_base_agglayer::AggLayerBridge::read_let_num_leaves(&bridge_account);
            let visible = consumed
                .iter()
                .chain(direct_notes.iter())
                .filter(|n| {
                    is_b2agg_note(n.details())
                        && matches!(
                            classify_b2agg_consumer(n.consumer_account(), self.bridge_id),
                            B2AggConsumerClass::Emit
                        )
                })
                .count() as u64;
            match let_assignment_gate(visible, on_chain) {
                LetGateVerdict::Aligned => {}
                LetGateVerdict::InvisibleGap { gap } => {
                    ::metrics::counter!(
                        "bridge_let_assignment_gate_halted_total",
                        "kind" => "invisible_gap"
                    )
                    .increment(1);
                    tracing::error!(
                        on_chain,
                        visible,
                        gap,
                        "Cantina #7 LET assignment gate: on-chain LET has leaves no visible \
                         B2AGG note accounts for (erased/undelivered exits?) — HALTING \
                         projection so no misnumbered depositCount is emitted; the \
                         reconciler/recovery must close the gap (retries next tick)"
                    );
                    return Ok(cursor);
                }
                LetGateVerdict::LocalAhead { excess } => {
                    ::metrics::counter!(
                        "bridge_let_assignment_gate_halted_total",
                        "kind" => "local_ahead"
                    )
                    .increment(1);
                    tracing::error!(
                        on_chain,
                        visible,
                        excess,
                        "Cantina #7 LET assignment gate: more visible B2AGG consumptions than \
                         on-chain LET leaves (client-store corruption or foreign-note \
                         misclassification) — HALTING projection (retries next tick)"
                    );
                    return Ok(cursor);
                }
            }
        } else {
            // Bridge account not yet in the local client store (first boot before
            // import) — nothing can have been consumed by it either; proceed.
            tracing::debug!("LET gate: bridge account not yet tracked — gate skipped");
        }
        // ── Cantina #7 PoC (part 2): same-tx B2AGG ordering ────────────────
        // `consumed_tx_order` is per-TRANSACTION, so two B2AGG notes consumed
        // by the SAME bridge tx tie under the intra-block sort and would fall
        // back to the details-commitment — a hash, arbitrary relative to the
        // on-chain LET append order (the tx's input-note order). The bridge's
        // per-account transaction feed carries that order in each header's
        // `input_notes()` list; when a tie exists, fetch it and thread the
        // (per-block tx order, within-tx position) pair into the sort. No tie
        // (the overwhelmingly common case) → zero extra RPC.
        let is_emit_b2agg = |n: &InputNoteRecord| {
            is_b2agg_note(n.details())
                && matches!(
                    classify_b2agg_consumer(n.consumer_account(), self.bridge_id),
                    B2AggConsumerClass::Emit
                )
        };
        // Group by the note's REAL consumed (block, tx_order): late-swept notes
        // share a bucket with on-time ones, and a tie is only a same-tx tie if
        // the consuming block matches too.
        //
        // PoC LIMITATION: the within-tx map is keyed by nullifier, but a
        // reconciler-fabricated `ConsumedExternalNoteState` record carries no
        // metadata, so `InputNoteRecord::nullifier()` returns `None` for it and
        // it cannot be matched to the tx feed. Those records fall back to the
        // details-commitment tie-break (unchanged from today). A production
        // version would additionally map nullifier→details-commitment (derivable
        // from each tx-input note) so metadata-less records also order exactly.
        let tied_notes: Vec<&InputNoteRecord> = {
            let mut groups: HashMap<(Option<u64>, Option<u32>), Vec<&InputNoteRecord>> =
                HashMap::new();
            for n in by_block.values().flatten().filter(|n| is_emit_b2agg(n)) {
                let key = (
                    n.state().consumed_block_height().map(|h| h.as_u64()),
                    n.state().consumed_tx_order(),
                );
                groups.entry(key).or_default().push(n);
            }
            groups
                .into_values()
                .filter(|g| g.len() >= 2)
                .flatten()
                .collect()
        };
        let mut within_tx: Option<HashMap<Nullifier, (u32, u32)>> = None;
        if !tied_notes.is_empty() {
            if let Some(rpc) = self.node_rpc.clone() {
                let heights: Vec<u64> = tied_notes
                    .iter()
                    .filter_map(|n| n.state().consumed_block_height().map(|h| h.as_u64()))
                    .collect();
                let (min_h, max_h) = match (heights.iter().min(), heights.iter().max()) {
                    (Some(min), Some(max)) => (*min, *max),
                    _ => (0, tip),
                };
                let txs = rpc
                    .sync_transactions(
                        BlockNumber::from(min_h as u32),
                        BlockNumber::from(max_h as u32),
                        vec![self.bridge_id],
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "same-tx ordering: sync_transactions({min_h}..{max_h}): {e}"
                        )
                    })?;
                let full = bridge_consumed_nullifiers(&txs, self.bridge_id);
                // Every tied note MUST be orderable, or emitting risks the
                // exact swap this exists to prevent — halt like the gate does.
                let unresolvable = tied_notes
                    .iter()
                    .any(|n| n.nullifier().is_none_or(|nf| !full.contains_key(&nf)));
                if unresolvable {
                    ::metrics::counter!(
                        "bridge_let_assignment_gate_halted_total",
                        "kind" => "unordered_tie"
                    )
                    .increment(1);
                    tracing::error!(
                        tied = tied_notes.len(),
                        min_h,
                        max_h,
                        "Cantina #7 same-tx ordering: a tied B2AGG note is missing from the \
                         bridge transaction feed — cannot determine on-chain LET append \
                         order; HALTING projection so no swapped depositCount is emitted \
                         (retries next tick)"
                    );
                    return Ok(cursor);
                }
                within_tx = Some(
                    full.into_iter()
                        .map(|(nf, (_block, order, pos))| (nf, (order, pos)))
                        .collect(),
                );
            } else {
                // No node RPC (unit-test topology): keep the legacy
                // commitment tie-break; production always wires node_rpc.
                tracing::warn!(
                    tied = tied_notes.len(),
                    "Cantina #7 same-tx ordering: node RPC unavailable — falling back to \
                     details-commitment tie-break for same-tx B2AGG notes"
                );
            }
        }
        drop(tied_notes);
        let no_notes: Vec<&InputNoteRecord> = Vec::new();
        while cursor < tip {
            let next = cursor + 1;
            let bucket = by_block.get(&next).unwrap_or(&no_notes);
            self.project_block_notes(
                bucket,
                &output_metadata,
                next,
                Some(client),
                within_tx.as_ref(),
            )
            .await?;
            // Advance the cursor only after the block is fully projected, so a
            // crash mid-block re-projects (idempotently) rather than skipping.
            // Persist BEFORE updating the in-memory cache so the durable cursor
            // never runs ahead of fully-projected state.
            self.store.set_projector_cursor(next).await?;
            self.cursor.store(next, Ordering::Release);
            cursor = next;
        }
        // The tick completed — only NOW mark the late-swept notes as handled, so
        // a mid-tick failure retries them next tick instead of dropping them.
        if !late_ids.is_empty() {
            self.swept
                .lock()
                .expect("swept cache poisoned")
                .extend(late_ids);
        }
        // Same contract for the direct-recovery queue: prune only after the
        // whole tick succeeded (retries are idempotent via `is_note_processed`).
        self.complete_direct_notes(&direct_done);
        // Observability: the projector follows the MIDEN chain, so its progress is
        // measured against the Miden tip (NOT L1). `projector_cursor == miden_tip`
        // means fully caught up; `synthetic_tip` is the actual synthetic L2 block
        // number the chain is exposing. Logged once per tick that did work.
        let synthetic_tip = self.store.get_latest_block_number().await?;
        tracing::info!(
            miden_tip = tip,
            projector_cursor = cursor,
            synthetic_tip,
            "synthetic projector tick: caught up to Miden tip"
        );
        Ok(cursor)
    }
}

/// Cantina #7 PoC — verdict of the pre-emit LET assignment gate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LetGateVerdict {
    /// Every on-chain LET leaf is accounted for by a visible bridge-consumed
    /// B2AGG note — index assignment is provably aligned; safe to project.
    Aligned,
    /// The on-chain LET holds `gap` leaves for which NO visible note exists
    /// (erased notes, undelivered sync). Assigning the next local index would
    /// misnumber it (and every exit after it) — projection must halt until
    /// the reconciler/recovery makes the missing consumption visible.
    InvisibleGap { gap: u64 },
    /// More visible bridge-consumed B2AGG notes than on-chain leaves — local
    /// client-store corruption or consumer misattribution. Fail closed.
    LocalAhead { excess: u64 },
}

/// Cantina #7 PoC — the pure alignment predicate behind the assignment gate.
///
/// `visible` = bridge-consumed B2AGG notes in the synced client store (plus
/// direct recoveries); `on_chain` = `AggLayerBridge::read_let_num_leaves` from
/// the same snapshot. Equality means the local `deposit_counter` inference and
/// the bridge's authoritative Local Exit Tree agree leaf-for-leaf. Pure (no
/// I/O) so it is unit-testable directly.
pub(crate) fn let_assignment_gate(visible: u64, on_chain: u64) -> LetGateVerdict {
    use std::cmp::Ordering as CmpOrdering;
    match visible.cmp(&on_chain) {
        CmpOrdering::Equal => LetGateVerdict::Aligned,
        CmpOrdering::Less => LetGateVerdict::InvisibleGap {
            gap: on_chain - visible,
        },
        CmpOrdering::Greater => LetGateVerdict::LocalAhead {
            excess: visible - on_chain,
        },
    }
}

/// MA#3 reclaim gate for the spent-before-import recovery path: map every
/// nullifier consumed by a BRIDGE-executed transaction to `(spend_block,
/// per-block bridge-tx order, within-tx input position)`.
///
/// The node's `sync_transactions` feed is filtered per account and each
/// transaction header commits to the nullifiers of the notes that transaction
/// consumed, so membership here is exact on-chain attribution of the consumer —
/// the same condition [`crate::bridge_out::classify_b2agg_consumer`] gates on
/// (`consumer == bridge`). The account-id re-check is fail-closed defense in
/// depth against a node that ignores the server-side filter. Pure (no I/O) so
/// it is unit-testable directly.
///
/// The **within-tx input position** (Cantina #7) is the note's index in the
/// header's ordered `input_notes()` list. Note scripts execute in exactly that
/// order during transaction execution, so for a tx consuming several B2AGG
/// notes it is the order their LET leaves were appended on-chain — the
/// authoritative intra-tx `depositCount` order that the client-store
/// `consumed_tx_order` (per-tx, not per-note) cannot distinguish.
pub(crate) fn bridge_consumed_nullifiers(
    txs: &[TransactionRecord],
    bridge_id: AccountId,
) -> HashMap<Nullifier, (u64, u32, u32)> {
    let mut per_block_order: HashMap<u64, u32> = HashMap::new();
    let mut out = HashMap::new();
    for tx in txs {
        if tx.transaction_header.account_id() != bridge_id {
            continue;
        }
        let block = tx.block_num.as_u64();
        let order = *per_block_order
            .entry(block)
            .and_modify(|i| *i += 1)
            .or_insert(0u32);
        for (pos, input) in tx.transaction_header.input_notes().iter().enumerate() {
            out.insert(input.nullifier(), (block, order, pos as u32));
        }
    }
    out
}

#[async_trait::async_trait]
impl SyncListener for SyntheticProjector {
    fn on_sync(&self, _summary: &SyncSummary) {
        // no-op — projection happens in `on_post_sync`, where we hold the live
        // client needed to fetch consumed notes and run Cantina #13 recovery.
    }

    async fn on_post_sync(&self, client: &mut MidenClientLib) -> anyhow::Result<()> {
        self.tick(client).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts_config::{AccountIdBech32, AccountsConfig};
    use crate::claim_watcher::derive_manual_claim_tx_hash;
    use crate::log_synthesis::{LogFilter, SyntheticLog};
    use crate::store::memory::InMemoryStore;
    use miden_base_agglayer::{
        B2AggNote, ClaimNote, ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex,
        LeafData, MetadataHash, ProofData, SmtNode, UpdateGerNote,
    };
    use miden_client::store::InputNoteState;
    use miden_client::store::input_note_states::ConsumedExternalNoteState;
    use miden_protocol::Felt;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, FungibleAsset};
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{
        NoteAssets, NoteAttachment, NoteAttachments, NoteDetails, NoteMetadata, NoteRecipient,
        NoteStorage, NoteType, PartialNoteMetadata,
    };
    use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint};
    use std::sync::Arc as StdArc;

    /// Cantina #7 PoC — the assignment gate's pure predicate. Aligned only on
    /// exact equality; any invisible on-chain leaf halts (that is the finding's
    /// failure mode: the next local index would misnumber the exit and shift
    /// every one after it); local-ahead fails closed.
    #[test]
    fn cantina7_let_assignment_gate_verdicts() {
        // Exact agreement — including the genesis (0,0) case — projects.
        assert_eq!(let_assignment_gate(0, 0), LetGateVerdict::Aligned);
        assert_eq!(let_assignment_gate(42, 42), LetGateVerdict::Aligned);
        // On-chain leaves nobody visible accounts for → halt with the gap size.
        assert_eq!(
            let_assignment_gate(41, 42),
            LetGateVerdict::InvisibleGap { gap: 1 }
        );
        assert_eq!(
            let_assignment_gate(0, 7),
            LetGateVerdict::InvisibleGap { gap: 7 }
        );
        // More visible consumptions than leaves → fail closed.
        assert_eq!(
            let_assignment_gate(43, 42),
            LetGateVerdict::LocalAhead { excess: 1 }
        );
    }

    /// Reconciler private-note wedge (0.15.5 hotfix): the exact miden-client
    /// rejection that froze the retroactive-heal sweep must be classified as
    /// skippable, so the reconciler drops just the private note and advances —
    /// while unrelated errors still propagate and fail the tick (stay loud).
    #[test]
    fn private_note_import_error_is_recognized() {
        // The literal error observed in prod (0.15.5).
        assert!(is_private_note_import_error(
            "Incomplete imported note is private"
        ));
        // Wrapped / prefixed in an error chain, and case-insensitive.
        assert!(is_private_note_import_error(&anyhow::anyhow!(
            "import_notes(20): Incomplete imported note is private"
        )));
        assert!(is_private_note_import_error("NOTE IS PRIVATE"));
        // Unrelated failures must NOT be swallowed — they keep failing the tick.
        assert!(!is_private_note_import_error("database is locked"));
        assert!(!is_private_note_import_error(&anyhow::anyhow!(
            "sync_notes(1..200): connection reset by peer"
        )));
    }

    // Four mutually-distinct, valid protocol-0.15 account ids reused from the
    // restore/bridge_out test fixtures. `FAUCET` is a real fungible-faucet id so
    // `FungibleAsset::new` accepts it.
    const FAUCET: &str = "0xac0000000000dd110000ee000000fc";
    const BRIDGE: &str = "0xaa0000000000bb110000cc000000dd";
    const GER_MANAGER: &str = "0xfa0000000000bb010000cc000000de";
    const SERVICE: &str = "0xbf0000000000cc010000dc000000ee";

    fn aid(hex: &str) -> AccountId {
        AccountId::from_hex(hex).expect("hex must decode")
    }

    /// A minimal `AccountsConfig` for projector construction — only the
    /// `bridge`, `ger_manager` and `service` ids are read by the projector.
    fn test_accounts() -> AccountsConfig {
        AccountsConfig {
            service: AccountIdBech32(aid(SERVICE)),
            bridge: AccountIdBech32(aid(BRIDGE)),
            faucet_eth: None,
            faucet_agg: None,
            wallet_hardhat: AccountIdBech32(aid(SERVICE)),
            ger_manager: Some(AccountIdBech32(aid(GER_MANAGER))),
        }
    }

    /// Wrap a `NoteDetails` + attachments into a consumed `InputNoteRecord`
    /// attributed to `block` with `tx_order`. This is the projector analogue of
    /// the restore/bridge_out `build_*` helpers, with `nullifier_block_height`
    /// set so the projector can attribute the note to a Miden block.
    fn consumed_note(
        details: NoteDetails,
        attachments: NoteAttachments,
        consumer: Option<AccountId>,
        block: u32,
        tx_order: Option<u32>,
    ) -> InputNoteRecord {
        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(block),
            consumer_account: consumer,
            consumed_tx_order: tx_order,
        });
        InputNoteRecord::new(details, attachments, None, state)
    }

    /// Build a bridge-consumed B2AGG note carrying a fungible asset from
    /// `FAUCET`, consumed by the bridge at `block` with `tx_order`.
    fn b2agg_note(block: u32, tx_order: Option<u32>) -> InputNoteRecord {
        b2agg_note_with_amount(block, tx_order, 50)
    }

    /// Like [`b2agg_note`] but with a caller-chosen asset amount, so tests
    /// needing several DISTINCT B2AGG notes (distinct details commitments) can
    /// vary the amount.
    fn b2agg_note_with_amount(block: u32, tx_order: Option<u32>, amount: u64) -> InputNoteRecord {
        // B2AGG storage: 6 felts (network + 5 address limbs); zeros parse fine.
        let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let asset: Asset = FungibleAsset::new(aid(FAUCET), amount).unwrap().into();
        let assets = NoteAssets::new(vec![asset]).unwrap();
        let details = NoteDetails::new(assets, recipient);
        consumed_note(
            details,
            NoteAttachments::default(),
            Some(aid(BRIDGE)),
            block,
            tx_order,
        )
    }

    /// Build a consumed CLAIM note with a valid `ClaimNoteStorage`, consumed at
    /// `block` with `tx_order`.
    fn claim_note(block: u32, tx_order: Option<u32>) -> InputNoteRecord {
        let mut gi_bytes = [0u8; 32];
        gi_bytes[23] = 1;
        gi_bytes[31] = 0x42;
        let mut origin_addr = [0u8; 20];
        origin_addr[19] = 0xAB;
        let mut dest_addr = [0u8; 20];
        dest_addr[..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut amount_bytes = [0u8; 32];
        amount_bytes[28..32].copy_from_slice(&1_000_000u32.to_be_bytes());

        let claim_storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new(gi_bytes),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network: 0x12345678,
                origin_token_address: EthAddress::new(origin_addr),
                destination_network: 0xAABBCCDD,
                destination_address: EthAddress::new(dest_addr),
                amount: EthAmount::new(amount_bytes),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::ZERO,
        };
        let storage = NoteStorage::try_from(claim_storage).expect("claim storage round-trips");
        let recipient = NoteRecipient::new(Word::default(), ClaimNote::script(), storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);
        consumed_note(
            details,
            NoteAttachments::default(),
            Some(aid(BRIDGE)),
            block,
            tx_order,
        )
    }

    /// Build a sanctioned consumed GER note (UpdateGerNote) targeting the bridge
    /// and minted by `GER_MANAGER`, encoding `ger_byte` in every 32-bit limb.
    /// Returns the record plus the details-commitment → metadata entry the
    /// projector needs for the MA#28 `ConsumedExternal` provenance fallback.
    fn ger_note(
        block: u32,
        tx_order: Option<u32>,
        ger_byte: u8,
    ) -> (InputNoteRecord, ([u8; 32], NoteMetadata)) {
        // 8 felts, each a u32 limb. restore reads each limb as a big-endian u32.
        let limb = u32::from_be_bytes([ger_byte; 4]);
        let storage = NoteStorage::new(vec![Felt::from(limb); 8]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), UpdateGerNote::script(), storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        // Provenance: sender = ger_manager, attachment = NetworkAccountTarget(bridge).
        let attachment = NoteAttachment::from(
            NetworkAccountTarget::new(aid(BRIDGE), NoteExecutionHint::Always).expect("nat"),
        );
        let attachments = NoteAttachments::from(attachment);
        let partial = PartialNoteMetadata::new(aid(GER_MANAGER), NoteType::Public);
        let metadata = NoteMetadata::new(partial, &attachments);

        let record = consumed_note(details, attachments, Some(aid(BRIDGE)), block, tx_order);
        let key = record.details_commitment().as_bytes();
        (record, (key, metadata))
    }

    /// Build a projector for the deterministic-core tests. The cursor/tip loop
    /// (`tick`) needs a live `&mut MidenClientLib`, so the unit tests drive the
    /// `project_notes` core directly with an in-memory consumed-note feed and a
    /// `None` recovery client.
    async fn test_projector(
        store: &StdArc<dyn Store>,
        block_state: &StdArc<BlockState>,
    ) -> SyntheticProjector {
        SyntheticProjector::new(
            store.clone(),
            block_state.clone(),
            &test_accounts(),
            7,
            None,
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// Regression lock for the prod restart-resync incident: the reconciler's
    /// sweep cursor was a memory-only AtomicU64 hardcoded to 0 at boot, so
    /// EVERY container restart (image update, crash, plain restart) re-walked
    /// the sweep from genesis — ~3h of resync + node load per restart on prod
    /// history. The cursor is now persisted (`Store::{get,set}_reconcile_cursor`,
    /// migration 010) and loaded in `SyntheticProjector::new` exactly like the
    /// projection cursor: a re-constructed projector must start its next
    /// sweep window at persisted+1, NOT at 1.
    #[tokio::test]
    async fn restart_resumes_reconcile_sweep_from_persisted_cursor() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());

        // First boot: nothing persisted → the sweep starts from genesis
        // (block 1). This is the designed first-boot heal and must not change.
        let projector = test_projector(&store, &block_state).await;
        assert_eq!(
            projector.next_reconcile_window(10_000),
            Some((1, RECONCILE_CHUNK)),
            "first boot (no persisted cursor) must sweep from genesis"
        );

        // Advance the sweep cursor via the persistence API — the same store
        // write `reconcile_notes` performs after a window completes.
        store.set_reconcile_cursor(4_200).await.unwrap();
        drop(projector);

        // "Container restart": re-construct the projector over the SAME store.
        let projector = test_projector(&store, &block_state).await;
        assert_eq!(
            projector.next_reconcile_window(10_000),
            Some((4_201, 4_200 + RECONCILE_CHUNK)),
            "restart must resume the sweep at persisted+1, not re-walk from genesis"
        );
        // Caught-up case: no window when the persisted cursor is at the tip.
        assert_eq!(projector.next_reconcile_window(4_200), None);
    }

    /// [`ReconcileFetcher`] fake for the catch-up driver tests: records every
    /// window it is asked for, optionally fails one window (by its `from`
    /// block), and returns NO candidates — so the driver's window batching,
    /// ordering, budget and cursor advancement run without a client handle
    /// (the per-window import is only entered when candidates exist).
    struct FakeFetcher {
        calls: std::sync::Mutex<Vec<(u64, u64)>>,
        fail_from: Option<u64>,
    }

    impl FakeFetcher {
        fn new(fail_from: Option<u64>) -> StdArc<Self> {
            StdArc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_from,
            })
        }

        fn calls(&self) -> Vec<(u64, u64)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ReconcileFetcher for FakeFetcher {
        async fn sync_note_ids(&self, from: u64, to: u64) -> anyhow::Result<Vec<NoteId>> {
            self.calls.lock().unwrap().push((from, to));
            if self.fail_from == Some(from) {
                anyhow::bail!("injected window-fetch failure ({from}..{to})");
            }
            Ok(Vec::new())
        }
    }

    /// Catch-up throughput contract: when the sweep is behind the tip, ONE
    /// `reconcile_notes` call processes MULTIPLE windows (batches of
    /// `concurrency`, repeated until caught up) under a sufficient budget —
    /// instead of the historical one-window-per-tick cadence. And the budget
    /// actually bounds the work: with a zero budget exactly one batch runs
    /// (guaranteed progress), leaving the rest for the next tick.
    #[tokio::test]
    async fn reconcile_catchup_processes_multiple_windows_per_tick_under_budget() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());

        // Zero budget: exactly one batch (= `concurrency` windows) per tick.
        let projector = test_projector(&store, &block_state)
            .await
            .with_reconcile_tuning(200, 4, Duration::ZERO);
        let fetcher = FakeFetcher::new(None);
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        projector
            .reconcile_notes_with(None, None, &f, 2_000)
            .await
            .unwrap();
        // Fetch-task completion order is unspecified — compare as a set.
        let mut calls = fetcher.calls();
        calls.sort_unstable();
        assert_eq!(
            calls,
            vec![(1, 200), (201, 400), (401, 600), (601, 800)],
            "zero budget: one batch of `concurrency` windows, then stop"
        );
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 800);

        // Ample budget: the same call catches all the way up in one tick.
        let projector = test_projector(&store, &block_state)
            .await
            .with_reconcile_tuning(200, 4, Duration::from_secs(60));
        let fetcher = FakeFetcher::new(None);
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        projector
            .reconcile_notes_with(None, None, &f, 2_000)
            .await
            .unwrap();
        let mut calls = fetcher.calls();
        calls.sort_unstable();
        assert_eq!(calls.len(), 6, "windows 801..2000 in batches of <=4");
        assert_eq!(calls.first(), Some(&(801, 1_000)));
        assert_eq!(calls.last(), Some(&(1_801, 2_000)));
        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            2_000,
            "multiple windows must advance the persisted cursor to the tip in ONE tick"
        );
    }

    /// ORDERING SAFETY: the cursor may only advance to block X when ALL windows
    /// <= X completed successfully. Inject a failure into the middle window of
    /// a concurrent batch — the windows below it keep their advancement (the
    /// low-water mark), the windows above it (already fetched) are discarded,
    /// and the next tick retries FROM the failed window.
    #[tokio::test]
    async fn reconcile_cursor_never_advances_past_failed_window() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state)
            .await
            .with_reconcile_tuning(200, 8, Duration::from_secs(60));

        // Window 3 of the batch (blocks 401..600) fails; 1,2 and 4..8 succeed.
        let fetcher = FakeFetcher::new(Some(401));
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        let err = projector
            .reconcile_notes_with(None, None, &f, 2_000)
            .await
            .expect_err("a failed window must fail the tick (stays loud/transient)");
        assert!(
            format!("{err:#}").contains("401..600"),
            "error must name the failed window: {err:#}"
        );
        assert_eq!(fetcher.calls().len(), 8, "the whole batch was fetched");
        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            400,
            "cursor must stop at the low-water mark (end of the last good window BELOW the failure)"
        );
        assert_eq!(
            projector.reconcile_cursor.load(Ordering::Acquire),
            400,
            "in-memory cursor must match the persisted low-water mark"
        );

        // Next tick (failure cleared): the sweep resumes AT the failed window
        // and catches up — nothing was skipped.
        let fetcher = FakeFetcher::new(None);
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        projector
            .reconcile_notes_with(None, None, &f, 2_000)
            .await
            .unwrap();
        let mut retry_calls = fetcher.calls();
        retry_calls.sort_unstable();
        assert_eq!(
            retry_calls.first(),
            Some(&(401, 600)),
            "retry must re-fetch the failed window first"
        );
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 2_000);
    }

    /// Caught-up steady state must be unchanged from the historical behavior:
    /// exactly ONE (small) window fetch per tick, no extra batches, and a tick
    /// at the tip fetches nothing.
    #[tokio::test]
    async fn reconcile_caught_up_single_window_unchanged() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        store.set_reconcile_cursor(9_950).await.unwrap();
        let projector = test_projector(&store, &block_state)
            .await
            .with_reconcile_tuning(200, 8, Duration::from_secs(60));

        let fetcher = FakeFetcher::new(None);
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        projector
            .reconcile_notes_with(None, None, &f, 10_000)
            .await
            .unwrap();
        assert_eq!(
            fetcher.calls(),
            vec![(9_951, 10_000)],
            "near the tip: exactly one partial window, same as the old per-tick sweep"
        );
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 10_000);

        // At the tip: nothing to do, no fetches at all.
        let fetcher = FakeFetcher::new(None);
        let f: StdArc<dyn ReconcileFetcher> = fetcher.clone();
        projector
            .reconcile_notes_with(None, None, &f, 10_000)
            .await
            .unwrap();
        assert!(fetcher.calls().is_empty(), "caught up: zero RPC work");
    }

    async fn register_faucet(store: &StdArc<dyn Store>) {
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id: aid(FAUCET),
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "ETH".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();
    }

    /// Collect all synthetic logs in `[from, to]` via the public `get_logs`
    /// API, preserving (block, insertion) order.
    async fn logs_in_range(store: &StdArc<dyn Store>, from: u64, to: u64) -> Vec<SyntheticLog> {
        let filter = LogFilter {
            from_block: Some(format!("0x{from:x}")),
            to_block: Some(format!("0x{to:x}")),
            ..Default::default()
        };
        store.get_logs(&filter, to).await.unwrap()
    }

    /// (i) A Miden block with a bridge-consumed B2AGG note + a CLAIM note + a
    /// GER note projects THREE synthetic logs into the SAME synthetic block
    /// (Miden-1:1: synthetic block N == Miden block N), in the deterministic
    /// `(consumed_tx_order, note_id)` order, with sequential log indices.
    #[tokio::test]
    async fn projects_three_derivations_into_one_miden_block() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(5, Some(0));
        let n_claim = claim_note(5, Some(1));
        let (n_ger, ger_meta) = ger_note(5, Some(2), 0x11);

        // Intentionally shuffled input order — the projector's deterministic
        // sort, not arrival order, fixes the output.
        let notes = vec![n_ger.clone(), n_claim.clone(), n_b2agg.clone()];
        let output_metadata = HashMap::from([ger_meta]);
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let written = projector
            .project_notes(&notes, &output_metadata, 5, None, None)
            .await
            .unwrap();
        assert_eq!(written, 3, "all three derivations must emit one log each");

        // Miden-1:1: all three logs land in synthetic block 5 (== the Miden block).
        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 3, "three logs in the one synthetic block");
        assert_eq!(
            logs.iter().map(|l| l.block_number).collect::<Vec<_>>(),
            vec![5, 5, 5],
            "Miden-1:1: every log for Miden block 5 lands in synthetic block 5",
        );
        // The synthetic tip == the Miden block.
        assert_eq!(store.get_latest_block_number().await.unwrap(), 5);
        // Log indices are sequential in projection order.
        assert_eq!(
            logs.iter().map(|l| l.log_index).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );

        // Deterministic order matches the consumed_tx_order we set: B2AGG(0)@1,
        // CLAIM(1)@2, GER(2)@3. Identify each by its distinctive tx-hash shape.
        let b2agg_id = hex::encode(n_b2agg.details_commitment().as_bytes());
        let claim_id = hex::encode(n_claim.details_commitment().as_bytes());
        assert_eq!(
            logs[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&b2agg_id),
            "first log must be the B2AGG bridge-out (tx_order 0)"
        );
        assert_eq!(
            logs[1].transaction_hash,
            derive_manual_claim_tx_hash(&claim_id),
            "second log must be the CLAIM (tx_order 1)"
        );
        assert!(
            logs[2].transaction_hash.starts_with("0x"),
            "third log must be the GER (tx_order 2)"
        );
    }

    /// (ii) Re-projecting the same Miden block is idempotent — no duplicate
    /// logs and no tip advance (the `project_*` dedup keys short-circuit).
    #[tokio::test]
    async fn reprojecting_same_block_is_idempotent() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(7, Some(0));
        let n_claim = claim_note(7, Some(1));
        let (n_ger, ger_meta) = ger_note(7, Some(2), 0x22);
        let notes = vec![n_b2agg, n_claim, n_ger];
        let output_metadata = HashMap::from([ger_meta]);
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let first = projector
            .project_notes(&notes, &output_metadata, 7, None, None)
            .await
            .unwrap();
        assert_eq!(first, 3);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 7);

        let second = projector
            .project_notes(&notes, &output_metadata, 7, None, None)
            .await
            .unwrap();
        assert_eq!(second, 0, "second projection must emit no new logs");
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            7,
            "tip stays at the Miden block on a no-op re-projection",
        );

        let logs = logs_in_range(&store, 0, 7).await;
        assert_eq!(logs.len(), 3, "no duplicate logs after re-projection");
    }

    /// (iii) Notes consumed at different Miden blocks project into the synthetic
    /// blocks matching their Miden heights (Miden-1:1) — synthetic block N is
    /// exactly Miden block N, including the gaps between them.
    #[tokio::test]
    async fn distinct_nullifier_heights_project_into_their_miden_blocks() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(3, Some(0)); // consumed at Miden block 3
        let n_claim = claim_note(8, Some(0)); // consumed at Miden block 8
        let notes = vec![n_b2agg.clone(), n_claim.clone()];
        let output_metadata = HashMap::new();
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Project Miden block 3: only the B2AGG note belongs here → synthetic 3.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 3, None, None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 3);
        // Project Miden block 8: only the CLAIM note belongs here → synthetic 8.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 8, None, None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 8);

        let logs = logs_in_range(&store, 0, 8).await;
        assert_eq!(logs.len(), 2);
        // Miden-1:1: synthetic blocks 3 and 8 (== the Miden heights), not 1 and 2.
        assert_eq!(logs[0].block_number, 3);
        assert_eq!(logs[1].block_number, 8);
        assert_eq!(
            logs[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
                n_b2agg.details_commitment().as_bytes()
            ))
        );
        assert_eq!(
            logs[1].transaction_hash,
            derive_manual_claim_tx_hash(&hex::encode(n_claim.details_commitment().as_bytes()))
        );
    }

    /// Cantina #19 regression lock — the OLD `BridgeOutScanner` advanced
    /// `latest_block_number` once PER consumed B2AGG note inside its loop
    /// (`block = get_latest_block_number()+1; process; set_latest_block_number`),
    /// so a single Miden tx carrying many B2AGG notes pushed later notes hundreds
    /// or thousands of synthetic blocks into the future, shadowing legitimate
    /// events. The redesign made the `SyntheticProjector` the SOLE tip-advancer
    /// using Miden-1:1 (synthetic block N == Miden block N).
    ///
    /// This test fabricates:
    ///   * FOUR distinct B2AGG notes all consumed at the SAME Miden block (100),
    ///     and asserts every one lands at synthetic block 100 (NOT 1,2,3,4) with
    ///     the tip advancing to exactly 100 (NOT 4); then
    ///   * three more B2AGG notes at a FAR-LATER Miden block (250), asserting they
    ///     land at 250 (NOT 5,6,7 — the per-note counter would ignore the height).
    ///
    /// The pre-fix per-note increment would have produced strictly-increasing
    /// distinct block numbers detached from the Miden height, failing every
    /// assertion below.
    #[tokio::test]
    async fn finding_19_projector_uses_miden_block_not_per_note_increment() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;
        let output_metadata = HashMap::new();

        // FOUR distinct B2AGG notes (distinct amounts → distinct commitments) all
        // consumed in ONE Miden tx at Miden block 100.
        let same_block = vec![
            b2agg_note_with_amount(100, Some(0), 11),
            b2agg_note_with_amount(100, Some(1), 22),
            b2agg_note_with_amount(100, Some(2), 33),
            b2agg_note_with_amount(100, Some(3), 44),
        ];
        let written = projector
            .project_notes(&same_block, &output_metadata, 100, None, None)
            .await
            .unwrap();
        assert_eq!(written, 4, "all four B2AGG notes must emit a log");

        // Miden-1:1: the tip is the Miden block itself, NOT tip+4. The pre-fix
        // per-note increment would have set it to 4.
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            100,
            "tip must be the Miden block (100), not advanced once per note"
        );

        let logs = logs_in_range(&store, 0, 100).await;
        assert_eq!(logs.len(), 4);
        let blocks: Vec<u64> = logs.iter().map(|l| l.block_number).collect();
        assert_eq!(
            blocks,
            vec![100, 100, 100, 100],
            "every note consumed at Miden block 100 lands at synthetic block 100; \
             the pre-fix loop would have scattered them across 1,2,3,4"
        );
        // Explicit: all notes share ONE block, i.e. no per-note advance happened.
        assert_eq!(
            blocks
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1,
            "N notes at the same Miden block must occupy exactly ONE synthetic block"
        );

        // A FAR-LATER Miden block: notes land at 250 (Miden-1:1), not 5,6,7. A
        // per-note counter continuing from the prior tip would ignore the height.
        let later_block = vec![
            b2agg_note_with_amount(250, Some(0), 55),
            b2agg_note_with_amount(250, Some(1), 66),
            b2agg_note_with_amount(250, Some(2), 77),
        ];
        let written_later = projector
            .project_notes(&later_block, &output_metadata, 250, None, None)
            .await
            .unwrap();
        assert_eq!(written_later, 3);
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            250,
            "tip follows the Miden height (250), not a per-note counter (would be 7)"
        );
        let later_logs = logs_in_range(&store, 101, 250).await;
        assert_eq!(later_logs.len(), 3);
        assert!(
            later_logs.iter().all(|l| l.block_number == 250),
            "notes at Miden block 250 all land at synthetic block 250, not 5/6/7"
        );
    }

    /// (iv) Two independent runs over the same consumed-note set produce
    /// byte-identical synthetic logs: same (Miden-1:1) block numbers, block
    /// hashes, log indices and ordering.
    #[tokio::test]
    async fn two_runs_are_byte_identical() {
        async fn run() -> Vec<SyntheticLog> {
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
            register_faucet(&store).await;
            let (n_ger, ger_meta) = ger_note(9, Some(2), 0x33);
            // Intentionally shuffled input order to prove the projector's
            // deterministic sort — not arrival order — fixes the output.
            let notes = vec![n_ger, claim_note(9, Some(1)), b2agg_note(9, Some(0))];
            let output_metadata = HashMap::from([ger_meta]);
            let block_state = StdArc::new(BlockState::new());
            let projector = test_projector(&store, &block_state).await;
            let written = projector
                .project_notes(&notes, &output_metadata, 9, None, None)
                .await
                .unwrap();
            assert_eq!(written, 3);
            logs_in_range(&store, 0, 9).await
        }

        let run_a = run().await;
        let run_b = run().await;
        assert_eq!(run_a.len(), 3);
        assert_eq!(run_a.len(), run_b.len());
        for (a, b) in run_a.iter().zip(run_b.iter()) {
            assert_eq!(a.block_number, b.block_number, "block numbers must match");
            assert_eq!(a.block_hash, b.block_hash, "block hashes must be identical");
            assert_eq!(a.log_index, b.log_index, "log indices must match");
            assert_eq!(
                a.transaction_hash, b.transaction_hash,
                "log ordering / tx hashes must match"
            );
            assert_eq!(a.topics, b.topics, "topics must match");
            assert_eq!(a.data, b.data, "data must match");
        }
    }

    /// Certificate-settlement regression: when `publish_claim` has linked the
    /// real claim eth-tx to the CLAIM note (`record_tx_note_link`), the projected
    /// ClaimEvent MUST ride that real tx hash — not a derived one. aggkit's
    /// L2BridgeSyncer fetches the claim tx by hash and decodes its `claimAsset`
    /// calldata to resolve the GER boundary; a derived hash points at a synthetic
    /// tx with EMPTY calldata, so aggkit fails "input too short: 0 bytes" and
    /// never settles the certificate.
    #[tokio::test]
    async fn claim_event_rides_linked_real_tx_hash() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_claim = claim_note(5, Some(0));
        let note_commitment = hex::encode(n_claim.details_commitment().as_bytes());
        let real_tx = "0x1111111111111111111111111111111111111111111111111111111111111111";
        // publish_claim records this link when it submits the CLAIM note.
        store
            .record_tx_note_link(real_tx, &note_commitment)
            .await
            .unwrap();

        let notes = vec![n_claim.clone()];
        let output_metadata = HashMap::new();
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 5, None, None)
                .await
                .unwrap(),
            1
        );

        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].transaction_hash, real_tx,
            "ClaimEvent must ride the linked real claim tx hash (carries claimAsset calldata)"
        );
        assert_ne!(
            logs[0].transaction_hash,
            derive_manual_claim_tx_hash(&note_commitment),
            "must NOT fall back to the derived hash when a link exists"
        );
    }

    /// Direct-projection recovery (spent-before-import): consumed-external
    /// B2AGG records fabricated by the reconciler bypass the client store —
    /// verify they bucket per the sealed-block rules (late → first window
    /// block, in-window → own Miden block, beyond-tip → deferred), emit a
    /// BridgeEvent through the SAME `project_b2agg_note` derivation, and are
    /// pruned from the queue only once projected.
    #[tokio::test]
    async fn direct_recovery_projects_bridge_consumed_note() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Spend block already passed by the cursor (3), inside this tick's
        // window (8), and beyond the tip (12). Distinct amounts → distinct
        // details commitments.
        let n_late = b2agg_note_with_amount(3, Some(0), 51);
        let n_exact = b2agg_note_with_amount(8, Some(0), 52);
        let n_future = b2agg_note_with_amount(12, Some(0), 53);
        projector.direct_recovered.lock().unwrap().extend([
            n_late.clone(),
            n_exact.clone(),
            n_future.clone(),
        ]);

        // Snapshot + bucket the way `tick` does, with cursor=5, tip=10.
        let direct: Vec<InputNoteRecord> = projector.direct_recovered.lock().unwrap().clone();
        let mut by_block: HashMap<u64, Vec<&InputNoteRecord>> = HashMap::new();
        let done = SyntheticProjector::bucket_direct_notes(&direct, &mut by_block, 5, 10);

        assert_eq!(
            done.len(),
            2,
            "late + in-window notes bucket; beyond-tip defers"
        );
        assert_eq!(
            by_block.get(&6).map(Vec::len),
            Some(1),
            "already-passed spend block projects into the first window block"
        );
        assert_eq!(
            by_block.get(&8).map(Vec::len),
            Some(1),
            "in-window spend block projects Miden-1:1 at its own height"
        );
        assert!(
            !by_block.contains_key(&12),
            "spend block beyond the sync tip must not bucket this tick"
        );

        // Project the buckets exactly like the tick loop.
        let empty = HashMap::new();
        let logs6 = projector
            .project_block_notes(&by_block[&6], &empty, 6, None, None)
            .await
            .unwrap();
        let logs8 = projector
            .project_block_notes(&by_block[&8], &empty, 8, None, None)
            .await
            .unwrap();
        assert_eq!(
            (logs6, logs8),
            (1, 1),
            "each recovered note emits one BridgeEvent"
        );

        let logs = logs_in_range(&store, 0, 10).await;
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].block_number, 6);
        assert_eq!(logs[1].block_number, 8);
        assert_eq!(
            logs[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
                n_late.details_commitment().as_bytes()
            )),
            "direct projection derives the SAME tx hash as every other path (dedup-stable)"
        );

        // Completion prunes only the projected notes; the deferred one stays
        // queued for a future tick.
        projector.complete_direct_notes(&done);
        let remaining = projector.direct_recovered.lock().unwrap().clone();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].details_commitment(),
            n_future.details_commitment(),
            "only the beyond-tip note remains queued"
        );
    }

    /// Cantina MA#28 — the projector's `ConsumedExternal` provenance fallback,
    /// fail-closed side. A GER-shaped consumed note whose details-commitment
    /// has NO matching entry in our own output-note metadata map (i.e. a note
    /// the proxy did not mint) must be skipped as `MissingMetadata`: zero
    /// synthetic logs, GER not injected. Re-projecting the SAME notes with the
    /// matching output record then emits — proving the skip was the metadata
    /// gate, not an unrelated short-circuit, and that the fail-closed skip
    /// stays retryable.
    #[tokio::test]
    async fn ma28_projector_ger_without_output_record_is_fail_closed_skip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let (n_ger, ger_meta) = ger_note(4, Some(0), 0x77);
        let notes = vec![n_ger];

        // No own output record → fail-closed skip.
        let written = projector
            .project_notes(&notes, &HashMap::new(), 4, None, None)
            .await
            .unwrap();
        assert_eq!(
            written, 0,
            "GER-shaped note without an own output record must project NO log (MA#28)"
        );
        assert!(
            !store.is_ger_injected(&[0x77u8; 32]).await.unwrap(),
            "unverifiable GER must not be marked injected"
        );
        assert!(
            logs_in_range(&store, 0, 4).await.is_empty(),
            "fail-closed skip must leave the synthetic log stream empty"
        );

        // Same note, WITH the output-record metadata → verified and emitted.
        let written = projector
            .project_notes(&notes, &HashMap::from([ger_meta]), 4, None, None)
            .await
            .unwrap();
        assert_eq!(
            written, 1,
            "the same note must emit once the output record verifies its provenance"
        );
        assert!(store.is_ger_injected(&[0x77u8; 32]).await.unwrap());
    }

    /// The MA#3 gate for the spent-before-import recovery: only nullifiers
    /// consumed by BRIDGE-executed transactions are attributed. A reclaim (or
    /// any non-bridge consumption) stays out of the map, so the caller skips
    /// it fail-closed instead of emitting a BridgeEvent for value that never
    /// left Miden.
    #[test]
    fn bridge_consumed_nullifiers_gates_non_bridge_txs() {
        use miden_protocol::note::Nullifier;
        use miden_protocol::transaction::{InputNoteCommitment, InputNotes, TransactionHeader};

        fn nf(byte: u64) -> Nullifier {
            Nullifier::from_raw(Word::new([Felt::new(byte).unwrap(); 4]))
        }
        fn tx(account: AccountId, block: u32, nullifier: Nullifier) -> TransactionRecord {
            TransactionRecord {
                block_num: BlockNumber::from(block),
                transaction_header: TransactionHeader::new(
                    account,
                    Word::empty(),
                    Word::empty(),
                    InputNotes::new(vec![InputNoteCommitment::from(nullifier)]).unwrap(),
                    vec![],
                    FungibleAsset::new(aid(FAUCET), 0).unwrap(),
                ),
                output_notes: vec![],
                erased_output_notes: vec![],
            }
        }

        let (a, b, c) = (nf(1), nf(2), nf(3));
        let txs = vec![
            tx(aid(BRIDGE), 9, a),  // bridge consumption → attributed, order 0
            tx(aid(SERVICE), 9, b), // sender reclaim → NOT attributed
            tx(aid(BRIDGE), 9, c),  // second bridge tx in the block → order 1
        ];
        let map = bridge_consumed_nullifiers(&txs, aid(BRIDGE));
        assert_eq!(map.get(&a), Some(&(9, 0, 0)));
        assert_eq!(
            map.get(&c),
            Some(&(9, 1, 0)),
            "per-block bridge-tx order increments"
        );
        assert!(
            !map.contains_key(&b),
            "non-bridge consumption must be gated out (MA#3 fail-closed)"
        );
    }

    /// Cantina #7 — a single bridge transaction consuming SEVERAL B2AGG notes
    /// must expose each note's within-tx input position: that is the order the
    /// note scripts executed and therefore the order their LET leaves were
    /// appended on-chain. `consumed_tx_order` alone cannot distinguish them.
    #[test]
    fn cantina7_bridge_consumed_nullifiers_within_tx_positions() {
        use miden_protocol::note::Nullifier;
        use miden_protocol::transaction::{InputNoteCommitment, InputNotes, TransactionHeader};

        fn nf(byte: u64) -> Nullifier {
            Nullifier::from_raw(Word::new([Felt::new(byte).unwrap(); 4]))
        }
        let (first, second, third) = (nf(7), nf(8), nf(9));
        let batched = TransactionRecord {
            block_num: BlockNumber::from(42u32),
            transaction_header: TransactionHeader::new(
                aid(BRIDGE),
                Word::empty(),
                Word::empty(),
                InputNotes::new(vec![
                    InputNoteCommitment::from(first),
                    InputNoteCommitment::from(second),
                    InputNoteCommitment::from(third),
                ])
                .unwrap(),
                vec![],
                FungibleAsset::new(aid(FAUCET), 0).unwrap(),
            ),
            output_notes: vec![],
            erased_output_notes: vec![],
        };
        let map = bridge_consumed_nullifiers(&[batched], aid(BRIDGE));
        assert_eq!(map.get(&first), Some(&(42, 0, 0)));
        assert_eq!(map.get(&second), Some(&(42, 0, 1)));
        assert_eq!(
            map.get(&third),
            Some(&(42, 0, 2)),
            "within-tx position must follow the header's ordered input_notes()"
        );
    }
}
