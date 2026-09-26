use super::*;
use crate::store::memory::InMemoryStore;
use std::collections::HashSet;

use miden_protocol::Word;
use miden_protocol::account::AccountType;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::note::Note;
use miden_protocol::transaction::RawOutputNote;
use miden_standards::note::{P2idNote, TxFeeNote};

fn output_fixture(step: FaucetStep, serial: u32) -> Note {
    let service = AccountId::builder().build_with_seed([1; 32]);
    let faucet = AccountId::builder()
        .account_type(AccountType::Public)
        .build_with_seed([2; 32]);
    let serial_number = Word::from([serial, 2, 3, 4]);
    match step {
        FaucetStep::Fund => P2idNote::builder()
            .sender(service)
            .target(faucet)
            .asset(FungibleAsset::new(faucet, 215_040).unwrap())
            .note_type(NoteType::Public)
            .serial_number(serial_number)
            .build()
            .unwrap()
            .into(),
        FaucetStep::Register => ConfigAggBridgeNote::create(
            ConversionMetadata {
                faucet_account_id: faucet,
                origin_token_address: EthAddress::new([7; 20]),
                scale: 10,
                origin_network: 2,
                is_native: false,
                metadata_hash: MetadataHash::new([8; 32]),
            },
            service,
            AccountId::builder()
                .account_type(AccountType::Public)
                .build_with_seed([3; 32]),
            &mut RandomCoin::new(serial_number),
        )
        .unwrap(),
        FaucetStep::Deploy => unreachable!(),
    }
}

fn fee_fixture() -> Note {
    TxFeeNote::builder()
        .sender(AccountId::builder().build_with_seed([1; 32]))
        .serial_number(Word::from([9u32, 2, 3, 4]))
        .asset(FungibleAsset::new(AccountId::builder().build_with_seed([4; 32]), 7).unwrap())
        .build()
        .unwrap()
        .into()
}

fn replayed_outputs(notes: Vec<Note>) -> RawOutputNotes {
    let outputs =
        RawOutputNotes::new(notes.into_iter().map(RawOutputNote::Full).collect()).unwrap();
    RawOutputNotes::read_from_bytes(&outputs.to_bytes()).unwrap()
}

fn recovers_output_with_fee(step: FaucetStep) {
    let intended = output_fixture(step, 1);
    for notes in [
        vec![fee_fixture(), intended.clone()],
        vec![intended.clone(), fee_fixture()],
    ] {
        assert_eq!(
            step_output_note_id(step, &replayed_outputs(notes)).unwrap(),
            intended.id(),
        );
    }
}

#[test]
fn faucet_fund_handoff_recovers_payment_alongside_fee() {
    recovers_output_with_fee(FaucetStep::Fund);
}

#[test]
fn faucet_register_handoff_recovers_configuration_alongside_fee() {
    recovers_output_with_fee(FaucetStep::Register);
}

#[test]
fn faucet_handoff_rejects_missing_ambiguous_and_wrong_step_outputs() {
    for step in [FaucetStep::Fund, FaucetStep::Register] {
        let intended = output_fixture(step, 1);
        assert_eq!(
            step_output_note_id(step, &replayed_outputs(vec![intended.clone()])).unwrap(),
            intended.id()
        );
        let other = if step == FaucetStep::Fund {
            FaucetStep::Register
        } else {
            FaucetStep::Fund
        };
        for notes in [
            vec![],
            vec![fee_fixture()],
            vec![intended.clone(), output_fixture(step, 2), fee_fixture()],
            vec![intended.clone(), output_fixture(other, 2), fee_fixture()],
            vec![output_fixture(other, 2)],
        ] {
            assert!(step_output_note_id(step, &replayed_outputs(notes)).is_err());
        }
    }
}

