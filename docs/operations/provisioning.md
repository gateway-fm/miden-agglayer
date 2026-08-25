# Provisioning

How to stand up a deployment of this service, with emphasis on key custody:
where account keys live, who creates them, and the order operations must happen
in so a broken custody setup fails at startup rather than mid-transaction.

Like the rest of `docs/operations`, this document is deployment-neutral.
Resolve `$NAMESPACE`, `$WORKLOAD`, `$SIGNER_URL`, `$DATABASE_URL` and secret
names from the deployment you are operating.

## The custody map

Four distinct kinds of secret exist in a deployment. They are provisioned by
different parties and must not be conflated:

| Secret | Owner | Provisioned by | Held by the proxy? |
|---|---|---|---|
| Miden **account** keys (`service`, `ger-manager`) | KMS / HSM / vault | your key-management process, **before** first boot | No — never, in remote custody |
| `--admin-api-key` | deployment secret store | deploy pipeline | Yes, in memory (compared constant-time) |
| `--miden-api-key` | node gateway operator | node operator | Yes, in memory (redacted in logs) |
| L1/aggkit EVM keys (aggoracle, aggsender, claimtxman) | those components' own config | the aggkit/bridge deployment | No — not this service's concern |

Only the first row is what "remote signing" and "KMS" refer to. This document
covers that row; the others are ordinary deployment secrets.

## Two custody modes

Custody is resolved once per process at startup and is mutually exclusive.

**Remote signer (production).** `--signer-url` / `AGGLAYER_SIGNER_URL` points at
a Web3Signer-compatible service. Every account signature is produced by that
service; the proxy generates, holds and stores no account secret. There is
deliberately **no per-account fallback** — a key the signer does not hold is a
hard error, never a silent local signature.

**Local keystore (development and the e2e suite only).** `--insecure-local-keystore`
/ `AGGLAYER_INSECURE_LOCAL_KEYSTORE=true` keeps account private keys on the
host's disk. Refused by `--require-hardening`. Setting both this and
`--signer-url` is refused at startup.

One of the two must be chosen explicitly. There is no implicit default that
silently writes a private key to disk.

## What the proxy requires of the signer

The proxy speaks a deliberately small HTTP contract, so any Web3Signer-compatible
service works and no cloud-vendor SDK is linked into this binary:

| | |
|---|---|
| List keys | `GET {signer}/api/v1/eth1/publicKeys` |
| Sign | `POST {signer}/api/v1/eth1/sign/{identifier}` with `{"data": "0x…"}` |
| Mode | Web3Signer **`eth1`** (secp256k1). `eth2` loads keys into memory and defeats vault custody |
| Response | 65 bytes, `r (32) ‖ s (32) ‖ v (1)`; `v` is normalised from the 27/28 encoding |
| Request timeout | `AGGLAYER_SIGNER_TIMEOUT_SECS`, default `30` |

**Digest compatibility** is the load-bearing detail. Miden's `EcdsaK256Keccak`
signs `keccak256(commitment_word_bytes)`. Web3Signer's eth1 endpoint computes
`keccak256(data)` over whatever you post. The proxy therefore posts the raw
32-byte commitment word as `data`, making the two digest constructions
byte-identical — no re-hashing, no message prefix, no EIP-191 wrapper. A unit
test pins remote signatures against a locally-computed one, so drift on either
side breaks CI rather than on-chain verification.

**Key identifiers** may be given in any encoding the signer publishes: 33-byte
compressed SEC1, 65-byte tagged uncompressed (`0x04 ‖ x ‖ y`), or the 64-byte
raw uncompressed point Ethereum tooling uses. All are normalised to compressed
internally. This widens accepted *encodings*, not accepted *keys* — a value that
is not a point on secp256k1 is rejected.

## Roles: one key each

Two roles exist, and each binds to its own key:

