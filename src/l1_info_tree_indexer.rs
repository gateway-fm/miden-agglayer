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
use sha3::{Digest, Keccak256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::store::Store;

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
    /// Strict-H6 evidence qualification tag (audit H6 BLOCKER 3). When a
    /// finality tag (`finalized`/`safe`) is configured, each poll also fetches
    /// that L1 block and persists its number via `set_l1_finalized_block`, which
    /// the strict gate uses to qualify evidence. In `Confirmations` mode the
    /// indexer does no extra work (the gate uses the head cursor + depth).
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

    /// Configure the strict-H6 evidence tag. In `Finalized`/`Safe` mode the
    /// indexer tracks the corresponding L1 finality block (BLOCKER 3).
    pub fn with_evidence_tag(mut self, tag: crate::ger::EvidenceTag) -> Self {
        self.evidence_tag = tag;
        self
    }

    /// The L1 block tag to poll for finality tracking, or `None` in
    /// confirmation-depth mode (no finality block needed).
    fn finality_block_tag(&self) -> Option<BlockNumberOrTag> {
        match self.evidence_tag {
            crate::ger::EvidenceTag::Confirmations(_) => None,
            crate::ger::EvidenceTag::Finalized => Some(BlockNumberOrTag::Finalized),
            crate::ger::EvidenceTag::Safe => Some(BlockNumberOrTag::Safe),
        }
    }

    /// Best-effort refresh of the persisted L1 finality-tag block (BLOCKER 3).
    /// A no-op in confirmation-depth mode. A fetch/persist failure just leaves
    /// the previous value — the strict gate stays fail-closed (never
    /// over-authorizes on a stale finality block).
    async fn refresh_finality_block<P: Provider>(&self, provider: &P) {
        let Some(tag) = self.finality_block_tag() else {
            return;
        };
        match provider.get_block_by_number(tag).await {
            Ok(Some(block)) => {
                let n = block.header.number;
                if let Err(e) = self.store.set_l1_finalized_block(n).await {
                    tracing::warn!(error = %e, tag = ?tag, "L1InfoTreeIndexer: failed to persist L1 finality block");
                }
            }
            Ok(None) => {
                tracing::debug!(tag = ?tag, "L1InfoTreeIndexer: L1 finality block not available yet");
            }
            Err(e) => {
                tracing::debug!(error = %e, tag = ?tag, "L1InfoTreeIndexer: failed to fetch L1 finality block");
            }
        }
    }

    /// Operator override for the indexer start block. Overrides both the
    /// persisted cursor and the L1-head fallback for one boot. After that
    /// boot's first persisted cursor write, the override stops mattering
    /// and the normal resume-from-cursor path takes over.
    pub fn with_from_block_override(mut self, from_block: u64) -> Self {
        self.from_block_override = Some(from_block);
        self
    }

    /// Spawn the indexer as a tokio task. Returns a oneshot sender for graceful
    /// shutdown — drop the sender or send `()` to stop the loop.
    ///
    /// Errors during polling are logged and the loop continues; we never want a
    /// transient L1 RPC blip to take down the whole service. Permanent failure
    /// (e.g. malformed contract address) returns Err synchronously.
    pub fn spawn(self) -> anyhow::Result<oneshot::Sender<()>> {
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

            // Resume from the persisted cursor if we have one, else start at
            // current L1 head. The persisted cursor closes the gap that
            // stranded GERs every time the proxy restarted (OOMKills,
            // planned deploys): historic `UpdateL1InfoTree` events emitted
            // during downtime are now indexed on the next boot and the
            // orphan ger_entries rows from that window get their (M, R)
            // filled in by the indexer's `set_ger_exit_roots` UPSERT.
            //
            // Fresh deployments (cursor = 0) start at head — same behaviour
            // as before persistence. Pre-existing deployments inherit a 0
            // cursor on first boot after the migration; treat 0 as "no
            // cursor recorded yet" and fall back to head to avoid a
            // multi-million-block backfill on the first boot.
            let head = provider.get_block_number().await.unwrap_or_else(|e| {
                tracing::error!(error = %e, "L1InfoTreeIndexer: failed to fetch initial L1 block; starting at 0");
                0
            });
            let stored = match self.store.get_l1_indexer_cursor().await {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "L1InfoTreeIndexer: failed to load persisted cursor; falling back to L1 head"
                    );
                    0
                }
            };
            // Resolve start block:
            //   1. Operator override (`--l1-indexer-from-block <N>`) wins
            //      unconditionally — used to backfill historic orphan GERs
            //      whose events predate the persisted cursor.
            //   2. Else persisted cursor minus reorg margin, if non-zero.
            //   3. Else current L1 head (fresh deployment).
            let mut last_processed: u64 = if let Some(forced) = self.from_block_override {
                tracing::warn!(
                    from_block = forced,
                    stored_cursor = stored,
                    l1_head = head,
                    "L1InfoTreeIndexer: operator override active — starting from forced block. \
                     Remove --l1-indexer-from-block after this boot's backfill completes."
                );
                forced.saturating_sub(1)
            } else if stored == 0 {
                head
            } else {
                // Re-process a small reorg window so we don't miss reorg'd
                // events. Sepolia 64 blocks ≈ 12 minutes, well inside what
                // `get_logs` can chunk through quickly via max_range.
                const REORG_MARGIN: u64 = 64;
                stored.saturating_sub(REORG_MARGIN)
            };
            tracing::info!(
                start_block = last_processed,
                stored_cursor = stored,
                l1_head = head,
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
        // Keep the L1 finality-tag block fresh for the strict gate (no-op in
        // confirmation-depth mode). Done before the head check so it still
        // advances while the head is idle.
        self.refresh_finality_block(provider).await;
        // BLOCKER 1 — mark finalized-chain evidence (no-op in confirmation-depth
        // mode). Runs regardless of head progress so finality marking keeps up.
        self.poll_finalized_scan(provider).await?;

        let head = provider.get_block_number().await?;
        if head <= *last_processed {
            return Ok(());
        }

        // Index the `(mainnet, rollup)` decomposition up to LATEST — ordinary
        // decomposition / bridge readiness (`zkevm_getExitRootsByGER`) must not
        // be delayed. H6 reorg-safety for the IRREVERSIBLE strict injection is
        // enforced at the gate instead (`ger::ensure_ger_l1_observed` requires
        // the observation to be `confirmations`-deep on L1, using this indexer's
        // persisted cursor as the head), so a not-yet-final observation is
        // recorded for normal use but not trusted for strict authorization.
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

        // Persist the cursor so a restart resumes from here instead of
        // jumping back to L1 head. Failure to persist is logged but does
        // not abort the loop — we'd rather keep indexing on a transient
        // DB blip than wedge the service.
        if let Err(e) = self.store.set_l1_indexer_cursor(to).await {
            tracing::warn!(
                error = %e,
                cursor = to,
                "L1InfoTreeIndexer: failed to persist cursor; continuing in-memory"
            );
            metrics::counter!("l1_info_tree_indexer_cursor_persist_errors_total").increment(1);
        }

        Ok(())
    }

    /// Audit H6 BLOCKER 1 — the FINALIZED-pinned scan. In a finality-tag mode,
    /// scan `(mainnet, rollup)` events over `[finalized_scan_cursor+1,
    /// finalized_block]` and `mark_ger_finalized` each pair. Because the window
    /// ends at the L1 finalized/safe block, every log it returns is on the
    /// canonical finalized chain — a fork's event at a height <= finalized is NOT
    /// returned, so it is never marked and can never authorize. A no-op in
    /// confirmation-depth mode. A mark-write failure keeps the batch retryable
    /// (cursor not advanced), exactly like the latest scan.
    async fn poll_finalized_scan<P: Provider>(&self, provider: &P) -> anyhow::Result<()> {
        if !self.evidence_tag.is_finality_tag() {
            return Ok(());
        }
        let finalized = self.store.get_l1_finalized_block().await?;
        if finalized == 0 {
            return Ok(());
        }
        let cursor = self.store.get_l1_finalized_scan_cursor().await?;
        if finalized <= cursor {
            return Ok(());
        }
        let from = cursor + 1;
        let to = finalized.min(from + self.max_range - 1);

        let filter = Filter::new()
            .address(self.contract_address)
            .from_block(from)
            .to_block(to)
            .event_signature(vec![
                UpdateL1InfoTree::SIGNATURE_HASH,
                UpdateGlobalExitRoot::SIGNATURE_HASH,
            ]);
        let logs: Vec<Log> = provider.get_logs(&filter).await?;

        let mut marked = 0usize;
        for log in &logs {
            let topics = log.topics();
            if topics.len() < 3 {
                continue;
            }
            let mainnet: [u8; 32] = topics[1].0;
            let rollup: [u8; 32] = topics[2].0;
            let combined = combined_ger(&mainnet, &rollup);
            if let Err(e) = self.store.mark_ger_finalized(&combined).await {
                metrics::counter!("l1_info_tree_indexer_log_errors_total").increment(1);
                return Err(e.context(format!(
                    "L1InfoTreeIndexer: finalized mark failed at block {}; finalized batch \
                     [{from}, {to}] left unadvanced (retryable)",
                    log.block_number.unwrap_or(0)
                )));
            }
            marked += 1;
        }

        if marked > 0 {
            tracing::info!(
                from,
                to,
                finalized,
                marked,
                "L1InfoTreeIndexer: marked finalized-chain evidence"
            );
        }
        if let Err(e) = self.store.set_l1_finalized_scan_cursor(to).await {
            tracing::warn!(
                error = %e,
                cursor = to,
                "L1InfoTreeIndexer: failed to persist finalized-scan cursor; continuing in-memory"
            );
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
    use alloy::primitives::{B256, Bytes, LogData, TxHash, U64};
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

    /// BLOCKER 1 — the indexer records the decomposition up to LATEST (NO
    /// confirmation delay for ordinary decomposition), while the STRICT gate
    /// enforces finality using the indexer cursor as the L1 head. Drives the
    /// REAL `poll_once` with a mock L1: a pair only 2 blocks deep is recorded
    /// immediately (so `get_ger_entry` / `zkevm_getExitRootsByGER` see it with no
    /// delay), yet the strict gate REFUSES it until the cursor advances past the
    /// confirmation depth — which is exactly what keeps a short-lived reorg from
    /// authorizing an irreversible injection while never delaying normal ops.
    ///
    /// Mutation check: dropping the gate's `l1_head - block >= confirmations`
    /// clause makes the shallow observation wrongly authorize (phase-1 pass).
    #[tokio::test]
    async fn h6_indexes_to_latest_but_strict_gate_waits_for_finality() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let indexer = test_indexer(store.clone());

        let mainnet = B256::from([0x0Au8; 32]);
        let rollup = B256::from([0x0Bu8; 32]);
        let ger = combined_ger(&mainnet.0, &rollup.0);
        let tx = TxHash::from([0x01u8; 32]);
        const CONF: u64 = 64;

        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let mut last_processed = 0u64;

        // Phase 1 — L1 head 10: poll_once indexes the pair at block 8 IMMEDIATELY
        // (only 2 deep) and advances the cursor to head. Ordinary decomposition
        // sees it at once; the strict gate refuses it (2 < 64) — transiently.
        asserter.push_success(&U64::from(10u64));
        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        asserter.push_success(&Option::<serde_json::Value>::None); // block ts → 0
        indexer
            .poll_once(&provider, &mut last_processed)
            .await
            .unwrap();
        assert_eq!(
            last_processed, 10,
            "cursor tracks latest (no confirmation delay)"
        );
        let entry = store
            .get_ger_entry(&ger)
            .await
            .unwrap()
            .expect("decomposition must be recorded immediately (no delay for normal ops)");
        assert!(entry.mainnet_exit_root.is_some() && entry.rollup_exit_root.is_some());

        let err = crate::ger::ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            crate::ger::EvidenceTag::Confirmations(CONF),
            tx,
        )
        .await
        .expect_err("strict gate must refuse a not-yet-confirmation-deep observation");
        assert!(
            err.to_string().contains("not yet"),
            "must cite the finality guard: {err:#}"
        );

        // Phase 2 — L1 advances to 80: poll_once moves the cursor to 80, so the
        // block-8 observation is now 72 deep (>= 64) and the strict gate passes.
        asserter.push_success(&U64::from(80u64));
        asserter.push_success(&Vec::<alloy::rpc::types::Log>::new()); // no new events
        indexer
            .poll_once(&provider, &mut last_processed)
            .await
            .unwrap();
        assert_eq!(last_processed, 80, "cursor advanced to the new head");
        crate::ger::ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            crate::ger::EvidenceTag::Confirmations(CONF),
            tx,
        )
        .await
        .expect("strict gate must authorize once the observation is confirmation-deep");
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
        asserter.push_success(&U64::from(100u64));
        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        asserter.push_success(&Option::<serde_json::Value>::None);

        let mut last_processed = 0u64;
        let err = indexer
            .poll_once(&provider, &mut last_processed)
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

    /// BLOCKER 1 (re-review) — the FINALIZED-pinned scan is the finalized-chain
    /// tie. In `finalized` mode it scans `[cursor+1, finalized_block]` (whose logs
    /// ARE the canonical finalized chain) and marks each pair `finalized_verified`
    /// — which is exactly what the strict `finalized` gate then requires. A
    /// `latest`-observed pair NOT covered by this scan is never marked and cannot
    /// authorize (proved in `ger::tests::h6_finalized_tag_gate_requires_finalized_chain_tie`).
    ///
    /// Mutation check: making `poll_finalized_scan` a no-op leaves the row
    /// unverified → the authorize step below fails.
    #[tokio::test]
    async fn poll_finalized_scan_marks_finalized_chain_evidence() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let indexer =
            test_indexer(store.clone()).with_evidence_tag(crate::ger::EvidenceTag::Finalized);

        let mainnet = B256::from([0x0Au8; 32]);
        let rollup = B256::from([0x0Bu8; 32]);
        let ger = combined_ger(&mainnet.0, &rollup.0);
        // The latest scan already recorded roots (block 8); NOT finalized yet.
        store
            .set_ger_exit_roots(&ger, mainnet.0, rollup.0, 8, 0)
            .await
            .unwrap();
        assert!(
            !store
                .get_ger_entry(&ger)
                .await
                .unwrap()
                .unwrap()
                .finalized_verified,
            "must start un-finalized"
        );

        // The L1 finalized block (100) is well above block 8; the finalized scan
        // covers [1, 100] and reads the canonical pair.
        store.set_l1_finalized_block(100).await.unwrap();
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        indexer.poll_finalized_scan(&provider).await.unwrap();

        assert!(
            store
                .get_ger_entry(&ger)
                .await
                .unwrap()
                .unwrap()
                .finalized_verified,
            "the finalized-pinned scan must mark the canonical pair finalized_verified"
        );
        assert_eq!(store.get_l1_finalized_scan_cursor().await.unwrap(), 100);

        // The strict `finalized` gate now authorizes it (roots + finalized_verified).
        crate::ger::ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            crate::ger::EvidenceTag::Finalized,
            TxHash::from([0x01u8; 32]),
        )
        .await
        .expect("a finalized-chain-verified observation must authorize");
    }
}
