#!/usr/bin/env bash
# L2→L1 bridge-out test
# Creates B2AGG note on Miden, waits for BridgeEvent, aggsender certificate, L1 settlement.
# Requires: wallet has balance from a prior L1→L2 deposit+claim.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"

WEI_PER_MIDEN_UNIT=10000000000  # 10^10: 18 ETH - 8 Miden decimals

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
L1_DEST=$(cast wallet address --private-key "$FUNDED_KEY")

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! eval "$cmd" 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || fail "cast (foundry) not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 not reachable"

ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
WALLET_ID=$(echo "$ACCOUNTS" | grep wallet_hardhat | sed 's/.*= "//;s/"//')
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

log "======================================================================"
log "  L2→L1 Bridge-Out"
log "======================================================================"
log "Wallet:  $WALLET_ID"
log "Bridge:  $BRIDGE_ID"
log "Faucet:  $FAUCET_ID"
log "L1 dest: $L1_DEST"

# ── Check wallet balance ──────────────────────────────────────────────────────
log "Checking wallet balance..."
BAL_OUT=$(docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount 999999999999 --dest-address "$L1_DEST" --dest-network 0 2>&1 || true)
BALANCE=$(echo "$BAL_OUT" | grep "wallet balance:" | head -1 | awk '{print $NF}')
log "Wallet balance: ${BALANCE:-0}"

if [[ -z "$BALANCE" || "$BALANCE" == "0" ]]; then
    fail "Wallet has no balance — run e2e-l1-to-l2.sh first"
fi

BRIDGE_AMOUNT=$((BALANCE / 2))
EXPECTED_L1_CHANGE=$((BRIDGE_AMOUNT * WEI_PER_MIDEN_UNIT))
log "Bridge-out amount: $BRIDGE_AMOUNT Miden units (expect +$EXPECTED_L1_CHANGE wei on L1)"

# ── Step 1: Create B2AGG note (bridge-out) ────────────────────────────────────
log "Step 1/4: Creating B2AGG bridge-out note..."
docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount "$BRIDGE_AMOUNT" --dest-address "$L1_DEST" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG note created"

# ── Step 2: Wait for BridgeEvent log ──────────────────────────────────────────
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
log "Step 2/4: Waiting for BridgeEvent in L2 proxy..."
wait_for "BridgeEvent in eth_getLogs" \
    "cast logs --rpc-url $L2_RPC --from-block 0 $BRIDGE_EVENT_TOPIC 2>/dev/null | grep -q 'data'" \
    120 5
pass "BridgeEvent detected in L2"

# ── Step 3: Wait for settlement + ClaimSettler auto-claim on L1 ──────────────
# Capture L1 balance before settlement so we detect the claim even if it's fast
L1_BAL_BEFORE=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
log "L1 balance before settlement: $L1_BAL_BEFORE"

log "Step 3/4: Waiting for certificate settlement on AggLayer..."
wait_for "certificate settled" \
    "docker logs --since $TEST_START_TIME $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    300 10
pass "Certificate settled on L1!"

# ── Step 4: Wait for ClaimSettler to auto-claim on L1 ─────────────────────────
log "Step 4/4: Waiting for ClaimSettler to auto-claim on L1..."

wait_for "L1 balance change (ClaimSettler auto-claim)" \
    "[[ \$(cast balance --rpc-url $L1_RPC $L1_DEST 2>/dev/null) != '$L1_BAL_BEFORE' ]]" \
    120 5

L1_BAL_AFTER=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
ACTUAL_L1_CHANGE=$((L1_BAL_AFTER - L1_BAL_BEFORE))
# The ClaimSettler signs with the same key as L1_DEST, so gas is deducted from the
# same account that receives the claim. Allow up to 0.01 ETH for gas costs.
MAX_GAS_COST=10000000000000000  # 0.01 ETH
MIN_EXPECTED=$((EXPECTED_L1_CHANGE - MAX_GAS_COST))
if [[ "$ACTUAL_L1_CHANGE" -lt "$MIN_EXPECTED" || "$ACTUAL_L1_CHANGE" -gt "$EXPECTED_L1_CHANGE" ]]; then
    fail "L1 balance change out of range: got $ACTUAL_L1_CHANGE wei, expected ~$EXPECTED_L1_CHANGE wei ($BRIDGE_AMOUNT Miden * 10^10, minus gas)"
fi
GAS_USED=$((EXPECTED_L1_CHANGE - ACTUAL_L1_CHANGE))
pass "L2→L1 COMPLETE! L1 balance: $L1_BAL_BEFORE → $L1_BAL_AFTER (+$ACTUAL_L1_CHANGE wei, gas: $GAS_USED wei)"

echo ""
log "======================================================================"
log "  L2→L1 TEST DONE"
log "======================================================================"
