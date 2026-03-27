use miden_client::rpc::Endpoint;
use miden_protocol::account::AccountId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::{Display, Formatter};
use std::path::{Component, PathBuf};
use std::{env, fs};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone)]
pub struct AccountIdBech32(pub AccountId);

impl Display for AccountIdBech32 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let net_id = Endpoint::localhost().to_network_id();
        let str = self.0.to_bech32(net_id);
        write!(f, "{str}")
    }
}

impl Serialize for AccountIdBech32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str = self.to_string();
        serializer.serialize_str(&str)
    }
}

impl<'de> Deserialize<'de> for AccountIdBech32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str = String::deserialize(deserializer)?;
        let (_, id) = AccountId::from_bech32(&str).map_err(serde::de::Error::custom)?;
        Ok(Self(id))
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
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let config_toml = toml::to_string(&config)?;
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
}
