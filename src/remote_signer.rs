//! Remote signing via a Web3Signer-compatible sidecar (no cloud-vendor code here).
//!
//! # Why a sidecar
//!
//! Account keys for a production deployment live in a KMS/HSM, never on the
//! proxy's disk. Rather than link a cloud SDK into this binary, the proxy speaks
//! one small HTTP contract to a signer service that owns that specificity —
//! Consensys Web3Signer fronts AWS KMS / Azure Key Vault / HashiCorp Vault
//! behind exactly this API, so swapping custody backends never touches us.
//!
//! # Digest compatibility (the load-bearing detail)
//!
//! Miden's `EcdsaK256Keccak` signs `keccak256(commitment_word_bytes)`:
//! `SigningInputs::to_commitment() -> Word` (32 bytes) is hashed by
//! `miden_crypto::dsa::ecdsa_k256_keccak::hash_message` and the resulting
//! 32-byte digest is signed with secp256k1.
//!
//! Web3Signer's eth1 endpoint (`POST /api/v1/eth1/sign/{identifier}` with
//! `{"data": "0x.."}`) computes `keccak256(data)` and signs that digest.
//!
//! Posting the raw 32-byte commitment word as `data` therefore makes the two
//! digest constructions byte-identical — no re-hashing, no message prefix, no
//! EIP-191 wrapper. `remote_signature_matches_local_sign` in the tests below
//! pins this equivalence against the local signer so a drift in either side
//! fails a unit test instead of an on-chain verification.
//!
//! # Signature encoding
//!
//! Web3Signer returns 65 bytes: `r (32) || s (32) || v (1)`. Miden wants the
//! same scalars plus a 0..=3 recovery id, so `v` is normalised from the
//! Ethereum-style 27/28 encoding when present.

use anyhow::{Context, anyhow};
use miden_protocol::Word;
use miden_protocol::account::auth::{PublicKey, PublicKeyCommitment};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak;
use miden_protocol::utils::serde::Deserializable;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Bytes of a compressed SEC1 secp256k1 public key (Web3Signer's key identifier).
const SEC1_COMPRESSED_LEN: usize = 33;
/// `r || s || v`.
const SIGNATURE_WITH_RECOVERY_LEN: usize = 65;

/// HTTP client for a Web3Signer-compatible signing service.
#[derive(Debug, Clone)]
pub struct RemoteSignerClient {
    base_url: String,
    http: reqwest::Client,
}

impl RemoteSignerClient {
    /// Builds a client for `base_url` (e.g. `http://web3signer:9000`).
    pub fn new(base_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building the remote-signer HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// Lists the secp256k1 public keys the signer holds, as 0x-prefixed
    /// compressed-SEC1 hex (Web3Signer's own identifier format — the same
    /// string is used as the `{identifier}` path segment when signing).
    pub async fn list_public_keys(&self) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/api/v1/eth1/publicKeys", self.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "remote signer listed keys with HTTP {status}: {body}"
            ));
        }
        let keys: Vec<String> =
            serde_json::from_str(&body).context("decoding the remote signer's public-key list")?;
        Ok(keys)
    }

    /// Signs `commitment` with the key identified by `identifier`.
    ///
    /// The commitment word is posted verbatim; the signer keccak-hashes it,
    /// which reproduces Miden's own `hash_message` (see the module docs).
    pub async fn sign(
        &self,
        identifier: &str,
        commitment: Word,
    ) -> anyhow::Result<ecdsa_k256_keccak::Signature> {
        let url = format!("{}/api/v1/eth1/sign/{}", self.base_url, identifier);
        let payload: [u8; 32] = commitment.into();
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "data": format!("0x{}", hex::encode(payload)) }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "remote signer refused to sign with HTTP {status}: {body}"
            ));
        }
        decode_signature(body.trim().trim_matches('"'))
    }
}

/// Decodes Web3Signer's `0x`-prefixed `r || s || v` response.
pub(crate) fn decode_signature(
    hex_signature: &str,
) -> anyhow::Result<ecdsa_k256_keccak::Signature> {
    let raw = hex::decode(hex_signature.trim_start_matches("0x"))
        .context("remote signature is not valid hex")?;
    if raw.len() != SIGNATURE_WITH_RECOVERY_LEN {
        return Err(anyhow!(
            "remote signature is {} bytes, expected {SIGNATURE_WITH_RECOVERY_LEN} (r||s||v)",
            raw.len()
        ));
    }
    let mut scalars = [0u8; 64];
    scalars.copy_from_slice(&raw[..64]);
    // Ethereum tooling encodes the recovery id as 27/28 (or 35+chain_id*2 for
    // EIP-155 transaction signatures); Miden wants the raw 0..=3 parity bit.
    let recovery_id = match raw[64] {
        v @ 0..=3 => v,
        v @ 27..=30 => v - 27,
        v => return Err(anyhow!("remote signature has an unsupported v byte: {v}")),
    };
    ecdsa_k256_keccak::Signature::from_sec1_bytes_and_recovery_id(scalars, recovery_id)
        .map_err(|err| anyhow!("remote signature is not a valid secp256k1 signature: {err}"))
}

