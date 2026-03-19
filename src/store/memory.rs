//! In-memory Store implementation — wraps HashMap/RwLock data structures.

use super::{Store, TxnData, TxnEntry};
use crate::log_synthesis::{
    GerEntry, L2_GLOBAL_EXIT_ROOT_ADDRESS, LogFilter, SyntheticLog, UPDATE_HASH_CHAIN_VALUE_TOPIC,
};
use alloy::primitives::{Address, LogData, TxHash, U256};
use lru::LruCache;
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;
use parking_lot::{Mutex, RwLock};
use sha3::{Digest, Keccak256};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

struct TxnReceipt {
    id: Option<TransactionId>,
    envelope: alloy::consensus::TxEnvelope,
    signer: Address,
    expires_at: Option<u64>,
    result: Option<Result<(), String>>,
    block_num: u64,
    logs: Vec<LogData>,
}

pub struct InMemoryStore {
    // Block number
    latest_block_number: RwLock<u64>,

    // Logs
    logs_by_block: RwLock<HashMap<u64, Vec<SyntheticLog>>>,
    logs_by_tx: RwLock<HashMap<String, Vec<SyntheticLog>>>,
    log_counter: RwLock<u64>,
    pending_events: RwLock<Vec<SyntheticLog>>,

    // GER
    seen_gers: RwLock<HashMap<[u8; 32], GerEntry>>,
    latest_ger: RwLock<Option<[u8; 32]>>,
    hash_chain_value: RwLock<[u8; 32]>,
    injected_gers: RwLock<HashSet<[u8; 32]>>,

    // Transactions
    transactions: Mutex<LruCache<TxHash, TxnReceipt>>,

    // Nonces
    nonces: RwLock<HashMap<String, u64>>,

    // Claims
    claimed: RwLock<HashSet<U256>>,

    // Address mappings
    address_mappings: RwLock<HashMap<Address, AccountId>>,

