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
use crate::bridge_out::is_b2agg_note;
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

/// Blocks swept per tick by the note-visibility reconciler. Bounds the
/// per-tick RPC work; catch-up from genesis proceeds at CHUNK blocks/tick.
const RECONCILE_CHUNK: u64 = 200;

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
        tracing::info!(
            reconcile_cursor = start_reconcile,
            "note reconciler: sweep cursor loaded — next sweep window starts at block {}",
            start_reconcile + 1
        );
        Ok(Self {
            store,
            block_state,
            bridge_id: accounts.bridge.0,
            local_network_id,
            expected_ger_sender,
            l1_rpc_url,
            cursor: AtomicU64::new(start_cursor),
            node_rpc,
            reconcile_cursor: AtomicU64::new(start_reconcile),
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
        Some((from, (from + RECONCILE_CHUNK - 1).min(tip)))
    }

    /// Note-visibility reconciler (completeness guarantee for externally-created
    /// network notes). Walks `sync_notes` over the next `RECONCILE_CHUNK` blocks
    /// and imports any tag-0 note the local store doesn't know. The next
    /// `sync_state` then discovers the (possibly historical) consumption via the
    /// nullifier check, and the late-consumption sweep in `tick` projects it.
    /// Non-B2AGG imports (MINTs to external wallets, etc.) are harmless: every
    /// `project_*` derivation gates on script root + consumer.
    async fn reconcile_notes(
        &self,
        client: &mut MidenClientLib,
        rpc: &dyn NodeRpcClient,
        tip: u64,
    ) -> anyhow::Result<()> {
        let Some((from, to)) = self.next_reconcile_window(tip) else {
            return Ok(());
        };
        let tags: BTreeSet<NoteTag> = BTreeSet::from([NoteTag::from(0u32)]);
        let blocks = rpc
            .sync_notes((from as u32).into(), (to as u32).into(), &tags)
            .await
            .map_err(|e| anyhow::anyhow!("sync_notes({from}..{to}): {e}"))?;
        let candidates: Vec<NoteId> = blocks
            .iter()
            .flat_map(|b| b.notes.keys().copied())
            .collect();
        if !candidates.is_empty() {
            let known: HashSet<NoteId> = client
                .get_input_notes(NoteFilter::List(candidates.clone()))
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
        // Persist write-behind AFTER the window's work completed, and BEFORE
        // updating the in-memory cache (same ordering guarantee as the
        // projection cursor in `tick`): the durable cursor never runs ahead of
        // work actually done. A crash between the work and this store redoes
        // the window next boot — safe, the sweep is idempotent. A persist
        // failure fails this tick (warn + retry in `tick`), leaving the
        // in-memory cursor un-advanced so the window is re-swept.
        self.store.set_reconcile_cursor(to).await?;
        self.reconcile_cursor.store(to, Ordering::Release);
        metrics::gauge!("synthetic_reconciler_cursor").set(to as f64);
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
        let consumed_by_bridge: HashMap<Nullifier, (u64, u32)> =
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
                Some((block, tx_order)) => {
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

        // Determinism: order intra-block events by (consumed_tx_order, note-id).
        // `consumed_tx_order` is the per-account position of the consuming
        // transaction within the block; the 32-byte details-commitment is the
        // stable tie-breaker. Compare the commitment bytes directly — identical
        // ordering to the old hex-string compare, but no per-comparison
        // allocation (matters when many notes share a block).
        notes.sort_by(|a, b| {
            a.state()
                .consumed_tx_order()
                .cmp(&b.state().consumed_tx_order())
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

            if project_claim_note(&self.store, note, miden_block, block_hash, bridge_address)
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
            && let Err(e) = self.reconcile_notes(client, rpc.as_ref(), tip).await
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
        let no_notes: Vec<&InputNoteRecord> = Vec::new();
        while cursor < tip {
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

/// MA#3 reclaim gate for the spent-before-import recovery path: map every
/// nullifier consumed by a BRIDGE-executed transaction to `(spend_block,
/// per-block bridge-tx order)`.
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
) -> HashMap<Nullifier, (u64, u32)> {
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
            out.insert(input.nullifier(), (block, order));
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
            .project_block_notes(&by_block[&6], &empty, 6, None)
            .await
            .unwrap();
        let logs8 = projector
            .project_block_notes(&by_block[&8], &empty, 8, None)
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
        assert_eq!(map.get(&a), Some(&(9, 0)));
        assert_eq!(
            map.get(&c),
            Some(&(9, 1)),
            "per-block bridge-tx order increments"
        );
        assert!(
            !map.contains_key(&b),
            "non-bridge consumption must be gated out (MA#3 fail-closed)"
        );
    }
}
