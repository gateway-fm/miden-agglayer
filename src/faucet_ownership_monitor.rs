//! Faucet ownership drift monitor — Cantina #4 monitor.
//!
//! Cantina #4 reports that the bridge is deployed with `NoAuth` and faucets
//! are deployed with `Ownable2Step` whose owner is the bridge. Combined with
//! the kernel asymmetry that allows `output_note_create` from a NoAuth
//! account, an attacker can author a forged note whose `sender = bridge`
//! and call `transfer_ownership` on a faucet — taking it over.
//!
//! The aggkit-side defense is detection: periodically read each registered
//! faucet's `owner` storage slot via FPI and compare to the expected bridge
//! AccountId. Drift = takeover signature → page critical.
//!
//! This module exposes the predicate. The wiring (periodic FPI read) lives
//! in `bridge_out::on_post_sync` (separate commit).

use miden_protocol::account::AccountId;

/// Outcome of a single faucet-owner check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipState {
    /// Owner matches the configured bridge. Healthy.
    Expected,
    /// Owner is set to a non-bridge account. Cantina #4 takeover signature.
    Drift {
        observed: AccountId,
        expected: AccountId,
    },
    /// Owner has been renounced (set to zero / no-owner). The faucet's
    /// `mint_and_send` will permanently reject every future mint.
    /// Cantina #4 DoS variant.
    Renounced,
}

/// A no-owner sentinel. The Ownable2Step contract uses `AccountId::ZERO`-
/// equivalent to mean "renounced". We compare against an opaque expected
/// bridge id; if `observed` is `None` the owner has been cleared.
pub fn check_faucet_owner(
    expected_bridge: AccountId,
    observed_owner: Option<AccountId>,
) -> OwnershipState {
    match observed_owner {
        None => OwnershipState::Renounced,
        Some(o) if o == expected_bridge => OwnershipState::Expected,
        Some(o) => OwnershipState::Drift {
            observed: o,
            expected: expected_bridge,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid(hex: &str) -> AccountId {
        AccountId::from_hex(hex).unwrap()
    }

    /// Cantina #4 — repro+regression. The predicate must distinguish three
    /// states cleanly:
    /// - Expected: owner matches the configured bridge
    /// - Drift: owner is some non-bridge account (takeover via forged
    ///   `transfer_ownership` note)
    /// - Renounced: owner has been cleared (DoS — faucet can never mint)
    #[test]
    fn cantina_4_faucet_ownership_predicate() {
        let bridge = aid("0x3d7c9747558851900f8206226dfbea");
        let attacker = aid("0x3d7c9747558851900f8206226dfbeb");

        // Healthy.
        assert_eq!(
            check_faucet_owner(bridge, Some(bridge)),
            OwnershipState::Expected
        );

        // Drift — attacker took over.
        match check_faucet_owner(bridge, Some(attacker)) {
            OwnershipState::Drift { observed, expected } => {
                assert_eq!(observed, attacker);
                assert_eq!(expected, bridge);
            }
            other => panic!("expected Drift, got {other:?}"),
        }

        // Renounced — DoS variant.
        assert_eq!(
            check_faucet_owner(bridge, None),
            OwnershipState::Renounced
        );
    }

    /// The expected bridge is opaque — even if it equals the attacker's
    /// account, the predicate trusts the configuration. (No
    /// "expected == attacker" silliness because the operator's input is
    /// what we're comparing against.)
    #[test]
    fn cantina_4_predicate_trusts_configured_bridge() {
        let bridge = aid("0x3d7c9747558851900f8206226dfbea");
        // If somehow the configured bridge equals the observed owner, no alert.
        assert_eq!(
            check_faucet_owner(bridge, Some(bridge)),
            OwnershipState::Expected
        );
    }
}
