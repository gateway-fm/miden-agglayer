# Operations runbook

This runbook covers the service on the current `main` branch. Deployment object
names and secret locations are intentionally discovered from the live platform;
the repository does not contain a canonical production Kubernetes manifest.

Start with [diagnostics](diagnostics.md), preserve evidence, and choose the
narrowest recovery whose preconditions are proven.

## 1. Non-negotiable runtime constraints

### One owner

Run one `miden-agglayer-service` process per Miden store and synthetic Postgres
store. The miden-client sqlite has a single in-process owner guard, and the
`SyntheticProjector` is the single live producer/owner of synthetic history.
Horizontal replicas sharing state are unsupported.

### Persistent identity

Persist the complete `--miden-store-dir`, including:

- `store.sqlite3` and its WAL/SHM files;
- `keystore/`;
- `bridge_accounts.toml`.

The keystore and account config control existing on-chain accounts. Losing or
replacing them is not ordinary cache loss. Back up/restore them as one unit and
never mount the same directory into two live service processes.

Set `MIDEN_STORE_BASE` when the store path comes from templated/untrusted input;
the service then enforces containment after symlink resolution.

### Durable synthetic store

Set `DATABASE_URL` in production. A serving start without it is now refused.
`ALLOW_EPHEMERAL_STORE=1` is an explicit development/test escape hatch that
selects `InMemoryStore` and loses synthetic logs, receipts, cursors, faucet
routes, admission state, and acknowledged future-nonce transactions at
restart. Never set it in production.

Postgres migrations are embedded into the binary and run automatically under
an advisory lock before the pool opens. A previously applied file whose
checksum differs aborts startup. Do not use a parallel migration init
container, edit an applied migration, or mark one applied manually.

### Rate-limit sizing vs the claim pipeline

The per-IP rate limit (`RATE_LIMIT_PER_SECOND` / `RATE_LIMIT_BURST`, default
500/500) must be sized above the claimtxman/aggkit burst rate. Measured on the
N=30 loadtest, the stack's own infrastructure generates 400–1000 rate-limited
requests per run at the default.

Current nonce admission persists a valid future nonce in the Postgres-backed
ephemeral txpool instead of rejecting everything behind a missing nonce. That
removes the old immediate `nonce mismatch` wedge, but it does not make a
rejected lower transaction optional: later transactions wait in the bounded
future-nonce queue until promotion is possible, and non-recovery rows may
eventually be evicted. If rate-limit hits correlate with
`rpc_future_nonce_parked_total` or bound-exceeded events, raise the rate limit
for the infrastructure boundary and have the sender retry the exact missing
signed transaction. Diagnose its state before changing sender storage; do not
clear proxy admission rows or invent a replacement transaction.

### Private listener and authentication

The listener defaults to `0.0.0.0:8546`. Bind to an IP address on loopback or a
private interface, or put it behind an authenticated network boundary. `--bind`
accepts a bare IP; the port is separate.

Current state-changing protections:

- no `ALLOWED_SIGNERS` means `eth_sendRawTransaction` rejects every signer;
- `--insecure-allow-any-signer` is development-only and incompatible with
  `--require-hardening`;
- no `ADMIN_API_KEY` means every `admin_*` call is disabled;
- no CORS configuration means browsers receive no allow-origin header;
- `--require-hardening` additionally requires remote-only loopback signer
  custody, an admin key, a signer allow-list, non-wildcard CORS, a
  configured/reachable remote prover, and the strict-H6 L1 evidence contract
  below.

### Remote prover

Set `MIDEN_PROVER_URL` in production. Local proving is the fallback development
behavior and can consume substantial memory. Keep
`MIDEN_PROVER_FALLBACK_TO_LOCAL=false` unless the deployment explicitly accepts
that availability/OOM trade-off. `--require-hardening` probes the remote prover
at startup and refuses local-only mode.

### L1 GER indexer

