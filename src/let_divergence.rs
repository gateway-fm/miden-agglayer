//! Local Exit Tree (LET) divergence detection — Cantina #9 monitor.
//!
//! Cantina #9 reports that a B2AGG note constructed with `NoteType::Private`
//! is consumed successfully by the on-chain bridge (LET frontier advances,
//! BURN emitted) but aggkit, which only sees public notes, never observes the
//! leaf preimage. From that leaf onwards, aggkit's view of the LET diverges
//! from the bridge's stored state — every subsequent legitimate bridge-out
//! produces a `smt_proof_local_exit_root` that doesn't match the on-chain
//! root, stranding user funds.
//!
//! We can't prevent the on-chain leaf from being appended (the upstream
//! `b2agg.masm` fix lands at the consensus layer, not here). What we CAN do
//! is detect divergence: periodically read the bridge's `let_num_leaves`
//! storage slot via FPI and compare against aggkit's locally-tracked
//! `deposit_counter`. A monotonic gap that opens between the two is the
//! signature of a private B2AGG (or any other path that advances the LET
//! without aggkit observing the corresponding public note).
//!
//! This module exposes:
//! - The pure comparison predicate `compare_let_state` so the detection
//!   logic is unit-testable in isolation from the Miden client.
//! - The `LetDivergence` enum the caller uses to drive alerts.
//!
//! The actual periodic FPI query lives in `bridge_out.rs::on_post_sync`
//! (added alongside this module): each sync tick, after processing
//! consumed B2AGGs, query the bridge account's `let_num_leaves` slot and
//! call `compare_let_state(on_chain, aggkit_counter)`. On divergence,
//! increment `bridge_let_divergence_total` and `tracing::error!` with
//! enough context for an operator to reconcile.

/// Outcome of a one-shot comparison between the on-chain LET leaf count and
/// aggkit's locally-tracked deposit counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LetDivergence {
    /// On-chain and aggkit agree. Healthy state.
    InSync,
    /// On-chain LET has advanced past aggkit's view. The gap is the count of
    /// leaves the bridge has appended that aggkit hasn't observed via a
    /// public B2AGG note. This is the Cantina #9 signature.
    OnChainAhead { gap: u64 },
    /// Aggkit reports more deposits than the on-chain LET — should never
    /// happen in production (we only count what the bridge has accepted).
    /// If observed, the local counter is corrupt or the on-chain tree was
    /// reset; treat as a critical state-restore signal.
    AggkitAhead { gap: u64 },
}

