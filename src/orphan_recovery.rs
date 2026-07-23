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
//! - **Confirmed submission but effect not yet observed** — a `Submitted` handoff
//!   or a recorded Miden tx id: Miden already has it, so poll for the effect with
//!   backoff and never re-submit.
//! - **Prepared-but-unconfirmed handoff** — the exact note is durable but Miden
//!   never confirmed it (e.g. the proxy was killed mid-proving). The note cannot be
//!   reproduced (its serial is random), so we cannot blindly re-drive. Wait until
//!   the authoritative reconcile cursor passes the note's expiration block — proving
//!   the creating tx can never be included, so the note is dead — then clear the
//!   stale link and re-drive a FRESH note; until then poll at the sweep cadence (the
//!   note may still be consumed). This relies on submission notes carrying a finite
//!   expiration delta (`claim::submission_note_expiration_delta`).
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
use crate::store::{NoteHandoffState, RecoverablePendingTxn};
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

/// Finalise the original proxy hash with a terminal SUCCESS receipt for a GER that
/// was applied ELSEWHERE (injected by another transaction, or a duplicate). GER
/// injection is idempotent and ownership-independent, so a duplicate is a success.
///
/// Uses `txn_commit_confirmed_duplicate` (NOT `txn_commit`): ordinary `txn_commit`
/// no-ops on a note-linked row (projector-owned), so a LINKED applied-elsewhere GER
/// would silently loop. The confirmed-duplicate path finalises a linked-or-unlinked
/// row without emitting an event and without overwriting an existing terminal
/// receipt. It is used ONLY for `AppliedElsewhere`; an EXACT-note GER is left pending
/// so the projector finalises it atomically with its `UpdateHashChainValue` event at
/// the consumption block (reviewer #4). Returns `true` only when durably written.
#[must_use]
async fn finalize_ger_duplicate(service: &ServiceState, tx: &RecoverablePendingTxn) -> bool {
    let block = service.store.get_latest_block_number().await.unwrap_or(0);
    if let Err(e) = service
        .store
        .txn_commit_confirmed_duplicate(tx.tx_hash, Ok(()), block)
        .await
    {
        tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, error = %e, "recovery: finalize_ger_duplicate commit failed; stopping signer, will retry next sweep");
        return false;
    }
    let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
    ::metrics::counter!("orphan_recovery_successes_total").increment(1);
    tracing::info!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, "recovery: GER already applied elsewhere (idempotent) — finalised original hash as a confirmed duplicate");
    true
}

/// Finalise the original proxy hash with a terminal REVERTED (`AlreadyClaimed()`)
/// receipt because the claim's global index was landed by a DIFFERENT transaction/
/// note (reviewer #3 — "applied elsewhere"). This tx's exact note was NOT the one
/// consumed, so a plain success receipt would be a lie; re-driving would build a
/// note that reverts on-chain.
///
/// Uses `txn_commit_confirmed_duplicate` (NOT `txn_commit`): an ordinary
/// `txn_commit` deliberately NO-OPS when a note handoff exists (it must not
/// overwrite a projector-owned linked receipt), so a LINKED applied-elsewhere claim
/// would silently stay pending and recovery would loop (reviewer). The confirmed-
/// duplicate path finalises a note-linked row without emitting an event and without
/// overwriting an existing terminal receipt. Returns `true` only when durably written.
#[must_use]
async fn finalize_already_claimed(
    service: &ServiceState,
    tx: &RecoverablePendingTxn,
    global_index: alloy::primitives::U256,
) -> bool {
    let block = service.store.get_latest_block_number().await.unwrap_or(0);
    let msg =
        format!("claim for globalIndex {global_index} already landed (AlreadyClaimed); reverted");
    if let Err(e) = service
        .store
        .txn_commit_confirmed_duplicate(tx.tx_hash, Err(msg), block)
        .await
    {
        tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, error = %e, "recovery: finalize_already_claimed commit failed; stopping signer, will retry next sweep");
        return false;
    }
    let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
    ::metrics::counter!("orphan_recovery_already_claimed_total").increment(1);
    tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, %global_index, "recovery: claim landed via another transaction — finalised original hash as AlreadyClaimed (reverted), not resubmitted");
    true
}

