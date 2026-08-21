//! L1 InfoTree event indexer — eliminates the RD-862 GER decomposition race.
//!
//! ## Why this exists
//!
//! `insertGlobalExitRoot(bytes32 combined)` only carries the keccak'd hash.
//! Recovering `(mainnet, rollup)` from that hash requires a reverse lookup.
//! The legacy code path in `service_send_raw_txn.rs::handle_send_raw_transaction`
//! tried `lastMainnetExitRoot()` / `lastRollupExitRoot()` view calls on L1 at
//! the moment the inject arrived. Under deposit load L1 has already advanced
//! past the pair that produced the combined hash and the keccak check fails
//! ~85-100% of the time (see `tests/baselines/baseline-rd862-repro.json`).
//!
//! Every regular CDK rollup avoids this by indexing L1's `UpdateL1InfoTree`
//! events: the pair is in the event payload itself, so reverse lookup becomes
//! a hashmap hit instead of a racing view call. This module is the missing
//! indexer — the architectural fix the wider plan calls for, scoped down to
//! exactly what's needed today to drive the orphan rate to zero.
//!
//! ## How it integrates
//!
//! Spawned from `main.rs` after `ServiceState` is ready, given an L1 RPC URL
//! and the GER manager contract address. Polls `eth_getLogs` for the two
//! event signatures `PolygonZkEVMGlobalExitRootV2` is known to emit:
//!   - `UpdateL1InfoTree(bytes32 mainnetExitRoot, bytes32 rollupExitRoot)`
//!   - `UpdateGlobalExitRoot(bytes32 mainnetExitRoot, bytes32 rollupExitRoot)`
//!
//! For each match, computes `combined = keccak(mainnet ‖ rollup)` and UPSERTs
//! the triple via `store.set_ger_exit_roots`. The PgStore impl has
//! `ON CONFLICT (ger_hash) DO UPDATE SET mainnet_exit_root = EXCLUDED, ...`,
//! so:
//!   - Indexer fires before `insert_ger` → entry pre-populated with (M, R).
//!     `the projector GER commit` then does `ON CONFLICT DO NOTHING` and
//!     preserves the indexer's roots.
//!   - `insert_ger` fires before indexer → entry exists with `None` roots.
//!     Indexer's UPSERT fills them in. Bridge-service's polling eventually
//!     re-queries `zkevm_getExitRootsByGER` and gets resolved roots.
//!
//! Either ordering converges to a resolved entry. No race window.

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use anyhow::Context as _;
use sha3::{Digest, Keccak256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::store::Store;

/// Per-RPC ceiling inside the synchronous catch-up. The alloy HTTP provider
/// has no default timeout, so without this a hung L1 endpoint stalls a startup
/// path forever — indistinguishable from a crashed process.
const CATCH_UP_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum head re-samples before the catch-up gives up as NOT-converged. A
/// chain producing blocks faster than we scan them must terminate with an
/// honest verdict rather than chase the head indefinitely.
const CATCH_UP_MAX_PASSES: u32 = 64;

/// Time allowance for ONE scan batch: a floor of [`CATCH_UP_RPC_TIMEOUT`] plus
/// room proportional to the block range (one timestamp RPC per unique block
/// dominates a dense batch), clamped to whatever is left of the overall budget.
fn batch_timeout_for(max_range: u64, remaining_budget: Duration) -> Duration {
    let scaled = Duration::from_secs(CATCH_UP_RPC_TIMEOUT.as_secs() + max_range / 20);
    if remaining_budget.is_zero() {
        // Budget already spent: let the caller's own budget check end the loop
        // rather than blocking here indefinitely.
        return Duration::from_secs(1);
    }
    scaled.min(remaining_budget)
}

/// Result of a bounded synchronous catch-up.
///
/// `converged` is the readiness signal callers act on: only `true` means the
/// evidence index reached an L1 head observed AFTER the last durable write, so
/// audit-H6 will corroborate any GER at or below it. Everything else — budget
/// exhausted, pass cap hit, no frontier configured — is a partial index, which
/// strict callers must treat as NOT ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUp {
    /// Last L1 block whose evidence is durably indexed.
    pub last_processed: u64,
    /// L1 head as of the final sample.
    pub head: u64,
    /// `last_processed >= head` at a head sampled after the final write.
    pub converged: bool,
    /// Nothing was scanned or persisted: empty cursor and no configured
    /// frontier (see `catch_up_to_head`).
    pub skipped_no_frontier: bool,
    /// Head re-samples performed (diagnostics).
    pub passes: u32,
}

impl CatchUp {
    fn not_converged(last_processed: u64, head: u64, passes: u32) -> Self {
        Self {
            last_processed,
            head,
            converged: false,
            skipped_no_frontier: false,
            passes,
        }
    }

