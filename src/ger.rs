use crate::miden_client::MidenClient;
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, TxHash};
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use sha3::{Digest, Keccak256};
use std::sync::Arc;

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L166
    #[derive(Debug)]
    function insertGlobalExitRoot(bytes32 root);
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L131
    #[derive(Debug)]
    function updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot);
}

/// Default L1 confirmation depth for STRICT H6 GER authorization (audit H6).
///
/// A Miden GER injection is IRREVERSIBLE and the evidence store has no
/// revoke/rollback, so under strict `--reject-unverified-ger-injection` an
/// observation must be at least this many L1 blocks deep before it authorizes an
/// injection — a short-lived reorg then cannot leave a stale "observed" row that
/// permanently authorizes a GER that never truly landed on canonical L1.
///
/// This depth is enforced AT THE GATE (`ensure_ger_l1_observed`), NOT at the
/// indexer cursor: the indexer records the `(mainnet, rollup)` decomposition up
/// to LATEST so ordinary decomposition / bridge readiness
/// (`zkevm_getExitRootsByGER`) is never delayed. Only strict authorization waits
/// for finality. 64 ≈ Sepolia finality (justification ~1 epoch, finality ~2
/// epochs / 64 slots).
pub const DEFAULT_CONFIRMATIONS: u64 = 64;

/// Minimum confirmation depth strict H6 will boot with. Zero would authorize an
/// irreversible injection from a 0-confirmation (freely reorg-able) observation,
/// defeating the finality guarantee — so strict mode refuses to start with
/// `L1_INDEXER_CONFIRMATIONS < MIN_STRICT_CONFIRMATIONS` (see
/// `check_h6_evidence_source`). Production should use `DEFAULT_CONFIRMATIONS` or
/// higher; the floor merely forbids the outright-unsafe zero.
pub const MIN_STRICT_CONFIRMATIONS: u64 = 1;

/// Compute the combined GER from mainnet and rollup exit roots.
pub fn combined_ger(mainnet: &[u8; 32], rollup: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(mainnet);
    hasher.update(rollup);
    hasher.finalize().into()
}

