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

# ── Step 3: Wait for certificate settlement on L1 ────────────────────────────
L1_BAL_BEFORE=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
log "L1 balance before settlement: $L1_BAL_BEFORE"

log "Step 3/5: Waiting for certificate settlement on AggLayer..."
wait_for "certificate settled" \
    "docker logs --since $TEST_START_TIME $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    300 10
pass "Certificate settled on L1!"

# ── Step 4: Wait for deposit to appear in bridge-service ──────────────────────
BRIDGE_SERVICE_URL="http://localhost:18080"
log "Step 4/5: Waiting for bridge-service to sync L2→L1 deposit..."
# L2 deposits have network_id=1 (logged on L2 chain) and dest_net=0 (going to L1)
wait_for "L2 deposit in bridge-service" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$L1_DEST' 2>/dev/null | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep.get('ready_for_claim') and dep.get('network_id')==1 for dep in d.get('deposits',[])) else 1)\"" \
    120 5
pass "L2→L1 deposit synced and ready_for_claim"

# ── Step 5: Claim on L1 via bridge-service proofs + cast ──────────────────────
log "Step 5/5: Claiming deposit on L1..."

# Get the deposit details from bridge-service
DEPOSITS_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$L1_DEST")
# Find the L2→L1 deposit (network_id=1 means logged on L2, ready_for_claim=true)
DEPOSIT_INFO=$(echo "$DEPOSITS_JSON" | python3 -c "
import json, sys
d = json.load(sys.stdin)
for dep in d.get('deposits', []):
    if dep.get('ready_for_claim') and dep.get('network_id') == 1:
        print(json.dumps(dep))
        break
")
[[ -z "$DEPOSIT_INFO" ]] && fail "Could not find ready L2→L1 deposit"

DEPOSIT_CNT=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['deposit_cnt'])")
ORIG_NET=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['orig_net'])")
ORIG_ADDR=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['orig_addr'])")
DEST_NET=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['dest_net'])")
DEST_ADDR_CLAIM=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['dest_addr'])")
AMOUNT_CLAIM=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['amount'])")
METADATA_CLAIM=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
GLOBAL_INDEX=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['global_index'])")

log "Deposit #$DEPOSIT_CNT: amount=$AMOUNT_CLAIM, globalIndex=$GLOBAL_INDEX"

# Get merkle proof from bridge-service (net_id=1 for L2 deposits)
NETWORK_ID_VAL=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['network_id'])")
PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$DEPOSIT_CNT&net_id=$NETWORK_ID_VAL")
[[ -z "$PROOF_JSON" ]] && fail "Could not get merkle proof"

MAINNET_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])")
ROLLUP_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])")

# Build SMT proof arrays (32 siblings each)
SMT_LOCAL=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['merkle_proof']
# Pad to 32 entries
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
")
SMT_ROLLUP=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['rollup_merkle_proof']
# Pad to 32 entries
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
")

# Submit claimAsset on L1
CLAIM_TX=$(cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" \
    'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
    "$SMT_LOCAL" "$SMT_ROLLUP" "$GLOBAL_INDEX" \
    "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
    "$ORIG_NET" "$ORIG_ADDR" \
    "$DEST_NET" "$DEST_ADDR_CLAIM" \
    "$AMOUNT_CLAIM" "$METADATA_CLAIM" \
    2>&1)

if echo "$CLAIM_TX" | grep -q "status.*1"; then
    pass "L1 claim transaction succeeded!"
else
    warn "L1 claim tx output: $CLAIM_TX"
    fail "L1 claim transaction failed"
fi

# Verify L1 balance change
L1_BAL_AFTER=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
ACTUAL_L1_CHANGE=$((L1_BAL_AFTER - L1_BAL_BEFORE))
# Gas is deducted from the same account, allow up to 0.01 ETH for gas costs.
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
