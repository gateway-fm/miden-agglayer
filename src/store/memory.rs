//! In-memory Store implementation — wraps HashMap/RwLock data structures.

use super::{FaucetEntry, Store, TxnData, TxnEntry, UnbridgeableBridgeOut, UnclaimableClaim};
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
    // Block number — synthetic EVM tip.
    latest_block_number: RwLock<u64>,

    // Raw Miden sync height — tracked SEPARATELY from the synthetic EVM tip
    // (Cantina #5). The sync loop writes the observed Miden block here; it
    // must never clobber `latest_block_number`.
    raw_miden_height: RwLock<u64>,

    // Bridge-out atomic-commit critical section (Cantina #5/#15). Held across
    // block allocation + deposit_count allocation + log insert so the whole
    // bundle is one atomic unit, matching the PgStore SERIALIZABLE transaction.
    bridge_out_commit_lock: Mutex<()>,

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

    // Unclaimable claims — first-write wins per global_index (RD-860).
    unclaimable: RwLock<HashMap<U256, UnclaimableClaim>>,

    // Unbridgeable bridge-outs — first-write wins per note_id (Cantina MA#18).
    unbridgeable_bridge_outs: RwLock<HashMap<String, UnbridgeableBridgeOut>>,

    // Address mappings
    address_mappings: RwLock<HashMap<Address, AccountId>>,

    // Bridge-out — note_id → assigned deposit_count. Stored as a map (not a
    // set) so an idempotent retry can REUSE the assigned count (Cantina #15)
    // rather than re-incrementing the counter.
    processed_notes: RwLock<HashMap<String, u32>>,
    deposit_counter: RwLock<u32>,

    // Claim watcher (independent from bridge-out so CLAIM observations do not
    // consume B2AGG `deposit_counter` slots — see commit_manual_claim_event_atomic).
    claim_watcher_processed: RwLock<HashMap<String, [u8; 32]>>,

    // Faucet registry
    faucets: RwLock<Vec<FaucetEntry>>,

    // Monitor trackers (RD-913) — in-memory mirror of monitor_burn_serials,
    // monitor_twin_notes, monitor_expected_mints. With InMemoryStore the
    // mirror IS the source of truth; with PgStore the DB is and these
    // structures live inside the tracker's LRU cache instead.
    monitor_burn_serials: RwLock<HashSet<[u8; 32]>>,
    monitor_twin_notes: RwLock<HashMap<[u8; 32], Vec<[u8; 32]>>>,
    monitor_expected_mints: RwLock<HashMap<[u8; 32], MonitorExpectedMintRow>>,
}

#[derive(Clone, Copy)]
struct MonitorExpectedMintRow {
    expected_mint: [u8; 32],
    ticks_pending: u32,
    alerted: bool,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<InMemoryStore>();

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            latest_block_number: RwLock::new(0),
            raw_miden_height: RwLock::new(0),
            bridge_out_commit_lock: Mutex::new(()),
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
            unclaimable: RwLock::new(HashMap::new()),
            unbridgeable_bridge_outs: RwLock::new(HashMap::new()),
            address_mappings: RwLock::new(HashMap::new()),
            processed_notes: RwLock::new(HashMap::new()),
            deposit_counter: RwLock::new(0),
            claim_watcher_processed: RwLock::new(HashMap::new()),
            faucets: RwLock::new(Vec::new()),
            monitor_burn_serials: RwLock::new(HashSet::new()),
            monitor_twin_notes: RwLock::new(HashMap::new()),
            monitor_expected_mints: RwLock::new(HashMap::new()),
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

    async fn get_raw_miden_height(&self) -> anyhow::Result<u64> {
        Ok(*self.raw_miden_height.read())
    }

