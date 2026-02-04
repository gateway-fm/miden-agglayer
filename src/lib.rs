mod accounts_config;
pub mod claim;
pub mod hex;
pub mod init;
pub mod logging;
mod miden_client;

pub const COMPONENT: &str = "miden-agglayer";

pub use miden_client::MidenClient;

#[derive(Clone)]
pub struct AccountsConfig(accounts_config::AccountsConfig);
pub use accounts_config::config_path_exists;

pub fn load_config(miden_store_dir: Option<std::path::PathBuf>) -> anyhow::Result<AccountsConfig> {
    accounts_config::load_config(miden_store_dir).map(AccountsConfig)
}
