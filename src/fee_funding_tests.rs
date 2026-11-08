use std::sync::Arc;

use miden_client::note::NoteFile;
use miden_client::rpc::{RpcEndpoint, RpcError};
use miden_client::store::input_note_states::ExpectedNoteState;
use miden_client::testing::account_id::{
    ACCOUNT_ID_FEE_FAUCET, ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_WITH_CALLBACKS,
};
use miden_client::testing::mock::MockRpcApi;
use miden_client::testing::{Auth, MockChainBuilder};
use miden_protocol::account::Account;
use miden_protocol::note::{NoteAssets, NoteRecipient, NoteStorage, NoteTag, PartialNoteMetadata};
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::{Felt, Word};
use miden_standards::code_builder::CodeBuilder;

use super::*;

fn snapshot() -> FeeSnapshot {
    FeeSnapshot {
        verification_base_fee: 7,
        fee_faucet_id: ACCOUNT_ID_FEE_FAUCET.try_into().unwrap(),
    }
}

fn fee_asset(amount: u64) -> Asset {
    FungibleAsset::new(snapshot().fee_faucet_id, amount)
        .unwrap()
        .into()
}

fn p2id(target: AccountId, serial: u32) -> Note {
    P2idNote::builder()
        .sender(target)
        .target(target)
        .serial_number(Word::from([Felt::from(serial); 4]))
        .note_type(NoteType::Public)
        .asset(fee_asset(10_000))
        .build()
        .unwrap()
        .into()
}

fn custom_note(target: AccountId, serial: u32) -> Note {
    unavailable_fee_note(target, target, fee_asset(1), serial)
}

fn mixed_asset_p2id(target: AccountId, serial: u32) -> Note {
    let note = p2id(target, serial);
    let extra = FungibleAsset::new(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_WITH_CALLBACKS
            .try_into()
            .unwrap(),
        1,
    )
    .unwrap();
    Note::new(
        NoteAssets::new(vec![fee_asset(10_000), extra.into()]).unwrap(),
        *note.metadata().partial_metadata(),
        note.recipient().clone(),
    )
}

fn unavailable_fee_note(sender: AccountId, target: AccountId, asset: Asset, serial: u32) -> Note {
    let source = "@note_script pub proc main() dropw push.0 assert.err=\"note unavailable\" end";
    let script = CodeBuilder::default().compile_note_script(source).unwrap();
    Note::new(
        NoteAssets::new(vec![asset]).unwrap(),
        PartialNoteMetadata::new(sender, NoteType::Public)
            .with_tag(NoteTag::with_account_target(target)),
        NoteRecipient::new(
            Word::from([Felt::from(serial); 4]),
            script,
            NoteStorage::new(vec![]).unwrap(),
        ),
    )
}

fn wallet_action_note(
    sender: AccountId,
    target: AccountId,
    fee_faucet: AccountId,
    serial: u32,
) -> Note {
    let source = format!(
        r#"
use miden::protocol::active_note
use miden::standards::assets::fungible_asset
use miden::standards::notes::p2id
use miden::standards::wallets::basic as basic_wallet

@note_script
@locals(9)
pub proc main()
    dropw
    exec.basic_wallet::move_note_assets_to_account
    push.8 push.0x400 exec.active_note::get_bounded_storage
    push.8 eq assert.err="invalid storage"
    mem_load.0x400
    push.{prefix}
    push.{suffix}
    exec.fungible_asset::create
    loc_storew_be.0 dropw
    loc_storew_be.4 dropw
    mem_loadw_be.0x404
    push.1
    mem_load.0x403
    mem_load.0x402
    mem_load.0x401
    exec.p2id::create_output_note
    loc_store.8
    loc_loadw_be.4
    padw
    loc_loadw_be.0
    loc_load.8
    movdn.8
    call.basic_wallet::move_asset_to_note
    dropw drop
end
"#,
        prefix = fee_faucet.prefix().as_felt().as_canonical_u64(),
        suffix = fee_faucet.suffix().as_canonical_u64(),
    );
    let script = CodeBuilder::default()
        .compile_note_script(source.as_str())
        .unwrap();
    let serial = Word::from([Felt::from(serial); 4]);
    let storage = NoteStorage::new(vec![
        Felt::from(500u32),
        sender.suffix(),
        sender.prefix().as_felt(),
        Felt::from(NoteTag::with_account_target(sender).as_u32()),
        serial[0],
        serial[1],
        serial[2],
        serial[3],
    ])
    .unwrap();
    Note::new(
        NoteAssets::new(vec![FungibleAsset::new(fee_faucet, 1).unwrap().into()]).unwrap(),
        PartialNoteMetadata::new(sender, NoteType::Public)
            .with_tag(NoteTag::with_account_target(target)),
        NoteRecipient::new(serial, script, storage),
    )
}

