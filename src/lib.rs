pub mod accounts_config;
pub mod address_mapper;
mod amount;
mod block_num_tracker;
pub mod block_state;
pub mod claim;
pub mod claim_tracker;
pub mod exit;
pub mod ger;
pub mod hex;
pub mod init;
pub mod log_synthesis;
pub mod logging;
pub mod miden_client;
pub mod nonce_tracker;
pub mod service;
pub mod service_get_txn_receipt;
pub mod service_send_raw_txn;
pub mod service_state;
mod txn_manager;

pub const COMPONENT: &str = "miden-agglayer";

pub use address_mapper::AddressMapper;
pub use block_num_tracker::BlockNumTracker;
pub use claim_tracker::ClaimTracker;
pub use miden_client::MidenClient;
pub use nonce_tracker::NonceTracker;
pub use service_state::ServiceState;
pub use txn_manager::TxnManager;

#[derive(Clone)]
pub struct AccountsConfig(accounts_config::AccountsConfig);
pub use accounts_config::config_path_exists;

pub fn load_config(miden_store_dir: Option<std::path::PathBuf>) -> anyhow::Result<AccountsConfig> {
    accounts_config::load_config(miden_store_dir).map(AccountsConfig)
}
