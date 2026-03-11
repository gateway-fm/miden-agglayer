use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::log_synthesis::LogStore;
use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::{FixedBytes, LogData, TxHash};
use alloy::sol_types::SolEvent;
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::{OutputNote, TransactionRequestBuilder};
use std::sync::Arc;

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L166
    #[derive(Debug)]
    function insertGlobalExitRoot(bytes32 root);
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
        "UpdateGerNote submitted to Miden node"
    );

    Ok(())
}

pub async fn insert_ger(
    params: insertGlobalExitRootCall,
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    log_store: &Arc<LogStore>,
    block_state: &Arc<BlockState>,
    txn_hash: TxHash,
) -> anyhow::Result<GerInsertResult> {
    let ger_bytes: [u8; 32] = params.root.0;
    let block_number = block_state.current_block_number();
    let block_hash = block_state.get_block_hash(block_number);

    // Check dedup before doing any work
    let is_new = !log_store.has_seen_ger(&ger_bytes);

    if is_new {
        tracing::info!(
            ger = %hex::encode(ger_bytes),
            block_number,
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

        // Miden submission succeeded — now record the event
        let tx_hash_str = format!("{txn_hash:#x}");
        log_store.add_ger_update_event(block_number, block_hash, &tx_hash_str, &ger_bytes);
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    let event = UpdateHashChainValue::new(params.root, FixedBytes::default());
    let log_data = event.encode_log_data();

    Ok(GerInsertResult {
        log_data,
        block_number,
        is_new,
    })
}
