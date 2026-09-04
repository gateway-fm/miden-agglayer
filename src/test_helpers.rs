//! Shared test utilities for the miden-agglayer crate.
//!
//! Provides `create_test_service()` and `test_accounts_config()` so that test
//! modules across the crate can set up a `ServiceState` without duplication
//! or `unsafe { std::mem::zeroed() }`.

use crate::accounts_config::{AccountIdBech32, AccountsConfig as InnerAccountsConfig};
use crate::block_state::BlockState;
use crate::store::memory::InMemoryStore;
use crate::store::{FaucetEntry, Store};
use crate::{AccountsConfig, MidenClient, ServiceState};
use miden_protocol::account::AccountId;
use std::sync::Arc;

/// A valid hex-encoded AccountId used across all test fixtures.
/// Protocol 0.15 rejects the old v0 AccountId encoding (`UnknownAccountIdVersion`);
/// this is a valid 0.15 (version-1) public regular-account id.
const TEST_ACCOUNT_HEX: &str = "0xac0000000000dd110000ee000000fc";

fn dummy_account_id() -> AccountIdBech32 {
    AccountIdBech32(AccountId::from_hex(TEST_ACCOUNT_HEX).expect("valid test account ID"))
}

/// Build an `AccountsConfig` with valid (but dummy) account IDs.
pub fn test_accounts_config() -> AccountsConfig {
    AccountsConfig(InnerAccountsConfig {
        service: dummy_account_id(),
        bridge: dummy_account_id(),
        faucet_eth: Some(dummy_account_id()),
        faucet_agg: None,
        ger_manager: None,
    })
}

/// Seed the faucet registry with the default ETH faucet for testing.
pub async fn seed_test_faucets(store: &dyn Store) {
    let eth_id = AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap();
    store
        .register_faucet(FaucetEntry {
            faucet_id: eth_id,
            origin_address: [0u8; 20],
            origin_network: 0,
            symbol: "ETH".into(),
            origin_decimals: 18,
            miden_decimals: 8,
            scale: 10,
            metadata: vec![],
        })
        .await
        .unwrap();
}

/// Create a `ServiceState` backed by `InMemoryStore` and a test `MidenClient`
/// stub (no real Miden node connection). Suitable for unit tests.
///
/// Audit C2 — `allow_any_signer = true` so tests that exercise the submission
/// paths don't each have to configure an allow-list. Production defaults remain
/// fail-closed (`allow_any_signer = false`, `allowed_signers = None`).
pub fn create_test_service() -> ServiceState {
    create_test_service_with_store(Arc::new(InMemoryStore::new()))
}

/// Like [`create_test_service`], but over a caller-provided store. Lets tests
/// keep a concrete handle (e.g. `Arc<InMemoryStore>` for its `#[cfg(test)]`
/// helpers such as `test_backdate_claim`) while the service holds the same
/// instance behind `Arc<dyn Store>`.
pub fn create_test_service_with_store(store: Arc<dyn Store>) -> ServiceState {
    let block_state = Arc::new(BlockState::new());
    let miden_client = MidenClient::new_test();
    let accounts = test_accounts_config();
    let mut state = ServiceState::new(miden_client, accounts, 1, 1, store, block_state);
    state.allow_any_signer = true;
    state
}

/// Build a REAL `MidenClientLib` backed by a throwaway sqlite store and an RPC
/// handle pointing at the (unused) localhost endpoint. `ClientBuilder::build`
/// performs no network I/O — it only initialises the sqlite store and reads the
/// (absent) genesis header — so tests can exercise code paths that require a
/// `&mut MidenClientLib` argument but return before issuing any RPC:
///
/// - the Cantina MA#23 `on_post_sync` dispatch gate in
///   `MidenClient::on_sync` (the listener decides whether to use the client).
///
/// Only available under `cfg(test)` — production code must never construct a
/// second client next to the process-wide `MidenClient` singleton.
#[cfg(test)]
pub async fn offline_miden_client_lib() -> crate::miden_client::MidenClientLib {
    use miden_client::builder::ClientBuilder;
    use miden_client::keystore::FilesystemKeyStore;
    use miden_client::rpc::Endpoint;
    use miden_client_sqlite_store::ClientBuilderSqliteExt;

    let store_dir = tempfile::tempdir().expect("tempdir").keep();
    let keystore_path = store_dir.join("keystore");
    std::fs::create_dir_all(&keystore_path).expect("keystore dir");
    let keystore = Arc::new(crate::proxy_keystore::ProxyKeystore::local(
        FilesystemKeyStore::new(keystore_path).expect("keystore"),
    ));

    ClientBuilder::new()
        .rpc(crate::miden_client::build_rpc_client(
            &Endpoint::localhost(),
            1_000,
            None,
        ))
        .sqlite_store(store_dir.join("store.sqlite3"))
        .authenticator(keystore)
        .build()
        .await
        .expect("offline MidenClientLib must build without a node")
}

/// Build an rpc `TransactionRecord` fixture via the proto conversion path —
/// the ONLY public constructor in miden-client 0.16.0-rc.5
/// (`consumed_note_refs` is `pub(crate)` upstream). Mirrors production wire
/// shape exactly: input notes are HEADERLESS (the rc.5 decoder still reads
/// `consumed_note_refs` from the flat proto list, not from the header's
/// input notes)
/// and public input-note identities arrive as explicit
/// `(nullifier, note_id)` refs.
///
/// Only compiled for the crate's own test harness: the proto module is public
/// only under miden-client's `testing` feature, which our dev-dependencies
/// enable.
#[cfg(test)]
pub fn test_tx_record(
    block: u32,
    account: AccountId,
    initial: miden_protocol::Word,
    final_state: miden_protocol::Word,
    nullifiers: Vec<miden_protocol::note::Nullifier>,
    consumed_note_refs: Vec<(
        miden_protocol::note::Nullifier,
        miden_protocol::note::NoteId,
    )>,
) -> miden_client::rpc::domain::transaction::TransactionRecord {
    use miden_client::rpc::generated as proto;
    let header = proto::transaction::TransactionHeader {
        transaction_id: None,
        account_id: Some(account.into()),
        initial_state_commitment: Some(initial.into()),
        final_state_commitment: Some(final_state.into()),
        input_notes: nullifiers
            .into_iter()
            .map(|n| proto::transaction::InputNoteCommitment {
                nullifier: Some(n.as_word().into()),
                header: None,
            })
            .collect(),
        output_notes: vec![],
    };
    let record = proto::rpc::TransactionRecord {
        block_num: block,
        header: Some(header),
        output_note_proofs: vec![],
        consumed_note_refs: consumed_note_refs
            .into_iter()
            .map(|(n, id)| proto::rpc::ConsumedNoteRef {
                nullifier: Some(n.as_word().into()),
                note_id: Some(id.into()),
            })
            .collect(),
    };
    record
        .try_into()
        .expect("proto test transaction record converts to the domain type")
}
