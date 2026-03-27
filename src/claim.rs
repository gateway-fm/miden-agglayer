use crate::accounts_config::AccountsConfig;
use crate::amount::validate_amount;
use crate::faucet_ops;
use crate::miden_client::{MidenClient, MidenClientLib};
use crate::store::{FaucetEntry, Store};
use alloy::primitives::{BlockNumber, Bytes, FixedBytes, LogData};
use alloy::sol_types::SolEvent;
use miden_base_agglayer::{
    ClaimNoteStorage, EthAddressFormat, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
    ProofData, SmtNode,
};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::note::Note;
use miden_protocol::transaction::{OutputNote, TransactionId};
use std::sync::{Arc, OnceLock};

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L556
    #[derive(Debug)]
    function claimAsset(
        bytes32[32] calldata smtProofLocalExitRoot,
        bytes32[32] calldata smtProofRollupExitRoot,
        uint256 globalIndex,
        bytes32 mainnetExitRoot,
        bytes32 rollupExitRoot,
        uint32 originNetwork,
        address originTokenAddress,
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        bytes calldata metadata
    );
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L139
    #[derive(Debug)]
    event ClaimEvent(
        uint256 globalIndex,
        uint32 originNetwork,
        address originAddress,
        address destinationAddress,
        uint256 amount
    );
}

