# `one-shot-ger-inject.sh` — what changed and why

Audience: anyone reviewing the script before running it against bali.

## TL;DR of the change

The script now submits **`updateExitRoot(rollup, mainnet)`** instead of
**`insertGlobalExitRoot(combined)`**. Both selectors are accepted by the proxy.
The difference is what the proxy does next.

| Aspect                          | OLD (`insertGlobalExitRoot`)                                         | NEW (`updateExitRoot`)                                            |
|---------------------------------|----------------------------------------------------------------------|-------------------------------------------------------------------|
| Calldata payload                | `combined = keccak(M ‖ R)` only                                      | both `R` and `M` (32 bytes each)                                  |
| Proxy refetches L1?             | **Yes** — calls `lastMainnetExitRoot()` + `lastRollupExitRoot()`     | **No**                                                            |
| Verification check              | proxy recomputes `keccak(M', R')` from refetch, compares to combined | none — roots are taken verbatim from calldata                     |
| Outcome if L1 advanced mid-flight | mismatch → row stored as `(NULL, NULL)`, dedup flag set → poisoned | unaffected — what we sent is what gets stored                     |
| Production race rate            | 92.9 % orphan (RD-862 baseline)                                      | 0 %                                                               |
| Solidity signature              | `insertGlobalExitRoot(bytes32 root)`                                 | `updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot)` |

Both handlers are in `src/service_send_raw_txn.rs`:
- `insertGlobalExitRoot` — lines 498-548 (the racy refetch lives here)
- `updateExitRoot` — lines 549-576 (writes both roots from params directly)

## Annotated diff

```diff
-# Reads the CURRENT (mainnetExitRoot, rollupExitRoot) pair from the L1
-# PolygonZkEVMGlobalExitRoot contract, computes combined = keccak(M ‖ R),
-# and submits a single insertGlobalExitRoot(combined) to the miden-agglayer
-# JSON-RPC. The proxy injects one UpdateGerNote on Miden, emits one
-# UpdateHashChainValue synthetic log; bridge-service's L2 sync picks it
-# up on its next ~2s chunk and flips ready_for_claim=true for every
-# pending L1→L2 deposit whose L1InfoTreeIndex is at or below the current.
+# Reads the CURRENT (mainnetExitRoot, rollupExitRoot) pair from the L1
+# PolygonZkEVMGlobalExitRoot contract and submits one
+#   updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot)
+# to the miden-agglayer JSON-RPC. The proxy stores BOTH roots from the call
+# parameters directly (no L1 refetch, so no race), computes
+# combined = keccak(M ‖ R), inserts one UpdateGerNote on Miden, emits one
+# UpdateHashChainValue synthetic log. ...
+#
+# WHY updateExitRoot AND NOT insertGlobalExitRoot
+# ───────────────────────────────────────────────
+# `insertGlobalExitRoot(bytes32 combined)` only carries the hashed pair.
+# The proxy MUST then re-read `lastMainnetExitRoot()` / `lastRollupExitRoot()`
+# from L1 itself to recover (M, R) ... Between our cast call and the
+# proxy's refetch, Sepolia almost always advances under deposit load →
+# mismatch → roots stored as (None, None) ...
+# This was measured at 92.9% orphan rate (see RD-862, commit 9e5b095).
+#
+# `updateExitRoot(R, M)` ships both roots in the calldata. The proxy
+# stores exactly what we sent — no refetch, no race.
+#
+# PARAMETER ORDER IS UNINTUITIVE: rollup FIRST, mainnet SECOND. See
+# `src/ger.rs:61` and the upstream Solidity at
+# GlobalExitRootManagerL2SovereignChain.sol#L131.

  ...

   echo "→ Reading current (mainnet, rollup) pair from L1 GER at $L1_GER_ADDRESS..."
   MAIN=$(cast call "$L1_GER_ADDRESS" "lastMainnetExitRoot()(bytes32)" --rpc-url "$L1_RPC_URL")
   ROLL=$(cast call "$L1_GER_ADDRESS" "lastRollupExitRoot()(bytes32)"  --rpc-url "$L1_RPC_URL")
   COMB=$(cast keccak "$(cast concat-hex "$MAIN" "$ROLL")")
-  printf "   mainnet   %s\n   rollup    %s\n   combined  %s\n" "$MAIN" "$ROLL" "$COMB"
+  printf "   mainnet     %s\n   rollup      %s\n   combined    %s  (informational; not sent)\n" \
+    "$MAIN" "$ROLL" "$COMB"
```

