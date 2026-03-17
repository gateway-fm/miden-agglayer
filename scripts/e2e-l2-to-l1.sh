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

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
L1_DEST=$(cast wallet address --private-key "$FUNDED_KEY")

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

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

ACCOUNTS=$(docker exec miden-agglayer-miden-agglayer-1 \
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
BAL_OUT=$(docker exec miden-agglayer-miden-agglayer-1 bridge-out-tool \
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
log "Bridge-out amount: $BRIDGE_AMOUNT (half of balance)"

# ── Step 1: Create B2AGG note (bridge-out) ────────────────────────────────────
log "Step 1/4: Creating B2AGG bridge-out note..."
docker exec miden-agglayer-miden-agglayer-1 bridge-out-tool \
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

# ── Step 3: Wait for aggsender certificate ────────────────────────────────────
log "Step 3/4: Waiting for certificate settlement on AggLayer..."
wait_for "certificate settled" \
    "docker logs miden-agglayer-aggkit-1 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    300 10
pass "Certificate settled on L1!"

# ── Step 4: Wait for L1 claim (bridge-service ClaimTxManager) ─────────────────
log "Step 4/4: Claiming on L1..."
L1_BAL_BEFORE=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
log "L1 balance before: $L1_BAL_BEFORE"

# Wait for bridge-service to sync the BridgeEvent from L2
BRIDGE_SVC="http://localhost:18080"
wait_for "bridge-service L2 deposit sync" \
    "curl -sf '$BRIDGE_SVC/bridges/$L1_DEST' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['network_id']==1 and dep['ready_for_claim'] for dep in d['deposits']) else 1)\"" \
    120 5
pass "Bridge-service synced L2→L1 deposit"

# Query Merkle proof and deposit details, then claim on L1 via cast
log "Building claimAsset tx..."
PROOF_JSON=$(curl -sf "$BRIDGE_SVC/merkle-proof?deposit_cnt=0&net_id=1")
DEP_JSON=$(curl -sf "$BRIDGE_SVC/bridges/$L1_DEST" | python3 -c "
import json, sys
d = json.load(sys.stdin)
for dep in d['deposits']:
    if dep['network_id'] == 1:
        print(json.dumps(dep))
        break
")

# Extract proof arrays as solidity tuple format [bytes32,bytes32,...]
SMT_LOCAL=$(echo "$PROOF_JSON" | python3 -c "import json,sys; p=json.load(sys.stdin)['proof']; print('[' + ','.join(p['merkle_proof']) + ']')")
SMT_ROLLUP=$(echo "$PROOF_JSON" | python3 -c "import json,sys; p=json.load(sys.stdin)['proof']; print('[' + ','.join(p['rollup_merkle_proof']) + ']')")
MAIN_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])")
ROLLUP_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])")

GLOBAL_INDEX=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['global_index'])")
ORIG_NET=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['orig_net'])")
ORIG_ADDR=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['orig_addr'])")
DEST_NET=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['dest_net'])")
DEST_ADDR_CLAIM=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['dest_addr'])")
AMOUNT=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['amount'])")
METADATA=$(echo "$DEP_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['metadata'])")

cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" \
    "claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)" \
    "$SMT_LOCAL" "$SMT_ROLLUP" "$GLOBAL_INDEX" "$MAIN_ROOT" "$ROLLUP_ROOT" \
    "$ORIG_NET" "$ORIG_ADDR" "$DEST_NET" "$DEST_ADDR_CLAIM" "$AMOUNT" "$METADATA" 2>&1

L1_BAL_AFTER=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
if [[ "$L1_BAL_AFTER" != "$L1_BAL_BEFORE" ]]; then
    pass "L2→L1 COMPLETE! L1 balance: $L1_BAL_BEFORE → $L1_BAL_AFTER"
else
    fail "L1 balance unchanged after claim"
fi

echo ""
log "======================================================================"
log "  L2→L1 TEST DONE"
log "======================================================================"
