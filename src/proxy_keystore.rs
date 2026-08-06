//! The keystore the proxy hands to `miden-client`.
//!
//! Custody is **either/or, never mixed**: the proxy runs in exactly one of two
//! modes for its whole lifetime.
//!
//! * [`ProxyKeystore::remote`] — the default. Every account signature is
//!   produced by a Web3Signer-compatible service that owns the key (AWS KMS /
//!   Azure Key Vault / HashiCorp Vault behind it). This process holds no
//!   secret, generates none, and stores none: `add_key` is refused and
//!   `get_key` is always `None`. A signing request for a key the signer does
//!   not hold is a hard error.
//! * [`ProxyKeystore::local`] — an explicit `--insecure-local-keystore`
//!   opt-in for development and the e2e suite, where keys live on disk.
//!
//! # Why there is no fallback
//!
//! An earlier draft let remote mode fall back to a local key when the signer
//! did not hold the requested commitment, to ease per-account migration. That
//! is exactly the property you do not want in a custody boundary: a
//! misconfigured signer, a partially-provisioned vault, or a stale on-disk key
//! would all keep signing — silently, from the wrong place — and every
//! observable symptom (transactions land, health stays green) is identical to
//! correct operation. Migration convenience is not worth an ambiguous answer to
//! "which key signed this?", so remote mode fails loudly instead.

use crate::remote_signer::{RemoteKeyDirectory, RemoteSignerClient};
use miden_client::keystore::{FilesystemKeyStore, KeyStoreError, Keystore};
use miden_protocol::account::AccountId;
use miden_protocol::account::auth::{AuthSecretKey, PublicKey, PublicKeyCommitment, Signature};
use miden_tx::AuthenticationError;
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Custody mode: local keys on disk, or a remote signer. Never both.
#[derive(Debug, Clone)]
pub enum ProxyKeystore {
    /// Keys on this host's disk (`--insecure-local-keystore`; dev/e2e only).
    Local(FilesystemKeyStore),
    /// Keys held by a remote signer; no secret exists in this process.
    Remote(Arc<RemoteBackend>),
}

/// A remote signer plus the key directory it exposed at startup.
#[derive(Debug)]
pub struct RemoteBackend {
    client: RemoteSignerClient,
    directory: RemoteKeyDirectory,
}

impl ProxyKeystore {
    /// Local-keystore mode. Requires the explicit insecure opt-in at the CLI.
    pub fn local(local: FilesystemKeyStore) -> Self {
        Self::Local(local)
    }

    /// Remote-signer mode (the default): loads the signer's key directory once
    /// at startup.
    ///
    /// Errors if the signer is unreachable or exposes no usable key — a
    /// configured-but-unusable signer must fail at boot, not at the first
    /// transaction.
    pub async fn remote(client: RemoteSignerClient) -> anyhow::Result<Self> {
        let directory = RemoteKeyDirectory::load(&client).await?;
        if directory.is_empty() {
            anyhow::bail!(
                "remote signer is reachable but exposes no usable secp256k1 key — refusing to \
                 start with a signer that cannot sign for any account"
            );
        }
        tracing::info!(
            target: crate::COMPONENT,
            remote_keys = directory.len(),
            "remote signer attached — ALL account signing is remote; this process holds no key"
        );
        Ok(Self::Remote(Arc::new(RemoteBackend { client, directory })))
    }

    /// Builds a remote backend from parts (test seam).
    #[cfg(test)]
    pub(crate) fn remote_from_parts(
        client: RemoteSignerClient,
        directory: RemoteKeyDirectory,
    ) -> Self {
        Self::Remote(Arc::new(RemoteBackend { client, directory }))
    }

    /// True when signing is remote.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// The commitments the signer can sign for (empty in local mode).
    pub fn remote_commitments(&self) -> Vec<PublicKeyCommitment> {
        match self {
            Self::Local(_) => Vec::new(),
            Self::Remote(backend) => backend.directory.commitments().collect(),
        }
    }
}

impl From<FilesystemKeyStore> for ProxyKeystore {
    fn from(local: FilesystemKeyStore) -> Self {
        Self::Local(local)
    }
}

impl TransactionAuthenticator for ProxyKeystore {
    async fn get_signature(
        &self,
        pub_key: PublicKeyCommitment,
        signing_info: &SigningInputs,
    ) -> Result<Signature, AuthenticationError> {
        match self {
            Self::Local(local) => local.get_signature(pub_key, signing_info).await,
            Self::Remote(backend) => {
                // No fallback by design: an unknown commitment means the vault
                // is not provisioned for this account, and signing it from
                // anywhere else would defeat the custody boundary.
                let identifier = backend.directory.identifier(pub_key).ok_or_else(|| {
                    AuthenticationError::other(format!(
                        "remote signer does not hold the key for {pub_key:?}; refusing to sign \
                         (remote-signing mode never falls back to a local key — provision this \
                         key in the signer, or start with --insecure-local-keystore)"
                    ))
                })?;
                // The commitment word IS the message the signer hashes; see
                // `remote_signer`'s digest-compatibility notes.
                let signature = backend
                    .client
                    .sign(identifier, signing_info.to_commitment())
                    .await
                    .map_err(|err| {
                        AuthenticationError::other(format!(
                            "remote signer failed to sign for {identifier}: {err:#}"
                        ))
                    })?;
                Ok(Signature::EcdsaK256Keccak(signature))
            }
        }
    }

