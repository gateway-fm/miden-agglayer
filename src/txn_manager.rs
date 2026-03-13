use crate::block_state::BlockState;
use crate::log_synthesis::{LogStore, SyntheticLog};
use crate::miden_client::{MidenClientLib, SyncListener};
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::{Recovered, SignerRecoverable};
use alloy::primitives::{Address, BlockNumber, LogData, TxHash};
use alloy_rpc_types_eth::{Filter, Log};
use lru::LruCache;
use miden_client::sync::SyncSummary;
use miden_protocol::note::NoteId;
use miden_protocol::transaction::TransactionId;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// Maximum blocks to wait for CLAIM note consumption before reverting.
const CLAIM_CONSUMPTION_TIMEOUT_BLOCKS: u64 = 50;

#[derive(Debug)]
struct TxnReceipt {
    id: Option<TransactionId>,
    envelope: TxEnvelope,
    signer: Address,
    expires_at: Option<BlockNumber>,

    result: Option<Result<(), String>>,
    block_num: BlockNumber,
    logs: Vec<LogData>,

    /// CLAIM note ID for consumption tracking (deferred receipts)
    claim_note_id: Option<String>,
    /// Block at which the claim was submitted (for timeout tracking)
    claim_submit_block: Option<BlockNumber>,
}

