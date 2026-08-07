use crate::accounts_config;
use crate::accounts_config::{AccountIdBech32, AccountsConfig};
use crate::faucet_ops;
use crate::miden_client::MidenClient;
use crate::miden_client::MidenClientLib;
use crate::proxy_keystore::ProxyKeystore;
use crate::remote_signer::SignerRole;
use anyhow::Context;
use miden_base_agglayer::{MetadataHash, create_bridge_account};
use miden_client::crypto::FeltRng;
use miden_client::keystore::Keystore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::auth::{AuthSecretKey, PublicKeyCommitment};
use miden_protocol::account::{Account, AccountId, AccountType};
use miden_protocol::address::NetworkId;
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::wallets::BasicWallet;
use miden_tx::auth::TransactionAuthenticator;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[derive(Debug)]
struct Accounts {
    service: Account,
    bridge: Account,
    faucet_eth: Account,
    ger_manager: Account,
}

impl From<Accounts> for AccountsConfig {
    fn from(accounts: Accounts) -> Self {
        Self {
            service: AccountIdBech32(accounts.service.id()),
            bridge: AccountIdBech32(accounts.bridge.id()),
            faucet_eth: Some(AccountIdBech32(accounts.faucet_eth.id())),
            // AGG genesis faucet removed during 0.14.x migration: it registered under origin
            // [0u8; 20] which collides with ETH in the new on-chain token_registry_map. Any
            // additional token (POL, USDC, …) is auto-created by find_or_create_faucet in
            // claim.rs on first bridge.
            faucet_agg: None,
            ger_manager: Some(AccountIdBech32(accounts.ger_manager.id())),
        }
    }
}

// pub so external operator tooling (bridge-out app's --create-native-faucet) can
// create a NON-service, operator-owned faucet with the same auth scheme as the proxy.
//
// feat/016-ecdsa-accounts: NEWLY PROVISIONED operator signer accounts (service,
// GER manager, standalone/foreign wallets, operator-native faucets) authenticate
// with EcdsaK256Keccak (secp256k1 + keccak) instead of Falcon512Poseidon2.
// This changes the DEFAULT for new accounts only. It does not rekey existing
// Falcon accounts and does not remove Falcon support: the keystore and the
// signing path still handle Falcon keys, so a deployment provisioned before this
// change keeps working. Bridge and AggLayer-owned wrapped-faucet authorization
// are outside this helper entirely.
// Note the upstream caveat: ECDSA approvers disclose their public key and
// signature at proving time, so there is no signer-key privacy — an accepted
// trade-off for these operator-owned infrastructure accounts.
//
// Key material comes from the OS CSPRNG. `new_ecdsa_k256_keccak()` sources that
// randomness itself, which is why this does not thread an explicit `rand`
// generator: fewer moving parts, and no direct `rand` dependency to keep in step
// with miden's own version (PR #160 review). What matters either way is that the
// client's deterministic Felt coin is NOT a CryptoRng and must never be a
// secp256k1 key source.
// When a remote signer (`--signer-url`) is configured, the account is bound to
// the key the operator named for THIS ROLE: we resolve that identifier against
// the signer's key directory, fail if the signer does not expose it, build the
// approver from it, and generate nothing locally — the secret never exists on
// this host. The returned `AuthSecretKey` is `None` in that case, which is what
// tells the caller there is no local key to store.
//
// The role→key mapping is an explicit operator contract. An earlier version took
// `remote_commitments().first()`, but signer key ORDER is not a contract (it
// shifts when keys are added, removed or rotated) and a single-key signer bound
// every role to the same key — the opposite of blast-radius isolation.
pub async fn create_auth_component(
    keystore: &ProxyKeystore,
    role: Option<SignerRole>,
) -> anyhow::Result<(AuthSingleSig, Option<AuthSecretKey>)> {
    if keystore.is_remote() {
        let role = role.ok_or_else(|| {
            anyhow::anyhow!(
                "remote custody requires a signer role for every provisioned account, so each                  one binds to its own operator-named key"
            )
        })?;
        let identifier = keystore.signer_key_identifier(role).ok_or_else(|| {
            anyhow::anyhow!(
                "no signer key configured for role {}; pass --signer-key {}=<identifier>",
                role.as_str(),
                role.as_str()
            )
        })?;
        let commitment = keystore.commitment_for_identifier(&identifier)?;
        let public_key = keystore
            .get_public_key(commitment)
            .await
            .context("remote signer listed a key commitment it cannot resolve to a public key")?;
        tracing::info!(
            target: crate::COMPONENT,
            role = role.as_str(),
            "binding a new account to the REMOTE signer key configured for this role — no secret              is generated locally"
        );
        return Ok((
            AuthSingleSig::new(Approver::from(public_key.as_ref())),
            None,
        ));
    }
    let key_pair = AuthSecretKey::new_ecdsa_k256_keccak();
    let auth_component = AuthSingleSig::new(Approver::from(&key_pair.public_key()));
    Ok((auth_component, Some(key_pair)))
}

