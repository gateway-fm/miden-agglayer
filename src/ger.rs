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
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
