use crate::block_state::BlockState;
use crate::store::Store;
use crate::*;
use std::path::PathBuf;
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
    /// Miden client store directory (for building fresh clients)
    pub miden_store_dir: PathBuf,
    /// Miden node URL (for building fresh clients)
    pub miden_node_url: String,
    /// CORS-allowed origins (R11). `None` = no cross-origin requests permitted (the
    /// safe default in production); `Some(list)` = explicit allowlist; the special
    /// single-entry `vec!["*"]` is reserved for dev-only wildcards.
    pub cors_allowed_origins: Option<Vec<String>>,
    /// Admin API key (R1). `None` = `admin_*` JSON-RPC methods are disabled
    /// entirely (the safe production default — fail closed). `Some(token)` =
    /// admin requests must carry `Authorization: Bearer <token>`.
    pub admin_api_key: Option<String>,
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
            miden_store_dir: PathBuf::new(),
            miden_node_url: String::new(),
            cors_allowed_origins: None,
            admin_api_key: None,
        }
    }
}
