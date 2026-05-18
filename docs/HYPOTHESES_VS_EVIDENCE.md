# Bali L1→L2 backlog — hypotheses vs. infrastructure evidence

Investigation date: 2026-05-18
Cluster: `dev-gateway-eks`, namespace `outpost-testnet-miden-testnet`
Method: read-only kubectl + read-only psql over port-forward
Authorisation: gateway infra debugging (no destructive ops attempted or authorised)

## Headline corrections to prior analysis

| Prior claim                                                             | Actual                                                                                                          |
|-------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------|
| Bali is pre-RD-862                                                     | Bali runs `gatewayfm/miden-agglayer:0.2.1` = `388775e` = **tip of `main`, RD-862 included**                       |
| ~3,400 deposits stuck                                                  | **1,131,706 deposits stuck** (essentially every L1→L2 ever); only 3 ever flipped `ready_for_claim`                |
| The race (`insertGlobalExitRoot` refetch) is the live problem          | The race is mostly *historic poison*; the live blocker is a different Miden-state failure (see below)            |
| One-shot `updateExitRoot` script will recover the backlog              | The script alone WON'T work — it also hits the live blocker. Miden-store reset is the precondition.              |

## Verdicts on Igor's hypotheses

| # | Igor's hypothesis                                              | Symptom predicted (Igor)                                | Bali reality                                                                                                   | Verdict             |
|---|----------------------------------------------------------------|---------------------------------------------------------|----------------------------------------------------------------------------------------------------------------|---------------------|
| 1 | Proxy "skipped" GER                                            | No note in Miden DB; no event in proxy `eth_getLogs`    | Currently TRUE for all GERs in last ~3 proxy-months: aggoracle pushes succeed at L1 but fail at proxy (see RC). | **PARTIALLY RIGHT** |
| 2 | Proxy submitted to Miden but didn't emit event                 | Miden note present; no event in proxy `eth_getLogs`     | Not observed. `commit_ger_event_atomic` is a single SERIALIZABLE txn (`src/store/postgres.rs:445-547`).         | **WRONG**           |
| 3 | Bridge "skipped" GER from proxy                                | Both Miden + proxy state OK; bridge didn't act          | Bridge IS picking up the 65 historic GERs that did succeed. The problem is the proxy hasn't produced any NEW GERs in ~3 months. | **PARTIALLY RIGHT** for the *historic* poisoned subset (27 of 65 GERs have NULL `(M, R)`), no for the live failure. |
| 4 | Something else                                                 | n/a                                                     | **The actual root cause is a 4th mode Igor's tree doesn't enumerate** — see RC below.                          | **THIS ONE**        |

## Root cause (live blocker, today)

**Every `insertGlobalExitRoot` aggoracle pushes is rejected at the proxy with:**

```
2026-05-18T14:19:20.795455Z INFO miden_agglayer_service::service: src/service.rs:396:
eth_sendRawTransaction: ERR account data wasn't found for account id 0xe9a21e616d9ed59016d481c7001393

2026-05-18T14:19:20.795473Z ERROR rpc::error: src/service_helpers.rs:85:
rpc error: account data wasn't found for account id 0xe9a21e616d9ed59016d481c7001393
```

The proxy's local `miden-client` sqlite store does not contain the account record for `0xe9a21e616d9ed59016d481c7001393` (likely the GER manager). Without it the proxy can't build/sign the `UpdateGerNote` Miden tx, so `eth_sendRawTransaction` returns an error to aggoracle BEFORE `insert_ger` runs. No Miden tx, no synthetic log, no `is_injected=TRUE`, no bridge-service advance.

This is a sibling of — but distinct from — the `IncorrectAccountInitialCommitment` Ivan saw earlier:
- `IncorrectAccountInitialCommitment` = account exists locally but its commitment is stale.
- `account data wasn't found` = account row missing entirely.

Both have the same recovery: `--reset-miden-store --restore`.

## Evidence — raw, citable

### Proxy pod (`miden-agglayer-0`)
- Image: `docker.io/gatewayfm/miden-agglayer:0.2.1` → commit `388775e` (current main, RD-862 included).
- Env: `GER_L1_ADDRESS`, `L1_RPC_URL`, `MIDEN_NODE_URL=rpc.testnet.miden.io:443` all set. RUST_LOG=debug.
- **Last terminated reason: `OOMKilled`**, exit 137. Restart count: 2. Memory limit 2Gi. Last OOM today 06:07Z.

### `L1InfoTreeIndexer` (RD-862)
- Alive, polling. Logs at 14:20Z show `from: 10874406, to: 10874406, head: 10874406` — caught up to L1 head, 1-block-per-poll cadence, no events found per cycle.
- **Critical caveat: cursor reinitialises to current L1 head on every restart, no backfill** (`src/l1_info_tree_indexer.rs:120-138`). Two OOMKills in 4 days → indexer has missed any L1 GER events emitted during those gaps. Any orphan GERs from before the indexer's current cursor are stranded.

### agglayer-store Postgres (`miden-agglayer-db`)
```
ger_entries: 741 total
             65 is_injected=TRUE
             27 of those 65 have mainnet_exit_root IS NULL  ← race-poisoned, stranded
             676 have (M, R) populated but is_injected=FALSE  ← indexer pre-populated, awaiting Miden submission
service_state.latest_block_number: 811102          (proxy current synthetic block)
service_state.log_counter: 68                     (4 non-GER synthetic logs + 65 GER + 1 ?)
transactions: 67 rows, ALL status=success         (no recent failures recorded — fails happen before write)
                                                  (latest tx block_number=98067; proxy is at 811102 — no successful tx in ~712k proxy blocks)
```

