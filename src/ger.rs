use crate::accounts_config::AccountsConfig;
use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::{FixedBytes, LogData, TxHash};
use alloy::sol_types::SolEvent;
use alloy_rpc_types_eth::TransactionRequest;
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::{OutputNote, TransactionRequestBuilder};
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

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L52
    #[derive(Debug)]
    event UpdateHashChainValue(
        bytes32 indexed newGlobalExitRoot,
        bytes32 indexed newHashChainValue
    );
}

impl UpdateHashChainValue {
    fn new(ger: FixedBytes<32>, chain_hash: FixedBytes<32>) -> Self {
        UpdateHashChainValue {
            newGlobalExitRoot: ger,
            newHashChainValue: chain_hash,
        }
    }
}

/// Result of a GER insertion.
pub struct GerInsertResult {
    pub log_data: LogData,
    pub block_number: u64,
    pub is_new: bool,
}

async fn submit_ger_to_miden(
    client: &mut MidenClientLib,
    ger_bytes: [u8; 32],
    accounts: &AccountsConfig,
) -> anyhow::Result<()> {
    // Use the dedicated ger_manager account for GER injection. This avoids
    // stale state errors: the service account is continuously modified by
    // the NTX builder (claim processing), making its state commitment
    // unpredictable. The ger_manager account is only used for GER injection,
    // so its state is stable between submissions.
    let ger_manager_id = accounts
        .ger_manager
        .as_ref()
        .map(|a| a.0)
        .unwrap_or(accounts.service.0);
    let bridge_id = accounts.bridge.0;

    // Retry up to 3 times. Re-import accounts (Network/public) before each
    // attempt so the local client has fresh state from the node — equivalent
    // to Igor's fresh-client-per-operation approach in aggkit-proxy.
    // See docs/ger-note-screening-bypass.md for full analysis.
    for attempt in 0..3u32 {
        client.sync_state().await?;
        // Refresh bridge account state (asset tree changes after CLAIM).
        match client.import_account_by_id(bridge_id).await {
            Ok(()) => tracing::info!(attempt, "bridge account re-imported from node"),
            Err(e) => tracing::warn!(attempt, "bridge re-import failed: {e:#}"),
        }

        let ger = ExitRoot::new(ger_bytes);
        let note = UpdateGerNote::create(ger, ger_manager_id, bridge_id, client.rng())?;
        if attempt == 0 {
            tracing::info!(note_id = %note.id(), "UpdateGerNote created");
        }

        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(note)])
            .build()?;

        // Try submit_new_transaction first — it bundles execute+prove+submit+apply
        // and correctly updates the ger_manager's local state (nonce, commitment).
        // If it fails due to the NoteScreener (stale bridge asset tree after CLAIM),
        // fall back to the split pattern which bypasses the NoteScreener.
        let tx_id = match client
            .submit_new_transaction(ger_manager_id, tx_request.clone())
            .await
        {
            Ok(id) => id,
            Err(e) => {
                let err_str = format!("{e:#?}");
                let is_note_screener = err_str.contains("NoteScreener")
                    || err_str.contains("NoteChecker")
                    || err_str.contains("FetchAssetWitness")
                    || err_str.contains("note relevance");

                if is_note_screener {
                    tracing::warn!(
                        attempt,
                        "GER submit_new_transaction hit NoteScreener, falling back to split pattern"
                    );
                    // Split: execute → prove → submit (skip apply since it's the screener that fails)
                    let tx_result = client
                        .execute_transaction(ger_manager_id, tx_request)
                        .await
                        .map_err(|e2| anyhow::anyhow!("split execute: {e2:#}"))?;
                    let proven = client
                        .prove_transaction(&tx_result)
                        .await
                        .map_err(|e2| anyhow::anyhow!("split prove: {e2:#}"))?;
                    let id = tx_result.executed_transaction().id();
                    let height = client
                        .submit_proven_transaction(proven, &tx_result)
                        .await
                        .map_err(|e2| anyhow::anyhow!("split submit: {e2:#}"))?;
                    // Try apply but tolerate failure
                    if let Err(e2) = client.apply_transaction(&tx_result, height).await {
                        tracing::warn!(attempt, "split apply also failed (continuing): {e2:#}");
                    }
                    id
                } else if attempt < 2 {
                    tracing::warn!(attempt, "GER TX failed, retrying: {e:#?}");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                } else {
                    return Err(anyhow::anyhow!("{e:#}"));
                }
            }
        };

        tracing::info!(
            tx_id = %tx_id,
            ger = %hex::encode(ger_bytes),
            "UpdateGerNote submitted to Miden node, waiting for commit..."
        );

        // Poll for transaction commitment. When the split fallback was used
        // and apply_transaction failed, the TX isn't in the local store.
        // Check the sync summary committed list + block advancement as fallbacks.
        let start_block = client.sync_state().await?.block_num.as_u32();
        let timeout_secs: u64 = std::env::var("GER_COMMIT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
            .clamp(5, 120);
        let mut committed = false;
        for _ in 0..timeout_secs {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let summary = client.sync_state().await?;
            // Check if the TX appears in the sync's committed list
            if summary.committed_transactions.iter().any(|id| *id == tx_id) {
                committed = true;
                break;
            }
            // Also check local store (works when apply_transaction succeeded)
            let txns = client
                .get_transactions(miden_client::store::TransactionFilter::All)
                .await?;
            if txns.iter().any(|t| {
                t.id == tx_id
                    && matches!(
                        t.status,
                        miden_client::transaction::TransactionStatus::Committed { .. }
                    )
            }) {
                committed = true;
                break;
            }
            // If 3+ blocks have passed since we started polling, the TX
            // was likely committed (the node accepted the proven TX).
            if summary.block_num.as_u32() >= start_block.saturating_add(3) {
                tracing::info!(
                    tx_id = %tx_id,
                    block = summary.block_num.as_u32(),
                    "assuming GER TX committed (block advanced past submission)"
                );
                committed = true;
                break;
            }
        }

        if !committed {
            anyhow::bail!("UpdateGerNote transaction {tx_id} not committed after {timeout_secs}s");
        }

        tracing::info!(tx_id = %tx_id, "UpdateGerNote transaction committed");
        return Ok(());
    }

    anyhow::bail!("GER injection failed after 3 attempts")
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    mainnet_exit_root: Option<[u8; 32]>,
    rollup_exit_root: Option<[u8; 32]>,
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    block_state: &Arc<crate::block_state::BlockState>,
    txn_hash: TxHash,
) -> anyhow::Result<GerInsertResult> {
    // Check dedup before doing any work
    let is_new = !store.has_seen_ger(&ger_bytes).await?;

    let mut block_number = 0u64; // assigned by store.advance_block_number() after Miden commit

    if is_new {
        tracing::info!(
            ger = %hex::encode(ger_bytes),
            "GER injection: submitting to Miden..."
        );

        // Submit to Miden first — only emit the log event on success
        let inner_accounts = accounts.0.clone();
        miden_client
            .with(move |client| {
                Box::new(
                    async move { submit_ger_to_miden(client, ger_bytes, &inner_accounts).await },
                )
            })
            .await?;

        // Use the store's sequential block counter so GER events and claim
        // events never collide on the same block number.
        block_number = store.advance_block_number().await?;
        let block_hash = block_state.get_block_hash(block_number);
        let timestamp = block_state.get_block_timestamp(block_number);

        // Miden submission succeeded — now record the event
        let tx_hash_str = format!("{txn_hash:#x}");
        store
            .add_ger_update_event(
                block_number,
                block_hash,
                &tx_hash_str,
                &ger_bytes,
                mainnet_exit_root,
                rollup_exit_root,
                timestamp,
            )
            .await?;
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    let event = UpdateHashChainValue::new(FixedBytes::from(ger_bytes), FixedBytes::default());
    let log_data = event.encode_log_data();

    Ok(GerInsertResult {
        log_data,
        block_number,
        is_new,
    })
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
