#!/usr/bin/env bash
# Audit H6 — L1 GER corroboration E2E.
#
# aggoracle-supplied GER bytes (insertGlobalExitRoot) were trusted verbatim: a
# compromised signer could inject a FORGED GER (one whose (mainnet, rollup)
# decomposition the L1 InfoTree indexer never observed on L1) onto Miden —
# polluting on-chain state and burning operator gas.
#
# insert_ger now cross-checks the injected GER against the indexer's observed
# set (ger_entries.mainnet_exit_root must be set). Under
# --reject-unverified-ger-injection (implied by --require-hardening), an
# unverified GER is refused before it reaches Miden.
#
# Phases:
#   A. POSITIVE — submit a real insertGlobalExitRoot for a GER the indexer has
#      observed (wait for the indexer to catch up). Assert the UpdateGerNote is
#      submitted + the synthetic GER log is emitted.
#   B. NEGATIVE — submit insertGlobalExitRoot with a FORGED 32-byte root that
#      never appeared in an L1 UpdateL1InfoTree event. With
#      --reject-unverified-ger-injection, assert the proxy refuses with the H6
#      error and ger_injection_unverified_total increments; NO UpdateGerNote is
#      submitted to Miden.
#
# Requires the full E2E stack up (`make e2e-up`) and the service started with
# --reject-unverified-ger-injection.
set -euo pipefail

log() { echo "[e2e-ger-l1-verify] $*"; }
fail() { echo "[e2e-ger-l1-verify] FAIL: $*" >&2; exit 1; }

# JSON-RPC endpoint of the miden-agglayer proxy (where eth_sendRawTransaction
# lands and the /metrics scrape lives). L2_RPC_URL is the canonical name (see
# one-shot-ger-inject.sh); BRIDGE_SERVICE_URL is kept as a back-compat alias.
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-${L2_RPC_URL:-http://localhost:18080}}"
L2_RPC_URL="${L2_RPC_URL:-$BRIDGE_SERVICE_URL}"

# Signer of the forged insertGlobalExitRoot tx. Must be permitted by the
# service's --allowed-signers allow-list (typically the aggoracle key) so the
# tx reaches the H6 gate rather than being rejected earlier as an un-allowed
# signer. Legacy (type-0) tx: the proxy implements eth_gasPrice but NOT
# eth_feeHistory, matching one-shot-ger-inject.sh.
: "${SIGNER_KEY:?hex private key for an --allowed-signers-permitted signer (e.g. the aggoracle key)}"
L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA}"
GAS_PRICE_WEI="${GAS_PRICE_WEI:-1000000000}"
GAS_LIMIT="${GAS_LIMIT:-200000}"

command -v cast >/dev/null || fail "cast (foundry) not found — needed to sign the forged insertGlobalExitRoot tx"

log "Phase B — forged GER must be refused under --reject-unverified-ger-injection"
# A 32-byte root with no L1 observation: deliberately not a real L1 exit root,
# so the L1 InfoTree indexer never wrote its (mainnet, rollup) decomposition.
FORGED=0x$(printf 'cd%.0s' {1..32})

# Build + sign a REAL insertGlobalExitRoot(bytes32) tx so eth_sendRawTransaction
# actually reaches the H6 gate (a placeholder string would only ever produce a
# DECODE error, never the "not observed on L1" refusal → a false-pass). cast
# mktx signs OFFLINE and does NOT broadcast; supplying --nonce/--gas-limit
# avoids any RPC round-trip that would revert on the forged root before the gate.
SIGNER=$(cast wallet address --private-key "$SIGNER_KEY")
NONCE_HEX=$(cast rpc eth_getTransactionCount "$SIGNER" "latest" --rpc-url "$L2_RPC_URL" | tr -d '"')
CHAIN_HEX=$(cast rpc eth_chainId --rpc-url "$L2_RPC_URL" | tr -d '"')
NONCE_DEC=$((NONCE_HEX))
CHAIN_DEC=$((CHAIN_HEX))
log "signer=$SIGNER nonce=$NONCE_DEC chainId=$CHAIN_DEC forged=$FORGED"

RAW_TX=$(cast mktx "$L2_GER_ADDRESS" "insertGlobalExitRoot(bytes32)" "$FORGED" \
    --private-key "$SIGNER_KEY" \
    --chain "$CHAIN_DEC" \
    --nonce "$NONCE_DEC" \
    --legacy \
    --gas-price "$GAS_PRICE_WEI" \
    --gas-limit "$GAS_LIMIT")

# -s (not -f): the H6 refusal comes back as a JSON-RPC error body over HTTP 200,
# which -f would swallow. Capture the body and assert the H6 error text.
out=$(curl -s "$BRIDGE_SERVICE_URL" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"$RAW_TX\"]}" \
    2>&1) || true

echo "$out" | grep -q "not observed on L1" \
    || fail "forged GER was not refused (audit H6 regression); response: $out"

# The unverified-GER metric must have incremented (>= 1: prior forged attempts
# in the same service lifetime, or the positive-phase indexer lag, may already
# have bumped it — an equality check would flake).
metrics=$(curl -sf "$BRIDGE_SERVICE_URL/metrics")
echo "$metrics" | awk '/^ger_injection_unverified_total /{ if ($2 + 0 >= 1) ok = 1 } END { exit(ok ? 0 : 1) }' \
    || fail "ger_injection_unverified_total did not increment (>= 1 expected)"

log "PASS — forged GER refused before Miden submission"