async fn deploy_account(
    client: &mut MidenClientLib,
    account_id: AccountId,
    name: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "deploying {} account {} ...",
        name,
        AccountIdBech32(account_id)
    );
    let dummy_txn = TransactionRequestBuilder::new().build()?;
    let txn_id = crate::metrics::meter_proof(
        crate::metrics::ProofKind::Init,
        crate::miden_client::submit_new_transaction(client, account_id, dummy_txn),
    )
    .await?;
    tracing::info!("deployed {name} account with txn_id {txn_id}");

    // Wait for the transaction to be committed (like ajl test's wait_for_tx)
    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("deploy tx {txn_id} committed");
    }
    Ok(())
}

async fn add_bridge(
    client: &mut MidenClientLib,
    _keystore: Arc<ProxyKeystore>,
    service_id: AccountId,
    ger_manager_id: AccountId,
    network_id: u32,
) -> anyhow::Result<Account> {
    // 0.16.0-alpha.5: the AggLayer network id is a per-bridge storage slot
    // again (agglayer::bridge::network_id, written once at account creation —
    // the 0.15.3 model; alpha.4 briefly regressed it to a compile-time MASM
    // constant). Must match the id the L1 RollupManager assigns this rollup,
    // or claims fail destination-network checks on both ends. 0.16 also split
    // the GER-manager role into injector + remover; we assign both roles to
    // the same ger_manager account (mirrors the upstream rust-sdk reference).
    let account = create_bridge_account(
        client.rng().draw_word(),
        service_id,
        ger_manager_id,
        ger_manager_id,
        network_id,
    );
    client.add_account(&account, false).await?;

    deploy_account(client, account.id(), "bridge").await?;

    Ok(account)
}

#[allow(clippy::too_many_arguments)]
async fn add_faucet(
    client: &mut MidenClientLib,
    token_symbol: &str,
    decimals: u8,
    origin_token_address: &[u8; 20],
    origin_network: u32,
    scale: u8,
    service_id: AccountId,
    bridge_account_id: AccountId,
    metadata_hash: MetadataHash,
) -> anyhow::Result<Account> {
    faucet_ops::create_and_register_faucet(
        client,
        token_symbol,
        decimals,
        origin_token_address,
        origin_network,
        scale,
        service_id,
        bridge_account_id,
        metadata_hash,
        false, // proxy-created faucet: bridge-owned mint/burn (not Miden-native)
    )
    .await
}

