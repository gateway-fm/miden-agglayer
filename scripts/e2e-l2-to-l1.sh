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
    # Subshell with pipefail off — see e2e-dynamic-erc20.sh for the SIGPIPE
    # rationale.
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

# Infrastructure account ids from the config file (NOT the sqlite store).
ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# ── Isolated bridge wallet (single-owner store policy) ───────────────────────
# The bridge-out spends the ISOLATED wallet funded by e2e-l1-to-l2.sh — both
# scripts default to the shared "e2e-suite" store subdir. Never touches the
# proxy's sqlite store.
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-suite}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ID" \
    || fail "could not provision isolated bridge-out wallet"

log "======================================================================"
log "  L2→L1 Bridge-Out"
log "======================================================================"
log "Wallet:  $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
log "Bridge:  $BRIDGE_ID"
log "Faucet:  $FAUCET_ID"
log "L1 dest: $L1_DEST"

# ── Check wallet balance ──────────────────────────────────────────────────────
log "Checking wallet balance..."
BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
log "Wallet balance: ${BALANCE:-0}"

if [[ -z "$BALANCE" || "$BALANCE" == "0" ]]; then
    fail "Wallet has no balance — run e2e-l1-to-l2.sh first"
fi

BRIDGE_AMOUNT=$((BALANCE / 2))
EXPECTED_L1_CHANGE=$((BRIDGE_AMOUNT * WEI_PER_MIDEN_UNIT))
log "Bridge-out amount: $BRIDGE_AMOUNT Miden units (expect +$EXPECTED_L1_CHANGE wei on L1)"

# Capture the L1 baseline BEFORE the deposit exists. The bridge-autoclaim
# service runs continuously and — now that the bridge-out→cert→claim path is
# fast and reliable — can settle + claim on L1 before a baseline sampled later
# (e.g. after the BridgeEvent wait) would run, which makes AFTER-BEFORE read 0.
# Sampling here, before the bridge-out, makes the +amount delta race-free.
L1_BAL_BEFORE=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
log "L1 balance before bridge-out: $L1_BAL_BEFORE"

# Baseline the newest existing L2->L1 deposit for this destination BEFORE the
# bridge-out, so the waits below latch onto THE DEPOSIT THIS RUN CREATES and not
# a leftover from an earlier run on the same chain (stack-reuse: the first-match
# lookup used to grab the already-claimed deposit #0 and read a 0 balance delta).
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
BASELINE_CNT=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$L1_DEST" 2>/dev/null | python3 -c "
import json, sys
try: d = json.load(sys.stdin)
except Exception: d = {}
cnts = [dep.get('deposit_cnt', -1) for dep in d.get('deposits', []) if dep.get('network_id') == 1]
print(max(cnts) if cnts else -1)
" 2>/dev/null); BASELINE_CNT=${BASELINE_CNT:--1}
log "Deposit-cnt baseline (this destination, pre-bridge-out): $BASELINE_CNT"

# ── Step 1: Create B2AGG note (bridge-out) ────────────────────────────────────
log "Step 1/4: Creating B2AGG bridge-out note (isolated client)..."
iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount "$BRIDGE_AMOUNT" --dest-address "$L1_DEST" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG note created"

# ── Step 2: Wait for BridgeEvent log ──────────────────────────────────────────
# Under load the bridge's B2AGG consumption hits miden-node block-producer
# crash-loops (v0.14.10 desync bug); each recovery takes 30-40s. Default 120s
# isn't enough for a single clean run on a constrained host. Override via
# BRIDGE_EVENT_TIMEOUT_S — the best-effort wrapper sets this to 600s.
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
BRIDGE_EVENT_TIMEOUT_S="${BRIDGE_EVENT_TIMEOUT_S:-120}"
log "Step 2/4: Waiting for BridgeEvent in L2 proxy (timeout ${BRIDGE_EVENT_TIMEOUT_S}s)..."
wait_for "BridgeEvent in eth_getLogs" \
    "cast logs --rpc-url $L2_RPC --from-block 0 $BRIDGE_EVENT_TOPIC 2>/dev/null | grep -q 'data'" \
    "$BRIDGE_EVENT_TIMEOUT_S" 5
pass "BridgeEvent detected in L2"

# ── Step 3: Wait for certificate settlement on L1 ────────────────────────────
# (L1_BAL_BEFORE was captured above, before the bridge-out, to avoid racing the
# autoclaim service.)
log "Step 3/5: Waiting for certificate settlement on AggLayer..."
# 900s, not 300s: cold-start agglayer prover can blow past 5 min on the first
# proof of a fresh `make test-e2e` run (circuit compile + load). Warm reruns
# settle in ~20s, well inside the original window. Keep the timeout wide so
# cold first-runs don't trip a regression false alarm.
wait_for "certificate settled" \
    "docker logs --since $TEST_START_TIME $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    900 10
pass "Certificate settled on L1!"

