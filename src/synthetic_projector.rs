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
//! `eth_blockNumber` tracks the Miden tip. Consumed notes are ordered by
//! `(consumed_block_height, consumed_tx_order, details_commitment)` before
//! deriving — the late-consumption sweep can fold notes from earlier (sealed)
//! Miden blocks into one projection block, so the primary key is each note's
//! on-chain `consumed_block_height` (not the projection block), preserving
//! global on-chain consumption order; `consumed_tx_order` then the 32-byte
//! details-commitment are the deterministic tie-breakers. Re-running the
//! projector over the same chain therefore yields byte-identical synthetic blocks
//! (numbers, hashes, log order, log indices). Because the projector is the sole
//! assigner of the synthetic tip, there is no `get_latest()+1` reservation race —
//! Finding #5 is eliminated by construction.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::bridge_out::{
    B2AggConsumerClass, classify_b2agg_consumer, derive_bridge_out_tx_hash, is_b2agg_note,
    parse_b2agg_storage,
};
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

/// How many ticks the projector holds (retries) for an UNAUTHENTICATED bridge-consumed note
/// the node's `get_notes_by_id` has not returned yet (tx feed ahead of the note DB) before it
/// gives up and loud-skips rather than freezing the tip. At the sub-second sync cadence this
/// is ~a handful of seconds — long enough for a transient lag to clear, short enough that a
/// genuine node fault never halts the bridge.
const FETCH_MISS_RETRY_BOUND: u32 = 20;

/// Completeness-auditor cadence: audit once every N projector ticks (~1s each → ~30s cycles).
const AUDIT_EVERY_N_TICKS: u64 = 30;