Configure both `L1_RPC_URL` and `GER_L1_ADDRESS`. If either is missing, the
InfoTree indexer is disabled in non-strict mode; strict H6 instead aborts
startup. Newly projected GER rows can remain unresolved and
`zkevm_getExitRootsByGER` returns `null` for them. There is no safe latest-root
fallback because a combined GER cannot be decomposed after the fact. Monitor
`l1_indexer_state` and the indexer error metrics.

`L1_EVIDENCE_TAG=latest|safe|finalized` selects the indexer's only L1 frontier;
the default is `latest`. `REJECT_UNVERIFIED_GER_INJECTION=true` makes GER
admission wait for roots written by that scan. `REQUIRE_HARDENING=true` implies
strict admission and requires `safe` or `finalized`.

Strict startup also validates an HTTP(S) L1 URL and EVM GER address. On a fresh
database it requires `L1_INDEXER_FROM_BLOCK` at or before rollup deployment; an
existing database may use its non-zero, policy-matched cursor. Before serving,
the indexer synchronously catches up to the selected frontier. The default
budget is 300 seconds (`L1_EVIDENCE_CATCHUP_BUDGET_SECS`); size it for an
initial historical backfill or startup aborts without binding the listener.

The database binds its evidence marker and cursor to the exact selected tag.
Changing it requires stopping the service, clearing the policy-derived marker,
cursor, and binding in one transaction, then restarting with a trusted
`L1_INDEXER_FROM_BLOCK`:

```sql
BEGIN;
UPDATE ger_entries SET finalized_verified = FALSE;
UPDATE l1_indexer_state
SET finalized_block = 0, finalized_scan_cursor = 0, evidence_tag = NULL;
COMMIT;
```

The `finalized_*` column names are retained for migration compatibility; they
store the selected policy's state, not a second scan. On first upgrade,
`latest` resumes the legacy `last_processed` cursor. `safe` and `finalized`
never inherit latest-scan progress and require an explicit first backfill.

### Termination

SIGTERM stops HTTP acceptance and signals the writer. A job already executing
can finish before the worker observes shutdown; queued work is not guaranteed
to drain. The process waits 20 seconds, snapshots residual non-terminal work to
`/tmp/agglayer-writer-queue-snapshot`, then shuts down the Miden client.

Before a planned restart, quiesce submitters and wait for
`agglayer_writer_queue_depth` to reach zero. Give the container more than 20
seconds of termination grace (30 seconds is the minimum practical envelope).
Remember that an ephemeral `/tmp` or SIGKILL can remove the restart snapshot.

## 2. Startup checklist

Before starting or rolling out:

1. Confirm immutable image digest and expected binary arguments with
   `miden-agglayer-service --help` from that image.
2. Confirm one replica and exclusive persistent-volume ownership.
3. Confirm `--miden-node`, `CHAIN_ID`, `NETWORK_ID`, `BRIDGE_ADDRESS`, L1 RPC,
   and GER contract match the deployment inventory.
4. Confirm Postgres and the complete Miden-store directory are mounted.
5. Confirm signer/admin/prover hardening and private listener topology.
6. Confirm `TMPDIR` is on a filesystem compatible with sqlite's atomic rename
   when the platform has previously produced cross-device rename errors. The
   checked-in Compose stack places it inside the store bind mount.
7. Confirm the faucet security reconciler is enabled. Defaults are a 30-second
   poll and three consecutive anomalous observations; setting poll seconds to
   zero disables the tripwire.
8. Confirm alerting and log collection before enabling bridge traffic.

On a normal existing deployment, startup loads `bridge_accounts.toml`. If it is
missing, the service initializes new accounts automatically. Treat an
unexpected `new config created` log as a stop condition.

`--init` always forces account initialization and exits, even when a config
exists. Never add it to a normal restart.

Post-start verification:

```bash
curl -fsS "$PROXY_RPC/health"
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]}'
curl -fsS "$PROXY_RPC/metrics"
```

Verify the tip advances, durable cursors resumed, the migration run completed,
the L1 indexer and faucet reconciler started when configured, and no integrity
counter increased.

## 3. Recovery mechanisms

The flags below are not interchangeable.

