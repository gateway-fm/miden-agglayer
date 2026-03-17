//! PostgreSQL Store implementation — selected via `--database-url`.
//!
//! Requires the `postgres` feature flag. This is a skeleton implementation
//! that will be fleshed out with real SQL queries.

use super::{Store, TxnData, TxnEntry};
use crate::log_synthesis::{GerEntry, LogFilter, SyntheticLog};
use alloy::primitives::{Address, TxHash, U256};
use deadpool_postgres::{Manager, Pool};
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;

pub struct PgStore {
    _pool: Pool,
}

impl PgStore {
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        let config: tokio_postgres::Config = database_url.parse()?;
        let manager = Manager::new(config, tokio_postgres::NoTls);
        let pool = Pool::builder(manager).max_size(16).build()?;

        // Verify connectivity
        let _client = pool.get().await?;
        tracing::info!("PgStore: connected to PostgreSQL");

        Ok(Self { _pool: pool })
    }
}

/// PgStore is a placeholder — all methods return defaults or no-ops.
/// Real SQL implementation is deferred until Phase 2 integration testing.
#[async_trait::async_trait]
impl Store for PgStore {
    async fn get_latest_block_number(&self) -> u64 { 0 }
    async fn set_latest_block_number(&self, _n: u64) {}
    async fn advance_block_number(&self) -> u64 { 1 }

    async fn add_log(&self, _log: SyntheticLog) {}
    async fn get_logs(&self, _filter: &LogFilter, _current_block: u64) -> Vec<SyntheticLog> {
        Vec::new()
    }
    async fn get_logs_for_tx(&self, _tx_hash: &str) -> Vec<SyntheticLog> {
        Vec::new()
    }

    async fn has_seen_ger(&self, _ger: &[u8; 32]) -> bool { false }
    async fn mark_ger_seen(&self, _ger: &[u8; 32], _entry: GerEntry) -> bool { true }
    async fn get_latest_ger(&self) -> Option<[u8; 32]> { None }
    async fn get_ger_entry(&self, _ger: &[u8; 32]) -> Option<GerEntry> { None }
    async fn is_ger_injected(&self, _ger: &[u8; 32]) -> bool { false }
    async fn mark_ger_injected(&self, _ger: [u8; 32]) {}
    async fn add_ger_update_event(
        &self, _block_number: u64, _block_hash: [u8; 32], _tx_hash: &str,
        _global_exit_root: &[u8; 32], _mainnet_exit_root: Option<[u8; 32]>,
        _rollup_exit_root: Option<[u8; 32]>, _timestamp: u64,
    ) {}

    async fn txn_begin(&self, _tx_hash: TxHash, _entry: TxnEntry) -> anyhow::Result<()> {
        Ok(())
    }
    async fn txn_commit(
        &self, _tx_hash: TxHash, _result: Result<(), String>,
        _block_num: u64, _block_hash: [u8; 32],
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn txn_receipt(&self, _tx_hash: TxHash) -> Option<(Result<(), String>, u64)> { None }
    async fn txn_get(&self, _tx_hash: TxHash) -> Option<TxnData> { None }
    async fn txn_pending_by_miden_id(&self, _id: TransactionId) -> Option<TxHash> { None }
    async fn txn_commit_pending(
        &self, _ids: &[TransactionId], _block_num: u64, _block_hash: [u8; 32],
    ) {}
    async fn txn_expire_pending(&self, _block_num: u64, _block_hash: [u8; 32]) {}

    async fn nonce_get(&self, _addr: &str) -> u64 { 0 }
    async fn nonce_increment(&self, _addr: &str) -> u64 { 0 }

    async fn try_claim(&self, _global_index: U256) -> anyhow::Result<()> { Ok(()) }
    async fn unclaim(&self, _global_index: &U256) {}
    async fn is_claimed(&self, _global_index: &U256) -> bool { false }

    async fn get_address_mapping(&self, _eth: &Address) -> Option<AccountId> { None }
    async fn set_address_mapping(&self, _eth: Address, _miden: AccountId) {}

    async fn is_note_processed(&self, _note_id: &str) -> bool { false }
    async fn mark_note_processed(&self, _note_id: String) -> u32 { 0 }
}