/// Completeness-auditor settle margin: only audit blocks at least this far behind the
/// projector cursor, so the client store's (lagging) consumption view has definitely caught
/// up on the audited range. Costs only detection latency, prevents false positives.
const AUDIT_SETTLE_MARGIN: u64 = 10;

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
    /// In-flight B2AGG note BODIES keyed by nullifier — the projector's authoritative
    /// body-resolution source, bounded to imported-but-not-yet-projected notes.
    ///
    /// `tick` sources B2AGG consumptions authoritatively from the bridge's transaction
    /// feed (nullifiers) and must resolve each nullifier to its note body. It CANNOT
    /// read the body from the live store by nullifier: [`InputNoteRecord::nullifier`]
    /// returns `Some` only while `metadata()` is `Some`, and a note's metadata becomes
    /// `None` the instant `sync_state` marks it `ConsumedExternal` — so the moment a
    /// B2AGG note is consumed it drops out of any store-nullifier map, and (since the
    /// nullifier mixes in the metadata word, [`Nullifier::new`]) it cannot be recomputed
    /// from `NoteDetails` alone. The body must therefore be captured by nullifier WHILE
    /// the note still has metadata, into this cache. Two feeders populate it, both
    /// filtered to B2AGG:
    ///
    ///   * [`Self::cache_committed_b2agg_bodies`] — every store note whose nullifier is
    ///     still computable (Committed etc.), refreshed at import time AND once per tick,
    ///     so a note imported this run OR still-Committed from a prior run is captured
    ///     before it can transition to `ConsumedExternal`.
    ///   * [`Self::recover_dropped_note_bodies`] — spent-before-import notes miden-client
    ///     0.15 silently drops on import (import returns Ok, note absent from store); it
    ///     re-fetches their bodies from the node so they resolve too.
    ///
    /// `tick` EVICTS a nullifier once its block is projected, keeping the map bounded to
    /// the in-flight set (imported, not-yet-projected) rather than growing without limit.
    recovered_bodies: std::sync::Mutex<HashMap<Nullifier, (NoteDetails, NoteAttachments)>>,
    /// Per-nullifier retry counter for the note-DB-lag backstop: an UNAUTHENTICATED
    /// bridge-consumed note the tx feed reported but `get_notes_by_id` has not returned yet
    /// (`sync_transactions` is eventual-consistent AHEAD of the node's note DB). The tick
    /// holds (Errs, retries) for up to [`FETCH_MISS_RETRY_BOUND`] ticks so a transient lag
    /// resolves correctly; past the bound it loud-skips and advances rather than freezing the
    /// tip forever (a frozen tip is a liveness failure). Entries are cleared once the note
    /// resolves or is skipped, so the map is bounded to the in-flight not-yet-returned set.
    fetch_miss_attempts: std::sync::Mutex<HashMap<Nullifier, u32>>,
    /// INSTR (observability-only): the `project_to` of the previous tick's `sync_transactions`
    /// sourcing window, so `tick` can detect a gap/overlap between consecutive source windows
    /// (a note whose block falls in an un-sourced gap would silently miss). Pure diagnostics —
    /// it never influences windowing or any control flow.
    last_source_window_to: AtomicU64,
    /// Completeness auditor (detection only, no healing): details-commitments already
    /// VERIFIED (BridgeEvent found at the exact consumption block) or already ALARMED
    /// (missing — alarm once, counter cumulative). Skipping these keeps each ~30s audit
    /// cycle O(new consumptions) and de-dupes alarms. In-memory on purpose: a restart
    /// re-audits from scratch, which is cheap and re-surfaces any standing violation.
    audit_resolved: std::sync::Mutex<HashSet<[u8; 32]>>,
    /// Tick counter driving the every-[`AUDIT_EVERY_N_TICKS`] audit cadence.
    audit_tick_counter: AtomicU64,
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
            recovered_bodies: std::sync::Mutex::new(HashMap::new()),
            fetch_miss_attempts: std::sync::Mutex::new(HashMap::new()),
            last_source_window_to: AtomicU64::new(0),
            audit_resolved: std::sync::Mutex::new(HashSet::new()),
            audit_tick_counter: AtomicU64::new(0),
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
                // PREPARED handoffs persist the exact Miden NoteId before the
                // external submit. `sync_notes` is the authoritative,
                // inclusive creation feed, so seeing that id confirms the
                // handoff even when a crash happened before the local client
                // applied the accepted transaction. Do this on the raw ids,
                // before body import/fetch (which may lag) and before the
                // durable cursor advances past the transaction's expiry.
                if !candidates.is_empty() {
                    let note_ids: Vec<String> = candidates.iter().map(NoteId::to_hex).collect();
                    self.store.confirm_prepared_note_handoffs(&note_ids).await?;
                }
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
                // Spent-before-import: `import_notes` returns Ok even for notes it
                // silently DROPPED because they were already consumed at import time
                // (miden-client 0.15 bug — see the `recovered_bodies` field docs).
                // Re-query which attempted ids landed; cache the LANDED (Committed) B2AGG
                // bodies by nullifier now, while their metadata is present, so they resolve
                // in `tick` even after `sync_state` marks them ConsumedExternal; the DROPPED
                // ones' bodies are recovered from the node by `recover_dropped_note_bodies`.
                if !attempted.is_empty() {
                    let landed_recs = client
                        .get_input_notes(NoteFilter::List(attempted.clone()))
                        .await
                        .map_err(|e| anyhow::anyhow!("get_input_notes(List) post-import: {e}"))?;
                    self.cache_committed_b2agg_bodies(&landed_recs, "import");
                    let landed: HashSet<NoteId> =
                        landed_recs.iter().filter_map(|rec| rec.id()).collect();
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
                        self.recover_dropped_note_bodies(rpc, &missing).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Capture the bodies of the given store notes into the B2AGG body cache, keyed by
    /// nullifier, for every note whose nullifier is still computable (metadata present —
    /// Committed and other pre-consumption states). This is how a B2AGG body survives the
    /// note later becoming `ConsumedExternal` (which nulls out
    /// [`InputNoteRecord::nullifier`]); see the [`Self::recovered_bodies`] field docs.
    ///
    /// Called at import time (freshly landed notes) AND once per tick over the whole store
    /// (notes still Committed from a prior run), so no B2AGG note can be consumed before its
    /// body is cached. Idempotent — re-inserting the same nullifier is a no-op. Only B2AGG
    /// bodies are kept (CLAIM/GER ride the store's consumed feed; other notes emit no event).
    fn cache_committed_b2agg_bodies(&self, records: &[InputNoteRecord], source: &str) {
        let mut cache = self
            .recovered_bodies
            .lock()
            .expect("recovered-bodies cache poisoned");
        for rec in records {
            // `nullifier()` is `Some` only while metadata is present; a note already
            // `ConsumedExternal` returns `None` and cannot be (re)cached here — by then it
            // must already be in the cache from an earlier Committed observation.
            let Some(nullifier) = rec.nullifier() else {
                continue;
            };
            if !is_b2agg_note(rec.details()) {
                continue;
            }
            // INSTR (observability-only): trace every B2AGG body captured, keyed by nullifier.
            let committed_block = rec
                .inclusion_proof()
                .map(|p| p.location().block_num().as_u64());
            let (tag, note_type) = rec
                .metadata()
                .map(|m| (m.tag().as_u32(), format!("{:?}", m.note_type())))
                .unzip();
            tracing::info!(
                "INSTR discover: nullifier={} note_id={} committed_block={:?} tag={:?} \
                 note_type={:?} source={}",
                nullifier.to_hex(),
                rec.id()
                    .map(|i| i.to_hex())
                    .unwrap_or_else(|| "none".into()),
                committed_block,
                tag,
                note_type,
                source
            );
            if cache
                .insert(
                    nullifier,
                    (rec.details().clone(), rec.attachments().clone()),
                )
                .is_none()
            {
                metrics::counter!("synthetic_reconciler_recovered_body_total").increment(1);
            }
        }
    }

    /// Cache the note BODIES of spent-before-import notes, keyed by nullifier.
    ///
    /// A B2AGG note ALREADY CONSUMED when the reconciler tries to import it is
    /// silently dropped by miden-client 0.15 (import returns Ok, note absent from
    /// store), so its body never lands in the client store. `tick` sources
    /// consumptions AUTHORITATIVELY from the bridge's transaction feed and resolves
    /// each consumed nullifier to its note body from the store — this fills the bodies
    /// the store is missing, so a dropped-then-consumed note's BridgeEvent is still
    /// emitted at its exact block.
    ///
    /// Only PUBLIC B2AGG bodies are cached: private notes can't be reconstructed,
    /// non-B2AGG public notes derive no synthetic event, and CLAIM/GER are our own
    /// notes that always reach the store normally. The MA#3 reclaim gate lives in
    /// `tick` now — it only ever sources consumptions from bridge transactions, so a
    /// reclaimed/unknown consumption is never projected (no gate needed here).
    async fn recover_dropped_note_bodies(
        &self,
        rpc: &dyn NodeRpcClient,
        missing: &[NoteId],
    ) -> anyhow::Result<()> {
        let fetched = rpc
            .get_notes_by_id(missing)
            .await
            .map_err(|e| anyhow::anyhow!("get_notes_by_id({}): {e}", missing.len()))?;
        let mut cache = self
            .recovered_bodies
            .lock()
            .expect("recovered-bodies cache poisoned");
        for f in fetched {
            let id = f.id();
            let FetchedNote::Public(note, _inclusion) = f else {
                tracing::debug!(
                    note_id = %id.to_hex(),
                    "dropped-body recovery: skipping private note (not reconstructable)"
                );
                continue;
            };
            let nullifier = note.nullifier();
            let attachments = note.attachments().clone();
            let details: NoteDetails = note.into();
            // Only PUBLIC B2AGG bodies matter: non-B2AGG public notes emit no synthetic
            // event, and CLAIM/GER are our own notes that always reach the store the normal
            // way. The MA#3 emit gate is NOT applied here — `tick` sources consumptions from
            // the bridge's own transaction feed, so only bridge-consumed nullifiers project.
            if !is_b2agg_note(&details) {
                continue;
            }
            // INSTR (observability-only): trace recovery-sourced B2AGG bodies, keyed by nullifier.
            tracing::info!(
                "INSTR discover: nullifier={} note_id={} committed_block=None tag=None \
                 note_type=None source=recovery",
                nullifier.to_hex(),
                id.to_hex()
            );
            if cache.insert(nullifier, (details, attachments)).is_none() {
                metrics::counter!("synthetic_reconciler_recovered_body_total").increment(1);
                tracing::debug!(
                    note_id = %id.to_hex(),
                    "dropped-body recovery: cached spent-before-import B2AGG body by nullifier"
                );
            }
        }
        Ok(())
    }

    /// Resolve the note bodies for a window's bridge-consumed nullifiers into ConsumedExternal
    /// records to project. `bridge_consumed_nullifiers` yields EVERY bridge consumption — real
    /// B2AGG exits AND the non-B2AGG notes the bridge routinely consumes (CLAIM, UpdateGerNote,
    /// genesis/setup notes) — so most inputs here are legitimately NOT B2AGG exits.
    ///
    /// INVARIANT — the projector MUST NEVER freeze the tip (a frozen tip is a liveness failure).
    /// Only a silent drop of a RESOLVABLE B2AGG exit is forbidden; skipping an unresolvable
    /// non-exit, or bounded-retry-then-loud-skip of an unresolvable one, is correct. Each cache
    /// miss is one of:
    ///   * AUTHENTICATED (no note id in the tx) + uncached — the cache is B2AGG-only, so this is
    ///     normally a non-B2AGG consumption (CLAIM/GER/genesis) the store consumed feed already
    ///     covers. SAFE SKIP + non-fatal metric (NEVER wedge — the pre-unified projector also
    ///     skipped these and passed the suite; fail-closing here froze the tip on block-13 setup).
    ///   * UNAUTHENTICATED, node returns a public B2AGG body — resolve + emit at exact block. This
    ///     is the acfee0cb completeness fix (note created+consumed under load before import).
    ///   * UNAUTHENTICATED, node RETURNS it as non-public / non-b2agg — provably not an exit
    ///     (legit CLAIM/GER) — SAFE SKIP.
    ///   * UNAUTHENTICATED, node did NOT return it — `sync_transactions` is ahead of the node's
    ///     note DB (eventual-consistent under load). BOUNDED RETRY: hold the tick (Err) up to
    ///     [`FETCH_MISS_RETRY_BOUND`] ticks so a transient lag resolves the real body; past the
    ///     bound, loud-skip + advance rather than freeze forever.
    ///
    /// Every resolved nullifier is recorded in `evict_by_block` so `tick` can bound the cache
    /// after the block seals. No `await` is held under any lock.
    async fn resolve_b2agg_consumptions(
        &self,
        fetcher: &dyn PublicNoteFetcher,
        consumed_refs: HashMap<Nullifier, ConsumedRef>,
        evict_by_block: &mut HashMap<u64, Vec<Nullifier>>,
    ) -> anyhow::Result<Vec<InputNoteRecord>> {
        let build = |details: NoteDetails, attachments: NoteAttachments, cref: &ConsumedRef| {
            let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                nullifier_block_height: BlockNumber::from(cref.block as u32),
                consumer_account: Some(self.bridge_id),
                consumed_tx_order: Some(cref.order),
            });
            InputNoteRecord::new(details, attachments, None, state)
        };

        // Phase 1 (fast path, no I/O): resolve from the cache. A cache miss WITHOUT a note id is
        // an authenticated consumption the B2AGG-only cache can't hold — normally a legit
        // non-B2AGG note (CLAIM/GER/genesis) covered by the store consumed feed: SAFE SKIP, never
        // wedge. Only UNAUTHENTICATED misses (note id present) go to the authoritative fetch.
        let mut recs: Vec<InputNoteRecord> = Vec::new();
        let mut misses: Vec<(Nullifier, ConsumedRef)> = Vec::new();
        {
            let cache = self
                .recovered_bodies
                .lock()
                .expect("recovered-bodies cache poisoned");
            for (nullifier, cref) in consumed_refs {
                if let Some((details, attachments)) = cache.get(&nullifier) {
                    // The cache holds ONLY B2AGG bodies (both feeders filter), so a hit needs
                    // no re-check.
                    recs.push(build(details.clone(), attachments.clone(), &cref));
                    evict_by_block
                        .entry(cref.block)
                        .or_default()
                        .push(nullifier);
                    tracing::info!(
                        "INSTR resolve: nullifier={} block={} outcome=cache_hit note_id={:?}",
                        nullifier.to_hex(),
                        cref.block,
                        cref.note_id.map(|i| i.to_hex())
                    );
                } else if cref.note_id.is_some() {
                    misses.push((nullifier, cref));
                } else {
                    // Authenticated + uncached → non-B2AGG consumption (store feed covers it).
                    metrics::counter!("synthetic_projector_b2agg_authenticated_skip_total")
                        .increment(1);
                    tracing::info!(
                        "INSTR resolve: nullifier={} block={} outcome=authenticated_skip note_id=None",
                        nullifier.to_hex(),
                        cref.block
                    );
                    tracing::debug!(
                        nullifier = %nullifier.to_hex(),
                        block = cref.block,
                        "projector: skipping authenticated uncached bridge consumption \
                         (non-B2AGG — CLAIM/GER/genesis, covered by the store consumed feed)"
                    );
                }
            }
        }
        if misses.is_empty() {
            return Ok(recs);
        }

        // Phase 2 (authoritative backstop): fetch the uncached UNAUTHENTICATED bodies by id.
        let fetch_ids: Vec<NoteId> = misses
            .iter()
            .filter_map(|(_, cref)| cref.note_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let FetchedBodies {
            bodies,
            returned_ids,
        } = fetcher.fetch_public_bodies(&fetch_ids).await?;
        let body_by_id: HashMap<NoteId, &FetchedBody> = bodies
            .iter()
            .filter(|b| is_b2agg_note(&b.details))
            .map(|b| (b.id, b))
            .collect();
        // Track the note-DB-lag retry counters under one lock (no await in this loop). A note
        // still under its retry bound holds the tick; past the bound it loud-skips and advances.
        let mut attempts = self
            .fetch_miss_attempts
            .lock()
            .expect("fetch-miss-attempts poisoned");
        let mut retry_ctx: Option<(Nullifier, NoteId, u64, u32)> = None;
        for (nullifier, cref) in &misses {
            let Some(note_id) = cref.note_id else {
                unreachable!("authenticated (note_id-less) misses are skipped in phase 1");
            };
            if let Some(body) = body_by_id.get(&note_id) {
                recs.push(build(body.details.clone(), body.attachments.clone(), cref));
                evict_by_block
                    .entry(cref.block)
                    .or_default()
                    .push(*nullifier);
                attempts.remove(nullifier);
                metrics::counter!("synthetic_projector_b2agg_authoritative_fetch_total")
                    .increment(1);
                tracing::info!(
                    "INSTR resolve: nullifier={} block={} outcome=authoritative_fetch note_id={}",
                    nullifier.to_hex(),
                    cref.block,
                    note_id.to_hex()
                );
                tracing::info!(
                    note_id = %note_id.to_hex(),
                    block = cref.block,
                    "projector: resolved an uncached (unauthenticated) B2AGG consumption by \
                     authoritative fetch"
                );
            } else if returned_ids.contains(&note_id) {
                // Node RETURNED it but it is non-public / non-b2agg — legit CLAIM/GER. Safe skip
                // (must NOT fail-closed, or a legit consumption wedges the tip).
                attempts.remove(nullifier);
                tracing::info!(
                    "INSTR resolve: nullifier={} block={} outcome=skip_returned_non_b2agg note_id={}",
                    nullifier.to_hex(),
                    cref.block,
                    note_id.to_hex()
                );
                tracing::debug!(
                    note_id = %note_id.to_hex(),
                    block = cref.block,
                    "authoritative fetch: node returned a non-b2agg note — safe skip (not an exit)"
                );
            } else {
                // Node did NOT return the id: tx feed ahead of the note DB. Bounded retry.
                tracing::info!(
                    "INSTR resolve: nullifier={} block={} outcome=fail_missing_fetch note_id={}",
                    nullifier.to_hex(),
                    cref.block,
                    note_id.to_hex()
                );
                let n = attempts.entry(*nullifier).or_insert(0);
                *n += 1;
                if *n > FETCH_MISS_RETRY_BOUND {
                    // Give up rather than freeze the tip forever: loud-skip and advance. A real
                    // b2agg exit lost here would surface as a missing BridgeEvent AND this alarm;
                    // a permanent not-return is a node fault worth alarming, not a bridge halt.
                    metrics::counter!("synthetic_projector_b2agg_fetch_missing_total").increment(1);
                    tracing::error!(
                        nullifier = %nullifier.to_hex(),
                        note_id = %note_id.to_hex(),
                        block = cref.block,
                        attempts = *n,
                        "projector: bridge-consumed note STILL not returned by get_notes_by_id \
                         after {FETCH_MISS_RETRY_BOUND} ticks — loud-skipping to keep the tip \
                         live (investigate: possible node note-DB fault or a genuine drop)."
                    );
                    attempts.remove(nullifier);
                } else if retry_ctx.is_none() {
                    // Within the bound: remember it to hold (Err) the tick after the loop.
                    retry_ctx = Some((*nullifier, note_id, cref.block, *n));
                }
            }
        }
        drop(attempts);
        if let Some((nullifier, note_id, block, n)) = retry_ctx {
            // Hold (retry) the whole tick: nothing seals, cursor unchanged, next tick re-fetches
            // once the note DB catches up. Bounded by FETCH_MISS_RETRY_BOUND so it can't freeze.
            tracing::warn!(
                nullifier = %nullifier.to_hex(),
                note_id = %note_id.to_hex(),
                block,
                attempt = n,
                bound = FETCH_MISS_RETRY_BOUND,
                "projector: bridge-consumed note not yet in the node's note DB (tx feed ahead) — \
                 holding the tick to retry"
            );
            return Err(anyhow::anyhow!(
                "projector: bridge-consumed note not yet in the node's note DB (nullifier {}, \
                 note_id {}, block {}, attempt {}/{}) — tx feed ahead of note DB, retry",
                nullifier.to_hex(),
                note_id.to_hex(),
                block,
                n,
                FETCH_MISS_RETRY_BOUND
            ));
        }
        Ok(recs)
    }

    /// COMPLETENESS AUDITOR — in-proxy early detection of missed BridgeEvents (the
    /// productionized `scripts/verify-event-completeness.sh`), detection ONLY: getLogs
    /// immutability forbids emitting into a sealed block, so a miss is alarmed loudly
    /// (metric + error log), never healed late.
    ///
    /// Ground truth needs no new RPC: the miden-client STORE. The reconciler imports every
    /// B2AGG note body and `sync_state` eventually marks it `Consumed*` with a
    /// `consumed_block_height` — laggingly, which is fine for detection: only blocks at
    /// least [`AUDIT_SETTLE_MARGIN`] behind the projector cursor are audited, and the full
    /// consumed set is re-scanned each cycle (late-learned consumptions cannot escape),
    /// de-duped via [`Self::audit_resolved`] so each cycle is O(new consumptions).
    ///
    /// For every consumed B2AGG note that SHOULD have emitted (mirrors the projector's own
    /// emit gates, so legitimately-skipped notes never false-alarm):
    ///   * reclaimed (consumer == Some(non-bridge)) → no event expected (MA#3), skip;
    ///   * unparsable storage → quarantined, never emits, skip;
    ///   * self-targeted (destination == local network) → poison-leaf, never emits (#13), skip;
    ///   * consumer unknown (`None`, the common case for externally-observed consumptions) →
    ///     INCLUDED — the log check decides (the unified projector emits from bridge-tx
    ///     attribution, so a genuinely bridge-consumed note must have its event);
    ///
    /// …check the synthetic store for a BridgeEvent at EXACTLY its consumption block, via the
    /// same join the projector writes: `derive_bridge_out_tx_hash(hex(details_commitment))`.
    /// Missing (or present at the wrong block) → `synthetic_projector_completeness_missing_total`
    /// (must stay 0; the soak gates on it) + a loud error, once per note.
    ///
    /// Returns the cycle's tallies so unit tests can assert alarm/no-alarm/dedupe directly.
    async fn audit_completeness(
        &self,
        consumed: &[InputNoteRecord],
        projector_cursor: u64,
    ) -> anyhow::Result<AuditOutcome> {
        let audit_to = projector_cursor.saturating_sub(AUDIT_SETTLE_MARGIN);
        // Liveness beacon: how far the auditor has audited (0 = not yet past the margin).
        ::metrics::gauge!("synthetic_projector_completeness_audit_lag").set(audit_to as f64);
        let mut outcome = AuditOutcome::default();
        if audit_to == 0 {
            return Ok(outcome);
        }
        for note in consumed {
            if !is_b2agg_note(note.details()) {
                continue;
            }
            let Some(block) = note.state().consumed_block_height().map(|h| h.as_u64()) else {
                continue;
            };
            if block > audit_to {
                // Inside the settle margin — not audited yet (next cycles will catch it).
                outcome.settling += 1;
                continue;
            }
            // Mirror the projector's emit gates (see doc comment): a note the projector
            // legitimately refuses to emit must not false-alarm.
            if matches!(
                classify_b2agg_consumer(note.consumer_account(), self.bridge_id),
                B2AggConsumerClass::Reclaimed
            ) {
                continue;
            }
            let Ok((destination_network, _)) = parse_b2agg_storage(note.details().storage()) else {
                continue; // quarantined (MA#18) — never emits
            };
            if destination_network == self.local_network_id {
                continue; // self-targeted poison leaf (#13) — never emits
            }
            let key: [u8; 32] = note.details_commitment().as_bytes();
            {
                let resolved = self
                    .audit_resolved
                    .lock()
                    .expect("audit-resolved set poisoned");
                if resolved.contains(&key) {
                    continue;
                }
            } // lock dropped before the await below
            let note_id_str = hex::encode(key);
            let tx_hash = derive_bridge_out_tx_hash(&note_id_str);
            let logs = self.store.get_logs_for_tx(&tx_hash).await?;
            outcome.audited += 1;
            if logs.iter().any(|l| l.block_number == block) {
                outcome.verified += 1;
            } else {
                // MISSED (or wrong-block, equally a violation). Alarm ONCE per note (the
                // resolved-set insert below de-dupes); the counter stays cumulative.
                outcome.missing += 1;
                ::metrics::counter!("synthetic_projector_completeness_missing_total").increment(1);
                tracing::error!(
                    details_commitment = %note_id_str,
                    nullifier = %note
                        .nullifier()
                        .map(|n| n.to_hex())
                        .unwrap_or_else(|| "none(consumed)".into()),
                    consumed_block = block,
                    audit_to,
                    found_at_blocks = ?logs.iter().map(|l| l.block_number).collect::<Vec<_>>(),
                    "completeness auditor: synthetic BridgeEvent MISSING at the consumption \
                     block — completeness violation (detection only; getLogs immutability \
                     forbids late healing)"
                );
            }
            self.audit_resolved
                .lock()
                .expect("audit-resolved set poisoned")
                .insert(key);
        }
        if outcome.missing > 0 || outcome.audited > 0 {
            tracing::info!(
                audit_to,
                audited = outcome.audited,
                verified = outcome.verified,
                missing = outcome.missing,
                settling = outcome.settling,
                "completeness auditor: cycle done"
            );
        }
        Ok(outcome)
    }

    /// The #30 visibility-barrier projection ceiling. With a barrier active (`has_barrier`
    /// — a reconciler is wired), never project past the reconciler's last completed sweep
    /// (`reconcile_cursor`), so a synthetic block is sealed only after its notes are
    /// visible; `reconcile_cursor > tip` (reconciler ahead) safely clamps back to `tip`.
    /// Without a barrier, project to the raw `tip` (legacy). Pure so the barrier invariant
    /// (ceiling never exceeds `reconcile_cursor` when active) is unit-testable.
    fn barrier_project_to(has_barrier: bool, tip: u64, reconcile_cursor: u64) -> u64 {
        if has_barrier {
            tip.min(reconcile_cursor)
        } else {
            tip
        }
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
    /// Determinism: consumed notes are ordered by
    /// `(consumed_block_height, consumed_tx_order, details_commitment)` before
    /// deriving. The primary key is each note's on-chain `consumed_block_height`
    /// (not the projection block) so the late-consumption sweep — which can fold
    /// notes from earlier sealed blocks into this projection block — preserves
    /// global on-chain consumption order; `consumed_tx_order` then the 32-byte
    /// details-commitment are the deterministic tie-breakers. Re-running over the
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
    ) -> anyhow::Result<usize> {
        let block_notes: Vec<&InputNoteRecord> = consumed
            .iter()
            .filter(|n| n.state().consumed_block_height().map(|h| h.as_u64()) == Some(miden_block))
            .collect();
        self.project_block_notes(&block_notes, output_metadata, miden_block, client)
            .await
    }

    /// Project the already-filtered notes consumed at `miden_block` into the
    /// single synthetic block `miden_block` (Miden-1:1), advancing the tip once
    /// after the block (write-before-advance), even when there are zero notes.
    async fn project_block_notes(
        &self,
        block_notes: &[&InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        mut client: Option<&mut MidenClientLib>,
    ) -> anyhow::Result<usize> {
        let mut notes: Vec<&InputNoteRecord> = block_notes.to_vec();

        // Determinism + on-chain order: order events by
        // (consumed_block_height, consumed_tx_order, note-id).
        //
        // Audit H4 — pre-fix this sorted by (consumed_tx_order, note-id) alone.
        // That was correct for a normal block (every note shares the same
        // consumed_block_height), but the late-consumption sweep mixes notes
        // from EARLIER (sealed) blocks into the current projection block. With
        // tx_order as the primary key, a late note consumed at block 3 with
        // tx_order 5 would sort AFTER an on-time note at block 6 with tx_order
        // 0 — even though on-chain the block-3 note was consumed first and so
        // must occupy the LOWER LET leaf / deposit_count. Making
        // consumed_block_height the primary key preserves global on-chain
        // consumption order across the mixed bucket, so the deposit_count the
        // autoclaim reads as `leaf_index` matches the on-chain Local Exit Tree.
        //
        // `consumed_tx_order` is the per-account position of the consuming
        // transaction within its block; the 32-byte details-commitment is the
        // stable tie-breaker. Compare the commitment bytes directly — identical
        // ordering to the old hex-string compare, but no per-comparison
        // allocation (matters when many notes share a block).
        let miden_block_height = miden_block;
        notes.sort_by(|a, b| {
            // Late-swept notes carry their ORIGINAL (earlier) consumed_block_height;
            // fall back to the projection block for notes without a recorded height
            // (should not happen for Consumed state, but keeps the sort total).
            let ha = a
                .state()
                .consumed_block_height()
                .map(|h| h.as_u64())
                .unwrap_or(miden_block_height);
            let hb = b
                .state()
                .consumed_block_height()
                .map(|h| h.as_u64())
                .unwrap_or(miden_block_height);
            ha.cmp(&hb)
                .then_with(|| {
                    a.state()
                        .consumed_tx_order()
                        .cmp(&b.state().consumed_tx_order())
                })
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
                // INSTR (observability-only): a B2AGG BridgeEvent was emitted. The reconstructed
                // ConsumedExternal record usually has no `nullifier()` (metadata dropped), so
                // the details-commitment is the join key back to the discover/resolve logs.
                tracing::info!(
                    "INSTR emit: nullifier={} details_commitment={} block={}",
                    note.nullifier()
                        .map(|n| n.to_hex())
                        .unwrap_or_else(|| "none".into()),
                    hex::encode(note.details_commitment().as_bytes()),
                    miden_block
                );
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

        self.project_notes(&consumed, &output_metadata, miden_block, Some(client))
            .await
    }

    /// Process every Miden block from `cursor + 1` up to the #30 visibility-barrier
    /// projection ceiling `project_to = min(tip, reconcile_cursor)` in order, projecting
    /// each one and advancing the cursor. Returns the new cursor (== `project_to`), which
    /// equals the Miden tip only when the barrier is not holding (reconciler caught up); a
    /// return value `< tip` means the barrier is holding projection at the reconcile
    /// frontier until the reconciler catches up. With no reconciler wired (`node_rpc =
    /// None`) `project_to == tip` and this is the legacy project-to-tip loop.
    ///
    /// This is the normal projector loop; catch-up after a restart is the same
    /// code path (the cursor simply starts further behind the ceiling).
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
        // #30 VISIBILITY BARRIER: never seal a synthetic block the reconciler
        // has not fully swept. `reconcile_notes` (above) advances
        // `reconcile_cursor` to its last completed sweep window; every external
        // bridge-out note at a block <= reconcile_cursor is therefore already in
        // the client store. Capping the projection loop at `min(tip,
        // reconcile_cursor)` makes exact-block emission a GUARANTEE for all event
        // types: a note is always visible BEFORE its block is projected, so
        // nothing is ever "late" (proxy-created CLAIM/GER notes are instant-
        // visible and were never late; B2AGG lateness was purely import lag,
        // eliminated here by construction — the late sweep below becomes an
        // alarm). In steady state the reconciler reaches `tip` every tick
        // (sub-second ~1000-block windows), so the barrier is a no-op; it only
        // bites when the reconciler falls behind, and then holding is the
        // correct failure mode — a late synthetic tip is benign (aggkit reads
        // block ranges and just sees the chain pause), an event at the wrong
        // block is poison. No reconciler wired (node_rpc = None, pure unit
        // tests / a non-reconciling deployment) => no barrier, legacy behavior.
        // Barrier = note-BODY-import gate: `reconcile_cursor` is the frontier whose note
        // BODIES the reconciler has imported (`sync_notes`). A B2AGG note consumed at N was
        // created at C <= N, so `reconcile_cursor >= N` guarantees its body is in the store.
        // We do NOT gate on B2AGG consumptions here — those are sourced AUTHORITATIVELY per
        // block from `sync_transactions` below, so there is no consumption lag and no B2AGG is
        // ever late; CLAIM/GER ride the store's consumed feed (proxy-internal, on time).
        let reconcile_cursor = self.reconcile_cursor.load(Ordering::Acquire);
        let project_to = if self.node_rpc.is_some() {
            let held = tip.saturating_sub(reconcile_cursor);
            ::metrics::gauge!("projector_visibility_barrier_held_blocks").set(held as f64);
            // Keep the fail-close alarm present and readable as 0 (this design routes by note
            // kind and skips — never wedges — a consumption whose body isn't imported, so the
            // counter is only ever a health readout; a genuine B2AGG miss surfaces in e2e).
            ::metrics::counter!("projector_unresolved_consumed_body_total").absolute(0);
            if held > 0 {
                tracing::debug!(
                    tip,
                    reconcile_cursor,
                    held,
                    "visibility barrier: holding projection at the reconcile frontier \
                     (reconciler behind tip)"
                );
            } else if reconcile_cursor > tip {
                // Reconciler swept PAST the node tip (persisted cursor ahead after a
                // restart, or a node reorg shortened the chain). `held` saturates to 0 so
                // the barrier projects to `tip` — which is SAFE (everything <= tip is
                // already reconciled, nothing strands). WARN (not debug): `reconcile_cursor`
                // is documented as the last COMPLETED sweep window, so it being ahead of the
                // node tip is an unexpected state (likely a reorg or a stale persisted
                // cursor) that operators should see, even though projection stays correct.
                tracing::warn!(
                    tip,
                    reconcile_cursor,
                    ahead = reconcile_cursor - tip,
                    "visibility barrier: reconcile_cursor is AHEAD of the node tip \
                     (reorg or stale persisted cursor?) — projecting to tip; safe but unexpected"
                );
            }
            Self::barrier_project_to(true, tip, reconcile_cursor)
        } else {
            Self::barrier_project_to(false, tip, 0)
        };
        if cursor >= project_to {
            return Ok(cursor);
        }
        // Output-note metadata (MA#28 GER provenance): our own minted notes carry the
        // sender metadata that a bridge-consumed ConsumedExternal record drops.
        let output_metadata: HashMap<[u8; 32], NoteMetadata> = client
            .get_output_notes(NoteFilter::All)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
            .into_iter()
            .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
            .collect();
        // Per-block consumption sourcing (docs/design/UNIFIED-PROJECTOR.md), routed by note
        // kind because the three types surface their consumptions differently:
        //
        //   * CLAIM / UpdateGerNote — created AND consumed by this proxy's own operations
        //     (the bridge consumes a GER injection roughly every block). They land in the
        //     local store's consumed feed on time and were never the notes the late-sweep
        //     chased, so they are sourced from the store's `Consumed` feed — the SAME source
        //     the projector always used for them.
        //
        //   * B2AGG bridge-out — imported EXTERNALLY by the reconciler; the store's discovery
        //     of its CONSUMPTION lags the chain (the bug the late-sweep fought). Sourced
        //     AUTHORITATIVELY from the bridge's own transaction feed, which gives the
        //     COMPLETE, FINALIZED B2AGG consumption set of [cursor+1, project_to] — so a
        //     B2AGG consumption can never surface "late", and the late-sweep is deleted.
        //
        // A bridge transaction can consume any of the three; routing by kind (NOT forcing
        // every bridge consumption through the B2AGG body path) is what keeps a GER/CLAIM
        // consumption — whose body the authoritative feed reports before the store's B2AGG
        // import frontier would have it — from wedging the tip.
        let consumed = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed input notes: {e}"))?;
        // CLAIM / GER from the store's consumed feed, at their finalized consumption block.
        // B2AGG is skipped here — sourced authoritatively below (keeping the two sources
        // disjoint keeps the reasoning clean; `is_note_processed` would dedup either way).
        let mut by_block: HashMap<u64, Vec<&InputNoteRecord>> = HashMap::new();
        for note in &consumed {
            if is_b2agg_note(note.details()) {
                continue;
            }
            if let Some(h) = note.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push(note);
            }
        }
        // AUTHORITATIVE B2AGG: for each bridge-consumed nullifier in the window, resolve its
        // note BODY and rebuild a ConsumedExternal record at the authoritative (block,
        // tx_order). Resolution is cache-first (bodies captured while the note was still
        // Committed — a B2AGG note's `InputNoteRecord::nullifier()` goes `None` the instant
        // `sync_state` marks it ConsumedExternal, so no live-store map can find it after
        // consumption) with an authoritative node fetch backing up any cache miss (a note
        // created+consumed under load before import is never cached but IS consumed
        // unauthenticated, so the tx carries its id). See `resolve_b2agg_consumptions`. A
        // nullifier that is neither cached, fetchable, nor B2AGG is a CLAIM/GER (the store
        // feed above covers it) or a not-yet-imported note — skipped there; an authenticated
        // uncached one fails the tick loudly. Projected nullifiers are evicted below.
        let mut evict_by_block: HashMap<u64, Vec<Nullifier>> = HashMap::new();
        let auth_b2agg: Vec<InputNoteRecord> = if let Some(rpc) = self.node_rpc.as_ref() {
            // Refresh the cache from the store's currently-resolvable (Committed) B2AGG notes
            // so notes still Committed from a PRIOR run are captured before they can be
            // consumed this run (import-time caching only covers notes imported THIS run).
            let all_input = client
                .get_input_notes(NoteFilter::All)
                .await
                .map_err(|e| anyhow::anyhow!("failed to get input notes: {e}"))?;
            self.cache_committed_b2agg_bodies(&all_input, "tick_scan");
            let txs = rpc
                .sync_transactions(
                    BlockNumber::from((cursor + 1) as u32),
                    BlockNumber::from(project_to as u32),
                    vec![self.bridge_id],
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!("sync_transactions({}..{}): {e}", cursor + 1, project_to)
                })?;
            // INSTR (observability-only): the sourcing window + a continuity check across ticks, so a
            // note whose consumption block fell in an un-sourced gap is visible. Pure logging —
            // reads/updates `last_source_window_to` but never affects windowing or control flow.
            let source_from = cursor + 1;
            tracing::info!(
                "INSTR source_window: from={} to={} reconcile_cursor={} tip={} tx_count={}",
                source_from,
                project_to,
                reconcile_cursor,
                tip,
                txs.len()
            );
            let prev_to = self
                .last_source_window_to
                .swap(project_to, Ordering::AcqRel);
            if prev_to != 0 && source_from != prev_to + 1 {
                tracing::info!(
                    "INSTR WINDOW_GAP: prev_to={} this_from={} (gap/overlap of {} blocks)",
                    prev_to,
                    source_from,
                    (source_from as i128) - (prev_to as i128 + 1)
                );
            }
            for tx in &txs {
                if tx.transaction_header.account_id() != self.bridge_id {
                    continue;
                }
                let nulls: Vec<String> = tx
                    .transaction_header
                    .input_notes()
                    .iter()
                    .map(|i| i.nullifier().to_hex())
                    .collect();
                tracing::info!(
                    "INSTR source_tx: tx_id={} block={} n_inputs={} nullifiers=[{}]",
                    tx.transaction_header.id().to_hex(),
                    tx.block_num.as_u64(),
                    nulls.len(),
                    nulls.join(",")
                );
            }
            let consumed_refs = bridge_consumed_nullifiers(&txs, self.bridge_id);
            // INSTR (observability-only): every attributed bridge consumption, keyed by nullifier.
            for (nullifier, cref) in &consumed_refs {
                tracing::info!(
                    "INSTR source_consumption: nullifier={} block={} order={} note_id={:?} \
                     authenticated={}",
                    nullifier.to_hex(),
                    cref.block,
                    cref.order,
                    cref.note_id.map(|i| i.to_hex()),
                    cref.note_id.is_none()
                );
            }
            let fetcher = RpcNoteFetcher(&**rpc);
            self.resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut evict_by_block)
                .await?
        } else {
            // No reconciler wired (pure unit tests / non-reconciling deployment): the
            // authoritative path is a no-op; tests drive `project_notes` directly.
            Vec::new()
        };
        for rec in &auth_b2agg {
            if let Some(h) = rec.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push(rec);
            }
        }
        let no_notes: Vec<&InputNoteRecord> = Vec::new();
        while cursor < project_to {
            let next = cursor + 1;
            let bucket = by_block.get(&next).unwrap_or(&no_notes);
            self.project_block_notes(bucket, &output_metadata, next, Some(client))
                .await?;
            // Advance the cursor only after the block is fully projected, so a
            // crash mid-block re-projects (idempotently) rather than skipping.
            // Persist BEFORE updating the in-memory cache so the durable cursor
            // never runs ahead of fully-projected state.
            self.store.set_projector_cursor(next).await?;
            self.cursor.store(next, Ordering::Release);
            // Evict this block's projected B2AGG nullifiers AFTER the cursor is persisted, so
            // the cache stays bounded to imported-but-not-yet-projected notes. Ordering it
            // after the persist means a crash before the persist leaves the entry in place
            // for the idempotent re-projection; a crash after (before evict) only leaks a
            // bounded, already-projected entry — never drops an event.
            if let Some(nulls) = evict_by_block.get(&next) {
                let mut cache = self
                    .recovered_bodies
                    .lock()
                    .expect("recovered-bodies cache poisoned");
                for nf in nulls {
                    cache.remove(nf);
                }
            }
            cursor = next;
        }
        // COMPLETENESS AUDITOR (detection only, every AUDIT_EVERY_N_TICKS ticks): diff the
        // store's consumed-B2AGG view against the synthetic log store for comfortably-sealed
        // blocks. Reuses this tick's already-fetched `consumed` feed — zero extra queries.
        // Non-fatal by construction: an audit failure warns and retries next cycle, it never
        // blocks projection.
        if self.node_rpc.is_some()
            && self
                .audit_tick_counter
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(AUDIT_EVERY_N_TICKS)
            && let Err(e) = self.audit_completeness(&consumed, cursor).await
        {
            tracing::warn!(
                error = %format!("{e:#}"),
                "completeness auditor failed (non-fatal — retried next cycle)"
            );
        }
        // Observability: the projector follows the MIDEN chain, so its progress is
        // measured against the Miden tip (NOT L1). With the #30 barrier the projector
        // catches up to `project_to = min(tip, reconcile_cursor)`, NOT necessarily the raw
        // tip: `projector_cursor == project_to` means caught up to the projection ceiling,
        // and `projector_cursor < miden_tip` (barrier_held > 0) means the barrier is
        // holding until the reconciler catches up. `synthetic_tip` is the actual synthetic
        // L2 block number the chain is exposing. Logged once per tick that did work.
        let synthetic_tip = self.store.get_latest_block_number().await?;
        tracing::info!(
            miden_tip = tip,
            project_to,
            projector_cursor = cursor,
            // Delta to the Miden tip — how far the projected head is lagging the chain. 0 =
            // fully caught up; > 0 = the visibility barrier is holding at the reconcile
            // frontier (== tip - project_to at tick end) until the reconciler catches up.
            blocks_behind_tip = tip.saturating_sub(cursor),
            synthetic_tip,
            "synthetic projector tick: caught up to the projection ceiling (min(tip, reconcile_cursor))"
        );
        Ok(cursor)
    }
}

