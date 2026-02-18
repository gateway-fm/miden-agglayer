use crate::miden_client::SyncListener;
use alloy::primitives::BlockNumber;
use miden_client::sync::SyncSummary;
use std::sync::RwLock;

pub struct BlockNumTracker {
    latest: RwLock<BlockNumber>,
}

impl BlockNumTracker {
    pub fn new() -> Self {
        let latest = RwLock::new(0);
        Self { latest }
    }

    pub fn latest(&self) -> BlockNumber {
        *self.latest.read().unwrap()
    }
}

impl SyncListener for BlockNumTracker {
    fn on_sync(&self, summary: &SyncSummary) {
        let mut latest_ref = self.latest.write().unwrap();
        *latest_ref = summary.block_num.as_u64();
    }
}

impl Default for BlockNumTracker {
    fn default() -> Self {
        Self::new()
    }
}
