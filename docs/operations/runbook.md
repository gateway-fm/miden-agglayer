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

Set `DATABASE_URL` in production. Without it the service uses `InMemoryStore`,
which loses synthetic logs, receipts, cursors, faucet routes, and admission
state at restart.

Postgres migrations are embedded into the binary and run automatically under
an advisory lock before the pool opens. A previously applied file whose
checksum differs aborts startup. Do not use a parallel migration init
container, edit an applied migration, or mark one applied manually.

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
- `--require-hardening` additionally requires admin key, signer allow-list,
  non-wildcard CORS, and a configured/reachable remote prover.

### Remote prover

Set `MIDEN_PROVER_URL` in production. Local proving is the fallback development
behavior and can consume substantial memory. Keep
`MIDEN_PROVER_FALLBACK_TO_LOCAL=false` unless the deployment explicitly accepts
that availability/OOM trade-off. `--require-hardening` probes the remote prover
at startup and refuses local-only mode.

### L1 GER indexer

Configure both `L1_RPC_URL` and `GER_L1_ADDRESS`. If either is missing, the
InfoTree indexer is disabled; newly projected GER rows can remain unresolved
and `zkevm_getExitRootsByGER` returns `null` for them. There is no safe
latest-root fallback because a combined GER cannot be decomposed after the
fact. Monitor `l1_indexer_state` and the indexer error metrics.

`L1_EVIDENCE_TAG=latest|safe|finalized` selects the indexer's only L1 frontier;
the default is `latest`. `REJECT_UNVERIFIED_GER_INJECTION=true` makes GER
admission wait for roots written by that scan. `REQUIRE_HARDENING=true` implies
strict admission and requires `safe` or `finalized`.

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

Look up the hash in `transactions`, `tx_note_links`, and
`nonce_reservations` as shown in diagnostics.

- Queue saturation returns JSON-RPC `-32005`; callers should back off and
  rebroadcast the exact signed envelope.
- Queue-wait TTL can fail a job only before dispatch when no durable handoff
  exists. The maintenance sweeper also evicts old terminal in-memory entries;
  it never fails queued/submitting work.
- Once an exact note handoff exists, timeout/error ambiguity leaves the receipt
  pending. Only exact note observation/commit or authoritative expiration
  classification may resolve it.
- A different transaction cannot replace the durable `(signer, nonce)` owner.
- A lower nonce admitted without reaching handoff blocks higher nonces until
  the exact lower signed transaction is resubmitted.

Recovery:

1. Obtain the original raw signed transaction from the caller/transaction
   manager or the durable `envelope_bytes` through an approved forensic path.
2. Submit those exact bytes again; verify the returned EVM hash is unchanged.
3. Observe handoff/receipt reconciliation and nonce progression.
4. Never construct a new random Miden note or a new EVM transaction at the same
   nonce to "unstick" it.
5. Never delete admission/handoff rows manually.

### Stuck GER injection (interrupted `ger_insert`) — aggoracle deadlock

Known failure mode (tracked as finding #70; recurred repeatedly under fault
testing). If the proxy is restarted/paused while a `ger_insert` writer job is
in flight, the aggoracle's `insertGlobalExitRoot` transaction can be left
`status='pending', block_number=0` permanently: nothing resumes the job after
restart. Two deadlocks then stack:

- the aggoracle's ethtxmanager polls that hash forever ("waiting signedTx to
  be mined"); and
- its monitored-transaction ID is deterministic (hash of from/to/calldata, no
  nonce), so even a recreated aggkit re-derives the same ID, logs
  `inject GER transaction already exists in monitoring DB`, and never sends a
  new injection.

Blast radius: GER injection stops → deposits never turn `ready_for_claim`,
L2↔L2 settlement stalls, and the store's UpdateHashChain/ClaimEvent counts
freeze while the chain keeps advancing. There is no error anywhere — only
silence.

