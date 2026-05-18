# Recover stuck L1→L2 deposits (one-shot)

**Symptom:** every L1→L2 deposit since `deposit_cnt=1127650` is stuck on the bali
bridge-service with `ready_for_claim=false` — including marti's
`deposit_cnt=1130654`.

## TL;DR

Run `scripts/one-shot-ger-inject.sh` (new — uses `updateExitRoot`). It submits
**both** the mainnet and rollup exit roots in the calldata, so the proxy stores
them directly from the call parameters; there is no L1 refetch on the proxy side
and therefore no race. bridge-service picks up the synthetic
`UpdateHashChainValue` log on its next ~2s L2 sync and flips
`ready_for_claim=true` for every backlogged deposit at or below the new GER's
L1InfoTreeIndex.

If the proxy fails the underlying Miden tx submission with
`IncorrectAccountInitialCommitment`, escalate via
`--reset-miden-store --restore` (full resync of the proxy's local miden-client
store) and re-run the script. See [Troubleshooting](#troubleshooting).

---

## What previously failed and why

A prior version of this runbook shipped a script that called
`insertGlobalExitRoot(bytes32 combined)`. That selector ships only the *hashed*
pair to the proxy, which then has to recover `(M, R)` from L1 by view-calling
`lastMainnetExitRoot()` / `lastRollupExitRoot()` and checking
`keccak(M ‖ R) == combined`. Two things go wrong with that:

1. **The race** (RD-862, commit `9e5b095`). Between the script's `cast call` on
   L1 and the proxy's refetch, Sepolia almost always advances. Baseline
   measurement on origin/main + stock aggkit 0.8.3-rc1 was **13 of 14 GERs
   orphaned across 3×N=30 runs — 92.9% orphan rate**
   (`tests/baselines/baseline-rd862-repro.json`). When the keccak check fails,
   the proxy stores the row as `(mainnet=NULL, rollup=NULL)` but still flips
   `is_injected=TRUE`. bridge-service's `zkevm_getExitRootsByGER` then returns
   nothing and the L1InfoTreeIndex never advances.
2. **Dedup poisoning.** Once `is_injected=TRUE` is set, `insert_ger`
   (`src/ger.rs:118`) treats the combined hash as already-handled. Retries with
   the same `combined` are silent no-ops — the runbook's "idempotent, just
   retry" claim was wrong.

The fix is to use the `updateExitRoot(bytes32 newRollupExitRoot, bytes32
newMainnetExitRoot)` selector instead. The proxy handler at
`src/service_send_raw_txn.rs:549-576` writes the roots verbatim from the call
parameters — no refetch, no race.

> **Param order is unintuitive: rollup FIRST, mainnet SECOND.** Confirm against
> `src/ger.rs:61` and the upstream Solidity at
> `GlobalExitRootManagerL2SovereignChain.sol#L131` before any manual cast send.

---

## Evidence — local end-to-end repro

Reproduced on `docker-compose.e2e.yml` via `scripts/e2e-recover-l1-to-l2.sh`.
Captured output: `docs/repro-evidence-20260518-1001.txt`.

### Part A — the bug

```
[10:00:43] A1. Stopping aggkit so deposits stack up without GER updates
[10:00:43] A2. bridgeAsset → deposit_cnt=4,5,6 (3 stuck deposits)
[10:00:46] A3. Poison a GER by submitting insertGlobalExitRoot(GARBAGE)
              GARBAGE = 0xdeadbeef...6a0ad4fb
              (proxy refetches (M,R), keccak ≠ GARBAGE, stores NULL roots)

[10:00:55] mainnet_exit_root = <NULL>
[10:00:55] rollup_exit_root  = <NULL>
[10:00:55] is_injected       = t
[10:00:55] PASS: BUG REPRODUCED: poisoned row has NULL roots AND is_injected=TRUE

[10:00:57] PASS: BUG CONFIRMED: synthetic log fired but bridge-service
                 can't resolve unmatched (M, R) — 3 deposits still stuck
```

### Part B — the fix

```
[10:00:57] B1. Running scripts/one-shot-ger-inject.sh with current L1 (M, R)
              → updateExitRoot(rollup=0x000…, mainnet=0x1d394d7a…)
              status: 1 (success)

[10:01:07] B2. mainnet_exit_root = 0x1d394d7a0cc1b01abbf18113b7a1f4605c40925b...
              rollup_exit_root   = 0x0000000000000000000000000000000000000000...
              is_injected        = t
[10:01:07] PASS: FIX VERIFIED at proxy level: (M, R) stored from call params, no race

[10:01:07] B3. cnt=4 blk=223 ready=true  claimed=false
              cnt=5 blk=224 ready=true  claimed=false
              cnt=6 blk=225 ready=true  claimed=false
[10:01:07] PASS: FIX VERIFIED end-to-end: all 3 deposits now ready_for_claim
```

> **Note on the local repro vs. bali:** the local stack runs current `main`
> which ALSO includes the RD-862 `L1InfoTreeIndexer`. That indexer would
> auto-heal a real race-poisoned row by UPSERTing `(M, R)` within ~1s of
> polling L1, hiding the bug. To make the local repro deterministic, the test
> rig poisons a *garbage* combined hash that doesn't correspond to any L1
> `(M, R)` pair — the indexer never observes it and the row stays poisoned,
> exactly mirroring the bali state where no indexer exists at all.

---

## Prerequisites

- `cast` (foundry) on `PATH` — `curl -L https://foundry.paradigm.xyz | bash && foundryup`
- aggoracle's private key (the only signer that survives the proxy's
  `ALLOWED_SIGNERS` allow-list — decrypt `aggoracle.keystore` with the password
  from the secret store)
