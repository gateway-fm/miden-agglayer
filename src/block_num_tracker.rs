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

    /// Advance the block number by 1 and return the new value.
    /// Used to ensure synthetic logs are placed at a block the BridgeL2Sync
    /// hasn't scanned yet.
    pub fn advance(&self) -> BlockNumber {
        let mut latest_ref = self.latest.write().unwrap();
        *latest_ref += 1;
        *latest_ref
    }
}

#[async_trait::async_trait]
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