| Mechanism | Changes | Exits? | Use when |
|---|---|---:|---|
| automatic account re-import | Re-imports one affected public account and retries a claim/GER submission once | No | `AccountDataNotFound` or incorrect initial commitment on a live submission |
| `--unlock-miden-accounts` | Clears `locked` in known miden-client sqlite account-header tables | Yes | The service is stopped and evidence proves a stale local lock only |
| `--resweep-from-genesis` | Resets the Postgres note-reconciler cursor to zero, then runs normally | No | Deliberate full-history visibility audit with an otherwise valid Miden store |
| `--l1-indexer-from-block N` | Overrides L1 InfoTree start for that boot | No | Deliberate GER decomposition backfill from a verified L1 block |
| `--restore` | Replays Miden history into the selected synthetic store, resets reconciler cursor, then exits | Yes | Reconstructing a lost/clean synthetic store from authoritative history |
| `--reset-miden-store` | Deletes only miden-client `store.sqlite3`, WAL, and SHM before startup | No by itself | Local Miden sqlite is irrecoverably divergent; keystore/config are intact |
| `--read-only` | Refuses all Miden transaction submission at the chokepoint | No | Passive recovery rehearsal/audit against a production network |

### Automatic account self-heal

The claim and GER paths classify recoverable account errors, import the affected
account from the node, and retry once. A single
`miden_account_reimport_total{outcome="ok"}` is the recovery working. Repeated
failures, `AccountIsPrivate`, or an account absent from chain require operator
analysis; do not loop restarts.

### Surgical unlock

Preconditions:

- service process is stopped (sqlite must have one owner);
- account config and keystore are backed up;
- the only proven defect is a stale sqlite `locked` flag;
- authoritative account state is otherwise consistent.

Run the same image with the same store mount:

```bash
miden-agglayer-service \
  --miden-store-dir "$MIDEN_STORE_DIR" \
  --unlock-miden-accounts
```

It updates `latest_account_headers` and `historical_account_headers`, then
exits. If both known tables/columns are absent, it fails loudly because the
miden-client schema changed; use the full reset decision instead of assuming a
zero-row success. Restart normally and verify a transaction before closing the
incident.

### Full-history note resweep

`--resweep-from-genesis` resets only the synthetic store's reconciler cursor.
The service remains live and walks Miden history in bounded windows. It can take
hours and load the node. Remove the flag after the audit boot; otherwise every
restart repeats the reset.

Do not use it for a node connectivity problem or to rewrite emitted logs. The
visibility barrier holds projection while the sweep catches up.

### L1 InfoTree backfill

Set `--l1-indexer-from-block N` only after deriving `N` from authoritative L1
event history. The override wins over the persisted cursor. Watch
`L1InfoTreeIndexer batch processed`, the durable cursor, and unresolved GER
rows until the indexer passes the L1 head, then remove the flag for the next
boot.

### Synthetic-store restore

`--restore` is a one-shot reconstruction and then exits. It does **not** create
or select a new Postgres database for you and is not a command to append guessed
events to live history. Establish the intended clean/coordinated recovery store
before running it.

Required inputs are the normal account config/Miden node/network settings plus
the target `DATABASE_URL`; provide L1 RPC for GER/metadata reconstruction. Use
`--read-only` when the drill must be provably non-mutating:

```bash
AGGLAYER_READ_ONLY=true \
miden-agglayer-service \
  --miden-node "$MIDEN_NODE_URL" \
  --miden-store-dir "$MIDEN_STORE_DIR" \
  --database-url "$DATABASE_URL" \
  --network-id "$NETWORK_ID" \
  --bridge-address "$BRIDGE_ADDRESS" \
  --l1-rpc-url "$L1_RPC_URL" \
  --restore
```

Run it with the normal service stopped and exclusive store ownership. Verify
every restore phase, counts, cursor/tip, faucet identities, quarantines, and a
complete log fingerprint before starting the normal service. `--restore`
replays synthetic events during this offline reconstruction; the
`SyntheticProjector` remains the sole producer in normal live operation.