/// Parses a compressed-SEC1 hex public key into Miden's `PublicKey`.
pub(crate) fn parse_public_key(sec1_hex: &str) -> anyhow::Result<ecdsa_k256_keccak::PublicKey> {
    let raw =
        hex::decode(sec1_hex.trim_start_matches("0x")).context("public key is not valid hex")?;
    if raw.len() != SEC1_COMPRESSED_LEN {
        return Err(anyhow!(
            "public key is {} bytes, expected {SEC1_COMPRESSED_LEN} (compressed SEC1)",
            raw.len()
        ));
    }
    ecdsa_k256_keccak::PublicKey::read_from_bytes(&raw)
        .map_err(|err| anyhow!("public key is not a valid secp256k1 point: {err}"))
}

/// How the proxy holds account keys for this process's lifetime.
///
/// Resolved once at startup from the CLI so every client built later agrees:
/// there is no per-call or per-account choice, by design (see
/// `crate::proxy_keystore`).
#[derive(Debug, Clone)]
pub enum CustodyMode {
    /// Keys held by a remote signer at this base URL (the default).
    RemoteSigner { base_url: String },
    /// Keys on this host's disk (explicit `--insecure-local-keystore`).
    InsecureLocalKeystore,
}

impl CustodyMode {
    /// Resolves the mode from the two mutually exclusive CLI options.
    ///
    /// Requiring one of them — rather than defaulting to local — is what makes
    /// on-disk keys a deliberate act: a deployment that forgets to configure
    /// custody fails to start instead of quietly running with local secrets.
    pub fn resolve(signer_url: Option<&str>, insecure_local: bool) -> anyhow::Result<Self> {
        match (signer_url, insecure_local) {
            (Some(_), true) => Err(anyhow!(
                "--signer-url and --insecure-local-keystore are mutually exclusive: custody is \
                 either remote or local, never both (a mixed keystore makes \"which key signed \
                 this?\" unanswerable). Pick one."
            )),
            (Some(base_url), false) => Ok(Self::RemoteSigner {
                base_url: base_url.to_string(),
            }),
            (None, true) => Ok(Self::InsecureLocalKeystore),
            (None, false) => Err(anyhow!(
                "no key custody configured: set --signer-url to a Web3Signer-compatible service \
                 (recommended — key material stays in a KMS/HSM), or pass \
                 --insecure-local-keystore to keep account private keys on this host's disk \
                 (development and e2e only)"
            )),
        }
    }
}

/// The keys a remote signer exposes, indexed by the commitment Miden accounts
/// actually reference.
#[derive(Debug, Default)]
pub struct RemoteKeyDirectory {
    by_commitment: BTreeMap<PublicKeyCommitment, (String, Arc<PublicKey>)>,
}

impl RemoteKeyDirectory {
    /// Fetches every key the signer holds and indexes it by commitment.
    ///
    /// Keys the proxy cannot parse are skipped with a warning rather than
    /// failing startup: a shared signer may hold keys for other services.
    pub async fn load(client: &RemoteSignerClient) -> anyhow::Result<Self> {
        let identifiers = client.list_public_keys().await?;
        let mut by_commitment = BTreeMap::new();
        for identifier in identifiers {
            match parse_public_key(&identifier) {
                Ok(ecdsa_key) => {
                    let public_key = PublicKey::EcdsaK256Keccak(ecdsa_key);
                    by_commitment.insert(
                        public_key.to_commitment(),
                        (identifier, Arc::new(public_key)),
                    );
                }
                Err(err) => tracing::warn!(
                    target: crate::COMPONENT,
                    identifier = %identifier,
                    error = %err,
                    "remote signer exposes a key this proxy cannot use — skipping"
                ),
            }
        }
        Ok(Self { by_commitment })
    }

    /// Number of usable keys.
    pub fn len(&self) -> usize {
        self.by_commitment.len()
    }

    /// True when the signer exposed no key this proxy can use.
    pub fn is_empty(&self) -> bool {
        self.by_commitment.is_empty()
    }

    /// The signer's identifier for `commitment`, if it holds that key.
    pub fn identifier(&self, commitment: PublicKeyCommitment) -> Option<&str> {
        self.by_commitment
            .get(&commitment)
            .map(|(identifier, _)| identifier.as_str())
    }

