use crate::block_state::BlockState;
use crate::log_synthesis::LogStore;
use crate::*;
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
    pub claim_tracker: Arc<ClaimTracker>,
    pub nonce_tracker: Arc<NonceTracker>,
    pub address_mapper: Arc<AddressMapper>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ServiceState>();

impl ServiceState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        miden_client: MidenClient,
        accounts: AccountsConfig,
        chain_id: u64,
        block_num_tracker: Arc<BlockNumTracker>,
        txn_manager: Arc<TxnManager>,
        block_state: Arc<BlockState>,
        log_store: Arc<LogStore>,
        claim_tracker: Arc<ClaimTracker>,
        nonce_tracker: Arc<NonceTracker>,
        address_mapper: Arc<AddressMapper>,
    ) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
            chain_id,
            block_num_tracker,
            txn_manager,
            block_state,
            log_store,
            claim_tracker,
            nonce_tracker,
            address_mapper,
        }
    }
}
