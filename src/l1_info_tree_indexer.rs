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
//!     `commit_ger_event_atomic` then does `ON CONFLICT DO NOTHING` and
//!     preserves the indexer's roots.
//!   - `insert_ger` fires before indexer → entry exists with `None` roots.
//!     Indexer's UPSERT fills them in. Bridge-service's polling eventually
//!     re-queries `zkevm_getExitRootsByGER` and gets resolved roots.
//!
//! Either ordering converges to a resolved entry. No race window.

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use sha3::{Digest, Keccak256};
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
        let head = provider.get_block_number().await?;
        if head <= *last_processed {
            return Ok(());
        }

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

        let mut indexed = 0usize;
        for log in logs {
            match self.process_log(&log).await {
                Ok(true) => indexed += 1,
                Ok(false) => {}
                Err(e) => {
                    // Don't fail the whole batch on one bad event; advance the
                    // cursor anyway so we don't get stuck retrying the same
                    // poison log forever.
                    tracing::warn!(
                        error = %e,
                        block = log.block_number.unwrap_or(0),
                        tx = ?log.transaction_hash,
                        "L1InfoTreeIndexer: failed to index log"
                    );
                    metrics::counter!("l1_info_tree_indexer_log_errors_total").increment(1);
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

    async fn process_log(&self, log: &Log) -> anyhow::Result<bool> {
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
            .set_ger_exit_roots(&combined, mainnet, rollup)
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
            block = log.block_number.unwrap_or(0),
            "L1InfoTreeIndexer: indexed exit-root pair"
        );
        Ok(true)
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
}