### What a restore preserves — and the one field it cannot

A restore reconstructs synthetic history from authoritative Miden state. Every
consumer-visible field of every log is reproduced exactly — `block_number`,
`log_index`, `block_hash`, `address`, `topics`, `data`, `transaction_index`,
`removed` — as is `hash_chain_value`. `BridgeEvent.transaction_hash` is likewise
stable, because it is derived from the note commitment on both the live and the
restore path.

**`logIndex` semantics (Ethereum-standard, restore-stable by construction).**
The served `logIndex` is the log's position within its block, dense from 0 —
standard Ethereum semantics — assigned in a canonical per-block order
(`UpdateHashChain`, then `ClaimEvent`, then `BridgeEvent`; within a kind,
emission order). It is NOT the store's internal emission counter: within a
block, the interleave of the GER writer and the projector is a wall-clock race
that no restore can replay (measured: 2 swapped pairs / 79 logs; re-deriving it
from note-consumption order made things worse and was reverted). Because the
canonical order is a pure function of each block's log *content*, a faithful
restore serves bit-identical `logIndex` values without needing to reproduce the
race. A filtered `eth_getLogs` still reports each log's absolute in-block
position, and receipts agree with `eth_getLogs`. The internal
`synthetic_logs.log_index` column keeps the global emission sequence and may
legitimately differ across a restore — never compare it across stores; compare
the served view.

**`ClaimEvent.transaction_hash` is the exception, and operators must expect it to
change.** A claim's tx hash has two possible sources:

| source | table | survives full DB loss? |
| --- | --- | --- |
| `observed_tx_hash` | `note_handoff` | no — lives only in Postgres |
| `get_tx_for_note` | `tx_note_links` | no — lives only in Postgres |
| `derive_manual_claim_tx_hash(note_commitment)` | — (deterministic keccak) | yes |

A claim that rode a real eth-tx (the `publish_claim` path) recorded that hash only
in Postgres — it was never written to L1 and the Miden note does not carry it. So
after a full DB loss the restore falls back to the derived hash. The rule:

> A claim's `transaction_hash` is rewritten real → derived on its **first**
> restore, and is a bit-exact no-op on every restore afterwards.

On a first restore of a production store this affects **every** claim that was
submitted through `publish_claim`, not a rare edge case.

**This is not data loss.** The log itself is intact and still fetchable by
`(block, log_index)`, and #136/#67 guarantee the full `claimAsset` calldata stays
servable under whichever hash carries the event — which is why aggkit continues to
settle across the rewrite. aggkit resolves through the note, so it is unaffected.

**Operator implication:** an external consumer that cached a claim's
`transaction_hash` before the restore will not find that hash afterwards. Consumers
that key on `(block_number, log_index)` or on the claim's global index are
unaffected. If you run such a consumer, re-index it from the restored logs rather
than reconciling by tx hash.

`scripts/e2e-full-db-loss-recovery.sh` asserts exactly this boundary: it compares
`eth_getLogs` before and after at the JSON-RPC level and fails if **anything other
than** `transaction_hash` differs. It also refuses to run against a store that is
itself restore output (`service_state.nonce_ledger_rebuilt = true`), because such a
run would only measure idempotence, not fidelity.

### Nonce admission after full PostgreSQL loss

The submitter nonce ledger is proxy-local and cannot be reconstructed from
Miden. A restore into a fresh PostgreSQL database therefore records a durable
rebuild marker. On the next serving boot, a signer with no nonce row may park
its first observed transaction; after a three-projected-block settle margin,
the lowest eligible parked nonce becomes that signer's baseline and the
contiguous run is promoted. Ordinary nonce ordering applies after that
first-contact seed.

The global first-contact window defaults to six hours from the restore stamp
and is configured with `NONCE_RECOVERY_WINDOW_SECS`. Restarts do not extend it.
A transaction parked while the window is open carries
`parked_during_recovery=true`; it remains eligible and is exempt from the normal
3600-block txpool TTL even if the global window closes first. It remains until
admission/reconciliation, or until an operator handles a surfaced stranded row.

