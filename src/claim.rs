use crate::accounts_config::AccountsConfig;
use crate::amount::validate_amount;
use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::{Bytes, FixedBytes, U256};
use miden_base_agglayer::ClaimNoteParams;
use miden_client::Word;
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::transaction::{ForeignAccount, TransactionRequestBuilder};
use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::crypto::utils::bytes_to_packed_u32_elements;
use miden_protocol::note::{Note, NoteInputs, NoteRecipient, NoteTag};
use miden_protocol::transaction::{OutputNote, TransactionId};
use miden_standards::note::WellKnownNote;
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
) -> anyhow::Result<(Note, AccountId, Word)> {
    let claim_note_creator = accounts.service.0;

    let destination_account_id =
        if params.destinationAddress.to_string() == "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" {
            accounts.wallet_hardhat.0
        } else {
            accounts.wallet_satoshi.0
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
    Ok((claim_note, destination_account_id, p2id_serial_number))
}

async fn consume_claim_by_faucet(
    claim_note: Note,
    faucet: AccountId,
    bridge: AccountId,
    destination_account_id: AccountId,
    p2id_serial_number: Word,
    client: &mut MidenClientLib,
) -> anyhow::Result<TransactionId> {
    let p2id_inputs =
        vec![destination_account_id.suffix(), destination_account_id.prefix().as_felt()];
    let p2id_recipient = NoteRecipient::new(
        p2id_serial_number,
        WellKnownNote::P2ID.script(),
        NoteInputs::new(p2id_inputs)?,
    );

    let foreign_bridge = ForeignAccount::public(bridge, AccountStorageRequirements::default())?;
    let txn_request = TransactionRequestBuilder::new()
        .input_notes([(claim_note, None); 1])
        .expected_output_recipients(vec![p2id_recipient])
        .foreign_accounts([foreign_bridge; 1])
        .build()?;

    let txn_id = client.submit_new_transaction(faucet, txn_request).await?;
    Ok(txn_id)
}

async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut MidenClientLib,
    accounts: AccountsConfig,
) -> anyhow::Result<TransactionId> {
    let faucet = find_target_faucet(params.originTokenAddress, &accounts);
    let (claim_note, destination_account_id, p2id_serial_number) =
        create_claim(params, faucet, accounts.clone(), client.rng())?;
    let claim_note_for_faucet = claim_note.clone();

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes([OutputNote::Full(claim_note); 1])
        .build()?;

    let txn_id = client.submit_new_transaction(accounts.service.0, txn_request).await?;
    tracing::debug!("submitted claim note txn: {txn_id}");

    loop {
        let summary = client.sync_state().await?;
        if summary.committed_transactions.contains(&txn_id) {
            break;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
    tracing::debug!("committed claim note txn: {txn_id}");

    tracing::debug!("consume_claim_by_faucet...");
    let txn_id = consume_claim_by_faucet(
        claim_note_for_faucet,
        faucet.id,
        accounts.bridge.0,
        destination_account_id,
        p2id_serial_number,
        client,
    )
    .await?;
    Ok(txn_id)
}

pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
) -> anyhow::Result<TransactionId> {
    let result = Arc::new(OnceLock::<TransactionId>::new());
    let result_internal = result.clone();

    let future = client.with(|client| {
        Box::new(async move {
            let txn_id = publish_claim_internal(params, client, accounts.0).await?;
            result_internal.set(txn_id).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(*result.get().unwrap())
}
