//! PostgreSQL Store implementation — selected via `--database-url`.
//!
//! Requires the `postgres` feature flag and a running PostgreSQL instance
//! with the schema from `migrations/001_initial.sql` applied.

use super::{
    FaucetEntry, Store, TxnData, TxnEntry, UnbridgeableBridgeOut, UnbridgeableBridgeOutReason,
    UnclaimableClaim, UnclaimableReason,
};
use crate::bridge_address::get_bridge_address;
use crate::log_synthesis::{
    GerEntry, L2_GLOBAL_EXIT_ROOT_ADDRESS, LogFilter, SyntheticLog, UPDATE_HASH_CHAIN_VALUE_TOPIC,
};
use alloy::consensus::TxEnvelope;
use alloy::eips::Encodable2718;
use alloy::primitives::{Address, LogData, TxHash, U256};
use deadpool_postgres::{Manager, Pool};
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;
use sha3::{Digest, Keccak256};
use tokio_postgres::types::ToSql;

pub struct PgStore {
    pool: Pool,
}

/// Convert a byte slice to a fixed 32-byte array, zero-padded if too short.
fn bytes_to_array_32(bytes: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    if bytes.len() == 32 {
        arr.copy_from_slice(bytes);
    }
    arr
}

impl PgStore {
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        let config: tokio_postgres::Config = database_url.parse()?;
        let manager = Manager::new(config, tokio_postgres::NoTls);
        let pool = Pool::builder(manager).max_size(16).build()?;

        // Verify connectivity
        let _client = pool.get().await?;
        tracing::info!("PgStore: connected to PostgreSQL");

        Ok(Self { pool })
    }
}

/// Parse a TransactionId hex string (from `TransactionId::to_hex()`) back to a
/// `TransactionId`. The format is `0x` followed by 64 hex chars representing
/// 32 bytes (4 little-endian Felt u64s).
fn parse_transaction_id(hex_str: &str) -> Option<TransactionId> {
    let word = Word::parse(hex_str).ok()?;
    Some(TransactionId::from_raw(word))
}

#[async_trait::async_trait]
impl Store for PgStore {
    // ── Block number ─────────────────────────────────────────────

