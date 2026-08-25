//! Client-side pacing for outbound Miden node RPC.
//!
//! # Why
//!
//! The node rate-limits callers. The vendored client's retry layer treats
//! `ResourceExhausted` as retryable and re-sends after 100ms, up to
//! `DEFAULT_MAX_RETRIES`, which means throttling is *survived* rather than
//! *avoided*: a recovery scan over a deep chain issues two requests per block as
//! fast as the loop can drive them, collects thousands of retry warnings, and
//! still does the same work — just with the node pushing back the whole way. Any
//! burst that outlasts the retry budget surfaces as a hard error instead.
//!
//! Pacing turns that into a first-class setting: tell the proxy how fast the RPC
//! endpoint will let it go, and it spaces its own calls to stay under that. A
//! recovery takes longer, which is the correct trade — the alternative is
//! hammering a node that is already saying no.
//!
//! # Why a configured rate, not a discovered one
//!
//! `NodeRpcClient::get_rpc_limits` exists but returns *batch-size* limits
//! (`note_ids_limit`, `nullifiers_limit`, …) — how many items may ride in one
//! request. It carries no requests-per-second figure, and nothing else on the
//! wire advertises one. So the ceiling has to come from the operator, who knows
//! what their gateway is provisioned for.
//!
//! # Why a decorator over the trait
//!
//! `build_rpc_client` is the single place every component gets its node handle:
//! restore, the synthetic projector, the persistent client, and both CLI tools.
//! Wrapping there paces all of them without touching a call site, and without
//! growing the vendored-client patch (which is deliberately one file, one hunk).
//!
//! Only the trait's *required* methods are implemented here. The provided ones
//! (`get_account_details`, `get_note_by_id`, `get_public_note_records`, …) are
//! defined in terms of the required ones, so they inherit pacing through the
//! primitives they call — and are NOT overridden, which would double-count one
//! logical request as several.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use miden_client::rpc::domain::account::{AccountProof, GetAccountRequest};
use miden_client::rpc::domain::account_vault::AccountVaultInfo;
use miden_client::rpc::domain::note::{FetchedNote, SyncNotesBlock};
use miden_client::rpc::domain::nullifier::NullifierUpdate;
use miden_client::rpc::domain::storage_map::StorageMapInfo;
use miden_client::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use miden_client::rpc::domain::transaction::TransactionRecord;
use miden_client::rpc::encryption::{AttestedTransactionEncryptionKey, SealedTransactionInputs};
use miden_client::rpc::{NetworkNoteStatusInfo, NodeRpcClient, RpcError, RpcLimits, RpcStatusInfo};
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::note::{NoteId, NoteScript, NoteTag};
use miden_protocol::transaction::ProvenTransaction;
use std::collections::BTreeSet;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// How many requests may be issued back-to-back after an idle period before
/// spacing kicks in. A small burst keeps latency-sensitive single calls (a
/// health probe, one claim submission) from paying a pacing delay, while a
/// sustained scan still converges to the configured rate.
const DEFAULT_BURST: u32 = 8;

/// Process-wide pace, installed once from the CLI flag. `None` = unpaced.
///
/// A global rather than a parameter because `build_rpc_client` is called from
/// eight places including two standalone binaries that have no access to the
/// serving process's parsed `Command`. Follows the existing
/// `BRIDGE_ADDRESS_CACHE` precedent.
static RPC_MAX_RPS: OnceLock<Option<u32>> = OnceLock::new();

/// Installs the configured ceiling. Idempotent-by-first-write: a second call is
/// ignored, so a test or a re-entrant init cannot silently change the pace of an
/// already-running process.
pub fn install_max_rps(max_rps: Option<u32>) {
    let _ = RPC_MAX_RPS.set(max_rps.filter(|rps| *rps > 0));
}