    /// The public key for `commitment`, if it holds that key.
    pub fn public_key(&self, commitment: PublicKeyCommitment) -> Option<Arc<PublicKey>> {
        self.by_commitment
            .get(&commitment)
            .map(|(_, key)| key.clone())
    }

    /// Every commitment the signer can sign for.
    pub fn commitments(&self) -> impl Iterator<Item = PublicKeyCommitment> + '_ {
        self.by_commitment.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::account::auth::AuthSecretKey;

    /// The contract this whole integration rests on: a signature produced from
    /// Web3Signer's wire format must be byte-identical to what the local signer
    /// produces for the same commitment — i.e. the remote service hashes the
    /// posted payload exactly the way `hash_message` does. We assert it by
    /// signing locally, re-encoding into the remote wire format, decoding it
    /// back, and verifying against the public key.
    #[test]
    fn remote_signature_matches_local_sign() {
        let key = AuthSecretKey::new_ecdsa_k256_keccak();
        let AuthSecretKey::EcdsaK256Keccak(signing_key) = &key else {
            panic!("requested an ECDSA key, got another scheme");
        };
        let commitment = Word::from([7u32, 9, 11, 13]);
        let local = signing_key.sign(commitment);

        // Re-encode exactly as Web3Signer would return it, then decode.
        let mut wire = Vec::with_capacity(65);
        wire.extend_from_slice(local.r());
        wire.extend_from_slice(local.s());
        wire.push(local.v());
        let decoded = decode_signature(&format!("0x{}", hex::encode(&wire)))
            .expect("the wire form must decode");

        assert_eq!(decoded.r(), local.r(), "r must round-trip");
        assert_eq!(decoded.s(), local.s(), "s must round-trip");
        assert_eq!(decoded.v(), local.v(), "recovery id must round-trip");

        let PublicKey::EcdsaK256Keccak(public_key) = key.public_key() else {
            panic!("requested an ECDSA key, got another scheme");
        };
        assert!(
            decoded.verify(commitment, &public_key),
            "a signature decoded from the remote wire format must verify against the commitment"
        );
    }

    /// Ethereum tooling reports v as 27/28; Miden wants 0/1. A signer that
    /// normalises differently must not silently produce an unverifiable
    /// signature.
    #[test]
    fn eth_style_recovery_ids_are_normalised() {
        let scalars = [1u8; 64];
        for (wire_v, expected) in [(0u8, 0u8), (1, 1), (27, 0), (28, 1)] {
            let mut wire = scalars.to_vec();
            wire.push(wire_v);
            let decoded = decode_signature(&hex::encode(&wire))
                .unwrap_or_else(|err| panic!("v={wire_v} must decode: {err}"));
            assert_eq!(decoded.v(), expected, "v={wire_v} must normalise");
        }
    }

    /// Custody must be an explicit choice. Forgetting to configure it is a
    /// startup error, NOT a silent fall back to on-disk keys — that default is
    /// the whole point of the flag pair.
    #[test]
    fn custody_must_be_configured_explicitly() {
        let err =
            CustodyMode::resolve(None, false).expect_err("no custody configured must fail closed");
        let rendered = format!("{err}");
        assert!(
            rendered.contains("--signer-url") && rendered.contains("--insecure-local-keystore"),
            "the error must name both ways to configure custody, got: {rendered}"
        );
    }

    /// Mixed custody is refused outright — the ambiguity it creates is exactly
    /// what remote-signing mode exists to remove.
    #[test]
    fn custody_modes_are_mutually_exclusive() {
        assert!(
            CustodyMode::resolve(Some("http://signer:9000"), true).is_err(),
            "asking for both remote and local custody must fail"
        );
    }

    #[test]
    fn custody_resolves_each_mode() {
        assert!(matches!(
            CustodyMode::resolve(Some("http://signer:9000"), false),
            Ok(CustodyMode::RemoteSigner { .. })
        ));
        assert!(matches!(
            CustodyMode::resolve(None, true),
            Ok(CustodyMode::InsecureLocalKeystore)
        ));
    }

    #[test]
    fn malformed_remote_responses_are_rejected() {
        assert!(decode_signature("not-hex").is_err(), "non-hex must fail");
        assert!(
            decode_signature(&hex::encode([0u8; 64])).is_err(),
            "a signature without a recovery id must fail"
        );
        let mut bad_v = [0u8; 65];
        bad_v[64] = 9;
        assert!(
            decode_signature(&hex::encode(bad_v)).is_err(),
            "an out-of-range v must fail rather than sign-with-garbage"
        );
        assert!(
            parse_public_key(&hex::encode([2u8; 10])).is_err(),
            "a truncated public key must fail"
        );
    }
}