/// One completeness-audit cycle's tallies (see [`SyntheticProjector::audit_completeness`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AuditOutcome {
    /// Notes checked against the synthetic log store this cycle (new, past the margin).
    pub audited: usize,
    /// Audited notes whose BridgeEvent was found at the exact consumption block.
    pub verified: usize,
    /// Audited notes with NO BridgeEvent at their consumption block — violations (alarmed).
    pub missing: usize,
    /// Notes still inside the settle margin — deferred to a later cycle.
    pub settling: usize,
}

/// One bridge-consumed input note, attributed from the bridge's transaction feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConsumedRef {
    /// Finalized block the bridge consumed the note in.
    pub block: u64,
    /// Per-block order of the consuming bridge transaction (intra-block determinism).
    pub order: u32,
    /// The consumed note's id — carried by the consuming transaction IFF the input was
    /// UNAUTHENTICATED ([`InputNoteCommitment::header`] is `Some` exactly then). An
    /// authenticated consumption carries no header, hence `None`; that is precisely the
    /// case where the note lived long enough to be discovered and cached, so the projector
    /// never needs to fetch it by id. When present, it lets the projector resolve a note
    /// body the cache never captured (created+consumed under load before import) by fetching
    /// it authoritatively from the node.
    pub note_id: Option<NoteId>,
}

