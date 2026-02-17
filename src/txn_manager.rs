use alloy::primitives::TxHash;
use lru::LruCache;
use miden_protocol::transaction::TransactionId;
use std::num::NonZeroUsize;
use std::sync::Mutex;

#[derive(Debug, Default)]
struct TxnReceipt {
    _id: Option<TransactionId>,
    result: Option<Result<(), String>>,
    block_num: u64,
}

pub struct TxnManager {
    transactions: Mutex<LruCache<TxHash, TxnReceipt>>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<TxnManager>();

impl TxnManager {
    pub fn new() -> Self {
        let transactions = LruCache::new(NonZeroUsize::new(64).unwrap());
        Self { transactions: Mutex::new(transactions) }
    }

    pub fn begin(&self, txn_hash: TxHash, txn_id: Option<TransactionId>) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        if transactions.contains(&txn_hash) {
            anyhow::bail!("TxnManager: transaction {txn_hash} already exists");
        }
        let receipt = TxnReceipt { _id: txn_id, ..Default::default() };
        _ = transactions.put(txn_hash, receipt);
        Ok(())
    }

    pub fn commit(
        &self,
        txn_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        let Some(receipt) = transactions.get_mut(&txn_hash) else {
            anyhow::bail!("TxnManager: transaction {txn_hash} not found");
        };
        receipt.result = Some(result);
        receipt.block_num = block_num;
        Ok(())
    }

    pub fn receipt(&self, txn_hash: TxHash) -> Option<(Result<(), String>, u64)> {
        let mut transactions = self.transactions.lock().unwrap();
        let receipt = transactions.get(&txn_hash)?;
        let result = receipt.result.clone()?;
        Some((result, receipt.block_num))
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::new()
    }
}