async fn add_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<ProxyKeystore>,
    role: Option<SignerRole>,
) -> anyhow::Result<Account> {
    // Public storage mode is REQUIRED for the proxy's infrastructure accounts
    // (service, ger_manager) so a missing local sqlite row can
    // be recovered via `Client::import_account_by_id` from the live Miden
    // node. Private accounts (the AccountBuilder default) cannot be
    // recovered — their full state lives ONLY in the proxy's sqlite. The
    // regression that put bali in this state was commit dbe5c2d (Apr 2026),
    // which folded `add_public_wallet` into `add_wallet` during the 0.14.x
    // migration and dropped the explicit storage_mode call. Bali ran with
    // Private accounts for ~20 days until the proxy's sqlite lost the
    // ger_manager row, after which every aggoracle GER push rejected with
    // `AccountDataNotFound` and `--reset-miden-store --restore` could not
    // bring it back.
    //
    // We use `Public` rather than `Network` because the latter is
    // testnet/devnet-only on current upstream — local miden-node builds
    // (and any production node not running with network-tx enabled) reject
    // Network deployments with `Network transactions may not be submitted
    // by users yet`. Public gives us the recovery property (state on-chain,
    // import-by-id works) without the network-tx-builder semantics, which
    // the proxy doesn't use anyway.
    let (auth_component, key_pair) = create_auth_component(keystore.as_ref(), role).await?;
    // Capture the commitment BEFORE the component is moved into the builder, so
    // the account↔key binding can be recorded once the id exists.
    let bound_commitment = auth_component.approver().pub_key();
    let account = Account::builder(client.rng().draw_word().into())
        .account_type(AccountType::Public)
        .with_component(BasicWallet)
        .with_auth_component(auth_component)
        .build()?;
    // Remote custody: remember which signer key signs for this account. Local
    // custody keeps its own index in the filesystem keystore, so this is a no-op.
    keystore.bind_account(account.id(), bound_commitment);
    // Remote-signer accounts have no local secret to persist.
    if let Some(key_pair) = key_pair.as_ref() {
        keystore.add_key(key_pair, account.id()).await?;
    }
    client.add_account(&account, false).await?;
    Ok(account)
}

/// Register the P2ID note tag for `wallet_id` so `sync_state` discovers incoming
/// P2ID (bridged-in) notes. The faucet's MASM `note_tag::create_account_target`
/// takes the top 14 bits of the account_id_prefix's high 32 bits:
/// `(prefix >> 32) & 0xFFFC0000`.
pub(crate) async fn register_wallet_p2id_tag(
    client: &mut MidenClientLib,
    wallet_id: AccountId,
) -> anyhow::Result<()> {
    use miden_protocol::note::NoteTag;
    let prefix_u64 = wallet_id.prefix().as_felt().as_canonical_u64();
    let hi32 = (prefix_u64 >> 32) as u32;
    let p2id_tag_value = hi32 & 0xFFFC0000u32; // top 14 bits
    let raw_tag = NoteTag::from(p2id_tag_value);
    tracing::info!(
        raw_tag = %u32::from(raw_tag),
        wallet = %AccountIdBech32(wallet_id),
        "registering P2ID note tag for wallet"
    );
    client.add_note_tag(raw_tag).await?;
    Ok(())
}

/// Create a standalone `Public` `BasicWallet` in `client`'s store and register
/// its P2ID note tag. Used by `bridge-out-tool --create-wallet` to stand up a
/// fully INDEPENDENT bridge-out wallet whose sqlite store is SEPARATE from the
/// proxy's — mirroring production, where the B2AGG (bridge-out) wallet is an
/// independent wallet the proxy never shares `store.sqlite3` with. The caller
/// is responsible for syncing afterwards to settle the account on the node.
pub async fn create_standalone_wallet(
    client: &mut MidenClientLib,
    keystore: Arc<ProxyKeystore>,
) -> anyhow::Result<Account> {
    // Operator tooling (bridge-out-tool) provisions these in LOCAL custody; in
    // remote custody this fails loudly rather than binding to an arbitrary key.
    let account = add_wallet(client, keystore, None).await?;
    register_wallet_p2id_tag(client, account.id()).await?;
    Ok(account)
}

async fn add_accounts(
    client: &mut MidenClientLib,
    keystore: Arc<ProxyKeystore>,
    network_id: u32,
) -> anyhow::Result<Accounts> {
    let service = add_wallet(client, keystore.clone(), Some(SignerRole::Service)).await?;
    let ger_manager = add_wallet(client, keystore.clone(), Some(SignerRole::GerManager)).await?;
    deploy_account(client, ger_manager.id(), "ger_manager").await?;
    let bridge = add_bridge(
        client,
        keystore.clone(),
        service.id(),
        ger_manager.id(),
        network_id,
    )
    .await?;
    // ETH: 18 origin decimals → 8 miden decimals (scale=10). Native ETH has empty metadata on
    // the L1 bridge, so the faucet's stored metadata_hash is keccak256("") — matches any
    // CLAIM leaf_data.metadata_hash for ETH deposits.
    let faucet_eth = add_faucet(
        client,
        "ETH",
        8,
        &[0u8; 20],
        0,
        10,
        service.id(),
        bridge.id(),
        MetadataHash::from_abi_encoded(&[]),
    )
    .await?;
    Ok(Accounts {
        service,
        bridge,
        faucet_eth,
        ger_manager,
    })
}

