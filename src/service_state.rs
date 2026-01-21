use crate::AccountsConfig;
use crate::MidenClient;
use std::sync::Arc;

#[derive(Clone)]
pub struct ServiceState {
    pub miden_client: Arc<MidenClient>,
    pub accounts: AccountsConfig,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ServiceState>();

impl ServiceState {
    pub fn new(miden_client: MidenClient, accounts: AccountsConfig) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
        }
    }
}
