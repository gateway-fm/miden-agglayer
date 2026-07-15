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

/// Polling policy while the single configured L1 scan catches up to a GER.
/// Waiting is side-effect-free: no nonce, transaction row, writer job, or Miden
/// submission exists until the selected `latest` / `safe` / `finalized` scan
/// has persisted both roots.
#[cfg(not(test))]
const GER_EVIDENCE_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(test)]
const GER_EVIDENCE_POLL_INTERVAL: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const GER_EVIDENCE_WAIT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
// Request-path tests exercise timeout behavior without waiting 15 minutes.
#[cfg(test)]
const GER_EVIDENCE_WAIT_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
enum GerL1GateError {
    #[error(
        "GER {ger} was not observed by the configured L1 `{evidence_tag}` scan (exit-root decomposition unresolved); refusing injection under --reject-unverified-ger-injection (audit H6). Retry after that scan catches up."
    )]
    NotObserved { ger: String, evidence_tag: String },
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

/// The single L1 evidence scan setting (audit H6). The indexer scans exactly one
/// canonical frontier and stores roots only from that frontier:
/// Parsed from `--l1-evidence-tag` / `L1_EVIDENCE_TAG`:
///   - `latest` — lowest latency; may include reorgable L1 blocks.
///   - `safe` — scan only through the L1 safe head.
///   - `finalized` — scan only through the L1 finalized head.
///
/// `safe` and `finalized` satisfy `--require-hardening`; `latest` does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvidenceTag {
    #[default]
    Latest,
    Safe,
    /// On the L1 finalized canonical chain.
    Finalized,
}

impl EvidenceTag {
    /// Human/log form, round-trippable through `parse`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Safe => "safe",
            Self::Finalized => "finalized",
        }
    }

    /// Parse the single CLI/env value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "latest" => Some(Self::Latest),
            "safe" => Some(Self::Safe),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }
}

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
/// `set_ger_exit_roots`; strict admission requires BOTH roots plus the
/// database-bound selected-scan provenance marker. When `require_l1_observed`
/// is set, a GER without that evidence is refused before it reaches Miden;
/// otherwise it is allowed through (to tolerate indexer lag) but flagged via the
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
    // GER never reaches the gate's evidence check — this read then just decides
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
/// (it atomically writes the `(mainnet, rollup)` decomposition and selected-scan
/// provenance via `set_ger_exit_roots`). A row populated by another path, or by
/// a pre-upgrade `latest` scan, is not sufficient until the configured scan
/// rewrites it. Missing selected-scan evidence is a forged-GER injection signal.
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
/// A strict-mode wait/refusal stays side-effect-free: no nonce is consumed, no
/// tx row/receipt is created, and no job is queued. This matters for `safe` and
/// `finalized`, where a legitimate event is intentionally absent until the
/// selected L1 frontier reaches it.
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

    // One scan, one policy, one provenance marker. The selected scan writes
    // roots and the provenance marker together, so old `latest` roots from an
    // upgraded database cannot be silently reinterpreted as `safe`/`finalized`
    // evidence. The physical column retains its migration-era name, but now
    // means "observed by the configured scan" for all three tags.
    let policy_observed = entry.as_ref().is_some_and(|e| e.evidence_verified);
    let l1_verified = roots_observed && policy_observed;
    if !l1_verified {
        ::metrics::counter!("ger_injection_unverified_total").increment(1);
        if require_l1_observed {
            return Err(GerL1GateError::NotObserved {
                ger: hex::encode(ger_bytes),
                evidence_tag: evidence_tag.describe().to_string(),
            }
            .into());
        }
        tracing::warn!(
            ger = %hex::encode(ger_bytes),
            tx = %txn_hash,
            roots_observed,
            evidence_tag = %evidence_tag.describe(),
            policy_observed,
            "GER injection not yet corroborated by the configured L1 InfoTree scan; \
             allowing through but unverified (lenient mode)"
        );
    }
    Ok(())
}

