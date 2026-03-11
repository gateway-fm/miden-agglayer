use miden_agglayer_service::block_state::BlockState;
use miden_agglayer_service::log_synthesis::LogStore;
use miden_agglayer_service::*;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServiceState {
    pub miden_client: Arc<MidenClient>,
    pub accounts: AccountsConfig,
    pub chain_id: u64,
    pub block_num_tracker: Arc<BlockNumTracker>,
    pub txn_manager: Arc<TxnManager>,
    pub block_state: Arc<BlockState>,
    pub log_store: Arc<LogStore>,
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
        block_state: Arc<BlockState>,
        log_store: Arc<LogStore>,
    ) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
            chain_id,
            block_num_tracker,
            txn_manager,
            block_state,
            log_store,
        }
    }
}