# ── Step 4: Wait for deposit to appear in bridge-service ──────────────────────
BRIDGE_SERVICE_URL="http://localhost:18080"
log "Step 4/5: Waiting for bridge-service to sync L2→L1 deposit..."
# L2 deposits have network_id=1 (logged on L2 chain) and dest_net=0 (going to L1)
wait_for "L2 deposit in bridge-service" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$L1_DEST' 2>/dev/null | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep.get('ready_for_claim') and dep.get('network_id')==1 and dep.get('deposit_cnt',-1)>$BASELINE_CNT for dep in d.get('deposits',[])) else 1)\"" \
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
# THIS run's deposit: newer than the pre-bridge-out baseline (stack-reuse safe)
cands = [dep for dep in d.get('deposits', [])
         if dep.get('ready_for_claim') and dep.get('network_id') == 1
         and dep.get('deposit_cnt', -1) > $BASELINE_CNT]
if cands:
    print(json.dumps(max(cands, key=lambda x: x.get('deposit_cnt', -1))))
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

# If the stack runs the bridge-autoclaim service (the restore suite does), it
# may have already claimed this deposit on L1 — the deposit then carries a
# claim_tx_hash. Re-claiming would revert (AlreadyClaimed), so verify the
# autoclaim receipt + balance instead and finish successfully.
# The autoclaim service races the manual claim path below — a deposit can be
# claimed by it at ANY point (before we query, while we fetch the proof, or
# between proof and our claimAsset). Both finishes are equally valid; this
# helper verifies the autoclaim receipt + balance and ends the test.
finish_via_autoclaim() {
    local tx="$1"
    log "Deposit claimed on L1 by the autoclaim service (tx $tx); verifying..."
    local status
    status=$(cast receipt --rpc-url "$L1_RPC" "$tx" status 2>/dev/null || echo "")
    # cast prints the receipt status as "1", "0x1", "true" or "1 (success)"
    # depending on the foundry version — accept any success spelling.
    [[ "$status" == *1* || "$status" == *true* ]] \
        || fail "autoclaim tx $tx receipt status not success: ${status:-<none>}"
    pass "L1 claim transaction succeeded (via autoclaim service)!"
    L1_BAL_AFTER=$(cast balance --rpc-url "$L1_RPC" "$L1_DEST")
    ACTUAL_L1_CHANGE=$((L1_BAL_AFTER - L1_BAL_BEFORE))
    # The autoclaimer pays gas from its own keystore account, so the
    # destination receives exactly the bridged amount.
    if [[ "$ACTUAL_L1_CHANGE" -ne "$EXPECTED_L1_CHANGE" ]]; then
        fail "L1 balance change mismatch: got $ACTUAL_L1_CHANGE wei, expected $EXPECTED_L1_CHANGE wei"
    fi
    pass "L2→L1 COMPLETE! L1 balance: $L1_BAL_BEFORE → $L1_BAL_AFTER (+$ACTUAL_L1_CHANGE wei, autoclaimed)"
    echo ""
    log "======================================================================"
    log "  L2→L1 TEST DONE"
    log "======================================================================"
    exit 0
}

# refresh_claim_tx: re-poll the deposit; echoes the claim_tx_hash if the
# autoclaimer has landed it (empty otherwise). Never fails the script.
refresh_claim_tx() {
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$L1_DEST?limit=100" 2>/dev/null | python3 -c "
import json, sys
try: d = json.load(sys.stdin)
except Exception: sys.exit(0)
for dep in d.get('deposits', []):
    if dep.get('network_id') == 1 and dep.get('deposit_cnt') == $DEPOSIT_CNT:
        print(dep.get('claim_tx_hash') or '')
        break
" 2>/dev/null || true
}

CLAIM_TX_HASH=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')")
[[ -n "$CLAIM_TX_HASH" ]] && finish_via_autoclaim "$CLAIM_TX_HASH"

# Get merkle proof from bridge-service (net_id=1 for L2 deposits)
NETWORK_ID_VAL=$(echo "$DEPOSIT_INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['network_id'])")
# The proof endpoint can transiently fail right after ready_for_claim flips
# (tree not yet built) — a one-shot `curl -sf` under set -e died SILENTLY here
# (2026-07-04). Retry up to 90s, and bail out to the autoclaim path if the
# autoclaimer lands the claim while we wait.
PROOF_JSON=""
for _ in $(seq 1 18); do
    PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$DEPOSIT_CNT&net_id=$NETWORK_ID_VAL" 2>/dev/null || true)
    [[ -n "$PROOF_JSON" ]] && break
    TX_NOW=$(refresh_claim_tx)
    [[ -n "$TX_NOW" ]] && finish_via_autoclaim "$TX_NOW"
    sleep 5
done
[[ -z "$PROOF_JSON" ]] && fail "Could not get merkle proof after 90s"

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
    2>&1) || true

STATUS=$(printf '%s\n' "$CLAIM_TX" | awk '$1=="status"{print $2; exit}')
if [[ "$STATUS" == "1" ]]; then
    pass "L1 claim transaction succeeded!"
else
    # Revert here usually means the autoclaimer beat us between proof fetch
    # and submission (AlreadyClaimed) — verify its claim instead of failing.
    TX_NOW=$(refresh_claim_tx)
    [[ -n "$TX_NOW" ]] && finish_via_autoclaim "$TX_NOW"
    warn "L1 claim tx output: $CLAIM_TX"
    fail "L1 claim transaction failed (status=$STATUS)"
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