The `cast call` block is unchanged: we still read `(M, R)` from L1.
What changes is what we send back to the proxy: instead of the 32-byte
hash, we send both 32-byte roots as separate parameters.

```diff
-echo "→ Submitting insertGlobalExitRoot($COMB) to $L2_GER_ADDRESS via $L2_RPC_URL..."
-# Legacy (type-0) tx: ...
-GAS_PRICE_WEI="${GAS_PRICE_WEI:-1000000000}"
-cast send "$L2_GER_ADDRESS" "insertGlobalExitRoot(bytes32)" "$COMB" \
-  --rpc-url "$L2_RPC_URL" \
-  --chain "$L2_CHAIN_ID" \
-  --private-key "$SIGNER_KEY" \
-  --legacy \
-  --gas-price "$GAS_PRICE_WEI"
+# Param order: rollup FIRST, mainnet SECOND.
+# Solidity signature: updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot)
+echo "→ Submitting updateExitRoot(rollup=$ROLL, mainnet=$MAIN) to $L2_GER_ADDRESS via $L2_RPC_URL..."
+# Legacy (type-0) tx: ...
+GAS_PRICE_WEI="${GAS_PRICE_WEI:-1000000000}"
+cast send "$L2_GER_ADDRESS" "updateExitRoot(bytes32,bytes32)" "$ROLL" "$MAIN" \
+  --rpc-url "$L2_RPC_URL" \
+  --chain "$L2_CHAIN_ID" \
+  --private-key "$SIGNER_KEY" \
+  --legacy \
+  --gas-price "$GAS_PRICE_WEI"
```

The only behavioural change in the wire-level call:

```
- selector  0x33d6247d  insertGlobalExitRoot(bytes32)
- payload   <combined>
+ selector  0x33616755  updateExitRoot(bytes32,bytes32)
+ payload   <rollupExitRoot> <mainnetExitRoot>     ← order matters
```

Everything else (legacy tx type, 1 gwei gas price, nonce/chainId resolution,
keystore key) is unchanged.

## Trailing diagnostic the script now prints

```diff
+echo "   If 'GER already seen, skipping duplicate' appears in proxy logs, this exact (M, R) pair was"
+echo "   previously injected (likely from a poisoned insertGlobalExitRoot run). Wait ~30s for Sepolia"
+echo "   to advance its exit roots, then re-run — the new pair will produce a fresh combined hash."
```

This is purely advisory output for the operator. It explains the dedup-poison
escape ("wait for the L1 GER to advance, re-run") inline so the runbook isn't
strictly required to interpret a `status 1 (success)` that nonetheless didn't
clear the backlog. The other failure mode the operator must watch for —
`IncorrectAccountInitialCommitment` in the proxy logs — is documented in the
runbook's Troubleshooting section (it surfaces in the *async* Miden tx, not in
the script's stdout, so a wrapper printout here would be misleading).

## What did NOT change

- The L1 reads (`cast call lastMainnetExitRoot()` / `lastRollupExitRoot()`).
- The signer / nonce / chainId resolution against the proxy.
- The default `L2_GER_ADDRESS` (`0xa40D…8fA`) and gas-price (1 gwei).
- The `--legacy` flag (proxy still doesn't implement `eth_feeHistory`).
- Idempotency: re-running with the same Sepolia state is still a no-op
  (`insert_ger` dedup on the combined hash, `src/ger.rs:118`).
