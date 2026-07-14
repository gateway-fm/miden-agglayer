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

/// Default confirmation depth for H6 GER evidence (audit H6 / reorg safety).
///
/// A Miden GER injection is IRREVERSIBLE, and the strict `--reject-unverified-
/// ger-injection` gate authorizes an injection once the indexer has recorded
/// the `(mainnet, rollup)` decomposition. The indexer therefore MUST NOT record
/// evidence that a reorg could still orphan: `set_ger_exit_roots` is UPSERT-only
/// (no delete / no `invalidated` state), so a stale "observed" row from a
/// reorged-away fork would permanently authorize a GER that never truly landed
/// on the canonical L1. We close this at the SOURCE by only ever indexing
/// evidence at least `confirmations` blocks below `latest` — a not-yet-final
/// GER is simply never recorded, so the strict gate fail-closes (retryable)
/// until finality is reached, and no rollback of an orphaned row is needed.
///
/// 64 mirrors the startup `REORG_MARGIN` and sits at/above realistic Sepolia
/// reorg depth (justification lands ~1 epoch, finality ~2 epochs / 64 slots).
/// Operators can raise it (or, in a future extension, point the indexer at the
/// `finalized`/`safe` block tag) via `--l1-indexer-confirmations`.
pub const DEFAULT_CONFIRMATIONS: u64 = 64;

pub struct L1InfoTreeIndexer {
    rpc_url: String,
    contract_address: Address,
    store: Arc<dyn Store>,
    poll_interval: Duration,
    max_range: u64,
    /// Confirmation depth below `latest` at which an observed exit-root pair is
    /// considered final enough to record as H6 GER evidence. See
    /// [`DEFAULT_CONFIRMATIONS`]. Evidence at a shallower depth is deliberately
    /// NOT recorded, so a short-lived reorg can never leave a stale "observed"
    /// row that permanently authorizes an irreversible strict-mode injection.
    confirmations: u64,
    /// Optional operator override: force the indexer to start polling from
    /// this L1 block on the next boot, ignoring any persisted cursor.
    /// Used to backfill historic orphan GERs whose `UpdateL1InfoTree` events
    /// predate the persisted cursor (e.g. bali's 27 NULL-roots rows from
    /// blocks 95k-130k). Operator passes via `--l1-indexer-from-block <N>`
    /// or env `L1_INDEXER_FROM_BLOCK`. After the backfill completes the
    /// cursor advances forward normally; remove the flag for subsequent
    /// boots.
    from_block_override: Option<u64>,
}

impl L1InfoTreeIndexer {
    pub fn new(rpc_url: String, contract_address: Address, store: Arc<dyn Store>) -> Self {
        Self {
            rpc_url,
            contract_address,
            store,
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_range: DEFAULT_MAX_RANGE,
            confirmations: DEFAULT_CONFIRMATIONS,
            from_block_override: None,
        }
    }

    /// Override the H6 evidence confirmation depth (audit H6 / reorg safety).
    /// A larger value is strictly safer (evidence is only recorded deeper below
    /// `latest`); a value of 0 disables the guard entirely (indexes up to
    /// `latest`) and reintroduces the reorg exposure — do not use in production
    /// with strict GER injection enabled.
    pub fn with_confirmations(mut self, confirmations: u64) -> Self {
        self.confirmations = confirmations;
        self
    }

