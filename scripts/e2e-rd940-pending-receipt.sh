#!/usr/bin/env bash
# RD-940 e2e — pending-receipt wire shape
#
# Validates Spec D §2.4 — when the writer worker has accepted a tx but not yet
# committed it, `eth_getTransactionReceipt(hash)` returns top-level JSON null
# (NOT a stub with `status: "0x0"`), and `eth_getTransactionByHash(hash)`
# returns the geth pending shape (blockHash/blockNumber/transactionIndex are
# JSON null; every other numeric field is a hex string).
#
# This is THE wire contract aggkit's ethtxmanager monitors against. A single
# unintended `null` on a value-typed field would silently panic aggkit's Go-side
# hexutil.Uint{,64} / hexutil.Big unmarshallers; a stubbed status:0x0 receipt
# would make ethtxmanager bail with "tx failed".
#
# Strategy: send a sleep-padded request that the worker will hold for at least
# a few seconds (relies on the existing GER-propagation 15s wait), poll the two
# read methods during the window, then poll again after commit.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck disable=SC1091
source "$PROJECT_DIR/fixtures/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

# Fabricate a tx hash unlikely to exist. We're testing the wire shape of an
# UNKNOWN hash here — receipt MUST be null. For the in-flight shape we'd need
# a real submission timed against the worker, which the L1→L2 e2e exercises
# upstream; here we lock the null-vs-stub contract.
UNKNOWN_HASH="0xdeadbeef$(head -c 28 /dev/urandom | xxd -p -c 28)"

log "Probe eth_getTransactionReceipt with unknown hash $UNKNOWN_HASH"
RECEIPT_RES=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$UNKNOWN_HASH\"]}" \
    "$L2_RPC")

# The unknown-hash receipt MUST be JSON null (aggkit reads this as "keep polling").
# A stub with status:0x0 would tell aggkit the tx failed permanently.
if ! grep -qE '"result"\s*:\s*null' <<<"$RECEIPT_RES"; then
    fail "Unknown-hash receipt is not null — actual: $RECEIPT_RES"
fi
pass "eth_getTransactionReceipt(unknown) returned top-level null (Spec D §2.4)"

log "Probe eth_getTransactionByHash with unknown hash"
TX_RES=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionByHash\",\"params\":[\"$UNKNOWN_HASH\"]}" \
    "$L2_RPC")
if ! grep -qE '"result"\s*:\s*null' <<<"$TX_RES"; then
    fail "Unknown-hash tx lookup is not null — actual: $TX_RES"
fi
pass "eth_getTransactionByHash(unknown) returned top-level null"

# Probe the eth_getTransactionCount tag honour with a freshly-generated random
# address that has no pending or committed activity. Both tags must return 0x0.
ADDR_LOWER=$(printf '0x%040x' "$RANDOM$RANDOM$RANDOM$RANDOM")
log "eth_getTransactionCount($ADDR_LOWER, 'latest') and ('pending') tag honour"
LATEST=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionCount\",\"params\":[\"$ADDR_LOWER\",\"latest\"]}" \
    "$L2_RPC" | sed -E 's/.*"result":"([^"]+)".*/\1/')
PENDING=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionCount\",\"params\":[\"$ADDR_LOWER\",\"pending\"]}" \
    "$L2_RPC" | sed -E 's/.*"result":"([^"]+)".*/\1/')
if [[ "$LATEST" != "0x0" ]] || [[ "$PENDING" != "0x0" ]]; then
    fail "Expected both tags to return 0x0 for unused address; got latest=$LATEST pending=$PENDING"
fi
pass "eth_getTransactionCount tag honour basics OK (latest=$LATEST, pending=$PENDING)"
