//! #156 — automatic recovery of acknowledged pending/unlinked transactions.
//!
//! A transaction can be durably admitted by the proxy — its pending row written,
//! the signer nonce CAS-advanced, and the RPC hash returned — while the actual
//! writer job exists only in the in-memory Tokio queue. A crash, a Miden outage,
//! or a clean shutdown that drops buffered jobs before a durable Miden handoff can
//! then leave the row `pending` with no `miden_tx_id` and no submitted note
//! handoff. The durable pending-nonce frontier correctly blocks every later nonce,
//! but nothing automatically resumes the lower transaction — so without this
//! module the only recovery is the upstream client re-submitting the exact signed
//! envelope, which an AggKit-style manager may have already evicted.
//!
//! The signed envelope in the `transactions` row IS the recovery source of truth.
//! On startup and on a bounded periodic sweep, [`recover_orphaned_pending_txns`]
//! walks each signer's durable pending transactions in nonce order and drives each
//! to a durable outcome without any client action:
//!
//! - **Live writer job** — leave it with the active worker.
//! - **Effect already applied in Miden** (GER injected / global index claimed) —
//!   finalise the original hash with a terminal success receipt (recovers a lost
//!   local receipt without re-submitting).
//! - **Handoff recorded but effect not yet observed** — the intent crossed the
//!   external submission boundary; poll it with backoff, never blindly re-submit.
//! - **No handoff, no effect** — an orphaned durable intent; re-enqueue its writer
//!   job (rebuilt from the stored envelope) directly, with persistent exponential
//!   backoff. The pending row and nonce CAS are already durable, so this is the
//!   exact post-admission work — it advances no nonce and recovers several orphans
//!   for one signer in nonce order.
//!
//! Delivery is at-least-once with state reconciliation: every re-drive checks
//! authoritative Miden state first, the re-enqueued job reuses the writer's
//! handoff fencing, and Miden duplicate-protection is the final safety boundary,
//! so recovery cannot double-advance a nonce or duplicate a GER/claim. Multi-
//! replica coordination is out of scope (#142).

use crate::service_send_raw_txn::decode_write_call;
use crate::service_state::ServiceState;
use crate::store::RecoverablePendingTxn;
use crate::writer_worker::DecodedWriteCall;
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, Bytes};
use std::time::{SystemTime, UNIX_EPOCH};

/// Max durable pending rows examined per sweep. A safety cap only — orphans are
/// rare; oldest rows are examined first so a cap never starves the urgent ones.
const RECOVERY_SCAN_LIMIT: usize = 10_000;

/// Base and ceiling for the persistent exponential backoff between orphan
/// re-drive attempts. The delay is `min(CAP, BASE * 2^attempts)` with ±25%
/// jitter, so a dependency outage does not produce a synchronised retry storm and
/// a persistently failing row does not spin.
const BACKOFF_BASE_SECS: u64 = 30;
const BACKOFF_CAP_SECS: u64 = 3_600;

/// Interval of the periodic recovery sweep (also the poll cadence for handoffs
/// awaiting Miden confirmation). Bounded so one malformed row cannot wedge the
/// service and unrelated signers keep making progress.
pub const RECOVERY_SWEEP_INTERVAL_SECS: u64 = 30;

/// Current unix time in seconds. Falls back to 0 only if the clock is before the
/// epoch (never in practice), which merely makes the row immediately eligible.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Next-attempt time for an orphan that just failed its `attempts`-th re-drive:
/// exponential backoff capped at [`BACKOFF_CAP_SECS`], with deterministic ±25%
/// jitter derived from the tx hash so retries de-synchronise without needing a
/// RNG (and stay reproducible for tests).
fn next_backoff_at(attempts: u32, jitter_seed: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << attempts.min(20));
    let base = exp.min(BACKOFF_CAP_SECS);
    // Jitter in [-25%, +25%]: map the seed to [0, base/2], subtract base/4.
    let span = base / 2;
    let jitter = if span == 0 {
        0i64
    } else {
        (jitter_seed % (span + 1)) as i64 - (base / 4) as i64
    };
    let delay = (base as i64 + jitter).max(BACKOFF_BASE_SECS as i64) as u64;
    now_unix().saturating_add(delay)
}

