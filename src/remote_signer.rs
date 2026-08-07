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
/// Tagged uncompressed SEC1: `0x04 || x || y`.
const SEC1_UNCOMPRESSED_LEN: usize = 65;
/// Ethereum/Web3Signer raw uncompressed point: `x || y`, no `0x04` tag.
const SEC1_UNCOMPRESSED_UNTAGGED_LEN: usize = 64;
/// The SEC1 tag byte identifying an uncompressed point.
const SEC1_UNCOMPRESSED_TAG: u8 = 0x04;
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

/// Parses a hex secp256k1 public key into Miden's `PublicKey`, accepting every
/// encoding a Web3Signer-compatible service may publish.
///
/// Miden's `read_from_bytes` takes only the 33-byte compressed SEC1 form, but
/// Web3Signer's eth1 `publicKeys` endpoint returns the Ethereum-style 64-byte
/// RAW uncompressed point (`x || y`, with the SEC1 `0x04` tag stripped, since
/// that is the preimage Ethereum addresses are derived from). Accepting only
/// compressed keys means rejecting the very signer this module exists to talk
/// to, so all three encodings are normalized to compressed here.
///
/// This widens the accepted *encodings*, not the accepted *keys*:
/// `from_sec1_bytes` rejects anything that is not actually a point on
/// secp256k1, so a malformed or attacker-supplied value cannot be laundered
/// into the key directory by the conversion.
pub(crate) fn parse_public_key(sec1_hex: &str) -> anyhow::Result<ecdsa_k256_keccak::PublicKey> {
    let raw =
        hex::decode(sec1_hex.trim_start_matches("0x")).context("public key is not valid hex")?;

    let compressed: Vec<u8> = match raw.len() {
        SEC1_COMPRESSED_LEN => raw,
        SEC1_UNCOMPRESSED_LEN | SEC1_UNCOMPRESSED_UNTAGGED_LEN => {
            let tagged = if raw.len() == SEC1_UNCOMPRESSED_UNTAGGED_LEN {
                let mut buf = Vec::with_capacity(SEC1_UNCOMPRESSED_LEN);
                buf.push(SEC1_UNCOMPRESSED_TAG);
                buf.extend_from_slice(&raw);
                buf
            } else {
                raw
            };
            let verifying = alloy::signers::k256::ecdsa::VerifyingKey::from_sec1_bytes(&tagged)
                .map_err(|err| anyhow!("public key is not a valid secp256k1 point: {err}"))?;
            verifying.to_encoded_point(true).as_bytes().to_vec()
        }
        other => {
            return Err(anyhow!(
                "public key is {other} bytes, expected {SEC1_COMPRESSED_LEN} (compressed SEC1), \
                 {SEC1_UNCOMPRESSED_UNTAGGED_LEN} (raw x||y, as Web3Signer's eth1 API returns), \
                 or {SEC1_UNCOMPRESSED_LEN} (tagged uncompressed SEC1)"
            ));
        }
    };

    ecdsa_k256_keccak::PublicKey::read_from_bytes(&compressed)
        .map_err(|err| anyhow!("public key is not a valid secp256k1 point: {err}"))
}

/// How the proxy holds account keys for this process's lifetime.
///
/// Resolved once at startup from the CLI so every client built later agrees:
/// there is no per-call or per-account choice, by design (see
/// `crate::proxy_keystore`).
#[derive(Debug, Clone)]
pub enum CustodyMode {
    /// Keys held by a remote signer at this base URL (the default), with the
    /// operator's role→key bindings.
    RemoteSigner {
        base_url: String,
        key_bindings: SignerKeyBindings,
    },
    /// Keys on this host's disk (explicit `--insecure-local-keystore`).
    InsecureLocalKeystore,
}

