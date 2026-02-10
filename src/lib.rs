mod accounts_config;
mod address_mapper;
mod amount;
mod block_num_tracker;
pub mod claim;
pub mod ger;
pub mod hex;
pub mod init;
pub mod logging;
mod miden_client;

pub const COMPONENT: &str = "miden-agglayer";

pub use block_num_tracker::BlockNumTracker;
pub use miden_client::MidenClient;

#[derive(Clone)]
pub struct AccountsConfig(accounts_config::AccountsConfig);
pub use accounts_config::config_path_exists;

pub fn load_config(miden_store_dir: Option<std::path::PathBuf>) -> anyhow::Result<AccountsConfig> {
    accounts_config::load_config(miden_store_dir).map(AccountsConfig)
}