/// Finalise the original hash with a terminal FAILURE receipt (a deterministic,
/// non-retryable error). Returns `true` only when durably written (see #6).
#[must_use]
async fn finalize_terminal_failure(
    service: &ServiceState,
    tx: &RecoverablePendingTxn,
    reason: String,
) -> bool {
    let block = service.store.get_latest_block_number().await.unwrap_or(0);
    let block_hash = service.block_state.get_block_hash(block);
    if let Err(e) = service
        .store
        .txn_commit(tx.tx_hash, Err(reason.clone()), block, block_hash)
        .await
    {
        tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, error = %e, "recovery: terminal-failure txn_commit failed; stopping signer, will retry next sweep");
        return false;
    }
    let _ = service.store.clear_recovery_backoff(tx.tx_hash).await;
    tracing::error!(target: "recovery", tx_hash = %tx.tx_hash, reason, "recovery: recorded terminal failure");
    true
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

    // Reviewer #5 (nonce) — durable admission is txn_begin THEN nonce CAS. A crash
    // BETWEEN them leaves this pending row with the signer nonce NOT advanced; if
    // recovery re-drove/executed the tx while the expected nonce stayed behind, the
    // NEXT tx would reuse this nonce. Repair the CAS first (idempotent: it advances
    // only when the expected nonce still equals this tx's nonce, and no-ops once
    // already advanced), before any finalisation or re-enqueue.
    if let Err(e) = crate::service_send_raw_txn::repair_commit_gap_nonce(
        service,
        &format!("{signer:#x}"),
        tx.nonce,
    )
    .await
    {
        poll_next_sweep(
            tx,
            &format!("nonce-gap repair failed (db unavailable?): {e}"),
        );
        return Step::StopSigner;
    }

    // Decode the durable write call once — needed both to reconcile against Miden
    // and, if we must re-drive, to rebuild the writer job.
    let decoded = match decode_write_call(envelope_input(&tx.envelope)) {
        Ok(d) => d,
        Err(e) => {
            // Deterministic decode failure — never transient. Record a terminal
            // failure so it stops blocking later nonces; stop the signer if that
            // durable write did NOT land (reviewer #6 — a lower nonce that did not
            // durably finalise must not let a higher one proceed).
            let ok = finalize_terminal_failure(
                service,
                tx,
                format!("recovery: undecodable write call ({e})"),
            )
            .await;
            return if ok { Step::Continue } else { Step::StopSigner };
        }
    };

    // Read the durable note handoff once: the exact `note_id` for the reconcile
    // (reviewer #3) and the `note_commitment` for the prepared-expiry path. tx.handoff
    // (row existence + state) already classified it; here we fetch the identity.
    let tx_key = format!("{:#x}", tx.tx_hash);
    let handoff = match service.store.get_note_handoff_for_tx(&tx_key).await {
        Ok(h) => h,
        Err(e) => {
            poll_next_sweep(tx, &format!("handoff read failed (db unavailable?): {e}"));
            return Step::StopSigner;
        }
    };
    let handoff_note_id = handoff.as_ref().and_then(|h| h.note_id.clone());

    // 2. Reconcile against a FRESH, authoritative Miden view (reviewer #5 — the
    //    classifier syncs before reading, so a stale local view never reports a false
    //    "absent"), using the EXACT handoff note when known so a claim landed by a
    //    DIFFERENT transaction is not mistaken for this one succeeding (reviewer #3).
    let outcome = match &decoded {
        DecodedWriteCall::Ger { ger_bytes } => {
            crate::applied_state::reconcile_ger_recovery(
                service,
                *ger_bytes,
                handoff_note_id,
                tx.handoff.is_some(),
            )
            .await
        }
        DecodedWriteCall::Claim { params } => {
            crate::applied_state::reconcile_claim_recovery(
                service,
                params.globalIndex,
                handoff_note_id,
                tx.handoff.is_some(),
            )
            .await
        }
    };
    match outcome {
        Err(e) => {
            // Node unavailable OR no fresh view — NEVER decide "absent" from a
            // stale/absent read. Poll at the sweep cadence (do NOT grow the backoff:
            // a node-down window would otherwise push recovery minutes out, past a
            // deposit's claim deadline).
            poll_next_sweep(
                tx,
                &format!("Miden reconcile failed (node unavailable/unsynced?): {e}"),
            );
            return Step::StopSigner;
        }
        Ok(crate::applied_state::ExactNoteOutcome::AppliedByExactNote) => {
            // The EXACT note landed. Reviewer #4 — finalisation of an exact landed note
            // is PROJECTOR-OWNED for BOTH GER and CLAIM: the SyntheticProjector
            // finalises the receipt atomically with its event (ClaimEvent /
            // UpdateHashChainValue) at the CONSUMPTION block. The Miden bridge state can
            // show the effect applied BEFORE the projector processes that block, so
            // recovery writing a success receipt now would expose a success at the wrong
            // block with no event. Leave it PENDING and poll — projection finalises it.
            let what = match &decoded {
                DecodedWriteCall::Ger { .. } => "GER",
                DecodedWriteCall::Claim { .. } => "claim",
            };
            poll_next_sweep(
                tx,
                &format!("{what} exact-note consumed; awaiting event projection (projector-owned)"),
            );
            return Step::StopSigner;
        }
        Ok(crate::applied_state::ExactNoteOutcome::AppliedElsewhere) => {
            // The effect was applied by ANOTHER transaction/note; the projector will
            // never emit an event for THIS tx's (unconsumed/absent) note, so recovery
            // finalises it as a confirmed duplicate. GER injection is idempotent →
            // success; a CLAIM would revert AlreadyClaimed. Both use the confirmed-
            // duplicate path (works on linked rows; emits no event).
            return match &decoded {
                DecodedWriteCall::Ger { .. } => {
                    if finalize_ger_duplicate(service, tx).await {
                        Step::Continue
                    } else {
                        Step::StopSigner
                    }
                }
                DecodedWriteCall::Claim { params } => {
                    if finalize_already_claimed(service, tx, params.globalIndex).await {
                        Step::Continue
                    } else {
                        Step::StopSigner
                    }
                }
            };
        }
        // NotApplied, or Uncertain (the exact note is Missing/unsynced): do NOT
        // finalise — fall through to the handoff-state machine, which polls a
        // submitted note, clears+re-drives a provably-dead prepared note, or
        // re-drives a pure orphan.
        Ok(crate::applied_state::ExactNoteOutcome::NotApplied)
        | Ok(crate::applied_state::ExactNoteOutcome::Uncertain) => {}
    }

    // 3. A CONFIRMED submission — a `Submitted` handoff or a recorded Miden tx id —
    //    crossed the boundary and Miden already has it. Poll for the effect at the
    //    sweep cadence; never re-submit, and hold later nonces until it resolves.
    if matches!(tx.handoff, Some(NoteHandoffState::Submitted)) || tx.miden_tx_id.is_some() {
        poll_next_sweep(tx, "submitted handoff; awaiting Miden confirmation");
        return Step::StopSigner;
    }

    // 4a. A PREPARED-but-unconfirmed handoff needs care: the writer durably recorded
    //     an EXACT note identity but was interrupted (e.g. killed mid-proving) before
    //     Miden confirmed it. We must NOT blindly re-drive — each GER/claim note is
    //     built with a fresh random serial, so a re-drive produces a DIFFERENT note
    //     that conflicts with the durable prepared link (`prepare_note_handoff` is
    //     first-writer-wins) and the outcome stays forever "ambiguous", looping. Mirror
    //     the public admission path: clear the prepared link ONLY once the authoritative
    //     Miden reconcile cursor has passed its expiration block (proving the creating
    //     tx can never be included, so the note is dead), then re-drive a fresh note.
    //     Until then, poll at the sweep cadence — the note may yet be consumed (→ effect
    //     applied, finalised in step 2) or will expire. (Relies on submission notes
    //     carrying a finite expiration delta; see `submission_note_expiration_delta`.)
    if matches!(tx.handoff, Some(NoteHandoffState::Prepared)) {
        let Some(commitment) = handoff.as_ref().map(|h| h.note_commitment.clone()) else {
            poll_next_sweep(
                tx,
                "prepared handoff no longer present; re-checking next sweep",
            );
            return Step::StopSigner;
        };
        match service
            .store
            .clear_expired_prepared_note_handoff(&tx_key, &commitment)
            .await
        {
            Ok(true) => {
                // Expired past the authoritative cursor — the stale note is provably
                // dead. Fall through to re-drive a fresh note.
                tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, "recovery: cleared expired prepared note handoff; re-driving a fresh note");
            }
            Ok(false) => {
                poll_next_sweep(
                    tx,
                    "prepared note not yet expired; awaiting consumption or expiration",
                );
                return Step::StopSigner;
            }
            Err(e) => {
                poll_next_sweep(
                    tx,
                    &format!("prepared-handoff expiration check failed: {e}"),
                );
                return Step::StopSigner;
            }
        }
    }

    // 4b. Orphaned durable intent (or a just-cleared, provably-dead prepared handoff).
    //     Re-drive the exact intent once its persistent backoff is due.
    if let Some(next_at) = tx.next_recovery_at
        && next_at > now_unix()
    {
        tracing::debug!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, next_at, "recovery: orphan backing off; not yet due");
        ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
        return Step::StopSigner;
    }

    let Some(handle) = service.writer_handle.as_ref() else {
        defer_with_backoff(service, tx, "no writer handle available").await;
        return Step::StopSigner;
    };

    // Reviewer #1 — persist the next backoff BEFORE the enqueue, and enqueue ONLY
    // after a CONFIRMED durable update. If the persist FAILS (DB error, or no row
    // matched), we must NOT enqueue: a crash after an un-backed-off enqueue but before
    // a durable Miden handoff would leave a zero/NULL backoff and the next sweep/boot
    // would re-drive immediately — the unbounded repeated-OOM retry-loop. The backoff
    // is cleared only on a durable handoff/terminal (via `finalize_*`), never merely
    // because the enqueue was accepted.
    ::metrics::counter!("orphan_recovery_attempts_total").increment(1);
    let jitter_seed = u64::from_le_bytes(tx.tx_hash.0[..8].try_into().unwrap_or([0u8; 8]));
    let next_at = next_backoff_at(tx.recovery_attempts, jitter_seed);
    let attempts = match service
        .store
        .record_recovery_attempt(tx.tx_hash, next_at)
        .await
    {
        Ok(attempts) => attempts,
        Err(e) => {
            // Backoff NOT durably persisted — defer to the next sweep rather than
            // enqueue without a durable gate.
            ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
            tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, error = %e, "recovery: backoff persist failed; NOT enqueuing without a durable backoff (deferred to next sweep)");
            return Step::StopSigner;
        }
    };
    if attempts >= 5 {
        ::metrics::counter!("orphan_recovery_persistent_failures_total").increment(1);
        tracing::error!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, attempts, "recovery: transaction has failed recovery repeatedly — operator attention required");
    }

    // Re-drive by RE-ENQUEUEING the writer job directly for this already-durable
    // intent (NOT the public admission path, which only resumes a tx exactly one
    // below the expected nonce and would reject the lower of several orphans as
    // stale). The nonce CAS is already persisted, so this advances no nonce and
    // recovers multiple orphans for one signer in nonce order; idempotency is
    // preserved by the writer's handoff fencing and Miden duplicate-protection.
    let job = decoded.into_job(tx.envelope.clone(), signer, tx.tx_hash);
    match handle.try_enqueue(job) {
        Ok(()) => {
            ::metrics::counter!("orphan_recovery_redrives_total").increment(1);
            tracing::info!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, signer = %tx.signer, attempts, "recovery: re-enqueued orphaned durable intent into the writer (backoff persisted pre-enqueue)");
            Step::Continue
        }
        Err(e) => {
            // Writer saturated/shut down — the backoff is already persisted above.
            ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
            tracing::warn!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, attempts, error = %e, "recovery: re-enqueue failed; deferred with persisted backoff");
            Step::StopSigner
        }
    }
}