/// A public note body fetched by id from the node — enough to rebuild a ConsumedExternal
/// record for projection. Private notes are dropped by the fetcher (a real public B2AGG
/// exit is always public), so callers only ever see public bodies.
#[derive(Clone)]
pub(crate) struct FetchedBody {
    pub id: NoteId,
    pub details: NoteDetails,
    pub attachments: NoteAttachments,
}

/// The result of an authoritative fetch: the decoded PUBLIC bodies AND the full set of ids
/// the node actually RETURNED (public or private, before any filtering). The `returned_ids`
/// set is the signal that lets the caller distinguish two very different cache-miss outcomes
/// for a note id it asked for: RETURNED-but-not-a-public-b2agg (proven not an exit → safe
/// skip) vs NOT-RETURNED (the node omitted it — a `sync_transactions`-ahead-of-note-DB load
/// race → fail-closed, retry). Without it a not-yet-indexed just-consumed note would silently
/// drop its BridgeEvent, the acfee0cb-class bug one level down.
pub(crate) struct FetchedBodies {
    pub bodies: Vec<FetchedBody>,
    pub returned_ids: HashSet<NoteId>,
}

/// The single node-RPC capability the authoritative B2AGG resolution needs: fetch the note
/// bodies for a set of ids, decoded to public [`FetchedBody`]s plus the set of ids the node
/// returned. Narrowed to one method so it is trivially mockable in unit tests — the full
/// [`NodeRpcClient`] has ~30 methods and needs a live node.
#[async_trait::async_trait]
pub(crate) trait PublicNoteFetcher: Send + Sync {
    async fn fetch_public_bodies(&self, ids: &[NoteId]) -> anyhow::Result<FetchedBodies>;
}