impl From<claimAssetCall> for ClaimEvent {
    fn from(value: claimAssetCall) -> Self {
        Self {
            globalIndex: value.globalIndex,
            originNetwork: value.originNetwork,
            originAddress: value.originTokenAddress,
            destinationAddress: value.destinationAddress,
            amount: value.amount,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Faucet {
    id: AccountId,
    decimals: u8,
    origin_token_decimals: u8,
}

/// Look up a faucet for the given origin token, auto-creating one if not found.
///
/// On the first bridge of a new ERC-20 token, the faucet is created on Miden,
/// registered in the bridge, and saved to the Store — all automatically.
async fn find_or_create_faucet(
    token_address: alloy::primitives::Address,
    origin_network: u32,
    metadata: &Bytes,
    store: &dyn Store,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
) -> anyhow::Result<Faucet> {
    // 1. Try store lookup first
    if let Some(entry) = store
        .get_faucet_by_origin(&token_address.0.0, origin_network)
        .await?
    {
        return Ok(Faucet {
            id: entry.faucet_id,
            decimals: entry.miden_decimals,
            origin_token_decimals: entry.origin_decimals,
        });
    }

    // 2. Auto-create: parse token metadata from claimAsset call
    let (symbol, origin_decimals) = faucet_ops::parse_token_metadata(metadata, &token_address)?;
    let miden_decimals: u8 = 8;
    let scale = origin_decimals.checked_sub(miden_decimals).ok_or_else(|| {
        anyhow::anyhow!(
            "origin decimals {origin_decimals} < miden decimals {miden_decimals} for token {token_address}"
        )
    })?;

    tracing::info!(
        token_address = %token_address,
        symbol = %symbol,
        origin_decimals,
        scale,
        "auto-creating faucet for new ERC-20 token"
    );

    // 3. Create faucet on Miden, deploy, register in bridge
    let faucet_account = faucet_ops::create_and_register_faucet(
        client,
        &symbol,
        miden_decimals,
        &token_address.0.0,
        origin_network,
        scale,
        accounts.service.0,
        accounts.bridge.0,
    )
    .await?;

    // 4. Save to store
    let entry = FaucetEntry {
        faucet_id: faucet_account.id(),
        origin_address: token_address.0.0,
        origin_network,
        symbol,
        origin_decimals,
        miden_decimals,
        scale,
    };
    store.register_faucet(entry).await?;

    Ok(Faucet {
        id: faucet_account.id(),
        decimals: miden_decimals,
        origin_token_decimals: origin_decimals,
    })
}

fn bytes32_array_to_smt_nodes(values: [FixedBytes<32>; 32]) -> [SmtNode; 32] {
    values.map(|v| SmtNode::new(v.0))
}

async fn create_claim(
    params: claimAssetCall,
    faucet: Faucet,
    accounts: &AccountsConfig,
    store: &dyn Store,
    rng: &mut impl FeltRng,
) -> anyhow::Result<Note> {
    let sender = accounts.service.0;

    let _dest_account =
        crate::address_mapper::resolve_address(store, params.destinationAddress, accounts).await?;

    let amount = validate_amount(params.amount, faucet.origin_token_decimals, faucet.decimals)?;

    let proof_data = ProofData {
        smt_proof_local_exit_root: bytes32_array_to_smt_nodes(params.smtProofLocalExitRoot),
        smt_proof_rollup_exit_root: bytes32_array_to_smt_nodes(params.smtProofRollupExitRoot),
        global_index: GlobalIndex::new(params.globalIndex.to_be_bytes::<32>()),
        mainnet_exit_root: ExitRoot::new(params.mainnetExitRoot.0),
        rollup_exit_root: ExitRoot::new(params.rollupExitRoot.0),
    };

    let leaf_data = LeafData {
        origin_network: params.originNetwork,
        origin_token_address: EthAddressFormat::new(params.originTokenAddress.0.0),
        destination_network: params.destinationNetwork,
        destination_address: EthAddressFormat::new(params.destinationAddress.0.0),
        amount: EthAmount::new(params.amount.to_be_bytes::<32>()),
        metadata_hash: MetadataHash::new(metadata_to_hash(&params.metadata)),
    };

    let storage = ClaimNoteStorage {
        proof_data,
        leaf_data,
        miden_claim_amount: Felt::from(amount),
    };

    let note = miden_base_agglayer::create_claim_note(storage, faucet.id, sender, rng)?;
    Ok(note)
}

/// Compute metadata hash: keccak256 of metadata bytes.
///
/// The Solidity bridge contract always uses `keccak256(metadata)` in the leaf
/// computation, even for empty metadata. For ETH deposits metadata is empty,
/// so this returns `keccak256("")` = `0xc5d246...`, NOT all zeros.
fn metadata_to_hash(metadata: &Bytes) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut hasher = Keccak256::new();
    hasher.update(metadata.as_ref());
    hasher.finalize().into()
}

#[derive(Debug, Clone)]
pub struct PublishClaimTxn {
    pub txn_id: TransactionId,
    pub expires_at: BlockNumber,
    pub log: LogData,
    /// CLAIM note ID for consumption tracking (deferred receipts).
    pub claim_note_id: Option<String>,
}

async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
    store: &dyn Store,
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let faucet = find_or_create_faucet(
        params.originTokenAddress,
        params.originNetwork,
        &params.metadata,
        store,
        client,
        accounts,
    )
    .await?;

    tracing::info!(
        global_index = %params.globalIndex,
        origin_network = %params.originNetwork,
        dest_address = %params.destinationAddress,
        amount = %params.amount,
        faucet_id = %crate::accounts_config::AccountIdBech32(faucet.id),
        mainnet_exit_root = %alloy::hex::encode(params.mainnetExitRoot.0),
        rollup_exit_root = %alloy::hex::encode(params.rollupExitRoot.0),
        "creating CLAIM note"
    );

    let claim_note = create_claim(params.clone(), faucet, accounts, store, client.rng()).await?;
    let claim_note_id = claim_note.id().to_string();

    const EXPIRATION_DELTA: u16 = 10;
    let expires_at = latest_block_num + EXPIRATION_DELTA as u64;

    // Wait for the NTX builder to consume the UpdateGerNote on the bridge account.
    // The CLAIM note's FPI calls assert_valid_ger which checks the bridge account's
    // GER storage. If we submit the CLAIM before the GER is stored, it will fail.
    // Typically the GER note is consumed within ~5s (2-3 blocks). We wait 5 cycles
    // of 3s (15s total) which gives the NTX builder plenty of time while keeping
    // the overall claim latency reasonable.
    tracing::info!("waiting for GER to propagate to bridge account before submitting CLAIM...");
    for i in 0..5 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        client.sync_state().await?;
        tracing::debug!(cycle = i, "GER propagation sync cycle");
    }
    tracing::info!("GER propagation wait complete, submitting CLAIM note");

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(claim_note); 1])
        .build()?;

    // Execute and check the output notes before submission
    let tx_result = client
        .execute_transaction(accounts.service.0, txn_request)
        .await?;
    let exec_tx = tx_result.executed_transaction();
    for (i, note) in exec_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::OutputNote::Full(n) => {
                let att = n.metadata().attachment();
                let att_default = att == &miden_protocol::note::NoteAttachment::default();
                format!("Full(attachment_empty={})", att_default)
            }
            miden_protocol::transaction::OutputNote::Partial(_) => "Partial".to_string(),
            miden_protocol::transaction::OutputNote::Header(_) => "Header".to_string(),
        };
        tracing::info!(note_idx = i, variant = %variant, "executed tx output note");
    }

    let proven_tx = client.prove_transaction(&tx_result).await?;
    for (i, note) in proven_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::OutputNote::Full(n) => {
                let att = n.metadata().attachment();
                let att_default = att == &miden_protocol::note::NoteAttachment::default();
                format!("Full(attachment_empty={})", att_default)
            }
            miden_protocol::transaction::OutputNote::Partial(_) => "Partial".to_string(),
            miden_protocol::transaction::OutputNote::Header(_) => "Header".to_string(),
        };
        tracing::info!(note_idx = i, variant = %variant, "proven tx output note");
    }

    let txn_id = tx_result.executed_transaction().id();
    let _submission_height = client
        .submit_proven_transaction(proven_tx, &tx_result)
        .await?;
    client
        .apply_transaction(&tx_result, _submission_height)
        .await?;
    tracing::info!("submitted claim note txn: {txn_id}, claim_note_id: {claim_note_id}");

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("claim tx {txn_id} committed to block");
    } else {
        anyhow::bail!("claim tx {txn_id} was submitted but not committed within 20s");
    }

    let event = ClaimEvent::from(params);
    let log = event.encode_log_data();

    Ok(PublishClaimTxn {
        txn_id,
        expires_at,
        log,
        claim_note_id: Some(claim_note_id),
    })
}

pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: Arc<dyn Store>,
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let result = Arc::new(OnceLock::<PublishClaimTxn>::new());
    let result_inner = result.clone();

    client
        .with(move |client| {
            Box::new(async move {
                let value =
                    publish_claim_internal(params, client, &accounts.0, &*store, latest_block_num)
                        .await?;
                let _ = result_inner.set(value);
                Ok(())
            })
        })
        .await?;

    result
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("publish_claim: closure completed but result was not set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::seed_test_faucets;
    use alloy::primitives::address;

    #[test]
    fn test_metadata_to_hash_empty() {
        let metadata = Bytes::from(vec![]);
        let hash = metadata_to_hash(&metadata);
        // keccak256("")
        let expected =
            hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
                .unwrap();
        assert_eq!(hash, expected.as_slice());
    }

    #[tokio::test]
    async fn test_find_faucet_eth_from_store() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        let entry = store
            .get_faucet_by_origin(&[0u8; 20], 0)
            .await
            .unwrap()
            .expect("ETH faucet should be registered");
        assert_eq!(entry.origin_decimals, 18);
        assert_eq!(entry.miden_decimals, 8);
        assert_eq!(entry.symbol, "ETH");
    }

    #[tokio::test]
    async fn test_find_faucet_unknown_returns_none() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        // Address not registered in the test seed
        let entry = store.get_faucet_by_origin(&[0xBB; 20], 0).await.unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn test_metadata_to_hash_non_empty() {
        let metadata = Bytes::from(vec![0x01, 0x02, 0x03]);
        let hash = metadata_to_hash(&metadata);
        // keccak256([0x01, 0x02, 0x03])
        let expected =
            hex::decode("f1885eda54b7a053318cd41e2093220dab15d65381b1157a3633a83bfd5c9239")
                .unwrap();
        assert_eq!(hash, expected.as_slice());
    }

    #[test]
    fn test_claim_event_from_claim_asset_call() {
        use alloy::primitives::{Address, U256};

        let call = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(42u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 1,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: address!("1234567890abcdef1234567890abcdef12345678"),
            amount: U256::from(1000u64),
            metadata: Default::default(),
        };
        let event = ClaimEvent::from(call);
        assert_eq!(event.globalIndex, U256::from(42u64));
        assert_eq!(event.originNetwork, 1);
        assert_eq!(event.amount, U256::from(1000u64));
    }

    #[test]
    fn test_bytes32_array_to_smt_nodes_converts() {
        let mut values = [FixedBytes::ZERO; 32];
        values[0] = FixedBytes::from([0xAA; 32]);
        values[31] = FixedBytes::from([0xBB; 32]);
        let nodes = bytes32_array_to_smt_nodes(values);
        // Verify we get back 32 nodes (basic structural check)
        assert_eq!(nodes.len(), 32);
    }
}
