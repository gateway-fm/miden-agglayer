# miden-agglayer diagnostics

The read-only playbook: what to inspect, in what order, to localise a
failure before reaching for [`runbook.md`](./runbook.md). Pair this with
the [`miden-bali-debug` skill](../../.claude/skills/miden-bali-debug/SKILL.md)
when you want the snapshot collected for you.

## Read-only contract

Everything in this doc is non-mutating:

- `kubectl`: `get`, `describe`, `logs`, `port-forward` only.
- SQL: `SELECT` / `\d` only — never `UPDATE`, `INSERT`, `DELETE`, `ALTER`,
  `TRUNCATE`.
- JSON-RPC: `eth_*` reads (`eth_blockNumber`, `eth_getLogs`, `eth_call`)
  and `zkevm_get*` reads.

If at any point you need a mutating action, **stop, write down what you
saw, switch to runbook.md**.

## Accounts at a glance — what to inspect and where

miden-agglayer's bridging surface involves five distinct account classes
on each side. Knowing where each lives is the prerequisite to tracing a
single deposit or withdrawal.

| Class | Side | Role | How to look it up |
|---|---|---|---|
| **Bridge contract** | L1 | `polygonZkEVMBridge` — receives `bridgeAsset()` for L1→L2 deposits and finalises `claimAsset()` for L2→L1 withdrawals. | `BRIDGE_ADDRESS` env on the proxy pod (bali: `0x1348947e282138d8f377b467f7d9c2eb0f335d1f`, source: `miden/bali-l1-deposit.sh`). |
| **L1 GER contract** | L1 | `polygonZkEVMGlobalExitRoot` — emits `UpdateL1InfoTree` events; the L1InfoTreeIndexer scrapes this. | `L1_GER_ADDRESS` env (bali: `0x2968d6d736178f8fe7393cc33c87f29d9c287e78`). |
| **RollupManager** | L1 | Receives `claimAsset()` post-cert; ClaimSettler talks to this. | `ROLLUP_MANAGER_ADDRESS` env (bali: `0xe2ef6215adc132df6913c8dd16487abf118d1764`). |
| **ger_manager** | L2 (Miden) | Receives `UpdateGerNote` notes injected by the proxy on each `insertGlobalExitRoot`/`updateExitRoot` call. | `bridge_accounts.toml` inside the pod at `--miden-store-dir/bridge_accounts.toml`. Account IDs are 30-hex (15 bytes). |
| **bridge** | L2 (Miden) | Mints wrapped assets via `mint_and_send` on CLAIM; consumes B2AGG notes for withdrawals. | Same `bridge_accounts.toml`. |
| **Faucet(s)** | L2 (Miden) | Owned by `bridge`, hold per-asset supply. Listed in the `faucet_registry` table (migration 002). | `SELECT * FROM faucet_registry;` against the proxy DB. |
| **Aggsender signer** | L1 + L2 | Submits AggLayer certificates. | aggkit config — `<TODO: confirm aggsender pubkey location for bali>`. |
| **Aggoracle signer** | L1 (read) → L2 (write) | Calls `insertGlobalExitRoot`/`updateExitRoot` on the proxy. | aggkit config. |
| **ClaimSettler signer** | L1 (write) | Auto-claims settled L2→L1 transfers. | `CLAIM_SETTLER_PRIVATE_KEY` env on the proxy pod. **Never log or echo this.** Derive the public address from logs only. |
| **Claim sponsor** | L2 (write) | Submits CLAIM notes on the proxy for L1→L2 deposits. | aggkit / claimsponsor config. |

To inspect an L1 account: `cast` against the Sepolia RPC.

```bash
cast balance <address> --rpc-url "$SEPOLIA_RPC_URL"
cast nonce   <address> --rpc-url "$SEPOLIA_RPC_URL"
cast code    <address> --rpc-url "$SEPOLIA_RPC_URL"   # contract presence check
```

To inspect a Miden account: the proxy exposes none of this directly via
JSON-RPC. The two read-only options:

- Read the cached state via the proxy DB tables — `address_mappings`,
  `faucet_registry`, `bridge_accounts.toml` (the file).
- Use `miden-cli` from a separate workstation against the same
  miden-node RPC. **Do not run `miden-cli` from inside the proxy pod** —
  it would contend with the live `MidenClient` on the same sqlite file.

`<TODO: Max — confirm a sanctioned bali miden-cli setup, ideally pointed
at rpc.testnet.miden.io with a read-only store_dir, and add the invocation
here.>`

## 1. Sanity (always run first)

```bash
kubectl config current-context           # must equal the cluster you think you're on
kubectl get pods -n outpost-testnet-miden-testnet
```