/// The effective ceiling for this process.
///
/// Falls back to the environment when nothing was installed, so the
/// `bridge-out-tool` and `note-probe` binaries — which parse their own args and
/// never see the service's flags — still honour `MIDEN_RPC_MAX_RPS`. A value of
/// `0` or an unparsable value means unpaced, matching the flag's semantics.
pub fn effective_max_rps() -> Option<u32> {
    if let Some(installed) = RPC_MAX_RPS.get() {
        return *installed;
    }
    std::env::var("MIDEN_RPC_MAX_RPS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|rps| *rps > 0)
}

/// A rate governor: at most `rps` acquisitions per second, tolerating a short
/// burst.
///
/// Implemented as a virtual-scheduling clock (GCRA). Each acquisition claims the
/// next slot and advances the cursor by one interval; a caller whose slot is
/// more than `tolerance` in the future sleeps until it comes due. Claiming the
/// slot happens under the lock and sleeping happens outside it, so concurrent
/// callers queue in arrival order instead of all waking to race for the same
/// slot.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    tolerance: Duration,
    /// Theoretical arrival time of the next request.
    cursor: Mutex<Option<Instant>>,
}

impl Pacer {
    /// `rps` must be non-zero; callers get `None` for "no pacing" instead of a
    /// zero rate, so that a misconfigured `0` can never mean "one request per
    /// eternity".
    pub fn new(rps: u32, burst: u32) -> Option<Self> {
        if rps == 0 {
            return None;
        }
        let interval = Duration::from_secs_f64(1.0 / f64::from(rps));
        Some(Self {
            interval,
            // `burst - 1`, not `burst`: the first call is always free (its slot
            // IS now), so N-1 intervals of tolerance buy exactly N back-to-back
            // calls. Using `burst` here yields N+1, which is how the unit test
            // below caught this. Saturating, so burst 0 and 1 both mean strict
            // spacing rather than an underflowed, effectively-infinite credit.
            tolerance: interval.saturating_mul(burst.saturating_sub(1)),
            cursor: Mutex::new(None),
        })
    }

    /// Waits until this caller's slot comes due. Returns how long it waited, for
    /// metrics.
    pub async fn acquire(&self) -> Duration {
        let now = Instant::now();
        let due = {
            let mut cursor = self.cursor.lock().await;
            // An idle gap resets the clock to now rather than letting unused
            // slots accumulate into an unbounded burst credit.
            let slot = cursor.map_or(now, |c| if c < now { now } else { c });
            *cursor = Some(slot + self.interval);
            // Within the burst tolerance the request goes immediately.
            slot.checked_sub(self.tolerance).unwrap_or(now).max(now)
        };
        let waited = due.saturating_duration_since(now);
        if !waited.is_zero() {
            tokio::time::sleep_until(due).await;
        }
        waited
    }
}

/// Wraps a node RPC client so every outbound request passes a [`Pacer`] first.
///
/// No `Debug` derive: `dyn NodeRpcClient` is not `Debug`, and a hand-written
/// impl would only be able to print the pacer anyway.
pub struct PacedRpcClient {
    inner: Arc<dyn NodeRpcClient>,
    pacer: Pacer,
}

impl PacedRpcClient {
    pub fn new(inner: Arc<dyn NodeRpcClient>, rps: u32) -> Option<Self> {
        Pacer::new(rps, DEFAULT_BURST).map(|pacer| Self { inner, pacer })
    }

    async fn gate(&self) {
        let waited = self.pacer.acquire().await;
        ::metrics::counter!("miden_rpc_paced_requests_total").increment(1);
        if !waited.is_zero() {
            ::metrics::counter!("miden_rpc_pace_wait_seconds_total")
                .increment(waited.as_micros() as u64);
        }
    }
}

#[async_trait::async_trait]
impl NodeRpcClient for PacedRpcClient {
    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError> {
        // Local state, not a wire call — deliberately unpaced.
        self.inner.set_genesis_commitment(commitment).await
    }

    fn has_genesis_commitment(&self) -> Option<Word> {
        self.inner.has_genesis_commitment()
    }

    async fn get_transaction_encryption_key(
        &self,
    ) -> Result<AttestedTransactionEncryptionKey, RpcError> {
        self.gate().await;
        self.inner.get_transaction_encryption_key().await
    }

    async fn submit_proven_transaction(
        &self,
        proven_transaction: ProvenTransaction,
        sealed_transaction_inputs: SealedTransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        self.gate().await;
        self.inner
            .submit_proven_transaction(proven_transaction, sealed_transaction_inputs)
            .await
    }

