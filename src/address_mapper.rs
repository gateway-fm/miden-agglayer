use crate::accounts_config::AccountsConfig;
use alloy::primitives::Address;
use miden_base_agglayer::{EthAddress, EthEmbeddedAccountId};
use miden_protocol::account::AccountId;

pub fn is_miden_compatible_address(address: Address) -> bool {
    // The canonical EthEmbeddedAccountId encoding embeds AccountId as:
    //   [4 zero bytes] [prefix(8 bytes)] [suffix(8 bytes)]
    // Only the first 4 bytes must be zero; byte 4 is the MSB of the prefix.
    address[0..4].iter().all(|b| *b == 0)
}

pub fn account_id_from_address(address: Address) -> Option<AccountId> {
    if !is_miden_compatible_address(address) {
        return None;
    }
    // 0.14.x: wrap the 20-byte EVM address and use the dedicated embedded-AccountId type.
    // `EthEmbeddedAccountId::try_from(eth_addr)` validates the 4 leading zero bytes and
    // reconstructs the inner AccountId; failure here means the address wasn't a valid
    // zero-padded Miden id even though `is_miden_compatible_address` thought it was.
    let eth_addr = EthAddress::new(address.0.0);
    EthEmbeddedAccountId::try_from(eth_addr)
        .ok()
        .map(AccountId::from)
}

/// Resolve an Ethereum address to a Miden AccountId.
/// Resolution order: known store mapping → zero-padding.
///
/// Cantina MA#8 — there is no special case for the well-known Hardhat
/// default-account address (`0xf39f...2266`). It flows through the normal
/// mapping path like any other address: a deposit targeting it resolves
/// only if an operator has explicitly mapped it (or it happens to be a
/// zero-padded Miden id, which the Hardhat EOA is not). No dev-only remap
/// to a hardcoded account, no gate, no bail — the address is treated like
/// any other EVM address.
///
/// Self-review C5 — the zero-padding fallback maps any 4-leading-zero EVM
/// address to a Miden AccountId WITHOUT verifying the account exists on
/// the Miden node. A malicious user can craft a destination address like
/// `0x00000000_aaaa...` and the claim will succeed, minting to a
/// never-deployed account → funds locked permanently.
///
/// We can't drop the fallback entirely (aggsender / aggoracle / hardhat
/// dev flows legitimately use the zero-padding scheme) but we can:
/// 1. Increment `address_mapper_zero_padding_fallback_total` so operators
///    can alert on unusual rates.
/// 2. Allow operators to disable the fallback in production via the
///    `reject_zero_padding` flag (default false for backward compat).
pub async fn resolve_address(
    store: &dyn crate::store::Store,
    address: Address,
    config: &AccountsConfig,
) -> anyhow::Result<AccountId> {
    resolve_address_with_policy(store, address, config, false).await
}

