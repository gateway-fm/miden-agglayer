use crate::miden_client::MidenClient;
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, TxHash};
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use sha3::{Digest, Keccak256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Polling policy for the narrow strict-H6 state where a GER is already
/// corroborated by the L1 indexer but has not reached the configured finality
/// yet. Keeping the request pending is side-effect-free (no nonce, tx row, or
/// Miden submission exists yet) and prevents a one-shot aggoracle broadcast
/// from being lost in the observation-to-finality race.
const GER_FINALITY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const GER_FINALITY_WAIT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
enum GerL1GateError {
    #[error(
        "GER {ger} was observed on L1 but is not yet final per the `{evidence_tag}` evidence setting (finality guard, audit H6); refusing injection under --reject-unverified-ger-injection. Transient — retry once the L1 observation finalizes."
    )]
    NotFinal { ger: String, evidence_tag: String },
    #[error(
        "GER {ger} was not observed on L1 by the indexer (exit-root decomposition unresolved); refusing injection under --reject-unverified-ger-injection (audit H6). Retry after the L1 InfoTree indexer catches up."
    )]
    NotObserved { ger: String },
}

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
/// This depth is enforced at admission (`wait_for_ger_l1_observed`), NOT at the
/// indexer cursor: the indexer records the `(mainnet, rollup)` decomposition up
/// to LATEST so ordinary decomposition / bridge readiness
/// (`zkevm_getExitRootsByGER`) is never delayed. Only strict authorization waits
/// for finality. 64 ≈ Sepolia finality (justification ~1 epoch, finality ~2
/// epochs / 64 slots).
pub const DEFAULT_CONFIRMATIONS: u64 = 64;

/// The SINGLE strict-H6 evidence-finality setting (audit H6). One value fully
/// specifies how the gate qualifies an L1 observation as final enough to
/// authorize an irreversible GER injection — there is no second finality knob.
/// Parsed from `--l1-evidence-tag` / `L1_EVIDENCE_TAG`:
///   - `confirmations:<N>` — depth-below-head: the observation must be `N` blocks
///     below the indexer's head cursor (strict-non-hardened only).
///   - `finalized`         — the observation's `(mainnet, rollup)` must be on the
///     L1 FINALIZED canonical chain (BLOCKER 1 finalized-chain tie). MANDATORY
///     under `--require-hardening`.
///   - `safe`              — same, against the L1 `safe` block (weaker; not
///     sufficient for hardened).
///
/// Normal (lenient) decomposition (`zkevm_getExitRootsByGER`) never consults
/// this — it reads the evidence row directly, so bridge readiness is unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceTag {
    /// Depth `N` below the indexer head cursor.
    Confirmations(u64),
    /// On the L1 finalized canonical chain.
    Finalized,
    /// On the L1 safe canonical chain.
    Safe,
}

impl Default for EvidenceTag {
    fn default() -> Self {
        Self::Confirmations(DEFAULT_CONFIRMATIONS)
    }
}

impl EvidenceTag {
    /// True for the L1 finality-tag modes (`finalized` / `safe`), which qualify
    /// evidence against the finalized/safe canonical chain rather than a
    /// confirmation depth below head.
    pub fn is_finality_tag(self) -> bool {
        matches!(self, Self::Finalized | Self::Safe)
    }

    /// Human/log form, round-trippable through `parse`.
    pub fn describe(self) -> String {
        match self {
            Self::Confirmations(n) => format!("confirmations:{n}"),
            Self::Finalized => "finalized".to_string(),
            Self::Safe => "safe".to_string(),
        }
    }

    /// Parse the single CLI/env value. Accepts `finalized`, `safe`,
    /// `confirmations:<N>`, and bare `confirmations` (→ `DEFAULT_CONFIRMATIONS`).
    /// `None` on an unrecognised token or a malformed depth.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "finalized" => return Some(Self::Finalized),
            "safe" => return Some(Self::Safe),
            "confirmations" => {
                return Some(Self::Confirmations(DEFAULT_CONFIRMATIONS));
            }
            _ => {}
        }
        let rest = s.strip_prefix("confirmations:")?;
        rest.trim().parse::<u64>().ok().map(Self::Confirmations)
    }
}