    async fn get_latest_block_number(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT latest_block_number FROM service_state WHERE id = 1",
                &[],
            )
            .await?;
        let val: i64 = row.get(0);
        Ok(val as u64)
    }

    async fn set_latest_block_number(&self, n: u64) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE service_state SET latest_block_number = $1, updated_at = now() WHERE id = 1",
                &[&(n as i64)],
            )
            .await?;
        Ok(())
    }

    async fn advance_block_number(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "UPDATE service_state SET latest_block_number = latest_block_number + 1, updated_at = now() WHERE id = 1 RETURNING latest_block_number",
                &[],
            )
            .await?;
        let val: i64 = row.get(0);
        Ok(val as u64)
    }

    async fn get_raw_miden_height(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT raw_miden_height FROM service_state WHERE id = 1",
                &[],
            )
            .await?;
        let val: i64 = row.get(0);
        Ok(val as u64)
    }

    async fn set_raw_miden_height(&self, height: u64) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE service_state SET raw_miden_height = $1, updated_at = now() WHERE id = 1",
                &[&(height as i64)],
            )
            .await?;
        Ok(())
    }

    async fn get_l1_indexer_cursor(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT last_processed FROM l1_indexer_state WHERE id = 1",
                &[],
            )
            .await?;
        match row {
            Some(r) => {
                let val: i64 = r.get(0);
                Ok(val as u64)
            }
            None => Ok(0),
        }
    }

    async fn set_l1_indexer_cursor(&self, block: u64) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE l1_indexer_state SET last_processed = $1, updated_at = now() WHERE id = 1",
                &[&(block as i64)],
            )
            .await?;
        Ok(())
    }

    // ── Logs ─────────────────────────────────────────────────────

    async fn add_log(&self, log: SyntheticLog) -> anyhow::Result<()> {
        let mut client = self.pool.get().await?;

        // S3 — atomic counter UPDATE + INSERT.
        //
        // Pre-fix the counter was incremented in one connection roundtrip
        // (UPDATE ... RETURNING log_counter - 1) and the INSERT happened in
        // a SEPARATE roundtrip. If the INSERT failed (constraint violation,
        // disk full, network hiccup), the counter had already advanced and
        // no row existed at that index — leaving permanent gaps in
        // log_index that downstream consumers (eth_getLogs callers
        // iterating by index) would silently skip.
        //
        // Now both run inside a single tokio_postgres transaction; the
        // commit/rollback boundary preserves the invariant that every
        // bumped counter has a matching row.
        let tx = client.transaction().await?;

        let row = tx
            .query_one(
                "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);

        let topics: Vec<&str> = log.topics.iter().map(|s| s.as_str()).collect();
        tx.execute(
            "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &log_index,
                &log.address,
                &topics,
                &log.data,
                &(log.block_number as i64),
                &log.block_hash.as_slice(),
                &log.transaction_hash,
                &(log.transaction_index as i64),
                &log.removed,
            ],
        )
        .await?;

        tx.commit().await?;

        tracing::debug!(
            block_number = log.block_number,
            tx_hash = %log.transaction_hash,
            "PgStore: log inserted"
        );
        Ok(())
    }

    async fn get_logs(
        &self,
        filter: &LogFilter,
        current_block: u64,
    ) -> anyhow::Result<Vec<SyntheticLog>> {
        let client = self.pool.get().await?;
        let from = filter.from_block_number(current_block) as i64;
        let to = filter.to_block_number(current_block) as i64;

        let rows = client
            .query(
                "SELECT log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed
                 FROM synthetic_logs
                 WHERE block_number >= $1 AND block_number <= $2
                 ORDER BY block_number, log_index
                 LIMIT 1000",
                &[&from, &to],
            )
            .await?;

        let logs: Vec<SyntheticLog> = rows
            .iter()
            .map(|r| {
                let bh = bytes_to_array_32(r.get(5));
                let topics: Vec<String> = r.get(2);
                SyntheticLog {
                    log_index: r.get::<_, i64>(0) as u64,
                    address: r.get(1),
                    topics,
                    data: r.get(3),
                    block_number: r.get::<_, i64>(4) as u64,
                    block_hash: bh,
                    transaction_hash: r.get(6),
                    transaction_index: r.get::<_, i64>(7) as u64,
                    removed: r.get(8),
                }
            })
            .collect();

        Ok(logs
            .into_iter()
            .filter(|l| filter.matches(l, current_block))
            .collect())
    }

    async fn get_logs_for_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<SyntheticLog>> {
        let client = self.pool.get().await?;
        let key = tx_hash.to_lowercase();

        let rows = client
            .query(
                "SELECT log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed
                 FROM synthetic_logs
                 WHERE lower(transaction_hash) = $1
                 ORDER BY log_index",
                &[&key],
            )
            .await
            ?;

        Ok(rows
            .iter()
            .map(|r| {
                let bh = bytes_to_array_32(r.get(5));
                let topics: Vec<String> = r.get(2);
                SyntheticLog {
                    log_index: r.get::<_, i64>(0) as u64,
                    address: r.get(1),
                    topics,
                    data: r.get(3),
                    block_number: r.get::<_, i64>(4) as u64,
                    block_hash: bh,
                    transaction_hash: r.get(6),
                    transaction_index: r.get::<_, i64>(7) as u64,
                    removed: r.get(8),
                }
            })
            .collect())
    }

    // ── GER ──────────────────────────────────────────────────────

    async fn has_seen_ger(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT 1 FROM ger_entries WHERE ger_hash = $1",
                &[&ger.as_slice()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn mark_ger_seen(&self, ger: &[u8; 32], entry: GerEntry) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let mainnet: Option<Vec<u8>> = entry.mainnet_exit_root.map(|r| r.to_vec());
        let rollup: Option<Vec<u8>> = entry.rollup_exit_root.map(|r| r.to_vec());

        let result = client
            .execute(
                "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (ger_hash) DO NOTHING",
                &[
                    &ger.as_slice(),
                    &mainnet as &(dyn ToSql + Sync),
                    &rollup as &(dyn ToSql + Sync),
                    &(entry.block_number as i64),
                    &(entry.timestamp as i64),
                ],
            )
            .await?;
        Ok(result > 0)
    }

    async fn get_latest_ger(&self) -> anyhow::Result<Option<[u8; 32]>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT ger_hash FROM ger_entries ORDER BY created_at DESC LIMIT 1",
                &[],
            )
            .await?;

        Ok(rows.first().and_then(|r| {
            let bytes: &[u8] = r.get(0);
            if bytes.len() == 32 {
                Some(bytes_to_array_32(bytes))
            } else {
                None
            }
        }))
    }

    async fn get_ger_entry(&self, ger: &[u8; 32]) -> anyhow::Result<Option<GerEntry>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT mainnet_exit_root, rollup_exit_root, block_number, timestamp FROM ger_entries WHERE ger_hash = $1",
                &[&ger.as_slice()],
            )
            .await
            ?;

        Ok(rows.first().map(|r| {
            let mainnet: Option<&[u8]> = r.get(0);
            let rollup: Option<&[u8]> = r.get(1);
            GerEntry {
                mainnet_exit_root: mainnet.filter(|v| v.len() == 32).map(bytes_to_array_32),
                rollup_exit_root: rollup.filter(|v| v.len() == 32).map(bytes_to_array_32),
                block_number: r.get::<_, i64>(2) as u64,
                timestamp: r.get::<_, i64>(3) as u64,
            }
        }))
    }

    async fn set_ger_exit_roots(
        &self,
        ger: &[u8; 32],
        mainnet_exit_root: [u8; 32],
        rollup_exit_root: [u8; 32],
        l1_block_number: u64,
        l1_timestamp: u64,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let mainnet = mainnet_exit_root.to_vec();
        let rollup = rollup_exit_root.to_vec();
        client
            .execute(
                "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (ger_hash) DO UPDATE
                 SET mainnet_exit_root = EXCLUDED.mainnet_exit_root,
                     rollup_exit_root  = EXCLUDED.rollup_exit_root,
                     block_number      = EXCLUDED.block_number,
                     timestamp         = EXCLUDED.timestamp",
                &[
                    &ger.as_slice(),
                    &mainnet,
                    &rollup,
                    &(l1_block_number as i64),
                    &(l1_timestamp as i64),
                ],
            )
            .await?;
        Ok(())
    }

    async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT is_injected FROM ger_entries WHERE ger_hash = $1 AND is_injected = TRUE",
                &[&ger.as_slice()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn mark_ger_injected(&self, ger: [u8; 32]) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO ger_entries (ger_hash, block_number, timestamp, is_injected)
                 VALUES ($1, 0, 0, TRUE)
                 ON CONFLICT (ger_hash) DO UPDATE SET is_injected = TRUE",
                &[&ger.as_slice()],
            )
            .await?;
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
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;

        let mainnet: Option<Vec<u8>> = mainnet_exit_root.map(|root| root.to_vec());
        let rollup: Option<Vec<u8>> = rollup_exit_root.map(|root| root.to_vec());
        txn.execute(
            "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (ger_hash) DO NOTHING",
            &[
                &global_exit_root.as_slice(),
                &mainnet as &(dyn ToSql + Sync),
                &rollup as &(dyn ToSql + Sync),
                &(block_number as i64),
                &(timestamp as i64),
            ],
        )
        .await?;

        let row = txn
            .query_one(
                "SELECT hash_chain_value FROM service_state WHERE id = 1 FOR UPDATE",
                &[],
            )
            .await?;
        let old_chain = bytes_to_array_32(row.get(0));

        let mut hasher = Keccak256::new();
        hasher.update(old_chain);
        hasher.update(global_exit_root);
        let new_chain: [u8; 32] = hasher.finalize().into();

        txn.execute(
            "UPDATE service_state SET hash_chain_value = $1, updated_at = now() WHERE id = 1",
            &[&new_chain.as_slice()],
        )
        .await?;

        let row = txn
            .query_one(
                "UPDATE service_state
                 SET log_counter = log_counter + 1, updated_at = now()
                 WHERE id = 1
                 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);
        let topics = [
            UPDATE_HASH_CHAIN_VALUE_TOPIC.to_string(),
            format!("0x{}", hex::encode(global_exit_root)),
            format!("0x{}", hex::encode(new_chain)),
        ];
        let topic_refs: Vec<&str> = topics.iter().map(|topic| topic.as_str()).collect();
        txn.execute(
            "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &log_index,
                &L2_GLOBAL_EXIT_ROOT_ADDRESS,
                &topic_refs,
                &"0x",
                &(block_number as i64),
                &block_hash.as_slice(),
                &tx_hash,
                &0_i64,
                &false,
            ],
        )
        .await?;

        txn.commit().await?;
        Ok(())
    }

    /// G5 + Cantina #5: atomic GER commit. The store ALLOCATES the synthetic
    /// block number inside this transaction (no caller-chosen block number),
    /// then folds all four writes (ger_entries upsert, hash chain update,
    /// synthetic_logs insert, is_injected flag) into ONE postgres transaction
    /// so a process crash anywhere mid-sequence either leaves nothing or
    /// leaves the full GER commit visible. Returns the allocated block number.
    async fn commit_ger_event_atomic(
        &self,
        tx_hash: &str,
        global_exit_root: &[u8; 32],
        mainnet_exit_root: Option<[u8; 32]>,
        rollup_exit_root: Option<[u8; 32]>,
    ) -> anyhow::Result<u64> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;

        // Cantina #5 — allocate the synthetic block number INSIDE this
        // transaction; the block hash and synthetic timestamp are pure
        // functions of the number.
        let block_row = txn
            .query_one(
                "UPDATE service_state SET latest_block_number = latest_block_number + 1, updated_at = now() WHERE id = 1 RETURNING latest_block_number",
                &[],
            )
            .await?;
        let block_number: u64 = block_row.get::<_, i64>(0) as u64;
        let block_hash = crate::block_state::SyntheticBlock::compute_hash_for_number(block_number);
        let timestamp = crate::block_state::BlockState::synthetic_timestamp(block_number);

        // Pre-existing add_ger_update_event sequence — duplicated here
        // so the whole bundle is one transaction.
        let mainnet: Option<Vec<u8>> = mainnet_exit_root.map(|root| root.to_vec());
        let rollup: Option<Vec<u8>> = rollup_exit_root.map(|root| root.to_vec());
        txn.execute(
            "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (ger_hash) DO NOTHING",
            &[
                &global_exit_root.as_slice(),
                &mainnet as &(dyn ToSql + Sync),
                &rollup as &(dyn ToSql + Sync),
                &(block_number as i64),
                &(timestamp as i64),
            ],
        )
        .await?;

        let row = txn
            .query_one(
                "SELECT hash_chain_value FROM service_state WHERE id = 1 FOR UPDATE",
                &[],
            )
            .await?;
        let old_chain = bytes_to_array_32(row.get(0));

        let mut hasher = Keccak256::new();
        hasher.update(old_chain);
        hasher.update(global_exit_root);
        let new_chain: [u8; 32] = hasher.finalize().into();

        txn.execute(
            "UPDATE service_state SET hash_chain_value = $1, updated_at = now() WHERE id = 1",
            &[&new_chain.as_slice()],
        )
        .await?;

        let row = txn
            .query_one(
                "UPDATE service_state
                 SET log_counter = log_counter + 1, updated_at = now()
                 WHERE id = 1
                 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);
        let topics = [
            UPDATE_HASH_CHAIN_VALUE_TOPIC.to_string(),
            format!("0x{}", hex::encode(global_exit_root)),
            format!("0x{}", hex::encode(new_chain)),
        ];
        let topic_refs: Vec<&str> = topics.iter().map(|topic| topic.as_str()).collect();
        txn.execute(
            "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &log_index,
                &L2_GLOBAL_EXIT_ROOT_ADDRESS,
                &topic_refs,
                &"0x",
                &(block_number as i64),
                &block_hash.as_slice(),
                &tx_hash,
                &0_i64,
                &false,
            ],
        )
        .await?;

        // mark_ger_injected, fused into the same transaction. The original
        // out-of-band call did `INSERT … ON CONFLICT … DO UPDATE`. Here the
        // row from the ger_entries insert above MUST exist, so we just flip
        // the flag.
        txn.execute(
            "UPDATE ger_entries SET is_injected = TRUE WHERE ger_hash = $1",
            &[&global_exit_root.as_slice()],
        )
        .await?;

        // The cursor was already advanced by the allocation UPDATE above —
        // no separate set_latest_block_number step (Cantina #5).
        txn.commit().await?;
        Ok(block_number)
    }

    // ── Transactions ─────────────────────────────────────────────

    async fn txn_begin(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");
        let miden_id = entry.id.map(|id| id.to_hex());
        let signer_str = format!("{:#x}", entry.signer);

        // Serialize envelope to RLP bytes
        let mut envelope_bytes = Vec::new();
        entry.envelope.encode_2718(&mut envelope_bytes);

        client
            .execute(
                "INSERT INTO transactions (tx_hash, miden_tx_id, envelope_bytes, signer, expires_at, status, block_number)
                 VALUES ($1, $2, $3, $4, $5, 'pending', 0)",
                &[
                    &hash_str,
                    &miden_id as &(dyn ToSql + Sync),
                    &envelope_bytes,
                    &signer_str,
                    &entry.expires_at.map(|v| v as i64) as &(dyn ToSql + Sync),
                ],
            )
            .await?;

        // Store attached logs
        for log_data in &entry.logs {
            let topics_bytes: Vec<Vec<u8>> = log_data
                .topics()
                .iter()
                .map(|t| t.as_slice().to_vec())
                .collect();
            client
                .execute(
                    "INSERT INTO transaction_logs (tx_hash, topics, data) VALUES ($1, $2, $3)",
                    &[&hash_str, &topics_bytes, &log_data.data.as_ref()],
                )
                .await?;
        }

        Ok(())
    }

    async fn txn_commit(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()> {
        let mut client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");

        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(msg) => ("failed", Some(msg.as_str())),
        };

        // S4 — atomic status update + log materialisation.
        //
        // Pre-fix the status `UPDATE transactions ... WHERE tx_hash = $`
        // committed first; only afterwards did we loop over
        // `transaction_logs` and call `self.add_log(log)` for each.
        // Each `add_log` opened its OWN transaction. If any of them
        // failed (e.g. log_index unique-constraint violation, network
        // hiccup), the `transactions` row already said 'success' with
        // zero or partial synthetic logs. `eth_getTransactionReceipt`
        // would then return a confirmed tx whose log set is incomplete
        // — exactly the kind of silent corruption indexer consumers
        // can't recover from.
        //
        // Now both run inside a single tokio_postgres transaction. On
        // any failure the rollback restores the 'pending' status AND
        // any partial log inserts.
        let tx = client.transaction().await?;

        tx.execute(
            "UPDATE transactions SET status = $1, error_message = $2, block_number = $3, updated_at = now() WHERE tx_hash = $4",
            &[
                &status,
                &error_msg as &(dyn ToSql + Sync),
                &(block_num as i64),
                &hash_str,
            ],
        )
        .await?;

        if result.is_ok() {
            // C11 — fold the latest_block_number advance into the same
            // transaction. Monotonic guard: only bump if block_num is
            // strictly greater than the current cursor, so sweep callers
            // committing earlier txs (txn_commit_pending replaying a
            // batch at the Miden block they were committed at) don't
            // roll the synthetic-log cursor backwards. The synthetic-
            // log virtual-block path (claim.rs, ger.rs) always passes
            // current_latest+1, so this collapses two roundtrips into
            // one and removes the gap where a crash between txn_commit
            // and set_latest_block_number would leave a finalized log
            // unreachable via eth_blockNumber.
            tx.execute(
                "UPDATE service_state SET latest_block_number = $1, updated_at = now() WHERE id = 1 AND $1 > latest_block_number",
                &[&(block_num as i64)],
            )
            .await?;

            // Fetch attached logs (within the same txn so we see a consistent
            // snapshot — no race with a concurrent txn_begin updating the
            // attached log set).
            let log_rows = tx
                .query(
                    "SELECT topics, data FROM transaction_logs WHERE tx_hash = $1",
                    &[&hash_str],
                )
                .await?;

            let bridge_address = get_bridge_address().to_string();
            for row in &log_rows {
                let topics_bytes: Vec<Vec<u8>> = row.get(0);
                let data_bytes: &[u8] = row.get(1);
                let topics: Vec<String> = topics_bytes
                    .iter()
                    .map(|t| format!("0x{}", hex::encode(t)))
                    .collect();
                let data_str = format!("0x{}", hex::encode(data_bytes));

                // Inline the add_log logic so the counter UPDATE + INSERT
                // run inside the SAME outer txn. Pre-fix `self.add_log`
                // opened a nested txn that committed independently.
                let counter_row = tx
                    .query_one(
                        "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                        &[],
                    )
                    .await?;
                let log_index: i64 = counter_row.get(0);
                let topic_strs: Vec<&str> = topics.iter().map(|s| s.as_str()).collect();
                tx.execute(
                    "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    &[
                        &log_index,
                        &bridge_address,
                        &topic_strs,
                        &data_str,
                        &(block_num as i64),
                        &block_hash.as_slice(),
                        &hash_str,
                        &0i64,
                        &false,
                    ],
                )
                .await?;
            }
        }

        tx.commit().await?;

        if result.is_ok() {
            tracing::info!("PgStore: committed txn {tx_hash}");
        } else if let Err(ref err) = result {
            tracing::error!("PgStore: failed txn {tx_hash}: {err}");
        }

        Ok(())
    }

    async fn txn_receipt(
        &self,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<(Result<(), String>, u64)>> {
        let client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");

        let rows = client
            .query(
                "SELECT status, error_message, block_number FROM transactions WHERE tx_hash = $1",
                &[&hash_str],
            )
            .await?;

        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let status: &str = row.get(0);
        let error_msg: Option<&str> = row.get(1);
        let block_num: i64 = row.get(2);

        match status {
            "success" => Ok(Some((Ok(()), block_num as u64))),
            "failed" => Ok(Some((
                Err(error_msg.unwrap_or("unknown error").to_string()),
                block_num as u64,
            ))),
            _ => Ok(None), // pending
        }
    }

    async fn txn_get(&self, tx_hash: TxHash) -> anyhow::Result<Option<TxnData>> {
        let client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");

        let rows = client
            .query(
                "SELECT miden_tx_id, envelope_bytes, signer, expires_at, status, error_message, block_number
                 FROM transactions WHERE tx_hash = $1",
                &[&hash_str],
            )
            .await
            ?;

        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let envelope_bytes: &[u8] = row.get(1);
        let signer_str: &str = row.get(2);
        let expires_at: Option<i64> = row.get(3);
        let status: &str = row.get(4);
        let error_msg: Option<&str> = row.get(5);
        let block_num: i64 = row.get(6);

        // Deserialize envelope.
        //
        // S9 — return Err on decode failure rather than None. Pre-fix a
        // corrupt or schema-drift envelope row was indistinguishable from
        // "tx not found" — `eth_getTransactionByHash` would lie to clients.
        // Surface the failure as a real error so operators see the
        // corruption (and the metric counter increments).
        use alloy::eips::Decodable2718;
        let envelope = TxEnvelope::decode_2718(&mut &envelope_bytes[..]).map_err(|e| {
            ::metrics::counter!("store_envelope_decode_errors_total").increment(1);
            tracing::error!(
                target: "store::postgres",
                tx_hash = %hash_str,
                error = ?e,
                "S9: TxEnvelope decode failed; returning error rather than masking as not-found"
            );
            anyhow::anyhow!(
                "stored TxEnvelope for {hash_str} cannot be decoded ({e}); \
                 row is corrupt or schema drifted"
            )
        })?;
        let signer = signer_str.parse::<Address>().map_err(|e| {
            ::metrics::counter!("store_envelope_decode_errors_total").increment(1);
            anyhow::anyhow!("stored signer for {hash_str} is not a valid Address ({e})")
        })?;

        let result = match status {
            "success" => Some(Ok(())),
            "failed" => Some(Err(error_msg.unwrap_or("").to_string())),
            _ => None,
        };

        // Fetch logs
        let log_rows = client
            .query(
                "SELECT topics, data FROM transaction_logs WHERE tx_hash = $1",
                &[&hash_str],
            )
            .await?;

        let logs: Vec<LogData> = log_rows
            .iter()
            .map(|r| {
                let topics_bytes: Vec<Vec<u8>> = r.get(0);
                let data_bytes: Vec<u8> = r.get(1);
                let topics: Vec<alloy::primitives::B256> = topics_bytes
                    .iter()
                    .filter_map(|t| {
                        if t.len() == 32 {
                            Some(alloy::primitives::B256::from_slice(t))
                        } else {
                            None
                        }
                    })
                    .collect();
                LogData::new_unchecked(topics, data_bytes.into())
            })
            .collect();

        // Task 3: Deserialize TransactionId from stored hex string
        let miden_id_str: Option<&str> = row.get(0);
        let id = miden_id_str.and_then(|s| {
            let hex_str = if s.starts_with("0x") {
                s.to_string()
            } else {
                format!("0x{s}")
            };
            parse_transaction_id(&hex_str)
        });

        Ok(Some(TxnData {
            id,
            envelope,
            signer,
            expires_at: expires_at.map(|v| v as u64),
            result,
            block_num: block_num as u64,
            logs,
        }))
    }

    async fn txn_pending_by_miden_id(&self, id: TransactionId) -> anyhow::Result<Option<TxHash>> {
        let client = self.pool.get().await?;
        let id_str = id.to_hex();

        let rows = client
            .query(
                "SELECT tx_hash FROM transactions WHERE miden_tx_id = $1 AND status = 'pending'",
                &[&id_str],
            )
            .await?;

        Ok(rows.first().and_then(|r| {
            let hash_str: &str = r.get(0);
            hash_str.parse().ok()
        }))
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
                tracing::warn!("PgStore: failed to commit transaction {hash}: {e}");
            }
        }
        Ok(())
    }

    async fn txn_expire_pending(&self, block_num: u64, block_hash: [u8; 32]) -> anyhow::Result<()> {
        let client = self.pool.get().await?;

        let rows = client
            .query(
                "SELECT tx_hash FROM transactions WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at <= $1",
                &[&(block_num as i64)],
            )
            .await
            ?;

        for row in &rows {
            let hash_str: &str = row.get(0);
            if let Ok(hash) = hash_str.parse::<TxHash>()
                && let Err(e) = self
                    .txn_commit(hash, Err("expired".to_string()), block_num, block_hash)
                    .await
            {
                tracing::warn!("PgStore: failed to expire transaction {hash}: {e}");
            }
        }
        Ok(())
    }

    // ── Nonces ───────────────────────────────────────────────────

    async fn nonce_get(&self, addr: &str) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let key = addr.to_lowercase();
        let rows = client
            .query("SELECT nonce FROM nonces WHERE address = $1", &[&key])
            .await?;
        Ok(rows.first().map(|r| r.get::<_, i64>(0) as u64).unwrap_or(0))
    }

    async fn nonce_increment(&self, addr: &str) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let key = addr.to_lowercase();
        let row = client
            .query_one(
                "INSERT INTO nonces (address, nonce) VALUES ($1, 1)
                 ON CONFLICT (address) DO UPDATE SET nonce = nonces.nonce + 1
                 RETURNING nonce - 1",
                &[&key],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as u64)
    }

    // ── Claims ───────────────────────────────────────────────────

    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let result = client
            .execute(
                "INSERT INTO claimed_indices (global_index) VALUES ($1)",
                &[&key],
            )
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(_) => anyhow::bail!("claim already submitted for global_index {global_index}"),
        }
    }

    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        client
            .execute(
                "DELETE FROM claimed_indices WHERE global_index = $1",
                &[&key],
            )
            .await?;
        Ok(())
    }

    async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let rows = client
            .query(
                "SELECT 1 FROM claimed_indices WHERE global_index = $1",
                &[&key],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn record_unclaimable_claim(&self, entry: UnclaimableClaim) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let global_index_hex = format!("{:#x}", entry.global_index);
        let destination_hex = format!("{:#x}", entry.destination_address);
        let origin_hex = format!("{:#x}", entry.origin_address);
        let amount_hex = format!("{:#x}", entry.amount);
        let eth_tx_hex = format!("{:#x}", entry.eth_tx_hash);
        let reason = entry.reason.as_str();
        let origin_network = entry.origin_network as i32;

        // First-write wins: ON CONFLICT DO NOTHING so aggkit retries don't error.
        // `INSERT … RETURNING` tells us whether a row was actually added.
        let rows = client
            .query(
                "INSERT INTO unclaimable_claims \
                 (global_index, destination_address, origin_network, origin_address, amount, reason, eth_tx_hash) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (global_index) DO NOTHING \
                 RETURNING global_index",
                &[
                    &global_index_hex,
                    &destination_hex,
                    &origin_network,
                    &origin_hex,
                    &amount_hex,
                    &reason,
                    &eth_tx_hex,
                ],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn get_unclaimable_claim(
        &self,
        global_index: &U256,
    ) -> anyhow::Result<Option<UnclaimableClaim>> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let rows = client
            .query(
                "SELECT global_index, destination_address, origin_network, origin_address, \
                        amount, reason, eth_tx_hash \
                 FROM unclaimable_claims WHERE global_index = $1",
                &[&key],
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let global_index_hex: String = row.get(0);
        let destination_hex: String = row.get(1);
        let origin_network: i32 = row.get(2);
        let origin_hex: String = row.get(3);
        let amount_hex: String = row.get(4);
        let reason_str: String = row.get(5);
        let eth_tx_hex: String = row.get(6);

        let reason = match reason_str.as_str() {
            "unresolvable_destination" => UnclaimableReason::UnresolvableDestination,
            other => anyhow::bail!("unknown unclaimable_claims.reason value: {other}"),
        };

        Ok(Some(UnclaimableClaim {
            global_index: U256::from_str_radix(global_index_hex.trim_start_matches("0x"), 16)?,
            destination_address: destination_hex.parse()?,
            origin_network: u32::try_from(origin_network)?,
            origin_address: origin_hex.parse()?,
            amount: U256::from_str_radix(amount_hex.trim_start_matches("0x"), 16)?,
            reason,
            eth_tx_hash: eth_tx_hex.parse()?,
        }))
    }

    // ── Unbridgeable bridge-outs (Cantina MA#18) ─────────────────

    async fn record_unbridgeable_bridge_out(
        &self,
        entry: UnbridgeableBridgeOut,
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let bridge_account = entry.bridge_account.to_hex();
        let reason = entry.reason.as_str();
        let observed_block = entry.observed_block as i64;

        // First-write wins: ON CONFLICT DO NOTHING so repeated sync ticks
        // observing the same erased note don't error or duplicate rows.
        let rows = client
            .query(
                "INSERT INTO unbridgeable_bridge_outs \
                 (note_id, bridge_account, reason, detail, note_dump, observed_block) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (note_id) DO NOTHING \
                 RETURNING note_id",
                &[
                    &entry.note_id,
                    &bridge_account,
                    &reason,
                    &entry.detail,
                    &entry.note_dump,
                    &observed_block,
                ],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn get_unbridgeable_bridge_out(
        &self,
        note_id: &str,
    ) -> anyhow::Result<Option<UnbridgeableBridgeOut>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT note_id, bridge_account, reason, detail, note_dump, observed_block \
                 FROM unbridgeable_bridge_outs WHERE note_id = $1",
                &[&note_id],
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let note_id_col: String = row.get(0);
        let bridge_account_hex: String = row.get(1);
        let reason_str: String = row.get(2);
        let detail: String = row.get(3);
        let note_dump: String = row.get(4);
        let observed_block: i64 = row.get(5);

        let reason = match reason_str.as_str() {
            "storage_parse_failed" => UnbridgeableBridgeOutReason::StorageParseFailed,
            "no_fungible_asset" => UnbridgeableBridgeOutReason::NoFungibleAsset,
            "unknown_faucet" => UnbridgeableBridgeOutReason::UnknownFaucet,
            "amount_overflow" => UnbridgeableBridgeOutReason::AmountOverflow,
            "atomic_commit_failed" => UnbridgeableBridgeOutReason::AtomicCommitFailed,
            other => anyhow::bail!("unknown unbridgeable_bridge_outs.reason value: {other}"),
        };

        Ok(Some(UnbridgeableBridgeOut {
            note_id: note_id_col,
            bridge_account: AccountId::from_hex(&bridge_account_hex)
                .map_err(|e| anyhow::anyhow!("decoding bridge_account from db row: {e}"))?,
            reason,
            detail,
            note_dump,
            observed_block: u64::try_from(observed_block)?,
        }))
    }

    // ── Address mappings ─────────────────────────────────────────

    async fn get_address_mapping(&self, eth: &Address) -> anyhow::Result<Option<AccountId>> {
        let client = self.pool.get().await?;
        let key = format!("{eth:#x}");
        let rows = client
            .query(
                "SELECT miden_account FROM address_mappings WHERE eth_address = $1",
                &[&key],
            )
            .await?;

        Ok(rows.first().and_then(|r| {
            let val: &str = r.get(0);
            AccountId::from_hex(val).ok()
        }))
    }

    async fn set_address_mapping(&self, eth: Address, miden: AccountId) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = format!("{eth:#x}");
        let val = miden.to_hex();
        client
            .execute(
                "INSERT INTO address_mappings (eth_address, miden_account) VALUES ($1, $2)
                 ON CONFLICT (eth_address) DO UPDATE SET miden_account = $2",
                &[&key, &val],
            )
            .await?;
        Ok(())
    }

    // ── Bridge-out ───────────────────────────────────────────────

    async fn is_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT 1 FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn get_deposit_count(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT deposit_counter FROM service_state WHERE id = 1",
                &[],
            )
            .await?;
        // service_state.deposit_counter is `INT NOT NULL` (postgres int4 / Rust i32),
        // not BIGINT. Reading as i64 panics with "error deserializing column 0".
        let val: i32 = row.get(0);
        Ok(val as u64)
    }

    async fn get_processed_deposit_count(&self, note_id: &str) -> anyhow::Result<Option<u32>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT deposit_count FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?;
        Ok(row.map(|r| r.get::<_, i32>(0) as u32))
    }

    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        // Cantina #15 — idempotent: if the note was already processed, REUSE
        // its assigned deposit_count instead of allocating a new one (which
        // would diverge the exported exit index from the Miden LET order).
        if let Some(existing) = txn
            .query_opt(
                "SELECT deposit_count FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?
        {
            let val: i32 = existing.get(0);
            txn.commit().await?;
            return Ok(val as u32);
        }
        let row = txn
            .query_one(
                "WITH counter AS (
                    UPDATE service_state SET deposit_counter = deposit_counter + 1, updated_at = now() WHERE id = 1
                    RETURNING deposit_counter - 1 AS val
                 )
                 INSERT INTO bridge_out_processed (note_id, deposit_count)
                 SELECT $1, val FROM counter
                 RETURNING deposit_count",
                &[&note_id],
            )
            .await?;
        let val = row.get::<_, i32>(0) as u32;
        txn.commit().await?;
        Ok(val)
    }

    /// B1: atomic B2AGG bridge-out commit. Folds five writes into one txn:
    ///   1. service_state.deposit_counter UPDATE → new count
    ///   2. bridge_out_processed INSERT (with the count)
    ///   3. service_state.log_counter UPDATE → new log_index
    ///   4. synthetic_logs INSERT (BridgeEvent)
    ///   5. service_state.latest_block_number UPDATE
    /// Either all visible or none — closes the gap where a crash between
    /// mark_note_processed and add_bridge_event would consume a deposit_count
    /// without ever emitting the synthetic log, causing aggsender to skip
    /// the BridgeEvent permanently.
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
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;

        // Cantina #15 — idempotent retry: if this note was already processed,
        // REUSE its deposit_count and emit no new log. The synthetic block was
        // allocated once for the batch by `allocate_synthetic_block` (Cantina
        // #5/#19), so this method writes into `block_number` and does NOT
        // advance the tip.
        if let Some(existing) = txn
            .query_opt(
                "SELECT deposit_count FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?
        {
            let deposit_count: u32 = existing.get::<_, i32>(0) as u32;
            txn.commit().await?;
            return Ok(deposit_count);
        }

        // 1+2: allocate deposit_count, INSERT processed-note row.
        let row = txn
            .query_one(
                "WITH counter AS (
                    UPDATE service_state SET deposit_counter = deposit_counter + 1, updated_at = now() WHERE id = 1
                    RETURNING deposit_counter - 1 AS val
                 )
                 INSERT INTO bridge_out_processed (note_id, deposit_count)
                 SELECT $1, val FROM counter
                 RETURNING deposit_count",
                &[&note_id],
            )
            .await?;
        let deposit_count: u32 = row.get::<_, i32>(0) as u32;

        // 3: allocate log_index.
        let row = txn
            .query_one(
                "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);

        // 4: encode + insert the synthetic log. Encoding is identical to
        // `add_bridge_event`'s default impl — keeping it inline here keeps
        // the whole bundle in one connection / transaction.
        let data = crate::bridge_out::encode_bridge_event_data(
            leaf_type,
            origin_network,
            origin_address,
            destination_network,
            destination_address,
            amount,
            metadata,
            deposit_count,
        );
        let topics_owned: [String; 1] = [crate::log_synthesis::BRIDGE_EVENT_TOPIC.to_string()];
        let topics: Vec<&str> = topics_owned.iter().map(|s| s.as_str()).collect();
        txn.execute(
            "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &log_index,
                &bridge_address,
                &topics,
                &data,
                &(block_number as i64),
                &block_hash.as_slice(),
                &tx_hash,
                &0_i64,
                &false,
            ],
        )
        .await?;

        // No tip advance here — the synthetic block was allocated once for the
        // whole batch by `allocate_synthetic_block` (Cantina #5/#19).
        txn.commit().await?;
        Ok(deposit_count)
    }

    async fn unmark_note_processed(&self, note_id: &str) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?;
        Ok(())
    }

    // ── Claim watcher ────────────────────────────────────────────

    async fn is_claim_note_processed(&self, note_id: &str) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT 1 FROM claim_watcher_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn mark_claim_note_processed(
        &self,
        note_id: String,
        global_index: [u8; 32],
        block_number: u64,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO claim_watcher_processed (note_id, global_index, block_number)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (note_id) DO NOTHING",
                &[&note_id, &global_index.as_slice(), &(block_number as i64)],
            )
            .await?;
        Ok(())
    }

    async fn has_claim_event_for_global_index(
        &self,
        global_index: &[u8; 32],
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        // 1. Any prior watcher-emission for this leaf.
        let watcher_rows = client
            .query(
                "SELECT 1 FROM claim_watcher_processed WHERE global_index = $1 LIMIT 1",
                &[&global_index.as_slice()],
            )
            .await?;
        if !watcher_rows.is_empty() {
            return Ok(true);
        }
        // 2. Normal-RPC path emission: scan synthetic_logs for a ClaimEvent
        //    whose ABI-encoded data starts with this 32-byte global_index.
        //    `data` is stored as `0x` + lowercase hex, so a prefix string match
        //    against `0x{global_index_hex}` is sound. Bounded by the
        //    ClaimEvent topic to keep the scan narrow.
        let topic = crate::log_synthesis::CLAIM_EVENT_TOPIC;
        let prefix = format!("0x{}", hex::encode(global_index));
        let pattern = format!("{prefix}%");
        let rpc_rows = client
            .query(
                "SELECT 1 FROM synthetic_logs \
                 WHERE topics[1] = $1 AND lower(data) LIKE $2 LIMIT 1",
                &[&topic, &pattern.to_lowercase()],
            )
            .await?;
        Ok(!rpc_rows.is_empty())
    }

    /// Cantina #5: atomic commit for a watcher-synthesised ClaimEvent. The
    /// store ALLOCATES the synthetic block number inside this transaction
    /// (no caller-chosen block number), folding the writes the default impl
    /// chains separately. Mirrors `commit_b2agg_event_atomic` /
    /// `commit_ger_event_atomic`. Returns the allocated block number.
    #[allow(clippy::too_many_arguments)]
    async fn commit_manual_claim_event_atomic(
        &self,
        note_id: String,
        bridge_address: &str,
        tx_hash: &str,
        global_index: [u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) -> anyhow::Result<u64> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;

        // Cantina #5 — allocate the synthetic block number INSIDE this
        // transaction; the block hash is a pure function of the number.
        let block_row = txn
            .query_one(
                "UPDATE service_state SET latest_block_number = latest_block_number + 1, updated_at = now() WHERE id = 1 RETURNING latest_block_number",
                &[],
            )
            .await?;
        let block_number: u64 = block_row.get::<_, i64>(0) as u64;
        let block_hash = crate::block_state::SyntheticBlock::compute_hash_for_number(block_number);

        // 1. Mark the CLAIM note processed (idempotent — second observation no-ops).
        txn.execute(
            "INSERT INTO claim_watcher_processed (note_id, global_index, block_number)
             VALUES ($1, $2, $3)
             ON CONFLICT (note_id) DO NOTHING",
            &[&note_id, &global_index.as_slice(), &(block_number as i64)],
        )
        .await?;

        // 2. Allocate a log_index.
        let row = txn
            .query_one(
                "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);

        // 3. Encode + insert the synthetic ClaimEvent log. Encoding is the
        //    same path `add_claim_event` would take — keep it inline to keep
        //    the bundle in one connection / one transaction.
        let data = crate::log_synthesis::encode_claim_event_data_u64(
            &global_index,
            origin_network,
            origin_address,
            destination_address,
            amount,
        );
        let topics_owned: [String; 1] = [crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()];
        let topics: Vec<&str> = topics_owned.iter().map(|s| s.as_str()).collect();
        txn.execute(
            "INSERT INTO synthetic_logs (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &log_index,
                &bridge_address,
                &topics,
                &data,
                &(block_number as i64),
                &block_hash.as_slice(),
                &tx_hash,
                &0_i64,
                &false,
            ],
        )
        .await?;

        // The cursor was already advanced by the allocation UPDATE above —
        // no separate set_latest_block_number step (Cantina #5).
        txn.commit().await?;
        Ok(block_number)
    }

    // ── Faucet registry ──────────────────────────────────────────

    async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let faucet_id = entry.faucet_id.to_hex();
        client
            .execute(
                "INSERT INTO faucet_registry (faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (faucet_id) DO UPDATE
                 SET origin_address = EXCLUDED.origin_address,
                     origin_network = EXCLUDED.origin_network,
                     symbol = EXCLUDED.symbol,
                     origin_decimals = EXCLUDED.origin_decimals,
                     miden_decimals = EXCLUDED.miden_decimals,
                     scale = EXCLUDED.scale",
                &[
                    &faucet_id,
                    &entry.origin_address.as_slice(),
                    &(entry.origin_network as i32),
                    &entry.symbol,
                    &(entry.origin_decimals as i16),
                    &(entry.miden_decimals as i16),
                    &(entry.scale as i16),
                ],
            )
            .await?;
        tracing::info!(faucet_id = %faucet_id, symbol = %entry.symbol, "PgStore: faucet registered");
        Ok(())
    }

    async fn get_faucet_by_origin(
        &self,
        origin_address: &[u8; 20],
        origin_network: u32,
    ) -> anyhow::Result<Option<FaucetEntry>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale
                 FROM faucet_registry
                 WHERE origin_address = $1 AND origin_network = $2",
                &[&origin_address.as_slice(), &(origin_network as i32)],
            )
            .await?;

        Ok(rows.first().and_then(pg_row_to_faucet_entry))
    }

    async fn find_faucets_by_origin_address(
        &self,
        origin_address: &[u8; 20],
    ) -> anyhow::Result<Vec<FaucetEntry>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale
                 FROM faucet_registry
                 WHERE origin_address = $1",
                &[&origin_address.as_slice()],
            )
            .await?;

        Ok(rows.iter().filter_map(pg_row_to_faucet_entry).collect())
    }

    async fn get_faucet_by_id(&self, faucet_id: AccountId) -> anyhow::Result<Option<FaucetEntry>> {
        let client = self.pool.get().await?;
        let id_str = faucet_id.to_hex();
        let rows = client
            .query(
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale
                 FROM faucet_registry
                 WHERE faucet_id = $1",
                &[&id_str],
            )
            .await?;

        Ok(rows.first().and_then(pg_row_to_faucet_entry))
    }

    async fn list_faucets(&self) -> anyhow::Result<Vec<FaucetEntry>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale
                 FROM faucet_registry
                 ORDER BY created_at",
                &[],
            )
            .await?;

        Ok(rows.iter().filter_map(pg_row_to_faucet_entry).collect())
    }

    // ── Monitor trackers (RD-913) ────────────────────────────────

    async fn burn_serial_seen(&self, serial: &[u8; 32]) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT 1 FROM monitor_burn_serials WHERE serial = $1 LIMIT 1",
                &[&serial.as_slice()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn burn_serial_observe(&self, serial: &[u8; 32]) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        // ON CONFLICT DO NOTHING with RETURNING tells us atomically whether
        // we inserted a new row or hit an existing one. The serial primary
        // key handles the race between two concurrent observations of the
        // same serial — exactly one INSERT wins.
        let rows = client
            .query(
                "INSERT INTO monitor_burn_serials (serial) VALUES ($1) \
                 ON CONFLICT (serial) DO NOTHING RETURNING serial",
                &[&serial.as_slice()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn twin_note_commitments(&self, note_id: &[u8; 32]) -> anyhow::Result<Vec<[u8; 32]>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT commitment FROM monitor_twin_notes \
                 WHERE note_id = $1 ORDER BY first_seen_at",
                &[&note_id.as_slice()],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let bytes: &[u8] = r.get(0);
                if bytes.len() == 32 {
                    Some(bytes_to_array_32(bytes))
                } else {
                    None
                }
            })
            .collect())
    }

    async fn twin_note_observe(
        &self,
        note_id: &[u8; 32],
        commitment: &[u8; 32],
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "INSERT INTO monitor_twin_notes (note_id, commitment) VALUES ($1, $2) \
                 ON CONFLICT (note_id, commitment) DO NOTHING RETURNING note_id",
                &[&note_id.as_slice(), &commitment.as_slice()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn expected_mint_record(
        &self,
        global_index: &[u8; 32],
        expected_mint: &[u8; 32],
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO monitor_expected_mints \
                 (global_index, expected_mint, ticks_pending, alerted) \
                 VALUES ($1, $2, 0, FALSE) \
                 ON CONFLICT (global_index) DO UPDATE \
                 SET expected_mint = EXCLUDED.expected_mint, \
                     ticks_pending = 0, \
                     alerted = FALSE, \
                     updated_at = now()",
                &[&global_index.as_slice(), &expected_mint.as_slice()],
            )
            .await?;
        Ok(())
    }

    async fn expected_mint_remove(&self, global_index: &[u8; 32]) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM monitor_expected_mints WHERE global_index = $1",
                &[&global_index.as_slice()],
            )
            .await?;
        Ok(())
    }

    async fn expected_mint_load_all(&self) -> anyhow::Result<Vec<([u8; 32], [u8; 32], u32, bool)>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT global_index, expected_mint, ticks_pending, alerted \
                 FROM monitor_expected_mints",
                &[],
            )
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let gi: &[u8] = r.get(0);
            let em: &[u8] = r.get(1);
            if gi.len() != 32 || em.len() != 32 {
                continue;
            }
            let ticks: i32 = r.get(2);
            let alerted: bool = r.get(3);
            out.push((
                bytes_to_array_32(gi),
                bytes_to_array_32(em),
                ticks.max(0) as u32,
                alerted,
            ));
        }
        Ok(out)
    }

    async fn expected_mint_update_tick(
        &self,
        global_index: &[u8; 32],
        ticks_pending: u32,
        alerted: bool,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        // Bound ticks_pending into i32 range so the column write never
        // panics on saturating_add edges. INT in postgres is signed 32-bit;
        // u32::MAX would overflow.
        let ticks_i32 = i32::try_from(ticks_pending).unwrap_or(i32::MAX);
        client
            .execute(
                "UPDATE monitor_expected_mints \
                 SET ticks_pending = $1, alerted = $2, updated_at = now() \
                 WHERE global_index = $3",
                &[&ticks_i32, &alerted, &global_index.as_slice()],
            )
            .await?;
        Ok(())
    }
}

fn pg_row_to_faucet_entry(row: &tokio_postgres::Row) -> Option<FaucetEntry> {
    let id_str: &str = row.get(0);
    let faucet_id = AccountId::from_hex(id_str).ok()?;
    let origin_bytes: &[u8] = row.get(1);
    let mut origin_address = [0u8; 20];
    if origin_bytes.len() == 20 {
        origin_address.copy_from_slice(origin_bytes);
    }
    Some(FaucetEntry {
        faucet_id,
        origin_address,
        origin_network: row.get::<_, i32>(2) as u32,
        symbol: row.get(3),
        origin_decimals: row.get::<_, i16>(4) as u8,
        miden_decimals: row.get::<_, i16>(5) as u8,
        scale: row.get::<_, i16>(6) as u8,
    })
}
