//! Deterministic claim inclusion checks, before any nonce or note is created.
//! A registered GER alone does not prove that it contains this deposit.

use crate::claim::claimAssetCall;
use alloy::primitives::{B256, keccak256};

fn leaf_hash(call: &claimAssetCall) -> B256 {
    let mut bytes = Vec::with_capacity(113);
    bytes.push(0); // claimAsset, not claimMessage
    bytes.extend_from_slice(&call.originNetwork.to_be_bytes());
    bytes.extend_from_slice(call.originTokenAddress.as_slice());
    bytes.extend_from_slice(&call.destinationNetwork.to_be_bytes());
    bytes.extend_from_slice(call.destinationAddress.as_slice());
    bytes.extend_from_slice(&call.amount.to_be_bytes::<32>());
    bytes.extend_from_slice(keccak256(&call.metadata).as_slice());
    keccak256(bytes)
}

fn root(mut leaf: B256, proof: &[B256; 32], mut index: u32) -> B256 {
    for sibling in proof {
        let mut pair = [0u8; 64];
        let (left, right) = if index & 1 == 0 {
            (leaf, *sibling)
        } else {
            (*sibling, leaf)
        };
        pair[..32].copy_from_slice(left.as_slice());
        pair[32..].copy_from_slice(right.as_slice());
        leaf = keccak256(pair);
        index >>= 1;
    }
    leaf
}

/// Match the bridge's Keccak SMT checks using the original uint256 amount and
/// ABI metadata. Never record a bad proof as an unclaimable deposit: a different
/// transaction can supply a valid proof for the same global index.
pub(crate) fn validate(call: &claimAssetCall) -> anyhow::Result<()> {
    let (index, source) = crate::applied_state::claim_coordinates(call.globalIndex)?;
    let local = root(leaf_hash(call), &call.smtProofLocalExitRoot, index);
    let valid = if source == 0 {
        local == call.mainnetExitRoot
    } else {
        root(local, &call.smtProofRollupExitRoot, source - 1) == call.rollupExitRoot
    };
    anyhow::ensure!(
        valid,
        "InvalidSmtProof: claim leaf is not included in the supplied exit root"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn set_valid_root(call: &mut claimAssetCall) {
    let (index, source) = crate::applied_state::claim_coordinates(call.globalIndex).unwrap();
    let local = root(leaf_hash(call), &call.smtProofLocalExitRoot, index);
    if source == 0 {
        call.mainnetExitRoot = local;
    } else {
        call.rollupExitRoot = root(local, &call.smtProofRollupExitRoot, source - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, U256};

    #[test]
    fn real_deposit_373_old_root_proves_empty_slot_fresh_root_proves_deposit() {
        use alloy_core::sol_types::SolCall;
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/claim-deposit-373.json")).unwrap();
        let data = alloy::hex::decode(fixture["input"].as_str().unwrap()).unwrap();
        let mut call = claimAssetCall::abi_decode(&data).unwrap();
        assert_eq!(call.globalIndex, (U256::from(1) << 64) | U256::from(373));
        assert_eq!(
            root(B256::ZERO, &call.smtProofLocalExitRoot, 373),
            call.mainnetExitRoot
        );
        assert_eq!(
            root(leaf_hash(&call), &call.smtProofLocalExitRoot, 373),
            "3a3a61e455d891caf3806038a9413b5f6c5c33e02191f7c783f8f046c26c5ed9"
                .parse::<B256>()
                .unwrap()
        );
        assert!(validate(&call).is_err());
        let proof = &fixture["fresh_proof"]["proof"];
        for (i, sibling) in proof["merkle_proof"].as_array().unwrap().iter().enumerate() {
            call.smtProofLocalExitRoot[i] = sibling.as_str().unwrap().parse().unwrap();
        }
        call.mainnetExitRoot = proof["main_exit_root"].as_str().unwrap().parse().unwrap();
        call.rollupExitRoot = proof["rollup_exit_root"].as_str().unwrap().parse().unwrap();
        validate(&call).unwrap();
    }

    fn call(source: u32) -> claimAssetCall {
        let mut call = claimAssetCall {
            smtProofLocalExitRoot: [B256::repeat_byte(0x42); 32],
            smtProofRollupExitRoot: [B256::repeat_byte(0x17); 32],
            globalIndex: crate::applied_state::global_index_for_claim(373, source),
            mainnetExitRoot: B256::ZERO,
            rollupExitRoot: B256::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::repeat_byte(1),
            destinationNetwork: 1,
            destinationAddress: Address::repeat_byte(2),
            amount: U256::MAX,
            metadata: vec![3, 4, 5].into(),
        };
        set_valid_root(&mut call);
        call
    }

    #[test]
    fn accepts_mainnet_and_two_level_rollup_with_full_uint256() {
        for source in [0, 1, 9, u32::MAX] {
            let call = call(source);
            validate(&call).unwrap();
            let mut changed = call.clone();
            changed.amount -= U256::from(1);
            assert!(validate(&changed).is_err());
            let mut changed = call.clone();
            changed.metadata = vec![3, 4, 6].into();
            assert!(validate(&changed).is_err());
            let mut changed = call.clone();
            changed.smtProofLocalExitRoot[1] = B256::ZERO;
            assert!(validate(&changed).is_err());
        }
    }

    #[test]
    fn rejects_proof_of_empty_slot_under_old_root() {
        let mut call = call(0);
        let valid_root = call.mainnetExitRoot;
        call.mainnetExitRoot = root(B256::ZERO, &call.smtProofLocalExitRoot, 373);
        assert!(validate(&call).is_err());
        call.mainnetExitRoot = valid_root;
        validate(&call).unwrap();
    }

    #[test]
    fn rollup_requires_both_levels_mainnet_ignores_unused_rollup_path() {
        let mut mainnet = call(0);
        mainnet.smtProofRollupExitRoot = [B256::ZERO; 32];
        validate(&mainnet).unwrap();
        let mut rollup = call(2);
        rollup.smtProofRollupExitRoot[7] = B256::ZERO;
        assert!(validate(&rollup).is_err());
    }

    #[test]
    fn rejects_noncanonical_and_overflowing_indices() {
        for index in [
            U256::MAX,
            U256::from(2) << 64,
            (U256::from(1) << 64) | (U256::from(1) << 32),
            U256::from(u32::MAX) << 32,
        ] {
            let mut call = call(0);
            call.globalIndex = index;
            assert!(validate(&call).is_err());
        }
    }
}