fn wallet() -> Account {
    Account::builder([91; 32])
        .with_components(Auth::IncrNonce)
        .with_component(miden_standards::account::wallets::BasicWallet)
        .build_existing()
        .unwrap()
}

#[tokio::test]
async fn custom_wallet_action_is_executable_but_not_selected_for_funding() {
    use miden_client::testing::MockTransactionInput;

    let mut builder = MockChainBuilder::new().verification_base_fee(7);
    let sender = builder
        .add_existing_wallet_with_assets(Auth::IncrNonce, [])
        .unwrap();
    let target = builder
        .add_existing_wallet_with_assets(Auth::IncrNonce, [fee_asset(10_000)])
        .unwrap();
    let note = wallet_action_note(sender.id(), target.id(), snapshot().fee_faucet_id, 55);
    builder.add_output_note(RawOutputNote::Full(note.clone()));
    let chain = builder.build().unwrap();
    let executed = chain
        .build_transaction(MockTransactionInput::AccountId(target.id()))
        .add_note_script(note.details().script().clone())
        .unauthenticated_input_note(note.clone())
        .build()
        .unwrap()
        .execute()
        .await
        .unwrap();
    let paid: u64 = executed
        .output_notes()
        .iter()
        .flat_map(|n| n.assets().iter())
        .filter_map(|a| a.as_fungible())
        .filter(|f| f.faucet_id() == snapshot().fee_faucet_id)
        .map(|f| f.amount().as_u64())
        .sum();
    assert_eq!(
        paid, 500,
        "the custom script must perform the wallet action in the control"
    );
    let good = p2id(target.id(), 56);
    assert_eq!(
        select_fee_notes([note.into(), good.clone().into()], target.id(), &snapshot()),
        vec![good],
    );
}

#[test]
fn selects_only_fee_p2ids_for_the_requested_wallet() {
    let target = wallet().id();
    let good = p2id(target, 1);
    let other = p2id(snapshot().fee_faucet_id, 2);
    // A matching tag is only a discovery hint, never recipient authorization.
    let wrong_target = Note::new(
        other.assets().clone(),
        PartialNoteMetadata::new(target, NoteType::Public)
            .with_tag(NoteTag::with_account_target(target)),
        other.recipient().clone(),
    );
    let wrong_storage = Note::new(
        good.assets().clone(),
        *good.metadata().partial_metadata(),
        NoteRecipient::new(
            Word::from([Felt::from(3u32); 4]),
            P2idNote::script(),
            NoteStorage::new(vec![]).unwrap(),
        ),
    );
    let without_fee = Note::new(
        NoteAssets::new(vec![]).unwrap(),
        *good.metadata().partial_metadata(),
        good.recipient().clone(),
    );
    let records = [
        custom_note(target, 4),
        mixed_asset_p2id(target, 5),
        wrong_target,
        wrong_storage,
        without_fee,
        good.clone(),
    ]
    .map(InputNoteRecord::from);
    assert_eq!(select_fee_notes(records, target, &snapshot()), vec![good]);
}

#[test]
fn incomplete_record_does_not_hide_a_valid_top_up() {
    let target = wallet().id();
    let good = p2id(target, 1);
    let incomplete = InputNoteRecord::new(
        good.details().clone(),
        good.attachments().clone(),
        None,
        ExpectedNoteState {
            metadata: None,
            after_block_num: BlockNumber::GENESIS,
            tag: None,
        }
        .into(),
    );
    assert_eq!(
        select_fee_notes([incomplete, good.clone().into()], target, &snapshot()),
        vec![good],
    );
}

#[test]
fn skipped_notes_do_not_use_the_per_tick_budget() {
    let target = wallet().id();
    let records = (0..MAX_FEE_NOTES_PER_TICK as u32 + 1)
        .map(|i| custom_note(target, i).into())
        .chain((100..200).map(|i| p2id(target, i).into()));
    let selected = select_fee_notes(records, target, &snapshot());
    assert_eq!(selected.len(), MAX_FEE_NOTES_PER_TICK);
    assert_eq!(selected.first(), Some(&p2id(target, 100)));
    assert_eq!(selected.last(), Some(&p2id(target, 131)));
}

