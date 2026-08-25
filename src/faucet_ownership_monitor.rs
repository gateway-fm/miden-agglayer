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

/// What the monitor OWES a registered faucet, decided from authoritative
/// registration metadata rather than from a decoder that can degrade.
///
/// # Why this cannot come from the decoder (PR #159 review)
///
/// `faucet_ops::classify_faucet_account` tries the AggLayer decoder and, on any
/// failure, falls back to the plain `FungibleFaucet` decoder. An AggLayer-owned
/// faucet whose code commitment or ownership-slot layout drifts fails the first
/// decode while its common fungible portion still decodes — so it classifies as
/// `NativeFungible`, and a monitor that trusts that classification skips
/// ownership verification and records the skip as benign. That is exactly the
/// silent blindness this module exists to prevent, reached by another route.
///
/// Registration metadata does not degrade: `is_native` is derivable as
/// `origin_network == local_network_id` (see `service_admin`'s
/// `admin_register_native_faucet` — native means the token ORIGINATES on this
/// Miden network, so there is no separate column). Decide the duty from that
/// first, then hold the decoder to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipDuty {
    /// Registered as Miden-originated: operator-owned by design, the bridge
    /// never owns it, so there is no ownership invariant to check.
    NotApplicable,
    /// Registered as foreign-origin: this MUST be an AggLayer-owned wrapped
    /// faucet, so its owner must decode. Any failure is a blind spot, never a
    /// benign native skip.
    MustBeAggLayerOwned,
}

/// Decides the duty from authoritative registration metadata.
pub fn ownership_duty(origin_network: u32, local_network_id: u32) -> OwnershipDuty {
    if origin_network == local_network_id {
        OwnershipDuty::NotApplicable
    } else {
        OwnershipDuty::MustBeAggLayerOwned
    }
}

/// What the monitor should do once the account has been classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationVerdict {
    /// Decode the owner and compare it against the bridge.
    Verify,
    /// Registered native: expected, benign, nothing to verify.
    SkipNative,
    /// The monitor cannot vouch for a faucet it is responsible for. Alert.
    Undecodable(&'static str),
}