/// Same as `resolve_address` but allows the caller to disable the
/// zero-padding fallback (production posture). When `reject_zero_padding`
/// is `true`, addresses that aren't in the store mapping fail with a
/// clear error rather than falling through to the structural reconstruction.
pub async fn resolve_address_with_policy(
    store: &dyn crate::store::Store,
    address: Address,
    _config: &AccountsConfig,
    reject_zero_padding: bool,
) -> anyhow::Result<AccountId> {
    // 1. Check existing mapping from store
    if let Some(id) = store.get_address_mapping(&address).await? {
        return Ok(id);
    }
    // 2. Try zero-padding (native Miden address) unless disabled.
    if reject_zero_padding {
        anyhow::bail!(
            "no known Miden AccountId for Ethereum address {address}; \
             zero-padding fallback disabled by configuration (C5)"
        );
    }
    if let Some(id) = account_id_from_address(address) {
        ::metrics::counter!("address_mapper_zero_padding_fallback_total").increment(1);
        tracing::warn!(
            target: "address_mapper",
            address = %address,
            account_id = %id,
            "C5: resolved EVM address via zero-padding fallback (no store mapping; \
             account existence on Miden NOT verified)"
        );
        return Ok(id);
    }
    anyhow::bail!("no known Miden AccountId for Ethereum address {address}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_is_miden_compatible_address() {
        assert!(!is_miden_compatible_address(Address::from([42u8; 20])));
        assert!(is_miden_compatible_address(Address::from([0u8; 20])));
        assert!(!is_miden_compatible_address(address!(
            "0x742d35Cc6634C0532925a3b844Bc9e7595f41111"
        )));
        // Canonical format: 4 zero bytes + 16 bytes of AccountId data
        assert!(is_miden_compatible_address(address!(
            "0x00000000ac0000000000dd110000ee000000fc00"
        )));
    }

    #[test]
    fn test_account_id_from_address() {
        let expected_account_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        // Canonical EthEmbeddedAccountId: [4 zero bytes][prefix(8)][suffix(8)]
        // AccountId 0xac0000000000dd110000ee000000fc has:
        //   prefix = 0xac0000000000dd11, suffix = 0x0000ee000000fc00
        let address = address!("0x00000000ac0000000000dd110000ee000000fc00");
        assert_eq!(account_id_from_address(address), Some(expected_account_id));

        assert_eq!(account_id_from_address(Address::from([42u8; 20])), None);
    }

    #[tokio::test]
    async fn test_resolve_zero_padded_address() {
        let addr = address!("0x00000000ac0000000000dd110000ee000000fc00");
        let expected = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let result = account_id_from_address(addr);
        assert_eq!(result, Some(expected));
    }

    /// Self-review C5 — repro+regression. With `reject_zero_padding = true`,
    /// the resolver must REFUSE the zero-padding fallback for addresses
    /// that aren't in the store mapping (or hardhat special case). Pre-fix
    /// there was no opt-out — every 4-leading-zero address resolved
    /// silently. The new flag lets operators turn off the fallback in
    /// production while leaving it on for dev/test setups that use
    /// hardhat / aggsender flows.
    #[tokio::test]
    async fn c5_reject_zero_padding_when_policy_set() {
        use crate::store::memory::InMemoryStore;
        let store = InMemoryStore::new();
        let cfg = test_accounts_config();
        let zero_padded = address!("0x00000000ac0000000000dd110000ee000000fc00");

        // Default policy (reject = false): fallback succeeds.
        let r = resolve_address_with_policy(&store, zero_padded, &cfg, false).await;
        assert!(r.is_ok());

        // Strict policy (reject = true): fallback refused with clear error.
        let r = resolve_address_with_policy(&store, zero_padded, &cfg, true).await;
        let err = r.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("fallback disabled"));
    }

    /// Self-review C5 — the explicit store mapping must always win
    /// regardless of the policy flag (operators who explicitly mapped
    /// an address must not be blocked by the strict policy).
    #[tokio::test]
    async fn c5_explicit_store_mapping_always_wins() {
        use crate::Store;
        use crate::store::memory::InMemoryStore;
        let store = InMemoryStore::new();
        let cfg = test_accounts_config();
        let mapped_addr = address!("0xabcdef1234567890abcdef1234567890abcdef12");
        let target = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        store
            .set_address_mapping(mapped_addr, target)
            .await
            .unwrap();

        // With strict policy, the mapping still resolves.
        let r = resolve_address_with_policy(&store, mapped_addr, &cfg, true).await;
        assert_eq!(r.unwrap(), target);
    }

    /// Cantina MA#8 — the well-known Hardhat default-account address
    /// (`0xf39f...2266`) must NOT be special-cased in `resolve_address`.
    /// Pre-fix there was an `if address == HARDHAT_ADDRESS` branch that
    /// remapped it to a hardcoded infrastructure account on every claim
    /// (later merely gated behind a flag). cergyk required the branch be
    /// removed entirely so the address flows through the normal mapping
    /// path with no special behavior. This test pins that: with no
    /// explicit store mapping, the Hardhat EOA (which is not a
    /// zero-padded Miden id) resolves to nothing — it errors just as it
    /// would for any other un-mapped, non-zero-padded EVM address.
    #[tokio::test]
    async fn ma8_hardhat_address_not_special_cased() {
        use crate::store::memory::InMemoryStore;
        let store = InMemoryStore::new();
        let cfg = test_accounts_config();
        let hardhat = address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");

        // Default policy: the Hardhat EOA is NOT zero-padded (high bytes
        // are 0xf39f...), and there is no store mapping. If the old special
        // case were still present it would resolve to a hardcoded account
        // (an `Ok`); with the branch removed it must error exactly like any
        // other un-mapped non-zero-padded address.
        let r = resolve_address_with_policy(&store, hardhat, &cfg, false).await;
        assert!(
            r.is_err(),
            "Hardhat EOA must not be special-cased; expected the normal \
             'no known Miden AccountId' error, but it resolved to {:?}",
            r.as_ref().ok(),
        );

        // And a generic un-mapped non-zero-padded address gives the same
        // outcome — proving the Hardhat address gets no distinct treatment.
        let other = address!("0xabcdef1234567890abcdef1234567890abcdef12");
        let r_other = resolve_address_with_policy(&store, other, &cfg, false).await;
        assert!(r_other.is_err());
    }

    /// Cantina MA#8 — once an operator explicitly maps the Hardhat EOA,
    /// it resolves to exactly the mapped target via the normal store
    /// path (no special case interferes).
    #[tokio::test]
    async fn ma8_hardhat_address_resolves_via_explicit_mapping() {
        use crate::Store;
        use crate::store::memory::InMemoryStore;
        let store = InMemoryStore::new();
        let cfg = test_accounts_config();
        let hardhat = address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        let target = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        store.set_address_mapping(hardhat, target).await.unwrap();

        let r = resolve_address_with_policy(&store, hardhat, &cfg, false).await;
        assert_eq!(
            r.unwrap(),
            target,
            "Hardhat EOA must resolve through the normal store mapping"
        );
    }

    fn test_accounts_config() -> AccountsConfig {
        use crate::accounts_config::AccountIdBech32;
        let id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        AccountsConfig {
            bridge: AccountIdBech32(id),
            ger_manager: Some(AccountIdBech32(id)),
            service: AccountIdBech32(id),
            faucet_eth: None,
            faucet_agg: None,
        }
    }
}
