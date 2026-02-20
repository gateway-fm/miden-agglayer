use crate::accounts_config::AccountsConfig;
use crate::address_mapper::account_id_from_address_config;
use crate::amount::validate_amount;
use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::{BlockNumber, Bytes, FixedBytes, LogData, U256};
use alloy::sol_types::SolEvent;
use miden_base_agglayer::ClaimNoteParams;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::crypto::utils::bytes_to_packed_u32_elements;
use miden_protocol::note::{Note, NoteTag};
use miden_protocol::transaction::{OutputNote, TransactionId};
use std::cmp::min;
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

#[derive(Debug, Default)]
struct ClaimNoteInputs {
    smt_proof_local_exit_root: Vec<Felt>,
    smt_proof_rollup_exit_root: Vec<Felt>,
    global_index: [Felt; 8],
    mainnet_exit_root: [u8; 32],
    rollup_exit_root: [u8; 32],
    origin_network: Felt,
    origin_token_address: [u8; 20],
    destination_network: Felt,
    destination_address: [u8; 20],
    _amount_u256: [Felt; 8],
    metadata: [Felt; 8],
}

fn fixed_felts_from_u256(value: U256) -> [Felt; 8] {
    bytes_to_packed_u32_elements(value.as_le_slice()).try_into().unwrap()
}

fn fixed_felts_from_bytes(value: Bytes) -> [Felt; 8] {
    let metadata_limit =
        min(value.len(), ClaimNoteInputs::default().metadata.len() * size_of::<u32>());
    if metadata_limit < value.len() {
        tracing::debug!("fixed_felts_from_bytes notice: cutting off information");
    }
    let felts = bytes_to_packed_u32_elements(value.slice(0..metadata_limit).as_ref());
    std::array::from_fn(|i| felts.get(i).cloned().unwrap_or_default())
}

type Bytes32 = FixedBytes<32>;
fn felts_from_bytes32_array(values: [Bytes32; 32]) -> Vec<Felt> {
    values
        .into_iter()
        .flat_map(|value| bytes_to_packed_u32_elements(&value.0))
        .collect()
}

impl From<claimAssetCall> for ClaimNoteInputs {
    fn from(value: claimAssetCall) -> Self {
        Self {
            smt_proof_local_exit_root: felts_from_bytes32_array(value.smtProofLocalExitRoot),
            smt_proof_rollup_exit_root: felts_from_bytes32_array(value.smtProofRollupExitRoot),
            global_index: fixed_felts_from_u256(value.globalIndex),
            mainnet_exit_root: value.mainnetExitRoot.0,
            rollup_exit_root: value.rollupExitRoot.0,
            origin_network: Felt::from(value.originNetwork),
            origin_token_address: value.originTokenAddress.0.0,
            destination_network: Felt::from(value.destinationNetwork),
            destination_address: value.destinationAddress.0.0,
            _amount_u256: fixed_felts_from_u256(value.amount),
            metadata: fixed_felts_from_bytes(value.metadata),
        }
    }
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
    pub id: AccountId,
    pub decimals: u8,
    pub origin_token_decimals: u8,
}

// TODO: obtain a faucet from registry for a given origin_token_address
fn find_target_faucet(
    token_address: alloy::primitives::Address,
    accounts: &AccountsConfig,
) -> Faucet {
    if token_address.to_string() == "0x0000000000000000000000000000000000000000" {
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

fn create_claim(
    params: claimAssetCall,
    faucet: Faucet,
    accounts: AccountsConfig,
    rng_mut: &mut impl FeltRng,
) -> anyhow::Result<Note> {
    let claim_note_creator = accounts.service.0;

    let Some(destination_account_id) =
        account_id_from_address_config(params.destinationAddress, &accounts)
    else {
        anyhow::bail!("create_claim: invalid destination address {}", params.destinationAddress);
    };

    let amount = validate_amount(params.amount, faucet.origin_token_decimals, faucet.decimals)?;

    let inputs = ClaimNoteInputs::from(params);
    let p2id_serial_number = rng_mut.draw_word();
    let claim_params = ClaimNoteParams {
        smt_proof_local_exit_root: inputs.smt_proof_local_exit_root,
        smt_proof_rollup_exit_root: inputs.smt_proof_rollup_exit_root,
        global_index: inputs.global_index,
        mainnet_exit_root: &inputs.mainnet_exit_root,
        rollup_exit_root: &inputs.rollup_exit_root,
        origin_network: inputs.origin_network,
        origin_token_address: &inputs.origin_token_address,
        destination_network: inputs.destination_network,
        destination_address: &inputs.destination_address,
        amount: fixed_felts_from_u256(U256::from(amount)),
        metadata: inputs.metadata,
        claim_note_creator_account_id: claim_note_creator,
        agglayer_faucet_account_id: faucet.id,
        output_note_tag: NoteTag::with_account_target(destination_account_id),
        p2id_serial_number,
        destination_account_id,
        rng: rng_mut,
    };

    let claim_note = miden_base_agglayer::create_claim_note(claim_params)?;
    Ok(claim_note)
}

#[derive(Debug, Clone)]
pub struct PublishClaimTxn {
    pub txn_id: TransactionId,
    pub expires_at: BlockNumber,
    pub log: LogData,
}

async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut MidenClientLib,
    accounts: AccountsConfig,
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let faucet = find_target_faucet(params.originTokenAddress, &accounts);
    let claim_note = create_claim(params.clone(), faucet, accounts.clone(), client.rng())?;

    const EXPIRATION_DELTA: u16 = 10;
    let expires_at = latest_block_num + EXPIRATION_DELTA as u64;

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(claim_note); 1])
        .expiration_delta(EXPIRATION_DELTA)
        .build()?;

    let txn_id = client.submit_new_transaction(accounts.service.0, txn_request).await?;
    tracing::debug!("submitted claim note txn: {txn_id}");

    let event = ClaimEvent::from(params);
    let log = event.encode_log_data();

    Ok(PublishClaimTxn { txn_id, expires_at, log })
}

pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
    latest_block_num: BlockNumber,
) -> anyhow::Result<PublishClaimTxn> {
    let result = Arc::new(OnceLock::<PublishClaimTxn>::new());
    let result_internal = result.clone();

    let future = client.with(move |client| {
        Box::new(async move {
            let result_value =
                publish_claim_internal(params, client, accounts.0, latest_block_num).await?;
            result_internal.set(result_value).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(result.get().unwrap().clone())
}