    // Bridge-out
    processed_notes: RwLock<HashSet<String>>,
    deposit_counter: RwLock<u32>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<InMemoryStore>();

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            latest_block_number: RwLock::new(0),
            logs_by_block: RwLock::new(HashMap::new()),
            logs_by_tx: RwLock::new(HashMap::new()),
            log_counter: RwLock::new(0),
            pending_events: RwLock::new(Vec::new()),
            seen_gers: RwLock::new(HashMap::new()),
            latest_ger: RwLock::new(None),
            hash_chain_value: RwLock::new([0u8; 32]),
            injected_gers: RwLock::new(HashSet::new()),
            transactions: Mutex::new(LruCache::new(NonZeroUsize::new(10_000).unwrap())),
            nonces: RwLock::new(HashMap::new()),
            claimed: RwLock::new(HashSet::new()),
            address_mappings: RwLock::new(HashMap::new()),
            processed_notes: RwLock::new(HashSet::new()),
            deposit_counter: RwLock::new(0),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Store for InMemoryStore {
    // ── Block number ─────────────────────────────────────────────

    async fn get_latest_block_number(&self) -> anyhow::Result<u64> {
        Ok(*self.latest_block_number.read())
    }

    async fn set_latest_block_number(&self, n: u64) -> anyhow::Result<()> {
        *self.latest_block_number.write() = n;
        Ok(())
    }

    async fn advance_block_number(&self) -> anyhow::Result<u64> {
        let mut num = self.latest_block_number.write();
        *num += 1;
        Ok(*num)
    }

    // ── Logs ─────────────────────────────────────────────────────

    async fn add_log(&self, mut log: SyntheticLog) -> anyhow::Result<()> {
        let mut counter = self.log_counter.write();
        log.log_index = *counter;
        *counter += 1;
        drop(counter);

        let block_num = log.block_number;
        let tx_hash = log.transaction_hash.to_lowercase();

        tracing::debug!(
            tx_hash = %tx_hash,
            block_number = block_num,
            topic0 = log.topics.first().map(|t| &t[..20.min(t.len())]).unwrap_or("none"),
            "Store: adding log"
        );

        self.logs_by_block
            .write()
            .entry(block_num)
            .or_default()
            .push(log.clone());

        self.logs_by_tx
            .write()
            .entry(tx_hash)
            .or_default()
            .push(log.clone());

        self.pending_events.write().push(log);
        Ok(())
    }

    async fn get_logs(
        &self,
        filter: &LogFilter,
        current_block: u64,
    ) -> anyhow::Result<Vec<SyntheticLog>> {
        let from = filter.from_block_number(current_block);
        let to = filter.to_block_number(current_block);

        // Drain pending events up to `to`
        {
            let mut pending = self.pending_events.write();
            let mut remaining = Vec::new();
            for evt in pending.drain(..) {
                if evt.block_number > to {
                    remaining.push(evt);
                }
            }
            *pending = remaining;
        }

        let mut result = Vec::new();
        let logs_by_block = self.logs_by_block.read();
        for block_num in from..=to {
            if let Some(logs) = logs_by_block.get(&block_num) {
                for log in logs {
                    if filter.matches(log, current_block) {
                        result.push(log.clone());
                    }
                }
            }
            if result.len() >= 1000 {
                break;
            }
        }
        Ok(result)
    }

    async fn get_logs_for_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<SyntheticLog>> {
        let key = tx_hash.to_lowercase();
        let map = self.logs_by_tx.read();
        let result = map.get(&key).cloned().unwrap_or_default();
        if result.is_empty() {
            let stored_keys: Vec<&String> = map.keys().collect();
            tracing::debug!(
                lookup_key = %key,
                stored_count = stored_keys.len(),
                stored_keys = ?stored_keys.iter().take(10).collect::<Vec<_>>(),
                "Store: get_logs_for_tx miss"
            );
        }
        Ok(result)
    }

    // ── GER ──────────────────────────────────────────────────────

    async fn has_seen_ger(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(self.seen_gers.read().contains_key(ger))
    }

    async fn mark_ger_seen(&self, ger: &[u8; 32], entry: GerEntry) -> anyhow::Result<bool> {
        let mut seen = self.seen_gers.write();
        if seen.contains_key(ger) {
            Ok(false)
        } else {
            seen.insert(*ger, entry);
            *self.latest_ger.write() = Some(*ger);
            Ok(true)
        }
    }

    async fn get_latest_ger(&self) -> anyhow::Result<Option<[u8; 32]>> {
        Ok(*self.latest_ger.read())
    }

    async fn get_ger_entry(&self, ger: &[u8; 32]) -> anyhow::Result<Option<GerEntry>> {
        Ok(self.seen_gers.read().get(ger).cloned())
    }

    async fn set_ger_exit_roots(
        &self,
        ger: &[u8; 32],
        mainnet_exit_root: [u8; 32],
        rollup_exit_root: [u8; 32],
    ) -> anyhow::Result<()> {
        let mut seen = self.seen_gers.write();
        let entry = seen.entry(*ger).or_insert(GerEntry {
            mainnet_exit_root: None,
            rollup_exit_root: None,
            block_number: 0,
            timestamp: 0,
        });
        entry.mainnet_exit_root = Some(mainnet_exit_root);
        entry.rollup_exit_root = Some(rollup_exit_root);
        Ok(())
    }

    async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(self.injected_gers.read().contains(ger))
    }

    async fn mark_ger_injected(&self, ger: [u8; 32]) -> anyhow::Result<()> {
        self.injected_gers.write().insert(ger);
        Ok(())
    }

    async fn add_ger_update_event(
        &self,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_exit_root: &[u8; 32],
        mainnet_exit_root: Option<[u8; 32]>,
        rollup_exit_root: Option<[u8; 32]>,
        timestamp: u64,
    ) -> anyhow::Result<()> {
        self.mark_ger_seen(
            global_exit_root,
            GerEntry {
                mainnet_exit_root,
                rollup_exit_root,
                block_number,
                timestamp,
            },
        )
        .await?;

        let new_hash_chain = {
            let mut hash_chain = self.hash_chain_value.write();
            let mut hasher = Keccak256::new();
            hasher.update(*hash_chain);
            hasher.update(global_exit_root);
            let result: [u8; 32] = hasher.finalize().into();
            *hash_chain = result;
            result
        };

        let log = SyntheticLog {
            address: L2_GLOBAL_EXIT_ROOT_ADDRESS.to_string(),
            topics: vec![
                UPDATE_HASH_CHAIN_VALUE_TOPIC.to_string(),
                format!("0x{}", hex::encode(global_exit_root)),
                format!("0x{}", hex::encode(new_hash_chain)),
            ],
            data: "0x".to_string(),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log).await
    }

