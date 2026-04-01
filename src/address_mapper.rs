use crate::accounts_config::AccountsConfig;
use alloy::primitives::Address;
use miden_protocol::account::AccountId;

const HARDHAT_ADDRESS: Address = Address::new([
    0xf3, 0x9f, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xf6, 0xf4, 0xce, 0x6a, 0xb8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

pub fn is_miden_compatible_address(address: Address) -> bool {
    // The canonical EthAddressFormat embeds AccountId as:
    //   [4 zero bytes] [prefix(8 bytes)] [suffix(8 bytes)]
    // Only the first 4 bytes must be zero; byte 4 is the MSB of the prefix.
    address[0..4].iter().all(|b| *b == 0)
}

pub fn account_id_from_address(address: Address) -> Option<AccountId> {
    if !is_miden_compatible_address(address) {
        return None;
    }
    // Extract prefix (8 bytes) and suffix (8 bytes) from canonical layout:
    //   address[0..4]  = zero padding
    //   address[4..12] = prefix (u64 big-endian)
    //   address[12..20] = suffix (u64 big-endian)
    let prefix = u64::from_be_bytes(address[4..12].try_into().ok()?);
    let suffix = u64::from_be_bytes(address[12..20].try_into().ok()?);
    let prefix_felt = miden_protocol::Felt::try_from(prefix).ok()?;
    let suffix_felt = miden_protocol::Felt::try_from(suffix).ok()?;
    AccountId::try_from([prefix_felt, suffix_felt]).ok()
}

/// Resolve an Ethereum address to a Miden AccountId.
/// Resolution order: hardhat special case → known mapping → zero-padding.
pub async fn resolve_address(
    store: &dyn crate::store::Store,
    address: Address,
    config: &AccountsConfig,
) -> anyhow::Result<AccountId> {
    // 1. Hardhat special case
    if address == HARDHAT_ADDRESS {
        return Ok(config.wallet_hardhat.0);
    }
    // 2. Check existing mapping from store
    if let Some(id) = store.get_address_mapping(&address).await? {
        return Ok(id);
    }
    // 3. Try zero-padding (native Miden address)
    if let Some(id) = account_id_from_address(address) {
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
        // Canonical EthAddressFormat: [4 zero bytes][prefix(8)][suffix(8)]
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
}
