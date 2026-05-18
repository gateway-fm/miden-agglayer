# Hypothesis — Bali L1→L2 backlog: race-poisoned GERs, one-shot recovers them

> Drafted 2026-05-18 UTC after local end-to-end repro + fix verification.
> Two Miden-destined deposits stuck on bali: `1130654` (marti), `1131034`.

## The bug

The pre-RD-862 proxy on bali handles `insertGlobalExitRoot(bytes32 combined)` by re-reading `lastMainnetExitRoot()` / `lastRollupExitRoot()` from L1 itself and checking `keccak(M ‖ R) == combined`. **Sepolia almost always advances between aggoracle's read and the proxy's refetch** under deposit load — the keccak check fails, the row is stored as `(mainnet=NULL, rollup=NULL)` but `is_injected=TRUE`, and bridge-service's `zkevm_getExitRootsByGER` can never resolve. `lastL1InfoTreeIndex` is frozen, all subsequent deposits get `ready_for_claim=false`, and the `is_injected=TRUE` flag dedup-poisons all retries with the same combined hash. **RD-862 baseline measured this at 92.9 % orphan rate.**

The previous runbook's local "verification" was misleading: anvil doesn't advance between read and refetch, so the race always wins locally.

## Falsifiable prediction

> A single `updateExitRoot(rollup, mainnet)` call to the bali miden-agglayer JSON-RPC — both roots in calldata, no L1 refetch, no race — will flip both stuck Miden deposits (`1130654`, `1131034`) from `ready_for_claim=false` to `true` within ~5 s.

## Why `updateExitRoot`, not `insertGlobalExitRoot`

| | `insertGlobalExitRoot(combined)` | `updateExitRoot(rollup, mainnet)` |
|---|---|---|
| Calldata | hashed pair only | both roots verbatim |
| Proxy re-reads L1? | **yes** (the race) | **no** |
| Production race rate | 92.9 % orphan | 0 % |
| On mismatch | stores `(NULL, NULL)` + dedup flag | n/a |

Param order is **rollup first, mainnet second** (`src/ger.rs:61`).

## Local end-to-end repro (already done)

`scripts/e2e-recover-l1-to-l2.sh` (full log: `docs/repro-evidence-20260518-1001.txt`):

- **Part A — bug**: stop aggkit, make 3 deposits, submit `insertGlobalExitRoot(GARBAGE)`. Postgres row: `(mainnet=NULL, rollup=NULL, is_injected=t)`. All 3 deposits stay `ready_for_claim=false`. ✅ poisoned exactly like bali.
- **Part B — fix**: run `scripts/one-shot-ger-inject.sh` (new — uses `updateExitRoot`). Postgres row now has the real `(M, R)` from calldata. All 3 deposits flip `ready_for_claim=true` within seconds. ✅

## Proof on bali (one command)

```bash
kubectl -n outpost-testnet-miden-testnet port-forward svc/miden-agglayer 8546:8546 &

L1_RPC_URL="https://ethereum-sepolia-rpc.publicnode.com" \
L1_GER_ADDRESS="0x2968d6d736178f8fe7393cc33c87f29d9c287e78" \
L2_RPC_URL="http://localhost:8546" \
L2_CHAIN_ID=1259691107 \
SIGNER_KEY="0x<aggoracle key — only key on ALLOWED_SIGNERS>" \
./scripts/one-shot-ger-inject.sh
```

Pass/fail check (no kubeconfig needed):

```bash
for cnt in 1130654 1131034; do
  curl -s "https://miden-testnet-bridge.dev.eu-north-3.gateway.fm/api/bridge?net_id=0&deposit_cnt=$cnt" \
    | jq -c '.deposit | {cnt:.deposit_cnt, ready:.ready_for_claim}'
done
```

- **PASS** — both `ready:true` within 30 s → hypothesis confirmed, both Miden claimants can `claimAsset`.
- **FAIL** — either still `ready:false` after 60 s and proxy logs show `IncorrectAccountInitialCommitment` → secondary failure (proxy's miden-client sqlite is stale). Escalate via `--reset-miden-store --restore`, restart proxy, re-run. Documented in `docs/RECOVER_L1_TO_L2_BACKLOG.md` § Troubleshooting.
- **FAIL** — either still `ready:false` and proxy logs show `GER already seen, skipping duplicate` → the current `(M, R)` was previously poisoned. Wait ~30 s for Sepolia to advance, re-run.

## Permanent fix (separate from the one-shot)

Deploy current `main` to bali (PR #41, commit `9e5b095`). Its `L1InfoTreeIndexer` watches L1 events directly and `set_ger_exit_roots` UPSERTs the roots, **eliminating the race for all future GERs and auto-healing previously-poisoned rows**. The one-shot is the emergency tool; the indexer is the durable fix.

## Blast radius / safety
- Idempotent at the GER level (`insert_ger` dedupes on combined hash, `src/ger.rs:118`).
- Touches no deposit state — only advances `lastL1InfoTreeIndex`. Claimants still submit `claimAsset`.
- Auth: signer must be in proxy's `ALLOWED_SIGNERS`. Only aggoracle's key qualifies.

## Deliverables (in this repo, untracked pending PR decision)
- `scripts/one-shot-ger-inject.sh` — new one-shot (uses `updateExitRoot`)
- `scripts/e2e-recover-l1-to-l2.sh` — deterministic bug repro + fix verifier
- `docs/RECOVER_L1_TO_L2_BACKLOG.md` — full runbook + troubleshooting
- `docs/RECOVER_L1_TO_L2_BACKLOG_DIFF.md` — annotated diff for reviewers
- `docs/repro-evidence-20260518-1001.txt` — captured E2E run output
- `~/Downloads/miden-l1-l2-recover-bali-2026-05-18.zip` (SHA-256 `40ff6662d3…`) — handoff bundle for Ivan Zubok
