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
    use miden_protocol::{Felt, Word};

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
        let bridge = aid("0xac0000000000dd110000ee000000fc");
        let attacker = aid("0xaa0000000000bc110000bc000000de");

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
        assert_eq!(check_faucet_owner(bridge, None), OwnershipState::Renounced);
    }

    /// Cantina #4, the half the predicate tests cannot reach: the OBSERVED
    /// owner has to actually come out of account storage. The predicate above
    /// is handed an `Option<AccountId>` already decoded, so it stays green even
    /// if the decode is completely broken — and a monitor that cannot read the
    /// slot reports exactly the same "no drift" as a healthy one.
    ///
    /// This pins the decode itself against a faucet built by the real 0.16
    /// builder, so a storage-layout or code-commitment change upstream breaks a
    /// test instead of silently blinding the monitor in production.
    #[test]
    fn cantina_4_owner_decodes_from_real_0_16_faucet_storage() {
        let bridge = aid("0xac0000000000dd110000ee000000fc");
        let faucet = miden_base_agglayer::create_agglayer_faucet(
            Word::from([1u32, 2, 3, 4]),
            "TST",
            8,
            Felt::new(1_000_000).unwrap(),
            bridge,
        );

        let observed = miden_base_agglayer::AggLayerFaucet::owner_account_id(&faucet)
            .expect("0.16 faucet storage must decode to an owner");
        assert_eq!(
            observed, bridge,
            "the builder sets Ownable2Step to the bridge; the decode must round-trip it"
        );
        assert_eq!(
            check_faucet_owner(bridge, Some(observed)),
            OwnershipState::Expected,
            "a freshly built bridge-owned faucet must read as healthy end to end"
        );
    }

    /// A NATIVE operator faucet legitimately has no `Ownable2Step` slots, so the
    /// AggLayer decode fails on it — by design, not by breakage. The monitor
    /// therefore cannot treat "decode failed" as "nothing to see": it must
    /// classify first, or it buries a genuinely broken AggLayer decode in the
    /// same silent skip it uses for every native faucet.
    #[test]
    fn cantina_4_native_faucet_is_not_decodable_and_must_be_classified() {
        use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
        use miden_protocol::asset::{AssetAmount, TokenSymbol};
        use miden_standards::account::auth::{Approver, AuthSingleSig};
        use miden_standards::account::faucets::{FungibleFaucet, TokenName};

        let native = FungibleFaucet::builder()
            .name(TokenName::new("native").unwrap())
            .symbol(TokenSymbol::try_from("NAT").unwrap())
            .decimals(8)
            .max_supply(AssetAmount::from(1_000_000u32))
            .build()
            .expect("native faucet component");
        // Same auth scheme the proxy deploys with; irrelevant to the decode but
        // the builder requires one.
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let account = miden_protocol::account::AccountBuilder::new([7u8; 32])
            .with_component(native)
            .with_auth_component(AuthSingleSig::new(Approver::new(
                key.public_key().to_commitment(),
                AuthScheme::Falcon512Poseidon2,
            )))
            .build()
            .expect("native faucet account");

        assert!(
            miden_base_agglayer::AggLayerFaucet::owner_account_id(&account).is_err(),
            "a native faucet has no AggLayer ownership slots — the decode must fail"
        );
        assert_eq!(
            crate::faucet_ops::classify_faucet_account(&account)
                .expect("a native faucet is a SUPPORTED kind")
                .0,
            crate::faucet_ops::FaucetKind::NativeFungible,
            "classification is what distinguishes 'not applicable' from 'broken'"
        );
    }

    /// The expected bridge is opaque — even if it equals the attacker's
    /// account, the predicate trusts the configuration. (No
    /// "expected == attacker" silliness because the operator's input is
    /// what we're comparing against.)
    #[test]
    fn cantina_4_predicate_trusts_configured_bridge() {
        let bridge = aid("0xac0000000000dd110000ee000000fc");
        // If somehow the configured bridge equals the observed owner, no alert.
        assert_eq!(
            check_faucet_owner(bridge, Some(bridge)),
            OwnershipState::Expected
        );
    }
}
