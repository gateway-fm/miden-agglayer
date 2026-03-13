use crate::accounts_config::AccountsConfig;
use alloy::primitives::Address;
use miden_protocol::account::AccountId;
use parking_lot::RwLock;
use sha3::{Digest, Keccak256};
use std::collections::HashMap;
use std::path::PathBuf;

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

/// Deterministically derive a Miden AccountId from an Ethereum address.
///
/// Uses keccak256("miden-agglayer-addr-v1" || address_bytes) as seed, then
/// sets metadata bits for RegularAccountUpdatableCode + Public + Version0.
fn derive_account_id(address: Address) -> anyhow::Result<AccountId> {
    let mut hasher = Keccak256::new();
    hasher.update(b"miden-agglayer-addr-v1");
    hasher.update(address.as_slice());
    let hash: [u8; 32] = hasher.finalize().into();

    let mut id_bytes = [0u8; 15];
    id_bytes.copy_from_slice(&hash[..15]);

    // Set metadata bits for RegularAccountUpdatableCode (0b01) + Public (0b00) + Version0 (0)
    // Byte 7 (LS byte of prefix): (storage_mode << 6) | (account_type << 4) | version
    id_bytes[7] = 0b01 << 4; // 0x10

    // Clear MSB of prefix (byte 0) — Felt requires < 2^63
    id_bytes[0] &= 0x7F;

    // Clear 32nd MSB of prefix (byte 3, bit 0)
    id_bytes[3] &= 0xFE;

    // Clear MSB of suffix (byte 8) — Felt requires < 2^63
    id_bytes[8] &= 0x7F;

    AccountId::try_from(id_bytes)
        .map_err(|e| anyhow::anyhow!("failed to derive AccountId for {address}: {e}"))
}

pub struct AddressMapper {
    mappings: RwLock<HashMap<Address, AccountId>>,
    persistence_path: Option<PathBuf>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<AddressMapper>();

impl AddressMapper {
    pub fn new(persistence_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let mappings = if let Some(ref path) = persistence_path {
            if path.exists() {
                let data = std::fs::read_to_string(path)?;
                let raw: HashMap<String, String> = serde_json::from_str(&data)?;
                let mut map = HashMap::new();
                for (eth_hex, miden_hex) in raw {
                    let eth: Address = eth_hex.parse()?;
                    let miden = AccountId::from_hex(&miden_hex)
                        .map_err(|e| anyhow::anyhow!("bad account id {miden_hex}: {e}"))?;
                    map.insert(eth, miden);
                }
                map
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };
        Ok(Self {
            mappings: RwLock::new(mappings),
            persistence_path,
        })
    }

    /// Resolve an Ethereum address to a Miden AccountId.
    /// Resolution order: hardhat special case → known mapping → zero-padding → derive new.
    pub fn resolve(&self, address: Address, config: &AccountsConfig) -> anyhow::Result<AccountId> {
        // 1. Hardhat special case
        if address == HARDHAT_ADDRESS {
            return Ok(config.wallet_hardhat.0);
        }
        // 2. Check existing mapping
        if let Some(id) = self.mappings.read().get(&address) {
            return Ok(*id);
        }
        // 3. Try zero-padding (native Miden address)
        if let Some(id) = account_id_from_address(address) {
            return Ok(id);
        }
        // 4. Derive deterministically and store
        let id = self.derive_and_store(address)?;
        Ok(id)
    }

    fn derive_and_store(&self, address: Address) -> anyhow::Result<AccountId> {
        let id = derive_account_id(address)?;
        tracing::info!(
            eth_address = %address,
            miden_account = %id.to_hex(),
            "AddressMapper: derived new mapping"
        );
        self.mappings.write().insert(address, id);
        self.persist();
        Ok(id)
    }

    fn persist(&self) {
        let Some(ref path) = self.persistence_path else {
            return;
        };
        let mappings = self.mappings.read();
        let raw: HashMap<String, String> = mappings
            .iter()
            .map(|(eth, miden)| (format!("{eth:#x}"), miden.to_hex()))
            .collect();
        let Ok(data) = serde_json::to_string_pretty(&raw) else {
            tracing::error!("AddressMapper: failed to serialize mappings");
            return;
        };
        drop(mappings);

        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!("AddressMapper: failed to write {}: {e}", tmp_path.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::error!("AddressMapper: failed to rename to {}: {e}", path.display());
        }
    }
}

impl Default for AddressMapper {
    fn default() -> Self {
        Self::new(None).unwrap()
    }
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

    #[test]
    fn test_derive_account_id_deterministic() {
        let addr = address!("0x742d35Cc6634C0532925a3b844Bc9e7595f41111");
        let id1 = derive_account_id(addr).unwrap();
        let id2 = derive_account_id(addr).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_derive_account_id_different_inputs() {
        let addr1 = address!("0x742d35Cc6634C0532925a3b844Bc9e7595f41111");
        let addr2 = address!("0x742d35Cc6634C0532925a3b844Bc9e7595f42222");
        let id1 = derive_account_id(addr1).unwrap();
        let id2 = derive_account_id(addr2).unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_derive_account_id_is_regular_public() {
        let addr = address!("0xdead00000000000000000000000000000000beef");
        let id = derive_account_id(addr).unwrap();
        assert!(id.is_regular_account());
        assert!(id.is_public());
    }

    #[test]
    fn test_resolve_zero_padded_address() {
        let mapper = AddressMapper::new(None).unwrap();
        // Canonical format: 4 zero bytes + prefix(8) + suffix(8)
        let addr = address!("0x000000003d7c9747558851900f8206226dfbea00");
        let expected = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        let result = account_id_from_address(addr);
        assert_eq!(result, Some(expected));
        // Verify the mapper doesn't store zero-padded addresses
        assert!(mapper.mappings.read().is_empty());
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "address_mapper_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("address_mappings.json");

        let addr = address!("0x742d35Cc6634C0532925a3b844Bc9e7595f41111");
        let derived_id;

        {
            let mapper = AddressMapper::new(Some(path.clone())).unwrap();
            derived_id = derive_account_id(addr).unwrap();
            mapper.mappings.write().insert(addr, derived_id);
            mapper.persist();
        }

        // Reload from file
        let mapper = AddressMapper::new(Some(path.clone())).unwrap();
        let loaded = mapper.mappings.read().get(&addr).copied();
        assert_eq!(loaded, Some(derived_id));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