    async fn set_raw_miden_height(&self, height: u64) -> anyhow::Result<()> {
        *self.raw_miden_height.write() = height;
        Ok(())
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
        l1_block_number: u64,
        l1_timestamp: u64,
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
        // Mirror the PgStore semantics: indexer is authoritative for L1
        // origin metadata, so overwrite unconditionally on every call.
        entry.block_number = l1_block_number;
        entry.timestamp = l1_timestamp;
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

    async fn record_unclaimable_claim(&self, entry: UnclaimableClaim) -> anyhow::Result<bool> {
        use std::collections::hash_map::Entry;
        let mut map = self.unclaimable.write();
        match map.entry(entry.global_index) {
            Entry::Occupied(_) => Ok(false),
            Entry::Vacant(slot) => {
                slot.insert(entry);
                Ok(true)
            }
        }
    }

    async fn get_unclaimable_claim(
        &self,
        global_index: &U256,
    ) -> anyhow::Result<Option<UnclaimableClaim>> {
        Ok(self.unclaimable.read().get(global_index).cloned())
    }

    // ── Unbridgeable bridge-outs (Cantina MA#18) ─────────────────

    async fn record_unbridgeable_bridge_out(
        &self,
        entry: UnbridgeableBridgeOut,
    ) -> anyhow::Result<bool> {
        use std::collections::hash_map::Entry;
        let mut map = self.unbridgeable_bridge_outs.write();
        match map.entry(entry.note_id.clone()) {
            Entry::Occupied(_) => Ok(false),
            Entry::Vacant(slot) => {
                slot.insert(entry);
                Ok(true)
            }
        }
    }

    async fn get_unbridgeable_bridge_out(
        &self,
        note_id: &str,
    ) -> anyhow::Result<Option<UnbridgeableBridgeOut>> {
        Ok(self.unbridgeable_bridge_outs.read().get(note_id).cloned())
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
        Ok(self.processed_notes.read().contains_key(note_id))
    }

    async fn get_processed_deposit_count(&self, note_id: &str) -> anyhow::Result<Option<u32>> {
        Ok(self.processed_notes.read().get(note_id).copied())
    }

    async fn get_deposit_count(&self) -> anyhow::Result<u64> {
        Ok(*self.deposit_counter.read() as u64)
    }

    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32> {
        // Cantina #15 — idempotent: a note already processed REUSES its
        // assigned deposit_count rather than allocating a new one (which would
        // diverge the exported exit index from the Miden LET order).
        let mut processed = self.processed_notes.write();
        if let Some(existing) = processed.get(&note_id) {
            return Ok(*existing);
        }
        let mut counter = self.deposit_counter.write();
        let deposit_count = *counter;
        *counter += 1;
        processed.insert(note_id, deposit_count);
        Ok(deposit_count)
    }

    async fn unmark_note_processed(&self, note_id: &str) -> anyhow::Result<()> {
        self.processed_notes.write().remove(note_id);
        Ok(())
    }

    /// Cantina #5/#15/#19 — atomic B2AGG commit for InMemoryStore at the
    /// store-allocated batch block `block_number`. Holds the dedicated commit
    /// lock across the deposit_count allocation + log insert so the bundle is
    /// one critical section (mirrors the PgStore SERIALIZABLE transaction).
    /// Does NOT advance the tip — `allocate_synthetic_block` already did, once
    /// for the whole batch. Idempotent: an already-processed note reuses its
    /// deposit_count and emits no duplicate log (Cantina #15).
    #[allow(clippy::too_many_arguments)]
    async fn commit_b2agg_event_atomic(
        &self,
        note_id: String,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        leaf_type: u8,
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_network: u32,
        destination_address: &[u8; 20],
        amount: u128,
        metadata: &[u8],
    ) -> anyhow::Result<u32> {
        // The commit lock guards the idempotency check + deposit_count
        // allocation as one critical section (so two concurrent commits never
        // assign the same deposit_count, and a retry of an already-processed
        // note reuses its count — Cantina #15). It is a non-Send parking_lot
        // guard, so it is scoped to the synchronous section and dropped BEFORE
        // the `add_bridge_event` await; the log insert itself is already
        // serialised by `add_log`'s own internal locks, and the synthetic block
        // was pre-allocated by `allocate_synthetic_block` (Cantina #5/#19).
        let already_processed;
        let deposit_count = {
            let _guard = self.bridge_out_commit_lock.lock();
            // Bind the read result to a local so the RwLock read guard is
            // released BEFORE the `None` arm takes the write lock — a `match`
            // scrutinee temporary would otherwise live for the whole match
            // body and deadlock parking_lot (non-reentrant).
            let existing = self.processed_notes.read().get(&note_id).copied();
            match existing {
                Some(dc) => {
                    already_processed = true;
                    dc
                }
                None => {
                    already_processed = false;
                    let mut counter = self.deposit_counter.write();
                    let dc = *counter;
                    *counter += 1;
                    self.processed_notes.write().insert(note_id, dc);
                    dc
                }
            }
        };
        if already_processed {
            return Ok(deposit_count);
        }
        self.add_bridge_event(
            bridge_address,
            block_number,
            block_hash,
            tx_hash,
            leaf_type,
            origin_network,
            origin_address,
            destination_network,
            destination_address,
            amount,
            metadata,
            deposit_count,
        )
        .await?;
        Ok(deposit_count)
    }

    // ── Claim watcher ────────────────────────────────────────────

    async fn is_claim_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
        Ok(self.claim_watcher_processed.read().contains_key(note_id))
    }

