//! MINT attachment-target monitor — Cantina #2 monitor.
//!
//! Cantina #2 reports that a CLAIM-generated MINT note targets a faucet
//! via `NetworkAccountTarget` attachment, but `miden::standards::notes::mint::main`
//! does NOT enforce that the consuming faucet matches the attachment.
//! Any faucet sharing the same bridge owner satisfies the
//! `assert_sender_is_owner` check, so a MINT built for faucet A can be
//! consumed by faucet B and produce B's wrapped asset for the original
//! claimant. The claimant can then bridge that asset out under B's route,
//! turning a proven `(N1, X)` claim into a withdrawal of `(N0, Y)`.
//!
//! The aggkit-side defense is detection: when we observe a MINT note
//! being consumed by a faucet, we know (a) which faucet consumed it
//! (the consuming account in the tx) and (b) which faucet the MINT was
//! targeted at (decoded from `NetworkAccountTarget`). If (a) != (b),
//! that's the Cantina #2 signature — page critical, freeze claim
//! processing for the affected asset.
//!
//! This module exposes the predicate. Wiring it into the consumed-note
//! observation path (extract MINT notes consumed by faucets, decode the
//! attachment, forward to the predicate) is a separate commit.

use miden_protocol::account::AccountId;

/// Outcome of a single MINT-attachment check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintTargetMatch {
    /// `consuming_faucet == intended_faucet`. Healthy.
    InOrder,
    /// `consuming_faucet != intended_faucet`. Cantina #2 signature.
    Mismatch {
        intended: AccountId,
        consuming: AccountId,
    },
}

/// Compare a MINT note's intended target faucet (decoded from its
/// `NetworkAccountTarget` attachment) against the faucet that consumed
/// it. Returns the alert kind.
pub fn check_mint_attachment(
    intended_faucet: AccountId,
    consuming_faucet: AccountId,
) -> MintTargetMatch {
    if intended_faucet == consuming_faucet {
        MintTargetMatch::InOrder
    } else {
        MintTargetMatch::Mismatch {
            intended: intended_faucet,
            consuming: consuming_faucet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid(hex: &str) -> AccountId {
        AccountId::from_hex(hex).unwrap()
    }

    /// Cantina #2 — repro+regression. Same intended/consuming faucet
    /// returns `InOrder`; different faucets return `Mismatch` carrying
    /// both ids for operator attribution.
    #[test]
    fn cantina_2_mint_target_predicate() {
        let faucet_a = aid("0xac0000000000dd110000ee000000fc");
        let faucet_b = aid("0xaa0000000000bc110000bc000000de");

        // Healthy: A consumed by A.
        assert_eq!(
            check_mint_attachment(faucet_a, faucet_a),
            MintTargetMatch::InOrder
        );

        // Cantina #2 signature: A's MINT consumed by B.
        match check_mint_attachment(faucet_a, faucet_b) {
            MintTargetMatch::Mismatch {
                intended,
                consuming,
            } => {
                assert_eq!(intended, faucet_a);
                assert_eq!(consuming, faucet_b);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }

        // The reverse mismatch is also flagged.
        match check_mint_attachment(faucet_b, faucet_a) {
            MintTargetMatch::Mismatch {
                intended,
                consuming,
            } => {
                assert_eq!(intended, faucet_b);
                assert_eq!(consuming, faucet_a);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }
}
