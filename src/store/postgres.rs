//! PostgreSQL Store implementation — selected via `--database-url`.
//!
//! Requires the `postgres` feature flag and a running PostgreSQL instance
//! with the schema from `migrations/001_initial.sql` applied.

use super::{Store, TxnData, TxnEntry};
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

    // ── Logs ─────────────────────────────────────────────────────

    async fn add_log(&self, log: SyntheticLog) -> anyhow::Result<()> {
        let client = self.pool.get().await?;

        // Get and increment log_counter atomically
        let row = client
            .query_one(
                "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                &[],
            )
            .await?;
        let log_index: i64 = row.get(0);

        let topics: Vec<&str> = log.topics.iter().map(|s| s.as_str()).collect();
        client
            .execute(
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
                let bh_bytes: &[u8] = r.get(5);
                let mut bh = [0u8; 32];
                if bh_bytes.len() == 32 {
                    bh.copy_from_slice(bh_bytes);
                }
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
                let bh_bytes: &[u8] = r.get(5);
                let mut bh = [0u8; 32];
                if bh_bytes.len() == 32 {
                    bh.copy_from_slice(bh_bytes);
                }
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
            let mut buf = [0u8; 32];
            if bytes.len() == 32 {
                buf.copy_from_slice(bytes);
                Some(buf)
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
                mainnet_exit_root: mainnet.and_then(|v| {
                    let mut buf = [0u8; 32];
                    if v.len() == 32 {
                        buf.copy_from_slice(v);
                        Some(buf)
                    } else {
                        None
                    }
                }),
                rollup_exit_root: rollup.and_then(|v| {
                    let mut buf = [0u8; 32];
                    if v.len() == 32 {
                        buf.copy_from_slice(v);
                        Some(buf)
                    } else {
                        None
                    }
                }),
                block_number: r.get::<_, i64>(2) as u64,
                timestamp: r.get::<_, i64>(3) as u64,
            }
        }))
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

        // Task 2: Wrap hash chain read-compute-write in a DB transaction
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;

        let row = txn
            .query_one(
                "SELECT hash_chain_value FROM service_state WHERE id = 1 FOR UPDATE",
                &[],
            )
            .await?;
        let old_bytes: &[u8] = row.get(0);
        let mut old_chain = [0u8; 32];
        if old_bytes.len() == 32 {
            old_chain.copy_from_slice(old_bytes);
        }

        let mut hasher = Keccak256::new();
        hasher.update(old_chain);
        hasher.update(global_exit_root);
        let new_chain: [u8; 32] = hasher.finalize().into();

        txn.execute(
            "UPDATE service_state SET hash_chain_value = $1, updated_at = now() WHERE id = 1",
            &[&new_chain.as_slice()],
        )
        .await?;

        txn.commit().await?;

        let log = SyntheticLog {
            address: L2_GLOBAL_EXIT_ROOT_ADDRESS.to_string(),
            topics: vec![
                UPDATE_HASH_CHAIN_VALUE_TOPIC.to_string(),
                format!("0x{}", hex::encode(global_exit_root)),
                format!("0x{}", hex::encode(new_chain)),
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
        let client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");

        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(msg) => ("failed", Some(msg.as_str())),
        };

        client
            .execute(
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
            tracing::info!("PgStore: committed txn {tx_hash}");

            // Fetch attached logs and add them to synthetic_logs
            let log_rows = client
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

                let log = SyntheticLog {
                    address: bridge_address.clone(),
                    topics,
                    data: data_str,
                    block_number: block_num,
                    block_hash,
                    transaction_hash: format!("{tx_hash:#x}"),
                    transaction_index: 0,
                    log_index: 0,
                    removed: false,
                };
                self.add_log(log).await?;
            }
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

        // Deserialize envelope
        use alloy::eips::Decodable2718;
        let Some(envelope) = TxEnvelope::decode_2718(&mut &envelope_bytes[..]).ok() else {
            return Ok(None);
        };
        let Some(signer) = signer_str.parse::<Address>().ok() else {
            return Ok(None);
        };

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
            .filter_map(|r| {
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
                Some(LogData::new_unchecked(topics, data_bytes.into()))
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
            if let Some(hash) = self.txn_pending_by_miden_id(*id).await? {
                if let Err(e) = self.txn_commit(hash, Ok(()), block_num, block_hash).await {
                    tracing::warn!("PgStore: failed to commit transaction {hash}: {e}");
                }
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
            if let Ok(hash) = hash_str.parse::<TxHash>() {
                if let Err(e) = self
                    .txn_commit(hash, Err("expired".to_string()), block_num, block_hash)
                    .await
                {
                    tracing::warn!("PgStore: failed to expire transaction {hash}: {e}");
                }
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

    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32> {
        let client = self.pool.get().await?;
        let row = client
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
        Ok(row.get::<_, i32>(0) as u32)
    }
}