    async fn get_public_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Option<Arc<PublicKey>> {
        match self {
            Self::Local(local) => local.get_public_key(pub_key_commitment).await,
            Self::Remote(backend) => backend.directory.public_key(pub_key_commitment),
        }
    }
}

#[async_trait::async_trait]
impl Keystore for ProxyKeystore {
    async fn add_key(
        &self,
        key: &AuthSecretKey,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        match self {
            Self::Local(local) => local.add_key(key, account_id).await,
            // Writing a secret to disk here would recreate the mixed-custody
            // hole this mode exists to close.
            Self::Remote(_) => Err(KeyStoreError::StorageError(
                "refusing to store a secret key on disk in remote-signing mode — provision keys \
                 in the signer, or start with --insecure-local-keystore"
                    .to_string(),
            )),
        }
    }

    async fn remove_key(&self, pub_key: PublicKeyCommitment) -> Result<(), KeyStoreError> {
        match self {
            Self::Local(local) => local.remove_key(pub_key).await,
            Self::Remote(_) => Err(KeyStoreError::StorageError(format!(
                "{pub_key:?} is held by the remote signer; remove it in the signer/KMS instead"
            ))),
        }
    }

    async fn get_key(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        match self {
            Self::Local(local) => local.get_key(pub_key).await,
            // Remote secrets are unreachable by construction; `None` is the
            // honest answer and the signing path never needs them.
            Self::Remote(_) => Ok(None),
        }
    }

    async fn get_account_key_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError> {
        match self {
            Self::Local(local) => local.get_account_key_commitments(account_id).await,
            // In remote mode the signer's keys are the only ones that can sign
            // for any account; the account's own auth component pins which
            // commitment is actually required, and `get_signature` rejects
            // anything the signer does not hold.
            Self::Remote(backend) => Ok(backend.directory.commitments().collect()),
        }
    }

    async fn get_account_id_by_key_commitment(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Option<AccountId>, KeyStoreError> {
        match self {
            Self::Local(local) => {
                local
                    .get_account_id_by_key_commitment(pub_key_commitment)
                    .await
            }
            // A vault key is not owned by a single account here; the binding
            // lives in the deployed accounts themselves.
            Self::Remote(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::Word;

    fn temp_local() -> FilesystemKeyStore {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        FilesystemKeyStore::new(dir).expect("filesystem keystore")
    }

    fn unreachable_remote() -> ProxyKeystore {
        ProxyKeystore::remote_from_parts(
            RemoteSignerClient::new("http://127.0.0.1:1", std::time::Duration::from_secs(1))
                .expect("client"),
            RemoteKeyDirectory::default(),
        )
    }

    fn some_commitment() -> PublicKeyCommitment {
        AuthSecretKey::new_ecdsa_k256_keccak()
            .public_key()
            .to_commitment()
    }

    /// Local mode is unchanged: local keys sign, nothing reports as remote.
    #[tokio::test]
    async fn local_mode_signs_with_local_keys() {
        let keystore = ProxyKeystore::local(temp_local());
        let key = AuthSecretKey::new_ecdsa_k256_keccak();
        let account = AccountId::from_hex("0xaa0000000000bc310000bc000000de").unwrap();
        let commitment = key.public_key().to_commitment();
        keystore.add_key(&key, account).await.expect("add_key");

        assert!(
            !keystore.is_remote(),
            "local mode must not report as remote"
        );
        assert!(
            keystore.remote_commitments().is_empty(),
            "local mode exposes no remote commitments"
        );

        let signature = keystore
            .get_signature(
                commitment,
                &SigningInputs::Blind(Word::from([1u32, 2, 3, 4])),
            )
            .await
            .expect("local signing must work");
        assert!(matches!(signature, Signature::EcdsaK256Keccak(_)));
        assert_eq!(
            keystore
                .get_account_id_by_key_commitment(commitment)
                .await
                .expect("index lookup"),
            Some(account),
            "local mode keeps the account index"
        );
    }

    /// Remote mode must never write a secret to disk — that would recreate the
    /// mixed-custody hole the mode exists to close.
    #[tokio::test]
    async fn remote_mode_refuses_to_store_secrets() {
        let keystore = unreachable_remote();
        let key = AuthSecretKey::new_ecdsa_k256_keccak();
        let account = AccountId::from_hex("0xaa0000000000bc310000bc000000de").unwrap();

        assert!(keystore.is_remote(), "remote mode must report as remote");
        assert!(
            keystore.add_key(&key, account).await.is_err(),
            "remote mode must refuse to persist a secret key"
        );
        assert!(
            keystore
                .get_key(key.public_key().to_commitment())
                .await
                .expect("get_key must not error")
                .is_none(),
            "remote mode must never surface a secret"
        );
    }

    /// The no-fallback rule: a key the signer does not hold is an error, not a
    /// silent local signature. This is what keeps "which key signed this?"
    /// unambiguous.
    #[tokio::test]
    async fn remote_mode_never_falls_back_to_a_local_key() {
        let keystore = unreachable_remote();
        let error = keystore
            .get_signature(
                some_commitment(),
                &SigningInputs::Blind(Word::from([1u32, 2, 3, 4])),
            )
            .await
            .expect_err("signing an unheld key must fail, never fall back");
        let rendered = format!("{error}");
        assert!(
            rendered.contains("does not hold the key"),
            "the error must name the real cause, got: {rendered}"
        );
    }
}
