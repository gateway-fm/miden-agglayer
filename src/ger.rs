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
    let ger = ExitRoot::new(ger_bytes);
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;

    // Sync state before building the transaction to ensure we have the
    // latest account commitments (the NTX builder may have modified the
    // bridge account since our last sync).
    client.sync_state().await?;

    let note = UpdateGerNote::create(ger, service_id, bridge_id, client.rng())?;
    tracing::info!(note_id = %note.id(), "UpdateGerNote created");

    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(note)])
        .build()?;

    let tx_id = client
        .submit_new_transaction(service_id, tx_request)
        .await?;
    tracing::info!(
        tx_id = %tx_id,
        ger = %hex::encode(ger_bytes),
        "UpdateGerNote submitted to Miden node, waiting for commit..."
    );

    // Poll for transaction commitment
    let timeout_secs: u64 = std::env::var("GER_COMMIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(5, 120);
    let mut committed = false;
    for _ in 0..timeout_secs {
        // We can check if txn is in the store and has a block number
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
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        client.sync_state().await?; // Sync to get latest updates
    }

    if !committed {
        anyhow::bail!("UpdateGerNote transaction {tx_id} not committed after {timeout_secs}s");
    }

    tracing::info!(tx_id = %tx_id, "UpdateGerNote transaction committed");
    Ok(())
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

    let mut block_number = block_state.current_block_number() + 1;

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

        // Re-assign block number after the Miden TX has committed so
        // current_block reflects the latest sync and +1 is genuinely ahead.
        block_number = block_state.current_block_number() + 1;
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

        // Advance latest_block_number so bridge-service queries the new block
        let current_latest = store.get_latest_block_number().await.unwrap_or(0);
        if block_number > current_latest {
            store.set_latest_block_number(block_number).await?;
        }
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