#[test]
fn faucet_handoff_rejects_noncanonical_fee_and_incomplete_outputs() {
    use miden_protocol::note::{
        NoteAssets, NoteRecipient, NoteStorage, NoteTag, PartialNoteMetadata,
    };
    let fee = fee_fixture();
    let sender = fee.metadata().sender();
    for step in [FaucetStep::Fund, FaucetStep::Register] {
        let intended = output_fixture(step, 1);
        let metadata = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(TxFeeNote::TAG);
        // A fee tag alone is insufficient. Also reject a real fee script with
        // the wrong tag, private visibility, storage, attachments or no assets.
        let malformed = [
            Note::new(fee.assets().clone(), metadata, intended.recipient().clone()),
            Note::new(
                fee.assets().clone(),
                metadata.with_tag(NoteTag::new(123)),
                fee.recipient().clone(),
            ),
            Note::new(
                fee.assets().clone(),
                PartialNoteMetadata::new(sender, NoteType::Private).with_tag(TxFeeNote::TAG),
                fee.recipient().clone(),
            ),
            Note::new(
                fee.assets().clone(),
                metadata,
                NoteRecipient::new(
                    Word::from([5u32, 2, 3, 4]),
                    TxFeeNote::script(),
                    NoteStorage::new(vec![Felt::ONE]).unwrap(),
                ),
            ),
            Note::with_attachments(
                fee.assets().clone(),
                metadata,
                fee.recipient().clone(),
                output_fixture(FaucetStep::Register, 3)
                    .attachments()
                    .clone(),
            ),
            Note::new(
                NoteAssets::new(vec![]).unwrap(),
                metadata,
                fee.recipient().clone(),
            ),
        ];
        for note in malformed {
            assert!(
                step_output_note_id(step, &replayed_outputs(vec![intended.clone(), note])).is_err()
            );
        }
        let outputs = RawOutputNotes::new(vec![
            RawOutputNote::Partial(intended.into()),
            RawOutputNote::Full(fee.clone()),
        ])
        .unwrap();
        assert!(step_output_note_id(step, &outputs).is_err());
    }
}

fn identity() -> FaucetDeployment {
    FaucetDeployment {
        key: alloy::signers::local::PrivateKeySigner::random()
            .address()
            .to_string(),
        binding: b"chain/bridge/service/origin/metadata/fee-policy".to_vec(),
        initial_account: b"initial account A with seed".to_vec(),
    }
}

fn prepared(generation: u32, expiration_block: u64, serial: u8) -> FaucetPreparedTx {
    FaucetPreparedTx {
        generation,
        expiration_block,
        executed: vec![serial; 8],
        proven: vec![serial; 16],
    }
}