/// Minimum confirmation depth strict H6 will boot with. Zero would authorize an
/// irreversible injection from a 0-confirmation (freely reorg-able) observation,
/// defeating the finality guarantee — so strict mode refuses to start with
/// `confirmations:<N>` where `N < MIN_STRICT_CONFIRMATIONS` (see
/// `check_h6_evidence_source`). Production should use `DEFAULT_CONFIRMATIONS` or
/// higher, or `finalized`; the floor merely forbids the outright-unsafe zero.
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
/// Crash-safe handoff: after local execution and proof succeed, BOTH the exact
/// note link and pending receipt are written inside this closure immediately
/// BEFORE `submit_proven_transaction`. A crash or ambiguous RPC result can
/// therefore never leave an externally submitted random note without its real
/// eth transaction identity, and a same-hash retry observes the link and does
/// not submit a second note. The serialized client also keeps projection behind
/// this handoff. This mirrors the claim submission boundary.
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
                let note_id = note.id().to_string();
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
                crate::miden_client::ensure_writable(ger_manager_id)?;
                let tx_result = client
                    .execute_transaction(ger_manager_id, tx_request)
                    .await?;
                let tx_id = tx_result.executed_transaction().id();
                let expiration_block = tx_result
                    .executed_transaction()
                    .expiration_block_num()
                    .as_u64();
                let proven_tx = crate::metrics::meter_proof(
                    crate::metrics::ProofKind::Ger,
                    client.prove_transaction(&tx_result),
                )
                .await?;

                // The note identity and pending receipt become durable immediately
                // before the first external submit. A crash after this point is
                // fail-closed: same-hash rebroadcasts observe the link and never
                // build a second random UpdateGerNote.
                record_ger_submission_handoff(
                    &*store,
                    txn_hash,
                    &note_commitment,
                    &note_id,
                    expiration_block,
                    txn_envelope,
                    signer,
                )
                .await?;
                let submission_height = client
                    .submit_proven_transaction(proven_tx, &tx_result)
                    .await?;
                client
                    .apply_transaction(&tx_result, submission_height)
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
                let tx_key = format!("{txn_hash:#x}");
                if !store
                    .confirm_note_handoff(&tx_key, &note_commitment)
                    .await?
                {
                    anyhow::bail!("GER note handoff changed before commit confirmation");
                }
                tracing::info!(tx_id = %tx_id, "UpdateGerNote transaction committed");
                Ok(())
            })
        })
        .await
}

