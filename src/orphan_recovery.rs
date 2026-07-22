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
//! - **No handoff, no effect** — an orphaned durable intent; re-drive the exact
//!   stored envelope through the same-hash durable-resume path with persistent
//!   exponential backoff.
//!
//! Delivery is at-least-once with state reconciliation: every re-drive checks
//! authoritative Miden state first, the durable-resume path reuses the writer's
//! handoff fencing, and Miden duplicate-protection is the final safety boundary,
//! so recovery cannot double-advance a nonce or duplicate a GER/claim. Multi-
//! replica coordination is out of scope (#142).

use crate::service_send_raw_txn::{decode_write_call, service_send_raw_txn};
use crate::service_state::ServiceState;
use crate::store::RecoverablePendingTxn;
use crate::writer_worker::DecodedWriteCall;
use alloy::consensus::TxEnvelope;
use alloy::eips::Encodable2718;
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

/// Re-hex the stored envelope for replay through the normal admission path.
fn envelope_hex(envelope: &TxEnvelope) -> String {
    let mut bytes = Vec::new();
    envelope.encode_2718(&mut bytes);
    format!("0x{}", alloy::hex::encode(bytes))
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
    // Re-drive through the PUBLIC entry: it acquires this signer's lock itself (so
    // recovery serialises against live admission) and, because the row is durable
    // pending with the nonce already advanced, takes the same-hash durable-resume
    // path — re-enqueueing the exact envelope without a second nonce advance.
    match Box::pin(service_send_raw_txn(
        service.clone(),
        envelope_hex(&tx.envelope),
    ))
    .await
    {
        Ok(_) => {
            // Re-enqueued (or reconciled) through the durable-resume path. Clear
            // backoff and keep walking: later nonces queue behind it in order.
            let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
            ::metrics::counter!("orphan_recovery_successes_total").increment(1);
            tracing::info!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, attempts = tx.recovery_attempts, "recovery: re-drove orphaned durable intent through the same-hash path");
            Step::Continue
        }
        Err(e) => {
            defer_with_backoff(service, tx, &format!("re-drive failed: {e}")).await;
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