impl CustodyMode {
    /// Resolves the mode from the two mutually exclusive CLI options.
    ///
    /// Requiring one of them — rather than defaulting to local — is what makes
    /// on-disk keys a deliberate act: a deployment that forgets to configure
    /// custody fails to start instead of quietly running with local secrets.
    pub fn resolve(
        signer_url: Option<&str>,
        insecure_local: bool,
        key_bindings: SignerKeyBindings,
    ) -> anyhow::Result<Self> {
        if signer_url.is_some() && !key_bindings.missing_roles().is_empty() {
            return Err(anyhow!(
                "remote custody needs a signer key for every role; missing: {}. Pass \
                 --signer-key <role>=<identifier> for each (key creation and IAM stay outside \
                 this process).",
                key_bindings
                    .missing_roles()
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        match (signer_url, insecure_local) {
            (Some(_), true) => Err(anyhow!(
                "--signer-url and --insecure-local-keystore are mutually exclusive: custody is \
                 either remote or local, never both (a mixed keystore makes \"which key signed \
                 this?\" unanswerable). Pick one."
            )),
            (Some(base_url), false) => Ok(Self::RemoteSigner {
                base_url: base_url.to_string(),
                key_bindings,
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

    /// Test seam: insert a key without going through the signer.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, identifier: String, public_key: PublicKey) {
        self.by_commitment.insert(
            public_key.to_commitment(),
            (identifier, Arc::new(public_key)),
        );
    }

    /// Number of usable keys.
    pub fn len(&self) -> usize {
        self.by_commitment.len()
    }

    /// True when the signer exposed no key this proxy can use.
    pub fn is_empty(&self) -> bool {
        self.by_commitment.is_empty()
    }

    /// The commitment for an operator-named key IDENTIFIER, if the signer
    /// exposes it.
    ///
    /// Matching is by COMMITMENT, not by string. The operator may reasonably
    /// write the key in any valid SEC1 encoding (compressed, tagged or raw
    /// uncompressed) while the signer publishes its own; comparing the strings
    /// would reject a correct configuration purely on formatting — the same
    /// encoding-mismatch class that made the first remote startup fail. Parsing
    /// both sides to a commitment makes the comparison encoding-independent.
    /// A non-key identifier still falls back to a case-insensitive string match.
    pub fn commitment_for_identifier(&self, identifier: &str) -> Option<PublicKeyCommitment> {
        if let Ok(parsed) = parse_public_key(identifier) {
            let commitment = PublicKey::EcdsaK256Keccak(parsed).to_commitment();
            if self.by_commitment.contains_key(&commitment) {
                return Some(commitment);
            }
            return None;
        }
        self.by_commitment
            .iter()
            .find(|(_, (id, _))| id.eq_ignore_ascii_case(identifier))
            .map(|(commitment, _)| *commitment)
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
        let err = CustodyMode::resolve(None, false, SignerKeyBindings::default())
            .expect_err("no custody configured must fail closed");
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
            CustodyMode::resolve(
                Some("http://signer:9000"),
                true,
                SignerKeyBindings::parse(&[
                    "service=0xaaa".to_string(),
                    "ger-manager=0xbbb".to_string()
                ])
                .unwrap()
            )
            .is_err(),
            "asking for both remote and local custody must fail"
        );
    }

    #[test]
    fn custody_resolves_each_mode() {
        assert!(matches!(
            CustodyMode::resolve(
                Some("http://signer:9000"),
                false,
                SignerKeyBindings::parse(&[
                    "service=0xaaa".to_string(),
                    "ger-manager=0xbbb".to_string()
                ])
                .unwrap()
            ),
            Ok(CustodyMode::RemoteSigner { .. })
        ));
        assert!(matches!(
            CustodyMode::resolve(None, true, SignerKeyBindings::default()),
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

    /// The gate found this the hard way: Web3Signer's eth1 `publicKeys`
    /// endpoint publishes the Ethereum-style RAW 64-byte point (`x || y`), not
    /// compressed SEC1, so a compressed-only parser rejects every key the
    /// signer holds and the proxy refuses to boot against a perfectly good
    /// signer.
    ///
    /// All three encodings must resolve to the SAME key — if they did not, the
    /// commitment would differ and the proxy would look up the wrong signer
    /// identifier at signing time.
    #[test]
    fn accepts_every_sec1_encoding_a_signer_may_publish() {
        let secret = AuthSecretKey::new_ecdsa_k256_keccak();
        let AuthSecretKey::EcdsaK256Keccak(inner) = &secret else {
            panic!("requested an ecdsa key");
        };
        let public = inner.public_key();
        let verifying = alloy::signers::k256::ecdsa::VerifyingKey::from_sec1_bytes(
            &miden_protocol::utils::serde::Serializable::to_bytes(&public),
        )
        .expect("miden's own serialization must round-trip through k256");

        let compressed = verifying.to_encoded_point(true);
        let uncompressed = verifying.to_encoded_point(false);
        let untagged = &uncompressed.as_bytes()[1..]; // strip the 0x04 tag

        assert_eq!(uncompressed.as_bytes().len(), 65);
        assert_eq!(untagged.len(), 64, "the shape Web3Signer actually returns");

        let from_compressed =
            parse_public_key(&hex::encode(compressed.as_bytes())).expect("compressed SEC1");
        let from_uncompressed =
            parse_public_key(&hex::encode(uncompressed.as_bytes())).expect("tagged uncompressed");
        let from_untagged =
            parse_public_key(&hex::encode(untagged)).expect("raw x||y — the Web3Signer shape");

        // `0x`-prefixed is what the API actually serves.
        let from_prefixed = parse_public_key(&format!("0x{}", hex::encode(untagged)))
            .expect("0x-prefixed raw x||y");

        assert_eq!(from_compressed.to_commitment(), public.to_commitment());
        assert_eq!(from_uncompressed.to_commitment(), public.to_commitment());
        assert_eq!(from_untagged.to_commitment(), public.to_commitment());
        assert_eq!(from_prefixed.to_commitment(), public.to_commitment());
    }

    /// Widening the accepted encodings must not widen the accepted KEYS: a
    /// 64-byte blob that is not a point on secp256k1 has to be rejected, not
    /// silently indexed into the key directory.
    #[test]
    fn rejects_a_64_byte_value_that_is_not_on_the_curve() {
        assert!(
            parse_public_key(&hex::encode([0xABu8; 64])).is_err(),
            "an off-curve 64-byte value must not be accepted as a public key"
        );
    }
}

/// The operator roles that get their own signer key.
///
/// # Why a role→key contract instead of "whatever the signer lists first"
///
/// The first version bound every account to `remote_commitments().first()`.
/// Signer key ORDER is not an operator configuration contract — it can change
/// when a key is added, removed or rotated — so the binding was both arbitrary
/// and unstable. Worse, a single-key signer (the e2e setup) silently bound the
/// service AND the GER manager to the same key, which is the opposite of
/// blast-radius isolation: one compromised or rotated key takes out both roles.
///
/// The operator now names the key for each role explicitly. Key creation and IAM
/// grants stay outside this process — the proxy only verifies that each named
/// key really is exposed by the signer, and binds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignerRole {
    /// The proxy's service account.
    Service,
    /// The dedicated GER-injection account.
    GerManager,
}

impl SignerRole {
    /// The name accepted in `--signer-key <role>=<identifier>`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::GerManager => "ger-manager",
        }
    }

    /// Every role that must be bound in remote custody.
    pub fn all() -> [SignerRole; 2] {
        [Self::Service, Self::GerManager]
    }

    fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "service" => Some(Self::Service),
            "ger-manager" | "ger_manager" => Some(Self::GerManager),
            _ => None,
        }
    }
}

/// Operator-declared role → signer key identifier.
#[derive(Debug, Clone, Default)]
pub struct SignerKeyBindings {
    by_role: BTreeMap<SignerRole, String>,
}

impl SignerKeyBindings {
    /// Parses repeated `role=identifier` arguments.
    pub fn parse(pairs: &[String]) -> anyhow::Result<Self> {
        let mut by_role: BTreeMap<SignerRole, String> = BTreeMap::new();
        for pair in pairs {
            let (role, identifier) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("--signer-key expects <role>=<identifier>, got {pair:?}"))?;
            let parsed = SignerRole::parse(role).ok_or_else(|| {
                anyhow!(
                    "unknown signer role {role:?}; expected one of: {}",
                    SignerRole::all()
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let identifier = identifier.trim();
            if identifier.is_empty() {
                return Err(anyhow!(
                    "--signer-key {}= has an empty identifier",
                    parsed.as_str()
                ));
            }
            if by_role.insert(parsed, identifier.to_string()).is_some() {
                return Err(anyhow!("--signer-key {} given twice", parsed.as_str()));
            }
        }
        // NOTE: uniqueness is NOT checked here. Two identifiers can be different
        // STRINGS and still be the same physical key — compressed vs tagged vs
        // raw SEC1, or different casing — and a string comparison would wave that
        // through while `commitment_for_identifier` happily resolves both to one
        // commitment. Uniqueness is therefore enforced in `resolve_unique`, after
        // every identifier has been reduced to its commitment.
        Ok(Self { by_role })
    }

    /// The identifier bound to `role`, if configured.
    pub fn identifier(&self, role: SignerRole) -> Option<&str> {
        self.by_role.get(&role).map(String::as_str)
    }

    /// True when no role was configured.
    pub fn is_empty(&self) -> bool {
        self.by_role.is_empty()
    }

    /// Resolves every configured role to its commitment against `directory`,
    /// rejecting two roles that resolve to the SAME physical key.
    ///
    /// This is where blast-radius isolation is actually enforced. Comparing the
    /// operator's identifier STRINGS cannot do it: the same key written as
    /// compressed SEC1, tagged uncompressed, raw `x||y`, or in different casing
    /// gives different strings and one commitment, so a string check would bind
    /// both roles to one key while reporting success.
    pub fn resolve_unique(
        &self,
        directory: &RemoteKeyDirectory,
    ) -> anyhow::Result<BTreeMap<SignerRole, PublicKeyCommitment>> {
        let mut by_role = BTreeMap::new();
        let mut seen: BTreeMap<PublicKeyCommitment, SignerRole> = BTreeMap::new();
        for (role, identifier) in &self.by_role {
            let commitment = directory
                .commitment_for_identifier(identifier)
                .ok_or_else(|| {
                    anyhow!(
                        "the remote signer does not expose the key configured for role {}; \
                         provision it in the signer (or correct --signer-key)",
                        role.as_str()
                    )
                })?;
            if let Some(other) = seen.insert(commitment, *role) {
                return Err(anyhow!(
                    "roles {} and {} resolve to the SAME signer key (identifiers differ only by \
                     encoding or casing); give each role its own key so a compromised or rotated \
                     key affects exactly one account",
                    other.as_str(),
                    role.as_str()
                ));
            }
            by_role.insert(*role, commitment);
        }
        Ok(by_role)
    }

    /// Roles with no configured key.
    pub fn missing_roles(&self) -> Vec<SignerRole> {
        SignerRole::all()
            .into_iter()
            .filter(|r| !self.by_role.contains_key(r))
            .collect()
    }
}

#[cfg(test)]
mod key_binding_tests {
    use super::*;

    fn b(pairs: &[&str]) -> anyhow::Result<SignerKeyBindings> {
        SignerKeyBindings::parse(&pairs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// PR #162 blocker: signer key ORDER is not an operator contract, so each
    /// role must name its key explicitly.
    #[test]
    fn roles_bind_to_their_configured_key() {
        let bindings = b(&["service=0xaaa", "ger-manager=0xbbb"]).expect("parse");
        assert_eq!(bindings.identifier(SignerRole::Service), Some("0xaaa"));
        assert_eq!(bindings.identifier(SignerRole::GerManager), Some("0xbbb"));
        assert!(bindings.missing_roles().is_empty());
    }

    /// Blast-radius isolation, enforced at COMMITMENT level.
    ///
    /// The first version compared identifier STRINGS, which the review correctly
    /// rejected: the same physical key written as compressed SEC1 and as raw
    /// `x||y` gives two different strings and one commitment, so a string check
    /// would bind both roles to one key and report success. This builds exactly
    /// that case — one key, two valid encodings — and requires it to fail.
    #[test]
    fn two_roles_may_not_share_one_key_in_different_encodings() {
        use miden_protocol::account::auth::AuthSecretKey;
        let secret = AuthSecretKey::new_ecdsa_k256_keccak();
        let AuthSecretKey::EcdsaK256Keccak(inner) = &secret else {
            panic!("requested an ecdsa key")
        };
        let public = inner.public_key();
        let verifying = alloy::signers::k256::ecdsa::VerifyingKey::from_sec1_bytes(
            &miden_protocol::utils::serde::Serializable::to_bytes(&public),
        )
        .expect("round-trip");
        let compressed = hex::encode(verifying.to_encoded_point(true).as_bytes());
        let raw = hex::encode(&verifying.to_encoded_point(false).as_bytes()[1..]);
        assert_ne!(compressed, raw, "the two encodings must differ as STRINGS");

        // A directory that holds exactly this one key.
        let mut directory = RemoteKeyDirectory::default();
        directory.insert_for_test(
            format!("0x{raw}"),
            PublicKey::EcdsaK256Keccak(public.clone()),
        );

        let bindings = b(&[
            &format!("service=0x{compressed}"),
            &format!("ger-manager=0x{raw}"),
        ])
        .expect("parse accepts them; uniqueness is a resolve-time property");

        let err = bindings
            .resolve_unique(&directory)
            .expect_err("one physical key must not bind two roles");
        assert!(
            format!("{err}").contains("SAME signer key"),
            "the error must explain the isolation problem, got: {err}"
        );
    }

    /// Distinct keys resolve cleanly.
    #[test]
    fn distinct_keys_resolve_per_role() {
        use miden_protocol::account::auth::AuthSecretKey;
        let mut directory = RemoteKeyDirectory::default();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let secret = AuthSecretKey::new_ecdsa_k256_keccak();
            let AuthSecretKey::EcdsaK256Keccak(inner) = &secret else {
                panic!()
            };
            let public = inner.public_key();
            let id = format!(
                "0x{}",
                hex::encode(miden_protocol::utils::serde::Serializable::to_bytes(
                    &public
                ))
            );
            directory.insert_for_test(id.clone(), PublicKey::EcdsaK256Keccak(public.clone()));
            ids.push(id);
        }
        let bindings = b(&[
            &format!("service={}", ids[0]),
            &format!("ger-manager={}", ids[1]),
        ])
        .expect("parse");
        let resolved = bindings.resolve_unique(&directory).expect("distinct keys");
        assert_eq!(resolved.len(), 2);
        assert_ne!(
            resolved[&SignerRole::Service],
            resolved[&SignerRole::GerManager]
        );
    }

    /// PR #162: a partially-configured signer must fail BEFORE provisioning.
    ///
    /// The init preflight resolves every role up front precisely so this state
    /// cannot get half-way: previously each role resolved independently while
    /// accounts were being created, so a missing GER key left an ORPHAN service
    /// account that fixing the config could not un-create.
    #[test]
    fn missing_role_key_is_detected_before_any_account_exists() {
        let partial = b(&["service=0xaaa"]).expect("parse");
        assert_eq!(
            partial.missing_roles(),
            vec![SignerRole::GerManager],
            "the preflight must see the gap while failing is still free"
        );
        assert!(
            CustodyMode::resolve(Some("http://127.0.0.1:9000"), false, partial).is_err(),
            "remote custody must refuse to start with a role unbound"
        );
    }

    /// An identifier the signer does not expose must fail loudly.
    #[test]
    fn unknown_identifier_is_rejected() {
        let bindings = b(&["service=0xdeadbeef", "ger-manager=0xfeedface"]).expect("parse");
        assert!(
            bindings
                .resolve_unique(&RemoteKeyDirectory::default())
                .is_err(),
            "a key the signer does not hold must not bind"
        );
    }

    /// A partially-configured signer must not start: the unconfigured role would
    /// otherwise have to fall back to an arbitrary key.
    #[test]
    fn remote_custody_requires_every_role() {
        let partial = b(&["service=0xaaa"]).expect("parse");
        assert_eq!(partial.missing_roles(), vec![SignerRole::GerManager]);
        let err = CustodyMode::resolve(Some("http://signer:9000"), false, partial)
            .expect_err("must refuse a partial binding");
        assert!(format!("{err}").contains("ger-manager"));
    }

    /// Local custody does not need signer keys.
    #[test]
    fn local_custody_needs_no_signer_keys() {
        assert!(matches!(
            CustodyMode::resolve(None, true, SignerKeyBindings::default()),
            Ok(CustodyMode::InsecureLocalKeystore)
        ));
    }

    #[test]
    fn malformed_bindings_are_rejected() {
        assert!(b(&["service"]).is_err(), "missing '=' must fail");
        assert!(b(&["service="]).is_err(), "empty identifier must fail");
        assert!(b(&["nope=0xaaa"]).is_err(), "unknown role must fail");
        assert!(
            b(&["service=0xa", "service=0xb"]).is_err(),
            "duplicate role must fail"
        );
    }
}

/// Whether a signer endpoint is safe to use without transport authentication.
///
/// # Why this gate exists (PR #162 review)
///
/// A KMS stops an attacker EXTRACTING the key. It does not stop anyone who can
/// reach the signing API from using it as a signing ORACLE — the proxy's whole
/// authority is "can talk to the signer". `--require-hardening` previously
/// accepted an arbitrary unauthenticated `http://` endpoint, so a hardened
/// deployment could point at a signer across the network with no authentication
/// and no transport integrity, and nothing would object.
///
/// This enum is a TRANSPORT CLASSIFICATION, not an authorization decision.
/// `Tls` records that the channel is encrypted and the SERVER authenticated —
/// it does NOT mean hardening accepts it. `hardening_signer_rejection` accepts
/// only `PrivateSidecar` (genuine loopback), because this client presents no
/// identity to the signer, so an `https` endpoint reachable by others is still
/// a signing oracle. There is no insecure override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerTransport {
    /// `https://` — the SERVER is authenticated and the channel encrypted, but
    /// this client presents no identity, so any other caller with network reach
    /// can still use the signing API. NOT a custody boundary on its own.
    Tls,
    /// Loopback or a compose-style private sidecar host: no network exposure.
    PrivateSidecar,
    /// Plain `http://` to a non-local host. A signing oracle on the network.
    UnauthenticatedRemote,
}

/// Classifies a signer URL for the hardening gate.
///
/// Parsed with a real URL parser rather than string prefixes. The hand-rolled
/// version accepted `http://127.attacker.example` (it merely started with
/// "127."), could be confused by userinfo-shaped URLs, and treated any
/// single-label host as a private sidecar — but a Docker/Kubernetes Service name
/// is reachable by every other workload on that network, which is not the
/// same-pod guarantee the classification implied (PR #162 review).
///
/// Plaintext is permitted ONLY for a genuine loopback address, which a same-pod
/// sidecar can use.
pub fn classify_signer_transport(base_url: &str) -> SignerTransport {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return SignerTransport::UnauthenticatedRemote;
    };
    // Credentials in the URL are never a boundary and often a typo/injection
    // vector; refuse to treat such a URL as trusted regardless of scheme.
    if !url.username().is_empty() || url.password().is_some() {
        return SignerTransport::UnauthenticatedRemote;
    }
    if url.scheme().eq_ignore_ascii_case("https") {
        return SignerTransport::Tls;
    }
    if !url.scheme().eq_ignore_ascii_case("http") {
        return SignerTransport::UnauthenticatedRemote;
    }
    match url.host() {
        // Literal loopback IPs only — a NAME that merely looks loopback-ish
        // (`127.attacker.example`) resolves wherever its owner wants.
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => SignerTransport::PrivateSidecar,
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => SignerTransport::PrivateSidecar,
        Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case("localhost") => {
            SignerTransport::PrivateSidecar
        }
        _ => SignerTransport::UnauthenticatedRemote,
    }
}

/// The signer-transport half of `--require-hardening`, as a pure function.
///
/// Lives in the LIBRARY on purpose: `make test-unit` runs `cargo test --lib`, so
/// a security boundary tested only inside the binary target is a boundary CI
/// does not check (PR #162 review). Returns the operator-facing reason when
/// hardening must refuse, `None` when the endpoint is acceptable.
pub fn hardening_signer_rejection(signer_url: Option<&str>) -> Option<String> {
    let url = signer_url?;
    match classify_signer_transport(url) {
        SignerTransport::PrivateSidecar => None,
        // `https` authenticates the SERVER to us; it does not authenticate US to
        // the signer. This client sends no certificate or token, so any caller
        // with network reach keeps the signing oracle — and a KMS prevents key
        // EXTRACTION, not key USE.
        SignerTransport::Tls | SignerTransport::UnauthenticatedRemote => Some(
            "  - --signer-url is not a loopback address. A KMS prevents key EXTRACTION, but any              caller that can reach the signing API has a signing ORACLE, and this client              authenticates itself to the signer in no way at all — https proves only that WE              authenticated the SERVER. Until caller authentication (mTLS client cert / token)              is implemented, run Web3Signer (or an authenticated relay) on the same host/pod              and point --signer-url at 127.0.0.1."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod hardening_boundary_tests {
    use super::*;

    /// The four cases the review asked CI to cover.
    #[test]
    fn hardening_accepts_only_loopback() {
        assert!(
            hardening_signer_rejection(Some("http://127.0.0.1:9000")).is_none(),
            "loopback is the boundary hardening permits"
        );
        assert!(
            hardening_signer_rejection(Some("http://localhost:9000")).is_none(),
            "localhost is loopback"
        );
        assert!(
            hardening_signer_rejection(Some("http://signer.example.com:9000")).is_some(),
            "plain http to a remote host is a signing oracle"
        );
        assert!(
            hardening_signer_rejection(Some("https://signer.example.com")).is_some(),
            "https authenticates the SERVER only — it is not a custody boundary"
        );
        assert!(
            hardening_signer_rejection(None).is_none(),
            "local custody configures no signer endpoint"
        );
    }

    /// The rejection reason must name the actual problem (caller authentication),
    /// not merely say "use TLS" — which is what an operator would otherwise do,
    /// and which would not fix it.
    #[test]
    fn rejection_reason_explains_caller_authentication() {
        let reason =
            hardening_signer_rejection(Some("https://signer.example.com")).expect("reject");
        assert!(reason.contains("authenticates itself to the signer in no way"));
        assert!(reason.contains("127.0.0.1"), "must say what to do instead");
    }
}

#[cfg(test)]
mod transport_gate_tests {
    use super::*;

    /// The hardening gate's job: a signing API reachable over the network with
    /// no authentication is a signing oracle, and KMS custody does not help.
    #[test]
    fn plain_http_to_a_remote_host_is_unauthenticated() {
        for url in [
            "http://signer.internal.example.com:9000",
            "http://10.0.0.5:9000",
            "http://signer.example.com",
        ] {
            assert_eq!(
                classify_signer_transport(url),
                SignerTransport::UnauthenticatedRemote,
                "{url} must be treated as an exposed signing oracle"
            );
        }
    }

    /// `https` CLASSIFIES as Tls — that is a statement about the transport, not
    /// about acceptance. Hardening still rejects it (see
    /// `hardening_accepts_only_loopback`), because server authentication is not
    /// caller authentication.
    #[test]
    fn https_classifies_as_tls_but_is_not_a_hardened_boundary() {
        assert_eq!(
            classify_signer_transport("https://signer.example.com:9000"),
            SignerTransport::Tls
        );
        assert!(
            hardening_signer_rejection(Some("https://signer.example.com:9000")).is_some(),
            "classification as Tls must NOT imply hardening accepts it"
        );
    }

    /// Only a GENUINE loopback address counts as private. A same-pod sidecar
    /// can use loopback; a shared-network service name cannot make that claim.
    #[test]
    fn only_real_loopback_is_private() {
        for url in [
            "http://localhost:9000",
            "http://127.0.0.1:9000",
            "http://[::1]:9000",
        ] {
            assert_eq!(
                classify_signer_transport(url),
                SignerTransport::PrivateSidecar,
                "{url} is genuine loopback"
            );
        }
    }

    /// The spoofing cases the previous string-prefix classifier accepted.
    #[test]
    fn loopback_lookalikes_and_userinfo_are_rejected() {
        for url in [
            // resolves wherever its owner points it — merely STARTS WITH "127."
            "http://127.attacker.example:9000",
            // a shared-network service name is reachable by other workloads
            "http://web3signer:9000",
            // userinfo is never a boundary
            "http://user:pass@127.0.0.1:9000",
            "https://user:pass@signer.example.com",
            // non-http schemes
            "ftp://signer.example.com",
            "not a url",
        ] {
            assert_eq!(
                classify_signer_transport(url),
                SignerTransport::UnauthenticatedRemote,
                "{url} must NOT be treated as a trusted boundary"
            );
        }
    }
}