    // ── Transactions ─────────────────────────────────────────────

    async fn txn_begin(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<()> {
        let mut txns = self.transactions.lock();
        if txns.contains(&tx_hash) {
            anyhow::bail!("Store: transaction {tx_hash} already exists");
        }
        let receipt = TxnReceipt {
            id: entry.id,
            envelope: entry.envelope,
            signer: entry.signer,
            expires_at: entry.expires_at,
            result: None,
            block_num: 0,
            logs: entry.logs,
        };
        let _ = txns.put(tx_hash, receipt);
        Ok(())
    }

    async fn txn_commit(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()> {
        let logs_to_add = {
            let mut txns = self.transactions.lock();
            let Some(receipt) = txns.get_mut(&tx_hash) else {
                anyhow::bail!("Store: transaction {tx_hash} not found");
            };
            receipt.result = Some(result);
            receipt.block_num = block_num;

            match &receipt.result {
                Some(Ok(_)) => {
                    tracing::info!(
                        "Store: committed txn {tx_hash}; miden txn: {:?}",
                        receipt.id
                    );
                    Some(receipt.logs.clone())
                }
                Some(Err(err)) => {
                    tracing::error!(
                        "Store: failed txn {tx_hash}; miden txn: {:?}; reason: {err}",
                        receipt.id
                    );
                    None
                }
                None => None,
            }
        }; // Mutex dropped before any .await

        if let Some(logs) = logs_to_add {
            let bridge_address = crate::bridge_address::get_bridge_address().to_string();
            for log_data in logs {
                let log = SyntheticLog {
                    address: bridge_address.clone(),
                    topics: log_data.topics().iter().map(|t| t.to_string()).collect(),
                    data: log_data.data.to_string(),
                    block_number: block_num,
                    block_hash,
                    transaction_hash: format!("{tx_hash:#x}"),
                    transaction_index: 0,
                    log_index: 0,
                    removed: false,
                };
                self.add_log(log).await?;
            }
        }
        Ok(())
    }

    async fn txn_receipt(
        &self,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<(Result<(), String>, u64)>> {
        let txns = self.transactions.lock();
        let Some(receipt) = txns.peek(&tx_hash) else {
            return Ok(None);
        };
        if receipt.result.is_none() {
            tracing::debug!("Store::txn_receipt: {tx_hash} exists but result=None (uncommitted)");
            return Ok(None);
        }
        let Some(result) = receipt.result.clone() else {
            return Ok(None);
        };
        Ok(Some((result, receipt.block_num)))
    }

    async fn txn_get(&self, tx_hash: TxHash) -> anyhow::Result<Option<TxnData>> {
        let txns = self.transactions.lock();
        let Some(receipt) = txns.peek(&tx_hash) else {
            return Ok(None);
        };
        Ok(Some(TxnData {
            id: receipt.id,
            envelope: receipt.envelope.clone(),
            signer: receipt.signer,
            expires_at: receipt.expires_at,
            result: receipt.result.clone(),
            block_num: receipt.block_num,
            logs: receipt.logs.clone(),
        }))
    }

    async fn txn_pending_by_miden_id(&self, id: TransactionId) -> anyhow::Result<Option<TxHash>> {
        let txns = self.transactions.lock();
        for (tx_hash, receipt) in txns.iter() {
            if receipt.result.is_none() && receipt.id == Some(id) {
                return Ok(Some(*tx_hash));
            }
        }
        Ok(None)
    }

    async fn txn_commit_pending(
        &self,
        ids: &[TransactionId],
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()> {
        for id in ids {
            if let Some(hash) = self.txn_pending_by_miden_id(*id).await?
                && let Err(e) = self.txn_commit(hash, Ok(()), block_num, block_hash).await
            {
                tracing::warn!("Failed to commit transaction {hash}: {e}");
            }
        }
        Ok(())
    }

    async fn txn_expire_pending(&self, block_num: u64, block_hash: [u8; 32]) -> anyhow::Result<()> {
        let expired: Vec<TxHash> = {
            let txns = self.transactions.lock();
            txns.iter()
                .filter(|(_, r)| {
                    r.result.is_none() && block_num >= r.expires_at.unwrap_or(u64::MAX)
                })
                .map(|(h, _)| *h)
                .collect()
        };
        for hash in expired {
            if let Err(e) = self
                .txn_commit(hash, Err("expired".to_string()), block_num, block_hash)
                .await
            {
                tracing::warn!("Failed to expire transaction {hash}: {e}");
            }
        }
        Ok(())
    }

    // ── Nonces ───────────────────────────────────────────────────

    async fn nonce_get(&self, addr: &str) -> anyhow::Result<u64> {
        Ok(*self.nonces.read().get(&addr.to_lowercase()).unwrap_or(&0))
    }

    async fn nonce_increment(&self, addr: &str) -> anyhow::Result<u64> {
        let key = addr.to_lowercase();
        let mut nonces = self.nonces.write();
        let nonce = nonces.entry(key).or_insert(0);
        let prev = *nonce;
        *nonce += 1;
        Ok(prev)
    }

    // ── Claims ───────────────────────────────────────────────────

    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
        let mut claimed = self.claimed.write();
        if !claimed.insert(global_index) {
            anyhow::bail!("claim already submitted for global_index {global_index}");
        }
        Ok(())
    }

    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()> {
        self.claimed.write().remove(global_index);
        Ok(())
    }

    async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool> {
        Ok(self.claimed.read().contains(global_index))
    }

    // ── Address mappings ─────────────────────────────────────────

    async fn get_address_mapping(&self, eth: &Address) -> anyhow::Result<Option<AccountId>> {
        Ok(self.address_mappings.read().get(eth).copied())
    }

    async fn set_address_mapping(&self, eth: Address, miden: AccountId) -> anyhow::Result<()> {
        self.address_mappings.write().insert(eth, miden);
        Ok(())
    }

    // ── Bridge-out ───────────────────────────────────────────────

    async fn is_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
        Ok(self.processed_notes.read().contains(note_id))
    }

    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32> {
        self.processed_notes.write().insert(note_id);
        let mut counter = self.deposit_counter.write();
        let deposit_count = *counter;
        *counter += 1;
        Ok(deposit_count)
    }

    async fn unmark_note_processed(&self, note_id: &str) -> anyhow::Result<()> {
        self.processed_notes.write().remove(note_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_synthesis::{CLAIM_EVENT_TOPIC, TopicFilter};

    #[tokio::test]
    async fn test_block_number() {
        let store = InMemoryStore::new();
        assert_eq!(store.get_latest_block_number().await.unwrap(), 0);
        store.set_latest_block_number(42).await.unwrap();
        assert_eq!(store.get_latest_block_number().await.unwrap(), 42);
        assert_eq!(store.advance_block_number().await.unwrap(), 43);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 43);
    }

    #[tokio::test]
    async fn test_nonce() {
        let store = InMemoryStore::new();
        assert_eq!(store.nonce_get("0xABC").await.unwrap(), 0);
        assert_eq!(store.nonce_increment("0xABC").await.unwrap(), 0);
        assert_eq!(store.nonce_increment("0xABC").await.unwrap(), 1);
        assert_eq!(store.nonce_get("0xabc").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_claims() {
        let store = InMemoryStore::new();
        let idx = U256::from(42u64);
        assert!(!store.is_claimed(&idx).await.unwrap());
        store.try_claim(idx).await.unwrap();
        assert!(store.is_claimed(&idx).await.unwrap());
        assert!(store.try_claim(idx).await.is_err());
        store.unclaim(&idx).await.unwrap();
        assert!(!store.is_claimed(&idx).await.unwrap());
        store.try_claim(idx).await.unwrap();
    }

    #[tokio::test]
    async fn test_bridge_out_tracker() {
        let store = InMemoryStore::new();
        assert!(!store.is_note_processed("note1").await.unwrap());
        let c = store
            .mark_note_processed("note1".to_string())
            .await
            .unwrap();
        assert_eq!(c, 0);
        assert!(store.is_note_processed("note1").await.unwrap());
        let c2 = store
            .mark_note_processed("note2".to_string())
            .await
            .unwrap();
        assert_eq!(c2, 1);
    }

    #[tokio::test]
    async fn test_ger_dedup() {
        let store = InMemoryStore::new();
        let ger = [0x11; 32];
        assert!(!store.has_seen_ger(&ger).await.unwrap());
        store
            .add_ger_update_event(0, [0u8; 32], "0xTx1", &ger, None, None, 0)
            .await
            .unwrap();
        assert!(store.has_seen_ger(&ger).await.unwrap());

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x100".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 100).await.unwrap();
        assert_eq!(logs.len(), 1);
    }

    #[tokio::test]
    async fn test_hash_chain_incremental() {
        let store = InMemoryStore::new();
        let ger1 = [0x11; 32];
        let ger2 = [0x22; 32];

        store
            .add_ger_update_event(0, [0u8; 32], "0xTx1", &ger1, None, None, 0)
            .await
            .unwrap();
        let hash1 = *store.hash_chain_value.read();

        store
            .add_ger_update_event(1, [1u8; 32], "0xTx2", &ger2, None, None, 0)
            .await
            .unwrap();
        let hash2 = *store.hash_chain_value.read();

        let mut hasher = Keccak256::new();
        hasher.update([0u8; 32]);
        hasher.update(ger1);
        let expected1: [u8; 32] = hasher.finalize().into();
        assert_eq!(hash1, expected1);

        let mut hasher = Keccak256::new();
        hasher.update(expected1);
        hasher.update(ger2);
        let expected2: [u8; 32] = hasher.finalize().into();
        assert_eq!(hash2, expected2);
        assert_ne!(hash1, hash2);
    }

    #[tokio::test]
    async fn test_log_add_and_query() {
        let store = InMemoryStore::new();
        store
            .add_claim_event(
                "0xBridge",
                100,
                [0xAA; 32],
                "0xTxHash",
                &[0x11; 32],
                1,
                &[0x22; 20],
                &[0x33; 20],
                1000,
            )
            .await
            .unwrap();

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x200".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 500).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].block_number, 100);
    }

    #[tokio::test]
    async fn test_log_filter_topic_match() {
        let store = InMemoryStore::new();
        store
            .add_log(SyntheticLog {
                address: "0x1234".to_string(),
                topics: vec![CLAIM_EVENT_TOPIC.to_string()],
                data: "0x".to_string(),
                block_number: 100,
                block_hash: [0u8; 32],
                transaction_hash: "0xabc".to_string(),
                transaction_index: 0,
                log_index: 0,
                removed: false,
            })
            .await
            .unwrap();

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x200".to_string()),
            topics: Some(vec![Some(TopicFilter::Single(
                CLAIM_EVENT_TOPIC.to_string(),
            ))]),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 500).await.unwrap();
        assert_eq!(logs.len(), 1);
    }

    #[tokio::test]
    async fn test_txn_lifecycle() {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;

        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([1u8; 32]);
        let envelope = alloy::consensus::TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            Signature::test_signature(),
            tx_hash,
        ));

        // Not found
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());

        // Begin
        store
            .txn_begin(
                tx_hash,
                TxnEntry {
                    id: None,
                    envelope,
                    signer: Address::ZERO,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());

        // Commit
        store
            .txn_commit(tx_hash, Ok(()), 42, [0u8; 32])
            .await
            .unwrap();
        let (res, block_num) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(res.is_ok());
        assert_eq!(block_num, 42);
    }

    #[tokio::test]
    async fn test_address_mappings() {
        let store = InMemoryStore::new();
        let addr = Address::from([42u8; 20]);
        assert!(store.get_address_mapping(&addr).await.unwrap().is_none());

        let miden_id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        store.set_address_mapping(addr, miden_id).await.unwrap();
        assert_eq!(
            store.get_address_mapping(&addr).await.unwrap(),
            Some(miden_id)
        );
    }

    #[tokio::test]
    async fn test_ger_injected() {
        let store = InMemoryStore::new();
        let ger = [0xAA; 32];
        assert!(!store.is_ger_injected(&ger).await.unwrap());
        store.mark_ger_injected(ger).await.unwrap();
        assert!(store.is_ger_injected(&ger).await.unwrap());
    }
}
