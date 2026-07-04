//! Shared test utilities for the miden-agglayer crate.
//!
//! Provides `create_test_service()` and `test_accounts_config()` so that test
//! modules across the crate can set up a `ServiceState` without duplication
//! or `unsafe { std::mem::zeroed() }`.

use crate::accounts_config::{AccountIdBech32, AccountsConfig as InnerAccountsConfig};
use crate::block_state::BlockState;
use crate::store::memory::InMemoryStore;
use crate::store::{FaucetEntry, Store};
use crate::{AccountsConfig, MidenClient, ServiceState};
use miden_protocol::account::AccountId;
use std::sync::Arc;

/// A valid hex-encoded AccountId used across all test fixtures.
/// Protocol 0.15 rejects the old v0 AccountId encoding (`UnknownAccountIdVersion`);
/// this is a valid 0.15 (version-1) public regular-account id.
const TEST_ACCOUNT_HEX: &str = "0xac0000000000dd110000ee000000fc";

fn dummy_account_id() -> AccountIdBech32 {
    AccountIdBech32(AccountId::from_hex(TEST_ACCOUNT_HEX).expect("valid test account ID"))
}

/// Build an `AccountsConfig` with valid (but dummy) account IDs.
pub fn test_accounts_config() -> AccountsConfig {
    AccountsConfig(InnerAccountsConfig {
        service: dummy_account_id(),
        bridge: dummy_account_id(),
        faucet_eth: Some(dummy_account_id()),
        faucet_agg: None,
        wallet_hardhat: dummy_account_id(),
        ger_manager: None,
    })
}

/// Seed the faucet registry with the default ETH faucet for testing.
pub async fn seed_test_faucets(store: &dyn Store) {
    let eth_id = AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap();
    store
        .register_faucet(FaucetEntry {
            faucet_id: eth_id,
            origin_address: [0u8; 20],
            origin_network: 0,
            symbol: "ETH".into(),
            origin_decimals: 18,
            miden_decimals: 8,
            scale: 10,
            metadata: vec![],
        })
        .await
        .unwrap();
}

/// Create a `ServiceState` backed by `InMemoryStore` and a test `MidenClient`
/// stub (no real Miden node connection). Suitable for unit tests.
pub fn create_test_service() -> ServiceState {
    let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
    let block_state = Arc::new(BlockState::new());
    let miden_client = MidenClient::new_test();
    let accounts = test_accounts_config();
    ServiceState::new(miden_client, accounts, 1, 1, store, block_state)
}

/// Build a REAL `MidenClientLib` backed by a throwaway sqlite store and an RPC
/// handle pointing at the (unused) localhost endpoint. `ClientBuilder::build`
/// performs no network I/O — it only initialises the sqlite store and reads the
/// (absent) genesis header — so tests can exercise code paths that require a
/// `&mut MidenClientLib` argument but return before issuing any RPC:
///
/// - the Cantina #1 cross-network refusal in `claim::find_or_create_faucet`
///   (bails before the client is touched);
/// - the Cantina MA#23 `on_post_sync` dispatch gate in
///   `MidenClient::on_sync` (the listener decides whether to use the client).
///
/// Only available under `cfg(test)` — production code must never construct a
/// second client next to the process-wide `MidenClient` singleton.
#[cfg(test)]
pub async fn offline_miden_client_lib() -> crate::miden_client::MidenClientLib {
    use miden_client::DebugMode;
    use miden_client::builder::ClientBuilder;
    use miden_client::keystore::FilesystemKeyStore;
    use miden_client::rpc::Endpoint;
    use miden_client_sqlite_store::ClientBuilderSqliteExt;

    let store_dir = tempfile::tempdir().expect("tempdir").keep();
    let keystore_path = store_dir.join("keystore");
    std::fs::create_dir_all(&keystore_path).expect("keystore dir");
    let keystore = Arc::new(FilesystemKeyStore::new(keystore_path).expect("keystore"));

    ClientBuilder::new()
        .rpc(crate::miden_client::build_rpc_client(
            &Endpoint::localhost(),
            1_000,
            None,
        ))
        .sqlite_store(store_dir.join("store.sqlite3"))
        .authenticator(keystore)
        .in_debug_mode(DebugMode::Disabled)
        .build()
        .await
        .expect("offline MidenClientLib must build without a node")
}