**Diagnosis (in order):**

1. Rule out the ntx-builder first (see the next procedure): if it is silent,
   restart it and re-check before touching anything else — an unconsumed
   UpdateGerNote resolves itself once consumption resumes.
2. Stuck pending injection:

   ```sql
   SELECT tx_hash, status, block_number, created_at
   FROM transactions
   WHERE lower(signer) = '<aggoracle sender, lowercase>'
     AND status = 'pending'
     AND created_at < now() - interval '3 minutes';
   ```

   The aggoracle sender address is logged at aggkit startup
   (`AggOracle sender address: 0x…`).
3. Aggoracle deadlock confirmation: aggkit logs repeat
   `inject GER transaction already exists in monitoring DB with ID 0x…` with
   no interleaved `submitted`, and the proxy receives no
   `eth_sendRawTransaction` from that sender.
4. Corroborate the freeze: `ger_entries.is_injected` stops advancing;
   `synthetic_logs` UpdateHashChain count is static while the Miden tip moves.

**Recovery.** This is the one documented exception to "never delete
admission rows / never replace a pending transaction". It is safe if and only
if ALL of the following hold — verify each before proceeding:

- the pending rows belong to the aggoracle sender only;
- their GERs show `is_injected = false` in `ger_entries` (the injection never
  landed — nothing external ever observed a receipt);
- the only consumer of those hashes is the aggoracle itself, and it is reset
  in the same procedure (fresh ethtxmanager state);
- GER injection is content-idempotent: the same root is safely re-injected
  under a new transaction.

Procedure (order matters — client side must come back AFTER the proxy):

```bash
# 1. stop the aggoracle so it cannot re-send mid-surgery
docker stop <aggkit-container>

# 2. proxy store: remove the dead pending injection(s) + realign the nonce
#    (psql into the proxy's PostgreSQL, agglayer_store)
DELETE FROM tx_note_links WHERE tx_hash IN
  (SELECT tx_hash FROM transactions
   WHERE lower(signer)='<aggoracle>' AND status='pending');
DELETE FROM transactions
  WHERE lower(signer)='<aggoracle>' AND status='pending';
-- MINED := count of that signer's success/reverted rows
DELETE FROM nonce_reservations
  WHERE lower(signer)='<aggoracle>' AND nonce >= MINED;
UPDATE nonces SET nonce = MINED WHERE lower(address)='<aggoracle>';

# 3. restart the proxy; wait for healthy
docker restart <proxy-container>

# 4. recreate aggkit WITH A FRESH CONTAINER (its ethtxmanager sqlite lives in
#    the container /tmp — a plain restart resumes the deadlocked state)
docker rm -f <aggkit-container>
docker compose up -d --no-deps aggkit
```

**Verify:** within ~1 minute the aggoracle logs `inject GER transaction
submitted`, the proxy mines it (`transactions.status='success'`,
`ger_entries.is_injected` flips true, UpdateHashChain count increments), and
new deposits turn `ready_for_claim`.

**Prevention / monitoring:** alert when
`count(pending aggoracle txs older than 3 min) > 0` or when
`ger_entries.is_injected` is static for >5 min while the Miden tip advances.
A supervised auto-heal implementing exactly the procedure above is acceptable.
The product fix — resuming interrupted `ger_insert` jobs on startup via the
durable note handoff — is tracked as finding #70; until it ships, treat this
procedure as the standing remediation.

### ntx-builder silent death (network-note consumption halts)

Upstream Miden issue (finding #68). After all account actors log
`Account actor deactivated due to idle timeout`, the ntx-builder can stop
following the chain entirely — no further `apply_committed_block` lines, no
error, process alive — while the tip advances. Because the bridge is a network
account, ALL bridge note consumption (CLAIM, UpdateGerNote) halts with it:
claims stop landing, GER injections stall (see the previous procedure — check
this FIRST), and store event counts freeze silently.

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