This policy intentionally treats **any signer first seen while the recovery
window is open as continuing**, including a genuinely new signer. That is an
accepted operational trade-off of recovering a nonce ledger with no chain
source. Freeze signer onboarding and changes to `ALLOWED_SIGNERS` during the
window, admit only the expected pre-loss identities, and verify
`rpc_nonce_ledger_bootstrapped_total` against that inventory. The
`rpc_nonce_recovery_mode_expired_total` counter records retirement of the
global window; already stamped rows remain grandfathered until handled.

### Full Miden sqlite reset plus restore

Use only when sqlite divergence cannot be repaired surgically and the managed
accounts are recoverable from the node. Public accounts can be re-imported;
private accounts cannot be reconstructed from node state alone.

Preconditions:

- service stopped;
- coordinated backup of Postgres and the entire Miden store;
- keystore and `bridge_accounts.toml` verified and separately protected;
- account visibility/recoverability verified;
- authoritative Miden/L1 endpoints available;
- recovery rehearsed on copied state.

```bash
AGGLAYER_READ_ONLY=true \
miden-agglayer-service \
  --miden-node "$MIDEN_NODE_URL" \
  --miden-store-dir "$MIDEN_STORE_DIR" \
  --database-url "$DATABASE_URL" \
  --network-id "$NETWORK_ID" \
  --bridge-address "$BRIDGE_ADDRESS" \
  --l1-rpc-url "$L1_RPC_URL" \
  --reset-miden-store \
  --restore
```

The reset deletes only `store.sqlite3` and its sidecars; it preserves the
keystore and account config. Combining it with restore also resets the durable
note-reconciler cursor so the next normal boot performs the required genesis
sweep.

Never substitute `--init` for recovery: it creates new account identities and
can strand control/balances associated with the old ones.

### Retained PostgreSQL + reset Miden store — a SUPPORTED mode, readiness-gated

