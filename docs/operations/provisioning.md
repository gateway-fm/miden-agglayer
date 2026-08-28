# Provisioning

How to stand up a deployment of this service, with emphasis on key custody:
where account keys live, who creates them, and the order that validates key
listing, binding, and signing authority before account creation. Serving
startup rechecks key exposure and account bindings; it is not itself a signing
canary.

Like the rest of `docs/operations`, this document is deployment-neutral.
Resolve `$NAMESPACE`, `$WORKLOAD`, `$SIGNER_URL`, `$DATABASE_URL` and secret
names from the deployment you are operating.

## The custody map

The following secret classes are relevant to this provisioning boundary. This
is not a complete inventory of every deployment secret; in particular, the
signer's cloud identity and the proxy's database credentials have different
owners and must not be conflated with the account keys:

| Secret | Owner | Provisioned by | Held by the proxy? |
|---|---|---|---|
| Miden **account** keys (`service`, `ger-manager`) | KMS / HSM / vault | your key-management process, **before** first boot | No — never, in remote custody |
| Signer cloud credentials / workload identity | signer workload | deploy and IAM pipelines | No — never supplied to the proxy |
| `DATABASE_URL` credentials | deployment secret store | database/deploy pipeline | Yes, as the proxy's database connection secret |
| `--admin-api-key` | deployment secret store | deploy pipeline | Yes, in memory (compared constant-time) |
| `--miden-api-key` | node gateway operator | node operator | Yes, in memory (redacted in logs) |
| L1/aggkit EVM keys (aggoracle, aggsender, claimtxman) | those components' own config | the aggkit/bridge deployment | No — not this service's concern |

The first two rows make up the remote-signing boundary. The proxy receives only
the public key identifiers and the signer's loopback URL; cloud credentials
belong to the signer workload. The remaining rows are separate deployment
secrets.

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
| Mode | Web3Signer **`eth1`**, the execution-layer secp256k1 API; the `eth2`/BLS API is incompatible |
| Response | Plain `0x`-prefixed text encoding 65 bytes, `r (32) ‖ s (32) ‖ v (1)`; the client also tolerates optional JSON quotes, and normalises Ethereum-style `v` to Miden's recovery id |
| Request timeout | `AGGLAYER_SIGNER_TIMEOUT_SECS`, default `30` |

**Digest compatibility** is the load-bearing detail. Miden's `EcdsaK256Keccak`
signs `keccak256(commitment_word_bytes)`. Web3Signer's eth1 endpoint computes
`keccak256(data)` over whatever you post. The proxy therefore posts the raw
32-byte commitment word as `data`, making the two digest constructions
byte-identical — no extra application hash, message prefix, or EIP-191 wrapper.
Unit tests pin the `r ‖ s ‖ v` decoder against a local signature; the
`e2e-web3signer.sh` integration test is what exercises an actual Web3Signer.

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

The proxy refuses to start if a role is unbound, if two roles resolve to the
same physical public key (even through different encodings), or if the signer
does not expose a named key. One key per role is deliberate: a compromised key
then affects exactly one account instead of both. Key creation and IAM grants
stay **outside** this process — the proxy never asks a signer to create a key.

## Provisioning order

The order matters. Keys exist before accounts, because an account is created
*bound to* a key that must already be in the vault.

**1. Provision the durable stores and validate every non-custody startup
input.** Before a boot that can initialize accounts:

- provision PostgreSQL and set `DATABASE_URL`; the binary must include the
  `postgres` feature and the database identity must be able to run the embedded
  migrations under an advisory lock;
- mount one persistent, exclusive `--miden-store-dir` for `store.sqlite3`, its
  WAL/SHM files, `keystore/`, and `bridge_accounts.toml`; and
