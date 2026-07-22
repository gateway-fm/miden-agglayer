# Diagnostics

This playbook is read-only. It collects enough evidence to choose a recovery
path without changing chain state, database rows, workload replicas, or files.

## 1. Identify the live deployment

The repository does not define production Kubernetes names. Discover and record
them:

```bash
kubectl config current-context
kubectl -n "$NAMESPACE" get deploy,statefulset,pod,service -o wide
kubectl -n "$NAMESPACE" get pod "$POD" \
  -o jsonpath='{.metadata.uid}{"\n"}{.status.startTime}{"\n"}{.status.containerStatuses[*].restartCount}{"\n"}'
```

Set `$WORKLOAD` to a resource/name pair such as `statefulset/example` or
`deployment/example`. Capture the runtime shape without printing literal
environment values:

```bash
kubectl -n "$NAMESPACE" get "$WORKLOAD" -o json | jq '
  .spec.template.spec.containers[] |
  {name, image, args,
   env: [.env[]? | {name, valueFrom}],
   envFrom, volumeMounts, resources}'
kubectl -n "$NAMESPACE" get "$WORKLOAD" -o json | jq '
  {replicas: .spec.replicas,
   strategy: (.spec.updateStrategy // .spec.strategy),
   terminationGracePeriodSeconds: .spec.template.spec.terminationGracePeriodSeconds,
   volumes: .spec.template.spec.volumes}'
```

Confirm exactly one service replica owns the Miden store. If more than one
replica is live against the same volume/database, treat that as an integrity
incident and preserve evidence before changing anything.

## 2. Reach the private HTTP listener

Use the deployment's approved private ingress or a port-forward:

```bash
kubectl -n "$NAMESPACE" port-forward pod/"$POD" 18546:8546
export PROXY_RPC=http://127.0.0.1:18546
```

Collect health, tip, identity, and metrics:

```bash
curl -i -fsS "$PROXY_RPC/health"
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"eth_chainId","params":[]}'
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"web3_clientVersion","params":[]}'
curl -fsS "$PROXY_RPC/metrics" > /tmp/miden-agglayer.metrics
```

`/health` is a readiness gate, not a deep bridge check. HTTP 200 means the
background Miden client is alive **and** no historical claim still awaits
calldata repair. HTTP 503 is returned on node connection loss
(`status: degraded`, `reason: node connection lost`) **or** while the
claim-calldata repair backlog is non-zero (`status: recovering`, retained-
PostgreSQL + reset-Miden-store recovery — see `runbook.md`); both 503 bodies
carry `claims_awaiting_calldata`. A 200 does not prove projector completeness,
GER indexing, or end-to-end settlement.

Repeat `eth_blockNumber` after Miden has produced blocks. A flat tip while the
authoritative Miden chain advances is an incident even if `/health` is 200.

## 3. Logs

Capture both the current and previous container when a restart occurred:

```bash
kubectl -n "$NAMESPACE" logs "$POD" -c "$CONTAINER" \
  --since=6h --timestamps > /tmp/miden-agglayer.current.log
kubectl -n "$NAMESPACE" logs "$POD" -c "$CONTAINER" \
  --previous --timestamps > /tmp/miden-agglayer.previous.log
```

The `--previous` command legitimately fails when there is no prior container.
Search the captured files locally so the original evidence remains intact:

```bash
rg -n -i \
  'panic|fatal|error|oom|database is locked|migration|checksum|heartbeat|note reconciler|visibility barrier|completeness|authoritative duplicate|writer_worker|handoff|L1InfoTreeIndexer|faucet|quarantin|reimport|locked' \
  /tmp/miden-agglayer.current.log /tmp/miden-agglayer.previous.log
```

High-signal interpretations:

| Evidence | Meaning |
|---|---|
| `heartbeat` every five minutes | Process loop is alive; inspect `miden_client_alive` and `latest_block` fields |
| `note reconciler: sweep cursor loaded` | Shows the durable starting cursor; zero is expected only for first boot or an intentional reset/restore/resweep |
| `visibility barrier` warnings or a positive held-block gauge | Projection is waiting for the note visibility sweep; investigate reconciler/node errors |
| `authoritative duplicate reconciliation is uncertain; keeping receipt null` | Exact note outcome is not proven; retaining a pending receipt is fail-closed |
| `writer job errored after durable note handoff; leaving receipt pending` | Do not force-fail or replace the transaction; use exact-hash retry guidance |
| `L1InfoTreeIndexer poll failed` | L1 RPC/indexer is degraded; GER decomposition may lag |
| `SECURITY TRIPWIRE` | An on-chain faucet registration is absent/invalid locally; process exits deliberately |
| `reimported from node` once | Automatic account self-heal succeeded |
| repeated `account reimport failed` | Account recovery cannot converge; inspect account visibility/type and store integrity |

## 4. Postgres snapshot

Obtain `$DATABASE_URL` through the deployment's approved secret-access path.
Do not paste it into tickets or captured terminal output. Run these queries with
a read-only database role when available:

```bash
psql "$DATABASE_URL" -X --set ON_ERROR_STOP=1
```

