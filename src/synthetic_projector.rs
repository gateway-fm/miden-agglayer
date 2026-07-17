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
//! `eth_blockNumber` tracks the Miden tip. Within each block, notes are ordered by
//! consuming transaction, B2AGG input position, details commitment, and NoteId. Re-running the
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
use crate::miden_client::{
    MidenClientLib, SyncListener, ensure_complete_note_response, ordered_account_transactions,
};
use crate::restore::{
    B2AggRestoreOutcome, ClaimProjectOutcome, GerProjectOutcome, project_b2agg_note,
    project_claim_note, project_ger_note,
};
use crate::store::Store;
use crate::writer_worker::DecodedWriteCall;
use alloy::primitives::TxHash;
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

/// Maximum exact-note duplicate checks per projector tick. The projector tick
/// is single-flight; this bound prevents a damaged backlog from starving normal
/// projection while guaranteeing steady progress.
const PENDING_DUPLICATE_RECONCILE_LIMIT: usize = 16;

/// Per-tick time budget for the reconciler's catch-up loop, in milliseconds.
/// Env-tunable via `RECONCILE_TICK_BUDGET_MS`. Projection runs AFTER the
/// reconciler inside `tick`, so the budget bounds how long a deep catch-up can
/// starve projection (and the 5s sync cadence): at least one window batch is
/// always processed per tick (guaranteed progress), then the loop stops once
/// the budget is spent. When the sweep is caught up the budget is irrelevant —
/// the single near-tip window completes in one iteration exactly as before.
const RECONCILE_TICK_BUDGET_MS_DEFAULT: u64 = 2_000;

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
struct PendingDuplicate {
    tx_hash: TxHash,
    call: DecodedWriteCall,
    note_id: String,
}

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
    /// bridge-outs missing under load). The reconciler walks creation blocks via `sync_notes`
    /// and persists body identity before the authoritative transaction feed supplies
    /// consumption order.
    node_rpc: Arc<dyn NodeRpcClient>,
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
    // Last transaction hash considered by the bounded duplicate sweep. This
    // process-local cursor is sufficient because one projector owns reconciliation.
    pending_duplicate_cursor: std::sync::Mutex<Option<TxHash>>,
    /// Completeness auditor (detection only, no healing): note occurrences already
    /// VERIFIED (BridgeEvent found at the exact consumption block) or already ALARMED
    /// (missing — alarm once, counter cumulative). Skipping these keeps each ~30s audit
    /// cycle O(new consumptions) and de-dupes alarms. In-memory on purpose: a restart
    /// re-audits from scratch, which is cheap and re-surfaces any standing violation.
    audit_resolved: std::sync::Mutex<HashSet<([u8; 32], u64, u32)>>,
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
        node_url: String,
        node_api_key: Option<String>,
    ) -> anyhow::Result<Self> {
        // Build a dedicated RPC handle for the note reconciler (the live
        // MidenClientLib does not expose its RPC client). Same URL resolution
        // as MidenClient itself.
        let endpoint = crate::miden_client::parse_node_url(&node_url)?;
        let node_rpc =
            crate::miden_client::build_rpc_client(&endpoint, 10_000, node_api_key.as_deref());
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
            pending_duplicate_cursor: std::sync::Mutex::new(None),
            audit_resolved: std::sync::Mutex::new(HashSet::new()),
            audit_tick_counter: AtomicU64::new(0),
        })
    }

    async fn load_pending_duplicate(
        &self,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<PendingDuplicate>> {
        let Some(transaction) = self.store.txn_get(tx_hash).await? else {
            return Ok(None);
        };
        if transaction.result.is_some() {
            return Ok(None);
        }
        let Some(handoff) = self
            .store
            .get_note_handoff_for_tx(&format!("{tx_hash:#x}"))
            .await?
        else {
            return Ok(None);
        };
        let Some(note_id) = handoff.note_id else {
            return Ok(None);
        };
        let call = crate::service_send_raw_txn::decode_envelope_write_call(&transaction.envelope)?;
        Ok(Some(PendingDuplicate {
            tx_hash,
            call,
            note_id,
        }))
    }

    async fn resolve_pending_duplicate(
        &self,
        client: &mut MidenClientLib,
        pending: &PendingDuplicate,
    ) -> anyhow::Result<crate::applied_state::ExactNoteOutcome> {
        match &pending.call {
            DecodedWriteCall::Ger { ger_bytes } => {
                crate::applied_state::reconcile_ger_handoff_with_client(
                    self.store.as_ref(),
                    client,
                    self.bridge_id,
                    *ger_bytes,
                    pending.note_id.clone(),
                )
                .await
            }
            DecodedWriteCall::Claim { params } => {
                crate::applied_state::reconcile_claim_handoff_with_client(
                    self.store.as_ref(),
                    client,
                    self.bridge_id,
                    params.globalIndex,
                    pending.note_id.clone(),
                )
                .await
            }
        }
    }

    async fn finalize_pending_duplicate(
        &self,
        pending: PendingDuplicate,
        outcome: crate::applied_state::ExactNoteOutcome,
    ) -> anyhow::Result<()> {
        if outcome != crate::applied_state::ExactNoteOutcome::AppliedElsewhere {
            // The exact note was consumed, or the evidence is absent/uncertain.
            // Normal projection owns exact-note finalization; otherwise stay pending.
            return Ok(());
        }
        let result = match pending.call {
            DecodedWriteCall::Ger { .. } => Ok(()),
            DecodedWriteCall::Claim { .. } => {
                Err("execution reverted: AlreadyClaimed()".to_string())
            }
        };
        let block_num = self.store.get_latest_block_number().await?;
        self.store
            .txn_commit_confirmed_duplicate(pending.tx_hash, result, block_num)
            .await
    }

    async fn reconcile_pending_duplicate(
        &self,
        client: &mut MidenClientLib,
        tx_hash: TxHash,
    ) -> anyhow::Result<()> {
        let Some(pending) = self.load_pending_duplicate(tx_hash).await? else {
            return Ok(());
        };
        let outcome = self.resolve_pending_duplicate(client, &pending).await?;
        self.finalize_pending_duplicate(pending, outcome).await
    }

    async fn reconcile_pending_duplicates(
        &self,
        client: &mut MidenClientLib,
    ) -> anyhow::Result<()> {
        let after = *self
            .pending_duplicate_cursor
            .lock()
            .expect("pending duplicate cursor mutex poisoned");
        let mut pending = self
            .store
            .pending_note_handoff_txs(after, PENDING_DUPLICATE_RECONCILE_LIMIT)
            .await?;
        if pending.is_empty() && after.is_some() {
            pending = self
                .store
                .pending_note_handoff_txs(None, PENDING_DUPLICATE_RECONCILE_LIMIT)
                .await?;
        }
        *self
            .pending_duplicate_cursor
            .lock()
            .expect("pending duplicate cursor mutex poisoned") = pending.last().copied();

        for tx_hash in pending {
            if let Err(error) = self.reconcile_pending_duplicate(client, tx_hash).await {
                tracing::warn!(
                    %tx_hash,
                    error = %format!("{error:#}"),
                    "authoritative duplicate reconciliation is uncertain; keeping receipt null"
                );
            }
        }
        Ok(())
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
    /// nullifier check; `tick` sources its consumption from the bridge transaction feed.
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
    /// unit-testable. `client` and `rpc` are only touched when a window actually
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
    /// `reconcile_notes`: unknown-ids diff, atomic batch import with the private-note
    /// fallback, and recovery for notes silently dropped after consumption.
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
            }

            // Persist every visible B2AGG identity. If miden-client dropped an already-spent
            // note or collapsed same-details siblings, fetch just the missing IDs directly.
            // This completes before the caller advances the durable reconcile cursor.
            let visible = client
                .get_input_notes(NoteFilter::List(candidates.to_vec()))
                .await
                .map_err(|e| anyhow::anyhow!("get_input_notes(List) post-import: {e}"))?;
            self.persist_b2agg_note_ids(&visible).await?;
            let visible_ids: HashSet<NoteId> =
                visible.iter().filter_map(InputNoteRecord::id).collect();
            let missing: Vec<NoteId> = candidates
                .iter()
                .filter(|id| !visible_ids.contains(id))
                .copied()
                .collect();
            self.persist_missing_b2agg_note_ids(rpc, &missing).await?;
        }
        Ok(())
    }

    /// Persist the nullifier-to-NoteId join while local records still expose metadata.
    async fn persist_b2agg_note_ids(&self, records: &[InputNoteRecord]) -> anyhow::Result<()> {
        let identities = records
            .iter()
            .filter(|record| is_b2agg_note(record.details()))
            .filter_map(|record| Some((record.nullifier()?, record.id()?)))
            .collect::<Vec<_>>();
        self.store.put_b2agg_note_ids(&identities).await
    }

    /// Fetch records hidden by miden-client's details-keyed SQLite store and persist only
    /// their identity join. The canonical body remains in the node and is fetched at use time.
    async fn persist_missing_b2agg_note_ids(
        &self,
        rpc: &dyn NodeRpcClient,
        missing: &[NoteId],
    ) -> anyhow::Result<()> {
        if missing.is_empty() {
            return Ok(());
        }
        // GrpcClient chunks this call using the node-advertised limit. The aggregate response
        // must still be exact; otherwise advancing the reconcile cursor would lose the only
        // opportunity to persist a headerless input's identity.
        let fetched = rpc
            .get_notes_by_id(missing)
            .await
            .map_err(|e| anyhow::anyhow!("get_notes_by_id({}): {e}", missing.len()))?;
        let returned: Vec<NoteId> = fetched.iter().map(FetchedNote::id).collect();
        ensure_complete_note_response(missing, &returned)?;
        let mut identities = Vec::new();
        for f in fetched {
            let id = f.id();
            let FetchedNote::Public(note, _inclusion) = f else {
                continue;
            };
            let nullifier = note.nullifier();
            let details: NoteDetails = note.into();
            if !is_b2agg_note(&details) {
                continue;
            }
            identities.push((nullifier, id));
        }
        self.store.put_b2agg_note_ids(&identities).await
    }

    /// Resolve the note bodies for a window's bridge-consumed nullifiers into ConsumedExternal
    /// records to project. `bridge_consumed_nullifiers` yields EVERY bridge consumption — real
    /// B2AGG exits AND the non-B2AGG notes the bridge routinely consumes (CLAIM, UpdateGerNote,
    /// genesis/setup notes) — so most inputs here are legitimately NOT B2AGG exits.
    ///
    /// The pinned miden-client drops transaction input headers, so B2AGG identity is normally
    /// recovered from the durable nullifier-to-NoteId join. A corrected client header is also
    /// accepted. Inputs with neither identity are normally CLAIM/GER
    /// setup notes; if one is actually a B2AGG, the pre-seal LET gate blocks the tick.
    ///
    async fn resolve_b2agg_consumptions(
        &self,
        fetcher: &dyn PublicNoteFetcher,
        consumed_refs: HashMap<Nullifier, ConsumedRef>,
        within_tx_pos: &mut HashMap<NoteId, u32>,
    ) -> anyhow::Result<Vec<(NoteId, InputNoteRecord)>> {
        let build = |details: NoteDetails, attachments: NoteAttachments, cref: &ConsumedRef| {
            let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
                nullifier_block_height: BlockNumber::from(cref.block as u32),
                consumer_account: Some(self.bridge_id),
                consumed_tx_order: Some(cref.order),
            });
            InputNoteRecord::new(details, attachments, None, state)
        };

        // miden-client 0.15 discards the headers in sync_transactions. Recover the NoteIds
        // captured before consumption so a restart does not turn every input into an
        // unresolvable nullifier.
        let nullifiers: Vec<Nullifier> = consumed_refs.keys().copied().collect();
        let durable_ids = self.store.get_b2agg_note_ids(&nullifiers).await?;

        // Resolve every identity through the canonical node body. Headerless inputs with no
        // persisted B2AGG identity are normally non-B2AGG bridge inputs; a hidden B2AGG still
        // fails closed at the independent LET cardinality gate.
        let mut refs = Vec::new();
        for (nullifier, cref) in consumed_refs {
            if let Some(note_id) = cref
                .note_id
                .or_else(|| durable_ids.get(&nullifier).copied())
            {
                refs.push((nullifier, cref, note_id));
            } else {
                metrics::counter!("synthetic_projector_b2agg_headerless_skip_total").increment(1);
                tracing::debug!(
                    nullifier = %nullifier.to_hex(),
                    block = cref.block,
                    "projector: skipping headerless unmapped bridge consumption \
                     (non-B2AGG — CLAIM/GER/genesis, covered by the store consumed feed)"
                );
            }
        }
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let fetch_ids: Vec<NoteId> = refs
            .iter()
            .map(|(_, _, note_id)| *note_id)
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
        let mut recs = Vec::new();
        for (nullifier, cref, note_id) in &refs {
            if let Some(body) = body_by_id.get(note_id) {
                within_tx_pos.insert(*note_id, cref.within_tx_pos);
                recs.push((
                    *note_id,
                    build(body.details.clone(), body.attachments.clone(), cref),
                ));
                metrics::counter!("synthetic_projector_b2agg_authoritative_fetch_total")
                    .increment(1);
                tracing::info!(
                    note_id = %note_id.to_hex(),
                    block = cref.block,
                    "projector: resolved B2AGG consumption by authoritative fetch"
                );
            } else if returned_ids.contains(note_id) {
                // Node RETURNED it but it is non-public / non-b2agg — legit CLAIM/GER. Safe skip
                // (must NOT fail-closed, or a legit consumption wedges the tip).
                tracing::debug!(
                    note_id = %note_id.to_hex(),
                    block = cref.block,
                    "authoritative fetch: node returned a non-b2agg note — safe skip (not an exit)"
                );
            } else {
                metrics::counter!("synthetic_projector_b2agg_fetch_missing_total").increment(1);
                tracing::error!(
                    nullifier = %nullifier.to_hex(),
                    note_id = %note_id.to_hex(),
                    block = cref.block,
                    "projector: identified bridge consumption was omitted by get_notes_by_id; \
                     refusing to seal"
                );
                anyhow::bail!(
                    "get_notes_by_id omitted identified bridge consumption {} at block {}",
                    note_id.to_hex(),
                    cref.block
                );
            }
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
        let mut occurrences: HashMap<([u8; 32], u64), u32> = HashMap::new();
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
            let occurrence = occurrences.entry((key, block)).or_default();
            let audit_key = (key, block, *occurrence);
            *occurrence += 1;
            {
                let resolved = self
                    .audit_resolved
                    .lock()
                    .expect("audit-resolved set poisoned");
                if resolved.contains(&audit_key) {
                    continue;
                }
            } // lock dropped before the await below
            let note_id_str = hex::encode(key);
            let tx_hash = derive_bridge_out_tx_hash(&note_id_str);
            let logs = self.store.get_logs_for_tx(&tx_hash).await?;
            outcome.audited += 1;
            if logs.iter().filter(|log| log.block_number == block).count() > audit_key.2 as usize {
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
                .insert(audit_key);
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
    /// Notes are ordered by transaction and, for B2AGG siblings, their authoritative
    /// transaction-input position. Stable commitment and NoteId tie-breakers keep replay
    /// deterministic.
    ///
    /// `client` (the live `&mut MidenClientLib`) is threaded through to
    /// `project_b2agg_note` for the Cantina #13 Layer-2 ERC-20 metadata
    /// recovery (`None` in unit tests, where the in-memory feed is supplied
    /// directly).
    /// Test helper for projecting a prebuilt consumed-note set.
    #[cfg(test)]
    async fn project_notes(
        &self,
        consumed: &[InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        client: Option<&mut MidenClientLib>,
        within_tx_pos: &HashMap<NoteId, u32>,
    ) -> anyhow::Result<usize> {
        let block_notes: Vec<(Option<NoteId>, &InputNoteRecord)> = consumed
            .iter()
            .filter(|n| n.state().consumed_block_height().map(|h| h.as_u64()) == Some(miden_block))
            .map(|n| {
                let id = n.id().or_else(|| {
                    // ConsumedExternal test fixtures intentionally omit protocol headers.
                    // Production B2AGGs arrive with their authoritative NoteId; give each
                    // ordinary fixture the equivalent deterministic identity here.
                    is_b2agg_note(n.details()).then(|| {
                        let attachments = NoteAttachments::default();
                        let metadata = NoteMetadata::new(
                            miden_protocol::note::PartialNoteMetadata::new(
                                self.bridge_id,
                                miden_protocol::note::NoteType::Public,
                            ),
                            &attachments,
                        );
                        NoteId::new(n.details_commitment(), &metadata)
                    })
                });
                (id, n)
            })
            .collect();
        self.project_block_notes(
            &block_notes,
            output_metadata,
            miden_block,
            client,
            within_tx_pos,
        )
        .await
    }

    /// Project the already-filtered notes consumed at `miden_block` into the
    /// single synthetic block `miden_block` (Miden-1:1), advancing the tip once
    /// after the block (write-before-advance), even when there are zero notes.
    async fn project_block_notes(
        &self,
        block_notes: &[(Option<NoteId>, &InputNoteRecord)],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        mut client: Option<&mut MidenClientLib>,
        within_tx_pos: &HashMap<NoteId, u32>,
    ) -> anyhow::Result<usize> {
        let mut notes: Vec<(Option<NoteId>, &InputNoteRecord)> = block_notes.to_vec();

        // Same-transaction B2AGG siblings must carry the input position from the
        // authoritative transaction header. Without it their LET order is unknowable.
        let mut ties: HashMap<Option<u32>, (usize, bool)> = HashMap::new();
        for (id, note) in &notes {
            if is_b2agg_note(note.details()) {
                let entry = ties
                    .entry(note.state().consumed_tx_order())
                    .or_insert((0, true));
                entry.0 += 1;
                entry.1 &= id.is_some_and(|id| within_tx_pos.contains_key(&id));
            }
        }
        if let Some((order, (siblings, _))) = ties
            .into_iter()
            .find(|(_, (siblings, resolved))| *siblings > 1 && !resolved)
        {
            ::metrics::counter!("bridge_within_tx_order_unresolved_total").increment(1);
            anyhow::bail!(
                "projector: {siblings} B2AGG siblings at block {miden_block}, transaction \
                 {order:?}, lack authoritative within-tx input order"
            );
        }

        // Per-block execution order, then the input position for B2AGG siblings.
        notes.sort_by(|(ida, a), (idb, b)| {
            a.state()
                .consumed_tx_order()
                .cmp(&b.state().consumed_tx_order())
                .then_with(|| {
                    let pa = ida
                        .and_then(|i| within_tx_pos.get(&i))
                        .copied()
                        .unwrap_or(0);
                    let pb = idb
                        .and_then(|i| within_tx_pos.get(&i))
                        .copied()
                        .unwrap_or(0);
                    pa.cmp(&pb)
                })
                .then_with(|| {
                    a.details_commitment()
                        .as_bytes()
                        .cmp(&b.details_commitment().as_bytes())
                })
                .then_with(|| ida.map(|i| i.as_bytes()).cmp(&idb.map(|i| i.as_bytes())))
        });

        let bridge_address = get_bridge_address();

        // Miden-1:1 numbering: synthetic block N == Miden block N. Every synthetic
        // log for this Miden block is written AT block `miden_block`; the tip is
        // advanced exactly ONCE, after the whole block (below). The projector is
        // the SOLE advancer of `latest_block_number` — nothing else may touch it.
        let block_hash = self.block_state.get_block_hash(miden_block);
        let timestamp = self.block_state.get_block_timestamp(miden_block);

        let mut logs = 0usize;
        for (note_id, note) in notes {
            if is_b2agg_note(note.details()) {
                let note_id = note_id.ok_or_else(|| {
                    anyhow::anyhow!("B2AGG projection requires an authoritative NoteId")
                })?;
                if project_b2agg_note(
                    &self.store,
                    note,
                    note_id,
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
                }
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

    /// Reconcile note visibility, then project every block through the Miden tip. No block
    /// seals until reconciliation reaches that same tip, because LET cardinality is a tip
    /// invariant.
    pub async fn tick(&self, client: &mut MidenClientLib) -> anyhow::Result<u64> {
        let tip = client
            .get_sync_height()
            .await
            .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
            .as_u64();
        let mut cursor = self.cursor.load(Ordering::Acquire);
        // Reconcile even when projection is already at the tip so note imports do not stall
        // while block production is paused.
        if let Err(e) = self.reconcile_notes(client, &self.node_rpc, tip).await {
            tracing::warn!(
                error = %format!("{e:#}"),
                "note reconciler failed (transient — will retry next tick)"
            );
        }
        // Receipt polling is store-only. Resolve confirmed duplicates here, on
        // the existing single-flight projector task, with a bounded batch.
        if let Err(error) = self.reconcile_pending_duplicates(client).await {
            tracing::warn!(
                error = %format!("{error:#}"),
                "pending duplicate reconciliation failed (transient — will retry next tick)"
            );
        }
        // Visibility barrier: a B2AGG note consumed at N was created at C <= N, so a
        // completed reconciliation through N guarantees its body was considered before N
        // seals. The authoritative transaction feed below supplies the consumptions.
        let reconcile_cursor = self.reconcile_cursor.load(Ordering::Acquire);
        let held = tip.saturating_sub(reconcile_cursor);
        ::metrics::gauge!("projector_visibility_barrier_held_blocks").set(held as f64);
        if held > 0 {
            tracing::debug!(
                tip,
                reconcile_cursor,
                held,
                "visibility barrier holding projection"
            );
            return Ok(cursor);
        }
        if reconcile_cursor > tip {
            tracing::warn!(
                tip,
                reconcile_cursor,
                "reconcile cursor is ahead of the Miden tip"
            );
        }
        if cursor >= tip {
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
        //   * CLAIM / UpdateGerNote — created and consumed by this proxy and sourced from
        //     the local store's consumed feed.
        //
        //   * B2AGG bridge-out — sourced authoritatively from the bridge transaction feed
        //     for the full projection window.
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
        // Buckets carry the note's unique NoteId alongside the record. It is mandatory for
        // authoritative B2AGG records; store-fed CLAIM/GER records in ConsumedExternal have
        // lost their metadata and therefore use `None`.
        let mut by_block: HashMap<u64, Vec<(Option<NoteId>, &InputNoteRecord)>> = HashMap::new();
        for note in &consumed {
            if is_b2agg_note(note.details()) {
                continue;
            }
            if let Some(h) = note.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push((note.id(), note));
            }
        }
        // AUTHORITATIVE B2AGG: for each bridge-consumed nullifier in the window, resolve its
        // note BODY and rebuild a ConsumedExternal record at the authoritative (block,
        // tx_order). Because miden-client 0.15 strips input headers, the reconciler durably
        // records the NoteId join before advancing its cursor; the body is then fetched from
        // the node. A nullifier with no B2AGG identity is a normal CLAIM/GER input or an
        // invisible exit; the LET gate distinguishes them fail-closed.
        // Cantina #7 (part 1): NoteId → position within the consuming tx's
        // ordered input_notes() (the on-chain LET append order), filled by
        // `resolve_b2agg_consumptions` from the same authoritative feed the records come
        // from. `project_block_notes` breaks same-tx sibling ties with it.
        let mut within_tx_pos: HashMap<NoteId, u32> = HashMap::new();
        let txs = self
            .node_rpc
            .sync_transactions(
                BlockNumber::from((cursor + 1) as u32),
                BlockNumber::from(tip as u32),
                vec![self.bridge_id],
            )
            .await
            .map_err(|e| anyhow::anyhow!("sync_transactions({}..{}): {e}", cursor + 1, tip))?;
        let consumed_refs = bridge_consumed_nullifiers(&txs, self.bridge_id)?;
        let fetcher = RpcNoteFetcher(&*self.node_rpc);
        let mut auth_b2agg = self
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut within_tx_pos)
            .await?;
        auth_b2agg.sort_by(|(id_a, note_a), (id_b, note_b)| {
            note_a
                .state()
                .consumed_block_height()
                .cmp(&note_b.state().consumed_block_height())
                .then_with(|| {
                    note_a
                        .state()
                        .consumed_tx_order()
                        .cmp(&note_b.state().consumed_tx_order())
                })
                .then_with(|| within_tx_pos.get(id_a).cmp(&within_tx_pos.get(id_b)))
                .then_with(|| id_a.as_bytes().cmp(&id_b.as_bytes()))
        });
        for (id, rec) in &auth_b2agg {
            if let Some(h) = rec.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push((Some(*id), rec));
            }
        }
        // A crash after an old commitment-keyed event but before cursor advance must replay
        // under its NoteId. The exact legacy log block prevents a future same-details note
        // from claiming historical state.
        for (id, rec) in &auth_b2agg {
            let Some(block) = rec.state().consumed_block_height().map(|h| h.as_u64()) else {
                continue;
            };
            let legacy_key = hex::encode(rec.details_commitment().as_bytes());
            let tx_hash = derive_bridge_out_tx_hash(&legacy_key);
            self.store
                .migrate_legacy_deposit_key(&legacy_key, &id.to_hex(), block, &tx_hash)
                .await?;
        }
        // Before sealing, every on-chain LET leaf must be represented by either the audited
        // legacy offset, a durable reservation, or an unreserved B2AGG in this tip window.
        let bridge_account = client
            .get_account(self.bridge_id)
            .await
            .map_err(|e| anyhow::anyhow!("LET gate: get_account({}): {e}", self.bridge_id))?
            .ok_or_else(|| {
                anyhow::anyhow!("LET gate: bridge account {} is unavailable", self.bridge_id)
            })?;
        let on_chain = miden_base_agglayer::AggLayerBridge::read_let_num_leaves(&bridge_account);
        let accounted = self.store.get_accounted_deposit_count().await?;
        let note_keys: Vec<String> = auth_b2agg.iter().map(|(id, _)| id.to_hex()).collect();
        let existing = self.store.get_deposit_indices(&note_keys).await?;
        let first_missing = note_keys
            .iter()
            .position(|key| !existing.contains_key(key))
            .unwrap_or(note_keys.len());
        if note_keys[first_missing..]
            .iter()
            .any(|key| existing.contains_key(key))
        {
            anyhow::bail!("LET reservations are not an execution-order prefix");
        }
        let prefix_start = accounted
            .checked_sub(first_missing as u64)
            .ok_or_else(|| anyhow::anyhow!("LET reservation accounting underflow"))?;
        for (offset, key) in note_keys[..first_missing].iter().enumerate() {
            let expected_index = u32::try_from(prefix_start + offset as u64)?;
            if existing.get(key) != Some(&expected_index) {
                anyhow::bail!(
                    "LET reservation order mismatch for {key}: stored={:?}, expected={expected_index}",
                    existing.get(key)
                );
            }
        }
        let unreserved = u64::try_from(note_keys.len() - existing.len())?;
        let expected = accounted
            .checked_add(unreserved)
            .ok_or_else(|| anyhow::anyhow!("LET gate accounting overflow"))?;
        if on_chain != expected {
            let (kind, gap) = if on_chain > expected {
                ("invisible_gap", on_chain - expected)
            } else {
                ("local_ahead", expected - on_chain)
            };
            ::metrics::counter!("bridge_let_assignment_gate_halted_total", "kind" => kind)
                .increment(1);
            anyhow::bail!(
                "LET cardinality gate blocked ({kind}, gap {gap}): on-chain={on_chain}, \
                 expected={expected}; see docs/operations/let-cardinality-gate.md"
            );
        }
        // EMITTED-FRONTIER GATE (complements the LET cardinality gate above).
        // The cardinality gate enforces `accounted == on_chain let_num_leaves` — but a
        // leaf can be RESERVED (counted) yet never EMITTED (quarantined / deferred /
        // unrecoverable-metadata / self-target). That leaf occupies its LET slot with NO
        // BridgeEvent, leaving a permanent GAP in the getLogs `depositCount` sequence.
        // aggkit's L2 bridgesync requires contiguous deposit indices, so it HALTS
        // ("state is inconsistent") on the gap and every later Miden certificate wedges.
        // Fail-closed: refuse to seal past a reserved-but-unemitted leaf so aggkit sees a
        // contiguous prefix and WAITS instead of wedging. The LER simply does not advance
        // for the withheld leaf. Recovery is operator-driven: fix the leaf's metadata
        // (registry backfill / a full DB drop + `--restore` rebuild from on-chain) so the
        // leaf emits its real event.
        if let Some((idx, note)) = self.store.first_unemitted_reservation().await? {
            ::metrics::counter!("bridge_unemitted_reservation_halt_total").increment(1);
            anyhow::bail!(
                "projector halted (fail-closed): note {note} (LET index {idx}) is reserved \
                 but its BridgeEvent was never emitted (unrecoverable metadata / quarantined \
                 leaf). Sealing past it would leave a getLogs gap that halts aggkit bridgesync. \
                 Fix the leaf's metadata (registry backfill, or back up + drop the DB and \
                 re-run `--restore` to rebuild from on-chain), then restart."
            );
        }
        let no_notes: Vec<(Option<NoteId>, &InputNoteRecord)> = Vec::new();
        while cursor < tip {
            let next = cursor + 1;
            let bucket = by_block.get(&next).unwrap_or(&no_notes);
            self.project_block_notes(bucket, &output_metadata, next, Some(client), &within_tx_pos)
                .await?;
            // Advance the cursor only after the block is fully projected, so a
            // crash mid-block re-projects (idempotently) rather than skipping.
            // Persist BEFORE updating the in-memory cursor so the durable cursor
            // never runs ahead of fully-projected state.
            self.store.set_projector_cursor(next).await?;
            self.cursor.store(next, Ordering::Release);
            cursor = next;
        }
        // COMPLETENESS AUDITOR (detection only, every AUDIT_EVERY_N_TICKS ticks): diff the
        // store's consumed-B2AGG view against the synthetic log store for comfortably-sealed
        // blocks. Reuses this tick's already-fetched `consumed` feed — zero extra queries.
        // Non-fatal by construction: an audit failure warns and retries next cycle, it never
        // blocks projection.
        if self
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
        let synthetic_tip = self.store.get_latest_block_number().await?;
        tracing::info!(
            miden_tip = tip,
            projector_cursor = cursor,
            synthetic_tip,
            "synthetic projector tick: caught up to the Miden tip"
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
    /// NoteId retained in the transaction input header, when exposed by the client. The pinned
    /// miden-client 0.15 decoder currently strips it, so production normally uses the durable
    /// nullifier-to-NoteId join instead.
    pub note_id: Option<NoteId>,
    /// Cantina #7 (part 1): the note's position within its consuming transaction's ORDERED
    /// `input_notes()` — the on-chain LET append order. When one bridge tx consumes several
    /// B2AGG notes they tie on `(block, order)`, and without this the sort fell through to
    /// details-commitment (hash) order — arbitrary relative to the on-chain append order, so
    /// `deposit_count` could be misnumbered (wrong globalIndex in certs/L1 exit tree, sealed
    /// forever by getLogs immutability). Authoritative: read straight from the tx header.
    pub within_tx_pos: u32,
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
        let mut bodies = Vec::new();
        let mut returned_ids = HashSet::new();
        let fetched = self
            .0
            .get_notes_by_id(ids)
            .await
            .map_err(|e| anyhow::anyhow!("get_notes_by_id({}): {e}", ids.len()))?;
        for f in fetched {
            let id = f.id();
            returned_ids.insert(id);
            let FetchedNote::Public(note, _inclusion) = f else {
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
/// order, and any NoteId retained by the client decoder).
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
) -> anyhow::Result<HashMap<Nullifier, ConsumedRef>> {
    let mut out = HashMap::new();
    for (block, order, tx) in ordered_account_transactions(txs, bridge_id)? {
        for (pos, input) in tx.transaction_header.input_notes().iter().enumerate() {
            // Future/fixed clients may retain this protocol header. v0.15 strips it, and the
            // projector falls back to the durable nullifier-to-NoteId join.
            let note_id = input.header().map(|h| h.id());
            out.insert(
                input.nullifier(),
                ConsumedRef {
                    block,
                    order,
                    note_id,
                    // Cantina #7: the header's input order IS the on-chain LET append order.
                    within_tx_pos: pos as u32,
                },
            );
        }
    }
    Ok(out)
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
    use miden_client::store::input_note_states::{ConsumedExternalNoteState, ExpectedNoteState};
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
    /// A B2AGG note carrying an asset from a CALLER-CHOSEN faucet — for tests that need one
    /// registered (emits) and one UNregistered (quarantines as UnknownFaucet) leaf, both
    /// still reserving their LET deposit index.
    fn b2agg_note_faucet(
        block: u32,
        tx_order: Option<u32>,
        faucet_id: AccountId,
        amount: u64,
    ) -> InputNoteRecord {
        let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let asset: Asset = FungibleAsset::new(faucet_id, amount).unwrap().into();
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
            crate::miden_client::effective_node_url(None),
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

    fn bridge_deposit_count(log: &SyntheticLog) -> u32 {
        let data = hex::decode(log.data.trim_start_matches("0x")).unwrap();
        u32::from_be_bytes(data[252..256].try_into().unwrap())
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
            .project_notes(&notes, &output_metadata, 5, None, &HashMap::new())
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
            .project_notes(&notes, &output_metadata, 7, None, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(first, 3);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 7);

        let second = projector
            .project_notes(&notes, &output_metadata, 7, None, &HashMap::new())
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
                .project_notes(&notes, &output_metadata, 3, None, &HashMap::new())
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 3);
        // Project Miden block 8: only the CLAIM note belongs here → synthetic 8.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 8, None, &HashMap::new())
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
            .project_notes(&same_block, &output_metadata, 100, None, &HashMap::new())
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
            .project_notes(&later_block, &output_metadata, 250, None, &HashMap::new())
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
                .project_notes(&notes, &output_metadata, 9, None, &HashMap::new())
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
                .project_notes(&notes, &output_metadata, 5, None, &HashMap::new())
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
            .project_notes(&notes, &HashMap::new(), 4, None, &HashMap::new())
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
            .project_notes(&notes, &HashMap::from([ger_meta]), 4, None, &HashMap::new())
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
            commitments: Vec<InputNoteCommitment>,
            initial: Word,
            final_state: Word,
        ) -> TransactionRecord {
            TransactionRecord {
                block_num: BlockNumber::from(block),
                transaction_header: TransactionHeader::new(
                    account,
                    initial,
                    final_state,
                    InputNotes::new(commitments).unwrap(),
                    vec![],
                    FungibleAsset::new(aid(FAUCET), 0).unwrap(),
                ),
                output_notes: vec![],
                erased_output_notes: vec![],
            }
        }

        let (a, b, c) = (nf(1), nf(2), nf(3));
        // The pinned miden-client decoder produces headerless commitments even when the
        // network transaction used an unauthenticated note.
        let auth = |n: Nullifier| InputNoteCommitment::from(n);
        // Preserve support for a corrected decoder that retains the protocol headers. Two
        // same-details siblings prove that input position, not a hash tie-break, is retained.
        let details_commitment = b2agg_note(5, Some(0)).details_commitment();
        let metadata_a = NoteMetadata::new(
            PartialNoteMetadata::new(aid(BRIDGE), NoteType::Public),
            &NoteAttachments::default(),
        );
        let metadata_b = NoteMetadata::new(
            PartialNoteMetadata::new(aid(SERVICE), NoteType::Public),
            &NoteAttachments::default(),
        );
        let header_a = NoteHeader::new(details_commitment, metadata_a);
        let header_b = NoteHeader::new(details_commitment, metadata_b);
        let expected_a = header_a.id();
        let expected_b = header_b.id();
        let unauth_a = InputNoteCommitment::from_parts_unchecked(nf(4), Some(header_a));
        let unauth_b = InputNoteCommitment::from_parts_unchecked(nf(5), Some(header_b));
        let state = |byte: u64| Word::new([Felt::new(byte).unwrap(); 4]);

        // Deliberately reverse the bridge transactions in the RPC response. Their state
        // commitments, not response/transaction-id order, establish execution order.
        let txs = vec![
            tx(
                aid(BRIDGE),
                9,
                vec![unauth_a, unauth_b],
                state(12),
                state(13),
            ),
            tx(aid(SERVICE), 9, vec![auth(b)], state(20), state(21)),
            tx(aid(BRIDGE), 9, vec![auth(a)], state(10), state(11)),
            tx(aid(BRIDGE), 9, vec![auth(c)], state(11), state(12)),
        ];
        let map = bridge_consumed_nullifiers(&txs, aid(BRIDGE)).unwrap();
        assert_eq!(
            map.get(&a),
            Some(&ConsumedRef {
                block: 9,
                order: 0,
                note_id: None,
                within_tx_pos: 0
            }),
            "headerless input is attributed by nullifier"
        );
        assert_eq!(
            map.get(&c),
            Some(&ConsumedRef {
                block: 9,
                order: 1,
                note_id: None,
                within_tx_pos: 0
            }),
            "per-block bridge-tx order increments"
        );
        assert_eq!(
            map.get(&nf(4)),
            Some(&ConsumedRef {
                block: 9,
                order: 2,
                note_id: Some(expected_a),
                within_tx_pos: 0
            }),
            "retained header exposes the first sibling identity and position"
        );
        assert_eq!(
            map.get(&nf(5)),
            Some(&ConsumedRef {
                block: 9,
                order: 2,
                note_id: Some(expected_b),
                within_tx_pos: 1
            }),
            "retained header exposes the second same-details sibling and position"
        );
        assert!(
            !map.contains_key(&b),
            "non-bridge consumption must be gated out (MA#3 fail-closed)"
        );

        let disconnected = vec![
            tx(aid(BRIDGE), 9, vec![auth(a)], state(30), state(31)),
            tx(aid(BRIDGE), 9, vec![auth(c)], state(40), state(41)),
        ];
        assert!(
            bridge_consumed_nullifiers(&disconnected, aid(BRIDGE)).is_err(),
            "ambiguous same-block execution order must fail closed"
        );
    }

    /// Restart regression: miden-client 0.15 strips input headers from sync_transactions and
    /// its SQLite key can collapse same-details notes. The durable nullifier-to-NoteId join
    /// must therefore recover both distinct notes after a process restart.
    #[tokio::test]
    async fn restart_recovers_headerless_same_details_siblings() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let first_process = test_projector(&store, &block_state).await;
        let details = b2agg_note(7, Some(0)).details().clone();
        let attachments = NoteAttachments::default();
        let metadata_a = NoteMetadata::new(
            PartialNoteMetadata::new(aid(SERVICE), NoteType::Public),
            &attachments,
        );
        let metadata_b = NoteMetadata::new(
            PartialNoteMetadata::new(aid(GER_MANAGER), NoteType::Public),
            &attachments,
        );
        let record = |metadata: NoteMetadata| {
            InputNoteRecord::new(
                details.clone(),
                attachments.clone(),
                None,
                ExpectedNoteState {
                    metadata: Some(metadata),
                    after_block_num: BlockNumber::from(0u32),
                    tag: Some(metadata.tag()),
                }
                .into(),
            )
        };
        let records = vec![record(metadata_a), record(metadata_b)];
        let id_a = records[0].id().unwrap();
        let id_b = records[1].id().unwrap();
        let nf_a = records[0].nullifier().unwrap();
        let nf_b = records[1].nullifier().unwrap();
        assert_eq!(
            records[0].details_commitment(),
            records[1].details_commitment()
        );

        first_process
            .persist_b2agg_note_ids(&records)
            .await
            .unwrap();
        drop(first_process);

        let restarted = test_projector(&store, &block_state).await;
        let refs = HashMap::from([
            (
                nf_a,
                ConsumedRef {
                    block: 8,
                    order: 0,
                    note_id: None,
                    within_tx_pos: 0,
                },
            ),
            (
                nf_b,
                ConsumedRef {
                    block: 8,
                    order: 0,
                    note_id: None,
                    within_tx_pos: 1,
                },
            ),
        ]);
        let fetcher = MockFetcher {
            bodies: vec![
                FetchedBody {
                    id: id_a,
                    details: details.clone(),
                    attachments: attachments.clone(),
                },
                FetchedBody {
                    id: id_b,
                    details,
                    attachments,
                },
            ],
            ..Default::default()
        };
        let mut positions = HashMap::new();
        let recovered = restarted
            .resolve_b2agg_consumptions(&fetcher, refs, &mut positions)
            .await
            .unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(positions.get(&id_a), Some(&0));
        assert_eq!(positions.get(&id_b), Some(&1));
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

    /// Distinct synthetic NoteIds for id-keyed ordering/dedup tests (the ordering map is
    /// keyed by the unique NoteId, not the shareable details commitment).
    fn test_note_id(byte: u64) -> NoteId {
        NoteId::from_raw(Word::new([Felt::new(byte).unwrap(); 4]))
    }

    fn nullifier(byte: u64) -> super::Nullifier {
        super::Nullifier::from_raw(Word::new([Felt::new(byte).unwrap(); 4]))
    }

    /// The regression for note `0xacfee0cb…` (N=30 loadtest, exactly 1 missing BridgeEvent): a
    /// bridge consumption created and consumed under load must still be resolved and emitted
    /// at its exact block. When a client retains the protocol header,
    /// `resolve_b2agg_consumptions` can fetch the body directly by NoteId.
    #[tokio::test]
    async fn uncached_consumption_with_header_resolves_via_authoritative_fetch() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // A real B2AGG body the node would return, and the retained input NoteId.
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
                within_tx_pos: 0,
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
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut HashMap::new())
            .await
            .expect("uncached consumption with a NoteId must resolve, not error");

        assert_eq!(
            recs.len(),
            1,
            "the uncached consumption must resolve via authoritative fetch"
        );
        assert_eq!(
            recs[0]
                .1
                .state()
                .consumed_block_height()
                .map(|h| h.as_u64()),
            Some(544),
            "resolved at the exact consumption block"
        );
        assert_eq!(
            recs[0].0, note_id,
            "the resolved record carries its unique NoteId (the ordering/dedup identity)"
        );
        assert!(
            is_b2agg_note(recs[0].1.details()),
            "the fetched body is the B2AGG note"
        );
    }

    /// The block-13 un-wedge regression: a headerless, unmapped bridge input is normally a
    /// non-B2AGG CLAIM/GER/genesis note covered by the store feed. It must be a safe skip; a
    /// hidden B2AGG is still caught by the independent LET gate.
    #[tokio::test]
    async fn headerless_unmapped_consumption_is_a_safe_skip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Block-13-shaped: a headerless bridge consumption with no persisted B2AGG identity.
        let nf = nullifier(0x64);
        let consumed_refs = HashMap::from([(
            nf,
            ConsumedRef {
                block: 13,
                order: 0,
                note_id: None,
                within_tx_pos: 0,
            },
        )]);
        let fetcher = MockFetcher::default();
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut HashMap::new())
            .await
            .expect("headerless unmapped consumption must be a safe skip");
        assert!(
            recs.is_empty(),
            "an authenticated non-B2AGG consumption emits no record (store feed covers it)"
        );
    }

    /// A requested identity missing from the node response must fail the tick even if an
    /// earlier attempt already reserved its LET index. Cardinality alone cannot detect a
    /// missing body once that reservation is counted.
    #[tokio::test]
    async fn identified_fetch_omission_fails_even_when_already_reserved() {
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
            within_tx_pos: 0,
        };
        store
            .reserve_deposit_index(&note_id.to_hex())
            .await
            .unwrap();
        let fetcher = MockFetcher::default();
        let err = projector
            .resolve_b2agg_consumptions(&fetcher, HashMap::from([(nf, cref)]), &mut HashMap::new())
            .await
            .expect_err("an identified body omission must fail before sealing");
        assert!(format!("{err:#}").contains("omitted identified bridge consumption"));
    }

    /// Case (A) — the complement: a bridge consumption the node DID return, but as a non-b2agg
    /// note (the bridge legitimately consumes CLAIM/GER). This is provably NOT a public exit, so
    /// it is a safe skip — no record and no error. Skipping here (not fail-closing) is
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
                within_tx_pos: 0,
            },
        )]);
        // Node RETURNS the id (in returned_ids) but with no public b2agg body.
        let fetcher = MockFetcher {
            bodies: vec![],
            also_returned: vec![note_id],
        };
        let recs = projector
            .resolve_b2agg_consumptions(&fetcher, consumed_refs, &mut HashMap::new())
            .await
            .expect("a returned non-b2agg note must be a safe skip, not an error");
        assert!(
            recs.is_empty(),
            "a non-b2agg bridge consumption emits no B2AGG record"
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
                .project_notes(
                    std::slice::from_ref(&emitted),
                    &HashMap::new(),
                    5,
                    None,
                    &HashMap::new(),
                )
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

    fn duplicate_test_call(global_index: alloy::primitives::U256) -> DecodedWriteCall {
        use alloy::primitives::{Address, FixedBytes};
        DecodedWriteCall::Claim {
            params: Box::new(crate::claim::claimAssetCall {
                smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
                smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
                globalIndex: global_index,
                mainnetExitRoot: FixedBytes::ZERO,
                rollupExitRoot: FixedBytes::ZERO,
                originNetwork: 0,
                originTokenAddress: Address::ZERO,
                destinationNetwork: 1,
                destinationAddress: Address::ZERO,
                amount: alloy::primitives::U256::from(1u8),
                metadata: Default::default(),
            }),
        }
    }

    fn duplicate_test_envelope(
        call: &DecodedWriteCall,
        tx_hash: TxHash,
    ) -> alloy::consensus::TxEnvelope {
        use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
        use alloy::primitives::{FixedBytes, Signature};
        use alloy_core::sol_types::SolCall;

        let input = match call {
            DecodedWriteCall::Claim { params } => params.abi_encode(),
            DecodedWriteCall::Ger { ger_bytes } => crate::ger::insertGlobalExitRootCall {
                root: FixedBytes::from(*ger_bytes),
            }
            .abi_encode(),
        };
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                input: input.into(),
                ..Default::default()
            },
            Signature::test_signature(),
            tx_hash,
        ))
    }

    async fn begin_duplicate_test_tx(
        store: &StdArc<dyn Store>,
        tx_hash: TxHash,
        call: &DecodedWriteCall,
    ) {
        store
            .txn_begin(
                tx_hash,
                crate::store::TxnEntry {
                    id: None,
                    envelope: duplicate_test_envelope(call, tx_hash),
                    signer: alloy::primitives::Address::from([0x77u8; 20]),
                    expires_at: Some(100),
                    logs: vec![],
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn background_reconciliation_finalizes_duplicate_claim_and_ger() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        store.set_latest_block_number(42).await.unwrap();
        let projector = test_projector(&store, &StdArc::new(BlockState::new())).await;

        let claim_hash = TxHash::from([0x91u8; 32]);
        let claim = duplicate_test_call(alloy::primitives::U256::from(9u8));
        begin_duplicate_test_tx(&store, claim_hash, &claim).await;
        projector
            .finalize_pending_duplicate(
                PendingDuplicate {
                    tx_hash: claim_hash,
                    call: claim,
                    note_id: "claim-note".into(),
                },
                crate::applied_state::ExactNoteOutcome::AppliedElsewhere,
            )
            .await
            .unwrap();
        let (claim_result, claim_block) = store
            .txn_receipt(claim_hash)
            .await
            .unwrap()
            .expect("duplicate claim receipt");
        assert!(claim_result.is_err());
        assert_eq!(claim_block, 42);

        let ger_hash = TxHash::from([0x92u8; 32]);
        let ger = DecodedWriteCall::Ger {
            ger_bytes: [0x92u8; 32],
        };
        begin_duplicate_test_tx(&store, ger_hash, &ger).await;
        projector
            .finalize_pending_duplicate(
                PendingDuplicate {
                    tx_hash: ger_hash,
                    call: ger,
                    note_id: "ger-note".into(),
                },
                crate::applied_state::ExactNoteOutcome::AppliedElsewhere,
            )
            .await
            .unwrap();
        let (ger_result, ger_block) = store
            .txn_receipt(ger_hash)
            .await
            .unwrap()
            .expect("duplicate GER receipt");
        assert!(ger_result.is_ok());
        assert_eq!(ger_block, 42);
    }

    #[tokio::test]
    async fn exact_or_uncertain_note_remains_pending_for_normal_projection() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let projector = test_projector(&store, &StdArc::new(BlockState::new())).await;
        let tx_hash = TxHash::from([0x93u8; 32]);
        let call = DecodedWriteCall::Ger {
            ger_bytes: [0x93u8; 32],
        };
        begin_duplicate_test_tx(&store, tx_hash, &call).await;

        for outcome in [
            crate::applied_state::ExactNoteOutcome::AppliedByExactNote,
            crate::applied_state::ExactNoteOutcome::NotApplied,
            crate::applied_state::ExactNoteOutcome::Uncertain,
        ] {
            projector
                .finalize_pending_duplicate(
                    PendingDuplicate {
                        tx_hash,
                        call: call.clone(),
                        note_id: "exact-or-unknown".into(),
                    },
                    outcome,
                )
                .await
                .unwrap();
            assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn durable_claim_event_short_circuits_miden_and_missing_ger() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        store.set_latest_block_number(55).await.unwrap();
        let projector = test_projector(&store, &StdArc::new(BlockState::new())).await;
        let global_index = alloy::primitives::U256::from(0x5514u64);
        let tx_hash = TxHash::from([0x94u8; 32]);
        let call = duplicate_test_call(global_index);
        begin_duplicate_test_tx(&store, tx_hash, &call).await;
        store
            .prepare_note_handoff(
                &format!("{tx_hash:#x}"),
                "pending-claim-commitment",
                "not-present-in-offline-miden",
                100,
            )
            .await
            .unwrap();
        store.try_claim(global_index).await.unwrap();
        store
            .commit_manual_claim_event_atomic(
                "other-claim-note".into(),
                "0x00000000000000000000000000000000000000aa",
                54,
                [0u8; 32],
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                global_index.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0u8; 20],
                1,
            )
            .await
            .unwrap();

        let combined_zero_ger = crate::ger::combined_ger(&[0u8; 32], &[0u8; 32]);
        assert!(!store.is_ger_injected(&combined_zero_ger).await.unwrap());
        let mut unavailable_miden = crate::test_helpers::offline_miden_client_lib().await;
        projector
            .reconcile_pending_duplicate(&mut unavailable_miden, tx_hash)
            .await
            .expect("durable ClaimEvent must avoid unavailable Miden state");

        let (result, block) = store
            .txn_receipt(tx_hash)
            .await
            .unwrap()
            .expect("known duplicate claim is terminal");
        assert!(result.is_err());
        assert_eq!(block, 55);
        assert!(
            store
                .pending_note_handoff_txs(None, PENDING_DUPLICATE_RECONCILE_LIMIT)
                .await
                .unwrap()
                .is_empty(),
            "terminal receipts must not be reconciled again"
        );
    }

    #[tokio::test]
    async fn pending_duplicate_query_is_bounded_and_cursor_ordered() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let call = DecodedWriteCall::Ger {
            ger_bytes: [0xa5u8; 32],
        };
        let hashes = [
            TxHash::from([1u8; 32]),
            TxHash::from([2u8; 32]),
            TxHash::from([3u8; 32]),
        ];
        for (index, tx_hash) in hashes.into_iter().enumerate() {
            begin_duplicate_test_tx(&store, tx_hash, &call).await;
            store
                .prepare_note_handoff(
                    &format!("{tx_hash:#x}"),
                    &format!("commitment-{index}"),
                    &format!("note-{index}"),
                    100,
                )
                .await
                .unwrap();
        }

        let first = store.pending_note_handoff_txs(None, 2).await.unwrap();
        assert_eq!(first, hashes[..2]);
        let second = store
            .pending_note_handoff_txs(first.last().copied(), 2)
            .await
            .unwrap();
        assert_eq!(second, hashes[2..]);
    }

    // ── Cantina #7 (part 1): within-tx sibling ordering ──────────────────────

    /// Regression (RED on pre-fix main): several B2AGG notes consumed by ONE bridge tx tie
    /// on `consumed_tx_order`, and the old `(tx_order, details_commitment)` key fell
    /// through to HASH order — arbitrary relative to the on-chain LET append order
    /// (deposit_count misnumbering = wrong globalIndex, sealed by getLogs immutability).
    /// The fix orders same-tx siblings by their position in the tx's `input_notes()`.
    /// The pos map is constructed as the REVERSE of commitment order, so this test FAILS
    /// on the old key by construction.
    #[tokio::test]
    async fn same_tx_siblings_emit_in_input_notes_order_not_commitment_order() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Three siblings, SAME block + SAME consuming tx (tx_order 0).
        let notes = [
            b2agg_note_with_amount(7, Some(0), 11),
            b2agg_note_with_amount(7, Some(0), 22),
            b2agg_note_with_amount(7, Some(0), 33),
        ];
        // Commitment (hash) order of the three — what the OLD key would emit in.
        let mut by_commitment: Vec<[u8; 32]> = notes
            .iter()
            .map(|n| n.details_commitment().as_bytes())
            .collect();
        by_commitment.sort();
        // Authoritative input_notes() order: the REVERSE of hash order, so the two orders
        // provably disagree for 3 distinct commitments. Each note gets a UNIQUE NoteId —
        // the map key — with its position in the (reversed) input order.
        let input_order: Vec<[u8; 32]> = by_commitment.iter().rev().copied().collect();
        let mut within_tx_pos: HashMap<NoteId, u32> = HashMap::new();
        let pairs: Vec<(Option<NoteId>, &InputNoteRecord)> = notes
            .iter()
            .map(|n| {
                let pos = input_order
                    .iter()
                    .position(|c| *c == n.details_commitment().as_bytes())
                    .unwrap() as u32;
                let id = test_note_id(100 + pos as u64);
                within_tx_pos.insert(id, pos);
                (Some(id), n)
            })
            .collect();

        let written = projector
            .project_block_notes(&pairs, &HashMap::new(), 7, None, &within_tx_pos)
            .await
            .unwrap();
        assert_eq!(written, 3);

        let logs = logs_in_range(&store, 0, 7).await;
        assert_eq!(logs.len(), 3);
        let expected: Vec<String> = input_order
            .iter()
            .map(|c| crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(c)))
            .collect();
        let got: Vec<String> = logs.iter().map(|l| l.transaction_hash.clone()).collect();
        assert_eq!(
            got, expected,
            "same-tx siblings must emit in input_notes() (LET append) order — NOT \
             commitment order (the reverse here); deposit_count follows emission order"
        );
        // Explicit: the old key's order provably disagrees.
        let old_key_order: Vec<String> = by_commitment
            .iter()
            .map(|c| crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(c)))
            .collect();
        assert_ne!(
            got, old_key_order,
            "fixture must distinguish the two orders"
        );
    }

    /// Full key across mixed blocks: multiple txs × multiple same-tx siblings each —
    /// the emission order is exactly (block, tx_order, within_tx_pos, commitment),
    /// with each tx's siblings pos-reversed from hash order to keep the tie-break honest.
    #[tokio::test]
    async fn mixed_txs_and_siblings_follow_the_full_projection_key() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // tx_order 0 consumes {41, 42}; tx_order 1 consumes {43, 44} — all at block 9.
        let tx0 = vec![
            b2agg_note_with_amount(9, Some(0), 41),
            b2agg_note_with_amount(9, Some(0), 42),
        ];
        let tx1 = vec![
            b2agg_note_with_amount(9, Some(1), 43),
            b2agg_note_with_amount(9, Some(1), 44),
        ];
        // Per tx: input order = reverse hash order; each note gets a unique NoteId keyed
        // to its position — commitment→id assignment recorded for the pair construction.
        let mut within_tx_pos: HashMap<NoteId, u32> = HashMap::new();
        let mut id_of: HashMap<[u8; 32], NoteId> = HashMap::new();
        let mut expected: Vec<String> = Vec::new();
        let mut next_id = 200u64;
        for tx in [&tx0, &tx1] {
            let mut cs: Vec<[u8; 32]> = tx
                .iter()
                .map(|n| n.details_commitment().as_bytes())
                .collect();
            cs.sort();
            cs.reverse(); // input order = reverse hash order within each tx
            for (pos, c) in cs.iter().enumerate() {
                let id = test_note_id(next_id);
                next_id += 1;
                within_tx_pos.insert(id, pos as u32);
                id_of.insert(*c, id);
                expected.push(crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
                    c,
                )));
            }
        }
        // Shuffled arrival order (interleaved txs, reversed).
        let notes = [
            tx1[0].clone(),
            tx0[1].clone(),
            tx1[1].clone(),
            tx0[0].clone(),
        ];
        let pairs: Vec<(Option<NoteId>, &InputNoteRecord)> = notes
            .iter()
            .map(|n| (id_of.get(&n.details_commitment().as_bytes()).copied(), n))
            .collect();
        let written = projector
            .project_block_notes(&pairs, &HashMap::new(), 9, None, &within_tx_pos)
            .await
            .unwrap();
        assert_eq!(written, 4);
        let got: Vec<String> = logs_in_range(&store, 0, 9)
            .await
            .iter()
            .map(|l| l.transaction_hash.clone())
            .collect();
        assert_eq!(
            got, expected,
            "emission must follow (block, tx_order, within_tx_pos, commitment) exactly"
        );
    }

    /// FAIL-CLOSED: a same-tx B2AGG tie whose within-tx order is NOT resolvable must HALT
    /// the projection tick (Err, nothing sealed) — skip-and-continue IS the corruption,
    /// because every subsequent deposit_count depends on the siblings' relative order.
    #[tokio::test]
    async fn unresolved_same_tx_tie_halts_projection_fail_closed() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let notes = vec![
            b2agg_note_with_amount(4, Some(0), 51),
            b2agg_note_with_amount(4, Some(0), 52),
        ];
        // No within-tx positions available for the tie.
        let err = projector
            .project_notes(&notes, &HashMap::new(), 4, None, &HashMap::new())
            .await
            .expect_err("an unresolvable same-tx tie must halt the tick");
        assert!(
            format!("{err:#}").contains("within-tx"),
            "halt must name the within-tx ordering: {err:#}"
        );
        assert!(
            logs_in_range(&store, 0, 4).await.is_empty(),
            "fail-closed: NOTHING may be sealed from the halted block"
        );
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            0,
            "the tip must not advance past the halted block"
        );
    }

    /// A skipped LET leaf still consumes its index; the following valid event must encode 1.
    #[tokio::test]
    async fn skipped_let_leaf_reserves_its_deposit_index() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Leaf 0: a DISTINCT, unregistered faucet → quarantines as UnknownFaucet
        // (reserves index 0, emits nothing). Leaf 1: the registered FAUCET → emits.
        let unregistered_faucet = aid("0xaa0000000000bc110000bc000000de");
        let quarantined = b2agg_note_faucet(5, Some(0), unregistered_faucet, 71);
        register_faucet(&store).await;
        let valid = b2agg_note_with_amount(5, Some(1), 72);

        let written = projector
            .project_notes(
                &[quarantined.clone(), valid.clone()],
                &HashMap::new(),
                5,
                None,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(written, 1, "only the valid leaf emits");
        assert_eq!(
            store.get_deposit_count().await.unwrap(),
            2,
            "both leaves indexed"
        );

        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(bridge_deposit_count(&logs[0]), 1);

        // Retry/restart: a fresh projector over the SAME store re-projects idempotently —
        // indices are stable, nothing double-allocates, no second event.
        let projector2 = test_projector(&store, &block_state).await;
        let rewritten = projector2
            .project_notes(
                &[quarantined.clone(), valid.clone()],
                &HashMap::new(),
                5,
                None,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(rewritten, 0, "replay emits nothing new");
        assert_eq!(
            store.get_deposit_count().await.unwrap(),
            2,
            "no re-allocation"
        );
    }

    /// The audited legacy offset is folded into every new reservation.
    #[tokio::test]
    async fn nonzero_baseline_folds_into_deposit_index() {
        let memory = StdArc::new(InMemoryStore::new());
        memory.set_let_gate_baseline_for_test(97);
        let store: StdArc<dyn Store> = memory;
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let leaf = b2agg_note_with_amount(5, Some(0), 71);
        let written = projector
            .project_notes(
                std::slice::from_ref(&leaf),
                &HashMap::new(),
                5,
                None,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(written, 1, "the leaf emits");
        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(bridge_deposit_count(&logs[0]), 97);

        // A SECOND leaf → baseline(97) + raw(1) = 98.
        let leaf2 = b2agg_note_with_amount(6, Some(0), 72);
        projector
            .project_notes(
                std::slice::from_ref(&leaf2),
                &HashMap::new(),
                6,
                None,
                &HashMap::new(),
            )
            .await
            .unwrap();
        let logs = logs_in_range(&store, 0, 6).await;
        assert_eq!(bridge_deposit_count(&logs[1]), 98);
    }

    /// REVIEW BLOCKER 5 — unique-identity collision: two DISTINCT notes sharing one
    /// details commitment are two real LET leaves. Commitment-keyed dedup collapsed the
    /// second (one event for two leaves, gate 'aligned' while an exit was silently
    /// dropped); NoteId-keyed dedup emits BOTH with distinct deposit counts.
    #[tokio::test]
    async fn same_commitment_distinct_notes_both_emit() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Two records with IDENTICAL details (same commitment) — distinct on-chain notes
        // (differing metadata), distinct unique ids, consumed by the same bridge tx.
        let note_a = b2agg_note_with_amount(6, Some(0), 55);
        let note_b = b2agg_note_with_amount(6, Some(0), 55);
        assert_eq!(
            note_a.details_commitment().as_bytes(),
            note_b.details_commitment().as_bytes(),
            "fixture: the two notes share a details commitment"
        );
        let id_a = test_note_id(401);
        let id_b = test_note_id(402);
        let within_tx_pos: HashMap<NoteId, u32> = [(id_a, 0), (id_b, 1)].into_iter().collect();
        let pairs: Vec<(Option<NoteId>, &InputNoteRecord)> =
            vec![(Some(id_a), &note_a), (Some(id_b), &note_b)];

        let written = projector
            .project_block_notes(&pairs, &HashMap::new(), 6, None, &within_tx_pos)
            .await
            .unwrap();
        assert_eq!(
            written, 2,
            "BOTH leaves must emit — dedup is by unique NoteId"
        );
        assert_eq!(store.get_deposit_count().await.unwrap(), 2);
        let logs = logs_in_range(&store, 0, 6).await;
        assert_eq!(
            logs.len(),
            2,
            "two events (shared tx hash, distinct deposit counts)"
        );
        assert_eq!(
            logs.iter().map(bridge_deposit_count).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