    /// The confirmed batch window `[from, to]` this poll may process, or `None`
    /// when nothing new is final yet. `to` never exceeds `latest -
    /// confirmations`, so only evidence at least `confirmations` blocks deep is
    /// ever recorded (audit H6): a not-yet-final `(mainnet, rollup)` pair is
    /// never written, the strict gate stays fail-closed on it, and a short-lived
    /// reorg cannot leave a stale row that authorizes an irreversible injection.
    /// Pure + total so the finality decision is unit-testable without a live L1.
    fn confirmed_window(&self, head: u64, last_processed: u64) -> Option<(u64, u64)> {
        // Highest block final enough to trust. `checked_sub` yields None when
        // the chain is shorter than the confirmation depth (fresh testnet) —
        // nothing is final yet.
        let confirmed = head.checked_sub(self.confirmations)?;
        if confirmed <= last_processed {
            return None;
        }
        let from = last_processed + 1;
        let to = confirmed.min(from + self.max_range - 1);
        Some((from, to))
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
        let head = provider.get_block_number().await?;
        // Audit H6 / BLOCKER 1 — only ever consider evidence that is at least
        // `confirmations` blocks deep. A not-yet-final pair is not fetched, so
        // it is never recorded and the strict gate fail-closes on it until it
        // finalizes. Returns None (skip this tick) when nothing new is final.
        let Some((from, to)) = self.confirmed_window(head, *last_processed) else {
            return Ok(());
        };

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
    use crate::log_synthesis::{GerEntry, LogFilter, SyntheticLog};
    use crate::store::memory::InMemoryStore;
    use crate::store::{FaucetEntry, TxnData, TxnEntry, UnclaimableClaim};
    use alloy::primitives::{B256, Bytes, LogData, TxHash, U64, U256};
    use alloy::providers::ProviderBuilder;
    use alloy_transport::mock::Asserter;
    use miden_protocol::account::AccountId;
    use miden_protocol::transaction::TransactionId;

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

    /// Construct a bare indexer over `store` with a chosen confirmation depth.
    /// The RPC URL is never dialled (poll_once is driven with a mock provider).
    fn test_indexer(store: Arc<dyn Store>, confirmations: u64) -> L1InfoTreeIndexer {
        L1InfoTreeIndexer::new(
            "http://mock.invalid".to_string(),
            Address::from([0x99u8; 20]),
            store,
        )
        .with_confirmations(confirmations)
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

    /// BLOCKER 1 — `confirmed_window` is the finality gate: it never returns a
    /// `to` above `head - confirmations`, so evidence shallower than the
    /// confirmation depth is never fetched (and thus never recorded). Pure, so
    /// the finality decision is pinned without a live L1.
    #[test]
    fn confirmed_window_excludes_unconfirmed_tail() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let ix = test_indexer(store, 64);
        // Chain shorter than the depth → nothing is final yet.
        assert_eq!(ix.confirmed_window(10, 0), None);
        // head - depth == 0, already covered by cursor 0 → nothing new.
        assert_eq!(ix.confirmed_window(64, 0), None);
        // head 100, depth 64 → only [1, 36] is final.
        assert_eq!(ix.confirmed_window(100, 0), Some((1, 36)));
        // Cursor already at/after the confirmed head → nothing new.
        assert_eq!(ix.confirmed_window(100, 36), None);
        assert_eq!(ix.confirmed_window(100, 40), None);
        // Partial progress resumes just past the cursor.
        assert_eq!(ix.confirmed_window(100, 10), Some((11, 36)));
        // max_range still caps a large confirmed span.
        assert_eq!(ix.confirmed_window(10_000, 0), Some((1, 1_000)));
    }

    /// BLOCKER 1 — depth 0 disables the guard (indexes up to `latest`); kept as
    /// an explicit escape-hatch contract so a future refactor can't silently
    /// change it.
    #[test]
    fn confirmed_window_zero_depth_tracks_head() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let ix = test_indexer(store, 0);
        assert_eq!(ix.confirmed_window(10, 0), Some((1, 10)));
    }

    /// BLOCKER 1 (reorg regression) — evidence from a NON-FINAL L1 block must
    /// NOT authorize a strict-mode GER injection; once the block is
    /// `confirmations`-deep it does. Drives the REAL `poll_once` with a mock L1:
    /// while the pair's block is within the confirmation window the indexer
    /// records nothing and `ensure_ger_l1_observed` (strict) fail-closes; after
    /// the head advances past the depth the pair is recorded and the gate
    /// authorizes it. This is what makes a short-lived reorg unable to leave a
    /// stale "observed" row that permanently authorizes an irreversible inject.
    ///
    /// Mutation check: setting `confirmations` to 0 (or removing the
    /// `confirmed_window` clamp) makes phase 1 record the pair and the strict
    /// gate wrongly authorize the unconfirmed GER — this test fails.
    #[tokio::test]
    async fn h6_nonfinal_evidence_not_authorized_until_confirmed() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let indexer = test_indexer(store.clone(), 64);

        let mainnet = B256::from([0x0Au8; 32]);
        let rollup = B256::from([0x0Bu8; 32]);
        let ger = combined_ger(&mainnet.0, &rollup.0);
        let tx = TxHash::from([0x01u8; 32]);

        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let mut last_processed = 0u64;

        // Phase 1 — L1 head 10, depth 64: the pair's block (8) is not final.
        // poll_once fetches only the head, records nothing, cursor stays put.
        asserter.push_success(&U64::from(10u64));
        indexer
            .poll_once(&provider, &mut last_processed)
            .await
            .unwrap();
        assert_eq!(last_processed, 0, "no confirmed range → cursor unchanged");
        assert!(
            store.get_ger_entry(&ger).await.unwrap().is_none(),
            "non-final evidence must NOT be recorded"
        );
        let refused = crate::ger::ensure_ger_l1_observed(&store, &ger, true, tx).await;
        let err = refused.expect_err("strict gate must refuse an unconfirmed GER");
        assert!(
            err.to_string().contains("not observed on L1"),
            "must cite L1 non-observation: {err:#}"
        );