```sql
-- Schema and the three durable cursors/tips.
SELECT name, checksum, applied_at
FROM schema_migrations ORDER BY name;
SELECT * FROM service_state;
SELECT * FROM l1_indexer_state;

-- Transaction, exact-note handoff, and nonce ownership.
SELECT t.tx_hash, t.signer, t.status, t.miden_tx_id, t.block_number,
       t.error_message, t.created_at, t.updated_at,
       l.handoff_state, l.note_id, l.note_commitment,
       l.prepared_expiration_block
FROM transactions AS t
LEFT JOIN tx_note_links AS l USING (tx_hash)
ORDER BY t.updated_at DESC
LIMIT 200;

SELECT signer, nonce, tx_hash, state, lease_expires_at, fence_token, created_at
FROM nonce_reservations
ORDER BY created_at DESC
LIMIT 200;

-- GER state and unresolved decomposition.
SELECT encode(ger_hash, 'hex') AS ger,
       encode(mainnet_exit_root, 'hex') AS mainnet_exit_root,
       encode(rollup_exit_root, 'hex') AS rollup_exit_root,
       block_number, timestamp, is_injected, created_at
FROM ger_entries
ORDER BY created_at DESC
LIMIT 200;

SELECT count(*) AS injected_without_decomposition
FROM ger_entries
WHERE is_injected
  AND (mainnet_exit_root IS NULL OR rollup_exit_root IS NULL);

-- Fail-closed records.
SELECT note_id, reason, detail, observed_block, created_at
FROM unbridgeable_bridge_outs
ORDER BY created_at DESC;

SELECT global_index, destination_address, origin_network,
       origin_address, amount, reason, eth_tx_hash, created_at
FROM unclaimable_claims
ORDER BY created_at DESC;

-- Faucet identity and metadata size without dumping metadata bytes.
SELECT faucet_id, origin_network, encode(origin_address, 'hex') AS origin_address,
       symbol, origin_decimals, miden_decimals, scale,
       octet_length(metadata) AS metadata_bytes, created_at
FROM faucet_registry
ORDER BY origin_network, origin_address;
```

Do not update these tables to clear a symptom. Several rows are fencing or
handoff records whose manual deletion can cause duplicate Miden effects or
misattribute a synthetic receipt.

## 5. Miden store identity

Inspect presence, ownership, size, and a checksum of the account config. Do not
print private-key files:

```bash
kubectl -n "$NAMESPACE" exec "$POD" -c "$CONTAINER" -- \
  sh -c 'ls -ld /var/lib/miden-agglayer-service; \
         ls -l /var/lib/miden-agglayer-service/store.sqlite3 \
               /var/lib/miden-agglayer-service/bridge_accounts.toml; \
         sha256sum /var/lib/miden-agglayer-service/bridge_accounts.toml'
```

Adapt the path from the live `--miden-store-dir` argument. Do not open or copy a
live sqlite database by reading only its main file; WAL/SHM state may be needed
for a consistent snapshot.

## 6. Symptom guides

### Health is 503 or Miden sync errors climb

Check node DNS/routing, TLS/API-key wiring, and the configured `--miden-node`
value. Correlate `miden_client_build_errors_total`,
`miden_client_restarts_total`, and `miden_sync_errors_total{kind=...}` with node
maintenance. Do not reset either store for a connectivity outage.

### Synthetic tip is flat

Compare:

- `service_state.latest_block_number`;
- `service_state.projector_cursor`;
- `service_state.reconcile_cursor`;
- `synthetic_reconciler_cursor`;
- `projector_visibility_barrier_held_blocks`;
- the Miden node tip from the node operator's supported tooling.

If the reconciler cursor trails, inspect its failed window and node RPC errors.
If the cursor advances but the projector does not, inspect projector errors and
the B2AGG authoritative-fetch/fetch-missing counters. Never advance a cursor by
SQL.

### Transaction receipt stays null

Look up the EVM hash in `transactions`, `tx_note_links`, and
`nonce_reservations`.

- No handoff plus a live queued/inflight job: wait or diagnose writer capacity.
- `handoff_state = 'prepared'`: the external submission outcome is ambiguous.
- `handoff_state = 'submitted'`: the exact note has been committed/observed;
  wait for projection/receipt completion.
- A lower nonce pending with no handoff blocks later nonce admission by design.

The only safe client retry is the original signed envelope (same hash). See the
[runbook](runbook.md#pending-transaction-or-writer-restart).

### GER exists but claims are not ready

Confirm both `--l1-rpc-url` and `--ger-l1-address` are configured, then compare
`l1_indexer_state.last_processed` with the L1 head. Inspect unresolved
`ger_entries`, indexer poll/log/cursor-persist error counters, and aggoracle's
actual submitted GER. `--l1-indexer-from-block` is an explicit one-boot
backfill override, not a normal permanent setting.

### Missing BridgeEvent or quarantine counter increased

Query `unbridgeable_bridge_outs` first. A row is a deliberate fail-closed
non-emission; follow [the quarantine guide](quarantine.md). With no quarantine
row, any increase in `synthetic_projector_completeness_missing_total` or
`synthetic_projector_b2agg_fetch_missing_total` is a projector completeness
incident.

### Account is locked or commitment diverged

The startup diagnostic reports managed accounts marked locked. The live claim
and GER paths re-import on recoverable account errors and retry once. A single
successful `miden_account_reimport_total{outcome="ok"}` is healing; repeated
failures require the recovery decision tree in the runbook. Do not run
`--unlock-miden-accounts` merely because a node connection failed.

### Process exits after faucet tripwire

Preserve the bridge account state, local faucet registry, and logs. The process
deliberately exits after an anomalous on-chain faucet persists for the configured
grace ticks. Treat it as possible admin-key misuse until ownership and metadata
are independently verified. Do not disable the reconciler to make the pod stay
up.

## 7. Evidence handoff

Attach to the incident without secrets:

- timestamp/time zone, cluster context, namespace, workload/pod UID;
- old/current image digest, sanitized args and secret *references*;
- current and previous logs;
- `/health`, JSON-RPC identity/tip results, and metrics snapshot;
- read-only SQL results above;
- account-config checksum and persistent-volume identity;
- relevant L1 transaction hash, EVM proxy transaction hash, Miden note ID, and
  affected global/deposit index.

State explicitly whether any mutation/restart occurred after evidence capture.
