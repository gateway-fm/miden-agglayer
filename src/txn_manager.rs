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
    id: Option<TransactionId>,
    envelope: TxEnvelope,
    signer: Address,
    expires_at: Option<BlockNumber>,

    result: Option<Result<(), String>>,
    block_num: BlockNumber,
    logs: Vec<LogData>,
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
        expires_at: Option<BlockNumber>,
        logs: Vec<LogData>,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        if transactions.contains(&txn_hash) {
            anyhow::bail!("TxnManager: transaction {txn_hash} already exists");
        }
        let signer = txn_envelope.recover_signer()?;
        let receipt = TxnReceipt {
            id: txn_id,
            envelope: txn_envelope,
            signer,
            expires_at,
            result: None,
            block_num: 0,
            logs,
        };
        _ = transactions.put(txn_hash, receipt);
        Ok(())
    }

    pub fn commit(
        &self,
        txn_hash: TxHash,
        result: Result<(), String>,
        block_num: BlockNumber,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        let Some(receipt) = transactions.get_mut(&txn_hash) else {
            anyhow::bail!("TxnManager: transaction {txn_hash} not found");
        };
        receipt.result = Some(result);
        receipt.block_num = block_num;

        let txn_id = &receipt.id;
        match &receipt.result {
            Some(Ok(_)) => {
                tracing::info!("TxnManager: committed eth txn: {txn_hash}; miden txn: {txn_id:?}")
            },
            Some(Err(err)) => tracing::error!(
                "TxnManager: failed eth txn: {txn_hash}; miden txn: {txn_id:?}; reason: {err}"
            ),
            None => {},
        }
        Ok(())
    }

    pub fn receipt(&self, txn_hash: TxHash) -> Option<(Result<(), String>, BlockNumber)> {
        let mut transactions = self.transactions.lock().unwrap();
        let receipt = transactions.get(&txn_hash)?;
        let result = receipt.result.clone()?;
        Some((result, receipt.block_num))
    }

    pub fn txn(&self, txn_hash: TxHash) -> Option<alloy::rpc::types::Transaction> {
        let mut transactions = self.transactions.lock().unwrap();
        let receipt = transactions.get(&txn_hash)?;
        let envelope = receipt.envelope.clone();
        let txn = alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(envelope, receipt.signer),
            block_hash: None,
            block_number: if receipt.result.is_some() {
                Some(receipt.block_num)
            } else {
                None
            },
            transaction_index: None,
            effective_gas_price: None,
        };
        Some(txn)
    }

    fn make_log(log_data: LogData, txn_hash: TxHash, block_num: BlockNumber) -> Log {
        let mut log = Log::<LogData>::default();
        log.inner.data = log_data;
        log.transaction_hash = Some(txn_hash);
        log.block_number = Some(block_num);
        log
    }

    pub fn logs(&self, filter: Filter) -> Vec<Log> {
        tracing::trace!("TxnManager.logs filter: {:?}", filter);
        let mut results: Vec<Log> = Vec::new();
        let transactions = self.transactions.lock().unwrap();
        for (txn_hash, receipt) in transactions.iter() {
            for log_data in &receipt.logs {
                let matches_block_range = filter.matches_block_range(receipt.block_num);
                if !matches_block_range {
                    continue;
                }
                let matches_topics = filter.matches_topics(log_data.topics());
                if !matches_topics {
                    continue;
                }
                let log = Self::make_log(log_data.clone(), *txn_hash, receipt.block_num);
                results.push(log);
            }
        }

        results
    }

    pub fn pending_txn_by_id(&self, id: TransactionId) -> Option<TxHash> {
        let transactions = self.transactions.lock().unwrap();
        for (txn_hash, receipt) in transactions.iter() {
            if receipt.result.is_none() && (receipt.id == Some(id)) {
                return Some(*txn_hash);
            }
        }
        None
    }

    fn commit_pending(&self, ids: &Vec<TransactionId>, block_num: BlockNumber) {
        for id in ids {
            if let Some(hash) = self.pending_txn_by_id(*id) {
                _ = self.commit(hash, Ok(()), block_num);
            }
        }
    }

    fn expired_pending(&self, block_num: BlockNumber) -> Vec<TxHash> {
        let mut results = Vec::<TxHash>::new();
        let transactions = self.transactions.lock().unwrap();
        for (txn_hash, receipt) in transactions.iter() {
            if receipt.result.is_none()
                && (block_num >= receipt.expires_at.unwrap_or(BlockNumber::MAX))
            {
                results.push(*txn_hash);
            }
        }
        results
    }

    fn expire_pending(&self, block_num: BlockNumber) {
        let expired_hashes = self.expired_pending(block_num);
        for hash in expired_hashes {
            _ = self.commit(hash, Err(String::from("expired")), block_num);
        }
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncListener for TxnManager {
    fn on_sync(&self, summary: &SyncSummary) {
        self.commit_pending(&summary.committed_transactions, summary.block_num.as_u64());
        self.expire_pending(summary.block_num.as_u64());
    }
}
