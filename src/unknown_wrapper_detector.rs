//! Unknown bridge-out wrapper detection — Cantina MA#4 monitor.
//!
//! Cantina MA#4 reports that `BridgeOutScanner` only treats notes whose script
//! root matches `B2AggNote::script_root()` as bridge-out signals. The bridge
//! account, however, has no on-chain restriction on which script bodies it
//! will accept — any note that calls `bridge_out::bridge_out` from inside a
//! transaction the bridge consumes will advance the on-chain LET frontier and
//! BURN funds. If an attacker (or any future legitimate but unrecognised
//! wrapper) crafts an alternate MASM script that does this, the bridge
//! consumes it, the LET advances, BURN is emitted — but aggkit's indexer
//! silently filters the note out at `is_b2agg_note`, never emits a synthetic
//! `BridgeEvent`, and the assets are effectively burned on L2 with no
//! corresponding L1 exit ticket.
//!
//! We cannot prevent the on-chain consumption (the gate would have to live in
//! the bridge MASM, not in aggkit). What we CAN do is detect it: every note
//! consumed by `bridge_account_id` whose script root is NOT in the set of
//! known-legitimate roots is an "unknown wrapper" candidate. Emit a
//! `bridge_unknown_wrapper_consumed_total` counter and a structured `warn`
//! log naming the unknown script root so an operator can investigate before
//! more funds are stranded.
//!
//! The set of "known-legitimate roots consumed by the bridge" is two:
//!   * `B2AggNote::script_root()` — the canonical bridge-out wrapper.
//!   * `miden_base_agglayer::claim_script().root()` — CLAIM notes are
//!     consumed by the bridge as a precondition to MINT emission (bridge-IN,
//!     not bridge-OUT, but still a legitimate bridge-account consumption).
//!
//! Anything else consumed by the bridge is the MA#4 signature.
//!
//! This module exposes:
//! - The pure predicate `classify_bridge_consumer_script` so the detection
//!   logic is unit-testable in isolation.
//! - The `BridgeConsumerScript` enum the caller uses to drive alerts.
//!
//! The actual periodic check lives in `bridge_out.rs::on_post_sync`: after
//! the existing per-note loop, every note whose `consumer_account ==
//! bridge_account_id` AND whose script root is unknown increments
//! `bridge_unknown_wrapper_consumed_total` and logs at `warn`.

/// Outcome of checking a single bridge-account-consumed note's script root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeConsumerScript {
    /// Note is a recognised B2AGG bridge-out wrapper. Normal flow.
    KnownB2Agg,
    /// Note is a recognised CLAIM consumed by the bridge to trigger a MINT.
    /// Normal flow (bridge-IN, not bridge-OUT, but legitimate bridge
    /// consumption).
    KnownClaim,
    /// Note was consumed by the bridge account but its script root is in
    /// neither set above — the Cantina MA#4 signature. Operator must
    /// investigate before the LET advance becomes permanent.
    Unknown,
}

/// Classify a bridge-account-consumed note's script root.
///
/// Pure (no I/O, no metrics) so it can be unit-tested directly. The caller
/// in `bridge_out::on_post_sync` first filters to notes where
/// `consumer_account == bridge_account_id`; this predicate then decides
/// whether the script root is known.
///
/// `known_b2agg_root` and `known_claim_root` are passed in (rather than
/// computed inside) so the unit tests can pin the predicate against
/// well-known fixed values without depending on the upstream MASM crate's
/// behaviour at test time.
pub fn classify_bridge_consumer_script(
    observed_root: [u8; 32],
    known_b2agg_root: [u8; 32],
    known_claim_root: [u8; 32],
) -> BridgeConsumerScript {
    if observed_root == known_b2agg_root {
        BridgeConsumerScript::KnownB2Agg
    } else if observed_root == known_claim_root {
        BridgeConsumerScript::KnownClaim
    } else {
        BridgeConsumerScript::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cantina MA#4 — repro+regression. The detection predicate must pin
    /// three states cleanly:
    /// - `KnownB2Agg`: observed root matches the canonical B2AGG wrapper.
    /// - `KnownClaim`: observed root matches the CLAIM script consumed by
    ///   the bridge to trigger MINTs.
    /// - `Unknown`: anything else — the MA#4 signature.
    ///
    /// Pre-fix this predicate did not exist; `BridgeOutScanner` filtered all
    /// non-B2AGG notes silently in `is_b2agg_note`, so an alternate wrapper
    /// that called `bridge_out::bridge_out` from a different script body
    /// would advance the on-chain LET with no aggkit observation.
    #[test]
    fn ma4_classify_bridge_consumer_script_branches() {
        let b2agg = [0xAAu8; 32];
        let claim = [0xBBu8; 32];

        // 1. Known B2AGG — canonical bridge-out wrapper.
        assert_eq!(
            classify_bridge_consumer_script(b2agg, b2agg, claim),
            BridgeConsumerScript::KnownB2Agg
        );

        // 2. Known CLAIM — bridge consumes CLAIM to mint.
        assert_eq!(
            classify_bridge_consumer_script(claim, b2agg, claim),
            BridgeConsumerScript::KnownClaim
        );

        // 3. Unknown — the MA#4 signature. Any other 32-byte script root
        // consumed by the bridge is an invisible exit risk.
        let foreign = [0xCCu8; 32];
        assert_eq!(
            classify_bridge_consumer_script(foreign, b2agg, claim),
            BridgeConsumerScript::Unknown
        );

        // Edge: all zeros — also Unknown unless one of the known roots
        // happens to be all-zero (it never is for non-trivial MASM).
        assert_eq!(
            classify_bridge_consumer_script([0u8; 32], b2agg, claim),
            BridgeConsumerScript::Unknown
        );
    }

    /// Pin that the predicate doesn't accidentally collapse the two known
    /// roots into one bucket. If B2AGG and CLAIM ever shared a script root
    /// (they don't; the MASM bodies are distinct) this predicate would
    /// still need to distinguish them so the metric label is correct.
    #[test]
    fn ma4_classify_distinguishes_b2agg_from_claim() {
        let b2agg = [0x01u8; 32];
        let claim = [0x02u8; 32];
        assert_eq!(
            classify_bridge_consumer_script(b2agg, b2agg, claim),
            BridgeConsumerScript::KnownB2Agg
        );
        assert_eq!(
            classify_bridge_consumer_script(claim, b2agg, claim),
            BridgeConsumerScript::KnownClaim
        );
        // Swap argument order to ensure we don't depend on positional luck.
        assert_eq!(
            classify_bridge_consumer_script(b2agg, claim, b2agg),
            BridgeConsumerScript::KnownClaim,
            "predicate must classify against the role each root plays, \
             not its argument position"
        );
    }

    /// If a future bridge variant adds a third legitimate script root (e.g.
    /// a separate "force-claim" wrapper), this predicate WILL flag it as
    /// `Unknown` until the call site is updated to pass a third known root
    /// and the predicate is extended. That's intentional — fail-loud is
    /// better than silent acceptance of unrecognised wrappers, which is
    /// exactly the MA#4 condition.
    #[test]
    fn ma4_new_wrapper_must_be_explicitly_recognised() {
        let b2agg = [0xAAu8; 32];
        let claim = [0xBBu8; 32];
        let new_wrapper = [0xDDu8; 32];
        assert_eq!(
            classify_bridge_consumer_script(new_wrapper, b2agg, claim),
            BridgeConsumerScript::Unknown,
            "any unrecognised wrapper consumed by the bridge MUST be flagged"
        );
    }
}