/// Wait for the configured single L1 scan to persist a GER. Before the selected
/// `safe`/`finalized` frontier reaches a legitimate event it is indistinguishable
/// from an unknown GER, so the bounded wait covers both missing roots and a
/// missing provenance marker. The signer allow-list and timeout bound exposure.
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
        GER_EVIDENCE_WAIT_TIMEOUT,
        GER_EVIDENCE_POLL_INTERVAL,
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
        Err(err) if require_l1_observed && err.downcast_ref::<GerL1GateError>().is_some() => err,
        Err(err) => return Err(err),
    };

    tracing::info!(
        ger = %hex::encode(ger_bytes),
        tx = %txn_hash,
        evidence_tag = %evidence_tag.describe(),
        timeout_secs = timeout.as_secs(),
        "waiting side-effect-free for the configured L1 scan to observe GER"
    );

    let started = Instant::now();
    loop {
        if ger_l1_evidence_reached(store, ger_bytes).await? {
            tracing::info!(
                ger = %hex::encode(ger_bytes),
                waited_ms = started.elapsed().as_millis() as u64,
                "configured L1 scan observed GER; continuing admission"
            );
            return Ok(());
        }
        if started.elapsed() >= timeout {
            tracing::warn!(
                ger = %hex::encode(ger_bytes),
                waited_secs = started.elapsed().as_secs(),
                "timed out waiting for configured L1 scan evidence"
            );
            return Err(pending_error);
        }
        tokio::time::sleep(poll_interval).await;
    }
}