/// Submit the actual UpdateGerNote Miden transaction. Factored out of
/// `insert_ger` so the caller can run it twice — once eagerly, then again
/// after `reimport_account` if the first attempt failed with a recoverable
/// account-state error.
///
/// Use the long-lived MidenClient. The dedicated ger_manager account
/// (separate from the service account that the NTX builder constantly
/// mutates via claim processing) keeps the account state stable across
/// GER submissions, so we don't need a fresh client per call.
///
/// Fresh-client-per-GER was removed because it shared the main sqlite
/// and advanced the sync cursor past blocks where bridge NTX consumes
/// the UpdateGerNote. The main client's subsequent sync_nullifiers only
/// queries [current_cursor, tip], so those consumption events were never
/// discovered and `NoteFilter::Consumed` returned nothing in restore.
///
/// Also records the durable eth-tx handoff via
/// [`record_ger_submission_handoff`]: the eth-tx ↔ note link
/// (`record_tx_note_link`, keyed by the note's `details_commitment` — hex,
/// encoded identically to how the projector keys consumed notes,
/// `InputNoteRecord::details_commitment()`) AND the pending receipt row
/// (`txn_begin`).
///
/// Handoff-before-projection (PR #127 review, point 6 + follow-up): BOTH the
/// link and the pending receipt are written INSIDE this closure, immediately
/// after the Miden tx commits and strictly BEFORE the serialized
/// `MidenClient::with` slot is released. The SyntheticProjector observes note
/// consumption through the same serialized client, so it cannot tick between
/// the note landing and the handoff existing — the GER event always rides the
/// REAL eth-tx hash (never the derived fallback) and `txn_commit` always
/// finds the pending row to finalise. (Pre-fix, the link was recorded here
/// but the pending row was only created by `handle_ger_result` AFTER
/// `insert_ger` returned and released the client; the projector could acquire
/// the client in that gap, resolve the real linked hash, and try to finalise
/// a receipt whose `txn_begin` didn't exist yet — on PostgreSQL that
/// silently finalised zero rows and the late `txn_begin` then left the real
/// receipt pending forever.) Mirrors the identical pattern in
/// `claim::attempt_publish_claim`.
///
/// Cantina #21 (PR #127 review, points 1/4): this function deliberately does
/// NOT wait for the NTX builder to consume the note into the bridge account.
/// GER propagation is fail-fast/retry-later: `eth_estimateGas` and the C6
/// pre-admission gate reject claims until the projector publishes the GER,
/// and the on-chain MASM `assert_valid_ger` remains the final safety gate.
async fn submit_update_ger_note(
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: Arc<dyn crate::store::Store>,
    ger_bytes: [u8; 32],
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    let inner_accounts = accounts.0.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                let ger_manager_id = inner_accounts
                    .ger_manager
                    .as_ref()
                    .map(|a| a.0)
                    .unwrap_or(inner_accounts.service.0);
                let bridge_id = inner_accounts.bridge.0;
                let ger = ExitRoot::new(ger_bytes);
                let note = UpdateGerNote::create(ger, ger_manager_id, bridge_id, client.rng())?;
                // Commitment of the on-chain note, matching the projector's
                // consumed-note key (`InputNoteRecord::details_commitment()`).
                let note_commitment = hex::encode(
                    miden_protocol::note::NoteDetails::from(&note)
                        .commitment()
                        .as_bytes(),
                );
                tracing::info!(
                    note_id = %note.id(),
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote created"
                );
                let tx_request = TransactionRequestBuilder::new()
                    .own_output_notes(vec![note])
                    .build()?;
                let tx_id = crate::metrics::meter_proof(
                    crate::metrics::ProofKind::Ger,
                    crate::miden_client::submit_new_transaction(client, ger_manager_id, tx_request),
                )
                .await?;
                tracing::info!(
                    tx_id = %tx_id,
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote submitted, waiting for commit..."
                );

                let committed = crate::miden_client::wait_for_transaction_commit(
                    client,
                    tx_id,
                    30,
                    std::time::Duration::from_secs(1),
                )
                .await?;
                if !committed {
                    anyhow::bail!("UpdateGerNote tx {tx_id} not committed after 30s");
                }
                tracing::info!(tx_id = %tx_id, "UpdateGerNote transaction committed");

                // Record the durable handoff (link + pending receipt) WHILE
                // STILL HOLDING the serialized client — see the function
                // docstring (handoff-before-projection). Recording after the
                // commit (not before the submit) keeps the recoverable-retry
                // path in `insert_ger` correct: a failed attempt records
                // nothing, so the retry's fresh note gets the first (and
                // only) link for this eth-tx and a single pending row.
                record_ger_submission_handoff(
                    &*store,
                    txn_hash,
                    &note_commitment,
                    txn_envelope,
                    signer,
                )
                .await?;
                Ok(())
            })
        })
        .await
}

/// The durable eth-tx handoff for a committed `UpdateGerNote` submission:
/// record the eth-tx ↔ note link AND create the pending receipt row that the
/// SyntheticProjector finalises (`txn_commit`) when it observes the note
/// consumed. This is the EXACT code `submit_update_ger_note` runs inside the
/// serialized `MidenClient::with` closure, factored out so tests can drive
/// the real submission ordering (handoff → client released → projection)
/// without a live Miden node.
///
/// Ordering within the handoff — link FIRST, pending row SECOND — is what
/// makes a partial failure retry-safe (no stray pending row):
///
///   - Link write fails → nothing durable exists; a resubmission of the same
///     eth-tx re-runs the whole path cleanly.
///   - Link lands but `txn_begin` fails → there is a link but NO pending row.
///     The projector tolerates the missing row (`let _ = txn_commit` in
///     `project_ger_note`; both stores now error identically on the missing
///     row) and still emits the GER event under the real linked hash, from
///     which `service_get_txn_receipt` can synthesise the receipt. A
///     resubmission re-runs the path: `record_tx_note_link` is first-write-
///     wins (no-op) and `txn_begin` creates the one pending row.
///   - The reverse order would allow the original bug shape: a pending row
///     with no link, which the projector can never finalise (derived-hash
///     fallback) — pending forever.
pub(crate) async fn record_ger_submission_handoff(
    store: &dyn crate::store::Store,
    txn_hash: TxHash,
    note_commitment: &str,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    store
        .record_tx_note_link(&format!("{txn_hash:#x}"), note_commitment)
        .await?;
    // `id: None` hides this row from the StoreSyncListener's commit-pending
    // sweep (which finalises by Miden tx id at the note's CREATION block);
    // the projector finalises it at the CONSUMPTION block instead — receipt
    // block == GER-log block. No `expires_at`: GER receipts are finalised by
    // consumption, not TTL (matches the pre-existing pending-row semantics).
    store
        .txn_begin(
            txn_hash,
            crate::store::TxnEntry {
                id: None,
                envelope: txn_envelope,
                signer,
                expires_at: None,
                logs: vec![],
            },
        )
        .await?;
    Ok(())
}