    /// Blocks still unscanned at the moment we stopped.
    pub fn lag(&self) -> u64 {
        self.head.saturating_sub(self.last_processed)
    }
}

alloy_core::sol! {
    /// Standard PolygonZkEVMGlobalExitRootV2 event (current contracts).
    #[derive(Debug)]
    event UpdateL1InfoTree(
        bytes32 indexed mainnetExitRoot,
        bytes32 indexed rollupExitRoot,
    );

    /// Older alias emitted by some deployments / earlier contract versions.
    /// Kept here so a deployment on the older event signature still indexes.
    #[derive(Debug)]
    event UpdateGlobalExitRoot(
        bytes32 indexed mainnetExitRoot,
        bytes32 indexed rollupExitRoot,
    );
}

/// Default poll cadence. Anvil ticks at 1s by default in our e2e stack;
/// real Sepolia advances ~12s, so 1s is conservative and gives sub-block
/// latency without hammering the RPC.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1_000);

/// Default per-iteration block range cap. Caps the cost of a backfill or a
/// late-start when `--from-block` is unset; full Sepolia history would
/// otherwise overwhelm a single `eth_getLogs`.
const DEFAULT_MAX_RANGE: u64 = 1_000;

pub struct L1InfoTreeIndexer {
    rpc_url: String,
    contract_address: Address,
    store: Arc<dyn Store>,
    poll_interval: Duration,
    max_range: u64,
    /// Optional operator override: force the indexer to start polling from
    /// this L1 block on the next boot, ignoring any persisted cursor.
    /// Used to backfill historic orphan GERs whose `UpdateL1InfoTree` events
    /// predate the persisted cursor (e.g. bali's 27 NULL-roots rows from
    /// blocks 95k-130k). Operator passes via `--l1-indexer-from-block <N>`
    /// or env `L1_INDEXER_FROM_BLOCK`. After the backfill completes the
    /// cursor advances forward normally; remove the flag for subsequent
    /// boots.
    from_block_override: Option<u64>,
    /// The one L1 frontier this indexer scans. Roots are learned exclusively
    /// from `latest`, `safe`, or `finalized` according to this setting.
    evidence_tag: crate::ger::EvidenceTag,
}

impl L1InfoTreeIndexer {
    pub fn new(rpc_url: String, contract_address: Address, store: Arc<dyn Store>) -> Self {
        Self {
            rpc_url,
            contract_address,
            store,
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_range: DEFAULT_MAX_RANGE,
            from_block_override: None,
            evidence_tag: crate::ger::EvidenceTag::default(),
        }
    }

    /// Configure the single L1 scan frontier.
    pub fn with_evidence_tag(mut self, tag: crate::ger::EvidenceTag) -> Self {
        self.evidence_tag = tag;
        self
    }

    fn scan_block_tag(&self) -> BlockNumberOrTag {
        match self.evidence_tag {
            crate::ger::EvidenceTag::Latest => BlockNumberOrTag::Latest,
            crate::ger::EvidenceTag::Safe => BlockNumberOrTag::Safe,
            crate::ger::EvidenceTag::Finalized => BlockNumberOrTag::Finalized,
        }
    }

    async fn scan_head<P: Provider>(&self, provider: &P) -> anyhow::Result<u64> {
        let tag = self.scan_block_tag();
        let block = provider.get_block_by_number(tag).await?.ok_or_else(|| {
            anyhow::anyhow!("L1 `{}` block is unavailable", self.evidence_tag.describe())
        })?;
        Ok(block.header.number)
    }

    /// Operator override for the indexer start block. Overrides both the
    /// persisted cursor and the L1-head fallback for one boot. After that
    /// boot's first persisted cursor write, the override stops mattering
    /// and the normal resume-from-cursor path takes over.
    pub fn with_from_block_override(mut self, from_block: u64) -> Self {
        self.from_block_override = Some(from_block);
        self
    }

    fn initial_cursor(&self, stored: u64, selected_head: u64) -> u64 {
        self.from_block_override
            .map(|from| from.saturating_sub(1))
            .unwrap_or_else(|| if stored == 0 { selected_head } else { stored })
    }

