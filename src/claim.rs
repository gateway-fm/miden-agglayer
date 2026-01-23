use crate::miden_client::MidenClient;
use alloy::primitives::{Bytes, FixedBytes, U256};
use miden_base_agglayer::{
    ClaimNoteParams, create_agglayer_faucet_builder, create_bridge_account_builder,
};
use miden_client::Word;
use miden_client::account::component::BasicWallet;
use miden_client::auth::NoAuth;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::Felt;
use miden_protocol::account::{Account, AccountComponent, AccountId};
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::crypto::utils::bytes_to_packed_u32_elements;
use miden_protocol::note::{Note, NoteTag};
use miden_protocol::transaction::{OutputNote, TransactionId};
use rand::Rng;
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
    amount_u256: [Felt; 8],
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
            amount_u256: fixed_felts_from_u256(value.amount),
            metadata: fixed_felts_from_bytes(value.metadata),
        }
    }
}

// TODO: remove
fn create_existing_agglayer_faucet(
    seed: Word,
    token_symbol: &str,
    decimals: u8,
    max_supply: Felt,
    bridge_account_id: AccountId,
) -> Account {
    create_agglayer_faucet_builder(seed, token_symbol, decimals, max_supply, bridge_account_id)
        .with_auth_component(AccountComponent::from(NoAuth))
        .build()
        .expect("Agglayer faucet account should be valid")
}

// TODO: remove
fn create_existing_bridge_account(seed: Word) -> Account {
    create_bridge_account_builder(seed)
        .with_auth_component(AccountComponent::from(NoAuth))
        .build()
        .expect("Bridge account should be valid")
}

fn create_claim(
    params: claimAssetCall,
    bridge_account_id: AccountId,
    rng_mut: &mut impl FeltRng,
) -> anyhow::Result<Note> {
    // TODO: obtain a faucet from registry for a given origin_token_address
    let agglayer_faucet = create_existing_agglayer_faucet(
        rng_mut.draw_word(),
        "AGG",
        8u8,
        Felt::new(1000000),
        bridge_account_id,
    );

    // TODO: setup a single global account for the service
    let claim_note_creator = Account::builder(rng_mut.random())
        .with_component(BasicWallet)
        .with_auth_component(AccountComponent::from(NoAuth))
        .build()?;

    // TODO: remove when output_note_tag and destination_account_id are removed from ClaimNoteParams
    let destination_account = Account::builder(rng_mut.random())
        .with_component(BasicWallet)
        .with_auth_component(AccountComponent::from(NoAuth))
        .build()?;

    let inputs = ClaimNoteInputs::from(params);
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
        amount: inputs.amount_u256,
        metadata: inputs.metadata,
        claim_note_creator_account_id: claim_note_creator.id(),
        agglayer_faucet_account_id: agglayer_faucet.id(),
        output_note_tag: NoteTag::with_account_target(destination_account.id()),
        p2id_serial_number: rng_mut.draw_word(),
        destination_account_id: destination_account.id(),
        rng: rng_mut,
    };

    let claim_note = miden_base_agglayer::create_claim_note(claim_params)?;
    Ok(claim_note)
}

async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut miden_client::Client<FilesystemKeyStore>,
) -> anyhow::Result<TransactionId> {
    // TODO: use a predefined bridge account
    let bridge_account = create_existing_bridge_account(client.rng().draw_word());
    tracing::debug!("publish_claim: bridge account id = {}", bridge_account.id());

    let claim_note = create_claim(params, bridge_account.id(), client.rng())?;

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(claim_note); 1])
        .build()?;

    let txn_id = client.submit_new_transaction(bridge_account.id(), txn_request).await?;
    Ok(txn_id)
}

pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
) -> anyhow::Result<TransactionId> {
    let result = Arc::new(OnceLock::<TransactionId>::new());
    let result_internal = result.clone();

    let future = client.with(|client| {
        Box::new(async move {
            let txn_id = publish_claim_internal(params, client).await?;
            result_internal.set(txn_id).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(*result.get().unwrap())
}
