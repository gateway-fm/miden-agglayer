use crate::miden_client::MidenClient;
use alloy::primitives::TxHash;
use alloy_rpc_types_eth::TransactionRequest;
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use sha3::{Digest, Keccak256};
use std::sync::Arc;

alloy_core::sol! {
    #[derive(Debug)]
    interface IGlobalExitRootV2 {
        function lastMainnetExitRoot() external view returns (bytes32);
        function lastRollupExitRoot() external view returns (bytes32);
    }
}

/// Read the individual exit roots from the L1 GER contract.
pub async fn fetch_l1_exit_roots(
    l1_rpc_url: &str,
    ger_address: &str,
) -> anyhow::Result<([u8; 32], [u8; 32])> {
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::sol_types::SolCall;

    let provider = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let ger_addr: alloy::primitives::Address = ger_address.parse()?;

    let mainnet_call = IGlobalExitRootV2::lastMainnetExitRootCall {};
    let mainnet_result = provider
        .call(
            TransactionRequest::default()
                .to(ger_addr)
                .input(mainnet_call.abi_encode().into()),
        )
        .await?;
    let mainnet_root: [u8; 32] = mainnet_result[..32].try_into()?;

    let rollup_call = IGlobalExitRootV2::lastRollupExitRootCall {};
    let rollup_result = provider
        .call(
            TransactionRequest::default()
                .to(ger_addr)
                .input(rollup_call.abi_encode().into()),
        )
        .await?;
    let rollup_root: [u8; 32] = rollup_result[..32].try_into()?;

    Ok((mainnet_root, rollup_root))
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
async fn submit_update_ger_note(
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    ger_bytes: [u8; 32],
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
                    client.submit_new_transaction(ger_manager_id, tx_request),
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
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    // Vestigial after the cut-over: the GER synthetic log (and the exit-root
    // detail it carried) is now emitted by the SyntheticProjector from the
    // consumed UpdateGerNote, not here. Kept in the signature so the GER-inject
    // call chain stays unchanged; a follow-up can prune the exit-root threading.
    _mainnet_exit_root: Option<[u8; 32]>,
    _rollup_exit_root: Option<[u8; 32]>,
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    _block_state: &Arc<crate::block_state::BlockState>,
    _txn_hash: TxHash,
) -> anyhow::Result<u64> {
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

    let mut block_number = 0u64; // assigned by store.advance_block_number() after Miden commit

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
        match submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await {
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
                submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await?;
            }
            Err(err) => return Err(err),
        }

        // The SyntheticProjector is the sole synthetic-event producer: it emits
        // the GER log + advances the synthetic tip when it observes the
        // UpdateGerNote consumed. This path only submits the note to Miden. Report
        // the current tip so the caller's receipt records at a real block (mirrors
        // `record_local_immediate_success`) rather than block 0.
        block_number = store.get_latest_block_number().await?;
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    Ok(block_number)
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