| Role | Flag spelling | Account |
|---|---|---|
| `service` | `--signer-key service=<identifier>` | the service account |
| `ger-manager` | `--signer-key ger-manager=<identifier>` (`ger_manager` also accepted) | the dedicated GER-injection account |

Supply them via repeated `--signer-key` flags or a comma-separated
`AGGLAYER_SIGNER_KEYS`:

```
AGGLAYER_SIGNER_KEYS=service=0x<pubkey>,ger-manager=0x<pubkey>
```

The proxy refuses to start if a role is unbound, if two roles name the same
identifier, or if the signer does not actually expose a named key. One key per
role is deliberate: a compromised or rotated key then takes out exactly one
account instead of both. Key creation and IAM grants stay **outside** this
process — the proxy never asks a signer to create a key.

## Provisioning order

The order matters. Keys exist before accounts, because an account is created
*bound to* a key that must already be in the vault.

**1. Create the account keys in your KMS/vault.** Two secp256k1 keys, one per
role. Grant the signer's identity permission to *use* them for signing, not to
export them.

**2. Configure the signer to expose exactly those keys.** One key-config entry
per key. This is the only file that differs between environments — see
[KMS backends](#kms-backends) below.

**3. Verify the signer serves the keys before pointing the proxy at it:**

```bash
curl -sf "$SIGNER_URL/upcheck"
curl -sf "$SIGNER_URL/api/v1/eth1/publicKeys"
```

> A signer with an unreadable or empty key directory reports `/upcheck` as **UP**
> while serving zero keys. Health-check on a **non-empty key list**, not on
> `/upcheck` alone, or a broken custody setup looks exactly like a working
> signer and only surfaces later as a confusing proxy boot error.

**4. First boot of the proxy** with `--signer-url` and both `--signer-key`
bindings. On a fresh store this creates the Miden accounts, each bound to the
remote public key for its role; no secret is generated locally. Account ids are
persisted to `bridge_accounts.toml` in the store directory.

**5. Every subsequent boot re-verifies the bindings** (see below). Steady-state
restarts do not re-create anything.

## Startup verification (what "healthy" means)

On every serving startup — not just the boot that created the accounts — the
proxy, for each role:

1. reads the **deployed on-chain account's** auth key out of its `AuthSingleSig`
   storage slot (importing the account by id if the local store is cold),
2. requires it to equal the key `--signer-key` names for that role, and
3. requires the signer to **still hold** that key.

All three must agree or the process aborts. The deployed account is the source
of truth on purpose: inserting the configured commitment and then checking it
would only prove the config agrees with itself.

Success is published as a gauge rather than a log line, because log field
rendering depends on the subscriber's formatter:

```
remote_signer_verified_accounts   # = number of roles verified (expect 2)
remote_signer_signatures_total
remote_signer_signature_failures_total
```

Alert on `remote_signer_verified_accounts` dropping below the configured role
count, and on any sustained rate of `remote_signer_signature_failures_total`.

## KMS backends

The proxy is unaware of which vault backs the signer. Swapping backends is a
change to the signer's key-config file and nothing else — the proxy flags, the
network topology and this document's procedures are unchanged.

Development (raw file, what the e2e suite generates):

```yaml
type: file-raw
keyType: SECP256K1
privateKey: "0x<32-byte hex>"
```

AWS KMS:

```yaml
type: aws-kms
region: eu-west-1
kmsKeyId: arn:aws:kms:eu-west-1:<account>:key/<uuid>   # kmsKeyId, not keyId
```

Azure Key Vault and HashiCorp Vault are configured with their respective
`type: azure` / `type: hashicorp` entries per Web3Signer's documentation. In all
cases the signer's identity needs **sign** permission only; it must not need
export.

Two operational notes that apply to the vault-backed setups:

- The identifier you put in `AGGLAYER_SIGNER_KEYS` is the **public key** the
  signer publishes, not the KMS key id or ARN. Read it back from
  `/api/v1/eth1/publicKeys` after configuring the backend.
- A KMS prevents key **extraction**. It does not prevent key **use** — see the
  next section.

## The loopback rule

`--require-hardening` **refuses a non-loopback `--signer-url`.**

This is not a formality. The proxy authenticates itself to the signer in no way
at all: no client certificate, no token. `https` proves that *we* authenticated
the *server*, not the reverse. So any caller with network reach to the signing
API holds a signing oracle, and a KMS does not help — it stops key extraction,
not key use.

Until caller authentication (mTLS client certificate or token) is implemented,
run the signer as a **sidecar on the same host/pod** and point `--signer-url` at
`127.0.0.1`. If you must place the signer elsewhere, put an authenticated relay
in front of it and point the proxy at the relay's loopback address.

## Rotation

There is **no in-place key rotation** for an existing account. An account's auth
key is fixed in its on-chain storage slot at creation; changing which key a role
binds to makes startup verification fail with "the deployed *role* account … is
signed for by a DIFFERENT key than `--signer-key` configures for that role."

That failure is correct behaviour — the deployment genuinely cannot sign for
that account any more. Removing the old key from the signer produces the same
abort from the other direction ("the remote signer does not hold the key(s)
bound to *n* configured account(s)").

So plan rotation as **provisioning a new account**, not as editing a binding.
Keep the old key available in the signer until the replacement account is
deployed and drained. Treat "rotate the ger-manager key" as a change/incident
procedure with a migration plan, not a config edit.

## Production checklist

`--require-hardening` (`REQUIRE_HARDENING=true`) turns each of the following
from a silent runtime exposure into a startup failure. Deployments should set
it, and the list below is exactly what it enforces:

| Requirement | Why |
|---|---|
| `--signer-url` is loopback | the client does not authenticate itself to the signer |
| `--insecure-local-keystore` unset | account keys must not be on the host's disk |
| `--admin-api-key` set | `admin_*` methods would otherwise be open |
| `--allowed-signers` set | `eth_sendRawTransaction` fail-closed allow-list |
| `--insecure-allow-any-signer` unset | legacy open mode, incompatible with the allow-list |
| `--cors-allowed-origins` has no `*` | any browser origin could reach mutating endpoints |
| `--miden-prover-url` set | the in-process prover is the documented OOM cause; also probed for reachability at startup |

Setting it additionally implies **strict H6** (`--reject-unverified-ger-injection`),
which refuses any GER not corroborated by the independent L1 InfoTree indexer,
and requires `--l1-evidence-tag=safe` or `finalized` — `latest` may include
reorgable L1 blocks and is not sufficient to authorize a GER.

Beyond the flags: never expose the JSON-RPC port directly to the public
internet, never run two service processes against one Miden store, and never
delete `keystore/` or `bridge_accounts.toml` as part of a routine recovery.

## Verifying a provisioned deployment

The e2e suite exercises the same code path a KMS-backed deployment uses — proxy
→ HTTP → signer → custody backend — differing only in the signer's key-config
file:

```bash
./scripts/e2e-web3signer.sh
```

It asserts that the proxy holds no local secret, that a full L1→L2 deposit is
signed remotely, that signing genuinely depends on the signer (stopping the
signer must break signing, and restarting it must resume it), that a proxy
restart re-verifies both role bindings against the deployed accounts, and that a
role pointed at the wrong key is refused at startup and that the refusal is not
sticky.

Against a real deployment, the equivalent read-only checks are:

```bash
curl -sf "$SIGNER_URL/api/v1/eth1/publicKeys"          # signer serves both keys
curl -sf "$PROXY_RPC/metrics" | grep remote_signer_    # verified_accounts == role count
```

## Related

- [Runbook](runbook.md) — startup, shutdown, recovery procedures
- [Monitoring](monitoring.md) — metrics and alerting
- [Diagnostics](diagnostics.md) — symptom-to-cause investigation
- [Upgrade guide](../UPGRADE.md) — in-place upgrade and rollback