    async fn submit_proven_batch(
        &self,
        proven_batch: ProvenBatch,
        proposed_batch: ProposedBatch,
        transaction_inputs: Vec<SealedTransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        self.gate().await;
        self.inner
            .submit_proven_batch(proven_batch, proposed_batch, transaction_inputs)
            .await
    }

    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        self.gate().await;
        self.inner
            .get_block_header_by_number(block_num, include_mmr_proof)
            .await
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<ProvenBlock, RpcError> {
        self.gate().await;
        self.inner
            .get_block_by_number(block_num, include_proof)
            .await
    }

    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        self.gate().await;
        self.inner.get_notes_by_id(note_ids).await
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        self.gate().await;
        self.inner
            .sync_chain_mmr(current_block_height, upper_bound)
            .await
    }

    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<SyncNotesBlock>, RpcError> {
        self.gate().await;
        self.inner.sync_notes(block_from, block_to, note_tags).await
    }

    async fn sync_nullifiers(
        &self,
        prefix: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        self.gate().await;
        self.inner
            .sync_nullifiers(prefix, block_from, block_to)
            .await
    }

    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        self.gate().await;
        self.inner.get_account(account_id, request).await
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        self.gate().await;
        self.inner.get_note_script_by_root(root).await
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        self.gate().await;
        self.inner
            .sync_storage_maps(block_from, block_to, account_id)
            .await
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        self.gate().await;
        self.inner
            .sync_account_vault(block_from, block_to, account_id)
            .await
    }

    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        self.gate().await;
        self.inner
            .sync_transactions(block_from, block_to, account_ids)
            .await
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        self.gate().await;
        self.inner.get_network_id().await
    }

    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
        self.gate().await;
        self.inner.get_rpc_limits().await
    }

    fn has_rpc_limits(&self) -> Option<RpcLimits> {
        self.inner.has_rpc_limits()
    }

    async fn set_rpc_limits(&self, limits: RpcLimits) {
        // Local state, not a wire call — deliberately unpaced.
        self.inner.set_rpc_limits(limits).await;
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        self.gate().await;
        self.inner.get_status_unversioned().await
    }

    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        self.gate().await;
        self.inner.get_network_note_status(note_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rps_is_unpaced_not_infinitely_slow() {
        assert!(
            Pacer::new(0, DEFAULT_BURST).is_none(),
            "rps=0 must mean 'no pacing', never a zero-rate governor that stalls forever"
        );
    }

    #[test]
    fn effective_rate_rejects_zero_and_garbage() {
        // `install_max_rps` filters the disabled sentinel, so a configured 0 is
        // indistinguishable from "flag absent" — the safe reading.
        assert_eq!(None, Some(0u32).filter(|rps| *rps > 0));
        assert_eq!(Some(50), Some(50u32).filter(|rps| *rps > 0));
        assert_eq!(None, "".parse::<u32>().ok().filter(|rps| *rps > 0));
        assert_eq!(None, "banana".parse::<u32>().ok().filter(|rps| *rps > 0));
    }

    #[tokio::test(start_paused = true)]
    async fn burst_passes_immediately_then_spacing_applies() {
        let pacer = Pacer::new(10, 4).expect("non-zero rps");
        for i in 0..4 {
            assert!(
                pacer.acquire().await.is_zero(),
                "call {i} is within the burst tolerance and must not wait"
            );
        }
        // The 5th call has exhausted the burst credit and must be spaced by one
        // interval (1/10s).
        let waited = pacer.acquire().await;
        assert_eq!(
            waited,
            Duration::from_millis(100),
            "past the burst, calls are spaced at exactly the configured interval"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_rate_converges_to_the_configured_ceiling() {
        let pacer = Pacer::new(20, 0).expect("non-zero rps");
        let start = Instant::now();
        for _ in 0..20 {
            pacer.acquire().await;
        }
        // 20 requests at 20/s with no burst credit occupy 19 intervals of 50ms
        // (the first is free), i.e. the rate is held rather than exceeded.
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(950),
            "a sustained run must not outpace the configured rate"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_does_not_accumulate_unbounded_burst_credit() {
        let pacer = Pacer::new(10, 2).expect("non-zero rps");
        pacer.acquire().await;
        // Go idle far longer than the tolerance.
        tokio::time::sleep(Duration::from_secs(60)).await;
        // The cursor resets to now, so exactly `burst` calls are free again —
        // not 600 slots' worth.
        assert!(pacer.acquire().await.is_zero());
        assert!(pacer.acquire().await.is_zero());
        assert_eq!(
            pacer.acquire().await,
            Duration::from_millis(100),
            "credit is capped at the burst, so a long idle cannot license a flood"
        );
    }
}
