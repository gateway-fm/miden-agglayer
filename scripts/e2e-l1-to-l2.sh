#!/usr/bin/env bash
# L1→L2 deposit + claim test
# Deposits ETH on L1, waits for bridge-service to sync + auto-claim on L2 proxy.
# The wallet receives tokens via CLAIM → NTX builder → P2ID note.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
BRIDGE_SERVICE_URL="http://localhost:18080"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
DEST_NETWORK=1  # Miden network ID from RollupManager
DEPOSIT_AMOUNT="10000000000000" # 10^13 wei → 1000 Miden units (scale 10^10: 18 ETH - 8 Miden decimals)
WEI_PER_MIDEN_UNIT=10000000000  # 10^10
EXPECTED_L2_BALANCE=$((DEPOSIT_AMOUNT / WEI_PER_MIDEN_UNIT))

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
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1 \
    || fail "Bridge service not reachable at $BRIDGE_SERVICE_URL"

# ── Get account IDs ──────────────────────────────────────────────────────────
ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
WALLET_ID=$(echo "$ACCOUNTS" | grep wallet_hardhat | sed 's/.*= "//;s/"//')
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# Get wallet's zero-padded Ethereum address (required by MASM to_account_id)
# bridge-out-tool prints "wallet: 0x<hex>" even if balance check fails
WALLET_HEX=$(docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount 1 --dest-address 0xdead --dest-network 0 2>&1 | grep "wallet:" | awk '{print $NF}' || true)
[[ -z "$WALLET_HEX" ]] && fail "Could not get wallet hex"
INNER="${WALLET_HEX#0x}"
PREFIX="${INNER:0:16}"
SUFFIX="${INNER:16:14}00"
DEST_ADDR="0x00000000${PREFIX}${SUFFIX}"

log "======================================================================"
log "  L1→L2 Deposit + Claim"
log "======================================================================"
log "Wallet:  $WALLET_ID ($WALLET_HEX)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"
log "Amount:  $DEPOSIT_AMOUNT wei (expect $EXPECTED_L2_BALANCE Miden units)"

# Work around aggkit aggoracle "already exists" bug (agglayer/aggkit#1479).
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql -U bridge_user -d bridge_db -c \
    "DELETE FROM sync.monitored_txs WHERE owner = 'aggoracle';" >/dev/null 2>&1 || true
docker restart "${AGGKIT_CONTAINER}" >/dev/null 2>&1 || true
sleep 5

# ── Step 1: Deposit on L1 ────────────────────────────────────────────────────
log "Step 1/5: Depositing on L1..."
TX=$(cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST_ADDR" "$DEPOSIT_AMOUNT" \
    0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_AMOUNT" 2>&1)
echo "$TX" | grep -q "status.*1" || fail "L1 deposit tx failed: $TX"
pass "L1 deposit succeeded"

# ── Step 2: Wait for deposit to be ready_for_claim ────────────────────────────
log "Step 2/5: Waiting for bridge-service sync + GER injection..."
wait_for "deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and dep['amount']!='0' for dep in d['deposits']) else 1)\"" \
    180 5
pass "Deposit is ready_for_claim"

# ── Step 3: Wait for CLAIM note submission ────────────────────────────────────
log "Step 3/5: Waiting for ClaimTxManager auto-claim..."
wait_for "claim tx submitted" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'submitted claim note txn'" \
    120 5
pass "CLAIM note submitted to Miden"

# ── Step 4: Wait for CLAIM note to commit ──────────────────────────────────
log "Step 4/5: Waiting for CLAIM commit + NTX builder processing..."
wait_for "claim tx committed" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    60 3
pass "CLAIM committed — waiting for NTX builder to create P2ID..."

# ── Step 5: Verify wallet balance ──────────────────────────────────────────────
log "Step 5/5: Checking wallet balance (sync + consume P2ID notes)..."
BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BAL_OUT=$(docker exec $AGGLAYER_CONTAINER bridge-out-tool \
        --store-dir /var/lib/miden-agglayer-service \
        --node-url http://miden-node:57291 \
        --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
        --amount 999999999 \
        --dest-address 0xdead --dest-network 0 2>&1 || true)
    BALANCE=$(echo "$BAL_OUT" | grep "wallet balance:" | head -1 | awk '{print $NF}')
    log "Attempt $attempt/15: balance = ${BALANCE:-0}"
    if [[ -n "$BALANCE" && "$BALANCE" != "0" ]]; then
        break
    fi
done

if [[ -z "$BALANCE" || "$BALANCE" == "0" ]]; then
    fail "Wallet balance is still 0 after 2.5 minutes"
elif [[ "$BALANCE" -ne "$EXPECTED_L2_BALANCE" ]]; then
    fail "Balance mismatch: got $BALANCE, expected $EXPECTED_L2_BALANCE (from $DEPOSIT_AMOUNT wei / 10^10)"
else
    pass "L1→L2 COMPLETE! Wallet balance: $BALANCE (expected $EXPECTED_L2_BALANCE)"
fi

echo ""
log "======================================================================"
log "  L1→L2 TEST DONE"
log "======================================================================"