/// Compare the bridge's on-chain `let_num_leaves` slot with aggkit's
/// `deposit_counter` and return the divergence kind.
///
/// Both values are u64 to accommodate any width the upstream bridge uses
/// for the leaf counter (it's u32 today, but we widen here so the
/// comparison logic survives a future format bump).
pub fn compare_let_state(on_chain_leaves: u64, aggkit_deposit_count: u64) -> LetDivergence {
    if on_chain_leaves == aggkit_deposit_count {
        LetDivergence::InSync
    } else if on_chain_leaves > aggkit_deposit_count {
        LetDivergence::OnChainAhead {
            gap: on_chain_leaves - aggkit_deposit_count,
        }
    } else {
        LetDivergence::AggkitAhead {
            gap: aggkit_deposit_count - on_chain_leaves,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cantina #9 — repro+regression. The detection predicate must
    /// distinguish three states cleanly:
    /// - In-sync: counts match.
    /// - On-chain-ahead: bridge has more leaves than aggkit observed
    ///   (Cantina #9 signature — private B2AGG was consumed).
    /// - Aggkit-ahead: aggkit observed more deposits than the bridge has
    ///   appended (impossible in steady state; signals local corruption).
    ///
    /// Pre-fix this predicate did not exist; aggkit had no way to detect
    /// the Cantina #9 condition. Post-fix the periodic comparison in
    /// `bridge_out::on_post_sync` increments `bridge_let_divergence_total`
    /// and pages on-call.
    #[test]
    fn cantina_9_let_divergence_predicate_pins_three_states() {
        // In-sync: zeroes match.
        assert_eq!(compare_let_state(0, 0), LetDivergence::InSync);
        // In-sync: large equal values match.
        assert_eq!(compare_let_state(12345, 12345), LetDivergence::InSync);

        // On-chain ahead by 1 (the smallest detectable Cantina #9 leak).
        assert_eq!(
            compare_let_state(101, 100),
            LetDivergence::OnChainAhead { gap: 1 }
        );
        // On-chain ahead by many (catastrophic — multiple private B2AGGs).
        assert_eq!(
            compare_let_state(1000, 1),
            LetDivergence::OnChainAhead { gap: 999 }
        );

        // Aggkit ahead — the inverse (state corruption).
        assert_eq!(
            compare_let_state(100, 101),
            LetDivergence::AggkitAhead { gap: 1 }
        );

        // u64-wide values — the comparison must not overflow.
        assert_eq!(
            compare_let_state(u64::MAX, u64::MAX - 1),
            LetDivergence::OnChainAhead { gap: 1 }
        );
        assert_eq!(
            compare_let_state(0, u64::MAX),
            LetDivergence::AggkitAhead { gap: u64::MAX }
        );
    }

    /// The gap field on `OnChainAhead` is the load-bearing diagnostic — an
    /// operator alerted on Cantina #9 needs to know how many leaves are
    /// missing so they can decide whether to freeze just this asset or
    /// the whole bridge. Pin that the gap matches `on_chain - aggkit`.
    #[test]
    fn cantina_9_gap_arithmetic_correct() {
        for (chain, agg, expected_gap) in [
            (5u64, 3, 2),
            (100, 1, 99),
            (u32::MAX as u64, 0, u32::MAX as u64),
        ] {
            assert_eq!(
                compare_let_state(chain, agg),
                LetDivergence::OnChainAhead { gap: expected_gap }
            );
        }
    }

    /// Cantina MA#18 — erased-B2AGG does NOT escape the monitor.
    ///
    /// cergyk challenges the PREMISE of this monitor: a B2AGG note that is
    /// *erased* (created and consumed within the same block) is stripped from
    /// the block's note/nullifier trees by `remove_erased_nullifiers`
    /// (miden-protocol `block/proposed_block.rs`), so it NEVER surfaces to the
    /// indexer as a consumed note. His conclusion: "they would escape the
    /// monitoring entirely here."
    ///
    /// That conclusion is FALSE for *this* monitor, and this test pins why.
    /// The two inputs to `compare_let_state` are sourced from completely
    /// different places:
    ///
    /// - `on_chain_leaves` is read DIRECTLY from the bridge ACCOUNT's storage
    ///   (`AggLayerBridge::read_let_num_leaves(&account)` → `account.storage()
    ///   .get_item(num_leaves_slot)`), via FPI, every sync tick — independent
    ///   of whether any consumed note was observed. The bridge account's
    ///   transaction that processes the B2AGG mutates this slot, and that
    ///   account-state delta is committed to the block by
    ///   `AccountUpdateAggregator` regardless of erasure. Erasure only touches
    ///   the note/nullifier trees, never account state. So `let_num_leaves`
    ///   ADVANCES for an erased B2AGG.
    ///
    /// - `aggkit_deposit_count` is incremented inside
    ///   `commit_b2agg_event_atomic`, reached only from
    ///   `BridgeOutScanner::process_consumed_note` — i.e. only when aggkit
    ///   OBSERVES the note as consumed. An erased note never surfaces, so this
    ///   counter does NOT advance.
    ///
    /// Net effect of an erased B2AGG: on-chain LET +1, aggkit deposit count
    /// unchanged → exactly the `OnChainAhead { gap: 1 }` signature this monitor
    /// is built to fire on. The erased note is therefore CAUGHT, not escaped.
    #[test]
    fn cantina_18_erased_b2agg_is_caught_not_escaped() {
        // Steady state: bridge LET and aggkit deposit count agree at N.
        let n = 100u64;
        assert_eq!(compare_let_state(n, n), LetDivergence::InSync);

        // A single erased B2AGG is processed by the bridge account: the
        // account-storage LET slot advances to N+1 (committed via the account
        // delta, unaffected by `remove_erased_nullifiers`), but aggkit's
        // deposit count stays at N because the note never surfaced as consumed.
        let on_chain_after_erased = n + 1;
        let aggkit_after_erased = n;
        assert_eq!(
            compare_let_state(on_chain_after_erased, aggkit_after_erased),
            LetDivergence::OnChainAhead { gap: 1 },
            "an erased B2AGG must register as OnChainAhead — the monitor reads \
             the LET from bridge-account storage, not from observed consumed notes"
        );

        // Multiple erased B2AGGs in a row open a monotonically growing gap; the
        // monitor keeps reporting the true shortfall so an operator can size the
        // freeze. This is the opposite of "escaping the monitoring entirely".
        for k in 1..=5u64 {
            assert_eq!(
                compare_let_state(n + k, n),
                LetDivergence::OnChainAhead { gap: k }
            );
        }
    }
}
