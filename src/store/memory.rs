//! In-memory Store implementation — wraps HashMap/RwLock data structures.

use super::{
    ClaimFence, FaucetEntry, NoteHandoff, NoteHandoffState, PendingNonceFrontier, Store, TxnData,
    TxnEntry, UnbridgeableBridgeOut, UnclaimableClaim,
};
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

/// #55 BLOCKER 1 — in-memory fenced admission-lease reservation row.
#[derive(Clone)]
struct Reservation {
    tx_hash: TxHash,
    state: ReservationState,
    lease_expires_at: std::time::Instant,
    fence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReservationState {
    Executing,
    ReleasedSuccess,
    ReleasedFailure,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimState {
    Executing,
    Prepared,
    Submitted,
    Landed,
}

#[derive(Clone)]
struct ClaimRecord {
    owner_tx_hash: Option<TxHash>,
    state: ClaimState,
    acquired_at: std::time::Instant,
    lease_expires_at: Option<std::time::Instant>,
    fence: u64,
}

#[derive(Clone)]
struct NoteHandoffRecord {
    note_commitment: String,
    note_id: Option<String>,
    state: NoteHandoffState,
    expiration_block: Option<u64>,
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

    #[cfg(test)]
    test_fail_next_ger_evidence_write: std::sync::atomic::AtomicBool,

    // Transactions
    transactions: Mutex<LruCache<TxHash, TxnReceipt>>,

    // Nonces
    nonces: RwLock<HashMap<String, u64>>,

    // #55 BLOCKER 1 — (signer, nonce) → fenced admission-lease reservations.
    nonce_reservations: RwLock<HashMap<(String, u64), Reservation>>,

    // #55 BLOCKER B test hook: when true, the NEXT `nonce_advance_cas` returns a
    // store error (simulating a DB failure on the CAS). Always false in production.
    test_fail_next_nonce_cas: std::sync::atomic::AtomicBool,

    // Claims — value = when `try_claim` acquired the lock (as read from
    // `claim_clock_now`), so orphaned records (crash between the lock write and
    // the CLAIM landing) can be superseded after a TTL (`try_reclaim_expired`,
    // SOAK FINDING #1).
    claimed: RwLock<HashMap<U256, ClaimRecord>>,

    // Test-only skew for the claim-lock clock. `test_backdate_claim` ages
    // records by moving this clock FORWARD instead of subtracting from the
    // stored `Instant`s — `Instant::now() - age` underflows (panics) whenever
    // the process has been alive for less than `age` (short test binaries, or
    // a raised CLAIM_RESUBMIT_TTL_SECS). See `claim_clock_now`.
    #[cfg(test)]
    claim_clock_skew: RwLock<std::time::Duration>,

    // Unclaimable claims — first-write wins per global_index (RD-860).
    unclaimable: RwLock<HashMap<U256, UnclaimableClaim>>,

    // Unbridgeable bridge-outs — first-write wins per note_id (Cantina MA#18).
    unbridgeable_bridge_outs: RwLock<HashMap<String, UnbridgeableBridgeOut>>,

    // Address mappings
    address_mappings: RwLock<HashMap<Address, AccountId>>,

    // Bridge-out
    processed_notes: RwLock<HashMap<String, u32>>,
    deposit_counter: RwLock<u32>,

    // Claim watcher (independent from bridge-out so CLAIM observations do not
    // consume B2AGG `deposit_counter` slots — see commit_manual_claim_event_atomic).
    claim_watcher_processed: RwLock<HashMap<String, [u8; 32]>>,

    /// Test hook (#55 BLOCKER B): when set to `Some(gi)`, the next
    /// `has_claim_event_for_global_index(gi)` call that would report NO event
    /// instead LANDS the claim (records a watcher ClaimEvent) as a side effect and
    /// still reports the miss — so the FOLLOWING call observes it. Deterministically
    /// models "the racing claim commits its ClaimEvent between `acquire_claim_lock`'s
    /// two landed reads."
    #[cfg(test)]
    test_land_after_next_has_claim_miss: RwLock<Option<[u8; 32]>>,

    // Faucet registry
    faucets: RwLock<Vec<FaucetEntry>>,

    // Monitor trackers (RD-913) — in-memory mirror of monitor_burn_serials,
    // monitor_twin_notes, monitor_expected_mints. With InMemoryStore the
    // mirror IS the source of truth; with PgStore the DB is and these
    // structures live inside the tracker's LRU cache instead.
    monitor_burn_serials: RwLock<HashSet<[u8; 32]>>,
    monitor_twin_notes: RwLock<HashMap<[u8; 32], Vec<[u8; 32]>>>,
    monitor_expected_mints: RwLock<HashMap<[u8; 32], MonitorExpectedMintRow>>,

    // Synthetic projector cursor (synthetic-indexer redesign, Phase 2a) —
    // last fully-projected Miden block height. Field-backed mirror of the
    // PgStore `service_state.projector_cursor` column. See
    // Store::get_projector_cursor / docs/SYNTHETIC-INDEXER-REDESIGN.md.
    projector_cursor: RwLock<u64>,

    // Note-reconciler sweep cursor — last Miden block fully swept by the
    // note-visibility reconciler. Field-backed mirror of the PgStore
    // `service_state.reconcile_cursor` column (migration 010). See
    // Store::get_reconcile_cursor.
    reconcile_cursor: RwLock<u64>,

    // Cursor of the one configured L1 evidence scan. PostgreSQL stores this in
    // the legacy `finalized_scan_cursor` column for upgrade-safe provenance.
    l1_evidence_cursor: RwLock<u64>,

    // Canonical EvidenceTag that produced the persisted selected-scan state.
    l1_evidence_policy: RwLock<Option<String>>,

    // Receipts map (synthetic-indexer redesign, Phase 2b substrate) —
    // first-write-wins evm_tx_hash -> note_commitment, with the reverse index
    // mirrored alongside it. UNUSED in Phase 2a. See Store::record_tx_note_link.
    tx_note_links: RwLock<HashMap<String, NoteHandoffRecord>>,
    note_tx_links: RwLock<HashMap<String, String>>,
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
            logs_by_block: RwLock::new(HashMap::new()),
            logs_by_tx: RwLock::new(HashMap::new()),
            log_counter: RwLock::new(0),
            pending_events: RwLock::new(Vec::new()),
            seen_gers: RwLock::new(HashMap::new()),
            latest_ger: RwLock::new(None),
            hash_chain_value: RwLock::new([0u8; 32]),
            injected_gers: RwLock::new(HashSet::new()),
            #[cfg(test)]
            test_fail_next_ger_evidence_write: std::sync::atomic::AtomicBool::new(false),
            transactions: Mutex::new(LruCache::new(NonZeroUsize::new(10_000).unwrap())),
            nonces: RwLock::new(HashMap::new()),
            nonce_reservations: RwLock::new(HashMap::new()),
            test_fail_next_nonce_cas: std::sync::atomic::AtomicBool::new(false),
            claimed: RwLock::new(HashMap::new()),
            #[cfg(test)]
            claim_clock_skew: RwLock::new(std::time::Duration::ZERO),
            unclaimable: RwLock::new(HashMap::new()),
            unbridgeable_bridge_outs: RwLock::new(HashMap::new()),
            address_mappings: RwLock::new(HashMap::new()),
            processed_notes: RwLock::new(HashMap::new()),
            deposit_counter: RwLock::new(0),
            claim_watcher_processed: RwLock::new(HashMap::new()),
            #[cfg(test)]
            test_land_after_next_has_claim_miss: RwLock::new(None),
            faucets: RwLock::new(Vec::new()),
            monitor_burn_serials: RwLock::new(HashSet::new()),
            monitor_twin_notes: RwLock::new(HashMap::new()),
            monitor_expected_mints: RwLock::new(HashMap::new()),
            projector_cursor: RwLock::new(0),
            reconcile_cursor: RwLock::new(0),
            l1_evidence_cursor: RwLock::new(0),
            l1_evidence_policy: RwLock::new(None),
            tx_note_links: RwLock::new(HashMap::new()),
            note_tx_links: RwLock::new(HashMap::new()),
        }
    }

    /// The claim-lock clock: `Instant::now()`, plus the test-only forward skew.
    /// Every claim-lock timestamp read/write (`try_claim`, `try_reclaim_expired`)
    /// goes through this, so tests can age records by advancing the clock —
    /// which is always representable — instead of `Instant` subtraction, which
    /// underflows (panics) when the process uptime is shorter than the age.
    fn claim_clock_now(&self) -> std::time::Instant {
        let now = std::time::Instant::now();
        #[cfg(test)]
        let now = now + *self.claim_clock_skew.read();
        now
    }

    /// Test-only: age the existing claim-lock record for `global_index` by
    /// `age`, so TTL-expiry paths (`try_reclaim_expired` via
    /// `acquire_claim_lock`) can be exercised through the full RPC pipeline
    /// without sleeping for the real `CLAIM_RESUBMIT_TTL_SECS` or mutating
    /// process-global env (unsafe under parallel tests on edition 2024).
    ///
    /// Implemented as a forward skew of the store's claim clock (all records
    /// age together — each test constructs its own store, so that is exactly
    /// the record under test), never as `Instant::now() - age` arithmetic:
    /// that subtraction underflows (panics) whenever the process has been
    /// alive for less than `age` — e.g. a short test binary with a raised
    /// `CLAIM_RESUBMIT_TTL_SECS`.
    ///
    /// Panics only if `global_index` has no record (a test-authoring error).
    #[cfg(test)]
    pub fn test_backdate_claim(&self, global_index: U256, age: std::time::Duration) {
        assert!(
            self.claimed.read().contains_key(&global_index),
            "test_backdate_claim: no claim record for global_index {global_index}"
        );
        *self.claim_clock_skew.write() += age;
    }

