pub mod account_recovery;
pub mod accounts_config;
pub mod address_mapper;
pub mod block_monitor;
pub mod block_state;
pub mod bridge_address;
pub mod bridge_out;
pub mod burn_serial_tracker;
pub mod claim;
pub mod claim_watcher;
pub mod exit;
pub mod expected_mint_tracker;
pub mod faucet_ops;
pub mod faucet_ownership_monitor;
pub mod forged_mint_detector;
pub mod ger;
pub mod hex;
pub mod init;
pub mod l1_info_tree_indexer;
pub mod l2_to_l1_claimer;
pub mod let_divergence;
pub mod log_synthesis;
pub mod logging;
pub mod metadata_recovery;
pub mod metrics;
pub mod miden_client;
pub mod mint_target_monitor;
pub mod recovery;
pub mod restore;
pub mod service;
pub(crate) mod service_admin;
pub(crate) mod service_debug;
pub(crate) mod service_eth_call;
pub(crate) mod service_get_logs;
pub mod service_get_txn_receipt;
pub(crate) mod service_helpers;
pub mod service_send_raw_txn;
pub mod service_state;
pub(crate) mod service_zkevm;
pub mod store;
#[cfg(test)]
pub mod test_helpers;
pub mod twin_note_detector;
pub mod unknown_wrapper_detector;
pub mod writer_worker;

pub const COMPONENT: &str = "miden-agglayer";

pub use miden_client::MidenClient;
pub use service_state::ServiceState;
pub use store::Store;

#[derive(Clone)]
pub struct AccountsConfig(pub accounts_config::AccountsConfig);
pub use accounts_config::config_path_exists;

pub fn load_config(miden_store_dir: Option<std::path::PathBuf>) -> anyhow::Result<AccountsConfig> {
    accounts_config::load_config(miden_store_dir).map(AccountsConfig)
}
