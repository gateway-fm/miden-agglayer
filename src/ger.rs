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