    /// Test hook (#55 BLOCKER B): arm the store so the NEXT
    /// `has_claim_event_for_global_index(gi)` that finds no event lands the claim as
    /// a side effect (see the field doc). Used to deterministically drive the
    /// try_claim-Err → reclaim-fail → re-read-landed interleaving.
    #[cfg(test)]
    pub fn test_land_gi_after_next_has_claim_miss(&self, global_index: [u8; 32]) {
        *self.test_land_after_next_has_claim_miss.write() = Some(global_index);
    }

    #[cfg(test)]
    pub fn test_fail_next_ger_evidence_write(&self) {
        self.test_fail_next_ger_evidence_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test hook (#55 BLOCKER B): arm the store so the NEXT `nonce_advance_cas`
    /// returns a store error, simulating a DB failure on the nonce CAS.
    #[cfg(test)]
    pub fn test_fail_next_nonce_cas(&self) {
        self.test_fail_next_nonce_cas
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test hook (#55 BLOCKER 1): force the admission lease for `(addr, nonce)` to
    /// have already EXPIRED, so the next `reserve_nonce` by the SAME tx takes over
    /// (crash-recovery path). Panics if there is no reservation (test-authoring bug).
    #[cfg(test)]
    pub fn test_expire_reservation_lease(&self, addr: &str, nonce: u64) {
        let key = (addr.to_lowercase(), nonce);
        let mut reservations = self.nonce_reservations.write();
        let r = reservations
            .get_mut(&key)
            .expect("test_expire_reservation_lease: no reservation for (addr, nonce)");
        r.lease_expires_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }

    /// Emit a synthetic BridgeEvent log. Private helper for the atomic B2AGG
    /// commit — it used to be a `Store` trait convenience method, but the only
    /// remaining caller is `commit_b2agg_event_atomic` below (PgStore inlines
    /// its own INSERT), so it lives here as a plain inherent method rather than
    /// widening the trait surface.
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
        metadata: &[u8],
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
                metadata,
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

    // ── Synthetic projector cursor (Phase 2a) ────────────────────

    async fn get_projector_cursor(&self) -> anyhow::Result<u64> {
        Ok(*self.projector_cursor.read())
    }

    async fn set_projector_cursor(&self, block: u64) -> anyhow::Result<()> {
        *self.projector_cursor.write() = block;
        Ok(())
    }

    // ── Note-reconciler sweep cursor ─────────────────────────────

    async fn get_reconcile_cursor(&self) -> anyhow::Result<u64> {
        Ok(*self.reconcile_cursor.read())
    }

    async fn set_reconcile_cursor(&self, block: u64) -> anyhow::Result<()> {
        *self.reconcile_cursor.write() = block;
        Ok(())
    }

    // ── Selected L1 evidence scan ────────────────────────────────

    async fn get_l1_evidence_cursor(&self) -> anyhow::Result<u64> {
        Ok(*self.l1_evidence_cursor.read())
    }

    async fn set_l1_evidence_cursor(&self, block: u64) -> anyhow::Result<()> {
        *self.l1_evidence_cursor.write() = block;
        Ok(())
    }

    async fn bind_l1_evidence_policy(&self, policy: &str) -> anyhow::Result<()> {
        let mut bound = self.l1_evidence_policy.write();
        match bound.as_deref() {
            Some(existing) if existing == policy => return Ok(()),
            Some(existing) => anyhow::bail!(
                "L1 evidence policy mismatch: store is bound to `{existing}`, configured `{policy}`; stop the service and reset/rebuild L1 evidence before changing policy"
            ),
            None => {}
        }

        let has_untagged_state = *self.l1_evidence_cursor.read() != 0
            || self
                .seen_gers
                .read()
                .values()
                .any(|entry| entry.evidence_verified);
        if has_untagged_state {
            anyhow::bail!(
                "L1 evidence state exists without an evidence policy; reset/rebuild L1 evidence before serving"
            );
        }
        *bound = Some(policy.to_owned());
        Ok(())
    }

    // ── Receipts map (Phase 2b substrate; unused in 2a) ──────────

    async fn record_tx_note_link(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<()> {
        // First-write-wins on the forward map; a second write for an
        // already-linked tx_hash is a no-op. The reverse index mirrors the
        // same first association so note -> tx stays consistent.
        let mut fwd = self.tx_note_links.write();
        if fwd.contains_key(tx_hash) {
            return Ok(());
        }
        fwd.insert(
            tx_hash.to_string(),
            NoteHandoffRecord {
                note_commitment: note_commitment.to_string(),
                note_id: None,
                state: NoteHandoffState::Submitted,
                expiration_block: None,
            },
        );
        drop(fwd);
        self.note_tx_links
            .write()
            .entry(note_commitment.to_string())
            .or_insert_with(|| tx_hash.to_string());
        Ok(())
    }

    async fn get_note_link_for_tx(&self, tx_hash: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .tx_note_links
            .read()
            .get(tx_hash)
            .map(|record| record.note_commitment.clone()))
    }

    async fn get_tx_for_note(&self, note_commitment: &str) -> anyhow::Result<Option<String>> {
        Ok(self.note_tx_links.read().get(note_commitment).cloned())
    }

    async fn get_note_handoff_for_tx(&self, tx_hash: &str) -> anyhow::Result<Option<NoteHandoff>> {
        Ok(self
            .tx_note_links
            .read()
            .get(tx_hash)
            .map(|record| NoteHandoff {
                note_commitment: record.note_commitment.clone(),
                note_id: record.note_id.clone(),
                state: record.state,
                expiration_block: record.expiration_block,
            }))
    }

    async fn pending_note_handoff_txs(
        &self,
        after: Option<TxHash>,
        limit: usize,
    ) -> anyhow::Result<Vec<TxHash>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let links = self.tx_note_links.read();
        let txns = self.transactions.lock();
        let mut pending: Vec<TxHash> = txns
            .iter()
            .filter(|(tx_hash, txn)| {
                txn.result.is_none()
                    && links
                        .get(&format!("{tx_hash:#x}"))
                        .is_some_and(|link| link.note_id.is_some())
            })
            .map(|(tx_hash, _)| *tx_hash)
            .collect();
        pending.sort_unstable();
        Ok(pending
            .into_iter()
            .filter(|tx_hash| after.is_none_or(|after| *tx_hash > after))
            .take(limit)
            .collect())
    }

    async fn prepare_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
        note_id: &str,
        expiration_block: u64,
    ) -> anyhow::Result<()> {
        let mut links = self.tx_note_links.write();
        if let Some(existing) = links.get(tx_hash) {
            if existing.note_commitment != note_commitment
                || existing.note_id.as_deref() != Some(note_id)
            {
                anyhow::bail!("transaction {tx_hash} is already linked to a different note");
            }
            return Ok(());
        }
        links.insert(
            tx_hash.to_string(),
            NoteHandoffRecord {
                note_commitment: note_commitment.to_string(),
                note_id: Some(note_id.to_string()),
                state: NoteHandoffState::Prepared,
                expiration_block: Some(expiration_block),
            },
        );
        self.note_tx_links
            .write()
            .entry(note_commitment.to_string())
            .or_insert_with(|| tx_hash.to_string());
        Ok(())
    }

    async fn confirm_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<bool> {
        let mut links = self.tx_note_links.write();
        let mut claimed = self.claimed.write();
        let Some(record) = links.get_mut(tx_hash) else {
            return Ok(false);
        };
        if record.note_commitment != note_commitment {
            return Ok(false);
        }
        record.state = NoteHandoffState::Submitted;
        record.expiration_block = None;
        if let Ok(owner) = tx_hash.parse::<TxHash>() {
            for claim in claimed.values_mut() {
                if claim.owner_tx_hash == Some(owner) && claim.state == ClaimState::Prepared {
                    claim.state = ClaimState::Submitted;
                    claim.lease_expires_at = None;
                }
            }
        }
        Ok(true)
    }

    async fn confirm_note_handoff_by_commitment(
        &self,
        note_commitment: &str,
    ) -> anyhow::Result<Option<String>> {
        let preferred = self.note_tx_links.read().get(note_commitment).cloned();
        let mut links = self.tx_note_links.write();
        let mut matching: Vec<String> = links
            .iter()
            .filter(|(_, link)| link.note_commitment == note_commitment)
            .map(|(tx_hash, _)| tx_hash.clone())
            .collect();
        if matching.is_empty() {
            return Ok(None);
        }
        matching.sort();
        let mut claimed = self.claimed.write();
        for tx_hash in &matching {
            let link = links.get_mut(tx_hash).expect("matching link exists");
            link.state = NoteHandoffState::Submitted;
            link.expiration_block = None;
            if let Ok(owner) = tx_hash.parse::<TxHash>() {
                for claim in claimed.values_mut() {
                    if claim.owner_tx_hash == Some(owner) && claim.state == ClaimState::Prepared {
                        claim.state = ClaimState::Submitted;
                        claim.lease_expires_at = None;
                    }
                }
            }
        }
        Ok(Some(preferred.unwrap_or_else(|| matching[0].clone())))
    }

    async fn confirm_prepared_note_handoffs(&self, note_ids: &[String]) -> anyhow::Result<u64> {
        let ids: HashSet<&str> = note_ids.iter().map(String::as_str).collect();
        let matches: Vec<(String, String)> = self
            .tx_note_links
            .read()
            .iter()
            .filter(|(_, link)| {
                link.state == NoteHandoffState::Prepared
                    && link.note_id.as_deref().is_some_and(|id| ids.contains(id))
            })
            .map(|(tx_hash, link)| (tx_hash.clone(), link.note_commitment.clone()))
            .collect();
        let mut confirmed = 0;
        for (tx_hash, commitment) in matches {
            confirmed += u64::from(self.confirm_note_handoff(&tx_hash, &commitment).await?);
        }
        Ok(confirmed)
    }

    async fn clear_expired_prepared_note_handoff(
        &self,
        tx_hash: &str,
        note_commitment: &str,
    ) -> anyhow::Result<bool> {
        let cursor = *self.reconcile_cursor.read();
        let mut links = self.tx_note_links.write();
        let mut claimed = self.claimed.write();
        let Some(record) = links.get(tx_hash) else {
            return Ok(false);
        };
        if record.state != NoteHandoffState::Prepared
            || record.note_commitment != note_commitment
            || record
                .expiration_block
                .is_none_or(|expiration| cursor <= expiration)
        {
            return Ok(false);
        }
        if let Ok(owner) = tx_hash.parse::<TxHash>() {
            let matching_claim = claimed.iter().find_map(|(gi, claim)| {
                (claim.owner_tx_hash == Some(owner)).then_some((*gi, claim.state))
            });
            match matching_claim {
                Some((gi, ClaimState::Prepared)) => {
                    claimed.remove(&gi);
                }
                Some((_gi, ClaimState::Landed)) => return Ok(false),
                None => {}
                Some(_) => return Ok(false),
            }
        }
        links.remove(tx_hash);
        if self
            .note_tx_links
            .read()
            .get(note_commitment)
            .is_some_and(|v| v == tx_hash)
        {
            self.note_tx_links.write().remove(note_commitment);
        }
        if let Ok(hash) = tx_hash.parse::<TxHash>()
            && let Some(receipt) = self.transactions.lock().get_mut(&hash)
            && receipt.result.as_ref().is_some_and(Result::is_err)
        {
            receipt.result = None;
            receipt.block_num = 0;
        }
        Ok(true)
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

        // Cantina #12 redesign — mirror the PgStore contract: NO row cap. Run the
        // exact `matches()` over every candidate and return ALL matches (a sparse
        // match in a dense range returns exactly the matches, never an error).
        // When a block_hash is set, `matches()` IGNORES the block range and keys
        // on the hash, so we must scan EVERY block — mirroring PgStore's
        // range-independent block_hash superset; otherwise we scan the inclusive
        // range. The only limit is the OOM ceiling on the matched count.
        let mut result = Vec::new();
        let logs_by_block = self.logs_by_block.read();
        let candidate_logs: Vec<&Vec<SyntheticLog>> = if filter.block_hash.is_some() {
            logs_by_block.values().collect()
        } else {
            (from..=to).filter_map(|b| logs_by_block.get(&b)).collect()
        };
        for logs in candidate_logs {
            for log in logs {
                if filter.matches(log, current_block) {
                    result.push(log.clone());
                    // OOM backstop only — NOT a normal cap.
                    if result.len() > super::GETLOGS_SAFETY_CEILING {
                        return Err(super::getlogs_row_cap_error(from, to));
                    }
                }
            }
        }
        // eth_getLogs ordering contract: results MUST be ordered by
        // (block_number, log_index), matching PgStore's `ORDER BY block_number,
        // log_index`. The range path already yields this (ascending blocks; within
        // a block, insertion order == log_index order), but the block_hash path
        // scans `HashMap::values()` in ARBITRARY order — so sort unconditionally to
        // pin the contract for both paths.
        result.sort_by_key(|l| (l.block_number, l.log_index));
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
        #[cfg(test)]
        if self
            .test_fail_next_ger_evidence_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected fault: durable evidence write failed");
        }
        let mut seen = self.seen_gers.write();
        let entry = seen.entry(*ger).or_insert(GerEntry {
            mainnet_exit_root: None,
            rollup_exit_root: None,
            block_number: 0,
            timestamp: 0,
            evidence_verified: false,
        });
        entry.mainnet_exit_root = Some(mainnet_exit_root);
        entry.rollup_exit_root = Some(rollup_exit_root);
        // Mirror the PgStore semantics: indexer is authoritative for L1
        // origin metadata, so overwrite unconditionally on every call.
        entry.block_number = l1_block_number;
        entry.timestamp = l1_timestamp;
        // Legacy physical name: this now means "written by the configured
        // latest/safe/finalized scan".
        entry.evidence_verified = true;
        Ok(())
    }

    async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(self.injected_gers.read().contains(ger))
    }