/// Outcome of examining one durable pending transaction — dictates whether the
/// signer's nonce walk continues to the next transaction or stops.
enum Step {
    /// This nonce is resolved (finalised, or re-driven into the writer in order);
    /// examine the next nonce for this signer.
    Continue,
    /// This nonce is not yet resolved (active, ambiguous, or backing off); leave
    /// every later nonce for a subsequent sweep to preserve ordering.
    StopSigner,
}

/// Is the transaction's intended effect ALREADY present in authoritative Miden
/// state? A GER is checked with `is_ger_injected`; a claim with `is_claimed` on
/// its global index. A missing local receipt is never treated as proof the effect
/// is absent — this is the reconciliation the recovery contract requires.
async fn effect_already_applied(
    service: &ServiceState,
    envelope: &TxEnvelope,
) -> anyhow::Result<bool> {
    match decode_write_call(envelope_input(envelope)) {
        Ok(DecodedWriteCall::Ger { ger_bytes }) => service.store.is_ger_injected(&ger_bytes).await,
        Ok(DecodedWriteCall::Claim { params }) => {
            service.store.is_claimed(&params.globalIndex).await
        }
        // An undecodable write is a deterministic failure, not a transient one; it
        // cannot have applied and must not be retried as an outage.
        Err(_) => Ok(false),
    }
}

/// Borrow an envelope's calldata `input` for `decode_write_call`.
fn envelope_input(envelope: &TxEnvelope) -> &Bytes {
    match envelope {
        TxEnvelope::Eip1559(s) => &s.tx().input,
        TxEnvelope::Eip2930(s) => &s.tx().input,
        TxEnvelope::Eip4844(s) => &s.tx().tx().input,
        TxEnvelope::Eip7702(s) => &s.tx().input,
        TxEnvelope::Legacy(s) => &s.tx().input,
    }
}

/// Finalise the original proxy hash with a terminal SUCCESS receipt because the
/// intended effect is already durable in Miden. Clears recovery backoff and
/// unblocks the next nonce. Never re-submits.
async fn finalize_success(service: &ServiceState, tx: &RecoverablePendingTxn) {
    let block = service.store.get_latest_block_number().await.unwrap_or(0);
    let block_hash = service.block_state.get_block_hash(block);
    if let Err(e) = service
        .store
        .txn_commit(tx.tx_hash, Ok(()), block, block_hash)
        .await
    {
        tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, error = %e, "recovery: finalize_success txn_commit failed; will retry next sweep");
        return;
    }
    let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
    ::metrics::counter!("orphan_recovery_successes_total").increment(1);
    tracing::info!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, "recovery: effect already applied in Miden — finalised original hash without resubmission");
}

