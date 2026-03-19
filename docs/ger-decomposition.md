# GER Decomposition: Analysis and Resolution

## Background

The aggoracle injects Global Exit Roots (GERs) into sovereign chains by calling
`insertGlobalExitRoot(bytes32 combinedGER)`. The combined GER is a one-way
Keccak256 hash of two components:

```
combinedGER = keccak256(mainnetExitRoot || rollupExitRoot)
```

Bridge-service needs the individual `(mainnetExitRoot, rollupExitRoot)` pair to
build Merkle proofs for claims. It fetches them via our `zkevm_getExitRootsByGER`
RPC endpoint.

## The Problem We Investigated

We identified a potential data quality issue: when `insertGlobalExitRoot` arrives
and L1 has already advanced to a newer GER, our service cannot decompose the
combined hash back to individual roots. We resolve roots by fetching the latest
pair from L1 and verifying `keccak256(mainnet || rollup) == combinedGER`. If L1
has moved on, the verification fails and roots are stored as `None`.

Our `zkevm_getExitRootsByGER` endpoint was returning fabricated zero roots
(`0x000...000`) when the actual roots were unknown. This could poison
bridge-service's database because:

1. Bridge-service's `syncTrustedState()` treats any non-nil response as valid
2. `AddTrustedGlobalExitRoot()` uses `ON CONFLICT DO NOTHING` — first write wins
3. Zero roots would fail Merkle proof verification in `getRollupExitProof()`

## Why This Is Not a Functional Issue

After investigating bridge-service's actual sync behaviour
([v0.6.4-RC2](https://github.com/0xPolygon/zkevm-bridge-service/tree/v0.6.4-RC2)),
we determined this is a **data quality bug**, not a functional one:

### Bridge-service never misses a GER

Bridge-service discovers GERs via two independent paths:

1. **L2 block sync** (`eth_getLogs` by block range) — syncs all events
   sequentially. Every GER we emit is seen, regardless of root resolution.
2. **Trusted state sync** (`syncTrustedState()`) — only checks the **latest**
   GER via `zkevm_getLatestGlobalExitRoot()`, then resolves its roots.

### Newer GERs always supersede

Exit trees are append-only. A newer GER covers all deposits that older GERs
covered. Bridge-service uses `GetLatestTrustedExitRoot()` when signaling
ClaimTxManager for claims (`synchronizer.go:648-660`), not "the exact GER I
just saw." So a correctly-resolved newer GER supersedes any unresolved older one.

### The race window is narrow

The aggoracle reads the latest GER from L1 and immediately calls
`insertGlobalExitRoot`. Our service fetches L1 roots in the same handler. The
GER is almost always still current on L1 at resolution time.

## What We Fixed

### Return `null` instead of zero roots (commit `471cb21`)

`src/service_zkevm.rs` — when `mainnet_exit_root` or `rollup_exit_root` is
`None`, return JSON `null` instead of a response with zero roots:

```rust
match (entry.mainnet_exit_root, entry.rollup_exit_root) {
    (Some(mainnet), Some(rollup)) => Ok(JsonRpcResponse::success(answer_id, ...)),
    _ => Ok(JsonRpcResponse::success(answer_id, serde_json::Value::Null)),
}
```

Bridge-service handles `null` correctly — it skips and retries next cycle:

```go
if exitRoots == nil {
    log.Debugf("skipping exitRoots because there is no result")
    return nil
}
```

### Lazy resolution on query

When `zkevm_getExitRootsByGER` is called for a GER with missing roots, we
attempt to resolve them from L1 on-the-fly. If L1's current roots match the
GER, we persist them and return the resolved data. This handles the common case
where the roots were unresolved at injection time but L1 hasn't moved on yet.

### Removed L1 backward log scanning (commit `531252f`)

Previously, `find_l1_exit_roots_by_ger()` scanned backward through L1 in
2000-block windows looking for `UpdateL1InfoTree` events. This was:

- **Slow**: Unbounded RPC calls, scanning to block 0
- **Unreliable**: Depends on unpruned/archive L1 nodes
- **Unnecessary**: The GER is almost always the latest on L1

Replaced with a simple `fetch_exit_roots()` + verify against the latest L1
roots. If they don't match, roots remain `None` and are returned as `null`.

## Root Cause Chain (for reference)

For the zero-root poisoning path that we eliminated:

```
1. insertGlobalExitRoot(GER) arrives
2. fetch_exit_roots() from L1 → roots don't match (L1 advanced)
3. Store roots as None
4. Bridge-service calls zkevm_getExitRootsByGER(GER)
5. OLD: Return {mainnetExitRoot: "0x000...", rollupExitRoot: "0x000..."}
   NEW: Return null
6. OLD: Bridge-service stores zeros via ON CONFLICT DO NOTHING (permanent)
   NEW: Bridge-service skips, retries next cycle
7. OLD: Merkle proof fails forever for claims under this GER
   NEW: Next cycle resolves (same GER still current) or superseded (newer GER)
```

## Long-Term Improvement: Aggoracle ChainSender

The decomposition problem exists because the aggoracle sends only the combined
hash. The permanent solution is to modify the Miden `ChainSender` in
[agglayer/aggkit](https://github.com/agglayer/aggkit) to send both roots:

- **Current**: `insertGlobalExitRoot(bytes32 combinedGER)` — one-way hash,
  roots lost
- **Target**: `updateExitRoot(bytes32 mainnet, bytes32 rollup)` — both roots
  preserved

The aggoracle already has both roots in its `L1InfoTreeLeaf` struct
(`aggoracle/chaingersender/evm.go`). Our service already handles the two-root
form via `updateExitRoot()` in `service_send_raw_txn.rs`.

This would eliminate the decomposition problem entirely — no L1 fetch, no race
condition, no null responses.

## Test Coverage

### Unit tests (`src/service_zkevm.rs`)

| Test | Scenario |
|------|----------|
| `test_exit_roots_returns_null_when_roots_unresolved` | GER exists, roots None → null |
| `test_exit_roots_returns_roots_when_resolved` | GER exists, roots present → data |
| `test_exit_roots_returns_null_for_unknown_ger` | GER not in store → null |
| `test_exit_roots_lazy_resolves_from_l1` | Roots None, L1 matches → resolved + persisted |
| `test_exit_roots_returns_null_when_l1_stale` | Roots None, L1 moved on → null |

### E2E test (`scripts/e2e-ger-decomposition.sh`)

Runs against the full docker-compose stack with real PostgreSQL:

1. Verifies resolved GER returns exit root data
2. Inserts fake GER with NULL roots into postgres → verifies `null` response
3. Verifies unknown GER returns `null`
4. Verifies partially resolved GER (one root missing) returns `null`

Run with: `make e2e-ger-decomposition`
