use crate::accounts_config::AccountsConfig;
use alloy::primitives::Address;
use miden_protocol::account::AccountId;

pub fn is_miden_compatible_address(address: Address) -> bool {
    address[0..5].iter().all(|b| *b == 0)
}

pub fn account_id_from_address(address: Address) -> Option<AccountId> {
    if !is_miden_compatible_address(address) {
        return None;
    }
    let mut id_bytes = [0u8; 15];
    id_bytes.copy_from_slice(&address[5..]);
    AccountId::try_from(id_bytes).ok()
}

pub fn account_id_from_address_config(
    address: Address,
    config: &AccountsConfig,
) -> Option<AccountId> {
    if address.to_string() == "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" {
        return Some(config.wallet_hardhat.0);
    }
    account_id_from_address(address)
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
        assert!(is_miden_compatible_address(address!(
            "0x000000000034C0532925a3b844Bc9e7595f41111"
        )));
    }

    #[test]
    fn test_account_id_from_address() {
        let expected_account_id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        let address = address!("0x00000000003d7c9747558851900f8206226dfbea");
        assert_eq!(account_id_from_address(address), Some(expected_account_id));

        assert_eq!(account_id_from_address(Address::from([42u8; 20])), None);

        let invalid_address = address!("0x000000000034C0532925a3b844Bc9e7595f41111");
        assert_eq!(account_id_from_address(invalid_address), None);
    }
}
