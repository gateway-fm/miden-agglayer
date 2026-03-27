#!/usr/bin/env bash
# Dynamic ERC-20 bridge E2E test — proves a brand new token auto-creates a faucet
# and can be bridged L1→L2 and back L2→L1 with correct balances.
#
# Flow:
#   1. Deploy TestToken ERC-20 (6 decimals) on Anvil
#   2. Approve + bridge to L2 via bridgeAsset()
#   3. Wait for auto-claim (triggers faucet auto-creation)
#   4. Verify L2 wallet balance via dynamically-discovered faucet
#   5. Bridge back L2→L1 via bridge-out-tool
#   6. Wait for certificate settlement + claim on L1
#   7. Verify L1 token balance restored
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
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1  # Miden network ID

# TestToken: 6 decimals. Bridge 1000 tokens = 1000 * 10^6 = 1_000_000_000 base units.
# With 6 origin decimals and 6 miden decimals → scale=0, no scaling.
# But miden_decimals is always 8, so: scale = 6 - 8 → NEGATIVE, which would fail.
# Actually, miden supports max 8 decimals. If origin has fewer decimals (6 < 8),
# we'd need to UPSCALE, not downscale. This is handled by our auto-creation:
# origin_decimals.checked_sub(miden_decimals) fails when origin < miden.
#
# So let's use 18 decimals to match ETH and get scale=10, OR use 8 decimals for scale=0.
# Using 18 decimals is most realistic (most ERC-20 tokens use 18).
TOKEN_DECIMALS=18
TOKEN_INITIAL_SUPPLY="1000000000000000000000000" # 1M tokens (10^6 * 10^18)
# Bridge 1000 tokens = 1000 * 10^18 base units
BRIDGE_AMOUNT="1000000000000000000000"
# With scale=10 (18 origin - 8 miden): 1000 * 10^18 / 10^10 = 1000 * 10^8 = 100_000_000_000
# That's 100 billion miden units... too large. Let's bridge a smaller amount.
# Bridge 0.001 tokens = 10^15 base units → 10^15 / 10^10 = 10^5 = 100_000 miden units
BRIDGE_AMOUNT="1000000000000000"  # 10^15 = 0.001 tokens
WEI_PER_MIDEN_UNIT=10000000000    # 10^10: 18 - 8 decimals
EXPECTED_L2_BALANCE=$((BRIDGE_AMOUNT / WEI_PER_MIDEN_UNIT))  # 100000

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
command -v forge >/dev/null || fail "forge (foundry) not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
curl -sf "$L2_RPC" -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null \
    || fail "L2 proxy not reachable"

ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
WALLET_ID=$(echo "$ACCOUNTS" | grep wallet_hardhat | sed 's/.*= "//;s/"//')
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')

# Get wallet's zero-padded Ethereum address
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')
WALLET_HEX=$(docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ETH" \
    --amount 1 --dest-address 0xdead --dest-network 0 2>&1 | grep "wallet:" | awk '{print $NF}' || true)
[[ -z "$WALLET_HEX" ]] && fail "Could not get wallet hex"
INNER="${WALLET_HEX#0x}"
PREFIX="${INNER:0:16}"
SUFFIX="${INNER:16:14}00"
DEST_ADDR="0x00000000${PREFIX}${SUFFIX}"

log "======================================================================"
log "  Dynamic ERC-20 Bridge E2E Test"
log "======================================================================"
log "Wallet:  $WALLET_ID ($WALLET_HEX)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"
log "Amount:  $BRIDGE_AMOUNT base units (expect $EXPECTED_L2_BALANCE Miden units)"

# ── Step 1: Deploy TestToken ERC-20 on Anvil ──────────────────────────────────
log "Step 1/7: Deploying TestToken ERC-20 on Anvil..."
DEPLOY_OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
    --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    --broadcast \
    --constructor-args "TestToken" "TT" "$TOKEN_DECIMALS" "$TOKEN_INITIAL_SUPPLY" 2>&1)
TOKEN_ADDR=$(echo "$DEPLOY_OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -z "$TOKEN_ADDR" ]] && fail "Failed to deploy TestToken: $DEPLOY_OUT"
pass "TestToken deployed at $TOKEN_ADDR"

# Verify token metadata
TOKEN_NAME=$(cast call --rpc-url "$L1_RPC" "$TOKEN_ADDR" "name()(string)")
TOKEN_SYMBOL=$(cast call --rpc-url "$L1_RPC" "$TOKEN_ADDR" "symbol()(string)")
TOKEN_DEC=$(cast call --rpc-url "$L1_RPC" "$TOKEN_ADDR" "decimals()(uint8)")
log "Token: name=$TOKEN_NAME, symbol=$TOKEN_SYMBOL, decimals=$TOKEN_DEC"

# Check admin_listFaucets before bridging
FAUCETS_BEFORE=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | python3 -c "import json,sys; r=json.load(sys.stdin); print(len(r.get('result',[])))")
log "Faucets registered before bridge: $FAUCETS_BEFORE"