/// Production [`PublicNoteFetcher`] over a live node RPC client. A `Sized` wrapper (not a
/// blanket `impl … for dyn NodeRpcClient`) because a `&dyn NodeRpcClient` cannot be coerced
/// to `&dyn PublicNoteFetcher` — both are unsized, so the fat-pointer vtable can only be
/// built from a `Sized` source.
pub(crate) struct RpcNoteFetcher<'a>(pub &'a dyn NodeRpcClient);

#[async_trait::async_trait]
impl PublicNoteFetcher for RpcNoteFetcher<'_> {
    async fn fetch_public_bodies(&self, ids: &[NoteId]) -> anyhow::Result<FetchedBodies> {
        let fetched = self
            .0
            .get_notes_by_id(ids)
            .await
            .map_err(|e| anyhow::anyhow!("get_notes_by_id({}): {e}", ids.len()))?;
        let mut bodies = Vec::with_capacity(fetched.len());
        let mut returned_ids = HashSet::with_capacity(fetched.len());
        for f in fetched {
            let id = f.id();
            // Record EVERY id the node responded with (public or private) BEFORE filtering — a
            // returned-but-non-public id is provably not an exit; a NOT-returned id is unknown.
            returned_ids.insert(id);
            let FetchedNote::Public(note, _inclusion) = f else {
                tracing::debug!(
                    note_id = %id.to_hex(),
                    "authoritative fetch: node returned a non-public note (cannot be a real exit)"
                );
                continue;
            };
            let attachments = note.attachments().clone();
            let details: NoteDetails = note.into();
            bodies.push(FetchedBody {
                id,
                details,
                attachments,
            });
        }
        Ok(FetchedBodies {
            bodies,
            returned_ids,
        })
    }
}