    /// Atomic GER commit (audit H2). Folds the idempotent chain roll + log
    /// emission with `is_injected = TRUE` into one operation, so a retry can
    /// never roll the hash chain / emit the synthetic log a second time (the
    /// in-memory analogue of postgres's single-transaction version).
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
        // Observing the exact note is authoritative confirmation of the
        // pre-submit handoff. Hold this guard through the in-memory commit so a
        // recovery clear cannot interleave with event publication.
        let mut links = self.tx_note_links.write();
        if let Some(link) = links.get_mut(&tx_hash.to_lowercase()) {
            link.state = NoteHandoffState::Submitted;
            link.expiration_block = None;
        }

        {
            let mut seen = self.seen_gers.write();
            if !seen.contains_key(global_exit_root) {
                seen.insert(
                    *global_exit_root,
                    GerEntry {
                        mainnet_exit_root,
                        rollup_exit_root,
                        block_number,
                        timestamp,
                        evidence_verified: false,
                    },
                );
                *self.latest_ger.write() = Some(*global_exit_root);
            }
        }

        // Audit H2 — idempotent chain roll + log emission. A retry (e.g. after a
        // crash) used to roll the hash chain and emit a duplicate synthetic log
        // a SECOND time, diverging the proxy's chain from aggkit. Gate on
        // whether a log with this deterministic tx_hash was already emitted.
        let already_emitted = self.logs_by_tx.read().contains_key(&tx_hash.to_lowercase());
        if !already_emitted {
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
                // Canonical lowercase, consistent with the gate above,
                // add_log's lowercase keying, and get_logs_for_tx — and with the
                // postgres store, which persists the lowercase transaction_hash.
                transaction_hash: tx_hash.to_lowercase(),
                transaction_index: 0,
                log_index: 0,
                removed: false,
            };
            let mut log = log;
            let mut counter = self.log_counter.write();
            log.log_index = *counter;
            *counter += 1;
            drop(counter);
            self.logs_by_block
                .write()
                .entry(block_number)
                .or_default()
                .push(log.clone());
            self.logs_by_tx
                .write()
                .entry(tx_hash.to_lowercase())
                .or_default()
                .push(log.clone());
            self.pending_events.write().push(log);
        }

        // Always set is_injected = TRUE (idempotent).
        self.injected_gers.write().insert(*global_exit_root);
        if let Ok(hash) = tx_hash.parse::<TxHash>()
            && let Some(receipt) = self.transactions.lock().get_mut(&hash)
        {
            receipt.result = Some(Ok(()));
            receipt.block_num = block_number;
        }
        drop(links);
        Ok(())
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

    async fn txn_begin_if_absent(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<bool> {
        let mut txns = self.transactions.lock();
        if let Some(receipt) = txns.get_mut(&tx_hash) {
            if receipt.result.is_none() {
                if entry.id.is_some() {
                    receipt.id = entry.id;
                }
                if entry.expires_at.is_some() {
                    receipt.expires_at = entry.expires_at;
                }
                if !entry.logs.is_empty() {
                    receipt.logs = entry.logs;
                }
            }
            return Ok(false);
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
        Ok(true)
    }

    async fn txn_commit(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()> {
        // Once an exact handoff exists, an error is ambiguous until commit,
        // observation, or expiration reconciliation. Never expose status 0.
        let logs_to_add = {
            let links = self.tx_note_links.read();
            if result.is_err() && links.contains_key(&format!("{tx_hash:#x}")) {
                return Ok(());
            }
            let mut txns = self.transactions.lock();
            let Some(receipt) = txns.get_mut(&tx_hash) else {
                anyhow::bail!("Store: transaction {tx_hash} not found");
            };
            // A real Miden landing must always beat a failure observation, and a
            // landed success must never be clobbered. Pending failures are
            // terminal; a later real landing may still heal one to success.
            enum St {
                Pending,
                Failed,
                Success,
            }
            let st = match &receipt.result {
                None => St::Pending,
                Some(Ok(_)) => St::Success,
                Some(Err(_)) => St::Failed,
            };
            let apply = match (&st, result.is_ok()) {
                (St::Success, _) => false,
                (_, true) => true,
                (St::Pending, false) => true,
                (St::Failed, false) => false,
            };
            if !apply {
                tracing::debug!(
                    "Store: txn {tx_hash} terminal transition ignored (success-always-wins CAS)"
                );
                None
            } else {
                let is_ok = result.is_ok();
                receipt.result = Some(result);
                receipt.block_num = block_num;
                if is_ok {
                    tracing::info!(
                        "Store: committed txn {tx_hash}; miden txn: {:?}",
                        receipt.id
                    );
                    Some(receipt.logs.clone())
                } else {
                    tracing::error!("Store: failed txn {tx_hash}; miden txn: {:?}", receipt.id);
                    None
                }
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

    async fn txn_commit_confirmed_duplicate(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
    ) -> anyhow::Result<()> {
        let mut txns = self.transactions.lock();
        let Some(receipt) = txns.get_mut(&tx_hash) else {
            anyhow::bail!("Store: transaction {tx_hash} not found");
        };
        if receipt.result.is_none() {
            receipt.result = Some(result);
            receipt.block_num = block_num;
            receipt.logs.clear();
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

    async fn pending_nonce_frontier(&self, addr: &str) -> anyhow::Result<PendingNonceFrontier> {
        let addr = addr.to_lowercase();
        let links = self.tx_note_links.read();
        let txns = self.transactions.lock();
        let mut frontier = PendingNonceFrontier::default();
        for (tx_hash, receipt) in txns.iter() {
            if receipt.result.is_some() || format!("{:#x}", receipt.signer).to_lowercase() != addr {
                continue;
            }
            let nonce = super::envelope_nonce(&receipt.envelope);
            frontier.lowest_pending = Some(
                frontier
                    .lowest_pending
                    .map_or(nonce, |current| current.min(nonce)),
            );
            if !links
                .get(&format!("{tx_hash:#x}"))
                .is_some_and(|link| link.state == NoteHandoffState::Submitted)
            {
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

    async fn nonce_advance_cas(&self, addr: &str, expected: u64) -> anyhow::Result<bool> {
        // BLOCKER B test hook — one-shot simulated CAS store failure.
        if self
            .test_fail_next_nonce_cas
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("simulated nonce_advance_cas store failure (test hook)");
        }
        let key = addr.to_lowercase();
        let mut nonces = self.nonces.write();
        let cur = nonces.entry(key).or_insert(0);
        if *cur == expected {
            *cur = expected + 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn reserve_nonce(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<crate::store::NonceReservation> {
        use crate::store::NonceReservation;
        let key = (addr.to_lowercase(), nonce);
        let now = std::time::Instant::now();
        let mut reservations = self.nonce_reservations.write();
        let existing = reservations.get(&key).cloned();
        match existing {
            None => {
                reservations.insert(
                    key,
                    Reservation {
                        tx_hash,
                        state: ReservationState::Executing,
                        lease_expires_at: now + lease,
                        fence: 1,
                    },
                );
                Ok(NonceReservation::Won { fence: 1 })
            }
            Some(r) => {
                // A nonce slot is permanently bound to its first tx hash. Even a
                // failed or expired attempt may have crossed an external side-effect
                // boundary, so a different replacement can never take it over.
                let takeover = r.tx_hash == tx_hash
                    && (matches!(
                        r.state,
                        ReservationState::ReleasedFailure | ReservationState::ReleasedSuccess
                    ) || r.lease_expires_at <= now);
                if takeover {
                    let fence = r.fence + 1;
                    reservations.insert(
                        key,
                        Reservation {
                            tx_hash,
                            state: ReservationState::Executing,
                            lease_expires_at: now + lease,
                            fence,
                        },
                    );
                    Ok(NonceReservation::Won { fence })
                } else if r.tx_hash == tx_hash {
                    Ok(NonceReservation::OwnedBySame)
                } else {
                    Ok(NonceReservation::HeldByOther(r.tx_hash))
                }
            }
        }
    }

    async fn renew_reservation(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<bool> {
        let key = (addr.to_lowercase(), nonce);
        let mut reservations = self.nonce_reservations.write();
        if let Some(r) = reservations.get_mut(&key)
            && r.tx_hash == tx_hash
            && !matches!(r.state, ReservationState::ReleasedFailure)
        {
            r.lease_expires_at = std::time::Instant::now() + lease;
            return Ok(true);
        }
        Ok(false)
    }

    async fn release_reservation(
        &self,
        addr: &str,
        nonce: u64,
        tx_hash: TxHash,
        fence: u64,
        success: bool,
    ) -> anyhow::Result<()> {
        let key = (addr.to_lowercase(), nonce);
        let mut reservations = self.nonce_reservations.write();
        if let Some(r) = reservations.get_mut(&key)
            && r.tx_hash == tx_hash
            && r.fence == fence
            && r.state == ReservationState::Executing
        {
            r.state = if success {
                ReservationState::ReleasedSuccess
            } else {
                r.lease_expires_at = std::time::Instant::now();
                ReservationState::ReleasedFailure
            };
        }
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
        // BLOCKER C — receipt + nonce in one atomic step: hold BOTH the
        // transactions and nonces locks so a reader can never observe the
        // receipt committed with the nonce not yet advanced (or vice versa).
        // The row is inserted already committed-`failed` (empty logs, no
        // synthetic ClaimEvent), so there is no pending window.
        let links = self.tx_note_links.read();
        let mut txns = self.transactions.lock();
        let mut nonces = self.nonces.write();
        // BLOCKER 4 — CONDITIONAL: never overwrite a REAL receipt. If this hash
        // already has a pending (result None → a real claim awaiting the projector)
        // or successful (Some(Ok) → a landed real claim) receipt, DO NOT rewrite it
        // to status 0. A cross-replica accept-and-revert on the same hash must
        // converge to the real outcome, not suppress a real success/pending. Only
        // write the reverted receipt when the hash is absent or already `failed`
        // (idempotent re-affirm).
        let may_write = match txns.peek(&tx_hash).map(|r| &r.result) {
            None => true, // absent
            Some(None) => !links.contains_key(&format!("{tx_hash:#x}")),
            // linked pending receipt is a real external handoff; an unlinked pending
            // row is only the durable pre-admission intent and may be reverted
            Some(Some(Ok(()))) => false, // successful real receipt — keep it
            Some(Some(Err(_))) => true,  // already failed — re-affirm
        };
        if may_write {
            let receipt = TxnReceipt {
                id: entry.id,
                envelope: entry.envelope,
                signer: entry.signer,
                expires_at: entry.expires_at,
                result: Some(Err(reason)),
                block_num,
                logs: vec![],
            };
            let _ = txns.put(tx_hash, receipt);
        }
        let cur = nonces.entry(addr.to_lowercase()).or_insert(0);
        let advanced = if *cur == expected_nonce {
            *cur = expected_nonce + 1;
            true
        } else {
            false
        };
        Ok(advanced)
    }

    // ── Claims ───────────────────────────────────────────────────

    async fn try_claim_fenced(
        &self,
        global_index: U256,
        owner_tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<ClaimFence>> {
        let now = self.claim_clock_now();
        let mut claimed = self.claimed.write();
        if claimed.contains_key(&global_index) {
            return Ok(None);
        }
        claimed.insert(
            global_index,
            ClaimRecord {
                owner_tx_hash: Some(owner_tx_hash),
                state: ClaimState::Executing,
                acquired_at: now,
                lease_expires_at: Some(now + lease),
                fence: 1,
            },
        );
        Ok(Some(ClaimFence { fence: 1 }))
    }

    async fn try_reclaim_claim_fenced(
        &self,
        global_index: U256,
        owner_tx_hash: TxHash,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<ClaimFence>> {
        let now = self.claim_clock_now();
        let mut claimed = self.claimed.write();
        let Some(record) = claimed.get_mut(&global_index) else {
            return Ok(None);
        };
        let expired = record
            .lease_expires_at
            .is_some_and(|deadline| deadline <= now)
            || (record.lease_expires_at.is_none()
                && now.saturating_duration_since(record.acquired_at) >= lease);
        if record.state != ClaimState::Executing
            || (record.owner_tx_hash != Some(owner_tx_hash) && !expired)
        {
            return Ok(None);
        }
        record.owner_tx_hash = Some(owner_tx_hash);
        record.acquired_at = now;
        record.lease_expires_at = Some(now + lease);
        record.fence += 1;
        Ok(Some(ClaimFence {
            fence: record.fence,
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
        let now = self.claim_clock_now();
        let tx_key = format!("{tx_hash:#x}");
        let mut links = self.tx_note_links.write();
        if let Some(existing) = links.get(&tx_key) {
            if existing.note_commitment != note_commitment
                || existing.note_id.as_deref() != Some(note_id)
            {
                anyhow::bail!("transaction {tx_key} is already linked to a different claim note");
            }
            return Ok(false);
        }
        let mut claimed = self.claimed.write();
        let Some(record) = claimed.get_mut(&global_index) else {
            return Ok(false);
        };
        if record.owner_tx_hash != Some(owner_tx_hash)
            || record.fence != fence
            || record.state != ClaimState::Executing
            || record
                .lease_expires_at
                .is_none_or(|deadline| deadline <= now)
        {
            return Ok(false);
        }
        links.insert(
            tx_key.clone(),
            NoteHandoffRecord {
                note_commitment: note_commitment.to_string(),
                note_id: Some(note_id.to_string()),
                state: NoteHandoffState::Prepared,
                expiration_block: Some(expiration_block),
            },
        );
        record.state = ClaimState::Prepared;
        record.lease_expires_at = None;
        self.note_tx_links
            .write()
            .entry(note_commitment.to_string())
            .or_insert(tx_key);
        Ok(true)
    }

    async fn unclaim_fenced(
        &self,
        global_index: &U256,
        owner_tx_hash: TxHash,
        fence: u64,
    ) -> anyhow::Result<bool> {
        let mut claimed = self.claimed.write();
        let removable = claimed.get(global_index).is_some_and(|record| {
            record.owner_tx_hash == Some(owner_tx_hash)
                && record.fence == fence
                && record.state == ClaimState::Executing
        });
        if removable {
            claimed.remove(global_index);
        }
        Ok(removable)
    }

    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
        let mut claimed = self.claimed.write();
        if claimed.contains_key(&global_index) {
            anyhow::bail!("claim already submitted for global_index {global_index}");
        }
        let now = self.claim_clock_now();
        claimed.insert(
            global_index,
            ClaimRecord {
                owner_tx_hash: None,
                state: ClaimState::Executing,
                acquired_at: now,
                lease_expires_at: None,
                fence: 0,
            },
        );
        Ok(())
    }

    async fn try_reclaim_expired(
        &self,
        global_index: U256,
        ttl: std::time::Duration,
    ) -> anyhow::Result<bool> {
        // One write lock end-to-end = atomic check-and-refresh: exactly one of any
        // concurrent recoveries wins; the losers observe the refreshed timestamp.
        let now = self.claim_clock_now();
        let mut claimed = self.claimed.write();
        match claimed.get_mut(&global_index) {
            Some(record)
                if record.owner_tx_hash.is_none()
                    && record.state == ClaimState::Executing
                    && now.saturating_duration_since(record.acquired_at) >= ttl =>
            {
                record.acquired_at = now;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()> {
        self.claimed.write().remove(global_index);
        Ok(())
    }

    async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool> {
        Ok(self.claimed.read().contains_key(global_index))
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

    async fn get_deposit_count(&self) -> anyhow::Result<u64> {
        Ok(*self.deposit_counter.read() as u64)
    }

    /// Atomic, idempotent B2AGG commit (audit H1/H3). Reuses the original
    /// `deposit_count` (no gap on retry) and emits the BridgeEvent at most once.
    ///
    /// Locking note: the `processed_notes` + `deposit_counter` write guards are
    /// held ONLY for the step-1 allocation block below, then dropped at the end
    /// of that scope; step 2 (the already-emitted check + `add_bridge_event`)
    /// runs without them held. That is sound because of the SINGLE-WRITER SERIAL
    /// INVARIANT: `commit_b2agg_event_atomic` is called ONLY from the projector
    /// path, which is strictly serial. The projector `tick()` borrows
    /// `&mut MidenClientLib` (one non-reentrant client) and commits one block at
    /// a time, write-before-advance:
    ///     while cursor < tip { project_block_notes(next).await?; set_projector_cursor(next).await? }
    /// so at most one commit is ever in flight for a given store. The
    /// `RECONCILE_CONCURRENCY` fan-out is FETCH-only (`sync_note_ids`), never the
    /// commit. No concurrent writer can therefore slip between the read and the
    /// insert here — the "TOCTOU" a reviewer might flag is not reachable, which
    /// is why no coarser lock (or tx_hash UNIQUE constraint) is needed.
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
        // 1. Allocate / reuse deposit_count atomically.
        let deposit_count = {
            let mut processed = self.processed_notes.write();
            if let Some(&existing) = processed.get(&note_id) {
                existing
            } else {
                let mut counter = self.deposit_counter.write();
                let dc = *counter;
                *counter += 1;
                processed.insert(note_id.clone(), dc);
                dc
            }
        };

        // 2. Emit the BridgeEvent. Idempotent on retry: the projector derives
        //    tx_hash deterministically from note_id, so a second emit would
        //    only duplicate the log. The InMemoryStore has no tx_hash unique
        //    constraint, so guard by checking the existing logs_by_block entry
        //    for the same tx_hash before emitting. This read-then-insert is race
        //    free under the single-writer serial invariant documented above (the
        //    projector is the only, strictly-serial caller).
        //
        //    Per-block scope is sufficient — this check only inspects
        //    `logs_by_block[block_number]`, NOT every block. That is not a
        //    cross-block dedup hole: a given note only ever projects to one
        //    block, and cross-block RE-projection is already fenced off upstream
        //    by the GLOBAL processed-note set. The projector consults
        //    `is_note_processed(note_id)` before it ever calls this method, so
        //    once a note is committed at block A it can never re-enter here for a
        //    later block B. The only way we reach this point twice for the same
        //    note is a same-block retry (same `block_number`, same derived
        //    `tx_hash`), which this per-block scan catches exactly.
        let already_emitted = self
            .logs_by_block
            .read()
            .get(&block_number)
            .map(|logs| logs.iter().any(|l| l.transaction_hash == tx_hash))
            .unwrap_or(false);
        if !already_emitted {
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
        }
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
        drop(logs);
        // Test hook (BLOCKER B): this call found NO event. If armed for this gi, LAND
        // it now so the NEXT call observes it, and still report this miss.
        #[cfg(test)]
        {
            let mut armed = self.test_land_after_next_has_claim_miss.write();
            if armed.as_ref() == Some(global_index) {
                *armed = None;
                drop(armed);
                self.claim_watcher_processed
                    .write()
                    .insert("blockerB-race-land".to_string(), *global_index);
            }
        }
        Ok(false)
    }

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
        // Link -> claim is the global handoff lock order. A ClaimEvent is the
        // terminal claim fence even on replay, so a publisher whose final read
        // raced this commit cannot subsequently prepare under an executing row.
        let mut links = self.tx_note_links.write();
        if let Some(link) = links.get_mut(&tx_hash.to_lowercase()) {
            link.state = NoteHandoffState::Submitted;
            link.expiration_block = None;
        }
        let gi = U256::from_be_bytes(global_index);
        let mut claimed = self.claimed.write();
        match claimed.entry(gi) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let claim = entry.get_mut();
                claim.state = ClaimState::Landed;
                claim.lease_expires_at = None;
                claim.fence += 1;
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ClaimRecord {
                    owner_tx_hash: None,
                    state: ClaimState::Landed,
                    acquired_at: self.claim_clock_now(),
                    lease_expires_at: None,
                    fence: 1,
                });
            }
        }

        let mut processed = self.claim_watcher_processed.write();
        let inserted = match processed.entry(note_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(global_index);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        };

        // Finalise a real linked receipt under the same in-process critical
        // section. Derived hashes simply have no transaction row.
        if let Ok(hash) = tx_hash.parse::<TxHash>()
            && let Some(receipt) = self.transactions.lock().get_mut(&hash)
        {
            receipt.result = Some(Ok(()));
            receipt.block_num = block_number;
        }

        if !inserted {
            return Ok(());
        }

        let mut log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()],
            data: crate::log_synthesis::encode_claim_event_data_u64(
                &global_index,
                origin_network,
                origin_address,
                destination_address,
                amount,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_lowercase(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        let mut counter = self.log_counter.write();
        log.log_index = *counter;
        *counter += 1;
        self.logs_by_block
            .write()
            .entry(block_number)
            .or_default()
            .push(log.clone());
        self.logs_by_tx
            .write()
            .entry(tx_hash.to_lowercase())
            .or_default()
            .push(log.clone());
        self.pending_events.write().push(log);
        Ok(())
    }

    // ── Faucet registry ──────────────────────────────────────────

    async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()> {
        let mut faucets = self.faucets.write();
        // Idempotent by faucet_id: the same faucet re-registering (e.g. startup
        // re-init) refreshes its mutable fields. Mirrors PgStore, whose faucet_id
        // primary key + Cantina #13 metadata guard impose the same two rules:
        //   (a) the origin is IMMUTABLE for a given faucet_id — PgStore would hit
        //       a duplicate-key error on a re-register carrying a different
        //       origin, so reject it here rather than silently rebinding;
        //   (b) never clobber stored metadata with empty — a blank re-register
        //       (`metadata = vec![]`) must not wipe good metadata persisted by an
        //       earlier non-empty registration or the Layer-2 backfill.
        if let Some(existing) = faucets.iter_mut().find(|f| f.faucet_id == entry.faucet_id) {
            if existing.origin_address != entry.origin_address
                || existing.origin_network != entry.origin_network
            {
                anyhow::bail!(
                    "register_faucet: faucet {} is already registered for origin \
                     (network {}); refusing to rebind it to a different origin (network {})",
                    entry.faucet_id,
                    existing.origin_network,
                    entry.origin_network,
                );
            }
            existing.symbol = entry.symbol;
            existing.origin_decimals = entry.origin_decimals;
            existing.miden_decimals = entry.miden_decimals;
            existing.scale = entry.scale;
            // Cantina #13 — only overwrite when the new metadata is non-empty.
            if !entry.metadata.is_empty() {
                existing.metadata = entry.metadata;
            }
            return Ok(());
        }
        // Finding #10 — converge on the (origin_address, origin_network) key.
        // A *different* faucet already owning this origin route means a
        // concurrent first-claim (or admin register) won the race; first-write
        // wins so we do NOT strand a second faucet by pushing a colliding row.
        // This mirrors `PgStore::register_faucet`'s
        // `ON CONFLICT (origin_address, origin_network)` convergence and the
        // real `idx_faucet_origin` unique index. Admin route *repair* uses the
        // dedicated repair tooling instead.
        if faucets.iter().any(|f| {
            f.origin_address == entry.origin_address && f.origin_network == entry.origin_network
        }) {
            tracing::warn!(
                origin_network = entry.origin_network,
                new_faucet_id = %entry.faucet_id,
                "finding #10: register_faucet origin already owned by another faucet; \
                 keeping the existing route (first-write wins)"
            );
            return Ok(());
        }
        faucets.push(entry);
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
    async fn l1_evidence_policy_binding_is_immutable() {
        let store = InMemoryStore::new();
        store.bind_l1_evidence_policy("finalized").await.unwrap();
        store.bind_l1_evidence_policy("finalized").await.unwrap();

        let err = store
            .bind_l1_evidence_policy("safe")
            .await
            .expect_err("a database policy change must fail closed");
        assert!(format!("{err:#}").contains("bound to `finalized`"));
    }

    #[tokio::test]
    async fn untagged_evidence_state_is_rejected() {
        let store = InMemoryStore::new();
        let ger = [0xA7; 32];
        store
            .set_ger_exit_roots(&ger, [1; 32], [2; 32], 10, 20)
            .await
            .unwrap();

        let err = store
            .bind_l1_evidence_policy("finalized")
            .await
            .expect_err("untagged verification evidence is ambiguous");
        assert!(format!("{err:#}").contains("without an evidence policy"));
    }

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
        assert!(entry.evidence_verified);

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
    async fn test_projector_cursor_round_trip() {
        // Synthetic-indexer redesign (Phase 2a): the projector cursor defaults
        // to 0 on a fresh store and round-trips through set/get.
        let store = InMemoryStore::new();
        assert_eq!(
            store.get_projector_cursor().await.unwrap(),
            0,
            "fresh store cursor must default to 0"
        );
        store.set_projector_cursor(7).await.unwrap();
        assert_eq!(store.get_projector_cursor().await.unwrap(), 7);
        // Overwrites (the projector advances monotonically but the store does
        // not enforce it — it just persists whatever the single owner writes).
        store.set_projector_cursor(42).await.unwrap();
        assert_eq!(store.get_projector_cursor().await.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_reconcile_cursor_round_trip() {
        // Note-reconciler sweep cursor persistence (prod incident: the cursor
        // was memory-only, so every container restart re-swept from genesis —
        // ~3h of resync on prod history). Defaults to 0 on a fresh store (the
        // designed first-boot heal sweep) and round-trips through set/get,
        // including the reset-to-0 the recovery flows perform.
        let store = InMemoryStore::new();
        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            0,
            "fresh store reconcile cursor must default to 0 (first-boot heal)"
        );
        store.set_reconcile_cursor(200).await.unwrap();
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 200);
        store.set_reconcile_cursor(400).await.unwrap();
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 400);
        // Reset-to-genesis (restore / --reset-miden-store / --resweep-from-genesis).
        store.set_reconcile_cursor(0).await.unwrap();
        assert_eq!(store.get_reconcile_cursor().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_tx_note_link_first_write_wins() {
        // Receipts map (Phase 2b substrate): first-write-wins forward map plus a
        // consistent reverse index.
        let store = InMemoryStore::new();
        assert_eq!(store.get_note_link_for_tx("0xtx1").await.unwrap(), None);
        assert_eq!(store.get_tx_for_note("note_a").await.unwrap(), None);

        store.record_tx_note_link("0xtx1", "note_a").await.unwrap();
        assert_eq!(
            store.get_note_link_for_tx("0xtx1").await.unwrap(),
            Some("note_a".to_string())
        );
        assert_eq!(
            store.get_tx_for_note("note_a").await.unwrap(),
            Some("0xtx1".to_string())
        );

        // First-write-wins: a second link for the same tx_hash is a no-op.
        store.record_tx_note_link("0xtx1", "note_b").await.unwrap();
        assert_eq!(
            store.get_note_link_for_tx("0xtx1").await.unwrap(),
            Some("note_a".to_string()),
            "second write for an existing tx_hash must not overwrite"
        );
        // The reverse index for the losing commitment was never created.
        assert_eq!(store.get_tx_for_note("note_b").await.unwrap(), None);

        // A distinct tx_hash links independently.
        store.record_tx_note_link("0xtx2", "note_c").await.unwrap();
        assert_eq!(
            store.get_note_link_for_tx("0xtx2").await.unwrap(),
            Some("note_c".to_string())
        );
        assert_eq!(
            store.get_tx_for_note("note_c").await.unwrap(),
            Some("0xtx2".to_string())
        );
    }

    #[tokio::test]
    async fn prepared_handoff_clears_only_after_authoritative_expiration() {
        let store = InMemoryStore::new();
        let tx = "0xprepared";
        store
            .prepare_note_handoff(tx, "commitment", "note-id", 10)
            .await
            .unwrap();

        store.set_reconcile_cursor(10).await.unwrap();
        assert!(
            !store
                .clear_expired_prepared_note_handoff(tx, "commitment")
                .await
                .unwrap()
        );
        store.set_reconcile_cursor(11).await.unwrap();
        assert!(
            store
                .clear_expired_prepared_note_handoff(tx, "commitment")
                .await
                .unwrap()
        );
        assert!(store.get_note_handoff_for_tx(tx).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn exact_observation_confirms_all_matching_handoffs_with_stable_attribution() {
        let store = InMemoryStore::new();
        store
            .prepare_note_handoff("0xtx2", "same-note", "note-id-2", 10)
            .await
            .unwrap();
        store
            .prepare_note_handoff("0xtx1", "same-note", "note-id-1", 10)
            .await
            .unwrap();

        assert_eq!(
            store
                .confirm_note_handoff_by_commitment("same-note")
                .await
                .unwrap(),
            Some("0xtx2".to_string()),
            "the first-associated tx remains the projector attribution"
        );
        for tx in ["0xtx1", "0xtx2"] {
            assert_eq!(
                store
                    .get_note_handoff_for_tx(tx)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                NoteHandoffState::Submitted
            );
        }
    }

    #[tokio::test]
    async fn raw_note_id_confirmation_prevents_expiration_clear() {
        let store = InMemoryStore::new();
        store
            .prepare_note_handoff("0xtx", "commitment", "exact-note-id", 10)
            .await
            .unwrap();
        assert_eq!(
            store
                .confirm_prepared_note_handoffs(&["exact-note-id".to_string()])
                .await
                .unwrap(),
            1
        );
        store.set_reconcile_cursor(11).await.unwrap();
        assert!(
            !store
                .clear_expired_prepared_note_handoff("0xtx", "commitment")
                .await
                .unwrap()
        );
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
    // The processed-set + deposit_count tracker, exercised through its sole
    // write path (`commit_b2agg_event_atomic`): distinct notes get sequential
    // deposit_counts, and each becomes visible to `is_note_processed`.
    async fn test_bridge_out_tracker() {
        let store = InMemoryStore::new();
        assert!(!store.is_note_processed("note1").await.unwrap());
        let c = store
            .commit_b2agg_event_atomic(
                "note1".to_string(),
                "0xbridge",
                1,
                [0xaa; 32],
                "0xtx1",
                0,
                1,
                &[0u8; 20],
                0,
                &[0xcc; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();
        assert_eq!(c, 0);
        assert!(store.is_note_processed("note1").await.unwrap());
        let c2 = store
            .commit_b2agg_event_atomic(
                "note2".to_string(),
                "0xbridge",
                2,
                [0xab; 32],
                "0xtx2",
                0,
                1,
                &[0u8; 20],
                0,
                &[0xcc; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();
        assert_eq!(c2, 1);
    }

    #[tokio::test]
    // Audit H1 — `commit_b2agg_event_atomic` must be a single all-or-nothing
    // operation that is also idempotent on retry: re-running it for an
    // already-committed note reuses the original deposit_count, does NOT bump
    // the counter, and does NOT emit a duplicate BridgeEvent.
    async fn h1_commit_b2agg_event_atomic_is_idempotent_on_retry() {
        use crate::log_synthesis::{BRIDGE_EVENT_TOPIC, LogFilter};

        let store = InMemoryStore::new();
        let note = "0xb2agg-note-1".to_string();
        let block = 10u64;

        let dc1 = store
            .commit_b2agg_event_atomic(
                note.clone(),
                "0xbridge",
                block,
                [0xaa; 32],
                "0xtx1",
                0,
                1,
                &[0u8; 20],
                0,
                &[0xcc; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();

        // Retry — simulates a re-projection after a crash before the txn
        // committed. The contract: same deposit_count, no duplicate event.
        let dc2 = store
            .commit_b2agg_event_atomic(
                note.clone(),
                "0xbridge",
                block,
                [0xaa; 32],
                "0xtx1",
                0,
                1,
                &[0u8; 20],
                0,
                &[0xcc; 20],
                1_000,
                &[0u8; 0],
            )
            .await
            .unwrap();

        assert_eq!(dc1, dc2, "retry must reuse the same deposit_count");
        assert_eq!(
            store.get_deposit_count().await.unwrap(),
            1,
            "counter must not advance on retry"
        );
        assert!(
            store.is_note_processed(&note).await.unwrap(),
            "note stays marked processed"
        );

        // Exactly one BridgeEvent log in the store.
        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xffff).await.unwrap();
        let bridge_logs: Vec<_> = logs
            .iter()
            .filter(|l| l.topics.first().is_some_and(|t| t == BRIDGE_EVENT_TOPIC))
            .collect();
        assert_eq!(
            bridge_logs.len(),
            1,
            "retry must not emit a duplicate BridgeEvent"
        );
    }

    #[tokio::test]
    async fn test_ger_dedup() {
        let store = InMemoryStore::new();
        let ger = [0x11; 32];
        assert!(!store.has_seen_ger(&ger).await.unwrap());
        store
            .commit_ger_event_atomic(0, [0u8; 32], "0xTx1", &ger, None, None, 0)
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
            .commit_ger_event_atomic(0, [0u8; 32], "0xTx1", &ger1, None, None, 0)
            .await
            .unwrap();
        let hash1 = *store.hash_chain_value.read();

        store
            .commit_ger_event_atomic(1, [1u8; 32], "0xTx2", &ger2, None, None, 0)
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

    /// Cantina finding #12 (redesign) — `get_logs` returns ALL matches with NO
    /// row cap. The ORIGINAL fix capped at 1000 raw rows and errored past it; the
    /// redesign pushes filtering into a SAFE SUPERSET and reads the whole set, so
    /// a dense block of matching logs comes back in full — not truncated, not
    /// errored.
    #[tokio::test]
    async fn finding_12_getlogs_returns_all_no_row_cap() {
        let store = InMemoryStore::new();
        let block = 4_242u64;
        let n = 2_500usize; // comfortably past the OLD 1000-row cap
        for i in 0..n {
            store
                .add_log(SyntheticLog {
                    log_index: 0,
                    address: "0xdead".to_string(),
                    topics: vec!["0xabcd".to_string()],
                    data: "0x".to_string(),
                    block_number: block,
                    block_hash: [0u8; 32],
                    transaction_hash: format!("0xf12_{i}"),
                    transaction_index: 0,
                    removed: false,
                })
                .await
                .unwrap();
        }

        let filter = LogFilter {
            from_block: Some(format!("0x{block:x}")),
            to_block: Some(format!("0x{block:x}")),
            ..Default::default()
        };
        let logs = store
            .get_logs(&filter, block)
            .await
            .expect("no row cap: a dense range must return ALL matches, not error");
        assert_eq!(logs.len(), n, "every matching log must be returned in full");
    }

    /// Cantina #12 GUARDRAIL — property-based equivalence. For a diverse
    /// population of SyntheticLogs and a diverse set of LogFilters,
    /// `InMemoryStore::get_logs` MUST equal the pure-Rust `matches()` oracle over
    /// the whole population. This is the correctness proof independent of any
    /// store internals. Explicit non-trivial cases asserted on top of the blanket
    /// equivalence:
    ///   (a) sparse match in a DENSE range — the old cap would have errored,
    ///   (b) MA#26 UHCV passthrough BOTH ways (topic0 includes UHCV ⇒ returned;
    ///       no topic0 / other topic0 ⇒ NOT returned),
    ///   (c) positional topics with wildcards + multi-alternatives + a filter
    ///       longer than the log's topics ⇒ reject.
    #[tokio::test]
    async fn getlogs_equivalence_matches_oracle_inmemory() {
        use crate::log_synthesis::equiv_fixtures::{
            DENSE_FILLER, SPARSE_MATCH_COUNT, Scenario, sorted_txs,
        };

        const {
            assert!(
                DENSE_FILLER > 1000,
                "sparse-in-dense case must exceed the OLD 1000-row cap to be meaningful"
            );
        }

        let store = InMemoryStore::new();
        let scn = Scenario::new(0, 0x_A11CE);
        for l in &scn.logs {
            store.add_log(l.clone()).await.unwrap();
        }

        for (name, f) in &scn.filters {
            let got = store
                .get_logs(f, scn.current_block)
                .await
                .unwrap_or_else(|e| panic!("filter `{name}`: get_logs errored: {e}"));
            let want = scn.reference_matches(f);
            assert_eq!(
                sorted_txs(&got),
                sorted_txs(&want),
                "filter `{name}`: store result diverged from matches() oracle"
            );

            // eth_getLogs ordering contract — results must be ordered by
            // (block_number, log_index). Exercised especially by the block_hash
            // filter, whose in-memory path scans an unordered HashMap.
            assert!(
                got.windows(2)
                    .all(|w| (w[0].block_number, w[0].log_index)
                        <= (w[1].block_number, w[1].log_index)),
                "filter `{name}`: results must be ordered by (block_number, log_index)"
            );

            let got_txs = sorted_txs(&got);
            let has = |tx: &str| got_txs.iter().any(|t| t == tx);
            match *name {
                "sparse_in_dense" => {
                    assert_eq!(
                        got.len(),
                        SPARSE_MATCH_COUNT,
                        "dense range must return exactly the sparse matches"
                    );
                    for tx in &scn.sparse_match_txs {
                        assert!(has(tx), "sparse match {tx} missing");
                    }
                }
                "passthrough_include" => assert!(
                    has(&scn.tx_passthrough),
                    "UHCV passthrough must be returned when the query's topic0 includes UHCV"
                ),
                "passthrough_exclude_no_topic" => assert!(
                    !has(&scn.tx_passthrough),
                    "no topic0 filter ⇒ passthrough must NOT leak the UHCV log"
                ),
                "passthrough_exclude_other_topic" => assert!(
                    !has(&scn.tx_passthrough),
                    "topic0 excludes UHCV ⇒ passthrough must NOT return the UHCV log"
                ),
                "positional_longer_than_log" => {
                    assert!(
                        !has(&scn.tx_positional_short),
                        "filter constrains topic position 2 but the log has only 2 topics ⇒ reject"
                    );
                    assert!(
                        has(&scn.tx_positional_long),
                        "len-3 log with matching positional topics ⇒ accept"
                    );
                }
                _ => {}
            }
        }
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

    /// PR #127 follow-up — the memory/postgres `txn_commit` contract:
    /// finalising a transaction that has no `txn_begin` row is an ERROR, not
    /// a silent no-op. The PgStore twin lives in
    /// `postgres_tests::test_pgstore_txn_commit_missing_row_errors`; the two
    /// stores must behave identically so a projector racing a submitter can
    /// never "finalise" zero rows and leave a late-begun receipt pending
    /// forever.
    #[tokio::test]
    async fn test_txn_commit_missing_row_errors() {
        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([0x77u8; 32]);
        let err = store
            .txn_commit(tx_hash, Ok(()), 42, [0u8; 32])
            .await
            .expect_err("txn_commit without a prior txn_begin must error");
        assert!(
            err.to_string().contains("not found"),
            "error must identify the missing row, got: {err:#}"
        );
        // And it must not have invented a receipt.
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());
    }

    /// Build a minimal pending `txn_begin` entry for the CAS tests below.
    async fn seed_pending_txn(store: &InMemoryStore, tx_hash: TxHash) {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;
        let envelope = alloy::consensus::TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            Signature::test_signature(),
            tx_hash,
        ));
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
    }

    /// BLOCKER 2 (success-always-wins CAS) — a SUCCESS receipt must survive a
    /// later failure commit. Models the TTL-sweeper race: the worker commits
    /// SUCCESS (status 0x1) first, then the sweeper's `write_failure_receipt`
    /// fires `txn_commit(Err)` for the same hash. Pre-fix the Err overwrote the
    /// success (status 0x1 → 0x0) and aggkit resubmitted a Miden op that had
    /// already landed. The success must be preserved verbatim.
    ///
    /// Mutation check: dropping the `(Some(Ok(_)), _) => Cas::NoOp` arm in
    /// `txn_commit` makes this assertion fail (the failure clobbers success).
    #[tokio::test]
    async fn test_txn_commit_terminal_success_not_clobbered_by_failure() {
        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([0x51u8; 32]);
        seed_pending_txn(&store, tx_hash).await;

        // Worker commits SUCCESS at block 7.
        store
            .txn_commit(tx_hash, Ok(()), 7, [0xAAu8; 32])
            .await
            .unwrap();

        // TTL sweeper races in with a failure commit for the same hash.
        store
            .txn_commit(
                tx_hash,
                Err("TTL expired (>300s in non-terminal state)".to_string()),
                9,
                [0xBBu8; 32],
            )
            .await
            .expect("late failure commit must be an accepted no-op, not an error");

        // The landed success is preserved: status stays 0x1, block unchanged.
        let (res, block_num) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(
            res.is_ok(),
            "first terminal (success) must win; got failure: {res:?}"
        );
        assert_eq!(block_num, 7, "success block must be preserved");
    }

    /// BLOCKER 2 (success-always-wins CAS) — a REAL Miden landing supersedes a
    /// prior (TTL/timeout) FAILURE, and the ClaimEvent the failure suppressed is
    /// re-materialised. Models the reverse race: the TTL sweeper commits a
    /// terminal FAILURE for a job whose worker is still running; Miden then
    /// LANDS; the projector's later `txn_commit(Ok)` must win so the durable
    /// receipt ends SUCCESS (status 0x1) WITH its ClaimEvent — never a stuck
    /// TTL-failure for a claim that actually landed.
    ///
    /// Mutation check: revert the override (make `(Some(Err(_)), true)` a
    /// `Cas::NoOp` / first-terminal-wins) → the receipt stays failed and the
    /// ClaimEvent is missing, failing both assertions.
    #[tokio::test]
    async fn test_txn_commit_success_supersedes_prior_failure_with_claimevent() {
        use alloy::primitives::{B256, Bytes, LogData};
        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([0x52u8; 32]);

        // Pending row carrying a ClaimEvent-shaped attached log.
        let claim_topic = B256::from([0xC1u8; 32]);
        let envelope =
            alloy::consensus::TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
                alloy::consensus::TxLegacy::default(),
                alloy::primitives::Signature::test_signature(),
                tx_hash,
            ));
        store
            .txn_begin(
                tx_hash,
                TxnEntry {
                    id: None,
                    envelope,
                    signer: Address::ZERO,
                    expires_at: None,
                    logs: vec![LogData::new_unchecked(
                        vec![claim_topic],
                        Bytes::from(vec![0xAB]),
                    )],
                },
            )
            .await
            .unwrap();

        // TTL sweeper fails the still-running job first.
        store
            .txn_commit(tx_hash, Err("TTL expired".to_string()), 3, [0u8; 32])
            .await
            .unwrap();
        assert!(
            store
                .get_logs_for_tx(&format!("{tx_hash:#x}"))
                .await
                .unwrap()
                .is_empty(),
            "a failure must NOT materialise the ClaimEvent"
        );

        // Miden actually landed → the projector commits success for the SAME hash.
        store
            .txn_commit(tx_hash, Ok(()), 5, [0u8; 32])
            .await
            .expect("a real landing must supersede the provisional failure");

        let (res, block_num) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(
            res.is_ok(),
            "success must supersede the TTL failure; got {res:?}"
        );
        assert_eq!(block_num, 5, "success block must win");
        let logs = store
            .get_logs_for_tx(&format!("{tx_hash:#x}"))
            .await
            .unwrap();
        assert_eq!(
            logs.len(),
            1,
            "the ClaimEvent the failure suppressed must be materialised on the success override"
        );
        assert_eq!(logs[0].topics[0], format!("{claim_topic:#x}"));
    }

    #[tokio::test]
    async fn handoff_blocks_failure_receipt_until_authoritative_clear() {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;

        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([0x78u8; 32]);
        let tx_key = format!("{tx_hash:#x}");
        let envelope = alloy::consensus::TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            Signature::test_signature(),
            tx_hash,
        ));
        store
            .txn_begin(
                tx_hash,
                TxnEntry {
                    id: None,
                    envelope,
                    signer: Address::ZERO,
                    expires_at: Some(10),
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        store
            .prepare_note_handoff(&tx_key, "commitment", "note-id", 10)
            .await
            .unwrap();

        store
            .txn_commit(tx_hash, Err("ambiguous".into()), 10, [0; 32])
            .await
            .unwrap();
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());

        store.set_reconcile_cursor(11).await.unwrap();
        assert!(
            store
                .clear_expired_prepared_note_handoff(&tx_key, "commitment")
                .await
                .unwrap()
        );
        store
            .txn_commit(tx_hash, Err("definitive".into()), 11, [0; 32])
            .await
            .unwrap();
        assert!(
            store
                .txn_receipt(tx_hash)
                .await
                .unwrap()
                .unwrap()
                .0
                .is_err()
        );
    }

    #[tokio::test]
    async fn confirmed_duplicate_finalizes_linked_pending_without_event() {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;

        let store = InMemoryStore::new();
        let tx_hash = TxHash::from([0x79u8; 32]);
        let tx_key = format!("{tx_hash:#x}");
        let envelope = alloy::consensus::TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy::default(),
            Signature::test_signature(),
            tx_hash,
        ));
        store
            .txn_begin(
                tx_hash,
                TxnEntry {
                    id: None,
                    envelope,
                    signer: Address::ZERO,
                    expires_at: Some(10),
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        store
            .prepare_note_handoff(&tx_key, "commitment", "note-id", 10)
            .await
            .unwrap();
        store
            .txn_commit(tx_hash, Err("raw Miden error".into()), 10, [0; 32])
            .await
            .unwrap();
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());

        store
            .txn_commit_confirmed_duplicate(
                tx_hash,
                Err("execution reverted: AlreadyClaimed()".into()),
                11,
            )
            .await
            .unwrap();
        let (result, block) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(result.is_err());
        assert_eq!(block, 11);
        assert!(
            store
                .txn_get(tx_hash)
                .await
                .unwrap()
                .unwrap()
                .logs
                .is_empty()
        );
        assert!(store.get_logs_for_tx(&tx_key).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_address_mappings() {
        let store = InMemoryStore::new();
        let addr = Address::from([42u8; 20]);
        assert!(store.get_address_mapping(&addr).await.unwrap().is_none());

        let miden_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
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
        store
            .commit_ger_event_atomic(0, [0u8; 32], "0xInjTx", &ger, None, None, 0)
            .await
            .unwrap();
        assert!(store.is_ger_injected(&ger).await.unwrap());
    }

    #[tokio::test]
    // Audit H2 — `commit_ger_event_atomic` must be idempotent: re-running it
    // for an already-emitted GER must NOT roll the hash chain a second time or
    // emit a duplicate UpdateHashChainValue log. The legacy two-step "roll
    // chain + emit log" then "mark injected" sequence could leave
    // is_injected=FALSE after the roll had committed, so the projector re-rolled
    // the chain on retry, diverging the proxy's hash_chain_value from aggkit.
    // Folding both into one atomic call closes that window.
    async fn h2_commit_ger_event_atomic_is_idempotent_on_retry() {
        let store = InMemoryStore::new();
        let ger = [0x55u8; 32];

        store
            .commit_ger_event_atomic(10, [0xaa; 32], "0xger-tx-1", &ger, None, None, 1000)
            .await
            .unwrap();
        let chain_after_first = *store.hash_chain_value.read();
        assert!(store.is_ger_injected(&ger).await.unwrap());

        // Retry — simulates a re-projection after a crash before the txn
        // committed. The contract: same hash_chain_value, no duplicate log,
        // GER stays injected.
        store
            .commit_ger_event_atomic(10, [0xaa; 32], "0xger-tx-1", &ger, None, None, 1000)
            .await
            .unwrap();

        let chain_after_retry = *store.hash_chain_value.read();
        assert_eq!(
            chain_after_first, chain_after_retry,
            "retry must NOT roll the hash chain a second time"
        );

        // Exactly one UpdateHashChainValue log emitted.
        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xffff).await.unwrap();
        let ger_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics
                    .first()
                    .is_some_and(|t| t == UPDATE_HASH_CHAIN_VALUE_TOPIC)
            })
            .collect();
        assert_eq!(ger_logs.len(), 1, "retry must NOT emit a duplicate GER log");
    }

    #[tokio::test]
    // Audit H2 — the idempotency gate must be CASE-INSENSITIVE on tx_hash.
    // transaction_hash is canonically lowercase hex across the store, so a
    // retry that arrives with a differently-cased form of the SAME hash must
    // still be recognized as already-emitted — otherwise the chain re-rolls
    // and a duplicate UpdateHashChainValue log is emitted (double-emit).
    async fn h2_commit_ger_event_atomic_is_idempotent_case_insensitive() {
        let store = InMemoryStore::new();
        let ger = [0x66u8; 32];

        // First commit with an UPPER/mixed-case tx_hash.
        store
            .commit_ger_event_atomic(10, [0xbb; 32], "0xDeadBEEF01", &ger, None, None, 1000)
            .await
            .unwrap();
        let chain_after_first = *store.hash_chain_value.read();
        assert!(store.is_ger_injected(&ger).await.unwrap());

        // Retry with the SAME hash in a different case (all lowercase).
        store
            .commit_ger_event_atomic(10, [0xbb; 32], "0xdeadbeef01", &ger, None, None, 1000)
            .await
            .unwrap();

        let chain_after_retry = *store.hash_chain_value.read();
        assert_eq!(
            chain_after_first, chain_after_retry,
            "differently-cased retry must NOT roll the hash chain a second time"
        );

        // Exactly one UpdateHashChainValue log across both casings.
        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xffff).await.unwrap();
        let ger_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics
                    .first()
                    .is_some_and(|t| t == UPDATE_HASH_CHAIN_VALUE_TOPIC)
            })
            .collect();
        assert_eq!(
            ger_logs.len(),
            1,
            "differently-cased retry must NOT emit a duplicate GER log"
        );
    }

    #[tokio::test]
    async fn test_faucet_registry() {
        let store = InMemoryStore::new();
        let faucet_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

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
                metadata: vec![],
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
                metadata: vec![],
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
        let usdc_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

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
                metadata: vec![],
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

    /// Finding #10 — repro+regression. A second `register_faucet` for an origin
    /// already owned by a *different* faucet must CONVERGE (first-write wins)
    /// rather than strand a second row. Pre-fix InMemoryStore silently pushed a
    /// colliding row (split state, mirroring PgStore's unique-index error), so
    /// the bridge could route by a faucet the local registry never resolved,
    /// hiding later bridge-outs.
    #[tokio::test]
    async fn finding_10_register_faucet_converges_on_origin_collision() {
        let store = InMemoryStore::new();
        let origin = [0xC0u8; 20];

        let faucet_a = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let faucet_b = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        // Worker A wins the race and registers first.
        store
            .register_faucet(FaucetEntry {
                faucet_id: faucet_a,
                origin_address: origin,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();

        // Worker B loses: a DIFFERENT faucet for the SAME (origin, network).
        // Post-fix this converges — no error, no second row.
        store
            .register_faucet(FaucetEntry {
                faucet_id: faucet_b,
                origin_address: origin,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();

        // Exactly one row survives — the first-writer's faucet.
        assert_eq!(store.list_faucets().await.unwrap().len(), 1);
        let by_origin = store
            .get_faucet_by_origin(&origin, 0)
            .await
            .unwrap()
            .expect("origin route must resolve");
        assert_eq!(by_origin.faucet_id, faucet_a, "first-write must win");

        // The losing faucet is NOT stranded in the registry (it was never
        // deployed on Miden in the real flow because the single-flight coordinator
        // makes a concurrent first-claim an AWAITER that reuses faucet A instead of
        // provisioning a second). resolve-by-id for the canonical faucet works; the
        // loser resolves to nothing.
        assert!(store.get_faucet_by_id(faucet_a).await.unwrap().is_some());
        assert!(store.get_faucet_by_id(faucet_b).await.unwrap().is_none());

        // Same faucet re-registering still refreshes its metadata (idempotent
        // by faucet_id) — the convergence must not regress this.
        store
            .register_faucet(FaucetEntry {
                faucet_id: faucet_a,
                origin_address: origin,
                origin_network: 0,
                symbol: "WTKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .get_faucet_by_id(faucet_a)
                .await
                .unwrap()
                .unwrap()
                .symbol,
            "WTKN"
        );
        assert_eq!(store.list_faucets().await.unwrap().len(), 1);
    }

    /// InMemoryStore::register_faucet must match PgStore's faucet_id-idempotent
    /// guards (Copilot review): a re-register by the same faucet_id must
    ///   (a) NEVER wipe existing non-empty metadata with an empty vec (Cantina
    ///       #13 — the preimage a later bridge-out needs), and
    ///   (b) REJECT an attempt to rebind the faucet_id to a different origin
    ///       (PgStore hits a duplicate faucet_id primary-key error there).
    #[tokio::test]
    async fn register_faucet_faucet_id_reregister_matches_pgstore_guards() {
        let store = InMemoryStore::new();
        let origin = [0xD1u8; 20];
        let faucet_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();

        // Initial registration carries real (non-empty) metadata.
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: origin,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![0xAB, 0xCD, 0xEF],
            })
            .await
            .unwrap();

        // (a) A blank re-register (empty metadata) refreshes symbol but must
        //     PRESERVE the stored metadata preimage.
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: origin,
                origin_network: 0,
                symbol: "WTKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();
        let after = store.get_faucet_by_id(faucet_id).await.unwrap().unwrap();
        assert_eq!(after.symbol, "WTKN", "mutable fields refresh");
        assert_eq!(
            after.metadata,
            vec![0xAB, 0xCD, 0xEF],
            "empty re-register must NOT wipe stored metadata (Cantina #13)"
        );

        // A non-empty re-register DOES overwrite the metadata.
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: origin,
                origin_network: 0,
                symbol: "WTKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![0x11, 0x22],
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .get_faucet_by_id(faucet_id)
                .await
                .unwrap()
                .unwrap()
                .metadata,
            vec![0x11, 0x22],
            "non-empty re-register overwrites metadata"
        );

        // (b) Rebinding the same faucet_id to a DIFFERENT origin is rejected.
        let err = store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: [0xD2u8; 20],
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .expect_err("rebinding a faucet_id to a new origin must be rejected");
        assert!(
            format!("{err:#}").contains("refusing to rebind"),
            "error must name the origin-rebind refusal, got: {err:#}"
        );
        // A different origin_network for the same faucet_id is likewise rejected.
        store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address: origin,
                origin_network: 7,
                symbol: "TKN".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .expect_err("rebinding to a new origin_network must be rejected");

        // Nothing leaked a second row.
        assert_eq!(store.list_faucets().await.unwrap().len(), 1);
    }
}