async fn init_internal(
    client: &mut MidenClientLib,
    keystore: Arc<ProxyKeystore>,
    net_id: NetworkId,
    network_id: u32,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    client.sync_state().await?;
    let accounts = add_accounts(client, keystore, network_id).await?;

    // Wait for the NTX builder to process account creation transactions
    // before submitting notes that target those accounts.
    tracing::info!("waiting for account transactions to settle...");
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        client.sync_state().await?;
    }
    tracing::info!("account settlement complete");

    // Faucet bridge registration is handled in create_and_register_faucet (via add_faucet)

    // 0.16: the 0.15-era register_p2id_script dummy-note step is gone. P2ID
    // notes are NoteType::Public and carry their script, and 0.16 requires
    // B2AGG notes to be public too, so the node no longer needs a script
    // pre-registered before NTX MINT->P2ID outputs (the upstream rust-sdk
    // agglayer reference flow performs no registration either). P2idNote also
    // now rejects empty asset lists, so the old dummy note cannot be built.

    let config = AccountsConfig::from(accounts);
    let config_path = accounts_config::save_config(config, &net_id, miden_store_dir)?;
    Ok(config_path)
}

pub async fn init(
    client: &MidenClient,
    net_id: NetworkId,
    network_id: u32,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let result = Arc::new(OnceLock::<PathBuf>::new());
    let result_internal = result.clone();
    let keystore = client.get_keystore();

    let future = client.with(move |client| {
        Box::new(async move {
            let result =
                init_internal(client, keystore, net_id, network_id, miden_store_dir).await?;
            result_internal.set(result).unwrap();
            Ok(())
        })
    });
    future.await?;

    Ok(result.get().unwrap().clone())
}