/// Durable pre-submit handoff for an `UpdateGerNote`: record the exact note link
/// and idempotently create or enrich the pending receipt before the external
/// submit. In normal RPC flow the unlinked pending row already exists as the
/// durable admission intent. Link failure leaves that intent retryable; once the
/// link lands, recovery is fail-closed and cannot build a second random note.
/// The projector can always attribute a later consumption to the real eth hash.
pub(crate) async fn record_ger_submission_handoff(
    store: &dyn crate::store::Store,
    txn_hash: TxHash,
    note_commitment: &str,
    note_id: &str,
    expiration_block: u64,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    let tx_key = format!("{txn_hash:#x}");
    store
        .prepare_note_handoff(&tx_key, note_commitment, note_id, expiration_block)
        .await?;
    // `id: None` hides this row from the StoreSyncListener's commit-pending
    // sweep (which finalises by Miden tx id at the note's CREATION block);
    // the projector finalises it at the CONSUMPTION block instead — receipt
    // block == GER-log block. No `expires_at`: GER receipts are finalised by
    // consumption, not TTL (matches the pre-existing pending-row semantics).
    store
        .txn_begin_if_absent(
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
    evidence_tag: EvidenceTag,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<bool> {
    // Audit H6 gate (dedup-first — see `wait_for_ger_l1_observed`). In writer
    // mode the SAME gate already ran on the request path before
    // `try_enqueue`/`nonce_increment` (PR #121 review); this run is the sync
    // path's primary admission decision and the writer path's
    // defense-in-depth.
    wait_for_ger_l1_observed(
        store,
        &ger_bytes,
        require_l1_observed,
        evidence_tag,
        txn_hash,
    )
    .await?;

    // Dedup: decide whether this is a NEW injection.
    //
    // Use `is_ger_injected` (not `has_seen_ger`) because the L1InfoTreeIndexer
    // pre-creates ger_entries rows for every L1 InfoTree pair as it observes
    // them, even before the corresponding Miden inject happens. With
    // `has_seen_ger` we'd skip the actual Miden tx submission as a "duplicate"
    // and the synthetic L2 event would never be emitted, leaving deposits
    // stuck `ready_for_claim=false`. Gating on `is_injected = TRUE` correctly
    // reflects "have we already submitted the Miden tx and committed the
    // synthetic event for this GER?". (`wait_for_ger_l1_observed` above already
    // short-circuits on the same `is_ger_injected` check, so an already-injected
    // GER never reaches the gate's finality logic — this read then just decides
    // the duplicate no-op return value.)
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
                if store
                    .get_note_link_for_tx(&format!("{txn_hash:#x}"))
                    .await?
                    .is_some()
                {
                    tracing::error!(
                        %txn_hash, error = %err,
                        "GER submission outcome is ambiguous after durable handoff; refusing to rebuild a second note"
                    );
                    return Err(err);
                }
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
                // No durable link exists, so the failure occurred before the
                // external submission boundary and a fresh local retry is safe.
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
/// The mandatory-writer path checks twice:
///   - `service_send_raw_txn` calls it on the request thread
///     before `try_enqueue` (which would otherwise consume the nonce, admit
///     the hash into the inflight dedup cache, and return a hash whose
///     receipt could never be written — the aggoracle/ethtxmanager wedge).
///   - `insert_ger` repeats it inside the worker immediately before Miden
///     submission as a defense-in-depth state check.
///
/// An unobserved strict-mode refusal stays transient and side-effect-free: no nonce
/// is consumed, no tx row/receipt is created, and no job is queued. An already
/// observed but shallow GER instead waits here, also before any side effect,
/// until the configured evidence finality is reached.
///
/// Metric discipline: `ger_injection_unverified_total` increments on every
/// unverified sighting this function makes. The request path invokes it under
/// strict mode; a failed admission never reaches the worker, so one rejected
/// submission is not double-counted.
pub async fn ensure_ger_l1_observed(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
    require_l1_observed: bool,
    evidence_tag: EvidenceTag,
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
    // additionally requires the observation to be final per the SINGLE
    // `evidence_tag` setting:
    //   - Confirmations(N): `l1_head - evidence_block >= N`, using the indexer's
    //     persisted head cursor (strict-non-hardened only). An evidence row with
    //     NO recorded L1 block (`block_number == 0`: a legacy/pre-guard row, or
    //     one seeded by a non-indexer write path) is NOT final — fail-closed —
    //     which closes the upgrade-state gap.
    //   - Finalized / Safe (BLOCKER 1 finalized-chain tie): the row must be
    //     `finalized_verified` — a flag the indexer sets ONLY from a scan pinned
    //     to the L1 finalized/safe canonical chain. A `latest`-observed row from
    //     a fork that was later reorged away is never `finalized_verified`, so a
    //     block-height coincidence (`block <= finalized`) can no longer
    //     authorize it. MANDATORY under `--require-hardening`.
    // Lenient mode never gates on finality.
    let final_enough = if require_l1_observed {
        match entry.as_ref() {
            Some(e) => match evidence_tag {
                EvidenceTag::Confirmations(depth) => {
                    let l1_head = store.get_l1_indexer_cursor().await?;
                    e.block_number > 0 && l1_head.saturating_sub(e.block_number) >= depth
                }
                EvidenceTag::Finalized | EvidenceTag::Safe => e.finalized_verified,
            },
            None => false,
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
            // e2e / callers match; "not yet" for the not-final case.)
            if roots_observed {
                return Err(GerL1GateError::NotFinal {
                    ger: hex::encode(ger_bytes),
                    evidence_tag: evidence_tag.describe(),
                }
                .into());
            }
            return Err(GerL1GateError::NotObserved {
                ger: hex::encode(ger_bytes),
            }
            .into());
        }
        tracing::warn!(
            ger = %hex::encode(ger_bytes),
            tx = %txn_hash,
            roots_observed,
            evidence_tag = %evidence_tag.describe(),
            "GER injection not yet corroborated/finalized by the L1 InfoTree indexer; \
             allowing through but unverified (lenient mode)"
        );
    }
    Ok(())
}

/// Wait for an already-observed GER to reach the configured strict-H6 finality.
///
/// An unobserved GER is still refused immediately: it may be forged, and the
/// indexer has supplied no evidence that waiting is justified. Once both roots
/// are observed, however, finality is only a time-dependent gate. Keeping this
/// request pending is side-effect-free and closes the race where aggoracle sent
/// once between observation and the next indexer cursor advance, received a
/// transient refusal, and never produced another usable submission.
pub async fn wait_for_ger_l1_observed(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
    require_l1_observed: bool,
    evidence_tag: EvidenceTag,
    txn_hash: TxHash,
) -> anyhow::Result<()> {
    wait_for_ger_l1_observed_with_timing(
        store,
        ger_bytes,
        require_l1_observed,
        evidence_tag,
        txn_hash,
        GER_FINALITY_WAIT_TIMEOUT,
        GER_FINALITY_POLL_INTERVAL,
    )
    .await
}

async fn wait_for_ger_l1_observed_with_timing(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
    require_l1_observed: bool,
    evidence_tag: EvidenceTag,
    txn_hash: TxHash,
    timeout: Duration,
    poll_interval: Duration,
) -> anyhow::Result<()> {
    let pending_error = match ensure_ger_l1_observed(
        store,
        ger_bytes,
        require_l1_observed,
        evidence_tag,
        txn_hash,
    )
    .await
    {
        Ok(()) => return Ok(()),
        Err(err)
            if require_l1_observed
                && matches!(
                    err.downcast_ref::<GerL1GateError>(),
                    Some(GerL1GateError::NotFinal { .. })
                ) =>
        {
            err
        }
        Err(err) => return Err(err),
    };

    tracing::info!(
        ger = %hex::encode(ger_bytes),
        tx = %txn_hash,
        evidence_tag = %evidence_tag.describe(),
        timeout_secs = timeout.as_secs(),
        "GER is L1-observed but not final yet; waiting side-effect-free before admission"
    );

    let started = Instant::now();
    loop {
        if ger_l1_finality_reached(store, ger_bytes, evidence_tag).await? {
            tracing::info!(
                ger = %hex::encode(ger_bytes),
                waited_ms = started.elapsed().as_millis() as u64,
                "GER L1 evidence reached finality; continuing admission"
            );
            return Ok(());
        }
        if started.elapsed() >= timeout {
            tracing::warn!(
                ger = %hex::encode(ger_bytes),
                waited_secs = started.elapsed().as_secs(),
                "timed out waiting for GER L1 evidence finality"
            );
            return Err(pending_error);
        }
        tokio::time::sleep(poll_interval).await;
    }
}

async fn ger_l1_finality_reached(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
    evidence_tag: EvidenceTag,
) -> anyhow::Result<bool> {
    if store.is_ger_injected(ger_bytes).await? {
        return Ok(true);
    }
    let Some(entry) = store.get_ger_entry(ger_bytes).await? else {
        return Ok(false);
    };
    if entry.mainnet_exit_root.is_none() || entry.rollup_exit_root.is_none() {
        return Ok(false);
    }
    match evidence_tag {
        EvidenceTag::Confirmations(depth) => {
            let l1_head = store.get_l1_indexer_cursor().await?;
            Ok(entry.block_number > 0 && l1_head.saturating_sub(entry.block_number) >= depth)
        }
        EvidenceTag::Finalized | EvidenceTag::Safe => Ok(entry.finalized_verified),
    }
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
            EvidenceTag::Confirmations(0),
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
            EvidenceTag::Confirmations(0),
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
            EvidenceTag::Confirmations(0),
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
            EvidenceTag::Confirmations(0),
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
        let err = ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            EvidenceTag::Confirmations(DEFAULT_CONFIRMATIONS),
            tx_hash,
        )
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
        ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            EvidenceTag::Confirmations(DEFAULT_CONFIRMATIONS),
            tx_hash,
        )
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
                    finalized_verified: false,
                },
            )
            .await
            .unwrap();
        // Even with a huge cursor, block 0 means "no recorded L1 block" → refused.
        store.set_l1_indexer_cursor(10_000_000).await.unwrap();
        let tx_hash = TxHash::from([0x45u8; 32]);
        let err = ensure_ger_l1_observed(
            &store,
            &ger,
            true,
            EvidenceTag::Confirmations(DEFAULT_CONFIRMATIONS),
            tx_hash,
        )
        .await
        .expect_err("a block-less legacy row must be refused under strict");
        assert!(
            err.to_string().contains("not yet"),
            "finality-guard refusal: {err}"
        );
        // Lenient mode lets it through (no bail).
        ensure_ger_l1_observed(
            &store,
            &ger,
            false,
            EvidenceTag::Confirmations(DEFAULT_CONFIRMATIONS),
            tx_hash,
        )
        .await
        .expect("lenient mode must not refuse a block-less row");
    }

    /// Audit H6 BLOCKER 1 (re-review) — the `finalized` evidence tag qualifies an
    /// observation by the FINALIZED-CHAIN TIE (`finalized_verified`), NOT by a
    /// block-height coincidence. A `latest`-observed row (roots present, block
    /// recorded) that the finalized-pinned scan never confirmed — e.g. a row from
    /// a fork later reorged away — must NOT authorize even with a huge head
    /// cursor / finalized block. Only after `mark_ger_finalized` (which the
    /// indexer runs solely from the finalized canonical chain) does it authorize.
    ///
    /// Mutation check: making the gate authorize on block-height instead of
    /// `finalized_verified` (drop the flag requirement) makes the reorged-fork
    /// row wrongly authorize.
    #[tokio::test]
    async fn h6_finalized_tag_gate_requires_finalized_chain_tie() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x1Au8; 32], &[0x1Bu8; 32]);
        let tx_hash = TxHash::from([0x46u8; 32]);
        // A `latest`-observed row: roots present, block 100. Set the head cursor
        // AND finalized block far above it, so a height-only check would pass.
        store
            .set_ger_exit_roots(&ger, [0x1Au8; 32], [0x1Bu8; 32], 100, 1_700_000_000)
            .await
            .unwrap();
        store.set_l1_indexer_cursor(10_000).await.unwrap();
        store.set_l1_finalized_block(10_000).await.unwrap();

        // NOT finalized-verified (the finalized scan never covered it, e.g. a
        // reorged fork) → refused despite the height coincidence.
        let err = ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Finalized, tx_hash)
            .await
            .expect_err("a non-finalized-verified row must be refused in finalized mode");
        assert!(
            err.to_string().contains("not yet"),
            "finality-guard refusal: {err}"
        );

        // The finalized-pinned scan confirms it on the canonical chain.
        store.mark_ger_finalized(&ger).await.unwrap();
        ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Finalized, tx_hash)
            .await
            .expect("a finalized-chain-verified observation must pass the strict gate");
    }

    /// Audit H6 — confirmation-depth mode still works under the SINGLE setting:
    /// `Confirmations(N)` authorizes iff `head_cursor - block >= N`, and the
    /// `finalized_verified` flag is irrelevant in this mode.
    #[tokio::test]
    async fn h6_confirmations_mode_under_single_setting() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x2Au8; 32], &[0x2Bu8; 32]);
        let tx_hash = TxHash::from([0x47u8; 32]);
        store
            .set_ger_exit_roots(&ger, [0x2Au8; 32], [0x2Bu8; 32], 100, 1_700_000_000)
            .await
            .unwrap();

        // 40 deep < 64 → refused.
        store.set_l1_indexer_cursor(140).await.unwrap();
        ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Confirmations(64), tx_hash)
            .await
            .expect_err("shallow observation must be refused in confirmations mode");
        // 70 deep >= 64 → authorized (no finalized_verified needed).
        store.set_l1_indexer_cursor(170).await.unwrap();
        ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Confirmations(64), tx_hash)
            .await
            .expect("confirmation-deep observation must pass");
    }

    /// Regression for task #59: once the indexer has resolved both roots, a
    /// one-block finality race is held side-effect-free until the cursor advances.
    /// Before this wait, a one-shot aggoracle submission could be refused in this
    /// window and the deposit would remain stalled indefinitely.
    #[tokio::test]
    async fn h6_observed_ger_waits_for_confirmation_cursor() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x3Au8; 32], &[0x3Bu8; 32]);
        let tx_hash = TxHash::from([0x48u8; 32]);
        store
            .set_ger_exit_roots(&ger, [0x3Au8; 32], [0x3Bu8; 32], 100, 1_700_000_000)
            .await
            .unwrap();
        store.set_l1_indexer_cursor(100).await.unwrap();

        let advancing_store = store.clone();
        let advance = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            advancing_store.set_l1_indexer_cursor(101).await.unwrap();
        });

        wait_for_ger_l1_observed_with_timing(
            &store,
            &ger,
            true,
            EvidenceTag::Confirmations(1),
            tx_hash,
            Duration::from_secs(1),
            Duration::from_millis(5),
        )
        .await
        .expect("an observed GER must be admitted once its confirmation becomes final");
        advance.await.unwrap();
        assert_eq!(store.get_l1_indexer_cursor().await.unwrap(), 101);
    }

    /// Unknown roots are not eligible for the wait: they may be forged and must
    /// retain the strict H6 immediate-refusal behavior.
    #[tokio::test]
    async fn h6_unobserved_ger_does_not_enter_finality_wait() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = [0x49u8; 32];
        let tx_hash = TxHash::from([0x49u8; 32]);

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_ger_l1_observed_with_timing(
                &store,
                &ger,
                true,
                EvidenceTag::Confirmations(1),
                tx_hash,
                Duration::from_secs(60),
                Duration::from_millis(5),
            ),
        )
        .await
        .expect("an unobserved GER must be rejected without waiting");
        let err = result.expect_err("an unobserved GER must remain rejected");
        assert!(err.to_string().contains("not observed on L1"));
    }
}