/// MA#3 reclaim gate for the authoritative consumption path: map every nullifier consumed
/// by a BRIDGE-executed transaction to a [`ConsumedRef`] (spend block, per-block bridge-tx
/// order, and — for unauthenticated inputs — the note id).
///
/// The node's `sync_transactions` feed is filtered per account and each
/// transaction header commits to the nullifiers of the notes that transaction
/// consumed, so membership here is exact on-chain attribution of the consumer —
/// the same condition [`crate::bridge_out::classify_b2agg_consumer`] gates on
/// (`consumer == bridge`). The account-id re-check is fail-closed defense in
/// depth against a node that ignores the server-side filter. Pure (no I/O) so
/// it is unit-testable directly.
pub(crate) fn bridge_consumed_nullifiers(
    txs: &[TransactionRecord],
    bridge_id: AccountId,
) -> HashMap<Nullifier, ConsumedRef> {
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
        for input in tx.transaction_header.input_notes().iter() {
            // `header()` is `Some` IFF the input is unauthenticated — then it carries the
            // NoteId, letting `tick` fetch an uncached body authoritatively.
            let note_id = input.header().map(|h| h.id());
            out.insert(
                input.nullifier(),
                ConsumedRef {
                    block,
                    order,
                    note_id,
                },
            );
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
        NoteAssets, NoteAttachment, NoteAttachments, NoteDetails, NoteHeader, NoteId, NoteMetadata,
        NoteRecipient, NoteStorage, NoteType, PartialNoteMetadata,
    };
    use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint};
    use std::sync::Arc as StdArc;

    /// Reconciler private-note wedge (0.15.5 hotfix): the exact miden-client
    /// rejection that froze the retroactive-heal sweep must be classified as
    /// skippable, so the reconciler drops just the private note and advances —
    /// while unrelated errors still propagate and fail the tick (stay loud).
    // #30 visibility barrier: the projection ceiling MUST NOT advance the projector past
    // the reconciler's last completed sweep, so a synthetic block is only ever sealed after
    // its notes are visible. These pin the pure ceiling function that `tick` uses.
    #[test]
    fn barrier_never_projects_past_reconcile_cursor() {
        // Reconciler behind the tip: ceiling is clamped to the reconcile frontier.
        assert_eq!(
            SyntheticProjector::barrier_project_to(true, 100, 40),
            40,
            "barrier must cap projection at reconcile_cursor when the reconciler is behind"
        );
        // Reconciler exactly at the tip: caught up, project the whole chain.
        assert_eq!(SyntheticProjector::barrier_project_to(true, 100, 100), 100);
        // Reconciler AHEAD of the tip (persisted cursor ahead / reorg): clamp back to tip,
        // never project a block that doesn't exist yet.
        assert_eq!(SyntheticProjector::barrier_project_to(true, 100, 150), 100);
        // The invariant, stated directly: with the barrier active the ceiling is never past
        // min(tip, reconcile_cursor) — i.e. never past reconcile_cursor while it is <= tip.
        for (tip, rc) in [(100u64, 0u64), (100, 1), (100, 99), (5, 3), (0, 0)] {
            assert!(SyntheticProjector::barrier_project_to(true, tip, rc) <= rc.min(tip));
            assert!(SyntheticProjector::barrier_project_to(true, tip, rc) <= tip);
        }
    }

    #[test]
    fn no_barrier_projects_to_raw_tip() {
        // Without a reconciler wired the projector keeps legacy behavior: project to tip.
        assert_eq!(SyntheticProjector::barrier_project_to(false, 100, 40), 100);
        assert_eq!(SyntheticProjector::barrier_project_to(false, 7, 0), 7);
    }

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
        note_ids: Vec<NoteId>,
    }

    impl FakeFetcher {
        fn new(fail_from: Option<u64>) -> StdArc<Self> {
            Self::with_note_ids(fail_from, Vec::new())
        }

        fn with_note_ids(fail_from: Option<u64>, note_ids: Vec<NoteId>) -> StdArc<Self> {
            StdArc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_from,
                note_ids,
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
            Ok(self.note_ids.clone())
        }
    }

    /// A raw `sync_notes` NoteId is enough to close a PREPARED handoff even
    /// when the subsequent body import cannot run. Conversely, a failed fetch
    /// must neither confirm the handoff nor move the durable low-water mark.
    #[tokio::test]
    async fn reconcile_raw_note_id_confirmation_is_ordered_before_cursor_advance() {
        let note_id = NoteId::from_raw(Word::new([Felt::new(0x42).unwrap(); 4]));
        let tx_hash = "0xprepared-note";

        // Successful raw-id observation: confirmation happens first. Supplying
        // no client deliberately fails the later body-import step, proving the
        // cursor is still held while the exact handoff is already confirmed.
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        store
            .prepare_note_handoff(tx_hash, "details-commitment", &note_id.to_hex(), 100)
            .await
            .unwrap();
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state)
            .await
            .with_reconcile_tuning(200, 1, Duration::ZERO);
        let fetcher = FakeFetcher::with_note_ids(None, vec![note_id]);
        let f: StdArc<dyn ReconcileFetcher> = fetcher;
        let err = projector
            .reconcile_notes_with(None, None, &f, 200)
            .await
            .expect_err("candidate import without a client must fail");
        assert!(format!("{err:#}").contains("no client handle"));
        assert_eq!(
            store
                .get_note_handoff_for_tx(tx_hash)
                .await
                .unwrap()
                .unwrap()
                .state,
            crate::store::NoteHandoffState::Submitted,
            "raw NoteId observation must confirm before body import"
        );
        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            0,
            "cursor must not advance when later window work fails"
        );

        // Failed window fetch: no raw ids were authoritatively observed, so
        // neither confirmation nor cursor advancement is allowed.
        let failed_store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        failed_store
            .prepare_note_handoff(tx_hash, "details-commitment", &note_id.to_hex(), 100)
            .await
            .unwrap();
        let failed_projector = test_projector(&failed_store, &block_state)
            .await
            .with_reconcile_tuning(200, 1, Duration::ZERO);
        let failed_fetcher = FakeFetcher::with_note_ids(Some(1), vec![note_id]);
        let failed: StdArc<dyn ReconcileFetcher> = failed_fetcher;
        failed_projector
            .reconcile_notes_with(None, None, &failed, 200)
            .await
            .expect_err("injected fetch failure must fail the window");
        assert_eq!(
            failed_store
                .get_note_handoff_for_tx(tx_hash)
                .await
                .unwrap()
                .unwrap()
                .state,
            crate::store::NoteHandoffState::Prepared,
            "a failed fetch must not confirm an unobserved NoteId"
        );
        assert_eq!(failed_store.get_reconcile_cursor().await.unwrap(), 0);
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
            .project_notes(&notes, &output_metadata, 5, None)
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
            .project_notes(&notes, &output_metadata, 7, None)
            .await
            .unwrap();
        assert_eq!(first, 3);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 7);

        let second = projector
            .project_notes(&notes, &output_metadata, 7, None)
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
                .project_notes(&notes, &output_metadata, 3, None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 3);
        // Project Miden block 8: only the CLAIM note belongs here → synthetic 8.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 8, None)
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
            .project_notes(&same_block, &output_metadata, 100, None)
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
            .project_notes(&later_block, &output_metadata, 250, None)
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

    /// Audit H4 — the late-consumption sweep mixes notes consumed at EARLIER
    /// (sealed) Miden blocks into the current projection block. The intra-block
    /// sort MUST preserve global on-chain consumption order so the `deposit_count`
    /// (which the autoclaim reads as the on-chain LET `leaf_index`) matches the
    /// bridge's actual leaf positions — otherwise the autoclaim builds an SMT
    /// proof against the wrong leaf and the exit is unclaimable.
    ///
    /// Pre-fix the sort keyed on `(consumed_tx_order, note_id)` alone, so a late
    /// note consumed at block 3 with tx_order 5 sorted AFTER an on-time note at
    /// block 6 with tx_order 0 — even though on-chain the block-3 note was
    /// consumed first. The fix adds `consumed_block_height` as the primary key.
    #[tokio::test]
    async fn h4_late_sweep_preserves_on_chain_consumption_order() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        // Note A: consumed at Miden block 3 (discovered late, swept forward),
        // tx_order 5. Distinct amount → distinct details commitment.
        let note_a = b2agg_note_with_amount(3, Some(5), 50);
        // Note B: consumed at Miden block 6 (on-time), tx_order 0.
        // Lower tx_order so the OLD (tx_order-first) sort would put B first.
        let note_b = b2agg_note_with_amount(6, Some(0), 60);
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Simulate the late-sweep: both projected into synthetic block 6 (note A
        // was swept forward because its real block 3 is sealed behind the cursor).
        // `tick()` calls `project_block_notes` directly with the mixed bucket, so
        // we do the same (project_notes would re-filter by block height).
        let notes_ref: Vec<&InputNoteRecord> = vec![&note_a, &note_b];
        projector
            .project_block_notes(&notes_ref, &HashMap::new(), 6, None)
            .await
            .unwrap();

        let logs = logs_in_range(&store, 0, 6).await;
        let tx_a = crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
            note_a.details_commitment().as_bytes(),
        ));
        let tx_b = crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
            note_b.details_commitment().as_bytes(),
        ));
        let idx_a = logs
            .iter()
            .position(|l| l.transaction_hash == tx_a)
            .expect("note A emitted a BridgeEvent");
        let idx_b = logs
            .iter()
            .position(|l| l.transaction_hash == tx_b)
            .expect("note B emitted a BridgeEvent");

        // A was consumed on-chain BEFORE B (block 3 < block 6), so A must get
        // the LOWER deposit_count → it must project (log_index) first.
        assert!(
            idx_a < idx_b,
            "late note consumed at block 3 must project before on-time note at block 6 \
             (on-chain LET leaf order); got idx_a={idx_a} idx_b={idx_b}"
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
                .project_notes(&notes, &output_metadata, 9, None)
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
                .project_notes(&notes, &output_metadata, 5, None)
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
            .project_notes(&notes, &HashMap::new(), 4, None)
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
            .project_notes(&notes, &HashMap::from([ger_meta]), 4, None)
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
        fn tx(
            account: AccountId,
            block: u32,
            commitment: InputNoteCommitment,
        ) -> TransactionRecord {
            TransactionRecord {
                block_num: BlockNumber::from(block),
                transaction_header: TransactionHeader::new(
                    account,
                    Word::empty(),
                    Word::empty(),
                    InputNotes::new(vec![commitment]).unwrap(),
                    vec![],
                    FungibleAsset::new(aid(FAUCET), 0).unwrap(),
                ),
                output_notes: vec![],
                erased_output_notes: vec![],
            }
        }

        let (a, b, c) = (nf(1), nf(2), nf(3));
        // Authenticated inputs (nullifier only, no header) — the common case.
        let auth = |n: Nullifier| InputNoteCommitment::from(n);
        // An UNAUTHENTICATED input carries a note header → the note id is recoverable. Build a
        // real header so its `id()` is what `bridge_consumed_nullifiers` must surface.
        let details_commitment = b2agg_note(5, Some(0)).details_commitment();
        let metadata = NoteMetadata::new(
            PartialNoteMetadata::new(aid(BRIDGE), NoteType::Public),
            &NoteAttachments::default(),
        );
        let header = NoteHeader::new(details_commitment, metadata);
        let expected_id = header.id();
        let unauth = InputNoteCommitment::from_parts_unchecked(nf(4), Some(header));

        let txs = vec![
            tx(aid(BRIDGE), 9, auth(a)),  // bridge consumption → attributed, order 0
            tx(aid(SERVICE), 9, auth(b)), // sender reclaim → NOT attributed
            tx(aid(BRIDGE), 9, auth(c)),  // second bridge tx in the block → order 1
            tx(aid(BRIDGE), 9, unauth), // unauthenticated bridge consumption → order 2, id carried
        ];
        let map = bridge_consumed_nullifiers(&txs, aid(BRIDGE));
        assert_eq!(
            map.get(&a),
            Some(&ConsumedRef {
                block: 9,
                order: 0,
                note_id: None
            }),
            "authenticated input → attributed with no note id"
        );
        assert_eq!(
            map.get(&c),
            Some(&ConsumedRef {
                block: 9,
                order: 1,
                note_id: None
            }),
            "per-block bridge-tx order increments"
        );
        assert_eq!(
            map.get(&nf(4)),
            Some(&ConsumedRef {
                block: 9,
                order: 2,
                note_id: Some(expected_id)
            }),
            "unauthenticated input must carry the consuming tx's note id (enables authoritative fetch)"
        );
        assert!(
            !map.contains_key(&b),
            "non-bridge consumption must be gated out (MA#3 fail-closed)"
        );
    }

    /// Regression lock for the dropped-BridgeEvent bug: the reason the projector resolves
    /// B2AGG bodies from the `recovered_bodies` cache (fed while notes are Committed) instead
    /// of a live-store nullifier map. Once `sync_state` marks a B2AGG note ConsumedExternal,
    /// [`InputNoteRecord::nullifier`] returns `None` (the metadata the nullifier is derived
    /// from is gone), so the note vanishes from any store map keyed on `nullifier()` — and on
    /// a fresh-stack catch-up the note is routinely consumed BEFORE the projector reaches its
    /// block, which silently dropped its BridgeEvent. Two invariants pin this down:
    ///   1. a consumed B2AGG note has no `nullifier()`, and
    ///   2. feeding an already-consumed note to `cache_committed_b2agg_bodies` caches NOTHING
    ///      (no key to store it under) — so bodies MUST be captured earlier, while Committed.
    #[tokio::test]
    async fn consumed_b2agg_note_cannot_be_cached_post_hoc() {
        let note = b2agg_note(7, Some(0));
        assert!(
            note.nullifier().is_none(),
            "a ConsumedExternal note must have no nullifier() — the exact miden-client \
             behavior the body cache exists to work around"
        );

        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // An already-consumed note has no nullifier to key on → nothing is cached. This is
        // WHY `cache_committed_b2agg_bodies` runs at import time and every tick over the
        // still-Committed store notes, before any of them can transition to ConsumedExternal.
        projector.cache_committed_b2agg_bodies(std::slice::from_ref(&note), "tick_scan");
        assert!(
            projector
                .recovered_bodies
                .lock()
                .expect("cache poisoned")
                .is_empty(),
            "a consumed note cannot be cached post-hoc; the cache is fed while Committed"
        );
    }

    /// A [`PublicNoteFetcher`] test double standing in for the node's `get_notes_by_id`.
    /// `bodies` are the public B2AGG-eligible bodies the node returns; `also_returned` are ids
    /// the node returns but WITHOUT a public b2agg body (e.g. a public CLAIM/GER or a private
    /// note) — present in `returned_ids` but absent from `bodies`. An id in neither is one the
    /// node did NOT return at all (the load-race case).
    #[derive(Default)]
    struct MockFetcher {
        bodies: Vec<FetchedBody>,
        also_returned: Vec<NoteId>,
    }
    #[async_trait::async_trait]
    impl PublicNoteFetcher for MockFetcher {
        async fn fetch_public_bodies(&self, ids: &[NoteId]) -> anyhow::Result<FetchedBodies> {
            let bodies: Vec<FetchedBody> = self
                .bodies
                .iter()
                .filter(|b| ids.contains(&b.id))
                .cloned()
                .collect();
            let mut returned_ids: HashSet<NoteId> = bodies.iter().map(|b| b.id).collect();
            returned_ids.extend(
                self.also_returned
                    .iter()
                    .filter(|id| ids.contains(id))
                    .copied(),
            );
            Ok(FetchedBodies {
                bodies,
                returned_ids,
            })
        }
    }

    fn nullifier(byte: u64) -> super::Nullifier {
        super::Nullifier::from_raw(Word::new([Felt::new(byte).unwrap(); 4]))
    }

    /// The regression for note `0xacfee0cb…` (N=30 loadtest, exactly 1 missing BridgeEvent): a
    /// bridge consumption whose body the projector never cached — because the note was
    /// created+consumed under load before the reconciler imported it Committed — must still be
    /// resolved and emitted at its exact block. Such a consumption is UNAUTHENTICATED, so the
    /// consuming tx carried the note id; `resolve_b2agg_consumptions` fetches the body by id
    /// (authoritative backstop) instead of silently dropping it.
    #[tokio::test]
    async fn uncached_unauthenticated_consumption_resolves_via_authoritative_fetch() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await; // cache empty

        // A real B2AGG body the node would return, and the id the consuming tx carries.
        let body_note = b2agg_note(544, Some(0));
        let details = body_note.details().clone();
        let note_id = NoteId::new(
            body_note.details_commitment(),
            &NoteMetadata::new(
                PartialNoteMetadata::new(aid(BRIDGE), NoteType::Public),
                &NoteAttachments::default(),
            ),
        );
        let nf = nullifier(0xac);

        let consumed_refs = HashMap::from([(
            nf,
            ConsumedRef {
                block: 544,
                order: 0,
                note_id: Some(note_id),
            },
        )]);
        let fetcher = MockFetcher {
            bodies: vec![FetchedBody {
                id: note_id,
                details: details.clone(),
                attachments: NoteAttachments::default(),
            }],
            ..Default::default()
        };
        let mut evict: HashMap<u64, Vec<super::Nullifier>> = HashMap::new();
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut evict)
            .await
            .expect("uncached-but-unauthenticated consumption must resolve, not error");

        assert_eq!(
            recs.len(),
            1,
            "the uncached unauthenticated consumption must resolve via authoritative fetch"
        );
        assert_eq!(
            recs[0].state().consumed_block_height().map(|h| h.as_u64()),
            Some(544),
            "resolved at the exact consumption block"
        );
        assert!(
            is_b2agg_note(recs[0].details()),
            "the fetched body is the B2AGG note"
        );
        assert_eq!(
            evict.get(&544).map(|v| v.as_slice()),
            Some([nf].as_slice()),
            "the resolved nullifier is queued for post-seal eviction"
        );
    }

    /// The block-13 un-wedge regression: an AUTHENTICATED bridge consumption (no note id in the
    /// tx) with no cached body is NORMALLY a legit non-B2AGG note (CLAIM/GER/genesis setup) the
    /// store consumed feed covers — the B2AGG-only cache never holds it. It must be a SAFE SKIP
    /// (no Err, no record, tick advances); an earlier version fail-closed here and froze the
    /// synthetic tip at 0 on the first block-13 GER/genesis consumption (244× ERROR, dead bridge).
    #[tokio::test]
    async fn authenticated_uncached_consumption_is_a_safe_skip_never_wedges() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Block-13-shaped: an authenticated bridge consumption, no cached body, no note id.
        let nf = nullifier(0x64);
        let consumed_refs = HashMap::from([(
            nf,
            ConsumedRef {
                block: 13,
                order: 0,
                note_id: None, // authenticated: no id, and not in the B2AGG cache
            },
        )]);
        let fetcher = MockFetcher::default();
        let mut evict: HashMap<u64, Vec<super::Nullifier>> = HashMap::new();
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut evict)
            .await
            .expect("authenticated uncached consumption must be a safe skip, NEVER an Err");
        assert!(
            recs.is_empty(),
            "an authenticated non-B2AGG consumption emits no record (store feed covers it)"
        );
        assert!(
            evict.is_empty(),
            "nothing resolved → nothing queued for eviction; the tick advances"
        );
    }

    /// Case (B) — the acfee0cb backstop under a note-DB lag, with the un-wedge guard. A bridge
    /// consumption that is uncached + unauthenticated (note id in the tx) but that
    /// `get_notes_by_id` does NOT return — `sync_transactions` is ahead of the node's note DB.
    /// The tick HOLDS (Errs, retries) for a bounded window so a transient lag can resolve the
    /// real body; past [`FETCH_MISS_RETRY_BOUND`] it LOUD-SKIPS and advances rather than freezing
    /// the tip forever. Both halves are asserted: it retries within the bound, then stops.
    #[tokio::test]
    async fn uncached_unauthenticated_fetch_omitted_retries_then_loud_skips() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let note_id = NoteId::new(
            b2agg_note(880, Some(0)).details_commitment(),
            &NoteMetadata::new(
                PartialNoteMetadata::new(aid(BRIDGE), NoteType::Public),
                &NoteAttachments::default(),
            ),
        );
        let nf = nullifier(0xb0);
        let cref = ConsumedRef {
            block: 880,
            order: 0,
            note_id: Some(note_id),
        };
        // The node returns NOTHING for the requested id (never indexed) on every tick.
        let fetcher = MockFetcher::default();

        // Within the bound: each tick HOLDS (Err), so the block never seals without its exit.
        for attempt in 1..=FETCH_MISS_RETRY_BOUND {
            let mut evict: HashMap<u64, Vec<super::Nullifier>> = HashMap::new();
            let err = projector
                .resolve_b2agg_consumptions(&fetcher, HashMap::from([(nf, cref)]), &mut evict)
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("not yet in the node's note DB"),
                "attempt {attempt} must hold the tick with the load-race reason: {err:#}"
            );
            assert!(evict.is_empty(), "a held tick queues no eviction");
        }

        // Past the bound: LOUD-SKIP + advance (Ok, no record) — the tip is never frozen forever.
        let mut evict: HashMap<u64, Vec<super::Nullifier>> = HashMap::new();
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, HashMap::from([(nf, cref)]), &mut evict)
            .await
            .expect("past the retry bound the tick must advance (loud-skip), never freeze");
        assert!(
            recs.is_empty(),
            "the unresolvable note is skipped, not emitted"
        );
        assert!(evict.is_empty(), "nothing resolved → nothing to evict");
    }

    /// Case (A) — the complement: a bridge consumption the node DID return, but as a non-b2agg
    /// note (the bridge legitimately consumes CLAIM/GER). This is provably NOT a public exit, so
    /// it is a safe skip — no record, no error, no eviction. Skipping here (not fail-closing) is
    /// what keeps a legit CLAIM/GER bridge consumption from wedging the tip.
    #[tokio::test]
    async fn returned_non_b2agg_consumption_is_a_safe_skip_not_a_wedge() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // A CLAIM note's id: the node returns it, but its body is not B2AGG.
        let claim = claim_note(901, Some(0));
        let note_id = NoteId::new(
            claim.details_commitment(),
            &NoteMetadata::new(
                PartialNoteMetadata::new(aid(BRIDGE), NoteType::Public),
                &NoteAttachments::default(),
            ),
        );
        let nf = nullifier(0xc1);
        let consumed_refs = HashMap::from([(
            nf,
            ConsumedRef {
                block: 901,
                order: 0,
                note_id: Some(note_id),
            },
        )]);
        // Node RETURNS the id (in returned_ids) but with no public b2agg body.
        let fetcher = MockFetcher {
            bodies: vec![],
            also_returned: vec![note_id],
        };
        let mut evict: HashMap<u64, Vec<super::Nullifier>> = HashMap::new();
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut evict)
            .await
            .expect("a returned non-b2agg note must be a safe skip, not an error");
        assert!(
            recs.is_empty(),
            "a non-b2agg bridge consumption emits no B2AGG record"
        );
        assert!(
            evict.is_empty(),
            "nothing resolved → nothing queued for eviction"
        );
    }

    /// Completeness auditor, happy + violation paths. A consumed B2AGG note past the settle
    /// margin WITH its BridgeEvent at the exact consumption block verifies (no alarm); one
    /// WITHOUT is a violation (missing=1).
    #[tokio::test]
    async fn completeness_auditor_flags_missing_event_and_passes_present_one() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Note A: consumed at block 5 and PROJECTED (real event in the synthetic store).
        let emitted = b2agg_note_with_amount(5, Some(0), 11);
        assert_eq!(
            projector
                .project_notes(std::slice::from_ref(&emitted), &HashMap::new(), 5, None)
                .await
                .unwrap(),
            1,
            "fixture: the event must actually be emitted"
        );
        // Note B: consumed at block 6, NEVER projected — the drop the auditor must catch.
        let dropped = b2agg_note_with_amount(6, Some(0), 22);

        // Cursor far enough that both blocks are past the settle margin.
        let outcome = projector
            .audit_completeness(&[emitted.clone(), dropped.clone()], 6 + AUDIT_SETTLE_MARGIN)
            .await
            .unwrap();
        assert_eq!(outcome.audited, 2);
        assert_eq!(
            outcome.verified, 1,
            "the emitted note verifies at its exact block"
        );
        assert_eq!(
            outcome.missing, 1,
            "the dropped note must be flagged as a completeness violation"
        );
    }

    /// Settle margin: a consumption newer than `audit_to` is NOT audited yet — no false
    /// positive while the store's (lagging) consumption view catches up. It IS audited (and
    /// flagged) once the cursor moves past the margin.
    #[tokio::test]
    async fn completeness_auditor_defers_consumptions_inside_settle_margin() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Consumed at block 50, never projected; cursor only 3 past it (< margin).
        let fresh = b2agg_note_with_amount(50, Some(0), 33);
        let outcome = projector
            .audit_completeness(std::slice::from_ref(&fresh), 53)
            .await
            .unwrap();
        assert_eq!(outcome.audited, 0, "inside the margin: not audited");
        assert_eq!(outcome.missing, 0, "inside the margin: no false positive");
        assert_eq!(outcome.settling, 1, "deferred, not forgotten");

        // Cursor past the margin: now audited and flagged.
        let outcome = projector
            .audit_completeness(std::slice::from_ref(&fresh), 50 + AUDIT_SETTLE_MARGIN)
            .await
            .unwrap();
        assert_eq!(outcome.audited, 1);
        assert_eq!(outcome.missing, 1, "past the margin the drop is caught");
    }

    /// Alarm de-dupe: the same missing note alarms exactly ONCE — later cycles skip it (the
    /// per-cycle `missing` tally returns to 0; the metric counter stays cumulative). A
    /// verified note is equally never re-checked.
    #[tokio::test]
    async fn completeness_auditor_alarms_once_per_note() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let dropped = b2agg_note_with_amount(7, Some(0), 44);
        let cursor = 7 + AUDIT_SETTLE_MARGIN;

        let first = projector
            .audit_completeness(std::slice::from_ref(&dropped), cursor)
            .await
            .unwrap();
        assert_eq!(first.missing, 1, "first cycle alarms");

        let second = projector
            .audit_completeness(std::slice::from_ref(&dropped), cursor)
            .await
            .unwrap();
        assert_eq!(
            second.missing, 0,
            "second cycle must NOT re-alarm (deduped)"
        );
        assert_eq!(second.audited, 0, "already-resolved notes are skipped");
    }

    /// Emit-gate mirroring: notes the projector legitimately refuses to emit (reclaimed by a
    /// non-bridge consumer — MA#3; self-targeted destination — #13) must never false-alarm.
    #[tokio::test]
    async fn completeness_auditor_skips_legitimately_unemitted_notes() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Reclaimed: consumed by the SERVICE account, not the bridge → no event expected.
        let reclaimed = {
            let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
            let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
            let asset: Asset = FungibleAsset::new(aid(FAUCET), 55).unwrap().into();
            let assets = NoteAssets::new(vec![asset]).unwrap();
            consumed_note(
                NoteDetails::new(assets, recipient),
                NoteAttachments::default(),
                Some(aid(SERVICE)),
                8,
                Some(0),
            )
        };
        // Self-targeted: destination_network == local (7) → poison leaf, never emits. The
        // first storage felt encodes the destination network byte-swapped (see
        // parse_b2agg_storage): 7u32 → swap → 0x07000000.
        let self_targeted = {
            let mut felts = vec![Felt::from(0u32); 6];
            felts[0] = Felt::from(u32::from_le_bytes(7u32.to_be_bytes()));
            let storage = NoteStorage::new(felts).unwrap();
            let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
            let asset: Asset = FungibleAsset::new(aid(FAUCET), 66).unwrap().into();
            let assets = NoteAssets::new(vec![asset]).unwrap();
            consumed_note(
                NoteDetails::new(assets, recipient),
                NoteAttachments::default(),
                Some(aid(BRIDGE)),
                8,
                Some(1),
            )
        };
        let outcome = projector
            .audit_completeness(&[reclaimed, self_targeted], 8 + AUDIT_SETTLE_MARGIN)
            .await
            .unwrap();
        assert_eq!(
            outcome.missing, 0,
            "legitimately-unemitted notes must never false-alarm"
        );
        assert_eq!(
            outcome.audited, 0,
            "gated notes are excluded before the log check"
        );
    }
}