Resetting the Miden store while **retaining** the synthetic Postgres is a
supported recovery mode (issue #148). In it, a processed claim and its synthetic
`ClaimEvent` survive in Postgres, but the claim's stored `claimAsset` calldata
envelope may be absent; the reset forces the genesis reconciler to re-observe
each historical CLAIM note and **backfill** its calldata
(`persist_synthetic_claim_tx`). Until that repair completes,
`eth_getTransactionByHash` would serve the claim with **empty input**, and
aggkit's bridgesync full-claim parser stalls on it.

**The service gates readiness on the repair.** While any ClaimEvent still lacks
its persisted calldata, `GET /health` returns **503** with
`{"status":"recovering","claims_awaiting_calldata":N}`; it flips to **200** only
once `N` reaches zero (every historical claim's calldata is re-persisted) and the
node connection is alive. The `claim_calldata_repair_backlog` gauge tracks `N`.
In steady-state operation this backlog is always zero (a claim's envelope is
admitted before its ClaimEvent is emitted), so `/health` is unaffected outside
recovery.

**Operator expectation:** do NOT release consumers — bridge-service / aggkit
bridgesync — until `/health` reports ready (200). Keep bridge-service stopped (or
un-pointed) through the recovering window; releasing it early exposes it to the
empty-input claim and stalls its sync. Once ready, resync the bridge-service
alongside the reset proxy (its cached sponsor nonce must be re-fetched against the
reset-to-0 proxy — see finding #65) and the recovered claim settles normally. A
persistently nonzero `claims_awaiting_calldata` means a claim's calldata is
genuinely unrecoverable (its metadata preimage is lost — see
`synthetic_claim_calldata_unrecoverable_total` and the mismatched-registry-row
procedure below); resolve that before releasing consumers rather than forcing
readiness.

### Detecting + remediating a mismatched native-faucet registry row

`admin_registerNativeFaucet` now validates caller-supplied metadata against the
deployed Miden faucet account before writing anything (issue #149): the persisted
+ emitted metadata-hash preimage `abi.encode(name, symbol, decimals)` is taken
from the faucet account, never from caller-supplied params. This guarantees the
preimage is reconstructable from authoritative chain state during `--restore`
(recovery derives its only native-token candidate from the faucet account). A
mismatched symbol, decimals, or name is rejected up-front with a specific error
and leaves no registry row.

**Legacy state is not migrated.** The supported rollout is clean-slate: the new
deployment starts fresh with this validation active, so no legacy mismatched row
carries into it, and no in-place repair path is provided. The detection below is
for diagnosing an unexpected mismatched row on a stack that must be kept — not a
supported migration.

A row registered by an **older build** may still carry a preimage that does not
match its deployed faucet account. Recovery does **not** silently guess a
preimage — an unrecoverable native row halts `--restore` fail-closed (its poison
leaf). To detect it:

1. Detect. For each native row (`origin_network` == the proxy's configured
   `network_id`), compare the stored preimage against the deployed faucet
   account's authoritative `token_name` / `symbol` / `decimals`:

   ```sql
   -- stored preimage (hex) per native faucet
   SELECT faucet_id, symbol, origin_decimals, encode(metadata,'hex')
   FROM faucet_registry
   WHERE origin_network = <configured network_id>;
   ```

   Read the faucet account's authoritative metadata from Miden (the same
   `token_name()` / `symbol()` / `decimals()` the proxy reads at registration),
   ABI-encode `(name, symbol, decimals)`, and confirm its `keccak256` equals the
   faucet's on-chain `MetadataHash`. A row whose stored preimage keccak differs
   from the deployed faucet's hash is mismatched.

2. Do not attempt an in-place repair. `admin_registerNativeFaucet` is
   register-if-absent (idempotent) — it never rewrites an existing row — so
   re-registration cannot fix a mismatched row. Surgical row deletion +
   re-registration is also **not** a repair: it discards the only locally
   retained legacy preimage and overwrites the bridge's current metadata hash,
   yet it cannot repair historical B2AGG leaves/events already committed with
   the old hash. The supported rollout is clean-slate, so legacy state is not
   migrated and this situation does not arise on it. If a mismatched row is
   ever observed on a stack that must be kept: preserve and back up the current
   state, quarantine the affected faucet, and escalate — or rebuild from a
   clean deployment. Do not perform surgical row edits.

## 4. Incident procedures

### Node outage or `/health` 503

1. Preserve health, Miden error metrics, and current/previous logs.
2. Verify the configured endpoint, DNS, routing, TLS, and `MIDEN_API_KEY`
   secret reference with the node operator.
3. Let the client's exponential reconnect loop work.
4. Do not reset sqlite/Postgres for a connectivity outage.
5. After recovery, verify cursors/tip advance and writer pending work resolves.

### Pending transaction or writer restart

Look up the hash in `queued_txns`, `transactions`, `tx_note_links`, and
`nonce_reservations` as shown in diagnostics. A parked future transaction has a
`queued_txns` row but no `transactions` row until promotion.

- Writer-queue saturation returns JSON-RPC `-32005`; callers should back off and
  rebroadcast the exact signed envelope.
- Queue-wait TTL can fail a job only before dispatch when no durable handoff
  exists. The maintenance sweeper also evicts old terminal in-memory entries;
  it never fails queued/submitting work.
- Once an exact note handoff exists, timeout/error ambiguity leaves the receipt
  pending. Only exact note observation/commit or authoritative expiration
  classification may resolve it.
- A different transaction cannot replace the durable `(signer, nonce)` owner.
- A valid nonce above the executable frontier is accepted into the bounded
  Postgres-backed but ephemeral `queued_txns` pool. Filling the gap triggers an
  ordered promotion attempt, with retained failures retried periodically;
  same-hash rebroadcast is idempotent and a different same-nonce hash is
  rejected.
- A non-recovery parked row still resident at its 3600-projected-block TTL may
  be evicted, including after a failed promotion attempt. Its hash then stops
  resolving and its capacity is reclaimed; the sender must retain and
  rebroadcast the signed envelope if it still needs it. Recovery-stamped rows
  follow the exemption described above.
- A lower nonce admitted without reaching handoff blocks higher nonces until
  the exact lower signed transaction is resubmitted.

Recovery:

1. Obtain the original raw signed transaction from the caller/transaction
   manager, `queued_txns.envelope`, or `transactions.envelope_bytes` through an
   approved forensic path.
2. Submit those exact bytes again; verify the returned EVM hash is unchanged.
3. Observe handoff/receipt reconciliation and nonce progression.
4. Never construct a new random Miden note or a new EVM transaction at the same
   nonce to "unstick" it.
5. Never delete admission/handoff rows manually.

### Interrupted GER or claim job: automatic orphan recovery

The service automatically recovers acknowledged `pending` transactions after an
interrupted writer job. A bounded background pass starts at boot and repeats
every 30 seconds. It works from the stored signed envelope and authoritative
Miden state, in nonce order per signer:

- an intent with no durable note handoff is re-enqueued with persistent,
  exponential backoff;
- a submitted handoff or recorded Miden transaction is polled and is never
  blindly resubmitted;
- a prepared-but-unconfirmed handoff is retained until the reconciliation
  cursor proves its finite expiration has passed, then a fresh note is driven;
  and
- an effect already applied in Miden is reconciled to a terminal receipt for
  the original proxy transaction hash.

Do not delete `transactions`, `tx_note_links`, `nonce_reservations`, or `nonces`,
and do not recreate a transaction at the same nonce. Those changes destroy the
recovery source of truth and can violate admission ordering.

**Diagnosis:** first rule out a silent ntx-builder using the next procedure. If
note consumption is advancing, inspect the recovery backlog and its handoff
state:

```sql
SELECT t.tx_hash, t.signer, t.miden_tx_id, t.status,
       t.recovery_attempts, t.next_recovery_at,
       l.handoff_state, l.note_id, l.prepared_expiration_block,
       t.created_at, t.updated_at
FROM transactions AS t
LEFT JOIN tx_note_links AS l ON l.tx_hash = t.tx_hash
WHERE t.status = 'pending'
ORDER BY t.created_at;
```

Watch `pending_unlinked_txns` and
`pending_unlinked_oldest_age_seconds`; the backlog and oldest age should fall as
dependencies recover. `orphan_recovery_successes_total`,
`orphan_recovery_redrives_total`, and
`orphan_recovery_already_claimed_total` identify forward progress. Page on any
increase in `orphan_recovery_persistent_failures_total`, or on a persistently
rising oldest-age gauge while the Miden node and writer are healthy. Correlate
with `target=recovery` logs and dependency health.

If recovery remains stuck, preserve the database, exact envelope, handoff row,
and logs and escalate for code-level diagnosis. A legacy prepared handoff with
`prepared_expiration_block = 4294967295` predates finite note expiry and cannot
self-heal through expiration; resolve that bounded upgrade edge case out of
band. It is not authorization for ad-hoc row deletion or nonce rewinding.

### ntx-builder silent death (network-note consumption halts)

Upstream Miden issue (finding #68). After all account actors log
`Account actor deactivated due to idle timeout`, the ntx-builder can stop
following the chain entirely — no further `apply_committed_block` lines, no
error, process alive — while the tip advances. Because the bridge is a network
account, ALL bridge note consumption (CLAIM, UpdateGerNote) halts with it:
claims stop landing, GER injections stall, and store event counts freeze
silently. Check this condition before treating an old pending transaction as an
orphan-recovery failure.

**Diagnosis:** compare the ntx-builder's last log timestamp against the Miden
tip. Healthy operation logs `apply_committed_block` every few seconds; more
than ~4 minutes of silence while the tip moves means it is dead. Recurrence is
more likely when note traffic is bursty/sparse (every actor idles out) and
intensifies under infrastructure faults.

**Recovery:** `docker restart <ntx-builder-container>`. It re-applies from the
committed tip and consumes the backlog within seconds; no state cleanup is
needed anywhere else.

**Prevention / monitoring:** alert on last-log age > 4 min while the tip
advances; an unsupervised watchdog restart on that condition is safe and
recommended until the upstream fix lands.

### Writer saturation

Quiesce or rate-limit producers, confirm the remote prover/Miden node is not the
bottleneck, and let the queue drain. Increasing
`AGGLAYER_WRITER_QUEUE_DEPTH` increases buffering, not throughput, and can
increase queue age. Change it only after measuring job latency and caller retry
budgets, then perform a planned restart with a zero queue.

### Remote prover unavailable

`--require-hardening` fails startup if the configured endpoint cannot be
reached. At runtime, inspect `miden_proof_generations_total` outcomes and proof
latency. Restore prover service/capacity first. Do not silently enable local
fallback on a memory-constrained production pod.

### GER not ready / claim rejected before admission

An unknown GER is rejected before nonce, claim lock, receipt, or writer queue
allocation. Correlate `rpc_claim_ger_not_seen_total`,
`rpc_estimate_gas_ger_not_ready_total`, aggoracle logs, `ger_entries`, and the L1
indexer cursor. Repair L1 RPC/indexer/aggoracle lag; the claimant can retry
cheaply after GER injection.

### Synthetic tip or completeness failure

Compare Miden tip, reconciler cursor, projector cursor, synthetic tip, and
visibility-barrier gauge. A held barrier is intentional fail-close behavior
while note visibility catches up. An increase in
`synthetic_projector_completeness_missing_total` or
`synthetic_projector_b2agg_fetch_missing_total` is a hard incident: pause
dependent certificate production, preserve stores/logs, and do not patch
historical logs.

### B2AGG quarantine

Pause affected bridge-out/certificate flow, preserve the note/table evidence,
and follow [the quarantine guide](quarantine.md). There is no supported live
single-note replay RPC.

### Faucet security tripwire

The reconciler exits the process after an anomalous on-chain faucet persists
for its grace window. Treat this as possible bridge-admin key misuse. Preserve
bridge account state and registry evidence, validate the faucet independently,
and rotate/escalate credentials as required. Do not set the poll interval to
zero to suppress the crash loop.

### Migration startup failure

- Connection/auth error: fix Postgres access; do not bypass migrations.
- Advisory-lock wait: find the other migration/service connection before
  killing anything.
- Checksum mismatch: the image embeds an edited already-applied migration.
  Stop rollout and use an image with the original file plus a new superseding
  migration.
- SQL application error: preserve the database and failed image digest; restore
  from backup only through the release rollback procedure.

### Admin or signer rejection

- `admin auth: admin endpoints disabled` means no `ADMIN_API_KEY` is configured.
- `-32001` with missing/invalid bearer token means the caller's admin secret
  wiring is wrong.
- Unauthorized signer means the recovered EVM sender is absent from
  `ALLOWED_SIGNERS` (case-insensitive address parsing).

Change allow-lists through the deployment secret/config pipeline. Do not enable
open signer mode as an incident shortcut on a reachable interface.

### Unclaimable claim record

`unclaimable_claims` records a claim whose destination could not be resolved;
the service emitted a synthetic completion without minting funds so upstream
retry loops stop. There is no current admin rescue endpoint. Preserve the row,
global index, destination, amount, and EVM hash and escalate to the bridge/token
owner; do not delete `claimed_indices` to replay it.

## 5. Planned shutdown and restart

1. Record tip/log fingerprint, image digest, metrics, pending transactions, and
   handoffs.
2. Pause submitters.
3. Wait for writer queue depth zero and note any durable pending handoffs.
4. Send SIGTERM and allow the full grace period.
5. Confirm `agglayer_writer_drain_outcome_total{outcome="clean"}` when the
   metric survives scraping; correlate logs and durable rows because the metric
   endpoint disappears at process exit.
6. Start one replacement process with identical stores/config.
7. Verify health, identity, cursor resume, immutable historical logs, tip
   progress, and pending exact-hash reconciliation.
8. Restore traffic gradually.

For image changes, follow the stricter [upgrade guide](../UPGRADE.md).