- The **internal** miden-agglayer JSON-RPC URL. The public
  `miden-agglayer.dev.eu-north-3.gateway.fm` returns 404 from the istio
  gateway — find the in-cluster Service URL (e.g.
  `http://miden-agglayer.<ns>.svc.cluster.local:8546`), or port-forward with
  `kubectl -n outpost-testnet-miden-testnet port-forward svc/miden-agglayer 8546:8546`
  then use `http://localhost:8546`.

---

## Step 1 — strongly recommended: deploy current `main` first

The script will recover the existing backlog on either build, but on pre-RD-862
NEW deposits keep landing in the same race. Current `main` ships the
`L1InfoTreeIndexer` (PR #41, commit `9e5b095`) which pre-populates the
`(M, R)` lookup by watching L1 events directly — eliminating the underlying
race for all future GERs **and** auto-healing any deposits whose combined hash
was previously stored as `(NULL, NULL)` (via `set_ger_exit_roots` UPSERT, which
overwrites the roots when the indexer observes the matching L1 event).

If a deploy is not possible right now: skip to Step 2. Recovery still works,
but new orphans will keep appearing until a deploy lands.

---

## Step 2 — run the one-shot

```bash
L1_RPC_URL="https://ethereum-sepolia-rpc.publicnode.com" \
L1_GER_ADDRESS="0x2968d6d736178f8fe7393cc33c87f29d9c287e78" \
L2_RPC_URL="<internal miden-agglayer RPC, e.g. http://localhost:8546 if port-forwarded>" \
L2_CHAIN_ID=1259691107 \
SIGNER_KEY="0x<aggoracle private key, hex, with 0x prefix>" \
./scripts/one-shot-ger-inject.sh
```

Expected output (last line should be `status  1 (success)`):

```
→ Reading current (mainnet, rollup) pair from L1 GER at 0x2968...e78...
   mainnet     0x<32 bytes>
   rollup      0x<32 bytes>
   combined    0x<32 bytes>  (informational; not sent)
→ Proxy state:
   signer   0x<aggoracle address>
   nonce    0x<n>
   chainId  0x4b16ec3   (= 1259691107)
→ Submitting updateExitRoot(rollup=0x..., mainnet=0x...) to 0xa40D...8fA via ...
   status              1 (success)
→ Done. bridge-service should flip ready_for_claim within ~2-3s.
```

---

## Step 3 — verify

```bash
# marti's deposit
curl -s "https://miden-testnet-bridge.dev.eu-north-3.gateway.fm/api/bridge?net_id=0&deposit_cnt=1130654" \
  | jq '.deposit | {cnt:.deposit_cnt, ready:.ready_for_claim, claimed:(.claim_tx_hash!="")}'
```

Expect within ~5 s: `ready: true`.

Spot-check a couple more in the backlog window `[1127651, 1131034]`:

```bash
for cnt in 1130654 1131034; do
  curl -s "https://miden-testnet-bridge.dev.eu-north-3.gateway.fm/api/bridge?net_id=0&deposit_cnt=$cnt" \
    | jq -c '{cnt:.deposit.deposit_cnt, ready:.deposit.ready_for_claim}'
done
```

Both should print `ready: true`.

---

## Troubleshooting

Tail the proxy logs while running the one-shot:

```bash
kubectl -n outpost-testnet-miden-testnet logs -f deploy/miden-agglayer \
  | grep -E 'GER|IncorrectAccountInitialCommitment|already seen|UpdateGerNote'
```

### `IncorrectAccountInitialCommitment` — proxy's miden-client store is stale

> Observed on 2026-05-??: the proxy proposes `0x105f4490…` while the live
> miden-node has `0x90d789d4…`. The `--unlock-miden-accounts` step clears the
> account lock but does NOT refresh the cached commitment.

The new script's `updateExitRoot` path still routes through `insert_ger`, which
submits an `UpdateGerNote` Miden transaction (`src/ger.rs:140-181`). If the
proxy's local miden-client sqlite has stale account state, that submission
rejects with `IncorrectAccountInitialCommitment` — the synthetic log never
emits and deposits stay stuck **even though the script itself reports
`status 1`** (the eth_sendRawTransaction succeeded; the failure is in the
asynchronous Miden tx).

**Escalation: full miden-client resync.**

1. Roll the proxy with these flags added to the entrypoint (terminates after
   restore completes; remove the flags and restart afterwards):

   ```
   --reset-miden-store --restore
   ```

   - `--reset-miden-store` wipes `store.sqlite3` + WAL/SHM so the next startup
     re-syncs every account commitment from the miden-node.
     (`src/main.rs:51-58`)
   - `--restore` rebuilds the proxy's PgStore state from on-chain notes in the
     same startup (`src/main.rs:47-49`, `src/restore.rs:54`).
   - Keystore and `bridge_accounts.toml` are preserved.

2. Wait for `restore` to log `bridge_outs_restored=… gers_restored=…` and exit.

3. Restart the proxy without the recovery flags.

4. Re-run `one-shot-ger-inject.sh`. The Miden submission should now succeed
   and the synthetic log emits within seconds.

### `GER already seen, skipping duplicate` in proxy logs

The current Sepolia `(M, R)` pair was previously injected — most likely by an
earlier failed run that poisoned the dedup. Two ways out:

- **Wait for Sepolia to advance.** Each new L1 GER event produces a different
  `(M, R)`, which keccaks to a fresh `combined` hash that has not been
  dedup-flagged. Bali Sepolia rolls every few blocks; ~30 s is usually
  enough. Re-run the script.
- **SQL fixup (only if waiting isn't viable).** Connect to the agglayer
  postgres and clear the poison for the specific combined hash:

  ```sql
  UPDATE ger_entries
     SET is_injected = FALSE
   WHERE ger_hash = decode('<combined hex without 0x>', 'hex')
     AND mainnet_exit_root IS NULL;
  ```

  Then re-run the script. **Only target rows where `mainnet_exit_root IS
  NULL`** — those are the poisoned ones. Do not flip `is_injected` on rows
  with non-NULL roots; that would re-trigger a Miden submission for an
  already-emitted GER.

### `signer … is not on the allow-list`

`SIGNER_KEY` is not in the proxy's `ALLOWED_SIGNERS` set. Re-export with the
aggoracle key (see `aggoracle.keystore` + decrypt password).

---

## Rollback / safety

- The script is idempotent at the GER level (`insert_ger` dedupes on the
  combined hash via `is_ger_injected`, `src/ger.rs:118`). Re-running with the
  same Sepolia `(M, R)` is a no-op — neither helpful nor harmful.
- It does not touch any deposit's claim state. It only advances
  `lastL1InfoTreeIndex` on the destination via the synthetic
  `UpdateHashChainValue` log. Claimants still need to submit their own
  `claimAsset` (or claimsponsor does it for them).
- It does not disturb aggoracle. If aggoracle's next push lands the same
  combined GER, dedup kicks in and aggoracle's call is a no-op.

---

## What to do *after* recovery

1. **Deploy current `main`** if not done in Step 1 — eliminates the
   underlying race for new deposits and unblocks any future stale-Miden
   recoveries that need the indexer's auto-heal.
2. **Add monitoring**:
   - Alert when `aggoracle_eth_sendRawTransaction_errors_total` rate climbs.
   - Alert when `ready_for_claim=false` deposit count is non-zero for > 5 min.
   - Alert on proxy logs containing `IncorrectAccountInitialCommitment`.
3. **Consider** a `miden-agglayer catch-up-gers --from-l1-block <N>`
   subcommand so future incidents self-heal on restart instead of needing a
   manual `cast send`.
