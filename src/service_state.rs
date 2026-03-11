use miden_agglayer_service::*;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServiceState {
    pub miden_client: Arc<MidenClient>,
    pub accounts: AccountsConfig,
    pub chain_id: u64,
    pub block_num_tracker: Arc<BlockNumTracker>,
    pub txn_manager: Arc<TxnManager>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ServiceState>();

impl ServiceState {
    pub fn new(
        miden_client: MidenClient,
        accounts: AccountsConfig,
        chain_id: u64,
        block_num_tracker: Arc<BlockNumTracker>,
        txn_manager: Arc<TxnManager>,
    ) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
            chain_id,
            block_num_tracker,
            txn_manager,
        }
    }
}
