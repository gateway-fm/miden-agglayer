use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::{Display, Formatter};
use std::path::{Component, PathBuf};
use std::{env, fs};

#[derive(Debug, Clone, Deserialize)]
pub struct AccountsConfig {
    pub service: AccountIdBech32,
    pub bridge: AccountIdBech32,
    /// Legacy field — kept for backward compatibility with existing TOML configs.
    /// New deployments use the dynamic faucet registry in the Store.
    #[serde(default)]
    pub faucet_eth: Option<AccountIdBech32>,
    /// Legacy field — kept for backward compatibility with existing TOML configs.
    #[serde(default)]
    pub faucet_agg: Option<AccountIdBech32>,
    pub wallet_hardhat: AccountIdBech32,
    /// Dedicated account for GER injection. Separate from `service` so the
    /// NTX builder's modifications to the service account don't cause stale
    /// state errors when submitting UpdateGerNotes.
    #[serde(default)]
    pub ger_manager: Option<AccountIdBech32>,
}

#[derive(Debug, Clone)]
pub struct AccountIdBech32(pub AccountId);

impl AccountIdBech32 {
    /// Render the account id as a bech32 string for the given network.
    ///
    /// Bech32 encodes the network in its HRP. Using the wrong HRP makes the
    /// address unfindable on the network's block explorer even though the
    /// underlying account is valid on-chain — see PR introducing this method.
    /// Any code that emits the bech32 form across a system boundary (on-disk
    /// config, logs, API responses, support tickets) MUST pass the
    /// `NetworkId` of the active node.
    pub fn to_bech32(&self, net_id: NetworkId) -> String {
        self.0.to_bech32(net_id)
    }
}

impl Display for AccountIdBech32 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // Network-agnostic hex form. For the bech32 form (which encodes the
        // network HRP) call `to_bech32(net_id)` with an explicit NetworkId.
        write!(f, "{}", self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for AccountIdBech32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let id = if s.starts_with("0x") || s.starts_with("0X") {
            AccountId::from_hex(&s).map_err(serde::de::Error::custom)?
        } else {
            // `from_bech32` validates the bech32 checksum but doesn't constrain the HRP;
            // older configs written before the network-id fix used the `mlcl` (local) HRP
            // even on testnet. Both forms decode to the same on-chain account.
            AccountId::from_bech32(&s)
                .map(|(_, id)| id)
                .map_err(serde::de::Error::custom)?
        };
        Ok(Self(id))
    }
}

/// On-disk representation. Holds the bech32 strings ready-encoded with the
/// network id supplied at save time, so the bech32 HRP matches the active
/// node. Constructing this from `AccountsConfig` is the only sanctioned way
/// to serialize the config — there is intentionally no derived `Serialize`
/// impl on `AccountsConfig` itself.
#[derive(Serialize)]
struct AccountsConfigOnDisk {
    service: String,
    bridge: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    faucet_eth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    faucet_agg: Option<String>,
    wallet_hardhat: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ger_manager: Option<String>,
}

impl AccountsConfigOnDisk {
    fn from_config(config: &AccountsConfig, net_id: &NetworkId) -> Self {
        let enc = |id: &AccountIdBech32| id.to_bech32(net_id.clone());
        Self {
            service: enc(&config.service),
            bridge: enc(&config.bridge),
            faucet_eth: config.faucet_eth.as_ref().map(&enc),
            faucet_agg: config.faucet_agg.as_ref().map(&enc),
            wallet_hardhat: enc(&config.wallet_hardhat),
            ger_manager: config.ger_manager.as_ref().map(&enc),
        }
    }
}

/// Reject paths containing parent-directory (`..`) traversal components.
fn sanitize_store_dir(dir: &PathBuf) -> anyhow::Result<PathBuf> {
    for component in dir.components() {
        if matches!(component, Component::ParentDir) {
            anyhow::bail!(
                "path traversal detected: store directory must not contain '..' \
                 components, got: {dir:?}"
            );
        }
    }

    // Canonicalize when the directory already exists on disk so that any
    // symlink-based traversal is also resolved. When it does not yet exist
    // (first run / --init), the lexical check above is sufficient.
    if dir.exists() {
        let canonical = dir.canonicalize()?;
        // After resolving symlinks, re-verify there are no `..` segments.
        for component in canonical.components() {
            if matches!(component, Component::ParentDir) {
                anyhow::bail!("path traversal detected after canonicalization: {canonical:?}");
            }
        }
        return Ok(canonical);
    }
    Ok(dir.clone())
}

