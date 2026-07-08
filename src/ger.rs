use crate::miden_client::MidenClient;
use alloy::primitives::TxHash;
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
/// Submit the `UpdateGerNote` to Miden and return the on-chain note's
/// `details_commitment` (hex), encoded identically to how the projector keys
/// consumed notes (`InputNoteRecord::details_commitment()`) — so `insert_ger`
/// can tie the real `insertGlobalExitRoot` eth-tx to this note via
/// `record_tx_note_link`. Returns `None` only when the submit closure did not
/// execute (a stubbed MidenClient in unit tests).
async fn submit_update_ger_note(
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    ger_bytes: [u8; 32],
) -> anyhow::Result<Option<String>> {
    let inner_accounts = accounts.0.clone();
    // `MidenClient::with` closures resolve to `Result<()>`; surface the note
    // commitment through a captured slot (same pattern as `publish_claim`).
    let commitment_slot = Arc::new(std::sync::OnceLock::<String>::new());
    let commitment_inner = commitment_slot.clone();
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
                let _ = commitment_inner.set(note_commitment);
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
                Ok(())
            })
        })
        .await?;
    Ok(commitment_slot.get().cloned())
}

/// Submit a GER injection to Miden. Returns `true` if a new `UpdateGerNote` was
/// submitted (and the real eth-tx ↔ note link recorded so the projector finalises
/// the receipt + emits the GER log on consumption), `false` if the GER was already
/// injected (a duplicate — the caller completes its receipt immediately).
///
/// Audit H6 — `require_l1_observed` cross-checks the injected GER against the
/// L1 InfoTree the indexer independently observed. The aggoracle-supplied GER
/// bytes are otherwise trusted verbatim: a compromised signer could inject a
/// FORGED GER (one whose `(mainnet, rollup)` decomposition the indexer never saw
/// on L1) onto Miden. The indexer writes the authoritative decomposition via
/// `set_ger_exit_roots`, so a GER whose `mainnet_exit_root` is `None` was NOT
/// observed on L1. When `require_l1_observed` is set, such a GER is refused
/// before it reaches Miden; otherwise it is allowed through (to tolerate indexer
/// lag) but flagged via the `ger_injection_unverified_total` metric + warn.
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    txn_hash: TxHash,
    require_l1_observed: bool,
) -> anyhow::Result<bool> {
    // Audit H6 — verify the GER was observed on L1 by the independent
    // L1InfoTreeIndexer (it writes the (mainnet, rollup) decomposition via
    // set_ger_exit_roots). A GER with no recorded decomposition was supplied
    // only by the aggoracle and never corroborated by an L1 observation — a
    // forged-GER injection signal.
    let l1_observed = store
        .get_ger_entry(&ger_bytes)
        .await?
        .is_some_and(|e| e.mainnet_exit_root.is_some());
    if !l1_observed {
        ::metrics::counter!("ger_injection_unverified_total").increment(1);
        tracing::warn!(
            ger = %hex::encode(ger_bytes),
            tx = %txn_hash,
            "GER injection not yet corroborated by the L1 InfoTree indexer \
             (mainnet_exit_root unset); allowing through but unverified"
        );
        if require_l1_observed {
            anyhow::bail!(
                "GER {} was not observed on L1 by the indexer (mainnet_exit_root unset); \
                 refusing injection under --reject-unverified-ger-injection (audit H6)",
                hex::encode(ger_bytes)
            );
        }
    }

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
        let note_commitment = match submit_update_ger_note(
            miden_client,
            accounts.clone(),
            ger_bytes,
        )
        .await
        {
            Ok(commitment) => commitment,
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
                submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await?
            }
            Err(err) => return Err(err),
        };

        // Tie the real `insertGlobalExitRoot` eth-tx to the on-chain UpdateGerNote so
        // the SyntheticProjector finalises THIS receipt (and emits the GER log) under
        // the real tx hash when it observes the note consumed — making the receipt
        // block == the GER-log block. No synthetic log / tip advance / receipt
        // completion happens in this path. (`note_commitment` is `None` only under a
        // stubbed test client; the projector then falls back to the derived hash.)
        if let Some(note_commitment) = note_commitment {
            store
                .record_tx_note_link(&format!("{txn_hash:#x}"), &note_commitment)
                .await?;
        }
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
    use crate::store::memory::InMemoryStore;
    use std::str::FromStr;
    use std::sync::Arc;
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
        let err = insert_ger(
            forged_ger,
            &miden_client,
            accounts.clone(),
            &store,
            tx_hash,
            true, // require_l1_observed
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
        )
        .await;
        if let Err(err) = lenient {
            assert!(
                !err.to_string().contains("not observed on L1"),
                "lenient mode must NOT refuse an unverified GER at the H6 gate: {err}"
            );
        }
    }
}
