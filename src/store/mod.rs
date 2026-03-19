//! Store — Unified data persistence layer.
//!
//! The `Store` trait abstracts all persistent and ephemeral state. Two
//! implementations:
//! - `InMemoryStore` — HashMap/RwLock, used as default and in tests
//! - `PgStore` — PostgreSQL-backed, selected via `--database-url`

pub mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(all(test, feature = "postgres"))]
mod postgres_tests;

use crate::block_state::BlockState;
use crate::log_synthesis::{GerEntry, LogFilter, SyntheticLog};
use crate::miden_client::{MidenClientLib, SyncListener};
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, LogData, TxHash, U256};
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;
use std::sync::Arc;

// ── Types ────────────────────────────────────────────────────────────

/// Data for registering a new transaction.
pub struct TxnEntry {
    pub id: Option<TransactionId>,
    pub envelope: TxEnvelope,
    pub signer: Address,
    pub expires_at: Option<u64>,
    pub logs: Vec<LogData>,
}

/// Full transaction data returned from the store.
#[derive(Debug, Clone)]
pub struct TxnData {
    pub id: Option<TransactionId>,
    pub envelope: TxEnvelope,
    pub signer: Address,
    pub expires_at: Option<u64>,
    pub result: Option<Result<(), String>>,
    pub block_num: u64,
    pub logs: Vec<LogData>,
}

impl TxnData {
    /// Build an `alloy::rpc::types::Transaction` for JSON-RPC responses.
    pub fn to_rpc_transaction(
        &self,
        _tx_hash: TxHash,
        block_state: &BlockState,
    ) -> alloy::rpc::types::Transaction {
        use alloy::consensus::transaction::Recovered;
        use alloy::primitives::B256;

        let is_confirmed = self.result.is_some();
        alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(self.envelope.clone(), self.signer),
            block_hash: if is_confirmed {
                Some(B256::from(block_state.get_block_hash(self.block_num)))
            } else {
                None
            },
            block_number: if is_confirmed {
                Some(self.block_num)
            } else {
                None
            },
            transaction_index: if is_confirmed { Some(0) } else { None },
            effective_gas_price: Some(0),
        }
    }
}