pub struct TxnManager {
    transactions: Mutex<LruCache<TxHash, TxnReceipt>>,
    log_store: Arc<LogStore>,
    block_state: Arc<BlockState>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<TxnManager>();

impl TxnManager {
    pub fn new(log_store: Arc<LogStore>, block_state: Arc<BlockState>) -> Self {
        let transactions = LruCache::new(NonZeroUsize::new(10_000).unwrap());
        Self {
            transactions: Mutex::new(transactions),
            log_store,
            block_state,
        }
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
            claim_note_id: None,
            claim_submit_block: None,
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
                tracing::info!("TxnManager: committed eth txn: {txn_hash}; miden txn: {txn_id:?}");

                // Add logs to LogStore immediately on commit.
                // This ensures bridge-service sees events as soon as txn is finalized.
                let logs = receipt.logs.clone();
                let signer = receipt.signer;
                let block_hash = self.block_state.get_block_hash(block_num);
                for log_data in logs {
                    let log = SyntheticLog {
                        address: format!("{:#x}", signer),
                        topics: log_data.topics().iter().map(|t| t.to_string()).collect(),
                        data: log_data.data.to_string(),
                        block_number: block_num,
                        block_hash,
                        transaction_hash: format!("{txn_hash:#x}"),
                        transaction_index: 0,
                        log_index: 0,
                        removed: false,
                    };
                    self.log_store.add_log(log);
                }
            }
            Some(Err(err)) => tracing::error!(
                "TxnManager: failed eth txn: {txn_hash}; miden txn: {txn_id:?}; reason: {err}"
            ),
            None => {}
        }
        Ok(())
    }

    /// Mark a transaction as awaiting CLAIM note consumption.
    /// Receipt will return None (pending) until consumption is confirmed or times out.
    pub fn begin_awaiting_consumption(
        &self,
        txn_hash: TxHash,
        claim_note_id: String,
        submit_block: BlockNumber,
    ) -> anyhow::Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        let Some(receipt) = transactions.get_mut(&txn_hash) else {
            anyhow::bail!("TxnManager: transaction {txn_hash} not found");
        };
        receipt.claim_note_id = Some(claim_note_id);
        receipt.claim_submit_block = Some(submit_block);
        Ok(())
    }

    /// Check if a transaction is awaiting CLAIM note consumption.
    pub fn is_awaiting_consumption(&self, txn_hash: TxHash) -> Option<(String, BlockNumber)> {
        let transactions = self.transactions.lock().unwrap();
        let receipt = transactions.peek(&txn_hash)?;
        if receipt.result.is_some() {
            return None; // already finalized
        }
        let note_id = receipt.claim_note_id.clone()?;
        let submit_block = receipt.claim_submit_block?;
        Some((note_id, submit_block))
    }

    /// Check if a claim has timed out (exceeded CLAIM_CONSUMPTION_TIMEOUT_BLOCKS).
    pub fn check_claim_timeout(&self, txn_hash: TxHash, current_block: BlockNumber) -> bool {
        let transactions = self.transactions.lock().unwrap();
        let Some(receipt) = transactions.peek(&txn_hash) else {
            return false;
        };
        if receipt.result.is_some() {
            return false;
        }
        if let Some(submit_block) = receipt.claim_submit_block
            && current_block > submit_block + CLAIM_CONSUMPTION_TIMEOUT_BLOCKS
        {
            return true;
        }
        false
    }

    pub fn receipt(&self, txn_hash: TxHash) -> Option<(Result<(), String>, BlockNumber)> {
        let transactions = self.transactions.lock().unwrap();
        let receipt = transactions.peek(&txn_hash)?;
        // If awaiting consumption (has claim_note_id), return None (pending) until cleared
        if receipt.claim_note_id.is_some() {
            return None;
        }
        let result = receipt.result.clone()?;
        Some((result, receipt.block_num))
    }

    pub fn txn(&self, txn_hash: TxHash) -> Option<alloy::rpc::types::Transaction> {
        let transactions = self.transactions.lock().unwrap();
        let receipt = transactions.peek(&txn_hash)?;
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

    fn commit_pending(&self, ids: &[TransactionId], block_num: BlockNumber) {
        for id in ids {
            if let Some(hash) = self.pending_txn_by_id(*id)
                && let Err(e) = self.commit(hash, Ok(()), block_num)
            {
                tracing::warn!("Failed to commit transaction {hash}: {e}");
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
            if let Err(e) = self.commit(hash, Err(String::from("expired")), block_num) {
                tracing::warn!("Failed to expire transaction {hash}: {e}");
            }
        }
    }

    fn expire_timed_out_claims(&self, block_num: BlockNumber) {
        let timed_out: Vec<TxHash> = {
            let transactions = self.transactions.lock().unwrap();
            transactions
                .iter()
                .filter_map(|(hash, receipt)| {
                    if receipt.result.is_some() {
                        return None;
                    }
                    let submit_block = receipt.claim_submit_block?;
                    if block_num > submit_block + CLAIM_CONSUMPTION_TIMEOUT_BLOCKS {
                        Some(*hash)
                    } else {
                        None
                    }
                })
                .collect()
        };

        for hash in timed_out {
            tracing::warn!(
                "Claim transaction {hash} timed out after {CLAIM_CONSUMPTION_TIMEOUT_BLOCKS} blocks"
            );
            if let Err(e) = self.commit(
                hash,
                Err(String::from("claim consumption timeout")),
                block_num,
            ) {
                tracing::warn!("Failed to timeout claim {hash}: {e}");
            }
        }
    }
}

#[async_trait::async_trait]
impl SyncListener for TxnManager {
    fn on_sync(&self, summary: &SyncSummary) {
        let block_num = summary.block_num.as_u64();
        self.commit_pending(&summary.committed_transactions, block_num);
        self.expire_pending(block_num);
        self.expire_timed_out_claims(block_num);
    }

    async fn on_post_sync(&self, client: &mut MidenClientLib) -> anyhow::Result<()> {
        let awaiting: Vec<(TxHash, String)> = {
            let transactions = self.transactions.lock().unwrap();
            transactions
                .iter()
                .filter_map(|(hash, receipt)| {
                    // Only check consumption if the creation txn is already committed
                    if receipt.result.is_some() {
                        receipt.claim_note_id.as_ref().map(|id| (*hash, id.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        for (txn_hash, note_id_str) in awaiting {
            let note_id = NoteId::try_from_hex(&note_id_str)
                .map_err(|e| anyhow::anyhow!("bad note id {note_id_str}: {e}"))?;

            let note_opt = client
                .get_input_note(note_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to get note {note_id_str}: {e}"))?;

            if let Some(note) = note_opt
                && note.is_consumed()
            {
                tracing::info!("CLAIM note {note_id_str} consumed, finalizing eth txn {txn_hash}");

                let mut transactions = self.transactions.lock().unwrap();
                if let Some(receipt) = transactions.get_mut(&txn_hash) {
                    receipt.claim_note_id = None;
                    let logs = receipt.logs.clone();
                    let block_num = receipt.block_num;
                    let signer = receipt.signer;
                    drop(transactions);

                    for log_data in logs {
                        let block_hash = self.block_state.get_block_hash(block_num);
                        let log = SyntheticLog {
                            address: format!("{:#x}", signer),
                            topics: log_data.topics().iter().map(|t| t.to_string()).collect(),
                            data: log_data.data.to_string(),
                            block_number: block_num,
                            block_hash,
                            transaction_hash: format!("{txn_hash:#x}"),
                            transaction_index: 0,
                            log_index: 0,
                            removed: false,
                        };
                        self.log_store.add_log(log);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::Signed;
    use alloy::primitives::B256;
    use alloy::primitives::Signature;

    fn create_test_txn_manager() -> (Arc<TxnManager>, Arc<LogStore>, Arc<BlockState>) {
        let log_store = Arc::new(LogStore::new());
        let block_state = Arc::new(BlockState::new());
        let txn_manager = Arc::new(TxnManager::new(log_store.clone(), block_state.clone()));
        (txn_manager, log_store, block_state)
    }

    #[test]
    fn test_txn_manager_lifecycle() {
        let (txn_manager, _log_store, _block_state) = create_test_txn_manager();
        let txn_hash = TxHash::from([1u8; 32]);
        let txn_envelope = TxEnvelope::Legacy(Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            Signature::test_signature(),
            txn_hash,
        ));

        // Not found
        assert!(txn_manager.receipt(txn_hash).is_none());

        // Begin
        txn_manager
            .begin(txn_hash, None, txn_envelope, None, vec![])
            .unwrap();
        assert!(txn_manager.receipt(txn_hash).is_none());

        // Commit
        txn_manager.commit(txn_hash, Ok(()), 42).unwrap();
        let (res, block_num) = txn_manager.receipt(txn_hash).unwrap();
        assert!(res.is_ok());
        assert_eq!(block_num, 42);

        // Logs should be added to LogStore if we had any
    }

    #[test]
    fn test_txn_manager_with_logs() {
        let (txn_manager, log_store, _block_state) = create_test_txn_manager();
        let txn_hash = TxHash::from([2u8; 32]);
        let txn_envelope = TxEnvelope::Legacy(Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            Signature::test_signature(),
            txn_hash,
        ));

        let log_data = LogData::new_unchecked(
            vec![B256::from([0xaa; 32])],
            alloy::primitives::bytes!("aabbcc"),
        );
        txn_manager
            .begin(txn_hash, None, txn_envelope, None, vec![log_data])
            .unwrap();
        txn_manager.commit(txn_hash, Ok(()), 100).unwrap();

        let filter = crate::log_synthesis::LogFilter::default();
        let logs = log_store.get_logs(&filter, 100);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].transaction_hash, format!("{txn_hash:#x}"));
    }
}