Expect: `miden-agglayer-0` `Running`, restart count low (single digits
over the last week, ideally 0-1 attributable to deploys).

If context is wrong: **stop**. If restart count is high, the data-layer
queries below may be chasing a downstream effect of a pod-layer cause.

## 2. Pod fingerprint

```bash
kubectl -n outpost-testnet-miden-testnet describe pod miden-agglayer-0 \
  | grep -E 'Image|Args|Reason|Started|Restart Count|Limits|Requests'
```

Capture and reason about:

- **`Image`** — pre-v0.3.0 (`:0.2.1` or earlier) is the postmortem-class
  baseline; the proxy is missing the IAIC fix, the Phase 0 restore
  reimport, and the runtime self-heal.
- **`Args`** — `--reset-miden-store`, `--restore`, `--unlock-miden-accounts`,
  `--init` should NEVER be live in steady state. If any of them is set,
  the pod is in a recovery sequence — wait for it to finish before
  trusting any data.
- **`Environment`** — `L1_RPC_URL`, `L1_GER_ADDRESS`, `DATABASE_URL`,
  `BRIDGE_ADDRESS` must all be set. `CLAIM_SETTLER_ENABLED=true` enables
  the L2→L1 auto-claim path; if `false`, withdrawals require manual
  claim on L1.
- **`Last State.Reason`** — `OOMKilled` is a tell that the indexer
  cursor may have rolled back to current L1 head on the latest restart.
- **`Limits.memory`** — flag for bump if OOMs are recurring.

## 3. Log signals

```bash
kubectl logs miden-agglayer-0 -n outpost-testnet-miden-testnet --tail=20000 \
  | grep -E 'L1InfoTreeIndexer|GER injection|GER already seen|exit roots don|incorrect account initial commitment|account data wasn|UpdateGerNote|insertGlobalExitRoot|updateExitRoot|OOMKilled|reimport|bridge_invariant|claim_watcher'
```

Translate patterns to verdicts via this decision table:

| Pattern | Verdict |
|---|---|
| `L1InfoTreeIndexer polled (no events), from: N, to: N, head: N` | Indexer alive + caught up. |
| `from: M, to: M, head: N` with `N >> M` | Indexer falling behind — runbook section E.2. |
| `GER injection: submitting to Miden... ger: <hex>` followed by no `UpdateGerNote transaction committed` for that hex within 30s | Miden submission stuck or failed silently — chase by GER hex. |
| `account data wasn't found for account id 0x<id>` | Miden-store divergence (missing account). Runbook A. |
| `incorrect account initial commitment` | Miden-store divergence (stale commitment) OR mempool conflict (read the gRPC tail). Runbook A. |
| `transaction conflicts with current mempool state` | Pure mempool conflict — should be impossible on v0.3.0+. If observed, regression. Runbook A.3. |
| `L1 exit roots don't match injected GER, storing without roots` | RD-862 race fired — orphan stored. Confirms `UseUpdateExitRoot=false` mode. |
| `GER already seen, skipping duplicate` | Dedup poison: the combined hash already has `is_injected=TRUE` with whatever roots are in `ger_entries` (possibly NULL). |
| `reimporting ger_manager` followed by `reimported from node` | Self-heal fired on a recoverable error. Expected once per pod restart on bali; multiple firings in steady state = chronic divergence. |
| `bridge_invariant_violation: <kind>` | Cantina hard-page metric incremented. Runbook D. |
| `claim_watcher synthesised ClaimEvent` in steady state (not just at startup) | The `eth_sendRawTransaction` path failed to record a CLAIM; watcher cleaned up after it. Sustained = `service_send_raw_txn` is broken. |

## 4. Trace a single L1→L2 deposit

You need:

