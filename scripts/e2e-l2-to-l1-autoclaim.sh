#!/usr/bin/env bash
# L2→L1 bridge-out test — AUTO-CLAIM variant.
#
# Identical to e2e-l2-to-l1.sh through certificate settlement + bridge-service
# sync, but instead of submitting claimAsset manually it asserts that our
# standalone Rust `bridge-autoclaim` service sponsors the claim on L1 by itself.
# Verifies the L1 recipient is credited the full bridged amount (sponsor pays
# gas). The assertions are claimer-agnostic, so this script is unchanged from
# the previous Go-autoclaimer wiring beyond this comment.
#
# Requires: stack up WITH the bridge-autoclaim service, and the wallet funded
# from a prior L1→L2 deposit+claim (run `make e2e-l1-to-l2` first).
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
AUTOCLAIM_CONTAINER="${AUTOCLAIM_CONTAINER:-${COMPOSE_PROJECT_NAME}-bridge-autoclaim-1}"

WEI_PER_MIDEN_UNIT=10000000000  # 10^10: 18 ETH - 8 Miden decimals

# Recipient of the bridge-out on L1. The autoclaimer sponsors the claim with a
# DIFFERENT account (claimsponsor.keystore), so unlike the manual flow this
# address pays no gas — it receives the full bridged amount.
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
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
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
docker inspect "$AUTOCLAIM_CONTAINER" >/dev/null 2>&1 \
    || fail "bridge-autoclaim container not found ($AUTOCLAIM_CONTAINER) — is it in docker-compose.e2e.yml and up?"

ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
WALLET_ID=$(echo "$ACCOUNTS" | grep wallet_hardhat | sed 's/.*= "//;s/"//')
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

log "======================================================================"
log "  L2→L1 Bridge-Out (AUTO-CLAIM)"
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
log "Bridge-out amount: $BRIDGE_AMOUNT Miden units (expect +$EXPECTED_L1_CHANGE wei on L1, sponsor pays gas)"

L1_BAL_BEFORE=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
log "L1 balance before: $L1_BAL_BEFORE"

# ── Step 1: Create B2AGG note (bridge-out) ────────────────────────────────────
log "Step 1/5: Creating B2AGG bridge-out note..."
docker exec $AGGLAYER_CONTAINER bridge-out-tool \
    --store-dir /var/lib/miden-agglayer-service \
    --node-url http://miden-node:57291 \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount "$BRIDGE_AMOUNT" --dest-address "$L1_DEST" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG note created"

# ── Step 2: Wait for BridgeEvent log ──────────────────────────────────────────
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
BRIDGE_EVENT_TIMEOUT_S="${BRIDGE_EVENT_TIMEOUT_S:-120}"
log "Step 2/5: Waiting for BridgeEvent in L2 proxy (timeout ${BRIDGE_EVENT_TIMEOUT_S}s)..."
wait_for "BridgeEvent in eth_getLogs" \
    "cast logs --rpc-url $L2_RPC --from-block 0 $BRIDGE_EVENT_TOPIC 2>/dev/null | grep -q 'data'" \
    "$BRIDGE_EVENT_TIMEOUT_S" 5
pass "BridgeEvent detected in L2"

# ── Step 3: Wait for certificate settlement on L1 ────────────────────────────
log "Step 3/5: Waiting for certificate settlement on AggLayer..."
wait_for "certificate settled" \
    "docker logs --since $TEST_START_TIME $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    900 10
pass "Certificate settled on L1!"

# ── Step 4: Wait for deposit to appear in bridge-service ──────────────────────
BRIDGE_SERVICE_URL="http://localhost:18080"
log "Step 4/5: Waiting for bridge-service to sync L2→L1 deposit..."
wait_for "L2 deposit in bridge-service" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$L1_DEST' 2>/dev/null | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep.get('ready_for_claim') and dep.get('network_id')==1 for dep in d.get('deposits',[])) else 1)\"" \
    120 5
pass "L2→L1 deposit synced and ready_for_claim"

# ── Step 5: Assert the autoclaimer sponsors the claim on L1 ───────────────────
# No manual claimAsset here — the bridge-autoclaim service should pick up the
# ready deposit (dest_net=0, source network_id=1) on its poll interval and
# submit the claim itself. Poll the L1 recipient balance until it is credited.
log "Step 5/5: Waiting for bridge-autoclaim to sponsor the claim on L1..."
AUTOCLAIM_TIMEOUT_S="${AUTOCLAIM_TIMEOUT_S:-180}"
wait_for "L1 recipient credited by autoclaimer" \
    "test \"\$(cast balance --rpc-url $L1_RPC $L1_DEST)\" != \"$L1_BAL_BEFORE\"" \
    "$AUTOCLAIM_TIMEOUT_S" 5

L1_BAL_AFTER=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
ACTUAL_L1_CHANGE=$((L1_BAL_AFTER - L1_BAL_BEFORE))

log "Recent bridge-autoclaim logs:"
docker logs --since "$TEST_START_TIME" "$AUTOCLAIM_CONTAINER" 2>&1 | grep -iE "claim|sponsor|network" | tail -n 15 || true

# The sponsor pays gas, so the recipient receives the FULL bridged amount.
if [[ "$ACTUAL_L1_CHANGE" -ne "$EXPECTED_L1_CHANGE" ]]; then
    fail "L1 balance change mismatch: got $ACTUAL_L1_CHANGE wei, expected exactly $EXPECTED_L1_CHANGE wei (sponsor pays gas, recipient gets full amount)"
fi
pass "L2→L1 AUTO-CLAIM COMPLETE! L1 balance: $L1_BAL_BEFORE → $L1_BAL_AFTER (+$ACTUAL_L1_CHANGE wei, gas paid by sponsor)"

echo ""
log "======================================================================"
log "  L2→L1 AUTO-CLAIM TEST DONE"
log "======================================================================"