/// Run against both store implementations: the losing writer must recover the
/// winner's identity AND transaction bytes, including across process restarts.
pub(crate) async fn store_contract(store: &dyn Store) {
    let first = identity();
    let mut other = first.clone();
    other.initial_account = b"competing account B with different seed".to_vec();
    let (a, b) = tokio::join!(
        store.reserve_faucet_deployment(first.clone()),
        store.reserve_faucet_deployment(other),
    );
    let saved = a.unwrap();
    assert_eq!(saved, b.unwrap());
    assert_eq!(
        Some(saved.clone()),
        store.get_faucet_deployment(&first.key).await.unwrap()
    );
    let mut wrong = first.clone();
    wrong.binding.push(42);
    assert!(store.reserve_faucet_deployment(wrong).await.is_err());
    assert_eq!(
        Some(saved),
        store.get_faucet_deployment(&first.key).await.unwrap()
    );

    for step in [FaucetStep::Fund, FaucetStep::Deploy, FaucetStep::Register] {
        // Concurrent execution must still converge BEFORE either writer sends.
        let (a, b) = tokio::join!(
            store.prepare_faucet_step(&first.key, step, prepared(0, 150, 1), 100),
            store.prepare_faucet_step(&first.key, step, prepared(0, 151, 2), 100),
        );
        let old = a.unwrap();
        assert_eq!(old, b.unwrap());
        assert_eq!(
            Some(old.clone()),
            store.get_faucet_step(&first.key, step).await.unwrap()
        );
        // Expiry equality is NOT proof the transaction cannot still land.
        assert!(
            store
                .prepare_faucet_step(&first.key, step, prepared(1, 200, 3), old.expiration_block)
                .await
                .is_err()
        );
        assert!(
            store
                .prepare_faucet_step(&first.key, step, prepared(2, 200, 3), 160)
                .await
                .is_err()
        );
        assert!(
            store
                .prepare_faucet_step(&first.key, step, prepared(1, 160, 3), 160)
                .await
                .is_err()
        );
        assert_eq!(
            Some(old),
            store.get_faucet_step(&first.key, step).await.unwrap()
        );
        let next = prepared(1, 250, 4);
        assert_eq!(
            next,
            store
                .prepare_faucet_step(&first.key, step, next.clone(), 160)
                .await
                .unwrap()
        );
        // A stale process cannot resurrect its superseded generation.
        assert!(
            store
                .prepare_faucet_step(&first.key, step, prepared(0, 300, 5), 260)
                .await
                .is_err()
        );
        assert_eq!(
            Some(next),
            store.get_faucet_step(&first.key, step).await.unwrap()
        );
    }
    assert!(
        store
            .prepare_faucet_step("missing-parent", FaucetStep::Fund, prepared(0, 150, 1), 100)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn faucet_identity_and_handoffs_converge_in_memory() {
    store_contract(&InMemoryStore::new()).await;
}

#[derive(Clone, Copy)]
enum Interruption {
    None,
    BeforeSend,
    AfterSend,
    AfterCommit,
}

#[derive(Default)]
struct Chain {
    height: u64,
    created: Vec<FaucetPreparedTx>,
    sent: Vec<FaucetPreparedTx>,
    landed: HashSet<Vec<u8>>,
}

// Every retry constructs a new runtime with no local client cache. Only the
// external chain and the proxy Store survive. Crashes can happen before send,
// after admission, or after commit but before SDK apply / registry completion.
struct FaultRuntime<'a> {
    chain: &'a mut Chain,
    interruption: Interruption,
    unreadable: bool,
}

impl StepRuntime for FaultRuntime<'_> {
    async fn sync_height(&mut self) -> anyhow::Result<u64> {
        Ok(self.chain.height)
    }
    async fn effect(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<bool> {
        ensure!(!self.unreadable, "node unavailable");
        Ok(self.chain.landed.contains(&saved.proven))
    }
    async fn prepare(&mut self, generation: u32) -> anyhow::Result<FaucetPreparedTx> {
        let next = prepared(
            generation,
            self.chain.height + 64,
            self.chain.created.len() as u8 + 1,
        );
        self.chain.created.push(next.clone());
        Ok(next)
    }
    async fn submit(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<()> {
        ensure!(
            !matches!(self.interruption, Interruption::BeforeSend),
            "crash before send"
        );
        self.chain.sent.push(saved.clone());
        ensure!(
            !matches!(self.interruption, Interruption::AfterSend),
            "submission outcome unknown"
        );
        self.chain.landed.insert(saved.proven.clone());
        ensure!(
            !matches!(self.interruption, Interruption::AfterCommit),
            "crash before local apply"
        );
        Ok(())
    }
}

pub(crate) async fn restart_contract(store: &dyn Store) {
    for interruption in [
        Interruption::BeforeSend,
        Interruption::AfterSend,
        Interruption::AfterCommit,
    ] {
        let identity = store.reserve_faucet_deployment(identity()).await.unwrap();
        let mut chain = Chain {
            height: 100,
            ..Default::default()
        };
        for step in [FaucetStep::Fund, FaucetStep::Deploy, FaucetStep::Register] {
            assert!(
                resume_step(
                    store,
                    &identity.key,
                    step,
                    &mut FaultRuntime {
                        chain: &mut chain,
                        interruption,
                        unreadable: false,
                    }
                )
                .await
                .is_err()
            );
            let saved = store
                .get_faucet_step(&identity.key, step)
                .await
                .unwrap()
                .unwrap();
            let creates = chain.created.len();
            resume_step(
                store,
                &identity.key,
                step,
                &mut FaultRuntime {
                    chain: &mut chain,
                    interruption: Interruption::None,
                    unreadable: false,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                chain.created.len(),
                creates,
                "restart executed a second transaction"
            );
            assert!(chain.landed.contains(&saved.proven));
            let sends = chain.sent.len();
            // Replay of the whole workflow after a later-stage crash must not
            // re-fund, re-deploy, or republish a committed registration note.
            resume_step(
                store,
                &identity.key,
                step,
                &mut FaultRuntime {
                    chain: &mut chain,
                    interruption: Interruption::None,
                    unreadable: false,
                },
            )
            .await
            .unwrap();
            assert_eq!(chain.sent.len(), sends);
        }
        assert_eq!(chain.created.len(), 3);
        assert_eq!(chain.landed.len(), 3);
        assert_eq!(
            Some(identity.clone()),
            store.get_faucet_deployment(&identity.key).await.unwrap()
        );
    }
}

#[tokio::test]
async fn faucet_restarts_at_each_send_and_commit_boundary() {
    restart_contract(&InMemoryStore::new()).await;
}

#[tokio::test]
async fn faucet_unknown_submission_replays_exact_bytes_until_expiry() {
    let store = InMemoryStore::new();
    let identity = store.reserve_faucet_deployment(identity()).await.unwrap();
    let mut chain = Chain {
        height: 100,
        ..Default::default()
    };
    for height in [100, 110, 164] {
        chain.height = height;
        assert!(
            resume_step(
                &store,
                &identity.key,
                FaucetStep::Fund,
                &mut FaultRuntime {
                    chain: &mut chain,
                    interruption: Interruption::AfterSend,
                    unreadable: false,
                }
            )
            .await
            .is_err()
        );
    }
    assert_eq!(chain.created.len(), 1);
    assert_eq!(chain.sent.len(), 3);
    assert!(chain.sent.iter().all(|tx| tx == &chain.sent[0]));
    chain.height = 165;
    // Expired + unavailable is still uncertain: never prepare another send.
    assert!(
        resume_step(
            &store,
            &identity.key,
            FaucetStep::Fund,
            &mut FaultRuntime {
                chain: &mut chain,
                interruption: Interruption::None,
                unreadable: true,
            }
        )
        .await
        .is_err()
    );
    assert_eq!(chain.created.len(), 1);
    assert_eq!(chain.sent.len(), 3);
    // Only fresh exact absence beyond expiry permits a new generation.
    resume_step(
        &store,
        &identity.key,
        FaucetStep::Fund,
        &mut FaultRuntime {
            chain: &mut chain,
            interruption: Interruption::None,
            unreadable: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(chain.created.len(), 2);
    assert_eq!(chain.sent.last().unwrap().generation, 1);
    assert_eq!(chain.landed.len(), 1);
}

#[tokio::test]
async fn faucet_expired_but_landed_is_not_replaced() {
    let store = InMemoryStore::new();
    let identity = store.reserve_faucet_deployment(identity()).await.unwrap();
    let mut chain = Chain {
        height: 100,
        ..Default::default()
    };
    assert!(
        resume_step(
            &store,
            &identity.key,
            FaucetStep::Fund,
            &mut FaultRuntime {
                chain: &mut chain,
                interruption: Interruption::AfterSend,
                unreadable: false,
            }
        )
        .await
        .is_err()
    );
    // It lands while the client is down, then expires before the client returns.
    chain.landed.insert(chain.sent[0].proven.clone());
    chain.height = 200;
    resume_step(
        &store,
        &identity.key,
        FaucetStep::Fund,
        &mut FaultRuntime {
            chain: &mut chain,
            interruption: Interruption::None,
            unreadable: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(chain.created.len(), 1);
    assert_eq!(chain.sent.len(), 1);
}

#[test]
fn faucet_corrupt_prepared_bytes_fail_closed() {
    let signer = AccountId::from_hex("0x18101fa522c174b165efd4f70a0385").unwrap();
    assert!(decode_handoff(&prepared(0, 164, 1), signer).is_err());
}

#[tokio::test]
async fn faucet_handoff_serializes_real_sdk_transaction_and_checks_identity() {
    use miden_client::testing::{Auth, MockChainBuilder, MockTransactionInput};
    let mut builder = MockChainBuilder::new();
    let account = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let chain = builder.build().unwrap();
    let executed = Box::pin(
        chain
            .build_transaction(MockTransactionInput::Account(account.clone()))
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    // Dummy proving keeps this serialization test cheap; proof validity is
    // checked by the node, and no mocked proof is used by production code.
    let proven = miden_tx::LocalTransactionProver::default()
        .prove_dummy(executed.clone())
        .unwrap();
    let result = TransactionResult::new(executed, vec![]).unwrap();
    let mut saved = FaucetPreparedTx {
        generation: 0,
        expiration_block: proven.expiration_block_num().as_u64(),
        executed: result.to_bytes(),
        proven: proven.to_bytes(),
    };
    let (replayed, proof) = decode_handoff(&saved, account.id()).unwrap();
    assert_eq!(replayed, result);
    assert_eq!(proof.id(), result.id());
    assert_eq!(proof.to_bytes(), saved.proven);
    let wrong_signer = AccountId::from_hex("0x18101fa522c174b165efd4f70a0385").unwrap();
    assert!(decode_handoff(&saved, wrong_signer).is_err());
    saved.expiration_block += 1;
    assert!(decode_handoff(&saved, account.id()).is_err());
}

#[test]
fn faucet_initial_identity_preserves_creation_seed() {
    let id = AccountId::from_hex("0x18101fa522c174b165efd4f70a0385").unwrap();
    let account = crate::network_accounts::faucet_account_builder(
        [7u8; 32],
        "MOP",
        8,
        Felt::new(u64::from(FungibleAsset::MAX_AMOUNT)).unwrap(),
        Felt::new(0).unwrap(),
        id,
        id,
        id,
    )
    .unwrap()
    .build()
    .unwrap();
    let recovered = Account::read_from_bytes(&account.to_bytes()).unwrap();
    assert!(recovered.is_new());
    assert!(recovered.seed().is_some());
    assert_eq!(account.id(), recovered.id());
    assert_eq!(account.seed(), recovered.seed());
    assert_eq!(account, recovered);
}

#[tokio::test]
async fn faucet_note_reconciliation_uses_node_without_local_apply() {
    use miden_client::testing::mock::MockRpcApi;
    use miden_client::testing::{Auth, MockChainBuilder};
    use miden_protocol::transaction::RawOutputNote;
    use std::sync::Arc;

    let mut builder = MockChainBuilder::new();
    let sender = builder
        .add_existing_wallet_with_assets(Auth::IncrNonce, [])
        .unwrap();
    let recipient = builder
        .add_existing_wallet_with_assets(Auth::IncrNonce, [])
        .unwrap();
    let fee_faucet = builder
        .add_existing_basic_faucet(Auth::IncrNonce, "FEE", 1000, Some(100))
        .unwrap()
        .id();
    let asset = Asset::Fungible(FungibleAsset::new(fee_faucet, 100).unwrap());
    let funding = builder
        .add_p2id_note(fee_faucet, sender.id(), &[asset], NoteType::Public)
        .unwrap();
    // Build the outgoing note separately so it does not already exist in genesis.
    let outgoing = MockChainBuilder::new()
        .add_p2id_note(
            sender.id(),
            recipient.id(),
            &[FungibleAsset::new(fee_faucet, 90).unwrap().into()],
            NoteType::Public,
        )
        .unwrap();
    let fee: Note = TxFeeNote::builder()
        .sender(sender.id())
        .serial_number(Word::from([9u32, 2, 3, 4]))
        .asset(FungibleAsset::new(fee_faucet, 10).unwrap())
        .build()
        .unwrap()
        .into();
    let script = miden_standards::tx_script::SendNotesTransactionScript::new(
        &sender.code_interface(),
        &[outgoing.clone().into(), fee.clone().into()],
    )
    .unwrap();
    let chain = builder.build().unwrap();
    let executed = Box::pin(
        chain
            .build_transaction(sender.id())
            .authenticated_input_note(funding.id())
            .send_notes_script(&script)
            .expected_output_note(RawOutputNote::Full(outgoing))
            .expected_output_note(RawOutputNote::Full(fee))
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    let result = TransactionResult::new(executed.clone(), vec![]).unwrap();
    let result = TransactionResult::read_from_bytes(&result.to_bytes()).unwrap();
    assert_eq!(result.created_notes().num_notes(), 2);
    let rpc = Arc::new(MockRpcApi::new(chain));
    let mut client = crate::test_helpers::offline_miden_client_lib().await;
    *client.test_rpc_api() = rpc.clone();
    // This exercises the pinned SDK's successful empty-response mapping.
    assert!(
        !step_effect(&mut client, FaucetStep::Fund, recipient.id(), &result)
            .await
            .unwrap()
    );
    let proven = miden_tx::LocalTransactionProver::default()
        .prove_dummy(executed)
        .unwrap();
    rpc.mock_chain
        .write()
        .add_pending_proven_transaction(proven);
    rpc.prove_block();
    // No apply_transaction or local outgoing-transaction record ever existed.
    assert!(
        step_effect(&mut client, FaucetStep::Fund, recipient.id(), &result)
            .await
            .unwrap()
    );
    assert!(
        step_effect(&mut client, FaucetStep::Register, recipient.id(), &result)
            .await
            .is_err()
    );
    *client.test_rpc_api() = crate::miden_client::build_rpc_client(
        &crate::miden_client::parse_node_url("http://127.0.0.1:1").unwrap(),
        25,
        None,
    );
    assert!(
        step_effect(&mut client, FaucetStep::Fund, recipient.id(), &result)
            .await
            .is_err()
    );
}