/// Examine one durable pending transaction (caller holds the signer lock).
async fn recover_one(service: &ServiceState, signer: Address, tx: &RecoverablePendingTxn) -> Step {
    // 1. An active writer job owns this hash/nonce — leave it be and keep walking;
    //    it is already progressing toward a handoff.
    let live = service
        .writer_handle
        .as_ref()
        .is_some_and(|h| h.is_inflight(&tx.tx_hash) || h.has_non_terminal_nonce(&signer, tx.nonce));
    if live {
        tracing::debug!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, "recovery: live writer job present; skipping");
        return Step::Continue;
    }

    // 2. Reconcile against authoritative Miden state before any resubmission.
    match effect_already_applied(service, &tx.envelope).await {
        Ok(true) => {
            finalize_success(service, tx).await;
            return Step::Continue;
        }
        Ok(false) => {}
        Err(e) => {
            // The node is likely unavailable; defer with backoff, do not abandon.
            defer_with_backoff(service, tx, &format!("effect check failed: {e}")).await;
            return Step::StopSigner;
        }
    }

    // 3. The intent crossed the external submission boundary (a durable handoff or
    //    a recorded Miden id) but the effect is not yet observed. Its outcome is
    //    ambiguous: poll with backoff, never blindly re-submit, and hold later
    //    nonces until it resolves.
    if tx.handoff.is_some() || tx.miden_tx_id.is_some() {
        defer_with_backoff(service, tx, "handoff recorded; awaiting Miden confirmation").await;
        return Step::StopSigner;
    }

    // 4. Orphaned durable intent: no live job, no handoff, effect absent. Re-drive
    //    the exact stored envelope once its persistent backoff is due.
    if let Some(next_at) = tx.next_recovery_at
        && next_at > now_unix()
    {
        tracing::debug!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, next_at, "recovery: orphan backing off; not yet due");
        ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
        return Step::StopSigner;
    }

    ::metrics::counter!("orphan_recovery_attempts_total").increment(1);

    // Re-drive by RE-ENQUEUEING the writer job directly for this already-durable
    // intent. The pending row and the nonce CAS are already persisted, so we must
    // NOT go back through nonce classification (the public admission path only
    // resumes a tx exactly one below the expected nonce, which would reject the
    // lower of several orphans as a stale nonce). Rebuilding the job from the
    // stored envelope and enqueueing it is the exact work the original admission
    // did after `durably_admit_and_advance_nonce`, so the nonce never advances
    // twice and multiple orphans recover in nonce order. Idempotency is preserved
    // by the writer's handoff fencing and Miden duplicate-protection.
    let Some(handle) = service.writer_handle.as_ref() else {
        defer_with_backoff(service, tx, "no writer handle available").await;
        return Step::StopSigner;
    };
    let decoded = match decode_write_call(envelope_input(&tx.envelope)) {
        Ok(d) => d,
        Err(e) => {
            // Deterministic decode failure — never a transient outage. Record a
            // definite terminal failure so it stops blocking later nonces.
            let block = service.store.get_latest_block_number().await.unwrap_or(0);
            let block_hash = service.block_state.get_block_hash(block);
            let _ = service
                .store
                .txn_commit(
                    tx.tx_hash,
                    Err(format!("recovery: undecodable write call ({e})")),
                    block,
                    block_hash,
                )
                .await;
            let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
            tracing::error!(target: "recovery", tx_hash = %tx.tx_hash, error = %e, "recovery: envelope is not a supported write call; recorded terminal failure");
            return Step::Continue;
        }
    };
    let job = decoded.into_job(tx.envelope.clone(), signer, tx.tx_hash);
    match handle.try_enqueue(job) {
        Ok(()) => {
            let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
            ::metrics::counter!("orphan_recovery_successes_total").increment(1);
            tracing::info!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, attempts = tx.recovery_attempts, "recovery: re-enqueued orphaned durable intent into the writer");
            Step::Continue
        }
        Err(e) => {
            // Writer saturated or shut down — transient. Defer with backoff; the
            // row stays durable and recoverable on the next sweep.
            defer_with_backoff(service, tx, &format!("re-enqueue failed: {e}")).await;
            Step::StopSigner
        }
    }
}

/// Record a failed/deferred attempt and schedule the next one with persistent
/// exponential backoff. Alerts (via a counter) once attempts are non-trivial.
async fn defer_with_backoff(service: &ServiceState, tx: &RecoverablePendingTxn, reason: &str) {
    let jitter_seed = u64::from_le_bytes(tx.tx_hash.0[..8].try_into().unwrap_or([0u8; 8]));
    let next_at = next_backoff_at(tx.recovery_attempts, jitter_seed);
    let attempts = service
        .store
        .record_recovery_attempt(tx.tx_hash, next_at)
        .await
        .unwrap_or(tx.recovery_attempts.saturating_add(1));
    ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
    tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, attempts, next_at, reason, "recovery: deferred with backoff");
    if attempts >= 5 {
        ::metrics::counter!("orphan_recovery_persistent_failures_total").increment(1);
        tracing::error!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, attempts, "recovery: transaction has failed recovery repeatedly — operator attention required");
    }
}