- `deposit_cnt` (decimal, from bridge-service or the depositor's `cast
  receipt` output).
- Optionally the user's destination address (Miden AccountId 30-hex or
  Eth-mapped 20-byte).

### 4.1 Did bridge-service see the deposit?

Port-forward the bridge DB (see runbook preamble), then:

```sql
SELECT deposit_cnt, network_id, dest_net, ready_for_claim, block_id,
       encode(orig_addr, 'hex') AS orig,
       encode(dest_addr, 'hex') AS dest,
       amount
FROM sync.deposit
WHERE network_id = 0 AND deposit_cnt = <cnt>;
```

Expect a row with `network_id=0` (L1 origin), `dest_net=<your rollup id>`
(bali: 73 historically, 76 post-relaunch — confirm against current
deployment).

- No row: bridge-service hasn't ingested the L1 event. Either bridge-service
  is behind on L1 sync, or the deposit transaction reverted on L1.
  Verify with `cast receipt <tx-hash> --rpc-url $SEPOLIA_RPC_URL`.
- `ready_for_claim = false`: no covering GER on bridge-service yet —
  jump to 4.2.
- `ready_for_claim = true`: the deposit is ready to be claimed on L2 —
  jump to 4.3.

### 4.2 Has the covering GER reached the proxy?

```sql
-- against the proxy DB
SELECT count(*) FROM ger_entries WHERE block_number > (
  SELECT block_number FROM ger_entries
  WHERE block_number <= (
    SELECT block_id FROM sync.deposit WHERE network_id=0 AND deposit_cnt=<cnt>
  )
  ORDER BY block_number DESC LIMIT 1
) AND is_injected;
```

A non-zero result means at least one newer GER has been injected on L2,
which should be sufficient to cover the deposit (exit trees are
append-only — see [`../ger-decomposition.md`](../ger-decomposition.md)).

If zero, the proxy hasn't injected a GER newer than the deposit's L1
block. Cross-reference with proxy logs around the deposit's timestamp.

### 4.3 Has the CLAIM landed on L2?

```sql
-- against the proxy DB
SELECT global_index, created_at FROM claimed_indices ORDER BY created_at DESC LIMIT 20;
```

Compute the expected `global_index` from `(network_id, deposit_cnt)`:

```
global_index = (1 << 64) | (network_id << 32) | deposit_cnt
```

(`<TODO: confirm exact bit layout against
src/service_send_raw_txn.rs claim-flow code — the postmortem shows
`global_index=18446744073710679244` which decodes to network/cnt values
that should match this formula>`.)

If the global_index appears in `claimed_indices`, the CLAIM was
processed. To verify the on-chain receipt:

```sql
SELECT tx_hash, status, signer, block_number, miden_tx_id
FROM transactions
WHERE tx_hash IN (
  SELECT transaction_hash FROM synthetic_logs
  WHERE topics[1] = '0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d'
  ORDER BY block_number DESC LIMIT 20
);
```

(`0x1df3f2a9...` is the `ClaimEvent` topic — also exported by
`miden/bali-l2-status.sh`.)

### 4.4 Did the user's balance actually change?

Miden balance inspection from outside the pod requires miden-cli, which
is out of scope for this doc — see the accounts table above. As a proxy
check, look up the `address_mappings` table:

```sql
SELECT * FROM address_mappings WHERE lower(eth_address) = lower('<dest_address>');
```

If the row is missing, the proxy zero-padded the destination
(`address_mapper_zero_padding_fallback_total` should have incremented at
claim time) — the user's balance increased on a synthesised Miden
account they don't control. **Page Max** before any remediation.

## 5. Trace a single L2→L1 withdrawal

You need the user's `BridgeEvent` log block + log_index, or the
`deposit_count` reported by the L2 proxy.

### 5.1 Did the proxy see the B2AGG note?

```sql
SELECT * FROM bridge_out_processed WHERE deposit_count = <n>;
```

If absent, `BridgeOutScanner` (`src/bridge_out.rs`) hasn't processed it.
Possible causes:

- Note's faucet missing from registry — `bridge_out_unknown_faucet_total`
  incremented. The note is quarantined by design.
- Note destination is invalid (zero address / EVM precompile range) —
  `bridge_out_invalid_destination_total` incremented. Refused.
- Self-targeted destination_network — `bridge_out_self_targeted_total`
  incremented. **Hard fault** — page Max.

### 5.2 Did the synthetic `BridgeEvent` log emit?

Using the running JSON-RPC (default port-forward 8546):

```bash
# Topic hash for BridgeEvent
TOPIC=0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b

curl -sS http://localhost:8546 \
  -H 'content-type: application/json' \
  --data "{
    \"jsonrpc\": \"2.0\", \"id\": 1,
    \"method\": \"eth_getLogs\",
    \"params\": [{
      \"fromBlock\": \"0x0\",
      \"toBlock\":   \"latest\",
      \"topics\":    [\"$TOPIC\"]
    }]
  }" | jq '.result | length'
```

For the at-a-glance dump (all three event types in one pass), use
`miden/bali-l2-status.sh` from the local checkout.

### 5.3 Did aggsender pick the event up + build a certificate?

`<TODO: confirm aggsender log signatures + cert lifecycle queries
against the bali aggsender setup. The aggsender pod and its REST API
URL need to land in this section.>`

### 5.4 Did ClaimSettler claim on L1?

ClaimSettler logs at INFO when it submits an L1 claim:

```logql
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "ClaimSettler.*submitted|ClaimSettler.*claimed"
```

If no log, check:

```bash
# Is ClaimSettler enabled?
kubectl -n outpost-testnet-miden-testnet describe pod miden-agglayer-0 \
  | grep -E 'CLAIM_SETTLER_ENABLED|CLAIM_SETTLER_WATCH_ADDRESSES'

