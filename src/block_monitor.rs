//! RD-940 Phase 3 — BlockMonitor (minimum-viable).
//!
//! This module is the **single-writer / single-reader surface** the spec
//! (`docs/design/RD-940-async-writer.md` §2.3) ultimately wants for synthetic
//! block-tip tracking. The full structural absorption of `BlockState` +
//! `StoreSyncListener` + the inline log emitters is deferred to a focused
//! follow-up PR (see the "Phase 3 deferred" note in the design doc).
//!
//! What lands here, today:
//!
//! - An `AtomicU64` tip mirror behind which the JSON-RPC read path
//!   (`eth_blockNumber`) can hot-read without touching the store. Cuts a
//!   stable-state per-request RTT down to a single relaxed-atomic load.
//! - A `BlockMonitor::record_tip(block_num)` write API that every site
//!   currently calling `store.set_latest_block_number(...)` or relying on a
//!   subsequent `commit_*_atomic` bump can call to keep the mirror in sync.
//! - Stale-low / stale-high safety: `record_tip` uses `fetch_max`, so the
//!   atomic monotonically advances. A reader sees a value strictly bounded
//!   by what has reached `store.set_latest_block_number` — never higher.
//!
//! What is NOT done here (deferred per design doc):
//!
//! - Absorbing the inline emitters in `claim.rs`, `ger.rs`, `bridge_out.rs`,
//!   `log_synthesis.rs` into a single `record(BlockEvent)` writer that
//!   enforces the log-first/cursor-second ordering structurally rather than
//!   via tribal-knowledge comments. The atomic-store helpers
//!   (`commit_ger_event_atomic`, `commit_manual_claim_event_atomic`,
//!   `commit_b2agg_event_atomic`) already enforce that ordering at the
//!   storage layer; centralising at the *call site* requires touching ~9
//!   files and is the next PR's scope.
//! - Deleting `StoreSyncListener` and `BlockState::on_sync`.
//! - Owning the synthetic-block header cache (today `BlockState` does;
//!   `BlockMonitor` only mirrors the tip).
//!
//! This split is deliberate: ship the hot-read fast-path now (zero
//! regression risk), ship the atomic-swap consolidation as its own
//! reviewable PR.

use crate::block_state::BlockState;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Centralised tip-cache + read surface for synthetic block state.
///
/// `BlockMonitor` is registered on `ServiceState` and shared via `Arc` across
/// every dispatcher clone. Its current responsibilities are minimal — see
/// the module docstring for the deferred work — but the API surface is the
/// one the Phase-3-follow-up PR will extend.
pub struct BlockMonitor {
    /// Reference to the existing `BlockState` (synthetic-block header cache,
    /// hash↔number map, SyncListener integration). Until the Phase-3 atomic
    /// swap, `BlockMonitor` does *not* own this state; it merely holds a
    /// handle so future migration is additive.
    block_state: Arc<BlockState>,
    /// Tip mirror — the highest block number any writer has reported via
    /// `record_tip`. `fetch_max` ensures monotonic advancement; the value is
    /// always ≤ what's reached `store.set_latest_block_number`, never above.
    tip: AtomicU64,
}

impl BlockMonitor {
    pub fn new(block_state: Arc<BlockState>) -> Self {
        Self {
            block_state,
            tip: AtomicU64::new(0),
        }
    }

    /// Cheap read of the current synthetic-block tip — backs
    /// `eth_blockNumber` without a store round-trip.
    ///
    /// Returns `0` until a writer has called `record_tip` at least once.
    /// Callers should fall back to `store.get_latest_block_number()` when
    /// the cached value is 0 to handle the cold-boot window.
    pub fn current_tip(&self) -> u64 {
        self.tip.load(Ordering::Relaxed)
    }

    /// Update the tip mirror. Idempotent under monotonic-bump semantics —
    /// `fetch_max` means a stale-low report is a no-op, and a higher value
    /// always wins. Must be called *after* the underlying store write
    /// completes so a reader can never see a tip beyond what the store has
    /// committed (stale-high is forbidden; stale-low is safe and the reader
    /// will recover on the next call).
    pub fn record_tip(&self, block_num: u64) {
        self.tip.fetch_max(block_num, Ordering::Relaxed);
    }

    /// Access the underlying `BlockState` — read-only borrow, kept for the
    /// transition window while writers still call into `BlockState`
    /// directly. The full absorption lands in the Phase-3 follow-up PR.
    pub fn block_state(&self) -> &Arc<BlockState> {
        &self.block_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_state::BlockState;
    use std::sync::Arc;

    #[test]
    fn current_tip_starts_at_zero() {
        let bm = BlockMonitor::new(Arc::new(BlockState::new()));
        assert_eq!(bm.current_tip(), 0);
    }

    #[test]
    fn record_tip_is_monotonic() {
        let bm = BlockMonitor::new(Arc::new(BlockState::new()));
        bm.record_tip(5);
        assert_eq!(bm.current_tip(), 5);
        bm.record_tip(3);
        // Stale-low report is a no-op; the higher value sticks.
        assert_eq!(bm.current_tip(), 5);
        bm.record_tip(10);
        assert_eq!(bm.current_tip(), 10);
    }

    /// `fetch_max` semantics across threads — 32 concurrent writers, each
    /// reporting a distinct random block number; the final tip must equal
    /// the global max, never less.
    #[tokio::test]
    async fn record_tip_concurrent_writers_converge_to_max() {
        let bm = Arc::new(BlockMonitor::new(Arc::new(BlockState::new())));
        let mut handles = Vec::new();
        for i in 1u64..=32 {
            let bm = bm.clone();
            handles.push(tokio::spawn(async move {
                bm.record_tip(i * 7);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(bm.current_tip(), 32 * 7);
    }
}
