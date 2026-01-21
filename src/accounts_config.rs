use miden_protocol::account::AccountId;
use miden_protocol::address::{CustomNetworkId, NetworkId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;
use std::str::FromStr;
use std::{env, fs};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsConfig {
    pub service: AccountIdBech32,
    pub bridge: AccountIdBech32,
    pub faucet_eth: AccountIdBech32,
    pub faucet_agg: AccountIdBech32,
    pub wallet_hardhat: AccountIdBech32,
    pub wallet_satoshi: AccountIdBech32,
}

#[derive(Debug, Clone)]
pub struct AccountIdBech32(pub AccountId);

impl Serialize for AccountIdBech32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let net_id = CustomNetworkId::from_str("local").unwrap();
        let str = self.0.to_bech32(NetworkId::Custom(Box::new(net_id)));
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

fn config_path(miden_store_dir_opt: Option<PathBuf>) -> PathBuf {
    let miden_store_dir = miden_store_dir_opt.unwrap_or_else(|| {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    });
    miden_store_dir.join("bridge_accounts.toml")
}

pub fn config_path_exists(miden_store_dir: Option<PathBuf>) -> std::io::Result<bool> {
    let config_path = config_path(miden_store_dir);
    fs::exists(&config_path)
}

pub fn save_config(
    config: AccountsConfig,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let config_toml = toml::to_string(&config)?;
    let config_path = config_path(miden_store_dir);
    fs::write(config_path.clone(), config_toml)?;
    Ok(config_path)
}

pub fn load_config(miden_store_dir: Option<PathBuf>) -> anyhow::Result<AccountsConfig> {
    let config_path = config_path(miden_store_dir);
    let config_toml = fs::read_to_string(config_path)?;
    let config = toml::from_str(&config_toml)?;
    Ok(config)
}