// ── Store Trait ───────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait Store: Send + Sync + 'static {
    // === Block number ===
    async fn get_latest_block_number(&self) -> anyhow::Result<u64>;
    async fn set_latest_block_number(&self, n: u64) -> anyhow::Result<()>;
    /// Increment block number by 1 and return the new value.
    async fn advance_block_number(&self) -> anyhow::Result<u64>;

    // === Synthetic logs ===
    async fn add_log(&self, log: SyntheticLog) -> anyhow::Result<()>;
    async fn get_logs(
        &self,
        filter: &LogFilter,
        current_block: u64,
    ) -> anyhow::Result<Vec<SyntheticLog>>;
    async fn get_logs_for_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<SyntheticLog>>;

    // === GER state ===
    async fn has_seen_ger(&self, ger: &[u8; 32]) -> anyhow::Result<bool>;
    /// Mark GER as seen. Returns true if newly inserted.
    async fn mark_ger_seen(&self, ger: &[u8; 32], entry: GerEntry) -> anyhow::Result<bool>;
    async fn get_latest_ger(&self) -> anyhow::Result<Option<[u8; 32]>>;
    async fn get_ger_entry(&self, ger: &[u8; 32]) -> anyhow::Result<Option<GerEntry>>;
    async fn set_ger_exit_roots(
        &self,
        ger: &[u8; 32],
        mainnet_exit_root: [u8; 32],
        rollup_exit_root: [u8; 32],
    ) -> anyhow::Result<()>;
    async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool>;
    async fn mark_ger_injected(&self, ger: [u8; 32]) -> anyhow::Result<()>;
    /// Atomically: mark GER seen, update hash chain, emit UpdateHashChainValue log.
    #[allow(clippy::too_many_arguments)]
    async fn add_ger_update_event(
        &self,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_exit_root: &[u8; 32],
        mainnet_exit_root: Option<[u8; 32]>,
        rollup_exit_root: Option<[u8; 32]>,
        timestamp: u64,
    ) -> anyhow::Result<()>;

    // === Transactions ===
    async fn txn_begin(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<()>;
    async fn txn_commit(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()>;
    async fn txn_receipt(
        &self,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<(Result<(), String>, u64)>>;
    async fn txn_get(&self, tx_hash: TxHash) -> anyhow::Result<Option<TxnData>>;
    async fn txn_pending_by_miden_id(&self, id: TransactionId) -> anyhow::Result<Option<TxHash>>;
    async fn txn_commit_pending(
        &self,
        ids: &[TransactionId],
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()>;
    async fn txn_expire_pending(&self, block_num: u64, block_hash: [u8; 32]) -> anyhow::Result<()>;

    // === Nonces ===
    async fn nonce_get(&self, addr: &str) -> anyhow::Result<u64>;
    /// Increment nonce, returning the value **before** increment.
    async fn nonce_increment(&self, addr: &str) -> anyhow::Result<u64>;

    // === Claims ===
    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()>;
    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()>;
    async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool>;

    // === Address mappings ===
    async fn get_address_mapping(&self, eth: &Address) -> anyhow::Result<Option<AccountId>>;
    async fn set_address_mapping(&self, eth: Address, miden: AccountId) -> anyhow::Result<()>;

    // === Bridge-out ===
    async fn is_note_processed(&self, note_id: &str) -> anyhow::Result<bool>;
    /// Mark note as processed, return the deposit count assigned to it.
    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32>;
    /// Roll back a processed-note marker when later persistence fails.
    async fn unmark_note_processed(&self, note_id: &str) -> anyhow::Result<()>;

    // === Convenience: claim event log ===
    #[allow(clippy::too_many_arguments)]
    async fn add_claim_event(
        &self,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_index: &[u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) -> anyhow::Result<()> {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()],
            data: crate::log_synthesis::encode_claim_event_data(
                global_index,
                origin_network,
                origin_address,
                destination_address,
                amount,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log).await
    }

    // === Convenience: bridge event log ===
    #[allow(clippy::too_many_arguments)]
    async fn add_bridge_event(
        &self,
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
        deposit_count: u32,
    ) -> anyhow::Result<()> {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![crate::log_synthesis::BRIDGE_EVENT_TOPIC.to_string()],
            data: crate::bridge_out::encode_bridge_event_data(
                leaf_type,
                origin_network,
                origin_address,
                destination_network,
                destination_address,
                amount,
                deposit_count,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log).await
    }
}

// ── StoreSyncListener ────────────────────────────────────────────────

/// Adapts the Store to the MidenClient sync loop.
///
/// Buffers sync data in `on_sync` (sync), processes in `on_post_sync` (async).
/// Replaces the old TxnManager + BlockNumTracker sync listeners.
pub struct StoreSyncListener {
    pub store: Arc<dyn Store>,
    pub block_state: Arc<BlockState>,
    pending: std::sync::Mutex<Option<SyncData>>,
}

struct SyncData {
    block_num: u64,
    committed_ids: Vec<TransactionId>,
}

impl StoreSyncListener {
    pub fn new(store: Arc<dyn Store>, block_state: Arc<BlockState>) -> Self {
        Self {
            store,
            block_state,
            pending: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl SyncListener for StoreSyncListener {
    fn on_sync(&self, summary: &SyncSummary) {
        let data = SyncData {
            block_num: summary.block_num.as_u64(),
            committed_ids: summary.committed_transactions.clone(),
        };
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(data);
    }

    async fn on_post_sync(&self, _client: &mut MidenClientLib) -> anyhow::Result<()> {
        let data = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(data) = data {
            let block_hash = self.block_state.get_block_hash(data.block_num);
            self.store.set_latest_block_number(data.block_num).await?;
            self.store
                .txn_commit_pending(&data.committed_ids, data.block_num, block_hash)
                .await?;
            self.store
                .txn_expire_pending(data.block_num, block_hash)
                .await?;
        }
        Ok(())
    }
}