# ── Step 2: Approve + Bridge L1→L2 ───────────────────────────────────────────
# Work around aggkit aggoracle "already exists" bug (agglayer/aggkit#1479).
# The aggoracle stops injecting GERs after this error because it treats
# "already exists" as fatal. Fixed in aggkit v0.8.2+ but our E2E uses v0.9.0-rc2.
# Clearing monitored_txs and restarting forces the aggoracle to re-process.
log "Clearing aggoracle state + restarting aggkit (aggkit#1479 workaround)..."
docker exec miden-agglayer-postgres-1 psql -U bridge_user -d bridge_db -c \
    "DELETE FROM sync.monitored_txs WHERE owner = 'aggoracle';" >/dev/null 2>&1 || true
docker restart "${AGGKIT_CONTAINER}" >/dev/null 2>&1 || true
sleep 10

log "Step 2/7: Approving bridge contract to spend TestToken..."
cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$TOKEN_ADDR" \
    "approve(address,uint256)" "$BRIDGE_ADDRESS" "$BRIDGE_AMOUNT" \
    >/dev/null 2>&1 || fail "approve failed"
pass "Approved bridge for $BRIDGE_AMOUNT base units"

log "Bridging TestToken L1→L2..."
TX=$(cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST_ADDR" "$BRIDGE_AMOUNT" \
    "$TOKEN_ADDR" true 0x \
    2>&1)
echo "$TX" | grep -q "status.*1" || fail "L1 bridge tx failed: $TX"
pass "TestToken bridged on L1"

# ── Step 3: Wait for auto-claim (which triggers faucet auto-creation) ─────────
log "Step 3/7: Waiting for deposit to be ready_for_claim..."
wait_for "deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and dep['amount']!='0' for dep in d['deposits']) else 1)\"" \
    180 5
pass "Deposit is ready_for_claim"

log "Waiting for faucet auto-creation + CLAIM note submission..."
wait_for "auto-creating faucet" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    180 5
pass "Faucet auto-creation triggered!"

wait_for "claim tx submitted" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'submitted claim note txn'" \
    120 5
pass "CLAIM note submitted"

wait_for "claim tx committed" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    60 3
pass "CLAIM committed to Miden block"

# ── Step 4: Verify faucet was auto-created ────────────────────────────────────
log "Step 4/7: Verifying faucet auto-creation..."
FAUCETS_AFTER=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}')
FAUCET_COUNT=$(echo "$FAUCETS_AFTER" | python3 -c "import json,sys; r=json.load(sys.stdin); print(len(r.get('result',[])))")
log "Faucets registered after bridge: $FAUCET_COUNT (was $FAUCETS_BEFORE)"

if [[ "$FAUCET_COUNT" -le "$FAUCETS_BEFORE" ]]; then
    fail "No new faucet was created! Expected faucet count > $FAUCETS_BEFORE"
fi

