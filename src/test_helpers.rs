//! Shared test utilities for the miden-agglayer crate.
//!
//! Provides `create_test_service()` and `test_accounts_config()` so that test
//! modules across the crate can set up a `ServiceState` without duplication
//! or `unsafe { std::mem::zeroed() }`.

use crate::accounts_config::{AccountsConfig as InnerAccountsConfig, AccountIdBech32};
use crate::block_state::BlockState;
use crate::store::memory::InMemoryStore;
use crate::{AccountsConfig, MidenClient, ServiceState};
use miden_protocol::account::AccountId;
use std::sync::Arc;

/// A valid hex-encoded AccountId used across all test fixtures.
const TEST_ACCOUNT_HEX: &str = "0x3d7c9747558851900f8206226dfbea";

fn dummy_account_id() -> AccountIdBech32 {
    AccountIdBech32(AccountId::from_hex(TEST_ACCOUNT_HEX).expect("valid test account ID"))
}

/// Build an `AccountsConfig` with valid (but dummy) account IDs.
pub fn test_accounts_config() -> AccountsConfig {
    AccountsConfig(InnerAccountsConfig {
        service: dummy_account_id(),
        bridge: dummy_account_id(),
        faucet_eth: dummy_account_id(),
        faucet_agg: dummy_account_id(),
        wallet_hardhat: dummy_account_id(),
    })
}

/// Create a `ServiceState` backed by `InMemoryStore` and a test `MidenClient`
/// stub (no real Miden node connection). Suitable for unit tests.
pub fn create_test_service() -> ServiceState {
    let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
    let block_state = Arc::new(BlockState::new());
    let miden_client = MidenClient::new_test();
    let accounts = test_accounts_config();
    ServiceState::new(
        miden_client,
        accounts,
        1,
        1,
        store,
        block_state,
        None,
        String::new(),
        String::new(),
    )
}