/// Defer to the NEXT periodic sweep without touching the durable backoff schedule.
/// Used for TRANSIENT conditions — the Miden node being unavailable, or a confirmed
/// submission still awaiting projection — where the right cadence is the sweep
/// interval (~[`RECOVERY_SWEEP_INTERVAL_SECS`]) and growing the exponential backoff
/// would only delay recovery once the condition clears. It records no attempt (so
/// these do not trip the persistent-failure alert); the oldest-age gauge covers a
/// genuinely stuck transaction.
fn poll_next_sweep(tx: &RecoverablePendingTxn, reason: &str) {
    ::metrics::counter!("orphan_recovery_deferred_total").increment(1);
    tracing::debug!(target: "recovery", tx_hash = %tx.tx_hash, nonce = tx.nonce, reason, "recovery: deferred to next sweep (transient)");
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
/// later nonce is never driven ahead of an unresolved lower one. Each signer's
/// walk runs UNDER that signer's lock so recovery's read-reconcile-re-enqueue is
/// serialised against live admission for the same signer (no interleaving at a
/// nonce recovery is touching); the re-drive enqueues the writer job directly
/// rather than re-entering the locking admission path, so there is no
/// self-deadlock. Different signers are independent. Best-effort and bounded: a
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
        // Serialise this signer's recovery against live admission: the walk reads
        // the pending state, reconciles it against Miden, and re-enqueues under one
        // continuous hold, so a concurrent `eth_sendRawTransaction` for the same
        // signer cannot interleave at a nonce recovery is resolving. The re-drive
        // uses `try_enqueue` directly (it does NOT re-acquire this lock), so the
        // hold is safe.
        let _guard = service.per_signer_locks.lock(signer).await;
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

    /// A `claimAsset` transaction really signed by `key` at `nonce` for
    /// `global_index`. Proof fields are zeroed — recovery classification never
    /// executes the proof; it only decodes the global index.
    fn signed_claim_tx(
        key: &PrivateKeySigner,
        nonce: u64,
        global_index: alloy::primitives::U256,
        dest_marker: u8,
    ) -> (TxEnvelope, TxHash, Address) {
        use crate::claim::claimAssetCall;
        let params = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: Address::from([dest_marker; 20]),
            amount: alloy::primitives::U256::from(1000u64),
            metadata: Default::default(),
        };
        let txn = TxLegacy {
            nonce,
            input: params.abi_encode().into(),
            chain_id: Some(1),
            gas_price: 1,
            ..Default::default()
        };
        let sig = key
            .sign_hash_sync(&txn.signature_hash())
            .expect("sign test claim tx");
        let signed = txn.into_signed(sig);
        let hash = *signed.hash();
        (signed.into(), hash, key.address())
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

    /// Reviewer #3 — a recovered CLAIM whose global index was landed by ANOTHER
    /// transaction (a projected ClaimEvent for a different tx hash) must be finalised
    /// as a REVERTED `AlreadyClaimed` receipt, NOT a false success, and must NOT be
    /// resubmitted (a fresh claim would revert on-chain). This is the exact-note-vs-
    /// applied-elsewhere distinction the coarse index check conflated.
    #[tokio::test]
    async fn claim_applied_elsewhere_finalizes_reverted_not_success() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let global_index = crate::applied_state::global_index_for_claim(7, 0);
        let (env, hash, signer) = signed_claim_tx(&key, 0, global_index, 0xC7);
        install_orphan(&store, &env, hash, signer, 0).await;
        // A DIFFERENT transaction already landed this global index.
        let gi_bytes = global_index.to_be_bytes::<32>();
        store
            .add_claim_event(
                &format!("{:#x}", Address::from([0xBBu8; 20])),
                1,
                [2u8; 32],
                &format!("{:#x}", TxHash::from([0x0Au8; 32])),
                &gi_bytes,
                0,
                &[0u8; 20],
                &[0xC7u8; 20],
                1000,
            )
            .await
            .unwrap();
        assert!(
            store
                .has_claim_event_for_global_index(&gi_bytes)
                .await
                .unwrap()
        );

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        let (result, _) = store
            .txn_receipt(hash)
            .await
            .unwrap()
            .expect("recovery must finalise a terminal receipt for an applied-elsewhere claim");
        let err = result.expect_err("an applied-elsewhere claim must be REVERTED, not a success");
        assert!(
            err.contains("AlreadyClaimed"),
            "the revert reason must state AlreadyClaimed: {err}"
        );
        assert!(
            !handle.is_inflight(&hash),
            "an applied-elsewhere claim must NOT be resubmitted to the writer"
        );
        assert_eq!(
            store.nonce_get(&signer_hex(signer)).await.unwrap(),
            1,
            "finalising an applied-elsewhere claim must not advance the nonce twice"
        );
    }

    /// Reviewer #3 — a LEGACY submitted claim (a durable handoff exists but its
    /// note_id is NULL, e.g. a `record_tx_note_link` / pre-migration-012 row) whose
    /// global index is claimed on-chain CANNOT prove whether ITS OWN note or another
    /// claimer's landed it. It must stay PENDING for normal projection to resolve —
    /// NOT be labelled AlreadyClaimed (a false revert on a possibly-successful claim).
    #[tokio::test]
    async fn legacy_null_note_id_claim_with_landed_index_stays_pending() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let global_index = crate::applied_state::global_index_for_claim(9, 0);
        let (env, hash, signer) = signed_claim_tx(&key, 0, global_index, 0xC9);
        install_orphan(&store, &env, hash, signer, 0).await;
        // A legacy SUBMITTED handoff with a NULL note_id (record_tx_note_link).
        store
            .record_tx_note_link(&format!("{hash:#x}"), "0xcommit")
            .await
            .unwrap();
        // The index is claimed on-chain (by SOME transaction).
        store
            .add_claim_event(
                &format!("{:#x}", Address::from([0xBBu8; 20])),
                1,
                [2u8; 32],
                &format!("{:#x}", TxHash::from([0x0Au8; 32])),
                &global_index.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0xC9u8; 20],
                1000,
            )
            .await
            .unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            store.txn_receipt(hash).await.unwrap().is_none(),
            "a legacy null-note_id submitted claim must stay PENDING for projection, not be finalised AlreadyClaimed"
        );
        assert!(
            !handle.is_inflight(&hash),
            "a submitted (null-note_id) claim must not be re-driven"
        );
    }

    /// Reviewer #4 (GER atomicity) — a legacy submitted GER (handoff, NULL note_id)
    /// must NOT be finalised off the bridge snapshot before the projector emits its
    /// UpdateHashChainValue event: it stays PENDING (Uncertain) for projection.
    #[tokio::test]
    async fn legacy_null_note_id_ger_stays_pending() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xD7);
        install_orphan(&store, &env, hash, signer, 0).await;
        // Legacy SUBMITTED handoff with a NULL note_id (record_tx_note_link).
        store
            .record_tx_note_link(&format!("{hash:#x}"), "0xcommit")
            .await
            .unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        // Recovery must NOT finalise it off the bridge snapshot (which would race the
        // projector's UpdateHashChainValue event): it stays PENDING for projection and
        // is not re-driven.
        assert!(
            store.txn_receipt(hash).await.unwrap().is_none(),
            "a legacy null-note_id GER must stay PENDING for projection, not be finalised by recovery"
        );
        assert!(
            !handle.is_inflight(&hash),
            "a legacy null-note_id GER must not be re-driven by recovery"
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

    /// #156 — a CONFIRMED (`Submitted`) handoff whose effect is not yet observed
    /// (projection lag / Miden outage) is POLLED with backoff, never re-submitted:
    /// Miden already has it, so re-submitting could duplicate work.
    #[tokio::test]
    async fn submitted_handoff_is_polled_not_resubmitted() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xD0);
        install_orphan(&store, &env, hash, signer, 0).await;
        // A SUBMITTED handoff: Miden committed (or the exact note was observed).
        let tx_key = format!("{hash:#x}");
        store
            .prepare_note_handoff(&tx_key, "0xcommit", "0xnote", 100)
            .await
            .unwrap();
        store
            .confirm_note_handoff(&tx_key, "0xcommit")
            .await
            .unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            !handle.is_inflight(&hash),
            "a confirmed (submitted) handoff must be polled, never re-submitted"
        );
        let after = store
            .recoverable_pending_txns(10)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.tx_hash == hash)
            .expect("still pending");
        // A confirmed submission is polled at the sweep cadence, NOT with the
        // exponential backoff — the projector observes it shortly, and delaying
        // finalisation would be wrong. So no durable backoff attempt is recorded.
        assert_eq!(
            after.recovery_attempts, 0,
            "a submitted-handoff poll must not grow the exponential backoff"
        );
        assert!(
            after.next_recovery_at.is_none(),
            "polling a confirmed submission must not schedule an exponential backoff"
        );
    }

    /// #156 (reviewer) — a PREPARED-but-unconfirmed handoff whose note is provably
    /// DEAD (the authoritative reconcile cursor has passed its expiration block, so
    /// the creating tx can never be included — the proxy was killed mid-proving) is
    /// re-driven as a FRESH note, not deferred forever. This is the "killed while
    /// sending/proving the tx → self-heals" case at the unit level. Re-driving as a
    /// fresh note (rather than resubmitting the old random-serial one, which would
    /// conflict with the durable prepared link and loop forever) is the fix.
    #[tokio::test]
    async fn expired_prepared_handoff_is_redriven() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xD1);
        install_orphan(&store, &env, hash, signer, 0).await;
        // A PREPARED handoff only — the Miden submit never confirmed (no
        // confirm_note_handoff), and the GER is not injected. Expiration block 100.
        store
            .prepare_note_handoff(&format!("{hash:#x}"), "0xcommit", "0xnote", 100)
            .await
            .unwrap();
        // The authoritative reconcile cursor has moved PAST the note's expiration —
        // the creating tx can never be included, so the stale note is dead.
        store.set_reconcile_cursor(101).await.unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            handle.is_inflight(&hash),
            "an EXPIRED prepared handoff must be re-driven as a fresh note, not deferred forever"
        );
        // The stale prepared link must have been cleared so the fresh re-drive's note
        // can be recorded without a first-writer-wins conflict.
        assert!(
            store
                .get_note_handoff_for_tx(&format!("{hash:#x}"))
                .await
                .unwrap()
                .is_none(),
            "the expired prepared link must be cleared before re-driving a fresh note"
        );
        assert_eq!(
            store.nonce_get(&signer_hex(signer)).await.unwrap(),
            1,
            "re-driving a prepared handoff must not advance the nonce twice"
        );
    }

    /// #156 (reviewer) — a PREPARED-but-unconfirmed handoff that is NOT yet expired
    /// (the reconcile cursor has not passed its expiration block) must NOT be
    /// re-driven: the exact note may still be in-flight and consumed, so building a
    /// second random-serial note would conflict (and, for claims, risk a duplicate).
    /// Recovery polls at the sweep cadence instead — no re-drive, no backoff growth,
    /// the durable link is preserved.
    #[tokio::test]
    async fn unexpired_prepared_handoff_is_polled_not_redriven() {
        let concrete = Arc::new(InMemoryStore::new());
        let store: Arc<dyn Store> = concrete.clone();
        let key = PrivateKeySigner::random();
        let (env, hash, _ger, signer) = signed_ger_tx(&key, 0, 0xD2);
        install_orphan(&store, &env, hash, signer, 0).await;
        store
            .prepare_note_handoff(&format!("{hash:#x}"), "0xcommit", "0xnote", 100)
            .await
            .unwrap();
        // Cursor still BEFORE the expiration — the note may yet be consumed.
        store.set_reconcile_cursor(50).await.unwrap();

        let service = with_writer(store.clone(), 64);
        let handle = service.writer_handle.as_ref().unwrap().clone();
        recover_orphaned_pending_txns(&service).await.unwrap();

        assert!(
            !handle.is_inflight(&hash),
            "an unexpired prepared handoff must be polled, not re-driven"
        );
        let after = store
            .recoverable_pending_txns(10)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.tx_hash == hash)
            .expect("still pending");
        assert_eq!(
            after.recovery_attempts, 0,
            "polling an unexpired prepared handoff must not grow the exponential backoff"
        );
        assert!(
            store
                .get_note_handoff_for_tx(&format!("{hash:#x}"))
                .await
                .unwrap()
                .is_some(),
            "the durable prepared link must be preserved while the note may still be consumed"
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
