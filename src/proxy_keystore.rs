//! The keystore the proxy hands to `miden-client`: local keys by default,
//! remote (Web3Signer/KMS) signing when `--signer-url` is configured.
//!
//! Keeping both behind one type means every call site — init, the writer
//! worker, the bridge-out tool — is unchanged whether keys live on disk or in
//! a vault. Only the signing step differs, and only for keys the remote signer
//! actually holds:
//!
//! * `get_signature` asks the remote signer when the requested commitment is in
//!   its key directory, and falls back to the local store otherwise. That
//!   fallback is what lets a deployment migrate one account at a time.
//! * key *management* (`add_key`, `get_key`, the account↔commitment index) is
//!   always local: remote keys are provisioned out-of-band in the vault and
//!   their secrets are, by design, unreachable from here. `get_key` returning
//!   `None` for them is correct — the signing path never needs the secret.
//!
//! Failing closed matters here: if the signer is configured but cannot sign,
//! we surface the error rather than silently falling back to a local key that
//! may be a stale copy of vault material.

use crate::remote_signer::{RemoteKeyDirectory, RemoteSignerClient};
use miden_client::keystore::{FilesystemKeyStore, KeyStoreError, Keystore};
use miden_protocol::account::AccountId;
use miden_protocol::account::auth::{AuthSecretKey, PublicKey, PublicKeyCommitment, Signature};
use miden_tx::AuthenticationError;
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A keystore that can sign locally, remotely, or both.
#[derive(Debug, Clone)]
pub struct ProxyKeystore {
    local: FilesystemKeyStore,
    remote: Option<Arc<RemoteBackend>>,
}

#[derive(Debug)]
struct RemoteBackend {
    client: RemoteSignerClient,
    directory: RemoteKeyDirectory,
}

impl ProxyKeystore {
    /// Local-only keystore (the default; unchanged behaviour).
    pub fn local(local: FilesystemKeyStore) -> Self {
        Self {
            local,
            remote: None,
        }
    }

    /// Attaches a remote signer, loading its key directory once at startup.
    ///
    /// Errors if the signer is unreachable or exposes no usable key: a
    /// configured-but-unusable signer must fail loudly at boot rather than at
    /// the first transaction.
    pub async fn with_remote_signer(
        local: FilesystemKeyStore,
        client: RemoteSignerClient,
    ) -> anyhow::Result<Self> {
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
            "remote signer attached; these key commitments will be signed remotely"
        );
        Ok(Self {
            local,
            remote: Some(Arc::new(RemoteBackend { client, directory })),
        })
    }

    /// The commitments the remote signer can sign for (empty when local-only).
    pub fn remote_commitments(&self) -> Vec<PublicKeyCommitment> {
        self.remote
            .as_ref()
            .map(|backend| backend.directory.commitments().collect())
            .unwrap_or_default()
    }

    /// True when a remote signer holds this key.
    fn is_remote(&self, commitment: PublicKeyCommitment) -> bool {
        self.remote
            .as_ref()
            .is_some_and(|backend| backend.directory.identifier(commitment).is_some())
    }
}

impl From<FilesystemKeyStore> for ProxyKeystore {
    fn from(local: FilesystemKeyStore) -> Self {
        Self::local(local)
    }
}

impl TransactionAuthenticator for ProxyKeystore {
    async fn get_signature(
        &self,
        pub_key: PublicKeyCommitment,
        signing_info: &SigningInputs,
    ) -> Result<Signature, AuthenticationError> {
        if let Some(backend) = self.remote.as_ref()
            && let Some(identifier) = backend.directory.identifier(pub_key)
        {
            // The commitment word IS the message the signer hashes; see
            // `remote_signer`'s digest-compatibility notes.
            let commitment = signing_info.to_commitment();
            let signature = backend
                .client
                .sign(identifier, commitment)
                .await
                .map_err(|err| {
                    AuthenticationError::other(format!(
                        "remote signer failed to sign for {identifier}: {err:#}"
                    ))
                })?;
            return Ok(Signature::EcdsaK256Keccak(signature));
        }
        self.local.get_signature(pub_key, signing_info).await
    }

    async fn get_public_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Option<Arc<PublicKey>> {
        if let Some(backend) = self.remote.as_ref()
            && let Some(key) = backend.directory.public_key(pub_key_commitment)
        {
            return Some(key);
        }
        self.local.get_public_key(pub_key_commitment).await
    }
}

#[async_trait::async_trait]
impl Keystore for ProxyKeystore {
    async fn add_key(
        &self,
        key: &AuthSecretKey,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        self.local.add_key(key, account_id).await
    }

    async fn remove_key(&self, pub_key: PublicKeyCommitment) -> Result<(), KeyStoreError> {
        if self.is_remote(pub_key) {
            // Vault-held material is not ours to delete, and silently
            // "succeeding" would hide that from an operator.
            return Err(KeyStoreError::StorageError(format!(
                "{pub_key:?} is held by the remote signer; remove it in the signer/KMS instead"
            )));
        }
        self.local.remove_key(pub_key).await
    }

    async fn get_key(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        // Remote secrets are unreachable by construction; `None` is the honest
        // answer and the signing path never needs them.
        self.local.get_key(pub_key).await
    }

    async fn get_account_key_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError> {
        self.local.get_account_key_commitments(account_id).await
    }

    async fn get_account_id_by_key_commitment(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Option<AccountId>, KeyStoreError> {
        self.local
            .get_account_id_by_key_commitment(pub_key_commitment)
            .await
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

    /// Without a signer configured the proxy must behave exactly as before:
    /// local keys sign, and nothing reports as remote.
    #[tokio::test]
    async fn local_only_keystore_signs_with_local_keys() {
        let keystore = ProxyKeystore::local(temp_local());
        let key = AuthSecretKey::new_ecdsa_k256_keccak_with_rng(&mut rand::rng());
        let account = AccountId::from_hex("0xaa0000000000bc310000bc000000de").unwrap();
        let commitment = key.public_key().to_commitment();
        keystore.add_key(&key, account).await.expect("add_key");

        assert!(
            keystore.remote_commitments().is_empty(),
            "a local-only keystore must report no remote keys"
        );
        assert!(
            !keystore.is_remote(commitment),
            "a local key must never be treated as remote"
        );

        let signature = keystore
            .get_signature(
                commitment,
                &SigningInputs::Blind(Word::from([1u32, 2, 3, 4])),
            )
            .await
            .expect("local signing must work");
        assert!(
            matches!(signature, Signature::EcdsaK256Keccak(_)),
            "the local signer must produce an ECDSA signature"
        );
    }

    /// The account↔key index stays local even when a signer is attached, so
    /// existing lookups keep working.
    #[tokio::test]
    async fn key_management_stays_local() {
        let keystore = ProxyKeystore::local(temp_local());
        let key = AuthSecretKey::new_ecdsa_k256_keccak_with_rng(&mut rand::rng());
        let account = AccountId::from_hex("0xaa0000000000bc310000bc000000de").unwrap();
        let commitment = key.public_key().to_commitment();
        keystore.add_key(&key, account).await.expect("add_key");

        assert_eq!(
            keystore
                .get_account_id_by_key_commitment(commitment)
                .await
                .expect("index lookup"),
            Some(account),
            "the account index must resolve a locally added key"
        );
        assert!(
            keystore
                .get_key(commitment)
                .await
                .expect("get_key")
                .is_some(),
            "a locally added secret must be retrievable"
        );
    }
}
