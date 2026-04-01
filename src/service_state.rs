use crate::block_state::BlockState;
use crate::store::Store;
use crate::*;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServiceState {
    pub miden_client: Arc<MidenClient>,
    pub accounts: AccountsConfig,
    pub chain_id: u64,
    /// Rollup network ID from RollupManager (used for bridge's networkID() call)
    pub network_id: u64,
    pub store: Arc<dyn Store>,
    pub block_state: Arc<BlockState>,
    /// L1 RPC URL for resolving exit roots from the L1 GER contract
    pub l1_rpc_url: Option<String>,
    /// L1 GER contract address
    pub ger_l1_address: Option<String>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ServiceState>();

impl ServiceState {
    pub fn new(
        miden_client: MidenClient,
        accounts: AccountsConfig,
        chain_id: u64,
        network_id: u64,
        store: Arc<dyn Store>,
        block_state: Arc<BlockState>,
    ) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
            chain_id,
            network_id,
            store,
            block_state,
            l1_rpc_url: None,
            ger_l1_address: None,
        }
    }
}