async fn client_with_notes(account: &Account, notes: &[Note]) -> (MidenClientLib, Arc<MockRpcApi>) {
    let mut builder = MockChainBuilder::with_accounts([account.clone()])
        .unwrap()
        .verification_base_fee(7);
    for note in notes {
        builder.add_output_note(RawOutputNote::Full(note.clone()));
    }
    let chain = builder.build().unwrap();
    let config = chain.protocol_config().clone();
    let genesis = chain.genesis_block_header().commitment();
    let rpc = Arc::new(MockRpcApi::new(chain));
    let mut client = crate::test_helpers::offline_miden_client_lib().await;
    *client.test_rpc_api() = rpc.clone();
    client.seed_protocol_config(config).await.unwrap();
    client
        .seed_transaction_encryption_key(
            miden_client::rpc::encryption::TransactionEncryptionKey::new_unattested(
                vec![1],
                miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey::new().public_key(),
                genesis,
            ),
        )
        .await
        .unwrap();
    client.add_account(account, false).await.unwrap();
    client.sync_state().await.unwrap();
    client
        .import_notes(
            &notes
                .iter()
                .map(|n| NoteFile::NoteId(n.id()))
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    (client, rpc)
}

#[tokio::test]
async fn sweep_consumes_p2id_and_leaves_custom_note_committed() {
    let mut account = wallet();
    account.vault_mut().add_asset(fee_asset(1_000)).unwrap();
    let custom = custom_note(account.id(), 1);
    let good = p2id(account.id(), 2);
    let mixed = mixed_asset_p2id(account.id(), 4);
    let action = wallet_action_note(
        snapshot().fee_faucet_id,
        account.id(),
        snapshot().fee_faucet_id,
        3,
    );
    let (mut client, rpc) = client_with_notes(
        &account,
        &[custom.clone(), action.clone(), mixed.clone(), good.clone()],
    )
    .await;
    assert_eq!(
        consume_fee_notes(&mut client, account.id(), &snapshot())
            .await
            .unwrap(),
        1
    );
    rpc.advance_blocks(1);
    client.sync_state().await.unwrap();
    assert!(
        client
            .get_input_note(custom.id())
            .await
            .unwrap()
            .unwrap()
            .is_committed()
    );
    assert!(
        client
            .get_input_note(good.id())
            .await
            .unwrap()
            .unwrap()
            .is_consumed()
    );
    let balance = crate::fee_vault_monitor::fee_balance(
        &client.get_account(account.id()).await.unwrap().unwrap(),
        snapshot().fee_faucet_id,
    );
    assert!((11_000 - max_fee_per_txn(7)..=11_000).contains(&balance));
    assert!(
        client
            .get_input_note(action.id())
            .await
            .unwrap()
            .unwrap()
            .is_committed()
    );
    assert!(
        client
            .get_input_note(mixed.id())
            .await
            .unwrap()
            .unwrap()
            .is_committed()
    );
    assert!(
        client
            .get_output_notes(NoteFilter::All)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        consume_fee_notes(&mut client, account.id(), &snapshot())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn failed_submission_does_not_block_the_next_top_up() {
    let account = wallet();
    let notes = [p2id(account.id(), 1), p2id(account.id(), 2)];
    let (mut client, rpc) = client_with_notes(&account, &notes).await;
    rpc.fail_next_call(
        RpcEndpoint::SubmitProvenTx,
        RpcError::InvalidResponse("test submission failure".into()),
    );
    assert_eq!(
        consume_fee_notes(&mut client, account.id(), &snapshot())
            .await
            .unwrap(),
        1
    );
    rpc.advance_blocks(1);
    client.sync_state().await.unwrap();
    assert_eq!(
        client
            .get_input_notes(NoteFilter::Committed)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        consume_fee_notes(&mut client, account.id(), &snapshot())
            .await
            .unwrap(),
        1
    );
    rpc.advance_blocks(1);
    client.sync_state().await.unwrap();
    assert!(
        client
            .get_input_notes(NoteFilter::Committed)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deployment_funding_uses_validated_p2ids() {
    let account = wallet();
    let good = p2id(account.id(), 2);
    let (mut client, _) = client_with_notes(
        &account,
        &[
            custom_note(account.id(), 1),
            mixed_asset_p2id(account.id(), 3),
            good.clone(),
        ],
    )
    .await;
    assert_eq!(
        wait_for_funding_notes(
            &mut client,
            account.id(),
            "service",
            &snapshot(),
            Duration::ZERO
        )
        .await
        .unwrap(),
        vec![good],
    );
}

/// Runs against the local Compose chain after `make e2e-l2l2-up`.
/// The real proxy monitor consumes the P2IDs; this client only sends and observes.
#[tokio::test]
#[ignore = "requires the local E2E stack and its genesis funding account"]
async fn live_wallet_top_ups() -> anyhow::Result<()> {
    use crate::accounts_config::FundingManifest;
    use crate::proxy_keystore::ProxyKeystore;
    use miden_client::account::AccountFile;
    use miden_client::builder::ClientBuilder;
    use miden_client::crypto::FeltRng;
    use miden_client::keystore::{FilesystemKeyStore, Keystore};
    use miden_client::rpc::Endpoint;
    use miden_client_sqlite_store::ClientBuilderSqliteExt;

    let manifest: FundingManifest = toml::from_str(&std::fs::read_to_string(
        ".miden-agglayer-data/funding.toml",
    )?)?;
    let file = AccountFile::read("fixtures/genesis/faucet_operator.mac")?;
    let operator_id = file.account().id();
    let fee_faucet_id = manifest.fee_faucet()?;
    let store = tempfile::tempdir()?;
    let keystore = Arc::new(ProxyKeystore::local(FilesystemKeyStore::new(
        store.path().join("keystore"),
    )?));
    for key in file.auth_secret_keys() {
        keystore.add_key(key, operator_id).await?;
    }
    let rpc = crate::miden_client::build_rpc_client(&Endpoint::localhost(), 30_000, None);
    let mut client = ClientBuilder::new()
        .rpc(rpc)
        .sqlite_store(store.path().join("store.sqlite3"))
        .authenticator(keystore)
        .build()
        .await?;
    client.import_account_by_id(operator_id).await?;
    client.sync_state().await?;
    let mut notes = Vec::new();
    let mut expected = Vec::new();
    for target in [manifest.service_id()?, manifest.ger_manager_id()?] {
        let good: Note = P2idNote::builder()
            .sender(operator_id)
            .target(target)
            .asset(FungibleAsset::new(fee_faucet_id, 20_000)?)
            .note_type(NoteType::Public)
            .generate_serial_number(client.rng())
            .build()?
            .into();
        let custom = wallet_action_note(
            operator_id,
            target,
            fee_faucet_id,
            client.rng().draw_word()[0].as_canonical_u64() as u32,
        );
        let unavailable = unavailable_fee_note(
            operator_id,
            target,
            FungibleAsset::new(fee_faucet_id, 1)?.into(),
            client.rng().draw_word()[0].as_canonical_u64() as u32,
        );
        println!(
            "wallet {}: P2ID {}, custom {}, unavailable {}",
            target.to_hex(),
            good.id(),
            custom.id(),
            unavailable.id()
        );
        expected.push((good.id(), custom.id(), unavailable.id()));
        notes.extend([good, custom, unavailable]);
    }
    let files: Vec<_> = notes.iter().map(|n| NoteFile::NoteId(n.id())).collect();
    let request = TransactionRequestBuilder::new()
        .own_output_notes(notes)
        .build()?;
    let tx = crate::miden_client::submit_new_transaction(&mut client, operator_id, request).await?;
    anyhow::ensure!(
        crate::miden_client::wait_for_transaction_commit(
            &mut client,
            tx,
            60,
            Duration::from_secs(2)
        )
        .await?,
        "top-up send did not commit"
    );
    client.import_notes(&files).await?;
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        client.sync_state().await?;
        let mut completed = true;
        for (good, custom, unavailable) in &expected {
            completed &= client
                .get_input_note(*good)
                .await?
                .context("P2ID missing")?
                .is_consumed();
            for id in [custom, unavailable] {
                anyhow::ensure!(
                    client
                        .get_input_note(*id)
                        .await?
                        .context("custom note missing")?
                        .is_committed(),
                    "custom fee note {id} did not remain committed",
                );
            }
        }
        if completed {
            println!(
                "both wallets consumed their P2ID top-ups and left all four custom notes committed"
            );
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet top-ups did not complete within 240s"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