/// Recover every acknowledged pending/unlinked transaction for every signer, each
/// signer walked in nonce order and stopped at the first unresolved nonce so a
/// later nonce is never driven ahead of an unresolved lower one. Each re-drive
/// re-enters the public admission path, which takes the per-signer lock itself, so
/// recovery serialises against live admission per attempt (holding a lock across
/// the whole walk would deadlock that re-entry). Best-effort and bounded: a
/// failure on one row or one signer never blocks startup or the recovery of
/// unrelated signers. Runs at startup and on the periodic sweep.
pub async fn recover_orphaned_pending_txns(service: &ServiceState) -> anyhow::Result<()> {
    let pending = service
        .store
        .recoverable_pending_txns(RECOVERY_SCAN_LIMIT)
        .await?;

    // Observe backlog for the alert gauge before acting.
    ::metrics::gauge!("pending_unlinked_txns").set(pending.len() as f64);
    let oldest_age = pending
        .iter()
        .filter(|t| t.handoff.is_none())
        .map(|t| t.age_secs)
        .max()
        .unwrap_or(0);
    ::metrics::gauge!("pending_unlinked_oldest_age_seconds").set(oldest_age as f64);

    if pending.is_empty() {
        return Ok(());
    }

    // `recoverable_pending_txns` is already ordered by (signer, nonce); group the
    // contiguous runs so each signer is processed under a single lock hold.
    let mut idx = 0;
    while idx < pending.len() {
        let signer_str = pending[idx].signer.clone();
        let mut end = idx + 1;
        while end < pending.len() && pending[end].signer == signer_str {
            end += 1;
        }
        let group = &pending[idx..end];
        idx = end;

        let Ok(signer) = signer_str.parse::<Address>() else {
            tracing::error!(target: "recovery", signer = %signer_str, "recovery: un-parseable signer; skipping");
            continue;
        };
        for tx in group {
            if let Step::StopSigner = recover_one(service, signer, tx).await {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ger::insertGlobalExitRootCall;
    use crate::store::{Store, TxnEntry, memory::InMemoryStore};
    use crate::test_helpers::create_test_service_with_store;
    use crate::writer_worker::{WriterWorker, WriterWorkerHandle};
    use alloy::consensus::{SignableTransaction, TxEnvelope, TxLegacy};
    use alloy::primitives::{Address, FixedBytes, TxHash};
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;
    use alloy_core::sol_types::SolCall;
    use std::sync::Arc;
    use std::time::Duration;

    /// A GER-injection transaction really signed by `key` at `nonce`. Returns the
    /// decoded envelope, its hash, the 32-byte GER, and the recovered signer.
    fn signed_ger_tx(
        key: &PrivateKeySigner,
        nonce: u64,
        marker: u8,
    ) -> (TxEnvelope, TxHash, [u8; 32], Address) {
        let ger_bytes = [marker; 32];
        let input = insertGlobalExitRootCall {
            root: FixedBytes::from(ger_bytes),
        }
        .abi_encode();
        let txn = TxLegacy {
            nonce,
            input: input.into(),
            chain_id: Some(1),
            gas_price: 1,
            ..Default::default()
        };
        let sig = key
            .sign_hash_sync(&txn.signature_hash())
            .expect("sign test tx");
        let signed = txn.into_signed(sig);
        let hash = *signed.hash();
        let envelope: TxEnvelope = signed.into();
        // The signer is the key's address; recovering it from the envelope would
        // need an extra trait import and must match this anyway.
        (envelope, hash, ger_bytes, key.address())
    }

    fn signer_hex(signer: Address) -> String {
        format!("{signer:#x}")
    }

    /// Model the exact durable state the issue targets: a `pending` row admitted
    /// via `durably_admit_and_advance_nonce` (pending row + nonce CAS) whose writer
    /// job was then lost — no handoff, no live worker. Advances the signer nonce
    /// from 0 through `nonce` so the row sits one below the expected nonce.
    async fn install_orphan(
        store: &Arc<dyn Store>,
        envelope: &TxEnvelope,
        hash: TxHash,
        signer: Address,
        nonce: u64,
    ) {
        store
            .txn_begin_if_absent(
                hash,
                TxnEntry {
                    id: None,
                    envelope: envelope.clone(),
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        let addr = signer_hex(signer);
        for n in 0..=nonce {
            assert!(
                store.nonce_advance_cas(&addr, n).await.unwrap(),
                "nonce CAS {n} must advance in test setup"
            );
        }
    }

    fn with_writer(store: Arc<dyn Store>, depth: usize) -> crate::service_state::ServiceState {
        let mut service = create_test_service_with_store(store);
        let (handle, _sd) = WriterWorker::spawn(service.clone(), depth, Duration::from_secs(60));
        // Leak the shutdown sender so the worker lives for the test.
        std::mem::forget(_sd);
        service.writer_handle = Some(Arc::new(handle));
        service
    }

    /// #156 tests 1/2/4 — a durable pending orphan with no handoff and no live
    /// writer job (crash after admission before enqueue, or the capacity race) is
    /// automatically re-driven back into the writer, and its nonce is NOT advanced
    /// a second time. No client rebroadcast.
    #[tokio::test]
    async fn orphan_pending_no_handoff_is_redriven() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xA0);
        install_orphan(&store, &env, hash, signer, 0).await;
        assert_eq!(store.nonce_get(&signer_hex(signer)).await.unwrap(), 1);

        let service = with_writer(store.clone(), 64);
        recover_orphaned_pending_txns(&service).await.unwrap();

        let handle = service.writer_handle.as_ref().unwrap();
        let recovered =
            handle.is_inflight(&hash) || store.txn_receipt(hash).await.unwrap().is_some();
        assert!(
            recovered,
            "the orphan must be re-driven into the writer (or finalised), not stranded"
        );
        assert_eq!(
            store.nonce_get(&signer_hex(signer)).await.unwrap(),
            1,
            "re-driving an orphan must NOT advance the nonce a second time"
        );
    }

    /// #156 test 12 (lost local receipt) + reconciliation — the GER is already
    /// applied in Miden but the proxy never recorded its receipt. Recovery
    /// finalises the original hash from authoritative state WITHOUT resubmitting.
    #[tokio::test]
    async fn effect_already_applied_finalizes_without_resubmit() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, ger, signer) = signed_ger_tx(&key, 0, 0xB0);
        install_orphan(&store, &env, hash, signer, 0).await;
        // The GER is already applied in Miden state (lost local receipt).
        store
            .commit_ger_event_atomic(1, [1u8; 32], &format!("{hash:#x}"), &ger, None, None, 0)
            .await
            .unwrap();
        assert!(store.is_ger_injected(&ger).await.unwrap());

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        let (result, _) = store
            .txn_receipt(hash)
            .await
            .unwrap()
            .expect("recovery must finalise a terminal receipt from applied Miden state");
        assert!(
            result.is_ok(),
            "finalised outcome must be success: {result:?}"
        );
        assert!(
            !handle.is_inflight(&hash),
            "an already-applied effect must NOT be resubmitted to the writer"
        );
    }

    /// #156 test 10 — multiple orphans for one signer are recovered in nonce order.
    #[tokio::test]
    async fn multiple_orphans_recovered_in_nonce_order() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env0, h0, _g0, signer) = signed_ger_tx(&key, 0, 0xC0);
        let (env1, h1, _g1, _s1) = signed_ger_tx(&key, 1, 0xC1);
        // Both admitted (nonce advanced to 2), neither handed off nor live.
        store
            .txn_begin_if_absent(h0, entry(&env0, signer))
            .await
            .unwrap();
        store
            .txn_begin_if_absent(h1, entry(&env1, signer))
            .await
            .unwrap();
        let addr = signer_hex(signer);
        assert!(store.nonce_advance_cas(&addr, 0).await.unwrap());
        assert!(store.nonce_advance_cas(&addr, 1).await.unwrap());

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        for (n, h) in [(0u64, h0), (1, h1)] {
            let recovered = handle.is_inflight(&h) || store.txn_receipt(h).await.unwrap().is_some();
            assert!(recovered, "orphan at nonce {n} must be recovered");
        }
        assert_eq!(
            store.nonce_get(&addr).await.unwrap(),
            2,
            "no nonce may advance twice during multi-orphan recovery"
        );
    }

    fn entry(env: &TxEnvelope, signer: Address) -> TxnEntry {
        TxnEntry {
            id: None,
            envelope: env.clone(),
            signer,
            expires_at: None,
            logs: vec![],
        }
    }

    /// #156 test 13 — a durable handoff is recorded but the effect is not yet
    /// confirmed (Miden outage). Recovery must NOT resubmit; it defers with backoff
    /// and the schedule is persisted.
    #[tokio::test]
    async fn handoff_recorded_is_polled_not_resubmitted() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xD0);
        install_orphan(&store, &env, hash, signer, 0).await;
        // A prepared handoff: the intent crossed the external submission boundary.
        store
            .prepare_note_handoff(&format!("{hash:#x}"), "0xcommit", "0xnote", 100)
            .await
            .unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            !handle.is_inflight(&hash),
            "a handoff-recorded tx must be polled, never blindly resubmitted"
        );
        let after = store
            .recoverable_pending_txns(10)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.tx_hash == hash)
            .expect("still pending");
        assert_eq!(
            after.recovery_attempts, 1,
            "backoff attempt must be recorded"
        );
        assert!(
            after.next_recovery_at.is_some(),
            "next-attempt must be scheduled"
        );
    }

    /// #156 test 8 — a persisted future backoff defers the re-drive: an orphan
    /// whose `next_recovery_at` is in the future is not retried yet, and is not
    /// resubmitted.
    #[tokio::test]
    async fn backoff_not_due_defers_redrive() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xE0);
        install_orphan(&store, &env, hash, signer, 0).await;
        // Schedule the next attempt far in the future.
        store
            .record_recovery_attempt(hash, now_unix() + 10_000)
            .await
            .unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            !handle.is_inflight(&hash),
            "an orphan that is backing off must not be re-driven yet"
        );
    }

    /// #156 test 4 — the writer-capacity race: a durable pending orphan whose
    /// re-drive is rejected because the writer is saturated stays automatically
    /// recoverable — its backoff advances and it is NOT stranded.
    #[tokio::test]
    async fn saturated_writer_defers_orphan_with_backoff() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xF0);
        install_orphan(&store, &env, hash, signer, 0).await;

        let mut service = create_test_service_with_store(store.clone());
        let (handle, _sat) = WriterWorkerHandle::saturated_for_test();
        assert_eq!(handle.available_capacity(), 0);
        service.writer_handle = Some(Arc::new(handle));

        recover_orphaned_pending_txns(&service).await.unwrap();

        let after = store
            .recoverable_pending_txns(10)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.tx_hash == hash)
            .expect("orphan must remain durably pending and recoverable, not stranded");
        assert!(
            after.recovery_attempts >= 1,
            "a saturated re-drive must record a backoff attempt, keeping the tx recoverable"
        );
        assert_eq!(
            store.nonce_get(&signer_hex(signer)).await.unwrap(),
            1,
            "a failed re-drive must not advance the nonce"
        );
    }

    /// Recovery leaves a transaction with a live writer job untouched (no
    /// duplicate work) and takes no action on an empty backlog.
    #[tokio::test]
    async fn empty_backlog_is_a_noop() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let service = with_writer(store, 64);
        // No pending rows at all.
        recover_orphaned_pending_txns(&service).await.unwrap();
    }
}
