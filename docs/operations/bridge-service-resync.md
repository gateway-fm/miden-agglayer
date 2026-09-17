# Recover a settled deposit stuck at `ready_for_claim=false`

This procedure recovers the **source network's bridge-service PostgreSQL
index** when a missed exit-root notification leaves an already-settled deposit
unclaimable. It uses the deployed bridge-service image unchanged. Miden, L1,
L2B, the proxy's PostgreSQL/client SQLite stores, and AggLayer certificate state
are preserved. No component fork or patch is required.

Verified on 2026-09-17 with bridge-service revision `2e87f97`, PostgreSQL, and a
sovereign-chain deployment indexing L1 plus one L2 per service. Rehearse on a
separate index before applying this to another version or topology. Use the
[operations deployment-discovery conventions](README.md#conventions) and the
approved incident procedure for database/service changes.

## Cause and identifying evidence

The source L2 synchronizer can index a GER before its L1 counterpart. It then
skips the incomplete L2 notification. When L1 catches up, it fills in the roots,
but the L1 notification marks only **L1-bound** deposits ready. The missed L2
notification was needed to mark deposits bound for another L2 ready. On the
observed sovereign-chain path, periodic trusted-state synchronization does not
replay that notification.

Establish all of the following before choosing this recovery:

- The deposit's certificate is settled and the covering root is present on L1.
- The source bridge API still reports `ready_for_claim=false`; its
  `/merkle-proof?deposit_cnt=…&net_id=…` returns HTTP 500 with
  `not synchronized deposit`.
- The source bridge-service database contains the matching L1 and L2 GER rows,
  and both synchronizers have caught up. A `synced` flag alone does not prove
  deposit readiness.
- Logs show `L1GER not found on database yet` / `Skipping to not provide an
  incomplete GER to the claimTxManager`, followed by processing the L1 root
  for deposits with `destination L1 (networkID = 0)`.

Resolve `BRIDGE_INDEX_DSN` to the **source bridge-service** database, not the
proxy's synthetic database. Set `SOURCE_NETWORK_ID`, `DEPOSIT_COUNT`, and
`GLOBAL_EXIT_ROOT` from the affected deposit and its settlement evidence:

```bash
psql "$BRIDGE_INDEX_DSN" -X -v ON_ERROR_STOP=1 \
  -v source_net="$SOURCE_NETWORK_ID" -v deposit_cnt="$DEPOSIT_COUNT" \
  -v ger_hex="${GLOBAL_EXIT_ROOT#0x}" <<'SQL'
SELECT network_id, percentage, remaining_blocks, synced FROM sync.status;
SELECT network_id, deposit_cnt, dest_net, ready_for_claim, tx_hash
FROM sync.deposit
WHERE network_id = :source_net AND deposit_cnt = :deposit_cnt;
SELECT e.id, e.network_id, b.block_num, e.allowed,
       encode(e.global_exit_root, 'hex') AS ger,
       cardinality(e.exit_roots) AS root_count
FROM sync.exit_root e LEFT JOIN sync.block b ON b.id = e.block_id
WHERE e.global_exit_root = decode(:'ger_hex', 'hex')
ORDER BY e.id;
SQL
```

For comparison, the verified incident had network `2`, deposit `7`, destination
network `1`: the L2 notification was skipped at `08:43:02 UTC`, and L1 indexed
its counterpart at `08:43:03 UTC`. The certificate was already settled.

A restart against copied failing state did **not** recover this incident. A
simultaneous L1/L2 rebuild into an empty index reproduced it. Resyncing AggLayer
certificate storage does not replay this bridge-service notification. The
recovery below orders the two indexers explicitly; the upstream race can recur.

## 1. Preserve state and check the cutover prerequisites

1. Record the image digest, original configuration and automatic-claim setting,
   affected deposit/global index, certificate, GER, API responses and logs.
   Capture Miden's genesis, height and volume identity, and L1/L2 genesis and
   historical block hashes so continuity can be checked afterward.
2. Quiesce affected claim submitters under the incident procedure. Keep the
   chains running. Protect configuration copies and database dumps as secrets.
3. Back up the complete source bridge-service index:

   ```bash
   umask 077
   mkdir -p "$EVIDENCE_DIR"
   pg_dump "$BRIDGE_INDEX_DSN" --format=custom \
     --file="$EVIDENCE_DIR/bridge-index-before.dump"
   ```

4. Inventory state that cannot be reconstructed simply by replaying chain events:

   ```sql
   SELECT status, count(*) FROM sync.monitored_txs GROUP BY status;
   SELECT network_id, deposit_cnt FROM sync.deposit WHERE ignore;
   ```

   The verified recovery had **no monitored transaction rows and no ignored
   deposits**. If either query returns rows, preserve and reconcile that state
   before replacement; this procedure does not establish a migration for it.
   Do not copy `monitored_txs` blindly: its deposit IDs may change during replay.

## 2. Rebuild L1 first in a separate empty index

Provision an empty PostgreSQL database, owned by the bridge-service database
role, with migration permissions. Preserve the original index. For example,
using a maintenance connection to the intended PostgreSQL server:

```bash
createdb --maintenance-db="$PG_ADMIN_DSN" --owner="$BRIDGE_DB_ROLE" "$REBUILD_DB"
```

Copy the original configuration to a protected recovery file. Point **both**
`[SyncDB.PgStorage]` and `[BridgeServer.DB.PgStorage]` at the new database,
including host, port, name and credentials. Keep the original L1 RPC/contracts
and deployment start block. Use separate private API ports or an isolated
container; do not put this instance behind the serving bridge API.

Edit these fields in that copy, preserving all other settings:

```toml
[Etherman]
L2URLs = []

[NetworkConfig]
L2GenBlockNumbers = []
L2PolygonBridgeAddresses = []
L2PolygonZkEVMGlobalExitRootAddresses = []
RequireSovereignChainSmcs = []

[ClaimTxManager]
Enabled = false

[Log]
Level = "debug"
```

All five L2 arrays must be empty together. `ClaimTxManager.Enabled=false`
prevents automatic claim submission during replay; the existing passive handler
will still update deposit readiness when L2 is added in the next phase.

Start the same image digest using the recovery configuration. With the tested
Docker image, the invocation is:

```bash
docker run -d --name "$L1_REPLAY_CONTAINER" --network "$RECOVERY_NETWORK" \
  -v "$L1_CONFIG:/etc/zkevm/bridge-config.toml:ro" \
  --entrypoint /app/zkevm-bridge "$BRIDGE_IMAGE" \
  run --cfg /etc/zkevm/bridge-config.toml
```

Wait for network `0` to report `synced=true`, `remaining_blocks=0`, and the
covering L1 GER and rollup-exit leaf to be indexed. Check errors and the actual
root, not just a timer or a nonempty table. Record the L1 frontier, then stop
this recovery instance (`docker stop "$L1_REPLAY_CONTAINER"` in the example).
Its database must remain intact.

## 3. Replay L2 with the passive readiness handler

Make a second recovery configuration pointing both database sections at the
**same rebuilt index**. Restore all five original L2 arrays and their original
start blocks. Keep `ClaimTxManager.Enabled=false` and debug logging enabled.
Start the unchanged image with this configuration; only one recovery instance
may write this index at a time. For Docker, choose an unused loopback
`RECOVERY_API_PORT`; set `BRIDGE_HTTP_PORT` from `[BridgeServer].HTTPPort`:

```bash
docker run -d --name "$L2_REPLAY_CONTAINER" --network "$RECOVERY_NETWORK" \
  -p "127.0.0.1:${RECOVERY_API_PORT}:${BRIDGE_HTTP_PORT}" \
  -v "$L2_CONFIG:/etc/zkevm/bridge-config.toml:ro" \
  --entrypoint /app/zkevm-bridge "$BRIDGE_IMAGE" \
  run --cfg /etc/zkevm/bridge-config.toml
RECOVERY_BRIDGE_API="http://127.0.0.1:${RECOVERY_API_PORT}"
```

Wait for both L1 and the source L2 to catch up. The L2 replay now finds the L1
roots already stored, and the passive handler receives complete notifications.
Verify the affected deposit becomes `ready_for_claim=true` and the recovery
API returns HTTP 200 with a proof:

```bash
curl --fail-with-body --silent --show-error \
  "$RECOVERY_BRIDGE_API/merkle-proof?deposit_cnt=$DEPOSIT_COUNT&net_id=$SOURCE_NETWORK_ID" \
  > "$EVIDENCE_DIR/rebuilt-proof.json"
```

If readiness or the proof still fails, retain the copies and logs and investigate;
do not manually set `ready_for_claim=true` or reset a chain to force success.

## 4. Audit and switch the serving service to the rebuilt index

Compare the original and rebuilt `sync.deposit`, `sync.claim` and
`sync.token_wrapped` records at a common indexed event frontier. Compare full
canonical content, including transaction hashes, amounts, destinations and
metadata. Join `block_id` through `sync.block.block_num`; internal row IDs may
change. The expected readiness correction is allowed; missing or changed
canonical records are not. Counts alone are insufficient.

For example, export each index's deposits with the same query and compare the
sorted output; perform the corresponding comparison for claims and wrapped
tokens, omitting only their internal `block_id`:

```sql
SELECT ((to_jsonb(d) - 'id' - 'block_id' - 'ready_for_claim') ||
        jsonb_build_object('canonical_block_number', b.block_num))::text
FROM sync.deposit d JOIN sync.block b ON b.id = d.block_id
ORDER BY 1;
```

Keep the `ignore` field in the deposit comparison. Resolve differing frontiers
and repeat the comparison before cutover. Recheck that monitored transaction
and ignored-deposit prerequisites still hold. The verified incident matched
all **18 deposits, 10 claims and 3 wrapped-token records**.

Stop the original source bridge-service and the recovery instance. Repeat the
audit at their recorded frontiers and retain a final original-index backup;
if the frontiers diverged, replay the lagging index and re-audit before cutover.
Then switch **both** database configurations to the audited index. Restore the
original automatic-claim and logging settings and start one serving instance
with the original image digest.

If the rebuilt index lives on an isolated PostgreSQL server, first dump it and
restore it into a separate empty database on the serving server, with the
correct application ownership/permissions. Audit that destination too. The
verified deployment retained its original database name by renaming the old
database to a backup name and the rebuilt database to the serving name, with
the service stopped and its connections closed. Record the old/new names and
configuration references; retain the old database for rollback. Do not drop it
or use a blanket Compose volume teardown.

## 5. Verify recovery end to end

- Confirm the deployed image digest is unchanged and the normal bridge API
  reports readiness and serves the affected proof after the switch.
- Confirm Miden/L1/L2 chain identities and historical anchors still match, with
  nondecreasing heights. This recovery must not reset any of those chains.
- Resume the affected claim path. Check for an existing claim before submitting
  it again; follow the [same-hash retry procedure](runbook.md#pending-transaction-or-writer-restart)
  if a transaction was already submitted.
- Verify a successful receipt, `eth_getTransactionByHash`, and exactly one
  matching `ClaimEvent` from `eth_getLogs` at the event's **canonical block**.
  The writer's initial note-submission block can precede the bridge-consumption
  event; do not substitute it for the event block.
- Verify recipient balances and the expected asset supply. For native unlocks,
  confirm no new wrapped faucet was provisioned. Check subsequent normal
  traffic before closing the incident.

In the verified incident, the blocked return claim completed at canonical block
`6823`, restored **500,000 native units**, left wrapped supply at **0**, and kept
the faucet count at **12**. Receipt, transaction lookup and exact-block log
checks passed. This proves recovery of that incident; a failed release-test run
still needs a new full validation attempt and must not be relabeled as passed.

If cutover checks fail, stop the replacement instance, preserve its logs/index,
and restore the previous database/configuration references before restarting
the original image. The old index may still contain the readiness incident;
rollback restores the previous state, not successful claimability. Reconcile
any transactions admitted after cutover before retrying. Retain all evidence
and keep the chains intact.