/// Submit a GER injection to Miden. Returns `true` if a new `UpdateGerNote` was
/// submitted (and the real eth-tx ↔ note link + pending receipt recorded so the
/// projector finalises the receipt + emits the GER log on consumption), `false`
/// if the GER was already injected (a duplicate — the caller completes its
/// receipt immediately).
///
/// Audit H6 — `require_l1_observed` cross-checks the injected GER against the
/// L1 InfoTree the indexer independently observed. The aggoracle-supplied GER
/// bytes are otherwise trusted verbatim: a compromised signer could inject a
/// FORGED GER (one whose `(mainnet, rollup)` decomposition the indexer never saw
/// on L1) onto Miden. The indexer writes the authoritative decomposition via
/// `set_ger_exit_roots`; a GER is "resolved" only when BOTH roots are recorded —
/// the same predicate `zkevm_getExitRootsByGER` answers with (anything less
/// returns null there so bridge-service retries). When `require_l1_observed` is
/// set, an unresolved GER is refused before it reaches Miden; otherwise it is
/// allowed through (to tolerate indexer lag) but flagged via the
/// `ger_injection_unverified_total` metric + warn.
///
/// The duplicate check runs BEFORE the H6 gate: an already-injected GER is a
/// no-op (`false`) regardless of verification state. The gate exists to stop
/// NEW submissions to Miden — a duplicate never reaches Miden, and refusing it
/// would break idempotency: the aggoracle re-submits GERs it cannot confirm
/// (restart with a stale view, restore replay), and an error here would put it
/// in a permanent retry loop over an injection that already happened.
// Two review threads (H6 `require_l1_observed` from #121, envelope+signer handoff
// from #127) each added a parameter to this already-wide submission entry point.
#[allow(clippy::too_many_arguments)]
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    txn_hash: TxHash,
    require_l1_observed: bool,
    confirmations: u64,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<bool> {
    // Audit H6 gate (dedup-first — see `ensure_ger_l1_observed`). In writer
    // mode the SAME gate already ran on the request path before
    // `try_enqueue`/`nonce_increment` (PR #121 review); this run is the sync
    // path's primary admission decision and the writer path's
    // defense-in-depth.
    ensure_ger_l1_observed(
        store,
        &ger_bytes,
        require_l1_observed,
        confirmations,
        txn_hash,
    )
    .await?;

    // Check dedup before doing any work.
    //
    // Use `is_ger_injected` (not `has_seen_ger`) because the L1InfoTreeIndexer
    // pre-creates ger_entries rows for every L1 InfoTree pair as it observes
    // them, even before the corresponding Miden inject happens. With
    // `has_seen_ger` we'd skip the actual Miden tx submission as a "duplicate"
    // and the synthetic L2 event would never be emitted, leaving deposits
    // stuck `ready_for_claim=false`. Gating on `is_injected = TRUE` correctly
    // reflects "have we already submitted the Miden tx and committed the
    // synthetic event for this GER?".
    let is_new = !store.is_ger_injected(&ger_bytes).await?;

    if is_new {
        tracing::info!(
            ger = %hex::encode(ger_bytes),
            "GER injection: submitting to Miden..."
        );

        // Submit with runtime self-heal: if the Miden submission rejects
        // with AccountDataNotFound (local sqlite missing the account row)
        // OR IncorrectAccountInitialCommitment (local commitment stale vs
        // the node's view), reimport the ger_manager account from the
        // live Miden node and retry once. See `src/account_recovery.rs`
        // for the analysis — this is the actual bali production cure.
        //
        // The eth-tx ↔ UpdateGerNote link AND the pending receipt row (which
        // let the SyntheticProjector finalise THIS receipt and emit the GER
        // log under the real tx hash on consumption, making receipt block ==
        // GER-log block) are recorded by `submit_update_ger_note` itself,
        // while it still holds the serialized Miden client — see its
        // docstring (handoff-before-projection).
        match submit_update_ger_note(
            miden_client,
            accounts.clone(),
            store.clone(),
            ger_bytes,
            txn_hash,
            txn_envelope.clone(),
            signer,
        )
        .await
        {
            Ok(()) => {}
            Err(err) if crate::account_recovery::is_recoverable_account_error(&err) => {
                tracing::warn!(
                    err = %err,
                    ger = %hex::encode(ger_bytes),
                    "GER injection: recoverable account error, reimporting ger_manager and retrying"
                );
                let ger_manager_id = accounts
                    .0
                    .ger_manager
                    .as_ref()
                    .map(|a| a.0)
                    .unwrap_or(accounts.0.service.0);
                crate::account_recovery::reimport_account(
                    miden_client,
                    ger_manager_id,
                    "ger_manager",
                )
                .await?;
                // Retry-safety: the first attempt failed BEFORE its handoff
                // (account errors surface at submit/commit time, and the
                // handoff only runs after commit confirmation), so no link or
                // pending row exists yet — the retry's fresh note records
                // both exactly once.
                submit_update_ger_note(
                    miden_client,
                    accounts.clone(),
                    store.clone(),
                    ger_bytes,
                    txn_hash,
                    txn_envelope,
                    signer,
                )
                .await?
            }
            Err(err) => return Err(err),
        };
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    Ok(is_new)
}

/// Audit H6 — the pre-admission L1-corroboration gate for GER injections
/// (PR #121 review: the gate MUST run before every enqueue path, nonce
/// increment, txn_begin, or receipt creation).
///
/// Verifies the GER was observed on L1 by the independent L1InfoTreeIndexer
/// (it writes the `(mainnet, rollup)` decomposition via `set_ger_exit_roots`).
/// "Observed" means BOTH roots resolved — the same predicate
/// `zkevm_getExitRootsByGER` uses (ger_entries rows exist in partial states:
/// the indexer pre-creates them with roots to be filled in later). A GER with
/// no resolved decomposition was supplied only by the aggoracle and never
/// corroborated by an L1 observation — a forged-GER injection signal.
///
/// The duplicate check runs FIRST: an already-injected GER never reaches
/// Miden, so the gate has nothing to stop — and refusing it would break
/// idempotency (the aggoracle re-submits GERs it cannot confirm after a
/// restart or restore replay, and an error here would wedge it in a permanent
/// retry loop over an injection that already happened).
///
/// Call sites (mirroring the C6 claim gate's two-path layout):
///   - sync path: `insert_ger` calls it at the top, before any Miden
///     submission; the dispatcher returns the error before `nonce_increment`
///     and before any `txn_begin`, so a strict-mode rejection is
///     side-effect-free.
///   - writer path: `service_send_raw_txn` calls it on the REQUEST thread
///     before `try_enqueue` (which would otherwise consume the nonce, admit
///     the hash into the inflight dedup cache, and return a hash whose
///     receipt could never be written — the aggoracle/ethtxmanager wedge).
///     The `insert_ger` call inside the worker remains as defense-in-depth.
///
/// A strict-mode refusal must stay TRANSIENT: no nonce is consumed, no tx
/// row/receipt is created, no job is queued — the identical signed
/// transaction (same nonce) is accepted verbatim once the indexer catches up.
///
/// Metric discipline: `ger_injection_unverified_total` increments on every
/// unverified sighting this function makes. The writer-mode request path only
/// invokes it under strict mode (where a failed gate bails before the worker
/// ever runs), so the counter never double-counts one submission.
pub async fn ensure_ger_l1_observed(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
    require_l1_observed: bool,
    confirmations: u64,
    txn_hash: TxHash,
) -> anyhow::Result<()> {
    // Dedup precedence — duplicates never reach Miden; see doc above.
    if store.is_ger_injected(ger_bytes).await? {
        return Ok(());
    }
    let entry = store.get_ger_entry(ger_bytes).await?;
    let roots_observed = entry
        .as_ref()
        .is_some_and(|e| e.mainnet_exit_root.is_some() && e.rollup_exit_root.is_some());

    // Finality is checked HERE (at the strict gate), NOT at the indexer cursor.
    // The indexer records the `(mainnet, rollup)` decomposition up to LATEST, so
    // ordinary decomposition / bridge readiness (`zkevm_getExitRootsByGER`, which
    // reads `get_ger_entry` directly and does NOT call this gate) is never
    // delayed. Only strict authorization of an IRREVERSIBLE injection
    // additionally requires the observation to be confirmation-deep on L1:
    //   l1_head - evidence_block >= confirmations,
    // using the indexer's persisted cursor as the observed L1 head. An evidence
    // row with NO recorded L1 block (`block_number == 0`: a legacy/pre-guard row,
    // or one seeded by a non-indexer write path) is treated as NOT final —
    // fail-closed — which also closes the upgrade-state gap for rows written
    // before this guard existed. Lenient mode never gates on finality.
    let final_enough = if require_l1_observed {
        match entry.as_ref() {
            Some(e) if e.block_number > 0 => {
                let l1_head = store.get_l1_indexer_cursor().await.unwrap_or(0);
                l1_head.saturating_sub(e.block_number) >= confirmations
            }
            _ => false,
        }
    } else {
        true
    };

    let l1_verified = roots_observed && final_enough;
    if !l1_verified {
        ::metrics::counter!("ger_injection_unverified_total").increment(1);
        if require_l1_observed {
            // A resolved-but-not-yet-final observation is a DISTINCT transient
            // state from an unresolved one; surface it plainly. ("not observed
            // on L1" stays the stable substring for the unresolved case that the
            // e2e / callers match.)
            if roots_observed {
                anyhow::bail!(
                    "GER {} was observed on L1 but is not yet {confirmations} confirmations \
                     deep (finality guard, audit H6); refusing injection under \
                     --reject-unverified-ger-injection. Transient — retry once the L1 \
                     observation finalizes.",
                    hex::encode(ger_bytes)
                );
            }
            anyhow::bail!(
                "GER {} was not observed on L1 by the indexer (exit-root decomposition \
                 unresolved); refusing injection under --reject-unverified-ger-injection \
                 (audit H6). Retry after the L1 InfoTree indexer catches up.",
                hex::encode(ger_bytes)
            );
        }
        tracing::warn!(
            ger = %hex::encode(ger_bytes),
            tx = %txn_hash,
            roots_observed,
            "GER injection not yet corroborated/finalized by the L1 InfoTree indexer; \
             allowing through but unverified (lenient mode)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use std::str::FromStr;
    use std::sync::Arc;

    /// Minimal signed legacy envelope + signer for `insert_ger` calls (the H6
    /// gate runs before Miden submission, so the stub client never executes the
    /// envelope — only its shape/signer matter). Keyed to `tx_hash` so the
    /// handoff records the real linked hash. Mirrors `restore::test_ger_envelope`.
    fn h6_test_envelope(tx_hash: TxHash) -> (TxEnvelope, Address) {
        use alloy::consensus::{Signed, TxLegacy};
        use alloy::primitives::Signature;
        let env = TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(1),
                ..Default::default()
            },
            Signature::test_signature(),
            tx_hash,
        ));
        (env, Address::ZERO)
    }

    #[test]
    fn test_combined_ger_keccak256() {
        let mainnet = [0x01u8; 32];
        let rollup = [0x02u8; 32];
        let result = combined_ger(&mainnet, &rollup);

        // Verify against direct keccak256 computation
        let mut hasher = Keccak256::new();
        hasher.update(mainnet);
        hasher.update(rollup);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_combined_ger_deterministic() {
        let mainnet = [0xAAu8; 32];
        let rollup = [0xBBu8; 32];
        assert_eq!(
            combined_ger(&mainnet, &rollup),
            combined_ger(&mainnet, &rollup)
        );
    }

    #[test]
    fn test_combined_ger_order_matters() {
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];
        assert_ne!(combined_ger(&a, &b), combined_ger(&b, &a));
    }

    /// Audit H6 — a GER whose `(mainnet, rollup)` decomposition was NOT
    /// corroborated by the L1 InfoTree indexer MUST be refused when
    /// `require_l1_observed` is set, BEFORE any Miden submission is attempted.
    /// Pre-fix, aggoracle-supplied GER bytes were trusted verbatim — a
    /// compromised signer could inject a forged GER onto Miden (state pollution,
    /// gas burn, and — with a colluding claim — a mint against an L1 deposit
    /// that never happened).
    ///
    /// The check fires at the top of `insert_ger`, so the MidenClient is never
    /// reached; a stub client is sufficient.
    #[tokio::test]
    async fn h6_unverified_ger_refused_when_strict() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let miden_client = crate::test_helpers::create_test_service().miden_client;
        let accounts = crate::test_helpers::test_accounts_config();
        let tx_hash = alloy::primitives::TxHash::from_str(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let forged_ger = [0xCDu8; 32]; // no ger_entries row → mainnet_exit_root unset

        // Strict mode: the unverified GER must be refused before Miden submission.
        let (env, signer) = h6_test_envelope(tx_hash);
        let err = insert_ger(
            forged_ger,
            &miden_client,
            accounts.clone(),
            &store,
            tx_hash,
            true, // require_l1_observed
            0,    // confirmations (irrelevant: no roots observed)
            env.clone(),
            signer,
        )
        .await
        .expect_err("unverified GER must be refused under require_l1_observed");
        let msg = err.to_string();
        assert!(
            msg.contains("not observed on L1"),
            "must cite L1 non-observation: {msg}"
        );

        // Lenient mode (default): the same GER is allowed through (it may still
        // Err downstream because the MidenClient stub can't really submit, but
        // it must NOT bail at the H6 gate). Assert the result is NOT the H6
        // "not observed on L1" refusal — a bare `let _ =` would pass even if
        // lenient mode wrongly refused, defeating the point of this test.
        let lenient = insert_ger(
            forged_ger,
            &miden_client,
            accounts,
            &store,
            tx_hash,
            false, // lenient
            0,     // confirmations (lenient never gates on finality)
            env,
            signer,
        )
        .await;
        if let Err(err) = lenient {
            assert!(
                !err.to_string().contains("not observed on L1"),
                "lenient mode must NOT refuse an unverified GER at the H6 gate: {err}"
            );
        }
    }

    /// Audit H6 (review follow-up) — the duplicate check runs BEFORE the strict
    /// gate. A GER that is already injected must be a no-op (`Ok(false)`) even
    /// when its exit-root decomposition never resolved: refusing it would break
    /// idempotency, and the aggoracle — which re-submits GERs it cannot confirm
    /// after a restart or restore replay — would loop forever retrying an
    /// injection that already happened (the gate outcome can never change if
    /// the roots never resolve).
    #[tokio::test]
    async fn h6_already_injected_ger_is_duplicate_not_refused_under_strict() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let miden_client = crate::test_helpers::create_test_service().miden_client;
        let accounts = crate::test_helpers::test_accounts_config();
        let tx_hash = alloy::primitives::TxHash::from_str(
            "0x2222222222222222222222222222222222222222222222222222222222222222",
        )
        .unwrap();
        let ger = [0xABu8; 32];

        // Injected on a previous run, decomposition never resolved (None, None)
        // — the exact state that pre-fix wedged aggoracle in a retry loop.
        store
            .commit_ger_event_atomic(1, [0u8; 32], "0xTxDup", &ger, None, None, 0)
            .await
            .unwrap();

        let (env, signer) = h6_test_envelope(tx_hash);
        let result = insert_ger(
            ger,
            &miden_client,
            accounts,
            &store,
            tx_hash,
            true,
            0, // confirmations (dedup precedence returns before the finality check)
            env,
            signer,
        )
        .await
        .expect("already-injected GER must be a duplicate no-op, not an H6 refusal");
        assert!(
            !result,
            "duplicate injection must return false (no new note)"
        );
    }

    /// Audit H6 (review follow-up) — the gate uses the SAME resolved predicate
    /// as `zkevm_getExitRootsByGER`: BOTH roots recorded. An entry the indexer
    /// fully resolved must pass the strict gate (any downstream error from the
    /// stub MidenClient must not be the H6 refusal).
    #[tokio::test]
    async fn h6_resolved_ger_passes_strict_gate() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let miden_client = crate::test_helpers::create_test_service().miden_client;
        let accounts = crate::test_helpers::test_accounts_config();
        let tx_hash = alloy::primitives::TxHash::from_str(
            "0x3333333333333333333333333333333333333333333333333333333333333333",
        )
        .unwrap();
        let mainnet = [0x0Au8; 32];
        let rollup = [0x0Bu8; 32];
        let ger = combined_ger(&mainnet, &rollup);

        // The indexer observed the pair on L1 and recorded the decomposition.
        store
            .set_ger_exit_roots(&ger, mainnet, rollup, 100, 1_700_000_000)
            .await
            .unwrap();

        let (env, signer) = h6_test_envelope(tx_hash);
        let result = insert_ger(
            ger,
            &miden_client,
            accounts,
            &store,
            tx_hash,
            true,
            0, // confirmations (finality asserted separately below)
            env,
            signer,
        )
        .await;
        if let Err(err) = result {
            assert!(
                !err.to_string().contains("not observed on L1"),
                "a fully-resolved GER must pass the strict H6 gate: {err}"
            );
        }
    }

    /// Audit H6 (BLOCKER 1) — finality is enforced AT THE GATE. A resolved
    /// observation must be at least `confirmations` L1 blocks deep (indexer
    /// cursor as head) before strict mode authorizes the irreversible injection.
    /// A not-yet-deep observation is refused (transient), and once the cursor
    /// advances past the confirmation depth it passes. Ordinary decomposition
    /// (`get_ger_entry`, unchanged) sees the row regardless — this delay applies
    /// ONLY to strict authorization.
    ///
    /// Mutation check: dropping the `l1_head - block >= confirmations` clause
    /// (always `final_enough = true`) makes the not-yet-deep case wrongly pass.
    #[tokio::test]
    async fn h6_strict_gate_requires_confirmation_depth() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x0Au8; 32], &[0x0Bu8; 32]);
        let tx_hash = TxHash::from([0x44u8; 32]);
        // Indexer recorded the pair at L1 block 100 (roots resolved immediately).
        store
            .set_ger_exit_roots(&ger, [0x0Au8; 32], [0x0Bu8; 32], 100, 1_700_000_000)
            .await
            .unwrap();

        // Cursor at 140 → only 40 deep, < 64 confirmations → refused (transient).
        store.set_l1_indexer_cursor(140).await.unwrap();
        let err = ensure_ger_l1_observed(&store, &ger, true, DEFAULT_CONFIRMATIONS, tx_hash)
            .await
            .expect_err("a not-yet-confirmation-deep observation must be refused under strict");
        assert!(
            err.to_string().contains("not yet"),
            "must cite the finality guard, not unresolved roots: {err}"
        );
        // Normal decomposition is unaffected: the row is present immediately.
        assert!(store.get_ger_entry(&ger).await.unwrap().is_some());

        // Cursor advances to 170 → 70 deep, >= 64 → authorized.
        store.set_l1_indexer_cursor(170).await.unwrap();
        ensure_ger_l1_observed(&store, &ger, true, DEFAULT_CONFIRMATIONS, tx_hash)
            .await
            .expect("a confirmation-deep observation must pass the strict gate");
    }

    /// Audit H6 (BLOCKER 1) — a legacy / pre-guard evidence row that carries NO
    /// recorded L1 block (`block_number == 0`) but somehow has both roots must be
    /// treated as unverified under strict (fail-closed), closing the
    /// upgrade-state gap. Lenient mode still lets it through.
    ///
    /// Mutation check: treating a block-less row as final (removing the
    /// `block_number > 0` guard) makes the strict case wrongly pass.
    #[tokio::test]
    async fn h6_strict_gate_refuses_blockless_legacy_row() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = [0x77u8; 32];
        // A row with both roots but block_number 0 (e.g. seeded by a non-indexer
        // path before this guard existed). commit_ger_event_atomic sets roots +
        // block 0 here and does NOT set is_injected... so use mark_ger_seen shape.
        store
            .mark_ger_seen(
                &ger,
                crate::log_synthesis::GerEntry {
                    mainnet_exit_root: Some([0x01u8; 32]),
                    rollup_exit_root: Some([0x02u8; 32]),
                    block_number: 0,
                    timestamp: 0,
                },
            )
            .await
            .unwrap();
        // Even with a huge cursor, block 0 means "no recorded L1 block" → refused.
        store.set_l1_indexer_cursor(10_000_000).await.unwrap();
        let tx_hash = TxHash::from([0x45u8; 32]);
        let err = ensure_ger_l1_observed(&store, &ger, true, DEFAULT_CONFIRMATIONS, tx_hash)
            .await
            .expect_err("a block-less legacy row must be refused under strict");
        assert!(
            err.to_string().contains("not yet"),
            "finality-guard refusal: {err}"
        );
        // Lenient mode lets it through (no bail).
        ensure_ger_l1_observed(&store, &ger, false, DEFAULT_CONFIRMATIONS, tx_hash)
            .await
            .expect("lenient mode must not refuse a block-less row");
    }
}
