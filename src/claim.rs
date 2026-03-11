use crate::accounts_config::AccountsConfig;
use crate::address_mapper::account_id_from_address_config;
use crate::amount::validate_amount;
use crate::miden_client::{MidenClient, MidenClientLib};
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

// TODO: obtain a faucet from registry for a given origin_token_address
fn find_target_faucet(
    token_address: alloy::primitives::Address,
    accounts: &AccountsConfig,
) -> Faucet {
    if token_address.is_zero() {
        Faucet {
            id: accounts.faucet_eth.0,
            decimals: 8,
            origin_token_decimals: 18,
        }
    } else {
        Faucet {
            id: accounts.faucet_agg.0,
            decimals: 8,
            origin_token_decimals: 8,
        }
    }
}

fn bytes32_array_to_smt_nodes(values: [FixedBytes<32>; 32]) -> [SmtNode; 32] {
    values.map(|v| SmtNode::new(v.0))
}

fn create_claim(
    params: claimAssetCall,
    faucet: Faucet,
    accounts: &AccountsConfig,
    rng: &mut impl FeltRng,
) -> anyhow::Result<Note> {
    let sender = accounts.service.0;

    if account_id_from_address_config(params.destinationAddress, accounts).is_none() {
        anyhow::bail!(
            "create_claim: invalid destination address {}",
            params.destinationAddress
        );
    }

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

/// Compute metadata hash: keccak256 of metadata bytes, or zero for empty metadata.
fn metadata_to_hash(metadata: &Bytes) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    if metadata.is_empty() {
        return [0u8; 32];
    }
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
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let faucet = find_target_faucet(params.originTokenAddress, accounts);
    let claim_note = create_claim(params.clone(), faucet, accounts, client.rng())?;
    let claim_note_id = claim_note.id().to_string();

    const EXPIRATION_DELTA: u16 = 10;
    let expires_at = latest_block_num + EXPIRATION_DELTA as u64;

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(claim_note); 1])
        .expiration_delta(EXPIRATION_DELTA)
        .build()?;

    let txn_id = client
        .submit_new_transaction(accounts.service.0, txn_request)
        .await?;
    tracing::debug!("submitted claim note txn: {txn_id}, claim_note_id: {claim_note_id}");

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
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let result = Arc::new(OnceLock::<PublishClaimTxn>::new());
    let result_inner = result.clone();

    client
        .with(move |client| {
            Box::new(async move {
                let value =
                    publish_claim_internal(params, client, &accounts.0, latest_block_num).await?;
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