# Get the new faucet's ID (look for our token symbol "TT")
NEW_FAUCET_ID=$(echo "$FAUCETS_AFTER" | python3 -c "
import json, sys
r = json.load(sys.stdin)
for f in r.get('result', []):
    if f.get('symbol') == 'TT':
        print(f['faucet_id'])
        break
")
[[ -z "$NEW_FAUCET_ID" ]] && fail "Could not find TT faucet in admin_listFaucets"
pass "Faucet auto-created: $NEW_FAUCET_ID (symbol=TT)"

# ── Step 5: Verify L2 wallet balance ──────────────────────────────────────────
# Convert faucet_id hex to bech32 for bridge-out-tool (it expects bech32)
# Actually, bridge-out-tool should accept the ID from admin_listFaucets
log "Step 5/7: Checking L2 wallet balance with new faucet..."
BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BAL_OUT=$(docker exec $AGGLAYER_CONTAINER bridge-out-tool \
        --store-dir /var/lib/miden-agglayer-service \
        --node-url http://miden-node:57291 \
        --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$NEW_FAUCET_ID" \
        --amount 999999999 \
        --dest-address 0xdead --dest-network 0 2>&1 || true)
    BALANCE=$(echo "$BAL_OUT" | grep "wallet balance:" | head -1 | awk '{print $NF}')
    log "Attempt $attempt/15: balance = ${BALANCE:-0}"
    if [[ -n "$BALANCE" && "$BALANCE" != "0" ]]; then
        break
    fi
done

if [[ -z "$BALANCE" || "$BALANCE" == "0" ]]; then
    fail "Wallet TestToken balance is still 0 after 2.5 minutes"
elif [[ "$BALANCE" -ne "$EXPECTED_L2_BALANCE" ]]; then
    fail "Balance mismatch: got $BALANCE, expected $EXPECTED_L2_BALANCE"
else
    pass "L1→L2 TestToken balance verified: $BALANCE Miden units"
fi

# ── Step 6: Bridge L2→L1 ─────────────────────────────────────────────────────
L1_DEST="$FUNDED_ADDR"
BRIDGE_OUT_AMOUNT=$((BALANCE / 2))
EXPECTED_L1_TOKENS=$((BRIDGE_OUT_AMOUNT * WEI_PER_MIDEN_UNIT))
log "Step 6/7: Bridging $BRIDGE_OUT_AMOUNT Miden units back to L1..."

docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$NEW_FAUCET_ID" \
    --amount "$BRIDGE_OUT_AMOUNT" --dest-address "$L1_DEST" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG note created for TestToken"

# Wait for BridgeEvent
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
wait_for "BridgeEvent in L2 logs" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'emitted BridgeEvent'" \
    120 5
pass "BridgeEvent emitted for TestToken bridge-out"

# Wait for certificate settlement
log "Step 7/7: Waiting for certificate settlement on L1..."

# Get L1 token balance before claim
L1_TOKEN_BAL_BEFORE=$(cast call --rpc-url "$L1_RPC" "$TOKEN_ADDR" "balanceOf(address)(uint256)" "$L1_DEST")
log "L1 TestToken balance before claim: $L1_TOKEN_BAL_BEFORE"

wait_for "certificate settled" \
    "docker logs --since $TEST_START_TIME $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    300 10
pass "Certificate settled on L1"

# Wait for deposit in bridge-service
wait_for "L2 deposit in bridge-service" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$L1_DEST' 2>/dev/null | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep.get('ready_for_claim') and dep.get('network_id')==1 for dep in d.get('deposits',[])) else 1)\"" \
    120 5
pass "TestToken L2→L1 deposit synced and ready_for_claim"

# Claim on L1 (same pattern as e2e-l2-to-l1.sh)
DEPOSITS_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$L1_DEST")
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

NETWORK_ID_VAL=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['network_id'])")
PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$DEPOSIT_CNT&net_id=$NETWORK_ID_VAL")
[[ -z "$PROOF_JSON" ]] && fail "Could not get merkle proof"

MAINNET_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])")
ROLLUP_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])")

SMT_LOCAL=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
")
SMT_ROLLUP=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
")

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

# Verify L1 token balance change (use python for big number arithmetic)
L1_TOKEN_BAL_AFTER=$(cast call --rpc-url "$L1_RPC" "$TOKEN_ADDR" "balanceOf(address)(uint256)" "$L1_DEST")
# Strip any trailing annotations like "[9.999e23]" from cast output
L1_TOKEN_BAL_BEFORE_CLEAN=$(echo "$L1_TOKEN_BAL_BEFORE" | awk '{print $1}')
L1_TOKEN_BAL_AFTER_CLEAN=$(echo "$L1_TOKEN_BAL_AFTER" | awk '{print $1}')
L1_CHANGE=$(python3 -c "print(int('$L1_TOKEN_BAL_AFTER_CLEAN') - int('$L1_TOKEN_BAL_BEFORE_CLEAN'))")
log "L1 TestToken balance: $L1_TOKEN_BAL_BEFORE_CLEAN → $L1_TOKEN_BAL_AFTER_CLEAN (+$L1_CHANGE)"

if [[ "$L1_CHANGE" != "$EXPECTED_L1_TOKENS" ]]; then
    fail "L1 token balance change mismatch: got $L1_CHANGE, expected $EXPECTED_L1_TOKENS"
fi

pass "Dynamic ERC-20 bridge COMPLETE!"
pass "  L1→L2: $BRIDGE_AMOUNT base units → $EXPECTED_L2_BALANCE Miden units"
pass "  L2→L1: $BRIDGE_OUT_AMOUNT Miden units → $EXPECTED_L1_TOKENS base units"
pass "  Faucet auto-created: $NEW_FAUCET_ID (symbol=TT, decimals=$TOKEN_DECIMALS)"

echo ""
log "======================================================================"
log "  DYNAMIC ERC-20 E2E TEST DONE"
log "======================================================================"