### bridge-db Postgres (`bridge-db`)
```
sync.deposit:
  network=0 (L1→L2), ready=FALSE:  1,131,706
  network=0 (L1→L2), ready=TRUE:           3   (cnt=1127628, 1127649, 1127650)
  network=73 (L2→L1), ready=TRUE:          1
marti's deposit (cnt=1130654): network=0, ready=FALSE, block_id=192629,
  dest_addr=0x000000007c2bce2e5f968f801d29d2a8226d9200

mt.root[network=0]:   1,131,709 rows, max deposit_id=1,131,852  (L1 InfoTree, fully indexed)
mt.root[network=73]:           1 row,  deposit_id=1,127,814     (only one L2→L1 bridge tx ever)

sync.exit_root[network=0]:  335,832 rows (L1 events bridge-service indexed directly)
sync.exit_root[network=73]:     65 rows (= proxy's UpdateHashChainValue events; one per ger_entries.is_injected=t)

Of the 65 n=73 exit_root rows:
  - ALL 65 have exit_roots[1] (mainnet) matching a mt.root[n=0] row. Max match deposit_id=1,127,887.
  - ZERO have exit_roots[2] (rollup) matching mt.root[n=73] (mt.root[n=73] only has 'a822866a…', none of the GERs do).
  - 17 carry the constant rollup 0x396ab55a… (= L1 lastRollupExitRoot at the time aggoracle read it).
```

### bridge-api logs
- Synchronizer is alive, scanning L2 blocks 811057+. Finds no new exit_root entries past id=332034. **The proxy isn't producing new GER events to ingest** (because every aggoracle push fails at the proxy, see RC).

### aggkit (`aggkit-0`)
- Container restarted 2 days ago. Healthy. Continues retrying `insertGlobalExitRoot`. Every push reaches the proxy and gets the "account data wasn't found" error → no state change.

## Why only 3 deposits ever flipped

- Bridge-service successfully advanced exactly once, to `deposit_cnt=1127650` (matching some early GER's M).
- After that point, either:
  - the rollup-root validation prevented subsequent advances (bridge-service implementation detail I can't fully confirm without zkevm-bridge source), OR
  - the L2 sync hit the moment when the proxy started failing all aggoracle pushes, and no subsequent GER ever cleared `is_injected=TRUE`.

The 65 successful is_injected GERs span proxy blocks 90,351 to 130,451. After block 130,451 (≈ 2024-01-19 02:50 in proxy-synthesized time, ≈ 3 proxy-months ago), zero GERs have been is_injected. That's the moment the Miden-store divergence began.

## Recovery plan (read-only verdict — no actions taken)

The new `updateExitRoot` one-shot script in `~/Downloads/miden-l1-l2-recover-bali-2026-05-18.zip` is necessary but **NOT SUFFICIENT** on its own. It will hit the same "account data wasn't found" error because it goes through the same `eth_sendRawTransaction` → `insert_ger` → Miden-submission path.

**Correct order, requires SRE to authorise the destructive step:**

1. **Restart the proxy with `--reset-miden-store --restore`** added to its Args. This:
   - Wipes `store.sqlite3` so the next startup re-fetches all accounts from `rpc.testnet.miden.io:443` (`src/main.rs:51-58`).
   - In the same boot, walks on-chain notes to rebuild the PgStore state (`src/main.rs:47-49`, `src/restore.rs:54`).
   - Keystore + `bridge_accounts.toml` preserved.
   - **Blast radius:** affects only this pod; the live Miden node is the source of truth and won't be touched. Failure mode: if reset takes longer than the readiness probe (60s startup), the pod will look unhealthy until restore completes — acceptable.
2. Wait for restore to log `bridge_outs_restored=… gers_restored=…` and the proxy to exit.
3. Remove the recovery flags and restart normally.
4. **Wait ~30s for aggoracle's next push** — should now succeed. ger_entries gets a fresh `is_injected=TRUE` row with (M, R) corresponding to current L1 state.
5. Bridge-service's L2 sync ingests the new event within 2s, the `<=` UPDATE Igor identified fires with M = `mt.root[n=0, deposit_id=~1,131,852]`, and **every stuck deposit at or below 1,131,852 flips `ready_for_claim=TRUE`** in one transaction.
6. Marti's deposit (cnt=1,130,654) is well below 1,131,852 → will flip.

**Open risk** independent of the immediate fix: the proxy's 2Gi memory limit and 2 OOMKills in 4 days suggests we'll be back here when the L1 indexer's history grows. Worth a bump to 4Gi before walking away, and adding an alert on OOMKills + a backfill subcommand for the indexer so a restart isn't an event-data-loss event.

## What I did NOT do (and why)

- **Did not run the one-shot script against bali.** It would hit the same error and either no-op or pollute log signals.
- **Did not run `--reset-miden-store --restore`.** Destructive (wipes local store) and would mask the live diagnosis. Wants SRE sign-off + a maintenance-window framing per org policy on irreversible state changes.
- **Did not modify any DB row** despite poisoned rows being SQL-fixable. Same reason.
- **Did not touch aggkit / agglayer / bridge configs.** Out of scope.
