# GER Decomposition Issue: Zero Roots Poisoning Bridge-Service

## Problem Statement

When `insertGlobalExitRoot(bytes32 combinedGER)` arrives, our service must
decompose the combined hash back into `(mainnetExitRoot, rollupExitRoot)`.
This is fundamentally a one-way hash — the individual roots cannot be derived
from the combined GER alone. We resolve them by fetching the latest roots from
L1 and verifying `keccak256(mainnet || rollup) == combinedGER`.

**The race:** If L1 has already advanced to a newer GER by the time we call
`fetch_exit_roots()`, the latest roots won't match the GER being injected.
When this happens, we store `None` for both roots — and our
`zkevm_getExitRootsByGER` RPC endpoint was returning `0x000...000` instead
of `null`, which poisons the bridge-service permanently.

## Root Cause Chain

### 1. Our service returns fabricated zero roots

`src/service_zkevm.rs` lines 73-74 (before fix):
```rust
let mainnet = entry.mainnet_exit_root.unwrap_or([0u8; 32]);
let rollup = entry.rollup_exit_root.unwrap_or([0u8; 32]);
```

When roots are `None` (unresolved), we return `0x000...000` for both —
which is a valid-looking response, not "unknown".

### 2. Bridge-service blindly trusts non-nil responses

`synchronizer.go` `syncTrustedState()`:
```go
exitRoots, err := s.zkEVMClient.ExitRootsByGER(s.ctx, lastGER)
if exitRoots == nil { return nil }  // only checks nil, not zero

ger := &etherman.GlobalExitRoot{
    ExitRoots: []common.Hash{exitRoots.MainnetExitRoot, exitRoots.RollupExitRoot},
}
s.storage.AddTrustedGlobalExitRoot(s.ctx, ger, nil)
```

A non-nil response with zero roots is treated as valid data.

### 3. First write wins — zero roots are permanent

`pgstorage.go` `AddTrustedGlobalExitRoot()`:
```go
INSERT INTO sync.exit_root (...) VALUES (...)
ON CONFLICT ON CONSTRAINT UC DO NOTHING;
```

Once zero roots are stored, the `DO NOTHING` prevents any future correction.
Even if we later resolve the real roots, bridge-service will never re-query.

### 4. Downstream: claims fail permanently

When ClaimTxManager tries to build a Merkle proof using the zero roots,
`getRollupExitProof()` performs root verification:
```go
if root != r {
    return nil, common.Hash{}, fmt.Errorf("error checking calculated root...")
}
```

Zero roots fail verification. Any deposit claimable under that GER is stuck.

## Severity Assessment

**Moderate, not critical.** Two mitigating factors:

1. **Bridge-service uses latest trusted GER for claims**, not arbitrary old
   GERs. `synchronizer.go:648-660` calls `GetLatestTrustedExitRoot()` when
   signaling ClaimTxManager. A newer, correctly-resolved GER supersedes the
   poisoned one for most deposits.

2. **The race window is narrow.** The aggoracle reads the latest GER from L1
   and immediately calls `insertGlobalExitRoot`. Our service fetches L1 roots
   in the same handler. The GER is typically still current on L1.

**However:** Any deposit that can only be claimed under the specific poisoned
GER (no newer GER covers it) is permanently stuck until the bridge-service
database is manually patched.

## Fix: Return `null` When Roots Are Unresolved (Option A)

**Change:** When `mainnet_exit_root` or `rollup_exit_root` is `None`, return
`null` from `zkevm_getExitRootsByGER` instead of a response with zero roots.

Bridge-service already handles `null` correctly:
```go
if exitRoots == nil {
    log.Debugf("skipping exitRoots because there is no result")
    return nil
}
```

This means bridge-service will retry on the next sync cycle. By then, either:
- The same GER is still current on L1 → our lazy resolution succeeds
- A newer GER has arrived → supersedes the unresolved one

No permanent poisoning in either case.

## Long-Term Fix: Eliminate the Decomposition Problem

Modify the aggoracle's Miden `ChainSender` to call `updateExitRoot(bytes32
mainnet, bytes32 rollup)` instead of `insertGlobalExitRoot(bytes32 combined)`.
The aggoracle already has both roots available in its `L1InfoTreeLeaf` — it
just discards them before forwarding.

See: `agglayer/aggkit` repo, `aggoracle/chaingersender/evm.go`

The `ChainSender` interface would need a new method (e.g.
`InjectGERWithRoots`) or the Miden sender can internally call
`L1InfoTreeSync.GetInfoByGlobalExitRoot(ger)` to look up the individual
roots before forwarding.

## Test Coverage

Unit tests in `src/service_zkevm.rs`:
- `test_exit_roots_returns_null_when_roots_unresolved` — proves Option A works
- `test_exit_roots_returns_roots_when_resolved` — proves resolved GERs still work
- `test_exit_roots_returns_null_for_unknown_ger` — proves unknown GERs return null
- `test_exit_roots_lazy_resolves_from_l1` — proves lazy L1 resolution path works