# Does the signer have ETH?
SIGNER=$(kubectl logs miden-agglayer-0 -n outpost-testnet-miden-testnet \
  | grep -m1 'ClaimSettler: signing as' | grep -oE '0x[0-9a-fA-F]{40}')
cast balance "$SIGNER" --rpc-url "$SEPOLIA_RPC_URL"
```

## 6. Proxy DB snapshot — health overview

Run the queries from
[`../../.claude/skills/miden-bali-debug/queries/01-proxy-health.sql`](../../.claude/skills/miden-bali-debug/queries/01-proxy-health.sql).
Sanity checks on the output:

- `total - injected` should be a small handful (a few in-flight GERs
  waiting for Miden commit). A large gap = aggoracle is pushing faster
  than Miden is committing, or every Miden submission is failing — see
  log signals in section 3.
- `injected AND mainnet_exit_root IS NULL` = STATE-C orphans. Pre-RD-862
  legacy on bali (~27 historic). Should not be growing on a
  `UseUpdateExitRoot=true` cluster — if it is, the aggkit config flag
  isn't actually enabled.
- `service_state.latest_block_number` should equal or be one ahead of
  the maximum `ger_entries.block_number`. A larger gap means the proxy
  is producing synthetic blocks (every 12s) but not landing GERs.
- `transactions.status` distribution — `success` should dominate;
  sustained `failed` rows are the leading indicator of failure mode A.

## 7. Bridge-service DB snapshot

Run [`../../.claude/skills/miden-bali-debug/queries/02-bridge-state.sql`](../../.claude/skills/miden-bali-debug/queries/02-bridge-state.sql).

Cross-cluster invariants:

- `stuck` count should be flat or shrinking. Growing = either proxy
  isn't emitting GERs (failure mode E) or bridge-service isn't ingesting
  them (sync gap).
- `max(sync.exit_root.id)` for the rollup network should advance every
  time the proxy injects a new GER. Plateaued = bridge-service has lost
  L2 sync.
- `matchable` count (rollup-side GER rows that join `mt.root[n=0]`)
  should equal the total rollup-side count, or the gap should match the
  number of unresolved GERs on the proxy. Larger gap = GERs that
  bridge-service has but cannot validate; investigate per-row.

## 8. Single-GER deep dive

Given a GER hex, the full lifecycle in 4 lookups:

```sql
-- 1) Proxy: did we store it? With which roots? Injected?
SELECT encode(ger_hash,'hex'), encode(mainnet_exit_root,'hex'),
       encode(rollup_exit_root,'hex'), is_injected, block_number,
       to_timestamp(timestamp)
FROM ger_entries WHERE ger_hash = decode('<hex>', 'hex');

-- 2) Bridge-service: did it ingest the synthetic event?
SELECT id, block_id, network_id,
       encode(exit_roots[1],'hex') AS m,
       encode(exit_roots[2],'hex') AS r
FROM sync.exit_root WHERE global_exit_root = decode('<hex>', 'hex');

-- 3) Proxy logs: lifecycle
--    {namespace="...", container="miden-agglayer"} |~ "<ger-hex>"

-- 4) Miden-side: was the UpdateGerNote actually committed?
SELECT tx_hash, miden_tx_id, status, block_number
FROM transactions
WHERE tx_hash IN (
  SELECT transaction_hash FROM synthetic_logs
  WHERE topics[1] = '0xda61aa7823fcd807e37b95aabcbe17f03a6f3efd514176444dae191d27fd66b3'
    AND topics[2] = '<ger-hex-32-byte-padded>'
);
```

(`0xda61aa78...` is the `UpdateL1InfoTree` topic, mirrored by the proxy
as the GER injection synthetic log.)

## 9. When to bring in higher-level help

Hand off — capture the snapshot first — when any of:

- Cantina hard-page metric increments (runbook D).
- ClaimSettler submitted but `cast receipt` shows the L1 claim reverted
  (`status: 0`) — implies on-chain bridge-state corruption.
- bridge_accounts.toml differs between the running pod and what version
  control has — implies the recovery flow ran `--init` accidentally.
- `address_mapper_zero_padding_fallback_total` rate spikes during a
  high-volume deposit window — implies a user is sending to an unmapped
  destination and may not be able to spend the resulting wrapped asset.
- Aggsender unable to build certificates against an otherwise-healthy
  proxy — issue is in aggkit / agglayer rather than here.

For all of these, post the snapshot block from
`miden-bali-debug`'s output format (skill docs §"Output format") into
the incident ticket.