- when using `--require-hardening`, validate the strict-H6 inputs described in
  [Production checklist](#production-checklist), including the initial L1
  indexer start block and catch-up budget.

The service refuses to serve with the in-memory store unless
`ALLOW_EPHEMERAL_STORE=1` explicitly accepts losing acknowledged future-nonce
transactions on restart. That override is for development and tests only;
never set it in production.

Do these checks first. Account initialization happens before the serving-store
and strict-H6 database gates, so a misconfigured first start can create Miden
account identities and write `bridge_accounts.toml` before aborting later in
startup.

**2. Create the account keys in your KMS/vault.** Create two distinct
secp256k1 signing keys, one per role. Grant only the backend-specific read/public
key and signing operations described below; never grant private-key export or
key administration to the signer workload.

**3. Configure a dedicated signer to expose those two keys.** Use one
key-config entry per key and supply cloud credentials only to the signer
workload. The proxy receives no cloud credential. See
[Signer backends](#signer-backends) below.

**4. Verify both keys, exercise signing authority, and validate the response
shape before pointing the proxy at the signer.** Set the expected identifiers
to the exact public-key strings from the authenticated DevOps handoff, run this
from the same network namespace the proxy will use, and then use those exact
strings in `AGGLAYER_SIGNER_KEYS`:

```bash
set -Eeuo pipefail
: "${SIGNER_URL:?}"
: "${EXPECTED_SERVICE_KEY:?}"
: "${EXPECTED_GER_MANAGER_KEY:?}"

curl -fsS "$SIGNER_URL/upcheck" >/dev/null
signer_keys="$(curl -fsS "$SIGNER_URL/api/v1/eth1/publicKeys")"
jq -e \
  --arg service "$EXPECTED_SERVICE_KEY" \
  --arg ger "$EXPECTED_GER_MANAGER_KEY" '
    type == "array" and length == 2 and
    $service != $ger and
    index($service) != null and index($ger) != null
  ' <<<"$signer_keys" >/dev/null

sign_test_data="0x$(printf '%064d' 0)"
for identifier in "$EXPECTED_SERVICE_KEY" "$EXPECTED_GER_MANAGER_KEY"; do
  signature="$(
    curl -fsS \
      -H 'content-type: application/json' \
      --data "$(jq -nc --arg data "$sign_test_data" '{data:$data}')" \
      "$SIGNER_URL/api/v1/eth1/sign/$identifier"
  )"
  signature="${signature#\"}"
  signature="${signature%\"}"
  [[ "$signature" =~ ^0x[0-9a-fA-F]{130}$ ]]
done
```

> A signer with an unreadable or empty key directory reports `/upcheck` as **UP**
> while serving zero keys. `/upcheck` alone, or even a non-empty key list, does
> not prove that both role keys are present or that the backend grants signing.
> Require the exact two-key match and both sign-endpoint calls before any
> account creation. Retain the provider-side audit events proving that both KMS
> keys served them. The shell check proves authorization and the 65-byte wire
> shape; it does **not** cryptographically validate the returned signatures or
> digest compatibility. A real signed Miden operation and the Web3Signer e2e
> test provide that proof.

**5. First boot of the proxy** with `--signer-url` and both `--signer-key`
bindings. When `bridge_accounts.toml` is absent (or `--init` is explicitly
used), this creates the Miden accounts, each bound to the remote public key for
its role; no account secret is generated locally. Account ids are persisted to
`bridge_accounts.toml` in the Miden store directory. `--init` forces this
identity-creation path and exits; never use it against an existing deployment.

**6. Every subsequent boot re-verifies the bindings** (see below). Steady-state
restarts do not re-create anything.

## Startup verification (what "healthy" means)

On every serving startup — not just the boot that created the accounts — the
proxy, for each account role present in `bridge_accounts.toml`:

1. reads the account record's auth key from its `AuthSingleSig` storage slot in
   the local Miden store, importing the public account by id from the node only
   when that local record is absent,
2. requires it to equal the key `--signer-key` names for that role, and
3. requires the signer to **still hold** that key.

All three must agree or the process aborts. The persisted or imported account
record is the source of truth on purpose: inserting the configured commitment
and then checking it would only prove the config agrees with itself. A newly
created service account can still exist only in the local client store at this
point; the check must not be described as an unconditional fresh on-chain read.

This is a key-list and binding check, not a startup signing canary. For example,
an AWS identity with `kms:GetPublicKey` but no `kms:Sign` can pass startup and
then fail on the first transaction. That is why the provisioning preflight
above invokes the signing endpoint for each key, and why deployment acceptance
still requires a real signed operation plus runtime signature counters.

Success is published as a gauge rather than a log line, because log field
rendering depends on the subscriber's formatter:

```
remote_signer_verified_accounts   # account roles verified (expect 2 when newly provisioned)
remote_signer_signatures_total
remote_signer_signature_failures_total
```

`remote_signer_verified_accounts` is set during startup; it is not a continuous
signer probe. Require it to equal `2` for a newly provisioned deployment, and
alert if the process/target is absent as well as if that gauge is wrong. During
runtime, alert on signature failures and on an expected workload producing no
increase in `remote_signer_signatures_total`.

## Signer backends

The proxy is unaware of the signer's storage backend, but that does **not** make
backend or key changes transparent. Existing Miden accounts remain bound to the
same public keys. A backend change is proxy-neutral only if it preserves those
exact keys and the loopback HTTP contract; moving away from a non-exportable
KMS key normally requires the unsupported account-migration procedure described
under [Rotation](#rotation). Backend credentials, IAM and signer configuration
also differ even though the proxy flags do not.

Development (raw file, what the e2e suite generates):

```yaml
type: file-raw
keyType: SECP256K1
privateKey: "0x<32-byte hex>"
```

AWS KMS:

```yaml
type: aws-kms
authenticationMode: ENVIRONMENT
region: eu-west-1
kmsKeyId: arn:aws:kms:eu-west-1:<account>:key/<uuid>   # kmsKeyId, not keyId
```

Create each AWS key as `ECC_SECG_P256K1`, `SIGN_VERIFY`, and verify it is
enabled. `authenticationMode: ENVIRONMENT` uses the AWS default credential
provider chain; deliver a short-lived workload identity to Web3Signer rather
than embedding `accessKeyId`, `secretAccessKey`, or `sessionToken` in the YAML.
Restrict the signer identity to the two key ARNs using separate IAM statements:
allow `kms:GetPublicKey` without signing-request conditions, and allow
`kms:Sign` with `kms:SigningAlgorithm=ECDSA_SHA_256` and
`kms:MessageType=DIGEST`. Do not put those conditions on the `GetPublicKey`
statement: that request supplies neither field and would be denied. Web3Signer
needs both operations, but startup key loading exercises only `GetPublicKey`;
`kms:Sign` is proved only by a real signing request.

For Azure Key Vault, `type: azure-key` performs signing inside Key Vault and is
the non-export custody option. `type: azure-secret` instead downloads the key
secret into Web3Signer. HashiCorp's `type: hashicorp` similarly reads private
key material from the KV store. Those latter modes still keep secrets out of
the proxy, but they do **not** provide the same non-export guarantee as AWS KMS
or `azure-key`, and their signer identities require secret-read permission
rather than only a signing operation. Follow Web3Signer's version-matched
[key configuration reference](https://docs.web3signer.consensys.io/reference/key-config-file-params).

Two operational notes that apply to the vault-backed setups:

- The identifier you put in `AGGLAYER_SIGNER_KEYS` is the **public key** the
  signer publishes, not the KMS key id or ARN. Read it back from
  `/api/v1/eth1/publicKeys` after configuring the backend.
- A non-export KMS backend prevents key **extraction**. It does not prevent key
  **use** — see the next section.

## The loopback rule

`--require-hardening` accepts only a plaintext HTTP URL whose host is a literal
loopback address (`127.0.0.0/8` or `::1`) or exactly `localhost`. It rejects
service DNS names, non-loopback hosts, URL credentials, other schemes, and even
a remote HTTPS signer.

This is not a formality. The proxy authenticates itself to the signer in no way
at all: no client certificate, no token. `https` proves that *we* authenticated
the *server*, not the reverse. So any caller with network reach to the signing
API holds a signing oracle, and a KMS does not help — it stops key extraction,
not key use.

Until caller authentication (mTLS client certificate or token) is implemented,
run the signer in the proxy's **same network namespace**—for example, as a
sidecar in the same Kubernetes pod—and point `--signer-url` at
`http://127.0.0.1:<port>`. Merely placing two containers on the same host is not
enough: each container normally has its own loopback namespace. If the signer
must run elsewhere, put an authenticated relay beside the proxy and point the
proxy at the relay's loopback listener.

The signer or relay must also **listen only on loopback** in that namespace. For
the pinned Web3Signer this means its version-matched equivalent of
`--http-listen-host=127.0.0.1`; do not publish the signing port with a host-port
mapping, Kubernetes Service, or Ingress. Verify the bound socket and confirm an
adjacent workload cannot connect. `--require-hardening` inspects only the URL
configured on the proxy—it cannot prove how the signer listener is bound or
exposed.

## Rotation

There is **no supported in-place key rotation or automated replacement-account
workflow** in the current service. An account's persisted/imported auth slot
remains the source of truth; changing a role binding to another key makes the
next startup fail its account-versus-configured check.

Removing a key while the process is already running is different: the signer
key directory is a startup snapshot, so live transactions begin failing at the
signing HTTP call and increment `remote_signer_signature_failures_total`. The
next startup then fails because the configured role key is no longer exposed.

Do not edit a binding, remove the old key, or run `--init` as a rotation
shortcut. Preserve the old key. Rotation requires an explicitly designed and
tested migration that deploys the replacement account, transfers every
affected on-chain bridge role and operational dependency, updates the account
configuration, and proves the old account is drained before retiring its key.
That migration is outside the current CLI and must be treated as a bespoke
change/incident procedure.

## Production checklist

`--require-hardening` (`REQUIRE_HARDENING=true`) makes the following custody and
request-policy predicates startup requirements. Deployments should set it.
Some defaults are already fail-closed rather than exposed: without an admin key
the admin methods are disabled, and without an allowed-signer list every signed
submission is rejected. Hardening requires an explicit production-complete
configuration instead of accepting those disabled defaults.

| Requirement | Why |
|---|---|
| `--signer-url` is accepted loopback HTTP | the client does not authenticate itself to the signer |
| `--insecure-local-keystore` unset | the proxy must not generate or persist account secrets in its own keystore |
| `--admin-api-key` set | hardening requires authenticated admin operations; without it they are disabled |
| non-empty `--allowed-signers` | hardening requires an explicit submitter policy; without it all signers are rejected |
| `--insecure-allow-any-signer` unset | legacy open mode, incompatible with the allow-list |
| `--cors-allowed-origins` has no `*` | any browser origin could reach mutating endpoints |
| `--miden-prover-url` set | the in-process prover is the documented OOM cause; also probed for reachability at startup |

This gate proves the proxy selected remote custody; it does not attest the
signer's backend. A loopback Web3Signer using `file-raw` keys can satisfy
hardening while keeping private material on the same host. Prove non-export KMS
custody separately from the signer config, IAM policy, two-key preflight, and
provider audit evidence.

Setting it also implies **strict H6**
(`--reject-unverified-ger-injection`). Current startup therefore additionally
requires all of the following:

- both `L1_RPC_URL` and `GER_L1_ADDRESS`, with a usable HTTP(S) RPC URL and a
  syntactically valid EVM contract address;
- `L1_EVIDENCE_TAG=safe` or `finalized`; `latest` may include reorgable L1
  blocks and is rejected under hardening;
- on a fresh PostgreSQL database, `L1_INDEXER_FROM_BLOCK` set at or before the
  rollup deployment as a one-boot initial-backfill override; remove it after
  successful catch-up because it outranks the persisted cursor and otherwise
  rewinds that range on every restart; an existing database may instead supply
  its non-zero, policy-matched evidence cursor; and
- synchronous L1 evidence catch-up before the JSON-RPC listener serves. The
  default ceiling is 300 seconds; size
  `L1_EVIDENCE_CATCHUP_BUDGET_SECS` for the initial historical range. Startup
  aborts rather than serving if the evidence index cannot converge in time.

These H6 checks are part of the effective hardened startup contract even though
they are evaluated separately from the core flag checklist.

Independently of hardening, production serving requires `DATABASE_URL`; never
use `ALLOW_EPHEMERAL_STORE=1`. Never expose the JSON-RPC port directly to the
public internet, run two service processes against one Miden store, or delete
`keystore/` or `bridge_accounts.toml` as part of routine recovery.

## Verifying a provisioned deployment

The e2e suite exercises the same proxy → HTTP → Web3Signer protocol and its
negative controls, using `file-raw` keys. It does **not** validate production
network topology, cloud credentials, IAM, or KMS audit evidence.

The script destroys its Compose volumes and fixture data. Run it only against a
disposable, isolated e2e project—never against a provisioned deployment:

```bash
./scripts/e2e-web3signer.sh
```

It asserts that the proxy holds no local secret, that a full L1→L2 deposit is
signed remotely, that signing genuinely depends on the signer (stopping the
signer must break signing, and restarting it must resume it), that a proxy
restart re-verifies both role bindings against the persisted account records,
and that a role pointed at the wrong key is refused at startup and that the
refusal is not sticky.

Against a real deployment, these are useful basic spot checks:

```bash
curl -fsS "$SIGNER_URL/api/v1/eth1/publicKeys" | jq .
expected_accounts="${EXPECTED_ACCOUNT_ROLE_COUNT:-2}"
verified="$(
  curl -fsS "$PROXY_RPC/metrics" |
    awk '$1 == "remote_signer_verified_accounts" {print $2}'
)"
test "${verified%.*}" -eq "$expected_accounts"
```

They are not equivalent to the e2e test and do not prove KMS signing authority.
Set `EXPECTED_ACCOUNT_ROLE_COUNT` to the roles present in
`bridge_accounts.toml`: `2` for a new deployment, or potentially `1` for a
supported legacy file without `ger_manager`. For deployment acceptance, repeat
the two-key signing-authority/response-shape preflight, execute a real signed
operation, prove `remote_signer_signatures_total` increased without a
failure-counter increase, and retain provider-side evidence that both expected
KMS keys served the signing calls.

## Related

- [Runbook](runbook.md) — startup, shutdown, recovery procedures
- [Monitoring](monitoring.md) — metrics and alerting
- [Diagnostics](diagnostics.md) — symptom-to-cause investigation
- [Upgrade guide](../UPGRADE.md) — in-place upgrade and rollback