fn config_path(miden_store_dir_opt: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let miden_store_dir = miden_store_dir_opt.unwrap_or_else(|| {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    });
    let safe_dir = sanitize_store_dir(&miden_store_dir)?;
    Ok(safe_dir.join("bridge_accounts.toml"))
}

pub fn config_path_exists(miden_store_dir: Option<PathBuf>) -> anyhow::Result<bool> {
    let config_path = config_path(miden_store_dir)?;
    Ok(fs::exists(&config_path)?)
}

pub fn save_config(
    config: AccountsConfig,
    net_id: &NetworkId,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let on_disk = AccountsConfigOnDisk::from_config(&config, net_id);
    let config_toml = toml::to_string(&on_disk)?;
    let config_path = config_path(miden_store_dir)?;
    fs::write(config_path.clone(), config_toml)?;
    Ok(config_path)
}

pub fn load_config(miden_store_dir: Option<PathBuf>) -> anyhow::Result<AccountsConfig> {
    let config_path = config_path(miden_store_dir)?;
    let config_toml = fs::read_to_string(config_path)?;
    let config = toml::from_str(&config_toml)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_ACCOUNT_HEX: &str = "0x27525024cc2047507cb35ee9ed00d4";

    fn dummy() -> AccountIdBech32 {
        AccountIdBech32(AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap())
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let bad = Some(PathBuf::from("/tmp/../etc"));
        let result = config_path(bad);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("path traversal"),
            "expected path traversal error, got: {msg}"
        );
    }

    #[test]
    fn rejects_embedded_traversal() {
        let bad = Some(PathBuf::from("/home/user/.miden/../../etc"));
        let result = config_path(bad);
        assert!(result.is_err());
    }

    #[test]
    fn allows_clean_absolute_path() {
        let good = Some(PathBuf::from("/tmp/miden-test-store"));
        let result = config_path(good);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("bridge_accounts.toml"));
    }

    #[test]
    fn allows_default_path() {
        let result = config_path(None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("bridge_accounts.toml"));
    }

    #[test]
    fn save_writes_bech32_with_configured_network_hrp() {
        let dir = tempdir().unwrap();
        let cfg = AccountsConfig {
            service: dummy(),
            bridge: dummy(),
            faucet_eth: Some(dummy()),
            faucet_agg: None,
            wallet_hardhat: dummy(),
            ger_manager: Some(dummy()),
        };
        save_config(cfg, &NetworkId::Testnet, Some(dir.path().to_path_buf())).unwrap();
        let body = fs::read_to_string(dir.path().join("bridge_accounts.toml")).unwrap();
        // Every emitted id must use the `mtst` testnet HRP — never `mlcl`.
        assert!(
            body.contains("\"mtst1"),
            "expected testnet (`mtst`) HRP in saved bech32; got:\n{body}"
        );
        assert!(
            !body.contains("\"mlcl1"),
            "saved bech32 must not use the local-network (`mlcl`) HRP; got:\n{body}"
        );
    }

    #[test]
    fn load_accepts_legacy_local_hrp_bech32() {
        // Files written by previous versions used the local-network HRP
        // unconditionally. They must still load — `from_bech32` ignores the HRP.
        let dir = tempdir().unwrap();
        let id = AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap();
        let local_hrp = NetworkId::new("mlcl").unwrap();
        let legacy_bech32 = id.to_bech32(local_hrp);
        let toml_body = format!(
            "service = \"{b}\"\nbridge = \"{b}\"\nwallet_hardhat = \"{b}\"\n",
            b = legacy_bech32
        );
        fs::write(dir.path().join("bridge_accounts.toml"), toml_body).unwrap();
        let loaded = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(loaded.bridge.0, id);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let cfg = AccountsConfig {
            service: dummy(),
            bridge: dummy(),
            faucet_eth: None,
            faucet_agg: None,
            wallet_hardhat: dummy(),
            ger_manager: None,
        };
        save_config(cfg, &NetworkId::Testnet, Some(dir.path().to_path_buf())).unwrap();
        let loaded = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            loaded.bridge.0,
            AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap()
        );
    }
}
