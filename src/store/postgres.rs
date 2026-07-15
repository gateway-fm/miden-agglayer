//! PostgreSQL Store implementation — selected via `--database-url`.
//!
//! Requires the `postgres` feature flag and a running PostgreSQL instance
//! with the schema from `migrations/001_initial.sql` applied.

use super::{
    ClaimFence, FaucetEntry, NoteHandoff, NoteHandoffState, PendingNonceFrontier, Store, TxnData,
    TxnEntry, UnbridgeableBridgeOut, UnbridgeableBridgeOutReason, UnclaimableClaim,
    UnclaimableReason,
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

    // ── Synthetic projector cursor (Phase 2a) ────────────────────
    //
    // Persisted as a column on the single-row service_state table, mirroring
    // latest_block_number / log_counter (migration 009).

    async fn get_projector_cursor(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT projector_cursor FROM service_state WHERE id = 1",
                &[],
            )
            .await?;
        let val: i64 = row.get(0);
        Ok(val as u64)
    }

    async fn set_projector_cursor(&self, block: u64) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE service_state SET projector_cursor = $1, updated_at = now() WHERE id = 1",
                &[&(block as i64)],
            )
            .await?;
        Ok(())
    }

    // ── Note-reconciler sweep cursor ─────────────────────────────
    //
    // Persisted as a column on the single-row service_state table, mirroring
    // projector_cursor (migration 010). Written write-behind AFTER a sweep
    // window completes; reset to 0 by the recovery flows so the full-history
    // heal sweep re-runs.

    async fn get_reconcile_cursor(&self) -> anyhow::Result<u64> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT reconcile_cursor FROM service_state WHERE id = 1",
                &[],
            )
            .await?;
        let val: i64 = row.get(0);
        Ok(val as u64)
    }

    async fn set_reconcile_cursor(&self, block: u64) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE service_state SET reconcile_cursor = $1, updated_at = now() WHERE id = 1",
                &[&(block as i64)],
            )
            .await?;
        Ok(())
    }

    // ── Receipts map (Phase 2b substrate; unused in 2a) ──────────
    //
    // First-write-wins evm_tx_hash -> note_commitment (migration 009). The
    // PRIMARY KEY on tx_hash plus ON CONFLICT DO NOTHING enforces first-write
    // semantics; the reverse lookup is served by idx_tx_note_links_note_commitment.

    async fn record_tx_note_link(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO tx_note_links (tx_hash, note_commitment) VALUES ($1, $2)
                 ON CONFLICT (tx_hash) DO NOTHING",
                &[&tx_hash, &note_commitment],
            )
            .await?;
        Ok(())
    }

    async fn get_note_link_for_tx(&self, tx_hash: &str) -> anyhow::Result<Option<String>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT note_commitment FROM tx_note_links WHERE tx_hash = $1",
                &[&tx_hash],
            )
            .await?;
        Ok(row.map(|r| r.get::<_, String>(0)))
    }

    async fn get_tx_for_note(&self, note_commitment: &str) -> anyhow::Result<Option<String>> {
        // First-associated tx for a note: order by created_at so a stable
        // (first) row is returned even if the reverse direction ever has
        // multiple tx_hash rows for one commitment.
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                // Secondary `tx_hash` key makes "first associated tx" stable even
                // when two rows share a `created_at` (possible under load) — a bare
                // `created_at` order leaves the winner up to Postgres.
                "SELECT tx_hash FROM tx_note_links WHERE note_commitment = $1
                 ORDER BY created_at ASC, tx_hash ASC LIMIT 1",
                &[&note_commitment],
            )
            .await?;
        Ok(row.map(|r| r.get::<_, String>(0)))
    }

    async fn get_note_handoff_for_tx(&self, tx_hash: &str) -> anyhow::Result<Option<NoteHandoff>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                "SELECT note_commitment, note_id, handoff_state, prepared_expiration_block
                 FROM tx_note_links WHERE tx_hash = $1",
                &[&tx_hash],
            )
            .await?;
        row.map(|row| {
            let state: String = row.get(2);
            let state = match state.as_str() {
                "prepared" => NoteHandoffState::Prepared,
                "submitted" => NoteHandoffState::Submitted,
                other => anyhow::bail!("unknown note handoff state {other:?} for {tx_hash}"),
            };
            Ok(NoteHandoff {
                note_commitment: row.get(0),
                note_id: row.get(1),
                state,
                expiration_block: row.get::<_, Option<i64>>(3).map(|v| v as u64),
            })
        })
        .transpose()
    }

    async fn pending_note_handoff_txs(
        &self,
        after: Option<TxHash>,
        limit: usize,
    ) -> anyhow::Result<Vec<TxHash>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let client = self.pool.get().await?;
        let after = after.map(|tx_hash| format!("{tx_hash:#x}"));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                "SELECT t.tx_hash
                 FROM transactions t
                 INNER JOIN tx_note_links l ON l.tx_hash = t.tx_hash
                 WHERE t.status = 'pending' AND l.note_id IS NOT NULL
                   AND ($1::TEXT IS NULL OR t.tx_hash > $1)
                 ORDER BY t.tx_hash ASC
                 LIMIT $2",
                &[&after, &limit],
            )
            .await?;
        rows.into_iter()
            .map(|row| {
                let hash: String = row.get(0);
                hash.parse::<TxHash>().map_err(|error| {
                    anyhow::anyhow!("invalid pending transaction hash {hash}: {error}")
                })
            })
            .collect()
    }

    async fn prepare_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
        note_id: &str,
        expiration_block: u64,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO tx_note_links
                    (tx_hash, note_commitment, note_id, handoff_state, prepared_expiration_block)
                 VALUES ($1, $2, $3, 'prepared', $4)
                 ON CONFLICT (tx_hash) DO NOTHING",
                &[
                    &tx_hash,
                    &note_commitment,
                    &note_id,
                    &(expiration_block as i64),
                ],
            )
            .await?;
        let row = client
            .query_one(
                "SELECT note_commitment, note_id FROM tx_note_links WHERE tx_hash = $1",
                &[&tx_hash],
            )
            .await?;
        let existing_commitment: String = row.get(0);
        let existing_note_id: Option<String> = row.get(1);
        if existing_commitment != note_commitment || existing_note_id.as_deref() != Some(note_id) {
            anyhow::bail!("transaction {tx_hash} is already linked to a different note");
        }
        Ok(())
    }

    async fn confirm_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<bool> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        let updated = txn
            .execute(
                "UPDATE tx_note_links
                 SET handoff_state = 'submitted', prepared_expiration_block = NULL
                 WHERE tx_hash = $1 AND note_commitment = $2",
                &[&tx_hash, &note_commitment],
            )
            .await?;
        if updated == 1 {
            txn.execute(
                "UPDATE claimed_indices
                 SET claim_state = 'submitted', lease_expires_at = NULL
                 WHERE owner_tx_hash = $1 AND claim_state = 'prepared'",
                &[&tx_hash],
            )
            .await?;
        }
        txn.commit().await?;
        Ok(updated == 1)
    }

    async fn confirm_note_handoff_by_commitment(
        &self,
        note_commitment: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        let rows = txn
            .query(
                "SELECT tx_hash FROM tx_note_links
                 WHERE note_commitment = $1
                 ORDER BY created_at ASC, tx_hash ASC
                 FOR UPDATE",
                &[&note_commitment],
            )
            .await?;
        if rows.is_empty() {
            txn.commit().await?;
            return Ok(None);
        }
        txn.execute(
            "UPDATE tx_note_links
             SET handoff_state = 'submitted', prepared_expiration_block = NULL
             WHERE note_commitment = $1",
            &[&note_commitment],
        )
        .await?;
        for row in &rows {
            let tx_hash: String = row.get(0);
            txn.execute(
                "UPDATE claimed_indices
                 SET claim_state = 'submitted', lease_expires_at = NULL
                 WHERE owner_tx_hash = $1 AND claim_state = 'prepared'",
                &[&tx_hash],
            )
            .await?;
        }
        let first_tx_hash: String = rows[0].get(0);
        txn.commit().await?;
        Ok(Some(first_tx_hash))
    }

    async fn confirm_prepared_note_handoffs(&self, note_ids: &[String]) -> anyhow::Result<u64> {
        if note_ids.is_empty() {
            return Ok(0);
        }
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        let rows = txn
            .query(
                "UPDATE tx_note_links
                 SET handoff_state = 'submitted', prepared_expiration_block = NULL
                 WHERE handoff_state = 'prepared' AND note_id = ANY($1)
                 RETURNING tx_hash",
                &[&note_ids],
            )
            .await?;
        for row in &rows {
            let tx_hash: String = row.get(0);
            txn.execute(
                "UPDATE claimed_indices
                 SET claim_state = 'submitted', lease_expires_at = NULL
                 WHERE owner_tx_hash = $1 AND claim_state = 'prepared'",
                &[&tx_hash],
            )
            .await?;
        }
        txn.commit().await?;
        Ok(rows.len() as u64)
    }

    async fn clear_expired_prepared_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<bool> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        let deleted = txn
            .execute(
                "DELETE FROM tx_note_links l
                 USING service_state s
                 WHERE l.tx_hash = $1 AND l.note_commitment = $2
                   AND l.handoff_state = 'prepared'
                   AND l.prepared_expiration_block IS NOT NULL
                   AND s.id = 1 AND s.reconcile_cursor > l.prepared_expiration_block",
                &[&tx_hash, &note_commitment],
            )
            .await?;
        if deleted != 1 {
            txn.rollback().await?;
            return Ok(false);
        }
        let row = txn
            .query_opt(
                "SELECT claim_state FROM claimed_indices WHERE owner_tx_hash = $1 FOR UPDATE",
                &[&tx_hash],
            )
            .await?;
        if let Some(row) = row {
            let state: String = row.get(0);
            match state.as_str() {
                "prepared" => {
                    txn.execute(
                        "DELETE FROM claimed_indices
                         WHERE owner_tx_hash = $1 AND claim_state = 'prepared'",
                        &[&tx_hash],
                    )
                    .await?;
                }
                // A landed claim is an authoritative fence. Rolling the link
                // back would reopen attribution and permit a duplicate claim.
                "landed" => {
                    txn.rollback().await?;
                    return Ok(false);
                }
                _ => {
                    txn.rollback().await?;
                    return Ok(false);
                }
            }
        }
        txn.execute(
            "UPDATE transactions
             SET status = 'pending', error_message = NULL, block_number = 0, updated_at = now()
             WHERE tx_hash = $1 AND status = 'failed'",
            &[&tx_hash],
        )
        .await?;
        txn.commit().await?;
        Ok(true)
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

    /// Cantina #12 redesign — filter in SQL (SAFE SUPERSET) + stream ALL matches.
    ///
    /// The old query filtered ONLY by block range in SQL, applied a `LIMIT 1000`
    /// to that UNFILTERED set, and matched address/topics in Rust afterward — so
    /// a dense range with few address matches errored on rows that weren't even
    /// answers. This pushes a PROVABLE SUPERSET of `matches()` into the `WHERE`
    /// (block + address-OR-UHCV + topic0; proof lives on the `LogFilter`
    /// superset helpers), streams the WHOLE superset over a portal via
    /// `query_raw` (NOT the buffering `query()`), and runs the UNCHANGED
    /// `filter.matches()` as the exact final filter row-by-row.
    ///
    /// There is NO normal-operation row cap: stream-exhaust ⇒ we returned every
    /// match. The only limit is [`crate::store::GETLOGS_SAFETY_CEILING`] on the
    /// **post-`matches()`** count — an OOM/aggkit-rechunk backstop, so a sparse
    /// match in a dense range never errors.
    async fn get_logs(
        &self,
        filter: &LogFilter,
        current_block: u64,
    ) -> anyhow::Result<Vec<SyntheticLog>> {
        use futures_util::TryStreamExt;
        use tokio_postgres::types::ToSql;

        let client = self.pool.get().await?;
        // Keep the ORIGINAL u64s for the ceiling error message. The range params
        // need a GUARDED u64 → i64 conversion (the `block_number` column is i64):
        // a bare `as i64` WRAPS negative for values above i64::MAX (e.g. a client
        // passing toBlock ≈ u64::MAX), which turned `block_number <= $to` into an
        // always-false predicate and silently returned zero rows. See the block
        // predicate below.
        let from_block = filter.from_block_number(current_block);
        let to_block = filter.to_block_number(current_block);

        // ── Build the SAFE-SUPERSET WHERE (numbered params in push order) ──
        let mut conds: Vec<String> = Vec::new();
        // `+ Send`: this Vec is held across the `query_raw(...).await` below, so the
        // async fn's future must be `Send` (async_trait Store contract). A bare
        // `Box<dyn ToSql + Sync>` is not `Send`; `param_refs` casts back down to the
        // `&(dyn ToSql + Sync)` that `query_raw` expects.
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        // Block predicate. When block_hash is set, `matches()` keys on the hash
        // string and IGNORES the range, so we mirror it as an EXACT string
        // comparison — `encode()` yields lowercase hex, so no decode is needed
        // and malformed input compares identically to `matches()`.
        if let Some(bh) = filter.block_hash.as_ref() {
            params.push(Box::new(bh.to_lowercase()));
            conds.push(format!(
                "('0x' || encode(block_hash, 'hex')) = ${}",
                params.len()
            ));
        } else {
            // Guarded u64 → i64 (block_number is an i64 column):
            //   fromBlock > i64::MAX ⇒ the range starts ABOVE every storable
            //     block_number ⇒ no row can match ⇒ return empty (absurd range).
            //   toBlock  > i64::MAX ⇒ clamp to i64::MAX ("up to the top"); every
            //     stored block_number is ≤ i64::MAX, so this includes all of them.
            if from_block > i64::MAX as u64 {
                return Ok(Vec::new());
            }
            let from = i64::try_from(from_block).expect("checked ≤ i64::MAX above");
            let to = i64::try_from(to_block).unwrap_or(i64::MAX);
            params.push(Box::new(from));
            let p_from = params.len();
            params.push(Box::new(to));
            let p_to = params.len();
            conds.push(format!(
                "block_number >= ${p_from} AND block_number <= ${p_to}"
            ));
        }

        // Address predicate (superset incl. MA#26 passthrough): the second
        // disjunct `lower(topics[1]) = UHCV` covers ALL possible passthrough rows
        // regardless of the query's topic0 filter; `matches()` applies the exact
        // passthrough + topic0-inclusion check afterward.
        if let Some(addrs_lower) = filter.address_alternatives_lower() {
            params.push(Box::new(addrs_lower));
            let p_addrs = params.len();
            params.push(Box::new(UPDATE_HASH_CHAIN_VALUE_TOPIC.to_lowercase()));
            let p_uhcv = params.len();
            conds.push(format!(
                "(lower(address) = ANY(${p_addrs}) OR lower(topics[1]) = ${p_uhcv})"
            ));
        }

        // topic0 predicate (only when position 0 is constrained). Postgres arrays
        // are 1-indexed → topic0 = topics[1]. Positions 1..3 stay in `matches()`.
        if let Some(topic0_lower) = filter.topic0_alternatives_lower() {
            params.push(Box::new(topic0_lower));
            let p_t0 = params.len();
            conds.push(format!("lower(topics[1]) = ANY(${p_t0})"));
        }

        let sql = format!(
            "SELECT log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed
             FROM synthetic_logs
             WHERE {}
             ORDER BY block_number, log_index",
            conds.join(" AND ")
        );

        // Stream incrementally over a portal. The pooled connection is held for
        // the stream's lifetime — bounded: we stop at the ceiling or on exhaust.
        let param_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let mut stream = Box::pin(client.query_raw(&sql, param_refs).await?);

        let mut out: Vec<SyntheticLog> = Vec::new();
        while let Some(r) = stream.try_next().await? {
            let bh = bytes_to_array_32(r.get(5));
            let topics: Vec<String> = r.get(2);
            let log = SyntheticLog {
                log_index: r.get::<_, i64>(0) as u64,
                address: r.get(1),
                topics,
                data: r.get(3),
                block_number: r.get::<_, i64>(4) as u64,
                block_hash: bh,
                transaction_hash: r.get(6),
                transaction_index: r.get::<_, i64>(7) as u64,
                removed: r.get(8),
            };
            if filter.matches(&log, current_block) {
                out.push(log);
                // OOM backstop only — NOT a normal cap. Exhausting the stream
                // without tripping this means we returned ALL matches.
                if out.len() > crate::store::GETLOGS_SAFETY_CEILING {
                    return Err(crate::store::getlogs_row_cap_error(from_block, to_block));
                }
            }
        }

        Ok(out)
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

    /// Atomic GER commit (audit H2). Single postgres txn folding the
    /// idempotent chain roll + log emission with `is_injected = TRUE`, so a
    /// crash can never leave the chain rolled without the injected flag set
    /// (which would cause a duplicate roll on retry).
    #[allow(clippy::too_many_arguments)]
    async fn commit_ger_event_atomic(
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
        let tx_hash_key = tx_hash.to_lowercase();

        // Exact note observation confirms the handoff in the same transaction
        // as the event and receipt. This runs even on an idempotent replay.
        txn.execute(
            "UPDATE tx_note_links
             SET handoff_state = 'submitted', prepared_expiration_block = NULL
             WHERE lower(tx_hash) = $1",
            &[&tx_hash_key],
        )
        .await?;

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

        // Idempotent chain roll + log emission (H2): skip if already emitted.
        // Canonicalize tx_hash to lowercase to match the store's convention
        // (get_logs_for_tx / memory.rs) so a mixed-case retry still matches the
        // stored lowercase row instead of double-emitting.
        let already_emitted = txn
            .query_opt(
                "SELECT 1 FROM synthetic_logs WHERE lower(transaction_hash) = $1 LIMIT 1",
                &[&tx_hash_key],
            )
            .await?
            .is_some();
        if !already_emitted {
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
                    &tx_hash_key,
                    &0_i64,
                    &false,
                ],
            )
            .await?;
        }

        // Always set is_injected = TRUE (idempotent UPSERT).
        txn.execute(
            "INSERT INTO ger_entries (ger_hash, block_number, timestamp, is_injected)
             VALUES ($1, $2, $3, TRUE)
             ON CONFLICT (ger_hash) DO UPDATE SET is_injected = TRUE",
            &[
                &global_exit_root.as_slice(),
                &(block_number as i64),
                &(timestamp as i64),
            ],
        )
        .await?;

        // A real linked GER hash has a pending transaction row. Finalise it in
        // the same commit as the injected flag/log so a crash cannot expose the
        // GER event while its receipt remains null. Derived hashes have no row.
        txn.execute(
            "UPDATE transactions SET status = 'success', error_message = NULL,
                    block_number = $1, updated_at = now()
             WHERE lower(tx_hash) = $2",
            &[&(block_number as i64), &tx_hash_key],
        )
        .await?;

        txn.commit().await?;
        Ok(())
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

    async fn txn_begin_if_absent(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<bool> {
        let mut client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");
        let miden_id = entry.id.map(|id| id.to_hex());
        let signer_str = format!("{:#x}", entry.signer);
        let mut envelope_bytes = Vec::new();
        entry.envelope.encode_2718(&mut envelope_bytes);
        let tx = client.transaction().await?;
        let inserted = tx.execute(
            "INSERT INTO transactions (tx_hash, miden_tx_id, envelope_bytes, signer, expires_at, status, block_number)
             VALUES ($1, $2, $3, $4, $5, 'pending', 0) ON CONFLICT (tx_hash) DO NOTHING",
            &[
                &hash_str,
                &miden_id as &(dyn ToSql + Sync),
                &envelope_bytes,
                &signer_str,
                &entry.expires_at.map(|v| v as i64) as &(dyn ToSql + Sync),
            ],
        ).await? == 1;
        let pending = if !inserted {
            tx.execute(
                "UPDATE transactions SET
                    miden_tx_id = COALESCE($2, miden_tx_id),
                    expires_at = COALESCE($3, expires_at), updated_at = now()
                 WHERE tx_hash = $1 AND status = 'pending'",
                &[
                    &hash_str,
                    &miden_id as &(dyn ToSql + Sync),
                    &entry.expires_at.map(|v| v as i64) as &(dyn ToSql + Sync),
                ],
            )
            .await?
                == 1
        } else {
            true
        };
        if pending && !entry.logs.is_empty() {
            tx.execute(
                "DELETE FROM transaction_logs WHERE tx_hash = $1",
                &[&hash_str],
            )
            .await?;
            for log_data in &entry.logs {
                let topics_bytes: Vec<Vec<u8>> = log_data
                    .topics()
                    .iter()
                    .map(|t| t.as_slice().to_vec())
                    .collect();
                tx.execute(
                    "INSERT INTO transaction_logs (tx_hash, topics, data) VALUES ($1, $2, $3)",
                    &[&hash_str, &topics_bytes, &log_data.data.as_ref()],
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(inserted)
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

        let updated = tx
            .execute(
                "UPDATE transactions
                 SET status = $1, error_message = $2, block_number = $3, updated_at = now()
                 WHERE tx_hash = $4
                   AND ($1 <> 'failed' OR NOT EXISTS (
                       SELECT 1 FROM tx_note_links WHERE tx_note_links.tx_hash = $4
                   ))",
                &[
                    &status,
                    &error_msg as &(dyn ToSql + Sync),
                    &(block_num as i64),
                    &hash_str,
                ],
            )
            .await?;

        // PR #127 review — finalising a transaction that has no `txn_begin`
        // row must be an ERROR, matching `InMemoryStore::txn_commit`
        // ("transaction {tx_hash} not found"). Pre-fix this UPDATE silently
        // affected zero rows and the method still committed the synthetic
        // logs below and returned Ok — so a projector racing a submitter
        // could "finalise" a receipt that was never durably begun, and the
        // late `txn_begin` would then park the real receipt as pending
        // forever. Every caller either creates the row first or explicitly
        // tolerates this error (`project_ger_note` / `project_claim_note`
        // use `let _ = ... inspect_err`, the pending/expiry sweeps log and
        // continue), so erroring here is safe and makes the two stores
        // behave identically. Bailing before `tx.commit()` rolls the whole
        // transaction back — no partial log/counter writes escape.
        if updated == 0 && result.is_err() {
            let protected = tx
                .query_opt(
                    "SELECT 1 FROM transactions t
                     JOIN tx_note_links l ON l.tx_hash = t.tx_hash
                     WHERE t.tx_hash = $1",
                    &[&hash_str],
                )
                .await?
                .is_some();
            if protected {
                tx.commit().await?;
                return Ok(());
            }
        }
        if updated == 0 {
            anyhow::bail!("PgStore: transaction {tx_hash} not found");
        }

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

    async fn txn_commit_confirmed_duplicate(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
    ) -> anyhow::Result<()> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let hash = format!("{tx_hash:#x}");
        let (status, error_message) = match &result {
            Ok(()) => ("success", None),
            Err(message) => ("failed", Some(message.as_str())),
        };
        let updated = tx
            .execute(
                "UPDATE transactions SET status = $1, error_message = $2, block_number = $3, updated_at = now() WHERE tx_hash = $4 AND status = $5",
                &[
                    &status,
                    &error_message as &(dyn ToSql + Sync),
                    &(block_num as i64),
                    &hash,
                    &"pending",
                ],
            )
            .await?;
        if updated == 1 {
            tx.execute("DELETE FROM transaction_logs WHERE tx_hash = $1", &[&hash])
                .await?;
        } else if tx
            .query_opt("SELECT 1 FROM transactions WHERE tx_hash = $1", &[&hash])
            .await?
            .is_none()
        {
            anyhow::bail!("PgStore: transaction {tx_hash} not found");
        }
        tx.commit().await?;
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

    async fn pending_nonce_frontier(&self, addr: &str) -> anyhow::Result<PendingNonceFrontier> {
        use alloy::eips::Decodable2718;

        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT t.envelope_bytes,
                        (l.tx_hash IS NULL OR l.handoff_state <> 'submitted') AS unlinked
                 FROM transactions t
                 LEFT JOIN tx_note_links l ON l.tx_hash = t.tx_hash
                 WHERE lower(t.signer) = lower($1) AND t.status = 'pending'",
                &[&addr],
            )
            .await?;
        let mut frontier = PendingNonceFrontier::default();
        for row in rows {
            let bytes: &[u8] = row.get(0);
            let envelope = TxEnvelope::decode_2718(&mut &bytes[..]).map_err(|err| {
                anyhow::anyhow!(
                    "pending transaction for signer {addr} has an undecodable envelope: {err}"
                )
            })?;
            let nonce = super::envelope_nonce(&envelope);
            frontier.lowest_pending = Some(
                frontier
                    .lowest_pending
                    .map_or(nonce, |current| current.min(nonce)),
            );
            if row.get::<_, bool>(1) {
                frontier.lowest_unlinked = Some(
                    frontier
                        .lowest_unlinked
                        .map_or(nonce, |current| current.min(nonce)),
                );
            }
        }
        Ok(frontier)
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

    async fn nonce_advance_cas(&self, addr: &str, expected: u64) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let key = addr.to_lowercase();
        // BLOCKER D — atomic conditional advance. A fresh address (no row) is
        // nonce 0: for `expected == 0` create/advance to 1 only while the current
        // value is still 0; otherwise advance only WHERE the stored nonce equals
        // `expected`. Postgres row-locking on the conflict/UPDATE serialises
        // concurrent replicas, so exactly one wins the CAS.
        let n = if expected == 0 {
            client
                .execute(
                    "INSERT INTO nonces (address, nonce) VALUES ($1, 1)
                     ON CONFLICT (address) DO UPDATE SET nonce = 1 WHERE nonces.nonce = 0",
                    &[&key],
                )
                .await?
        } else {
            client
                .execute(
                    "UPDATE nonces SET nonce = nonce + 1 WHERE address = $1 AND nonce = $2",
                    &[&key, &(expected as i64)],
                )
                .await?
        };
        Ok(n == 1)
    }

    async fn reserve_nonce(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<crate::store::NonceReservation> {
        use crate::store::NonceReservation;
        let mut client = self.pool.get().await?;
        let key = addr.to_lowercase();
        let hash_str = format!("{tx_hash:#x}");
        let lease_secs = lease.as_secs().max(1) as f64;

        // ONE transaction: lock the slot row (FOR UPDATE), then decide + write.
        // Serialises concurrent replicas on the (signer, nonce) row so exactly one
        // is ever told `Won`.
        let tx = client.transaction().await?;
        let existing = tx
            .query_opt(
                "SELECT tx_hash, state, (lease_expires_at <= now()) AS expired, fence_token
                 FROM nonce_reservations WHERE signer = $1 AND nonce = $2 FOR UPDATE",
                &[&key, &(nonce as i64)],
            )
            .await?;
        let outcome = match existing {
            None => {
                tx.execute(
                    "INSERT INTO nonce_reservations (signer, nonce, tx_hash, state, lease_expires_at, fence_token)
                     VALUES ($1, $2, $3, 'executing', now() + ($4 || ' seconds')::interval, 1)",
                    &[&key, &(nonce as i64), &hash_str, &lease_secs.to_string()],
                )
                .await?;
                NonceReservation::Won { fence: 1 }
            }
            Some(row) => {
                let row_hash: String = row.get(0);
                let state: String = row.get(1);
                let expired: bool = row.get(2);
                let fence: i64 = row.get(3);
                // A nonce slot is permanently bound to its first transaction hash.
                // Expiry only permits recovery by that exact signed transaction; a
                // replacement is unsafe because the prior external outcome may be ambiguous.
                let same_tx = row_hash.eq_ignore_ascii_case(&hash_str);
                let takeover = same_tx
                    && (state == "released_failure" || state == "released_success" || expired);
                if takeover {
                    let new_fence = fence + 1;
                    tx.execute(
                        "UPDATE nonce_reservations SET tx_hash = $5, state = 'executing',
                         lease_expires_at = now() + ($3 || ' seconds')::interval,
                         fence_token = $4
                         WHERE signer = $1 AND nonce = $2",
                        &[
                            &key,
                            &(nonce as i64),
                            &lease_secs.to_string(),
                            &new_fence,
                            &hash_str,
                        ],
                    )
                    .await?;
                    NonceReservation::Won {
                        fence: new_fence as u64,
                    }
                } else if row_hash.eq_ignore_ascii_case(&hash_str) {
                    NonceReservation::OwnedBySame
                } else {
                    // NIT — propagate a parse error WITH context instead of
                    // substituting the zero hash.
                    let other =
                        <TxHash as std::str::FromStr>::from_str(&row_hash).map_err(|e| {
                            anyhow::anyhow!(
                                "nonce_reservations row for signer {key} nonce {nonce} has an \
                             unparsable tx_hash {row_hash:?}: {e}"
                            )
                        })?;
                    NonceReservation::HeldByOther(other)
                }
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    async fn renew_reservation(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let key = addr.to_lowercase();
        let hash_str = format!("{tx_hash:#x}");
        let lease_secs = lease.as_secs().max(1) as f64;
        let updated = client
            .execute(
                "UPDATE nonce_reservations
             SET lease_expires_at = now() + ($4 || ' seconds')::interval
             WHERE signer = $1 AND nonce = $2 AND tx_hash = $3
               AND state <> 'released_failure'",
                &[&key, &(nonce as i64), &hash_str, &lease_secs.to_string()],
            )
            .await?;
        Ok(updated == 1)
    }

    async fn release_reservation(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        fence: u64,
        success: bool,
    ) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = addr.to_lowercase();
        let hash_str = format!("{tx_hash:#x}");
        let new_state = if success {
            "released_success"
        } else {
            "released_failure"
        };
        // FENCED: only the current fence owner still in `executing` may release.
        client
            .execute(
                "UPDATE nonce_reservations SET state = $5,
                    lease_expires_at = CASE WHEN $5 = 'released_failure' THEN now() ELSE lease_expires_at END
                 WHERE signer = $1 AND nonce = $2 AND tx_hash = $3 AND fence_token = $4
                   AND state = 'executing'",
                &[
                    &key,
                    &(nonce as i64),
                    &hash_str,
                    &(fence as i64),
                    &new_state,
                ],
            )
            .await?;
        Ok(())
    }

    async fn commit_reverted_receipt_and_advance_nonce(
        &self,
        tx_hash: TxHash,
        entry: TxnEntry,
        reason: String,
        block_num: u64,
        _block_hash: [u8; 32],
        addr: &str,
        expected_nonce: u64,
    ) -> anyhow::Result<bool> {
        let mut client = self.pool.get().await?;
        let hash_str = format!("{tx_hash:#x}");
        let miden_id = entry.id.map(|id| id.to_hex());
        let signer_str = format!("{:#x}", entry.signer);
        let mut envelope_bytes = Vec::new();
        entry.envelope.encode_2718(&mut envelope_bytes);
        let key = addr.to_lowercase();

        // BLOCKER C — receipt + nonce in ONE transaction. The tx row is inserted
        // already committed-`failed` (status 0x0, no attached logs → no ClaimEvent),
        // so there is no `txn_begin`→`txn_commit` pending window a crash could freeze
        // forever.
        //
        // BLOCKER 4 — CONDITIONAL upsert: `ON CONFLICT ... WHERE transactions.status
        // = 'failed'` so a REAL receipt is NEVER overwritten to status 0. If a
        // cross-replica path already materialised the real claim under this hash
        // (status 'pending' → awaiting the projector, or 'success' → landed), the
        // conflicting UPDATE's WHERE fails → the real receipt survives and both
        // replicas converge to the real outcome. Absent → insert; already 'failed' →
        // idempotently re-affirm.
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO transactions (tx_hash, miden_tx_id, envelope_bytes, signer, expires_at, status, error_message, block_number)
             VALUES ($1, $2, $3, $4, $5, 'failed', $6, $7)
             ON CONFLICT (tx_hash) DO UPDATE SET status = 'failed', error_message = $6, block_number = $7, updated_at = now()
             WHERE transactions.status = 'failed' OR
               (transactions.status = 'pending' AND NOT EXISTS (
                   SELECT 1 FROM tx_note_links
                   WHERE tx_note_links.tx_hash = transactions.tx_hash
               ))",
            &[
                &hash_str,
                &miden_id as &(dyn ToSql + Sync),
                &envelope_bytes,
                &signer_str,
                &entry.expires_at.map(|v| v as i64) as &(dyn ToSql + Sync),
                &reason,
                &(block_num as i64),
            ],
        )
        .await?;
        let n = if expected_nonce == 0 {
            tx.execute(
                "INSERT INTO nonces (address, nonce) VALUES ($1, 1)
                 ON CONFLICT (address) DO UPDATE SET nonce = 1 WHERE nonces.nonce = 0",
                &[&key],
            )
            .await?
        } else {
            tx.execute(
                "UPDATE nonces SET nonce = nonce + 1 WHERE address = $1 AND nonce = $2",
                &[&key, &(expected_nonce as i64)],
            )
            .await?
        };
        tx.commit().await?;
        Ok(n == 1)
    }

    // ── Claims ───────────────────────────────────────────────────

    async fn try_claim_fenced(
        &self,
        global_index: U256,
        owner_tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<ClaimFence>> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let owner = format!("{owner_tx_hash:#x}");
        let lease_secs = lease.as_secs_f64();
        let inserted = client
            .execute(
                "INSERT INTO claimed_indices
                (global_index, owner_tx_hash, fence_token, claim_state, lease_expires_at)
             VALUES ($1, $2, 1, 'executing', now() + ($3 || ' seconds')::interval)
             ON CONFLICT (global_index) DO NOTHING",
                &[&key, &owner, &lease_secs.to_string()],
            )
            .await?;
        Ok((inserted == 1).then_some(ClaimFence { fence: 1 }))
    }

    async fn try_reclaim_claim_fenced(
        &self,
        global_index: U256,
        owner_tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<ClaimFence>> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let owner = format!("{owner_tx_hash:#x}");
        let lease_secs = lease.as_secs_f64();
        let row = client.query_opt(
            "UPDATE claimed_indices SET owner_tx_hash = $2, fence_token = fence_token + 1,
                created_at = now(), lease_expires_at = now() + ($3 || ' seconds')::interval
             WHERE global_index = $1 AND claim_state = 'executing'
               AND (owner_tx_hash = $2 OR lease_expires_at <= now() OR
                    (lease_expires_at IS NULL AND created_at <= now() - ($3 || ' seconds')::interval))
             RETURNING fence_token",
            &[&key, &owner, &lease_secs.to_string()],
        ).await?;
        Ok(row.map(|row| ClaimFence {
            fence: row.get::<_, i64>(0) as u64,
        }))
    }

    async fn prepare_claim_submission_fenced(
        &self,
        global_index: U256,
        owner_tx_hash: TxHash,
        fence: u64,
        tx_hash: TxHash,
        note_commitment: &str,
        note_id: &str,
        expiration_block: u64,
    ) -> anyhow::Result<bool> {
        let mut client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let owner = format!("{owner_tx_hash:#x}");
        let tx_hash = format!("{tx_hash:#x}");
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO tx_note_links
                (tx_hash, note_commitment, note_id, handoff_state, prepared_expiration_block)
             VALUES ($1, $2, $3, 'prepared', $4)
             ON CONFLICT (tx_hash) DO NOTHING",
            &[
                &tx_hash,
                &note_commitment,
                &note_id,
                &(expiration_block as i64),
            ],
        )
        .await?;
        let row = tx
            .query_one(
                "SELECT note_commitment, note_id FROM tx_note_links WHERE tx_hash = $1",
                &[&tx_hash],
            )
            .await?;
        let existing: String = row.get(0);
        let existing_note_id: Option<String> = row.get(1);
        if existing != note_commitment || existing_note_id.as_deref() != Some(note_id) {
            anyhow::bail!("transaction {tx_hash} is already linked to a different claim note");
        }
        let updated = tx
            .execute(
                "UPDATE claimed_indices SET claim_state = 'prepared', lease_expires_at = NULL
                 WHERE global_index = $1 AND owner_tx_hash = $2 AND fence_token = $3
                   AND claim_state = 'executing' AND lease_expires_at > now()",
                &[&key, &owner, &(fence as i64)],
            )
            .await?;
        if updated != 1 {
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn unclaim_fenced(
        &self,
        global_index: &U256,
        owner_tx_hash: TxHash,
        fence: u64,
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let owner = format!("{owner_tx_hash:#x}");
        let deleted = client
            .execute(
                "DELETE FROM claimed_indices WHERE global_index = $1
             AND owner_tx_hash = $2 AND fence_token = $3 AND claim_state = 'executing'",
                &[&key, &owner, &(fence as i64)],
            )
            .await?;
        Ok(deleted == 1)
    }

    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        let result = client
            .execute(
                "INSERT INTO claimed_indices (global_index, claim_state) VALUES ($1, 'executing')",
                &[&key],
            )
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(_) => anyhow::bail!("claim already submitted for global_index {global_index}"),
        }
    }

    async fn try_reclaim_expired(
        &self,
        global_index: U256,
        ttl: std::time::Duration,
    ) -> anyhow::Result<bool> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        // Single UPDATE = atomic check-and-refresh (row lock): only a record older than
        // `ttl` is superseded, and exactly one concurrent recovery wins — the loser's
        // WHERE clause sees the refreshed created_at and matches nothing.
        let updated = client
            .execute(
                "UPDATE claimed_indices SET created_at = now()
                 WHERE global_index = $1
                   AND owner_tx_hash IS NULL AND claim_state = 'executing'
                   AND created_at <= now() - make_interval(secs => $2)",
                &[&key, &ttl.as_secs_f64()],
            )
            .await?;
        Ok(updated > 0)
    }

    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let key = format!("{global_index:#x}");
        client
            .execute(
                "DELETE FROM claimed_indices WHERE global_index = $1 AND owner_tx_hash IS NULL",
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
            "metadata_too_large" => UnbridgeableBridgeOutReason::MetadataTooLarge,
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

    /// Atomic, idempotent B2AGG commit (audit H1/H3). Single postgres txn:
    ///   1. reuse-or-allocate `deposit_count` (no counter bump on retry)
    ///   2. allocate `log_index` + INSERT the synthetic BridgeEvent (skipped if
    ///      a log with this deterministic tx_hash already exists)
    /// A crash at any point rolls the whole txn back, so the note can never be
    /// left marked-processed without a matching BridgeEvent.
    ///
    /// SINGLE-WRITER SERIAL INVARIANT — why the `SELECT 1 FROM synthetic_logs`
    /// read-then-INSERT in step 2 (and the analogous read-then-INSERT in step 1)
    /// is NOT a reachable TOCTOU race, and why no row lock / UNIQUE constraint /
    /// `ON CONFLICT` is required:
    ///
    /// This method is called ONLY from the projector path, which is strictly
    /// serial. The projector `tick()` borrows `&mut MidenClientLib` — one
    /// non-reentrant client instance — and drives the commit loop one block at a
    /// time, writing before advancing the cursor:
    ///     while cursor < tip { project_block_notes(next).await?; set_projector_cursor(next).await? }
    /// There is exactly one in-flight `commit_b2agg_event_atomic` at any moment
    /// for a given store, so no concurrent writer can slip an insert between this
    /// transaction's SELECT and its INSERT. The `RECONCILE_CONCURRENCY` fan-out
    /// is FETCH-only (parallel `sync_note_ids`), never the commit — it never
    /// touches `service_state`, `bridge_out_processed`, or `synthetic_logs`.
    /// A Copilot reviewer flagged the read/insert gap as a TOCTOU; it reads as
    /// intentional under this single-writer serial invariant.
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

        // 1. Reuse-or-allocate deposit_count. Idempotent: a retry after a
        //    committed txn finds the existing row and reuses its count, so the
        //    counter never advances twice for one note (no gap — H3).
        let deposit_count: i32 = if let Some(row) = txn
            .query_opt(
                "SELECT deposit_count FROM bridge_out_processed WHERE note_id = $1",
                &[&note_id],
            )
            .await?
        {
            row.get(0)
        } else {
            let row = txn
                .query_one(
                    "UPDATE service_state
                     SET deposit_counter = deposit_counter + 1, updated_at = now()
                     WHERE id = 1
                     RETURNING deposit_counter - 1",
                    &[],
                )
                .await?;
            let dc: i32 = row.get(0);
            txn.execute(
                "INSERT INTO bridge_out_processed (note_id, deposit_count) VALUES ($1, $2)",
                &[&note_id, &dc],
            )
            .await?;
            dc
        };

        // 2. Idempotent log emission. tx_hash is derived deterministically from
        //    note_id, so a retry produces the same tx_hash — skip the insert if
        //    a row already exists for it (no duplicate BridgeEvent, no gap in
        //    log_index).
        let already_emitted = txn
            .query_opt(
                "SELECT 1 FROM synthetic_logs WHERE transaction_hash = $1 LIMIT 1",
                &[&tx_hash],
            )
            .await?
            .is_some();
        if !already_emitted {
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

            let data = crate::bridge_out::encode_bridge_event_data(
                leaf_type,
                origin_network,
                origin_address,
                destination_network,
                destination_address,
                amount,
                metadata,
                deposit_count as u32,
            );
            let topics_owned: [String; 1] = [crate::log_synthesis::BRIDGE_EVENT_TOPIC.to_string()];
            let topics: Vec<&str> = topics_owned.iter().map(|s| s.as_str()).collect();
            txn.execute(
                "INSERT INTO synthetic_logs
                    (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
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
        }

        txn.commit().await?;
        Ok(deposit_count as u32)
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

    /// Atomic commit for a watcher-synthesised ClaimEvent and its linked receipt.
    /// The block tip is sealed by `SyntheticProjector` after the whole block, not
    /// by an individual note projection.
    #[allow(clippy::too_many_arguments)]
    async fn commit_manual_claim_event_atomic(
        &self,
        note_id: String,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_index: [u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) -> anyhow::Result<()> {
        let mut client = self.pool.get().await?;
        let txn = client.transaction().await?;
        let tx_hash_key = tx_hash.to_lowercase();

        // Link -> claim is the global handoff lock order. Observation is
        // terminal even on replay, and fences out a publisher that raced the
        // projector after its final landed-state read.
        txn.execute(
            "UPDATE tx_note_links
             SET handoff_state = 'submitted', prepared_expiration_block = NULL
             WHERE lower(tx_hash) = $1",
            &[&tx_hash_key],
        )
        .await?;
        let global_index_key = format!("{:#x}", U256::from_be_bytes(global_index));
        txn.execute(
            "INSERT INTO claimed_indices
                 (global_index, owner_tx_hash, fence_token, claim_state, lease_expires_at)
             VALUES ($1, NULL, 1, 'landed', NULL)
             ON CONFLICT (global_index) DO UPDATE
             SET claim_state = 'landed', lease_expires_at = NULL,
                 fence_token = claimed_indices.fence_token + 1",
            &[&global_index_key],
        )
        .await?;

        // 1. Mark the note processed. Exactly one concurrent projector emits the
        // log; a retry still repairs the linked receipt below.
        let inserted = txn
            .execute(
                "INSERT INTO claim_watcher_processed (note_id, global_index, block_number)
             VALUES ($1, $2, $3)
             ON CONFLICT (note_id) DO NOTHING",
                &[&note_id, &global_index.as_slice(), &(block_number as i64)],
            )
            .await?
            == 1;

        if inserted {
            let row = txn
                .query_one(
                    "UPDATE service_state SET log_counter = log_counter + 1, updated_at = now() WHERE id = 1 RETURNING log_counter - 1",
                    &[],
                )
                .await?;
            let log_index: i64 = row.get(0);
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
                    &tx_hash_key,
                    &0_i64,
                    &false,
                ],
            )
            .await?;
        }

        // A real linked hash has a pending `transactions` row; a derived hash
        // does not, so this idempotent update naturally becomes a no-op. Keeping
        // it in this transaction prevents a visible ClaimEvent/null-receipt gap.
        txn.execute(
            "UPDATE transactions SET status = 'success', error_message = NULL,
                    block_number = $1, updated_at = now()
             WHERE lower(tx_hash) = lower($2)",
            &[&(block_number as i64), &tx_hash_key],
        )
        .await?;

        txn.commit().await?;
        Ok(())
    }

    // ── Faucet registry ──────────────────────────────────────────

    async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()> {
        let client = self.pool.get().await?;
        let faucet_id = entry.faucet_id.to_hex();
        // Finding #10 — converge on the (origin_address, origin_network) unique
        // key (`idx_faucet_origin`), not only on the faucet_id primary key. A
        // second first-claim worker that raced past the local miss and deployed
        // its own faucet must NOT error into the split state where the local
        // registry keeps faucet A while the bridge routes by faucet B; instead
        // it converges to the route already persisted.
        //
        // The `WHERE faucet_registry.faucet_id = EXCLUDED.faucet_id` guard keeps
        // the historical faucet_id-idempotent metadata refresh (same faucet
        // re-registering updates symbol/decimals) while making a *different*
        // faucet for the same origin a no-op (first-write wins). There is no
        // live route-swap API; the only way to repoint an origin at a different
        // faucet is out-of-band DR/repair tooling operating directly on the row.
        client
            .execute(
                "INSERT INTO faucet_registry (faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale, metadata)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (origin_address, origin_network) DO UPDATE
                 SET symbol = EXCLUDED.symbol,
                     origin_decimals = EXCLUDED.origin_decimals,
                     miden_decimals = EXCLUDED.miden_decimals,
                     scale = EXCLUDED.scale,
                     -- Cantina #13 — never clobber stored metadata with empty. A
                     -- blank re-register (metadata = vec![]) must not wipe good
                     -- metadata persisted by an earlier non-empty registration or
                     -- the Layer-2 backfill (which always writes non-empty, so it
                     -- still updates here).
                     metadata = CASE
                         WHEN EXCLUDED.metadata = ''::bytea THEN faucet_registry.metadata
                         ELSE EXCLUDED.metadata
                     END
                 WHERE faucet_registry.faucet_id = EXCLUDED.faucet_id",
                &[
                    &faucet_id,
                    &entry.origin_address.as_slice(),
                    &(entry.origin_network as i32),
                    &entry.symbol,
                    &(entry.origin_decimals as i16),
                    &(entry.miden_decimals as i16),
                    &(entry.scale as i16),
                    &entry.metadata.as_slice(),
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
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale, metadata
                 FROM faucet_registry
                 WHERE origin_address = $1 AND origin_network = $2",
                &[&origin_address.as_slice(), &(origin_network as i32)],
            )
            .await?;

        Ok(rows.first().and_then(pg_row_to_faucet_entry))
    }

    async fn get_faucet_by_id(&self, faucet_id: AccountId) -> anyhow::Result<Option<FaucetEntry>> {
        let client = self.pool.get().await?;
        let id_str = faucet_id.to_hex();
        let rows = client
            .query(
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale, metadata
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
                "SELECT faucet_id, origin_address, origin_network, symbol, origin_decimals, miden_decimals, scale, metadata
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
        metadata: {
            let m: &[u8] = row.get(7);
            m.to_vec()
        },
    })
}
