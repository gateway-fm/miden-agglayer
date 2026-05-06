//! Forged-MINT detector — Cantina #4 monitor.
//!
//! Cantina #4 reports that a NoAuth bridge + bridge-as-owner faucet
//! combination lets an attacker author a forged note whose `sender = bridge`
//! WITHOUT going through `bridge_in::claim`. The faucet's `owner_only`
//! check passes (sender == owner == bridge), so the faucet executes
//! `mint_and_send` and produces wrapped tokens for the attacker. Forged
//! mints bypass claim proof / GER check / registry lookup / amount
//! verification entirely.
//!
//! The aggkit-side defense is reconciliation: every legitimate MINT note
//! is preceded by a successful aggkit-mediated `claimAsset` that lands in
//! `claimed_indices`. A MINT note observed on-chain whose corresponding
//! globalIndex (or expected NoteId derivable from the claim) is NOT in
//! aggkit's record is forged.
//!
//! This module exposes the predicate. The wiring (extracting expected
//! claim NoteIds from observed MINTs and querying `claimed_indices`)
//! lives in `bridge_out::on_post_sync` (separate commit).

/// Outcome of a single MINT-vs-claim reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintAttribution {
    /// MINT corresponds to a legitimate aggkit-recorded claim. Healthy.
    Recognised,
    /// MINT does NOT match any aggkit-recorded claim. Forged signature
    /// per Cantina #4. Page critical and freeze claim processing.
    Forged,
}

/// Reconcile an observed MINT note against aggkit's claim history.
///
/// `mint_corresponds_to_claim` is the result of the caller's lookup —
/// typically `store.is_claimed(globalIndex)` or
/// `store.has_claim_with_expected_mint_note_id(...)`. Returns the alert
/// kind based purely on that boolean, so the predicate is unit-testable
/// without a Store mock.
pub fn classify_observed_mint(mint_corresponds_to_claim: bool) -> MintAttribution {
    if mint_corresponds_to_claim {
        MintAttribution::Recognised
    } else {
        MintAttribution::Forged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cantina #4 — repro+regression. The predicate maps the boolean
    /// "does this MINT correspond to a recorded claim?" to the alert kind.
    /// Trivial in shape but the contract is load-bearing for the wiring
    /// commit: `false` → critical alert, `true` → no alert.
    #[test]
    fn cantina_4_forged_mint_predicate() {
        assert_eq!(classify_observed_mint(true), MintAttribution::Recognised);
        assert_eq!(classify_observed_mint(false), MintAttribution::Forged);
    }
}