/// Reconciles the authoritative duty against what the decoder actually said.
///
/// `observed` is `None` when classification failed outright.
pub fn reconcile_classification(
    duty: OwnershipDuty,
    observed: Option<crate::faucet_ops::FaucetKind>,
) -> ClassificationVerdict {
    use crate::faucet_ops::FaucetKind;
    match (duty, observed) {
        // Registered native: the bridge does not own it, so whatever the
        // decoder says, there is no ownership invariant here.
        (OwnershipDuty::NotApplicable, _) => ClassificationVerdict::SkipNative,

        // Registered foreign-origin and it decodes as AggLayer-owned: verify.
        (OwnershipDuty::MustBeAggLayerOwned, Some(FaucetKind::AggLayerOwned)) => {
            ClassificationVerdict::Verify
        }

        // THE COUNTEREXAMPLE the review found. Registered foreign-origin, but
        // the AggLayer decode degraded to the plain fungible view. This used to
        // be filed as a benign native skip; it is actually the monitor going
        // blind on a faucet the bridge is supposed to own.
        (OwnershipDuty::MustBeAggLayerOwned, Some(FaucetKind::NativeFungible)) => {
            ClassificationVerdict::Undecodable(
                "registered as foreign-origin (bridge-owned) but only the plain fungible view \
                 decodes — the AggLayer code commitment or storage layout has drifted",
            )
        }

        // Registered foreign-origin and nothing decodes at all.
        (OwnershipDuty::MustBeAggLayerOwned, None) => ClassificationVerdict::Undecodable(
            "registered as foreign-origin (bridge-owned) but matches no supported faucet type",
        ),
    }
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
        // Same production builder path faucet_ops uses (rc.4): admin is any
        // account (irrelevant to the owner decode), fees are zero against a
        // dummy fee faucet — pure construction, no chain access.
        let admin = aid("0xac0000000000dd110000ee000000ad");
        let fee_faucet = aid("0x9a0000000000dd110000ee000000fc");
        let faucet = miden_base_agglayer::AggLayerFaucet::account_builder(
            Word::from([1u32, 2, 3, 4]),
            "TST",
            8,
            Felt::new(1_000_000).unwrap(),
            Felt::new(0).unwrap(),
            admin,
            bridge,
            crate::fee_policy::zero_fee_policy_manager_for(
                miden_base_agglayer::AggLayerFaucet::allowed_notes(),
                fee_faucet,
            ),
        )
        .build()
        .expect("agglayer faucet account");

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
            .with_component(AuthSingleSig::new(Approver::new(
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

    /// PR #159 blocker, the counterexample my earlier tests missed.
    ///
    /// `classify_faucet_account` falls back to the plain `FungibleFaucet`
    /// decoder whenever the AggLayer decoder fails. So an AggLayer-owned faucet
    /// whose code commitment or ownership-slot layout drifts still classifies as
    /// `NativeFungible` — and the previous monitor filed that under
    /// `unchecked{reason="native_faucet"}`, documented as *expected and benign*,
    /// and skipped ownership verification entirely.
    ///
    /// That is the same silent blindness the Cantina #4 patch set out to remove,
    /// reached by a different route: a faucet the bridge is supposed to own
    /// stops being verified, and the dashboard shows a benign counter rather
    /// than an alert. Deciding the duty from registration metadata — which does
    /// not degrade — is what closes it.
    #[test]
    fn cantina_4_degraded_agglayer_decode_is_never_downgraded_to_native() {
        // Registered foreign-origin (origin_network != ours) ⇒ bridge-owned.
        let duty = ownership_duty(0, 1);
        assert_eq!(duty, OwnershipDuty::MustBeAggLayerOwned);

        // The decoder degraded to the plain fungible view.
        let verdict =
            reconcile_classification(duty, Some(crate::faucet_ops::FaucetKind::NativeFungible));
        match verdict {
            ClassificationVerdict::Undecodable(why) => {
                assert!(
                    why.contains("drifted"),
                    "the alert must name the real cause, got: {why}"
                );
            }
            other => panic!(
                "a registered bridge-owned faucet that only decodes as fungible MUST alert,                  got {other:?} — this is the exact regression PR #159 flagged"
            ),
        }

        // And total decode failure is likewise an alert, not a skip.
        assert!(matches!(
            reconcile_classification(duty, None),
            ClassificationVerdict::Undecodable(_)
        ));
    }

    /// The duty comes from `origin_network == local_network_id`, the same rule
    /// `admin_register_native_faucet` documents as deriving `is_native`. A
    /// genuinely native faucet must still be skipped benignly — otherwise the
    /// fix trades silent blindness for permanent false alarms.
    #[test]
    fn cantina_4_registered_native_faucet_is_still_a_benign_skip() {
        let duty = ownership_duty(1, 1);
        assert_eq!(duty, OwnershipDuty::NotApplicable);
        assert_eq!(
            reconcile_classification(duty, Some(crate::faucet_ops::FaucetKind::NativeFungible)),
            ClassificationVerdict::SkipNative
        );
        // Even if such an account somehow decodes as AggLayer-owned, the bridge
        // does not own a Miden-originated token: still nothing to verify.
        assert_eq!(
            reconcile_classification(duty, Some(crate::faucet_ops::FaucetKind::AggLayerOwned)),
            ClassificationVerdict::SkipNative
        );
    }

    /// A correctly-decoding bridge-owned faucet must still be verified — the
    /// fix must not turn every foreign-origin faucet into an alert.
    #[test]
    fn cantina_4_healthy_bridge_owned_faucet_is_verified() {
        assert_eq!(
            reconcile_classification(
                ownership_duty(0, 1),
                Some(crate::faucet_ops::FaucetKind::AggLayerOwned)
            ),
            ClassificationVerdict::Verify
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