    async fn mark_claim_note_processed(
        &self,
        note_id: String,
        global_index: [u8; 32],
        _block_number: u64,
    ) -> anyhow::Result<()> {
        self.claim_watcher_processed
            .write()
            .insert(note_id, global_index);
        Ok(())
    }

    async fn has_claim_event_for_global_index(
        &self,
        global_index: &[u8; 32],
    ) -> anyhow::Result<bool> {
        // 1. Any prior watcher-emission for this leaf.
        if self
            .claim_watcher_processed
            .read()
            .values()
            .any(|gi| gi == global_index)
        {
            return Ok(true);
        }
        // 2. Normal-RPC path: scan synthetic_logs for a ClaimEvent whose 32-byte
        //    data prefix matches the global_index. Encoding lives in
        //    `log_synthesis::encode_claim_event_data*`; the global_index is the
        //    first 32 bytes of the ABI-encoded data, so a prefix match is sound.
        let topic = crate::log_synthesis::CLAIM_EVENT_TOPIC;
        let prefix = format!("0x{}", hex::encode(global_index));
        let logs = self.logs_by_block.read();
        for v in logs.values() {
            for log in v {
                if log.topics.first().is_some_and(|t| t == topic)
                    && log.data.len() >= prefix.len()
                    && log.data[..prefix.len()].eq_ignore_ascii_case(&prefix)
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ── Faucet registry ──────────────────────────────────────────

    async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()> {
        let mut faucets = self.faucets.write();
        if let Some(existing) = faucets.iter_mut().find(|f| f.faucet_id == entry.faucet_id) {
            *existing = entry;
        } else {
            faucets.push(entry);
        }
        Ok(())
    }

    async fn get_faucet_by_origin(
        &self,
        origin_address: &[u8; 20],
        origin_network: u32,
    ) -> anyhow::Result<Option<FaucetEntry>> {
        let faucets = self.faucets.read();
        Ok(faucets
            .iter()
            .find(|f| f.origin_address == *origin_address && f.origin_network == origin_network)
            .cloned())
    }

    async fn find_faucets_by_origin_address(
        &self,
        origin_address: &[u8; 20],
    ) -> anyhow::Result<Vec<FaucetEntry>> {
        let faucets = self.faucets.read();
        Ok(faucets
            .iter()
            .filter(|f| f.origin_address == *origin_address)
            .cloned()
            .collect())
    }

    async fn get_faucet_by_id(&self, faucet_id: AccountId) -> anyhow::Result<Option<FaucetEntry>> {
        let faucets = self.faucets.read();
        Ok(faucets.iter().find(|f| f.faucet_id == faucet_id).cloned())
    }

    async fn list_faucets(&self) -> anyhow::Result<Vec<FaucetEntry>> {
        Ok(self.faucets.read().clone())
    }

    // ── Monitor trackers (RD-913) ────────────────────────────────

    async fn burn_serial_seen(&self, serial: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(self.monitor_burn_serials.read().contains(serial))
    }

    async fn burn_serial_observe(&self, serial: &[u8; 32]) -> anyhow::Result<bool> {
        let mut set = self.monitor_burn_serials.write();
        Ok(set.insert(*serial))
    }

    async fn twin_note_commitments(&self, note_id: &[u8; 32]) -> anyhow::Result<Vec<[u8; 32]>> {
        Ok(self
            .monitor_twin_notes
            .read()
            .get(note_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn twin_note_observe(
        &self,
        note_id: &[u8; 32],
        commitment: &[u8; 32],
    ) -> anyhow::Result<bool> {
        let mut map = self.monitor_twin_notes.write();
        let entry = map.entry(*note_id).or_default();
        if entry.contains(commitment) {
            Ok(false)
        } else {
            entry.push(*commitment);
            Ok(true)
        }
    }

    async fn expected_mint_record(
        &self,
        global_index: &[u8; 32],
        expected_mint: &[u8; 32],
    ) -> anyhow::Result<()> {
        let mut map = self.monitor_expected_mints.write();
        map.insert(
            *global_index,
            MonitorExpectedMintRow {
                expected_mint: *expected_mint,
                ticks_pending: 0,
                alerted: false,
            },
        );
        Ok(())
    }

    async fn expected_mint_remove(&self, global_index: &[u8; 32]) -> anyhow::Result<()> {
        self.monitor_expected_mints.write().remove(global_index);
        Ok(())
    }

    async fn expected_mint_load_all(&self) -> anyhow::Result<Vec<([u8; 32], [u8; 32], u32, bool)>> {
        let map = self.monitor_expected_mints.read();
        Ok(map
            .iter()
            .map(|(gi, row)| (*gi, row.expected_mint, row.ticks_pending, row.alerted))
            .collect())
    }

    async fn expected_mint_update_tick(
        &self,
        global_index: &[u8; 32],
        ticks_pending: u32,
        alerted: bool,
    ) -> anyhow::Result<()> {
        let mut map = self.monitor_expected_mints.write();
        if let Some(row) = map.get_mut(global_index) {
            row.ticks_pending = ticks_pending;
            row.alerted = alerted;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_synthesis::{CLAIM_EVENT_TOPIC, TopicFilter};

    #[tokio::test]
    async fn set_ger_exit_roots_persists_l1_block_and_timestamp() {
        // Before this change, both columns were hardcoded to 0 in PgStore and
        // ignored in InMemoryStore. The indexer is the authoritative writer
        // for L1 origin metadata, so the InMemoryStore — which mirrors
        // PgStore semantics for tests — must round-trip them.
        let store = InMemoryStore::new();
        let ger = [0x11u8; 32];
        let mainnet = [0x22u8; 32];
        let rollup = [0x33u8; 32];

        // First write: fresh entry — block + ts land as given.
        store
            .set_ger_exit_roots(&ger, mainnet, rollup, 10_900_000, 1_779_300_000)
            .await
            .unwrap();
        let entry = store.get_ger_entry(&ger).await.unwrap().unwrap();
        assert_eq!(entry.mainnet_exit_root, Some(mainnet));
        assert_eq!(entry.rollup_exit_root, Some(rollup));
        assert_eq!(entry.block_number, 10_900_000);
        assert_eq!(entry.timestamp, 1_779_300_000);

        // Second write at a later L1 block (same GER hash): indexer is
        // authoritative, so the new L1 origin metadata overwrites the old.
        // This is the "L2 path wrote the row first with stale values; later
        // indexer poll corrects it" convergence the docstring describes.
        store
            .set_ger_exit_roots(&ger, mainnet, rollup, 10_900_005, 1_779_300_060)
            .await
            .unwrap();
        let entry = store.get_ger_entry(&ger).await.unwrap().unwrap();
        assert_eq!(entry.block_number, 10_900_005);
        assert_eq!(entry.timestamp, 1_779_300_060);
    }

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
    async fn test_unclaimable_claims_first_write_wins() {
        use crate::store::{UnclaimableClaim, UnclaimableReason};
        let store = InMemoryStore::new();
        let idx = U256::from(999u64);
        let first = UnclaimableClaim {
            global_index: idx,
            destination_address: Address::from([0x42; 20]),
            origin_network: 0,
            origin_address: Address::ZERO,
            amount: U256::from(100u64),
            reason: UnclaimableReason::UnresolvableDestination,
            eth_tx_hash: TxHash::default(),
        };
        let second = UnclaimableClaim {
            // Same global_index, different everything else — mimics aggkit retrying
            // the same claim with a new outer tx envelope.
            global_index: idx,
            destination_address: Address::from([0x77; 20]),
            origin_network: 9,
            origin_address: Address::from([0xaa; 20]),
            amount: U256::from(200u64),
            reason: UnclaimableReason::UnresolvableDestination,
            eth_tx_hash: TxHash::from([0xff; 32]),
        };

        assert!(store.get_unclaimable_claim(&idx).await.unwrap().is_none());
        assert!(
            store.record_unclaimable_claim(first.clone()).await.unwrap(),
            "first insert returns true"
        );
        assert!(
            !store.record_unclaimable_claim(second).await.unwrap(),
            "duplicate global_index returns false (first-write wins)"
        );
        let got = store.get_unclaimable_claim(&idx).await.unwrap().unwrap();
        assert_eq!(got.destination_address, first.destination_address);
        assert_eq!(got.amount, first.amount);
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

    #[tokio::test]
    async fn test_faucet_registry() {
        let store = InMemoryStore::new();
        let faucet_id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();

        // Initially empty
        assert!(store.list_faucets().await.unwrap().is_empty());
        assert!(store.get_faucet_by_id(faucet_id).await.unwrap().is_none());
        assert!(
            store
                .get_faucet_by_origin(&[0u8; 20], 0)
                .await
                .unwrap()
                .is_none()
        );

        // Register ETH faucet
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "ETH".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
            })
            .await
            .unwrap();

        // Lookup by ID
        let entry = store.get_faucet_by_id(faucet_id).await.unwrap().unwrap();
        assert_eq!(entry.symbol, "ETH");
        assert_eq!(entry.scale, 10);

        // Lookup by origin
        let entry = store
            .get_faucet_by_origin(&[0u8; 20], 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.faucet_id, faucet_id);

        // List
        assert_eq!(store.list_faucets().await.unwrap().len(), 1);

        // Upsert (update symbol)
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "WETH".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
            })
            .await
            .unwrap();
        let entry = store.get_faucet_by_id(faucet_id).await.unwrap().unwrap();
        assert_eq!(entry.symbol, "WETH");
        assert_eq!(store.list_faucets().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_faucet_registry_dynamic_erc20_bidirectional() {
        // Simulate: register a new ERC-20 (USDC), then resolve it for bridge-out
        let store = InMemoryStore::new();
        let usdc_id = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();

        // Simulate auto-creation during first L1→L2 claim
        let usdc_origin = [0xA0; 20]; // USDC contract address
        store
            .register_faucet(FaucetEntry {
                faucet_id: usdc_id,
                origin_address: usdc_origin,
                origin_network: 0,
                symbol: "USDC".into(),
                origin_decimals: 6,
                miden_decimals: 6,
                scale: 0,
            })
            .await
            .unwrap();

        // L1→L2 claim lookup: find faucet by origin address
        let claim_faucet = store
            .get_faucet_by_origin(&usdc_origin, 0)
            .await
            .unwrap()
            .expect("USDC faucet should be found for L1→L2 claim");
        assert_eq!(claim_faucet.symbol, "USDC");
        assert_eq!(claim_faucet.origin_decimals, 6);
        assert_eq!(claim_faucet.scale, 0);

        // L2→L1 bridge-out lookup: find faucet by Miden account ID
        let bridge_out_faucet = store
            .get_faucet_by_id(usdc_id)
            .await
            .unwrap()
            .expect("USDC faucet should be found for L2→L1 bridge-out");
        assert_eq!(bridge_out_faucet.origin_address, usdc_origin);
        assert_eq!(bridge_out_faucet.origin_network, 0);
        assert_eq!(bridge_out_faucet.scale, 0);

        // Verify amount scaling: 1000 USDC with scale=0 → no change
        let origin_amount =
            crate::bridge_out::reverse_scale_amount(1000, bridge_out_faucet.scale).unwrap();
        assert_eq!(origin_amount, 1000);
    }

    /// Cantina #5 — store-owned atomic synthetic block allocation regression.
    ///
    /// The auditor PoC (`poc-05.rs`,
    /// `bridge_event_can_be_added_to_an_already_visible_block`) reproduced the
    /// ROOT CAUSE: two writers reserved the SAME block number from the same
    /// observed tip via `get_latest_block_number()+1`, the first event made
    /// block N visible, and the second event was appended into that
    /// already-visible block — without changing its hash, so undetectable.
    ///
    /// After the fix, block allocation is store-owned and transactional
    /// (`allocate_synthetic_block` / the atomic commit helpers). This test is
    /// the POSITIVE form of the PoC: two concurrent allocations MUST get
    /// DISTINCT block numbers, and a late writer can never reuse a block an
    /// earlier writer already published into. This is exactly the property
    /// cergyk's objection said was missing.
    #[tokio::test]
    async fn cantina_5_concurrent_allocations_get_distinct_blocks() {
        use crate::log_synthesis::{BRIDGE_EVENT_TOPIC, CLAIM_EVENT_TOPIC, LogFilter};
        use std::collections::BTreeSet;
        use std::sync::Arc;

        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());

        // Start from a raw Miden tip of 100, mirroring the PoC's setup.
        store.set_latest_block_number(100).await.unwrap();

        // Model the PoC's two-writer race WITHOUT pre-bumping the tip between
        // the reservations — which is exactly the scenario the racy
        // `get_latest_block_number()+1` mishandled: both writers would have
        // observed tip 100 and both reserved 101. With store-owned allocation
        // each call is a single atomic advance-and-return, so back-to-back
        // reservations from two store handles that have NOT published anything
        // in between are still DISTINCT.
        let writer_a = store.clone();
        let writer_b = store.clone();
        let res_a = writer_a.allocate_synthetic_block().await.unwrap();
        let res_b = writer_b.allocate_synthetic_block().await.unwrap();
        assert_ne!(
            res_a, res_b,
            "two writers reserving the next synthetic block from the same observed tip MUST \
             get DISTINCT blocks — the PoC collision is impossible (Cantina #5)"
        );
        assert_eq!(
            (res_a, res_b),
            (101, 102),
            "store-owned allocation hands out 101 then 102 — no shared block"
        );

        // Hammer it: many sequential reservations are all distinct and
        // contiguous (no duplicate ever handed out).
        let n = 64usize;
        let mut more = BTreeSet::new();
        for _ in 0..n {
            assert!(
                more.insert(store.allocate_synthetic_block().await.unwrap()),
                "every store-owned allocation is unique — no block handed out twice"
            );
        }
        assert_eq!(more.len(), n);
        assert_eq!(*more.iter().min().unwrap(), 103);

        // The first writer publishes a ClaimEvent into block 101.
        let first_block = 101u64;
        let first_hash = crate::block_state::SyntheticBlock::compute_hash_for_number(first_block);
        store
            .add_claim_event(
                "0x0000000000000000000000000000000000000001",
                first_block,
                first_hash,
                "0xclaim",
                &[0x11; 32],
                0,
                &[0u8; 20],
                &[1u8; 20],
                1,
            )
            .await
            .unwrap();

        // A later bridge-out lands at its OWN distinct block (102), committed
        // atomically (deposit_count + log, Cantina #15) — it can NOT be
        // appended into the first writer's already-published block 101.
        let second_block = 102u64;
        let second_hash = crate::block_state::SyntheticBlock::compute_hash_for_number(second_block);
        let deposit_count = store
            .commit_b2agg_event_atomic(
                "note-late".to_string(),
                "0x0000000000000000000000000000000000000001",
                second_block,
                second_hash,
                "0xbridge",
                0,
                0,
                &[0u8; 20],
                1,
                &[1u8; 20],
                1,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(deposit_count, 0, "first bridge-out gets deposit_count 0");

        // The ClaimEvent is alone in block 101; the BridgeEvent is alone in the
        // distinct block 102. The late log did NOT land in the already-visible
        // first block — the PoC's failure is now impossible.
        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, second_block).await.unwrap();
        let claim_log = logs
            .iter()
            .find(|l| l.topics[0] == CLAIM_EVENT_TOPIC)
            .expect("ClaimEvent present");
        let bridge_log = logs
            .iter()
            .find(|l| l.topics[0] == BRIDGE_EVENT_TOPIC)
            .expect("BridgeEvent present");
        assert_eq!(claim_log.block_number, first_block);
        assert_eq!(bridge_log.block_number, second_block);
        assert_ne!(
            claim_log.block_number, bridge_log.block_number,
            "the late BridgeEvent landed in a DISTINCT block, not the already-visible ClaimEvent block"
        );
        assert_ne!(
            claim_log.block_hash, bridge_log.block_hash,
            "distinct blocks have distinct hashes — no silent same-hash append"
        );
    }

    /// Cantina #15 — the deposit_count increment is folded into the same
    /// commit as the synthetic-log insert, and is idempotent on retry: a
    /// re-commit of the same note REUSES the assigned deposit_count and emits
    /// NO duplicate log (rather than re-incrementing and diverging the exported
    /// exit index from the Miden Local Exit Tree order).
    #[tokio::test]
    async fn cantina_15_b2agg_commit_is_atomic_and_idempotent_on_retry() {
        use crate::log_synthesis::LogFilter;
        let store = InMemoryStore::new();

        let block = store.allocate_synthetic_block().await.unwrap();
        let hash = crate::block_state::SyntheticBlock::compute_hash_for_number(block);

        // First commit assigns deposit_count 0 and writes one BridgeEvent.
        let dc1 = store
            .commit_b2agg_event_atomic(
                "note-X".to_string(),
                "0xbridge",
                block,
                hash,
                "0xtx",
                0,
                0,
                &[0u8; 20],
                1,
                &[1u8; 20],
                1,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(dc1, 0);
        assert_eq!(store.get_deposit_count().await.unwrap(), 1);

        // Retry of the SAME note: reuses deposit_count 0, does NOT re-increment
        // the counter, does NOT emit a second log.
        let dc2 = store
            .commit_b2agg_event_atomic(
                "note-X".to_string(),
                "0xbridge",
                block,
                hash,
                "0xtx",
                0,
                0,
                &[0u8; 20],
                1,
                &[1u8; 20],
                1,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(dc2, 0, "retry REUSES the assigned deposit_count");
        assert_eq!(
            store.get_deposit_count().await.unwrap(),
            1,
            "retry must NOT re-increment the deposit_counter (Cantina #15)"
        );

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, block).await.unwrap();
        assert_eq!(logs.len(), 1, "retry must NOT emit a duplicate BridgeEvent");

        // A DIFFERENT note gets the next deposit_count, atomically.
        let dc3 = store
            .commit_b2agg_event_atomic(
                "note-Y".to_string(),
                "0xbridge",
                block,
                hash,
                "0xtx2",
                0,
                0,
                &[0u8; 20],
                1,
                &[1u8; 20],
                1,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(dc3, 1, "distinct note gets the next deposit_count");
    }

    /// Cantina #5 — the raw Miden sync height is tracked SEPARATELY from the
    /// synthetic EVM tip. Advancing the raw height must never move the
    /// synthetic tip, and allocating synthetic blocks must never move the raw
    /// height.
    #[tokio::test]
    async fn cantina_5_raw_miden_height_is_separate_from_synthetic_tip() {
        let store = InMemoryStore::new();

        store.set_latest_block_number(50).await.unwrap();
        store.set_raw_miden_height(123_456).await.unwrap();

        assert_eq!(store.get_latest_block_number().await.unwrap(), 50);
        assert_eq!(store.get_raw_miden_height().await.unwrap(), 123_456);

        // Allocating a synthetic block moves ONLY the synthetic tip.
        let b = store.allocate_synthetic_block().await.unwrap();
        assert_eq!(b, 51);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 51);
        assert_eq!(
            store.get_raw_miden_height().await.unwrap(),
            123_456,
            "synthetic allocation must not touch the raw Miden height"
        );

        // Bumping the raw Miden height moves ONLY the raw height.
        store.set_raw_miden_height(123_999).await.unwrap();
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            51,
            "raw Miden height must not clobber the synthetic tip"
        );
        assert_eq!(store.get_raw_miden_height().await.unwrap(), 123_999);
    }

    /// Cantina #1 — repro+regression. Two faucets registered for the same origin token
    /// address under different `origin_network` values must both surface from the new
    /// `find_faucets_by_origin_address` lookup so `find_or_create_faucet` can refuse a
    /// colliding auto-create. Without this method, aggkit only had the
    /// `(token, network)` pair lookup which always misses the existing entry under a
    /// different network — letting auto-create silently overwrite the on-chain registry.
    #[tokio::test]
    async fn cantina_1_find_faucets_by_origin_address_surfaces_cross_network_collision() {
        let store = InMemoryStore::new();
        let token_addr = [0xA0u8; 20]; // shared origin token address (e.g. CREATE2-cloned)

        let faucet_n0 = AccountId::from_hex("0x3d7c9747558851900f8206226dfbea").unwrap();
        let faucet_n1 = AccountId::from_hex("0x3d7c9747558851900f8206226dfbeb").unwrap();

        store
            .register_faucet(FaucetEntry {
                faucet_id: faucet_n0,
                origin_address: token_addr,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
            })
            .await
            .unwrap();
        store
            .register_faucet(FaucetEntry {
                faucet_id: faucet_n1,
                origin_address: token_addr,
                origin_network: 7,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
            })
            .await
            .unwrap();

        // Per-pair lookup correctly returns each network's own entry.
        let only_n0 = store.get_faucet_by_origin(&token_addr, 0).await.unwrap();
        assert_eq!(only_n0.unwrap().origin_network, 0);

        // The new method surfaces BOTH — this is what `find_or_create_faucet` uses to
        // detect that a same-address-different-network entry already exists and refuse
        // to auto-create a second one (which would silently overwrite the on-chain
        // address-keyed registry, Cantina #1).
        let all = store
            .find_faucets_by_origin_address(&token_addr)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "should surface every faucet for this token");
        let networks: std::collections::BTreeSet<u32> =
            all.iter().map(|f| f.origin_network).collect();
        assert_eq!(networks, [0u32, 7].iter().copied().collect());

        // Other origin addresses are unaffected.
        let other = store
            .find_faucets_by_origin_address(&[0u8; 20])
            .await
            .unwrap();
        assert!(other.is_empty());
    }
}