    /// Spawn the indexer as a tokio task. Returns a oneshot sender for graceful
    /// shutdown — drop the sender or send `()` to stop the loop.
    ///
    /// Errors during polling are logged and the loop continues; we never want a
    /// transient L1 RPC blip to take down the whole service. Permanent failure
    /// (e.g. malformed contract address) returns Err synchronously.
    /// Bring the L1 evidence index up to the current head SYNCHRONOUSLY,
    /// then return the block it reached.
    ///
    /// FINDING #113: a full-DB-loss restore drops the store that holds this
    /// index, so after `--restore` the proxy previously started with NO L1
    /// evidence and rebuilt it lazily in the background ticker. During that
    /// catch-up window the proxy is serving but not READY: the audit-H6 guard
    /// correctly refuses to inject any GER it has not yet observed on L1, and
    /// aggkit's ethtxmanager turns that transient refusal into a PERMANENT
    /// stop (its deterministic-ID dedup never re-sends). Measured live: the
    /// refusal fired at 23:07:18 and this index caught up at 23:07:21 — three
    /// seconds too late, and GER injection stayed frozen until manual
    /// intervention.
    ///
    /// Running the catch-up as part of the restore closes that window: when
    /// the one-shot exits 0, the evidence needed to accept injections is
    /// already present, so the operator's next step (start the proxy) yields
    /// a proxy that is correct immediately rather than eventually.
    pub async fn catch_up_to_head(&self, budget: Duration) -> anyhow::Result<CatchUp> {
        let started = Instant::now();
        let provider = ProviderBuilder::new().connect_http(
            self.rpc_url
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid L1 RPC URL '{}': {}", self.rpc_url, e))?,
        );

        // A cursor-read failure is NOT "no cursor". Collapsing it to 0 would
        // silently restart the scan at the L1 head and abandon every block of
        // evidence below it, so propagate instead.
        let stored = self
            .store
            .get_l1_evidence_cursor()
            .await
            .context("loading the persisted L1 evidence cursor for the synchronous catch-up")?;

        // No frontier to scan FROM: an empty cursor with no operator
        // `--l1-indexer-from-block` means "fresh deployment, start at the
        // current head" (see `initial_cursor`). Scanning under that policy is
        // not a catch-up at all — worse, if a block lands between our two head
        // reads we would index that ONE block and persist a non-zero cursor,
        // which makes the startup backfill invariant (`check_h6_backfill_invariant`,
        // main.rs) read as "this database has a real evidence index" when in
        // fact everything below it was never scanned. Do nothing, persist
        // nothing, and report it so strict callers stay fail-closed.
        if self.from_block_override.is_none() && stored == 0 {
            tracing::info!(
                evidence_tag = %self.evidence_tag.describe(),
                "L1InfoTreeIndexer: no evidence frontier (empty cursor, no --l1-indexer-from-block) \
                 — skipping synchronous catch-up rather than persisting a head cursor that would \
                 hide the unscanned history below it"
            );
            return Ok(CatchUp {
                last_processed: 0,
                head: 0,
                converged: false,
                skipped_no_frontier: true,
                passes: 0,
            });
        }

        let head0 = self.scan_head_bounded(&provider).await?;
        let mut last_processed = self.initial_cursor(stored, head0);

        // A frontier AHEAD of the source head is not "already caught up": the
        // loop below would find `last_processed >= head` immediately and report
        // converged having scanned nothing, so strict serving would start with
        // zero corroboration. This is a misconfiguration (a from-block for a
        // different//reset chain, or a cursor from a longer chain) and must be
        // said out loud rather than silently satisfied.
        if last_processed > head0 {
            anyhow::bail!(
                "L1 evidence frontier is AHEAD of the chain: scanning would start at block {} \
                 but the `{}` head is only {}. Nothing can be corroborated from here — check \
                 --l1-indexer-from-block, or whether this database belongs to a different (or \
                 since-reset) L1 than the configured RPC.",
                last_processed + 1,
                self.evidence_tag.describe(),
                head0,
            );
        }
        let mut head = head0;
        tracing::info!(
            start_block = last_processed,
            stored_cursor = stored,
            l1_head = head,
            budget_secs = budget.as_secs(),
            evidence_tag = %self.evidence_tag.describe(),
            "L1InfoTreeIndexer: synchronous catch-up starting (restore/startup readiness, #113)"
        );

        // Termination is guaranteed three ways, because this runs on a startup
        // path where a hang is indistinguishable from a dead process:
        //   * every RPC is wrapped in `CATCH_UP_RPC_TIMEOUT`;
        //   * the whole loop is bounded by `budget`;
        //   * `CATCH_UP_MAX_PASSES` caps head re-sampling, so a chain that
        //     produces blocks faster than we can scan them exits as
        //     NOT-converged instead of chasing the head forever.
        // Re-sampling the head at all (rather than freezing the first sample)
        // is deliberate: "ready" must mean caught up to a head observed AFTER
        // the last batch we wrote.
        let mut passes = 0u32;
        loop {
            while last_processed < head {
                if started.elapsed() >= budget {
                    return Ok(CatchUp::not_converged(last_processed, head, passes));
                }
                let before = last_processed;
                // One batch is a getLogs over up to `max_range` blocks PLUS a
                // timestamp fetch per unique block PLUS the durable writes, so
                // a flat per-RPC ceiling applied to the whole batch fails a
                // dense-but-healthy range every time and blocks strict startup
                // for a reason that has nothing to do with health. Scale the
                // allowance with the work, and never exceed what is left of the
                // overall budget.
                let batch_timeout =
                    batch_timeout_for(self.max_range, budget.saturating_sub(started.elapsed()));
                timeout(
                    batch_timeout,
                    self.poll_to_head(&provider, &mut last_processed, head),
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "L1 evidence scan of blocks {}..={head} timed out after {}s",
                        before + 1,
                        batch_timeout.as_secs()
                    )
                })??;
                // `poll_to_head` advances the cursor only after the batch is
                // durably written, so a successful call that did not advance
                // means we would spin on the same window forever.
                anyhow::ensure!(
                    last_processed > before,
                    "L1 evidence scan made no progress at block {before} (head {head}) — \
                     refusing to spin"
                );
            }