async fn ger_l1_evidence_reached(
    store: &Arc<dyn crate::store::Store>,
    ger_bytes: &[u8; 32],
) -> anyhow::Result<bool> {
    if store.is_ger_injected(ger_bytes).await? {
        return Ok(true);
    }
    let Some(entry) = store.get_ger_entry(ger_bytes).await? else {
        return Ok(false);
    };
    Ok(entry.mainnet_exit_root.is_some()
        && entry.rollup_exit_root.is_some()
        && entry.evidence_verified)
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

    #[test]
    fn evidence_tag_accepts_only_scan_frontiers() {
        assert_eq!(EvidenceTag::parse("latest"), Some(EvidenceTag::Latest));
        assert_eq!(EvidenceTag::parse(" SAFE "), Some(EvidenceTag::Safe));
        assert_eq!(
            EvidenceTag::parse("FINALIZED"),
            Some(EvidenceTag::Finalized)
        );
        assert_eq!(EvidenceTag::parse("confirmations:64"), None);
        assert_eq!(EvidenceTag::parse("confirmations"), None);
    }

    /// Audit H6 — a GER whose `(mainnet, rollup)` decomposition was NOT
    /// corroborated by the L1 InfoTree indexer MUST be refused when
    /// `require_l1_observed` is set, BEFORE any Miden submission is attempted.
    /// Pre-fix, aggoracle-supplied GER bytes were trusted verbatim — a
    /// compromised signer could inject a forged GER onto Miden (state pollution,
    /// gas burn, and — with a colluding claim — a mint against an L1 deposit
    /// that never happened).
    ///
    #[tokio::test]
    async fn h6_unverified_ger_refused_when_strict() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let tx_hash = alloy::primitives::TxHash::from_str(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let forged_ger = [0xCDu8; 32]; // no ger_entries row → mainnet_exit_root unset

        let err = ensure_ger_l1_observed(&store, &forged_ger, true, EvidenceTag::Latest, tx_hash)
            .await
            .expect_err("unverified GER must be refused under require_l1_observed");
        let msg = err.to_string();
        assert!(
            msg.contains("not observed by the configured L1 `latest` scan"),
            "must cite L1 non-observation: {msg}"
        );

        ensure_ger_l1_observed(&store, &forged_ger, false, EvidenceTag::Latest, tx_hash)
            .await
            .expect("lenient mode must allow unverified GER evidence");
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
            EvidenceTag::Latest,
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

    /// Audit H6 (review follow-up) — an entry fully written by the selected scan
    /// (both roots plus provenance) must pass the strict gate. Any downstream
    /// error from the stub MidenClient must not be the H6 refusal.
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
            EvidenceTag::Latest,
            env,
            signer,
        )
        .await;
        if let Err(err) = result {
            assert!(
                !err.to_string()
                    .contains("not observed by the configured L1"),
                "a fully-resolved GER must pass the strict H6 gate: {err}"
            );
        }
    }

    /// Pre-upgrade roots have no selected-scan provenance and must remain
    /// untrusted until the configured scan rewrites them. This is what prevents
    /// old `latest` observations being silently relabelled `safe`/`finalized`.
    #[tokio::test]
    async fn h6_strict_gate_requires_selected_scan_provenance() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = [0x77u8; 32];
        let mainnet = [0x01u8; 32];
        let rollup = [0x02u8; 32];
        store
            .mark_ger_seen(
                &ger,
                crate::log_synthesis::GerEntry {
                    mainnet_exit_root: Some(mainnet),
                    rollup_exit_root: Some(rollup),
                    block_number: 100,
                    timestamp: 1_700_000_000,
                    evidence_verified: false,
                },
            )
            .await
            .unwrap();
        let tx_hash = TxHash::from([0x45u8; 32]);
        let err = ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Safe, tx_hash)
            .await
            .expect_err("roots without selected-scan provenance must be refused");
        assert!(
            err.to_string()
                .contains("not observed by the configured L1 `safe` scan"),
            "selected-scan refusal: {err}"
        );

        // The selected scan atomically rewrites roots plus its provenance marker.
        store
            .set_ger_exit_roots(&ger, mainnet, rollup, 100, 1_700_000_000)
            .await
            .unwrap();
        ensure_ger_l1_observed(&store, &ger, true, EvidenceTag::Safe, tx_hash)
            .await
            .expect("the selected scan's atomic evidence write must authorize the GER");
    }

    /// All three settings share the same admission predicate because the
    /// configured tag controls what the sole indexer scans, not a second gate.
    #[tokio::test]
    async fn h6_selected_scan_evidence_authorizes_every_tag() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x2Au8; 32], &[0x2Bu8; 32]);
        let tx_hash = TxHash::from([0x47u8; 32]);
        store
            .set_ger_exit_roots(&ger, [0x2Au8; 32], [0x2Bu8; 32], 100, 1_700_000_000)
            .await
            .unwrap();

        for tag in [
            EvidenceTag::Latest,
            EvidenceTag::Safe,
            EvidenceTag::Finalized,
        ] {
            ensure_ger_l1_observed(&store, &ger, true, tag, tx_hash)
                .await
                .unwrap_or_else(|err| {
                    panic!("selected-scan evidence must pass for {tag:?}: {err}")
                });
        }
    }

    /// A legitimate GER is absent until the selected scan reaches it. Admission
    /// waits side-effect-free and succeeds as soon as that atomic evidence write
    /// lands; there is only the selected scan's cursor.
    #[tokio::test]
    async fn h6_waits_for_selected_scan_evidence() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = combined_ger(&[0x3Au8; 32], &[0x3Bu8; 32]);
        let tx_hash = TxHash::from([0x48u8; 32]);

        let indexing_store = store.clone();
        let index = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            indexing_store
                .set_ger_exit_roots(&ger, [0x3Au8; 32], [0x3Bu8; 32], 100, 1_700_000_000)
                .await
                .unwrap();
        });

        wait_for_ger_l1_observed_with_timing(
            &store,
            &ger,
            true,
            EvidenceTag::Finalized,
            tx_hash,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
        .expect("a GER must be admitted once the configured scan persists it");
        index.await.unwrap();
    }

    /// A forged/unknown GER remains rejected when the bounded wait expires.
    #[tokio::test]
    async fn h6_unknown_ger_is_rejected_after_bounded_wait() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let ger = [0x49u8; 32];
        let tx_hash = TxHash::from([0x49u8; 32]);

        let err = wait_for_ger_l1_observed_with_timing(
            &store,
            &ger,
            true,
            EvidenceTag::Safe,
            tx_hash,
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .await
        .expect_err("an unknown GER must remain rejected after the bounded wait");
        assert!(
            err.to_string()
                .contains("not observed by the configured L1 `safe` scan"),
            "selected-scan refusal: {err}"
        );
    }
}