        // Phase 2 — head advances to 100: block 8 is now 92 deep (> 64). The
        // confirmed window [1, 36] is fetched, the pair recorded, gate passes.
        asserter.push_success(&U64::from(100u64));
        asserter.push_success(&vec![pair_log(mainnet, rollup, 8)]);
        asserter.push_success(&Option::<serde_json::Value>::None); // block ts → 0
        indexer
            .poll_once(&provider, &mut last_processed)
            .await
            .unwrap();
        assert_eq!(last_processed, 36, "cursor advances to the confirmed head");
        let entry = store
            .get_ger_entry(&ger)
            .await
            .unwrap()
            .expect("confirmed evidence must be recorded");
        assert!(
            entry.mainnet_exit_root.is_some() && entry.rollup_exit_root.is_some(),
            "both roots must be resolved once confirmed"
        );
        crate::ger::ensure_ger_l1_observed(&store, &ger, true, tx)
            .await
            .expect("strict gate must authorize a confirmed GER");
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
        let store: Arc<dyn Store> = Arc::new(FailingGerStore::new());
        let indexer = test_indexer(store, 64);

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

    /// A Store that fails only `set_ger_exit_roots` (the indexer's durable
    /// evidence write) and delegates everything else to a real InMemoryStore.
    /// Used by `h6_evidence_write_failure_leaves_batch_retryable`.
    struct FailingGerStore {
        inner: InMemoryStore,
    }
    impl FailingGerStore {
        fn new() -> Self {
            Self {
                inner: InMemoryStore::new(),
            }
        }
    }
    #[async_trait::async_trait]
    impl Store for FailingGerStore {
        async fn set_ger_exit_roots(
            &self,
            _ger: &[u8; 32],
            _mainnet_exit_root: [u8; 32],
            _rollup_exit_root: [u8; 32],
            _l1_block_number: u64,
            _l1_timestamp: u64,
        ) -> anyhow::Result<()> {
            anyhow::bail!("injected fault: durable evidence write failed")
        }
        async fn get_latest_block_number(&self) -> anyhow::Result<u64> {
            self.inner.get_latest_block_number().await
        }
        async fn set_latest_block_number(&self, n: u64) -> anyhow::Result<()> {
            self.inner.set_latest_block_number(n).await
        }
        async fn advance_block_number(&self) -> anyhow::Result<u64> {
            self.inner.advance_block_number().await
        }
        async fn add_log(&self, log: SyntheticLog) -> anyhow::Result<()> {
            self.inner.add_log(log).await
        }
        async fn get_logs(
            &self,
            filter: &LogFilter,
            current_block: u64,
        ) -> anyhow::Result<Vec<SyntheticLog>> {
            self.inner.get_logs(filter, current_block).await
        }
        async fn get_logs_for_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<SyntheticLog>> {
            self.inner.get_logs_for_tx(tx_hash).await
        }
        async fn has_seen_ger(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
            self.inner.has_seen_ger(ger).await
        }
        async fn mark_ger_seen(&self, ger: &[u8; 32], entry: GerEntry) -> anyhow::Result<bool> {
            self.inner.mark_ger_seen(ger, entry).await
        }
        async fn get_latest_ger(&self) -> anyhow::Result<Option<[u8; 32]>> {
            self.inner.get_latest_ger().await
        }
        async fn get_ger_entry(&self, ger: &[u8; 32]) -> anyhow::Result<Option<GerEntry>> {
            self.inner.get_ger_entry(ger).await
        }
        async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
            self.inner.is_ger_injected(ger).await
        }
        async fn commit_ger_event_atomic(
            &self,
            block_number: u64,
            block_hash: [u8; 32],
            tx_hash: &str,
            global_exit_root: &[u8; 32],
            mainnet_exit_root: Option<[u8; 32]>,
            rollup_exit_root: Option<[u8; 32]>,
            timestamp: u64,
        ) -> anyhow::Result<()> {
            self.inner
                .commit_ger_event_atomic(
                    block_number,
                    block_hash,
                    tx_hash,
                    global_exit_root,
                    mainnet_exit_root,
                    rollup_exit_root,
                    timestamp,
                )
                .await
        }
        async fn txn_begin(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<()> {
            self.inner.txn_begin(tx_hash, entry).await
        }
        async fn txn_commit(
            &self,
            tx_hash: TxHash,
            result: Result<(), String>,
            block_num: u64,
            block_hash: [u8; 32],
        ) -> anyhow::Result<()> {
            self.inner
                .txn_commit(tx_hash, result, block_num, block_hash)
                .await
        }
        async fn txn_receipt(
            &self,
            tx_hash: TxHash,
        ) -> anyhow::Result<Option<(Result<(), String>, u64)>> {
            self.inner.txn_receipt(tx_hash).await
        }
        async fn txn_get(&self, tx_hash: TxHash) -> anyhow::Result<Option<TxnData>> {
            self.inner.txn_get(tx_hash).await
        }
        async fn txn_pending_by_miden_id(
            &self,
            id: TransactionId,
        ) -> anyhow::Result<Option<TxHash>> {
            self.inner.txn_pending_by_miden_id(id).await
        }
        async fn txn_commit_pending(
            &self,
            ids: &[TransactionId],
            block_num: u64,
            block_hash: [u8; 32],
        ) -> anyhow::Result<()> {
            self.inner
                .txn_commit_pending(ids, block_num, block_hash)
                .await
        }
        async fn txn_expire_pending(
            &self,
            block_num: u64,
            block_hash: [u8; 32],
        ) -> anyhow::Result<()> {
            self.inner.txn_expire_pending(block_num, block_hash).await
        }
        async fn nonce_get(&self, addr: &str) -> anyhow::Result<u64> {
            self.inner.nonce_get(addr).await
        }
        async fn nonce_increment(&self, addr: &str) -> anyhow::Result<u64> {
            self.inner.nonce_increment(addr).await
        }
        async fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
            self.inner.try_claim(global_index).await
        }
        async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()> {
            self.inner.unclaim(global_index).await
        }
        async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool> {
            self.inner.is_claimed(global_index).await
        }
        async fn try_reclaim_expired(
            &self,
            global_index: U256,
            ttl: std::time::Duration,
        ) -> anyhow::Result<bool> {
            self.inner.try_reclaim_expired(global_index, ttl).await
        }
        async fn record_unclaimable_claim(&self, entry: UnclaimableClaim) -> anyhow::Result<bool> {
            self.inner.record_unclaimable_claim(entry).await
        }
        async fn get_unclaimable_claim(
            &self,
            global_index: &U256,
        ) -> anyhow::Result<Option<UnclaimableClaim>> {
            self.inner.get_unclaimable_claim(global_index).await
        }
        async fn get_address_mapping(&self, eth: &Address) -> anyhow::Result<Option<AccountId>> {
            self.inner.get_address_mapping(eth).await
        }
        async fn set_address_mapping(&self, eth: Address, miden: AccountId) -> anyhow::Result<()> {
            self.inner.set_address_mapping(eth, miden).await
        }
        async fn is_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
            self.inner.is_note_processed(note_id).await
        }
        async fn get_deposit_count(&self) -> anyhow::Result<u64> {
            self.inner.get_deposit_count().await
        }
        async fn commit_b2agg_event_atomic(
            &self,
            note_id: String,
            bridge_address: &str,
            block_number: u64,
            block_hash: [u8; 32],
            tx_hash: &str,
            leaf_type: u8,
            origin_network: u32,
            origin_address: &[u8; 20],
            destination_network: u32,
            destination_address: &[u8; 20],
            amount: u128,
            metadata: &[u8],
        ) -> anyhow::Result<u32> {
            self.inner
                .commit_b2agg_event_atomic(
                    note_id,
                    bridge_address,
                    block_number,
                    block_hash,
                    tx_hash,
                    leaf_type,
                    origin_network,
                    origin_address,
                    destination_network,
                    destination_address,
                    amount,
                    metadata,
                )
                .await
        }
        async fn is_claim_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
            self.inner.is_claim_note_processed(note_id).await
        }
        async fn mark_claim_note_processed(
            &self,
            note_id: String,
            global_index: [u8; 32],
            block_number: u64,
        ) -> anyhow::Result<()> {
            self.inner
                .mark_claim_note_processed(note_id, global_index, block_number)
                .await
        }
        async fn has_claim_event_for_global_index(
            &self,
            global_index: &[u8; 32],
        ) -> anyhow::Result<bool> {
            self.inner
                .has_claim_event_for_global_index(global_index)
                .await
        }
        async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()> {
            self.inner.register_faucet(entry).await
        }
        async fn get_faucet_by_origin(
            &self,
            origin_address: &[u8; 20],
            origin_network: u32,
        ) -> anyhow::Result<Option<FaucetEntry>> {
            self.inner
                .get_faucet_by_origin(origin_address, origin_network)
                .await
        }
        async fn get_faucet_by_id(
            &self,
            faucet_id: AccountId,
        ) -> anyhow::Result<Option<FaucetEntry>> {
            self.inner.get_faucet_by_id(faucet_id).await
        }
        async fn list_faucets(&self) -> anyhow::Result<Vec<FaucetEntry>> {
            self.inner.list_faucets().await
        }
    }
}
