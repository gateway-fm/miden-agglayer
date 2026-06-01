#!/usr/bin/env bash
# One-shot GER injection — clears a stuck L1→L2 backlog.
#
# Reads the CURRENT (mainnetExitRoot, rollupExitRoot) pair from the L1
# PolygonZkEVMGlobalExitRoot contract and submits one
#   updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot)
# to the miden-agglayer JSON-RPC. The proxy stores BOTH roots from the call
# parameters directly (no L1 refetch, so no race), computes
# combined = keccak(M ‖ R), inserts one UpdateGerNote on Miden, emits one
# UpdateHashChainValue synthetic log. bridge-service's L2 sync picks it up
# on its next ~2s chunk and flips ready_for_claim=true for every pending
# L1→L2 deposit whose L1InfoTreeIndex is at or below the new GER's index.
#
# WHY updateExitRoot AND NOT insertGlobalExitRoot
# ───────────────────────────────────────────────
# `insertGlobalExitRoot(bytes32 combined)` only carries the hashed pair.
# The proxy MUST then re-read `lastMainnetExitRoot()` / `lastRollupExitRoot()`
# from L1 itself to recover (M, R), and check keccak(M||R) == combined.
# Between our cast call and the proxy's refetch, Sepolia almost always
# advances under deposit load → mismatch → roots stored as (None, None)
# → bridge-service can't resolve the GER → the combined hash gets
# dedup-flagged and the very re-run we're trying does nothing.
# This was measured at 92.9% orphan rate (see RD-862, commit 9e5b095).
#
# `updateExitRoot(R, M)` ships both roots in the calldata. The proxy
# stores exactly what we sent — no refetch, no race.
#
# PARAMETER ORDER IS UNINTUITIVE: rollup FIRST, mainnet SECOND. See
# `src/ger.rs:61` and the upstream Solidity at
# GlobalExitRootManagerL2SovereignChain.sol#L131.
#
# Requires:
#   - cast (foundry) on PATH
#   - L1 RPC must expose lastMainnetExitRoot() / lastRollupExitRoot() view
#   - signer key must be in proxy's ALLOWED_SIGNERS if that allow-list is set
#     (typically aggoracle's key, the only one survives `--allowed-signers`)
#
# Env:
#   L1_RPC_URL       Sepolia / anvil L1 RPC (required)
#   L1_GER_ADDRESS   L1 PolygonZkEVMGlobalExitRoot contract (required)
#   L2_RPC_URL       miden-agglayer JSON-RPC URL (required)
#   SIGNER_KEY       hex private key (required)
#   L2_GER_ADDRESS   target of the cast send (default: synthetic emitter
#                    0xa40D...8fA — the proxy routes by selector, not addr)
#   L2_CHAIN_ID      proxy chain id (default: queried via eth_chainId)
#   GAS_PRICE_WEI    legacy tx gas price (default 1 gwei, matches proxy)
set -euo pipefail

: "${L1_RPC_URL:?L1 RPC URL (e.g. https://ethereum-sepolia-rpc.publicnode.com or http://localhost:8545)}"
: "${L1_GER_ADDRESS:?L1 PolygonZkEVMGlobalExitRoot address}"
: "${L2_RPC_URL:?miden-agglayer JSON-RPC URL}"
: "${SIGNER_KEY:?hex private key for an ALLOWED_SIGNERS-permitted signer}"

L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA}"

command -v cast >/dev/null || { echo "error: cast (foundry) not found" >&2; exit 1; }

echo "→ Reading current (mainnet, rollup) pair from L1 GER at $L1_GER_ADDRESS..."
MAIN=$(cast call "$L1_GER_ADDRESS" "lastMainnetExitRoot()(bytes32)" --rpc-url "$L1_RPC_URL")
ROLL=$(cast call "$L1_GER_ADDRESS" "lastRollupExitRoot()(bytes32)"  --rpc-url "$L1_RPC_URL")
COMB=$(cast keccak "$(cast concat-hex "$MAIN" "$ROLL")")
printf "   mainnet     %s\n   rollup      %s\n   combined    %s  (informational; not sent)\n" \
  "$MAIN" "$ROLL" "$COMB"

SIGNER=$(cast wallet address --private-key "$SIGNER_KEY")
NONCE_HEX=$(cast rpc eth_getTransactionCount "$SIGNER" "latest" --rpc-url "$L2_RPC_URL" | tr -d '"')
CHAIN_HEX=$(cast rpc eth_chainId --rpc-url "$L2_RPC_URL" | tr -d '"')
CHAIN_DEC=$((CHAIN_HEX))
NONCE_DEC=$((NONCE_HEX))
: "${L2_CHAIN_ID:=$CHAIN_DEC}"
printf "→ Proxy state:\n   signer   %s\n   nonce    %s (= %s)\n   chainId  %s (= %s)\n" \
  "$SIGNER" "$NONCE_HEX" "$NONCE_DEC" "$CHAIN_HEX" "$CHAIN_DEC"

if [[ "$L2_CHAIN_ID" != "$CHAIN_DEC" ]]; then
  echo "warning: L2_CHAIN_ID=$L2_CHAIN_ID overrides proxy-reported $CHAIN_DEC" >&2
fi

# Param order: rollup FIRST, mainnet SECOND.
# Solidity signature: updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot)
echo "→ Submitting updateExitRoot(rollup=$ROLL, mainnet=$MAIN) to $L2_GER_ADDRESS via $L2_RPC_URL..."
# Legacy (type-0) tx: the proxy implements eth_gasPrice but NOT eth_feeHistory,
# so EIP-1559 fee discovery in `cast send` fails. --legacy + an explicit
# gas price matches what the proxy's eth_gasPrice returns (1 gwei, see
# src/service.rs:382). Adjust GAS_PRICE_WEI in env if your deployment differs.
GAS_PRICE_WEI="${GAS_PRICE_WEI:-1000000000}"
cast send "$L2_GER_ADDRESS" "updateExitRoot(bytes32,bytes32)" "$ROLL" "$MAIN" \
  --rpc-url "$L2_RPC_URL" \
  --chain "$L2_CHAIN_ID" \
  --private-key "$SIGNER_KEY" \
  --legacy \
  --gas-price "$GAS_PRICE_WEI"

echo "→ Done. bridge-service should flip ready_for_claim for all pending deposits within ~2-3s (its L2 SyncInterval)."
echo "   If 'GER already seen, skipping duplicate' appears in proxy logs, this exact (M, R) pair was"
echo "   previously injected (likely from a poisoned insertGlobalExitRoot run). Wait ~30s for Sepolia"
echo "   to advance its exit roots, then re-run — the new pair will produce a fresh combined hash."