            passes += 1;
            if started.elapsed() >= budget || passes >= CATCH_UP_MAX_PASSES {
                return Ok(CatchUp::not_converged(last_processed, head, passes));
            }

            let new_head = self.scan_head_bounded(&provider).await?;
            if new_head <= last_processed {
                // The cursor write inside `poll_to_head` is best-effort (a
                // transient DB blip must not wedge the live ticker), so
                // in-memory progress can outrun what is durable. For a
                // READINESS verdict that distinction matters: if the cursor did
                // not persist, the next process starts over and the barrier we
                // just passed proved nothing about it. Re-read and require
                // durability before claiming convergence.
                let persisted =
                    self.store.get_l1_evidence_cursor().await.context(
                        "re-reading the L1 evidence cursor to confirm catch-up durability",
                    )?;
                if persisted < last_processed {
                    tracing::warn!(
                        persisted,
                        last_processed,
                        "L1InfoTreeIndexer: catch-up reached {last_processed} in memory but only \
                         {persisted} is durable — reporting NOT converged"
                    );
                    return Ok(CatchUp::not_converged(persisted, new_head, passes));
                }
                tracing::info!(
                    last_processed,
                    l1_head = new_head,
                    passes,
                    elapsed_secs = started.elapsed().as_secs(),
                    "L1InfoTreeIndexer: synchronous catch-up complete — L1 evidence is current"
                );
                return Ok(CatchUp {
                    last_processed,
                    head: new_head,
                    converged: true,
                    skipped_no_frontier: false,
                    passes,
                });
            }
            head = new_head;
        }
    }

    async fn scan_head_bounded<P: Provider>(&self, provider: &P) -> anyhow::Result<u64> {
        timeout(CATCH_UP_RPC_TIMEOUT, self.scan_head(provider))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "reading the L1 `{}` head timed out after {}s",
                    self.evidence_tag.describe(),
                    CATCH_UP_RPC_TIMEOUT.as_secs()
                )
            })?
    }

    /// Spawn the background ticker, resuming at `resume_at` when the caller has
    /// already brought the index up to that block (the strict-H6 readiness
    /// barrier does exactly that).
    ///
    /// Without this, `spawn()` re-resolves the start block from scratch — and
    /// `from_block_override` outranks both the stored cursor and the barrier's
    /// result, so a deployment that keeps `--l1-indexer-from-block` set (ours
    /// does) would rewind to that block and replay the entire range again the
    /// moment the listener binds: duplicate work in front of every new GER, and
    /// on a long chain a replay that outlives the process.
    pub fn spawn_resuming_at(self, resume_at: Option<u64>) -> anyhow::Result<oneshot::Sender<()>> {
        self.spawn_inner(resume_at)
    }

    pub fn spawn(self) -> anyhow::Result<oneshot::Sender<()>> {
        self.spawn_inner(None)
    }

    fn spawn_inner(self, resume_at: Option<u64>) -> anyhow::Result<oneshot::Sender<()>> {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let provider = ProviderBuilder::new().connect_http(
            self.rpc_url
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid L1 RPC URL '{}': {}", self.rpc_url, e))?,
        );

        tokio::spawn(async move {
            tracing::info!(
                contract = %self.contract_address,
                rpc = %self.rpc_url,
                poll_interval_ms = self.poll_interval.as_millis() as u64,
                "L1InfoTreeIndexer starting"
            );

            // Resume from the selected-policy cursor if we have one, else start
            // at the configured L1 frontier. The persisted cursor closes the gap that
            // stranded GERs every time the proxy restarted (OOMKills,
            // planned deploys): historic `UpdateL1InfoTree` events emitted
            // during downtime are now indexed on the next boot and the
            // orphan ger_entries rows from that window get their (M, R)
            // filled in by the indexer's `set_ger_exit_roots` UPSERT.
            //
            // Fresh lenient deployments (cursor = 0) start at the selected head — same behaviour
            // as before persistence. Pre-existing deployments inherit a 0
            // cursor on first boot after the migration; treat 0 as "no
            // cursor recorded yet" and fall back to head to avoid a
            // multi-million-block backfill on the first boot.
            let head = self.scan_head(&provider).await.unwrap_or_else(|e| {
                tracing::error!(error = %e, tag = %self.evidence_tag.describe(), "L1InfoTreeIndexer: failed to fetch initial selected L1 block; starting at 0");
                0
            });
            let stored = match self.store.get_l1_evidence_cursor().await {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "L1InfoTreeIndexer: failed to load selected-policy cursor; falling back to selected L1 head"
                    );
                    0
                }
            };
            // Resolve start block:
            //   1. Operator override (`--l1-indexer-from-block <N>`) wins
            //      unconditionally — used to backfill historic orphan GERs
            //      whose events predate the persisted cursor.
            //   2. Else the persisted selected-policy cursor, if non-zero.
            //   3. Else the selected L1 head (fresh lenient deployment).
            if let Some(forced) = self.from_block_override {
                tracing::warn!(
                    from_block = forced,
                    stored_cursor = stored,
                    l1_head = head,
                    "L1InfoTreeIndexer: operator override active — starting from forced block. \
                     Remove --l1-indexer-from-block after this boot's backfill completes."
                );
            }
            // A completed readiness barrier is authoritative for where the
            // ticker resumes: it already scanned (and persisted) up to
            // `resume_at`, and re-applying the operator override here would
            // throw that away.
            let mut last_processed = match resume_at {
                Some(from_barrier) => self.initial_cursor(stored, head).max(from_barrier),
                None => self.initial_cursor(stored, head),
            };
            tracing::info!(
                start_block = last_processed,
                stored_cursor = stored,
                resumed_from_barrier = ?resume_at,
                selected_head = head,
                evidence_tag = %self.evidence_tag.describe(),
                from_block_override = ?self.from_block_override,
                "L1InfoTreeIndexer cursor initialized"
            );

            let mut ticker = tokio::time::interval(self.poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("L1InfoTreeIndexer shutdown requested");
                        break;
                    }
                    _ = ticker.tick() => {}
                }

                if let Err(e) = self.poll_once(&provider, &mut last_processed).await {
                    tracing::warn!(error = %e, last_processed, "L1InfoTreeIndexer poll failed, retrying");
                    metrics::counter!("l1_info_tree_indexer_poll_errors_total").increment(1);
                }
            }

            tracing::info!("L1InfoTreeIndexer stopped");
        });

        Ok(shutdown_tx)
    }

    async fn poll_once<P: Provider>(
        &self,
        provider: &P,
        last_processed: &mut u64,
    ) -> anyhow::Result<()> {
        let head = self.scan_head(provider).await?;
        self.poll_to_head(provider, last_processed, head).await
    }

    async fn poll_to_head<P: Provider>(
        &self,
        provider: &P,
        last_processed: &mut u64,
        head: u64,
    ) -> anyhow::Result<()> {
        if head <= *last_processed {
            return Ok(());
        }

        // One configured scan is the sole source of L1 root evidence. In
        // `safe`/`finalized` mode, decomposition intentionally becomes visible
        // only when that frontier reaches the event.
        let from = *last_processed + 1;
        let to = head.min(from + self.max_range - 1);

        // Single filter matching either event signature; the topic-OR is
        // expressed by passing both signature hashes in topic[0].
        let filter = Filter::new()
            .address(self.contract_address)
            .from_block(from)
            .to_block(to)
            .event_signature(vec![
                UpdateL1InfoTree::SIGNATURE_HASH,
                UpdateGlobalExitRoot::SIGNATURE_HASH,
            ]);

        let logs: Vec<Log> = provider.get_logs(&filter).await?;
        let log_count = logs.len();

        // Per-poll cache: one `eth_getBlockByNumber` per *unique* L1 block in
        // the batch, used to populate the L1 timestamp written to
        // `ger_entries.timestamp`. Events from the same block share an RPC
        // roundtrip, so a steady-state poll that sees 0–1 unique blocks per
        // tick costs nothing extra in the common case.
        let mut block_timestamps: HashMap<u64, u64> = HashMap::new();

        let mut indexed = 0usize;
        for log in logs {
            let block_number = log.block_number.unwrap_or(0);
            let timestamp = self
                .resolve_block_timestamp(provider, block_number, &mut block_timestamps)
                .await;

            match self.process_log(&log, block_number, timestamp).await {
                Ok(true) => indexed += 1,
                Ok(false) => {}
                Err(e) => {
                    // Audit H6 / BLOCKER 3 — a durable evidence-write failure
                    // must keep the batch RETRYABLE. `process_log` only returns
                    // Err from the `set_ger_exit_roots` write (a malformed log
                    // returns Ok(false), never Err), so this is always a
                    // transient store failure, NOT a poison log. Advancing the
                    // cursor past it would drop a legitimate GER's evidence
                    // permanently, and under strict mode that GER would stay
                    // unverified forever (the side-effect-free retry never
                    // clears within the process lifetime). Propagate WITHOUT
                    // touching `*last_processed`, so the next poll re-runs
                    // exactly this window; `set_ger_exit_roots` is an idempotent
                    // UPSERT, so re-indexing already-written pairs is safe.
                    tracing::warn!(
                        error = %e,
                        block = block_number,
                        tx = ?log.transaction_hash,
                        from,
                        to,
                        "L1InfoTreeIndexer: durable evidence write failed; leaving batch \
                         unadvanced for retry"
                    );
                    metrics::counter!("l1_info_tree_indexer_log_errors_total").increment(1);
                    return Err(e.context(format!(
                        "L1InfoTreeIndexer: evidence write failed at block {block_number}; \
                         batch [{from}, {to}] left unadvanced (retryable)"
                    )));
                }
            }
        }

        // INFO-level activity log: bumped from debug per Igor's review on PR #41.
        // Quiet ticks (no events in the polled range) are kept at debug so we
        // don't flood the log file at the 1s poll cadence, but any range that
        // either contains events or indexes new pairs is surfaced.
        if log_count > 0 || indexed > 0 {
            tracing::info!(
                from,
                to,
                head,
                log_count,
                indexed,
                "L1InfoTreeIndexer batch processed"
            );
        } else {
            tracing::debug!(from, to, head, "L1InfoTreeIndexer polled (no events)");
        }
        metrics::counter!("l1_info_tree_indexer_pairs_indexed_total").increment(indexed as u64);

        *last_processed = to;

        // Persist the selected-policy cursor so a restart resumes from here.
        // Failure to persist is logged but does
        // not abort the loop — we'd rather keep indexing on a transient
        // DB blip than wedge the service.
        if let Err(e) = self.store.set_l1_evidence_cursor(to).await {
            tracing::warn!(
                error = %e,
                cursor = to,
                "L1InfoTreeIndexer: failed to persist selected-policy cursor; continuing in-memory"
            );
            metrics::counter!("l1_info_tree_indexer_cursor_persist_errors_total").increment(1);
        }

        Ok(())
    }

    async fn process_log(
        &self,
        log: &Log,
        block_number: u64,
        timestamp: u64,
    ) -> anyhow::Result<bool> {
        // Both event signatures have the same shape: two indexed bytes32.
        // Topic 0 = event sig hash, topic 1 = mainnetExitRoot, topic 2 = rollupExitRoot.
        let topics = log.topics();
        if topics.len() < 3 {
            return Ok(false);
        }

        // Decode which event signature so the log line is unambiguous in
        // testing — `UpdateL1InfoTree` and `UpdateGlobalExitRoot` carry the
        // same (mainnet, rollup) payload but represent different stages of
        // the L1 GER lifecycle. Easier to debug a stuck deposit if the log
        // tells you which one fired.
        let event_kind = if topics[0].0 == UpdateL1InfoTree::SIGNATURE_HASH.0 {
            "UpdateL1InfoTree"
        } else if topics[0].0 == UpdateGlobalExitRoot::SIGNATURE_HASH.0 {
            "UpdateGlobalExitRoot"
        } else {
            "unknown"
        };

        let mainnet: [u8; 32] = topics[1].0;
        let rollup: [u8; 32] = topics[2].0;
        let combined = combined_ger(&mainnet, &rollup);

        self.store
            .set_ger_exit_roots(&combined, mainnet, rollup, block_number, timestamp)
            .await?;

        // INFO-level so test runs show every pair indexed in real time
        // (Igor's review on PR #41). One pair == one L1 deposit's worth of
        // GER state arriving — exactly what an operator wants to see during
        // a stuck-deposit triage.
        tracing::info!(
            event = event_kind,
            mainnet = %hex::encode(mainnet),
            rollup = %hex::encode(rollup),
            combined = %hex::encode(combined),
            block = block_number,
            timestamp,
            "L1InfoTreeIndexer: indexed exit-root pair"
        );
        Ok(true)
    }

    /// Resolve the L1 block timestamp for a given block number, using and
    /// updating the per-poll cache. Returns 0 if the block is unknown
    /// (block_number == 0) or if the RPC lookup fails — the indexer's
    /// upsert path keeps the row writable in that case, and the next
    /// successful poll will overwrite with the real timestamp.
    async fn resolve_block_timestamp<P: Provider>(
        &self,
        provider: &P,
        block_number: u64,
        cache: &mut HashMap<u64, u64>,
    ) -> u64 {
        if block_number == 0 {
            return 0;
        }
        if let Some(&ts) = cache.get(&block_number) {
            return ts;
        }
        match provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await
        {
            Ok(Some(block)) => {
                let ts = block.header.timestamp;
                cache.insert(block_number, ts);
                ts
            }
            Ok(None) => {
                tracing::debug!(
                    block = block_number,
                    "L1InfoTreeIndexer: get_block_by_number returned None; timestamp left as 0 (will be overwritten on next observation)"
                );
                0
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    block = block_number,
                    "L1InfoTreeIndexer: get_block_by_number failed; timestamp left as 0 (will be overwritten on next observation)"
                );
                0
            }
        }
    }
}

