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
DEST_NETWORK=77  # Miden AggLayer network ID (MIDEN_NETWORK_ID constant in protocol 0.15)
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
    # Run the probe in a subshell with pipefail disabled — see full comment
    # in e2e-dynamic-erc20.sh::wait_for. `docker logs ... | grep -q` tripped
    # pipefail because grep -q closes the pipe early, docker logs takes
    # SIGPIPE, and the 141 exit was propagating as "no match".
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

# Progress-based wait. Polls $cmd like wait_for, but doesn't have a hard
# wall-clock timeout — instead it fails only when $progress_cmd's output has
# been *unchanged* for $stall_timeout consecutive seconds. The idea: bridge-
# service / aggkit / miden-node sync can legitimately take many minutes on a
# cold stack, but if NOTHING is changing for a minute then something is stuck.
#
#   wait_for_progress "desc" "<condition cmd>" "<progress probe>" stall_timeout interval max_total
#
# Args:
#   desc          : human-readable description
#   cmd           : the actual condition to wait for (exit 0 = pass)
#   progress_cmd  : command whose stdout we hash; a change = "stack made progress"
#   stall_timeout : seconds since the last progress change before we fail (default 90)
#   interval      : poll interval in seconds (default 5)
#   max_total     : absolute backstop in seconds (default 1800 = 30min) so a runaway
#                   doesn't run forever even if the progress probe keeps flapping
wait_for_progress() {
    local desc="$1" cmd="$2" progress_cmd="$3"
    local stall_timeout="${4:-90}" interval="${5:-5}" max_total="${6:-1800}"
    local elapsed=0 stalled=0
    local last_progress="" current_progress=""
    log "Waiting: $desc (stall_timeout=${stall_timeout}s, max_total=${max_total}s)..."
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        sleep "$interval"
        elapsed=$((elapsed + interval))
        current_progress=$( ( set +o pipefail; eval "$progress_cmd" ) 2>/dev/null || echo "")
        if [[ "$current_progress" != "$last_progress" ]]; then
            # State changed — reset the stall counter, print the new snapshot.
            stalled=0
            last_progress="$current_progress"
            echo ""
            log "  progress: ${current_progress:-<empty>}"
        else
            stalled=$((stalled + interval))
            echo -n "."
        fi
        [[ "$stalled" -ge "$stall_timeout" ]] && fail "Stalled: $desc (no progress for ${stall_timeout}s; last seen: ${last_progress:-<empty>})"
        [[ "$elapsed" -ge "$max_total" ]] && fail "Hard timeout: $desc (${max_total}s, last progress: ${last_progress:-<empty>})"
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

# ── Step 1: Deposit on L1 ────────────────────────────────────────────────────
log "Step 1/5: Depositing on L1..."
TX=$(cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST_ADDR" "$DEPOSIT_AMOUNT" \
    0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_AMOUNT" 2>&1)
# Parse the status field from cast's receipt dump (format: "status  1 (success)").
# Using grep on "status.*1" is fragile — the receipt's logs blob contains other
# text that the shell pipe sometimes fails to match reliably.
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "L1 deposit tx failed (status=$STATUS): $TX"
pass "L1 deposit succeeded"

# ── Step 2: Wait for deposit to be ready_for_claim ────────────────────────────
# Progress-based: bridge-service / aggkit / miden-node cold-start sync can
# legitimately take 30+s; only fail when nothing is changing. The progress
# probe is "(L1 head, proxy_synthetic_block_number, deposit_count_visible_to_bridge)"
# — any change resets the stall counter.
log "Step 2/5: Waiting for bridge-service sync + GER injection..."
wait_for_progress "deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and dep['amount']!='0' for dep in d['deposits']) else 1)\"" \
    "L1=\$(cast block-number --rpc-url '$L1_RPC' 2>/dev/null || echo ?); \
     L2=\$(curl -sf -X POST '$L2_RPC' -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}' 2>/dev/null | python3 -c 'import json,sys; print(int(json.load(sys.stdin)[\"result\"],16))' 2>/dev/null || echo ?); \
     N=\$(curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)[\"deposits\"]))' 2>/dev/null || echo ?); \
     echo \"L1=\$L1 L2_synth=\$L2 deposits_seen=\$N\"" \
    90 5 600
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
