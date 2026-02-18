use crate::miden_client::SyncListener;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::{Recovered, SignerRecoverable};
use alloy::primitives::{Address, BlockNumber, LogData, TxHash};
use alloy_rpc_types_eth::{Filter, Log};
use lru::LruCache;
use miden_client::sync::SyncSummary;
use miden_protocol::transaction::TransactionId;
use std::num::NonZeroUsize;
use std::sync::Mutex;

#[derive(Debug)]
struct TxnReceipt {
    _id: Option<TransactionId>,
    envelope: TxEnvelope,
    signer: Address,
    result: Option<Result<(), String>>,
    block_num: BlockNumber,
    logs: Vec<Log>,
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

    pub fn begin(
        &self,
        txn_hash: TxHash,
        txn_id: Option<TransactionId>,
        txn_envelope: TxEnvelope,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        if transactions.contains(&txn_hash) {
            anyhow::bail!("TxnManager: transaction {txn_hash} already exists");
        }
        let signer = txn_envelope.recover_signer()?;
        let receipt = TxnReceipt {
            _id: txn_id,
            envelope: txn_envelope,
            signer,
            result: None,
            block_num: 0,
            logs: Vec::new(),
        };
        _ = transactions.put(txn_hash, receipt);
        Ok(())
    }

    pub fn commit(
        &self,
        txn_hash: TxHash,
        result: Result<(), String>,
        block_num: BlockNumber,
        logs: Vec<LogData>,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        let Some(receipt) = transactions.get_mut(&txn_hash) else {
            anyhow::bail!("TxnManager: transaction {txn_hash} not found");
        };
        receipt.result = Some(result);
        receipt.block_num = block_num;
        receipt.logs = logs
            .into_iter()
            .map(|log_data| -> Log {
                let mut log: Log<LogData> = Log::<LogData>::default();
                log.inner.data = log_data;
                log.transaction_hash = Some(txn_hash);
                log.block_number = Some(block_num);
                log
            })
            .collect();
        Ok(())
    }

    pub fn receipt(&self, txn_hash: TxHash) -> Option<(Result<(), String>, BlockNumber)> {
        let mut transactions = self.transactions.lock().unwrap();
        let receipt = transactions.get(&txn_hash)?;
        let result = receipt.result.clone()?;
        Some((result, receipt.block_num))
    }

    pub fn committed_txn(&self, txn_hash: TxHash) -> Option<alloy::rpc::types::Transaction> {
        let mut transactions = self.transactions.lock().unwrap();
        let receipt = transactions.get(&txn_hash)?;
        let envelope = receipt.envelope.clone();
        let txn = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(envelope, receipt.signer),
            block_hash: None,
            block_number: Some(receipt.block_num),
            transaction_index: None,
            effective_gas_price: None,
        };
        Some(txn)
    }

    pub fn logs(&self, filter: Filter) -> Vec<Log> {
        tracing::trace!("TxnManager.logs filter: {:?}", filter);
        let mut results = Vec::new();
        let transactions = self.transactions.lock().unwrap();
        for (_, receipt) in transactions.iter() {
            for log in &receipt.logs {
                let matches_block_range =
                    filter.matches_block_range(log.block_number.unwrap_or_default());
                if !matches_block_range {
                    continue;
                }
                let matches_topics = filter.matches_topics(log.topics());
                if !matches_topics {
                    continue;
                }
                results.push(log.clone());
            }
        }

        results
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncListener for TxnManager {
    fn on_sync(&self, _summary: &SyncSummary) {
        // TODO: update result and block_num on pending transactions
    }
}
