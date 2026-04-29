use crate::accounts_config::AccountsConfig;
use alloy::primitives::Address;
use miden_base_agglayer::{EthAddress, EthEmbeddedAccountId};
use miden_protocol::account::AccountId;

const HARDHAT_ADDRESS: Address = Address::new([
    0xf3, 0x9f, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xf6, 0xf4, 0xce, 0x6a, 0xb8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

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
/// Resolution order: hardhat special case → known mapping → zero-padding.
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
    config: &AccountsConfig,
    reject_zero_padding: bool,
) -> anyhow::Result<AccountId> {
    // 1. Hardhat special case
    if address == HARDHAT_ADDRESS {
        return Ok(config.wallet_hardhat.0);
    }
    // 2. Check existing mapping from store
    if let Some(id) = store.get_address_mapping(&address).await? {
        return Ok(id);
    }
    // 3. Try zero-padding (native Miden address) unless disabled.
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
            "0x000000003d7c9747558851900f8206226dfbea00"
        )));
    }

    #[test]
    fn test_account_id_from_address() {
        let expected_account_id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        // Canonical EthEmbeddedAccountId: [4 zero bytes][prefix(8)][suffix(8)]
        // AccountId 0x3d7c9747558851900f8206226dfbea has:
        //   prefix = 0x3d7c974755885190, suffix = 0x0f8206226dfbea00
        let address = address!("0x000000003d7c9747558851900f8206226dfbea00");
        assert_eq!(account_id_from_address(address), Some(expected_account_id));

        assert_eq!(account_id_from_address(Address::from([42u8; 20])), None);
    }

    #[tokio::test]
    async fn test_resolve_zero_padded_address() {
        let addr = address!("0x000000003d7c9747558851900f8206226dfbea00");
        let expected = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
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
        let zero_padded = address!("0x000000003d7c9747558851900f8206226dfbea00");

        // Default policy (reject = false): fallback succeeds.
        let r =
            resolve_address_with_policy(&store, zero_padded, &cfg, false).await;
        assert!(r.is_ok());

        // Strict policy (reject = true): fallback refused with clear error.
        let r =
            resolve_address_with_policy(&store, zero_padded, &cfg, true).await;
        let err = r.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("fallback disabled"));
    }

    /// Self-review C5 — the explicit store mapping must always win
    /// regardless of the policy flag (operators who explicitly mapped
    /// an address must not be blocked by the strict policy).
    #[tokio::test]
    async fn c5_explicit_store_mapping_always_wins() {
        use crate::store::memory::InMemoryStore;
        use crate::Store;
        let store = InMemoryStore::new();
        let cfg = test_accounts_config();
        let mapped_addr = address!("0xabcdef1234567890abcdef1234567890abcdef12");
        let target = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        store.set_address_mapping(mapped_addr, target).await.unwrap();

        // With strict policy, the mapping still resolves.
        let r =
            resolve_address_with_policy(&store, mapped_addr, &cfg, true).await;
        assert_eq!(r.unwrap(), target);
    }

    fn test_accounts_config() -> AccountsConfig {
        use crate::accounts_config::AccountIdBech32;
        let id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        AccountsConfig {
            bridge: AccountIdBech32(id),
            ger_manager: Some(AccountIdBech32(id)),
            service: AccountIdBech32(id),
            faucet_eth: None,
            faucet_agg: None,
            wallet_hardhat: AccountIdBech32(id),
        }
    }
}