fn combined_ger(mainnet: &[u8; 32], rollup: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(mainnet);
    hasher.update(rollup);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use alloy::primitives::{B256, Bytes, LogData, TxHash};
    use alloy::providers::ProviderBuilder;
    use alloy_transport::mock::Asserter;

    #[test]
    fn combined_ger_matches_ger_module() {
        // Sanity: this module's combined_ger must agree with crate::ger::combined_ger,
        // since the two are derived independently and any divergence would mean
        // indexed pairs would land under the wrong key in ger_entries.
        let mainnet = [1u8; 32];
        let rollup = [2u8; 32];
        assert_eq!(
            combined_ger(&mainnet, &rollup),
            crate::ger::combined_ger(&mainnet, &rollup),
        );
    }

    #[test]
    fn event_signatures_are_distinct() {
        // If these collide with each other or with anything else we filter on,
        // the OR-filter would silently miss one event family.
        assert_ne!(
            UpdateL1InfoTree::SIGNATURE_HASH,
            UpdateGlobalExitRoot::SIGNATURE_HASH
        );
    }

    // ── H6 reorg-safety + retryable-batch regressions (PR #121 re-review) ──

    /// Construct a bare indexer over `store`. The RPC URL is never dialled
    /// (poll_once is driven with a mock provider).
    fn test_indexer(store: Arc<dyn Store>) -> L1InfoTreeIndexer {
        L1InfoTreeIndexer::new(
            "http://mock.invalid".to_string(),
            Address::from([0x99u8; 20]),
            store,
        )
    }

    /// Build an `UpdateL1InfoTree` log carrying the `(mainnet, rollup)` pair at
    /// `block`, shaped exactly as `process_log` decodes it (topic0 = event sig,
    /// topic1 = mainnet, topic2 = rollup).
    fn pair_log(mainnet: B256, rollup: B256, block: u64) -> alloy::rpc::types::Log {
        let data = LogData::new_unchecked(
            vec![UpdateL1InfoTree::SIGNATURE_HASH, mainnet, rollup],
            Bytes::new(),
        );
        alloy::rpc::types::Log {
            inner: alloy::primitives::Log {
                address: Address::from([0x99u8; 20]),
                data,
            },
            block_hash: None,
            block_number: Some(block),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    /// The one selected scan writes roots and provenance together, then advances
    /// its one cursor. The strict gate can therefore trust exactly those rows.
    #[tokio::test]
    async fn selected_scan_persists_roots_provenance_and_cursor() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let indexer = test_indexer(store.clone()).with_evidence_tag(crate::ger::EvidenceTag::Safe);

        let mainnet = B256::from([0x0Au8; 32]);
        let rollup = B256::from([0x0Bu8; 32]);
        let ger = combined_ger(&mainnet.0, &rollup.0);

        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let mut last_processed = 0u64;

        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        asserter.push_success(&Option::<serde_json::Value>::None); // block ts → 0
        indexer
            .poll_to_head(&provider, &mut last_processed, 10)
            .await
            .unwrap();
        assert_eq!(last_processed, 10);
        assert_eq!(store.get_l1_evidence_cursor().await.unwrap(), 10);
        let entry = store
            .get_ger_entry(&ger)
            .await
            .unwrap()
            .expect("selected scan must persist the decomposition");
        assert!(entry.mainnet_exit_root.is_some() && entry.rollup_exit_root.is_some());
        assert!(entry.evidence_verified);

        crate::ger::ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            crate::ger::EvidenceTag::Safe,
            TxHash::from([0x01u8; 32]),
        )
        .await
        .expect("selected-scan evidence must authorize strict admission");
    }

    /// BLOCKER 3 (retryable batch) — a durable evidence-write failure must keep
    /// the batch retryable: `poll_once` must propagate the error and leave the
    /// cursor UNADVANCED so the next poll re-attempts the same window (the
    /// `set_ger_exit_roots` UPSERT makes retries idempotent). Pre-fix the loop
    /// logged the error and advanced the cursor anyway, dropping the GER's
    /// evidence permanently — under strict mode that GER stays unverified for
    /// the whole process lifetime.
    ///
    /// Mutation check: reverting to the old "log + continue" arm (no early
    /// return) makes poll_once return Ok and advance the cursor to 36 — this
    /// test fails on both assertions.
    #[tokio::test]
    async fn h6_evidence_write_failure_leaves_batch_retryable() {
        let store = Arc::new(InMemoryStore::new());
        store.test_fail_next_ger_evidence_write();
        let indexer = test_indexer(store);

        let mainnet = B256::from([0x0Cu8; 32]);
        let rollup = B256::from([0x0Du8; 32]);

        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        asserter.push_success(&Option::<serde_json::Value>::None);

        let mut last_processed = 0u64;
        let err = indexer
            .poll_to_head(&provider, &mut last_processed, 100)
            .await
            .expect_err("a durable evidence-write failure must fail the batch");
        assert!(
            err.to_string().contains("evidence write failed"),
            "error must identify the retryable batch: {err:#}"
        );
        assert_eq!(
            last_processed, 0,
            "cursor MUST NOT advance past a batch whose evidence write failed"
        );
    }

    #[test]
    fn one_tag_and_from_block_drive_the_single_cursor() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let latest = test_indexer(store.clone());
        let safe = test_indexer(store.clone()).with_evidence_tag(crate::ger::EvidenceTag::Safe);
        let finalized = test_indexer(store)
            .with_evidence_tag(crate::ger::EvidenceTag::Finalized)
            .with_from_block_override(12_345);

        assert_eq!(latest.scan_block_tag(), BlockNumberOrTag::Latest);
        assert_eq!(safe.scan_block_tag(), BlockNumberOrTag::Safe);
        assert_eq!(finalized.scan_block_tag(), BlockNumberOrTag::Finalized);
        assert_eq!(finalized.initial_cursor(0, 20_000_000), 12_344);
        assert_eq!(safe.initial_cursor(77, 100), 77);
        assert_eq!(latest.initial_cursor(0, 100), 100);
    }

    /// An empty cursor with no configured frontier means "fresh deployment,
    /// start at head" — there is nothing to catch UP to, and scanning anyway
    /// risks persisting a head cursor that makes an unscanned history look
    /// like a real evidence index to `check_h6_backfill_invariant`.
    ///
    /// The RPC URL below is unroutable on purpose: reaching the network at all
    /// would fail the test, which is exactly the assertion — the frontier check
    /// must happen BEFORE any L1 call.
    #[tokio::test]
    async fn catch_up_skips_and_persists_nothing_without_a_frontier() {
        let store = Arc::new(InMemoryStore::new());
        let indexer = test_indexer(store.clone() as Arc<dyn Store>);

        let outcome = indexer
            .catch_up_to_head(Duration::from_secs(30))
            .await
            .expect("a missing frontier is a configuration state, not an error");

        assert!(outcome.skipped_no_frontier, "must report the skip");
        assert!(
            !outcome.converged,
            "a skipped catch-up is NOT readiness — strict callers must fail closed on it"
        );
        assert_eq!(outcome.passes, 0, "no scan passes may run");
        assert_eq!(
            store.get_l1_evidence_cursor().await.unwrap(),
            0,
            "the cursor must stay 0 so the startup backfill invariant still sees a fresh database"
        );
    }

    /// A cursor READ failure must propagate. Collapsing it to 0 would resume the
    /// scan at the L1 head and abandon every block of evidence below it, while
    /// reporting success.
    #[tokio::test]
    async fn catch_up_propagates_cursor_read_failure() {
        let store = Arc::new(InMemoryStore::new());
        store.fail_l1_evidence_cursor_reads(true);
        let indexer = test_indexer(store.clone() as Arc<dyn Store>).with_from_block_override(1_000);

        let err = indexer
            .catch_up_to_head(Duration::from_secs(30))
            .await
            .expect_err("an unreadable cursor must not be treated as an empty cursor");
        let report = format!("{err:#}");
        assert!(
            report.contains("L1 evidence cursor"),
            "the error must name what failed, got: {report}"
        );
    }

    /// Readiness semantics: only `converged` means "H6 can corroborate roots up
    /// to `head`". Budget/pass exhaustion is a partial index and must never be
    /// mistaken for readiness.
    #[test]
    fn not_converged_reports_the_lag() {
        let partial = CatchUp::not_converged(1_000, 1_250, 7);
        assert!(!partial.converged);
        assert!(!partial.skipped_no_frontier);
        assert_eq!(partial.lag(), 250);

        let ready = CatchUp {
            last_processed: 1_250,
            head: 1_250,
            converged: true,
            skipped_no_frontier: false,
            passes: 2,
        };
        assert_eq!(ready.lag(), 0);
    }
}