/// Rebuilds and VERIFIES the account↔signer-key bindings on every serving
/// startup (PR #162 review).
///
/// # Why this must run outside init
///
/// `bind_account` only ran while phase-1 init was creating accounts, against a
/// temporary client that is then shut down. The serving process builds a fresh
/// keystore, and a normal restart skips init entirely — so the binding map was
/// empty exactly when it mattered and `verify_bound_accounts` had nothing to
/// check. A verification that only runs on the one boot that creates the
/// accounts is not a verification.
///
/// # Why the deployed account is the source of truth
///
/// Inserting the CONFIGURED commitment and then checking it would be circular:
/// it proves the config agrees with itself. What matters is that the account
/// actually deployed on chain is signed for by the key the operator configured
/// AND that the signer still holds it. So for each role we read the account's
/// real auth commitment out of its `AuthSingleSig` storage slot and require all
/// three to agree.
pub async fn verify_remote_bindings(
    client: &mut MidenClientLib,
    keystore: &ProxyKeystore,
    accounts: &crate::accounts_config::AccountsConfig,
) -> anyhow::Result<usize> {
    use miden_standards::account::auth::AuthSingleSig;

    if !keystore.is_remote() {
        return Ok(0);
    }

    let configured = keystore.resolve_role_commitments()?;
    let mut roles: Vec<(SignerRole, AccountId)> = vec![(SignerRole::Service, accounts.service.0)];
    if let Some(ger) = accounts.ger_manager.as_ref() {
        roles.push((SignerRole::GerManager, ger.0));
    }

    for (role, account_id) in roles {
        let expected = configured.get(&role).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "no signer key configured for role {}; pass --signer-key {}=<identifier>",
                role.as_str(),
                role.as_str()
            )
        })?;

        // Import-by-id so a restart with a cold local store still verifies
        // against real on-chain state rather than skipping the check.
        if client
            .get_account(account_id)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            client.import_account_by_id(account_id).await.map_err(|e| {
                anyhow::anyhow!(
                    "cannot read the deployed {} account {account_id} to verify its signer key: {e}",
                    role.as_str()
                )
            })?;
        }
        let account = client
            .get_account(account_id)
            .await
            .map_err(|e| anyhow::anyhow!("get_account({account_id}): {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the deployed {} account {account_id} is unavailable; cannot verify custody",
                    role.as_str()
                )
            })?;

        let deployed: PublicKeyCommitment = account
            .storage()
            .get_item(AuthSingleSig::public_key_slot())
            .map_err(|e| {
                anyhow::anyhow!(
                    "cannot read the auth key slot of the deployed {} account {account_id}: {e}",
                    role.as_str()
                )
            })?
            .into();

        if deployed != expected {
            anyhow::bail!(
                "the deployed {} account {account_id} is signed for by a DIFFERENT key than \
                 --signer-key configures for that role. This deployment cannot sign for it. \
                 Either the signer/config was changed after the account was created, or this \
                 store was restored against the wrong signer.",
                role.as_str()
            );
        }
        keystore.bind_account(account_id, deployed);
    }

    let verified = keystore.verify_bound_accounts()?;
    tracing::info!(
        target: crate::COMPONENT,
        accounts = verified,
        "remote custody verified: every configured account's deployed auth key is the one \
         --signer-key names, and the signer still holds it"
    );
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::account::auth::AuthScheme;

    fn local_keystore() -> crate::proxy_keystore::ProxyKeystore {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        crate::proxy_keystore::ProxyKeystore::local(
            miden_client::keystore::FilesystemKeyStore::new(dir).expect("filesystem keystore"),
        )
    }

    /// PR #160 review: this branch's ONLY security-sensitive behaviour is which
    /// signature scheme newly provisioned operator accounts are deployed with,
    /// and it had no direct regression test. A silent revert to Falcon — or an
    /// approver derived from a different key than the one we keep — would have
    /// been caught by nothing.
    ///
    /// Pins all three halves of the binding:
    ///   * the secret key we persist is EcdsaK256Keccak;
    ///   * the auth component advertises EcdsaK256Keccak; and
    ///   * the component's approver commits to THAT key, not another.
    /// On this branch `create_auth_component` is signer-aware, so the test runs
    /// it in LOCAL custody — the only mode that generates a key here at all. In
    /// remote custody the approver comes from the signer and there is no local
    /// secret to pin (that path is covered by `proxy_keystore`'s tests).
    #[tokio::test]
    async fn create_auth_component_pins_ecdsa_k256_keccak() {
        let keystore = local_keystore();
        let (component, key_pair) = create_auth_component(&keystore, None)
            .await
            .expect("auth component");
        let key_pair = key_pair.expect("local custody must generate a local key");

        assert_eq!(
            key_pair.auth_scheme(),
            AuthScheme::EcdsaK256Keccak,
            "the generated secret key must be secp256k1/keccak — a Falcon key here would \
             silently re-introduce the scheme this branch removes"
        );
        assert_eq!(
            component.approver().auth_scheme(),
            AuthScheme::EcdsaK256Keccak,
            "the deployed component must advertise the same scheme as the key it is bound to"
        );
        assert_eq!(
            component.approver().pub_key(),
            key_pair.public_key().to_commitment(),
            "the approver must commit to the key we actually keep; a mismatch deploys an \
             account nobody can sign for"
        );
    }

    /// Two calls must not return the same key — i.e. this detects KEY REUSE
    /// across provisioned accounts. It is deliberately not a randomness-quality
    /// test: a single inequality cannot demonstrate that an RNG is sound, only
    /// that two draws differed. What it does catch is the concrete mistake of
    /// binding every account to one shared key.
    #[tokio::test]
    async fn create_auth_component_draws_fresh_key_material() {
        let keystore = local_keystore();
        let (a, ka) = create_auth_component(&keystore, None).await.expect("first");
        let (b, kb) = create_auth_component(&keystore, None)
            .await
            .expect("second");
        let (ka, kb) = (
            ka.expect("local custody generates a key"),
            kb.expect("local custody generates a key"),
        );
        assert_ne!(
            ka.public_key().to_commitment(),
            kb.public_key().to_commitment(),
            "two provisioned accounts must not share a key (blast-radius isolation)"
        );
        assert_ne!(a.approver().pub_key(), b.approver().pub_key());
    }
}
