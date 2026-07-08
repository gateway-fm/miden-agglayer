#!/usr/bin/env bash
# Cantina #10 — concurrent first-claim faucet registration E2E.
#
# The finding: two first-claims for the SAME unseen ERC-20 origin, racing
# find_or_create_faucet, could each pass the empty-local get_faucet_by_origin
# check and each deploy + bridge-register a DISTINCT faucet. The local PgStore
# would then be pinned to faucet A while the bridge's address-keyed route ended
# on faucet B. Later B-minted bridge-outs failed resolve_faucet_origin, emitted
# NO synthetic BridgeEvent, and L2→L1 settlement silently never saw them.
#
# HONESTY: e2e concurrency is BEST-EFFORT at actually triggering the race — it
# is timing-dependent (the two L1 txs must land close enough that both deposits
# become ready_for_claim before either first-claim finishes deploying a faucet).
# The DETERMINISTIC proof of the fix is the unit test
#   claim::tests::finding_10_concurrent_first_claims_deploy_single_faucet
# (N concurrent first-claims through the single-flight coordinator, shared store,
# asserts the provisioning path runs EXACTLY ONCE and every awaiter resolves to
# the one winning faucet — a concurrent second claim never reaches provisioning).
# This e2e adds real-wiring + real-store + stranded-route coverage the unit test
# cannot: it drives actual L1 bridgeAsset txs, the live autoclaim/mint path, and
# a real bridge-OUT to prove the converged route is NOT stranded.
#
# Flow:
#   1. Provision an ISOLATED bridge wallet (single-owner-store policy — never
#      touch the proxy's sqlite store; any bridge-out runs via iso_tool).
#   2. Deploy ONE fresh TestToken (18 decimals). Approve the bridge for 2×amount.
#   3. Fire TWO bridgeAsset() txs for the SAME token to the SAME dest, in
#      PARALLEL with EXPLICIT sequential nonces so they pack the same/adjacent
#      L1 block → both deposits become ready_for_claim ~simultaneously → the two
#      first-claims race find_or_create_faucet for the same unseen origin.
#   4. Wait for both deposits ready_for_claim + claimed (autoclaim → L2 mint).
#   5. ASSERT: (a) EXACTLY ONE faucet has origin_address == the token (the core
#      race assertion — not two, not a split); (b) isolated-wallet L2 balance ==
#      sum of BOTH deposits (both minted under the one faucet, none stranded);
#      (c) bridge OUT some of the token and assert a BridgeEvent IS emitted
#      (proves the converged route is live, not the finding's silent drop).
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
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1  # Miden network id — local topology patch pins MIDEN_NETWORK_ID=1

# TestToken: 18 decimals → scale=10 (18 origin - 8 miden). Bridge 0.001 tokens
# = 10^15 base units per tx → 10^15 / 10^10 = 100_000 miden units per deposit.
# TWO deposits → 200_000 miden units total under the single converged faucet.
TOKEN_DECIMALS=18
TOKEN_INITIAL_SUPPLY="1000000000000000000000000" # 1M tokens
BRIDGE_AMOUNT="1000000000000000"                  # 10^15 = 0.001 tokens, per tx
WEI_PER_MIDEN_UNIT=10000000000                    # 10^10: 18 - 8 decimals
EXPECTED_PER_DEPOSIT=$((BRIDGE_AMOUNT / WEI_PER_MIDEN_UNIT))   # 100000
EXPECTED_L2_BALANCE=$((EXPECTED_PER_DEPOSIT * 2))             # 200000 (both)
APPROVE_AMOUNT=$((BRIDGE_AMOUNT * 2))                          # 2× for two txs

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
    # pipefail disabled inside the probe: `docker logs | grep -q` sends SIGPIPE
    # to docker logs on first match, whose 141 exit would otherwise look like a
    # miss. See e2e-dynamic-erc20.sh for the full rationale.
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
command -v forge >/dev/null || fail "forge (foundry) not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
wait_for "L2 proxy healthy" \
    "curl -sf '$L2_RPC' -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}' >/dev/null" \
    60 3

# Infrastructure account ids from the config file (NOT the sqlite store).
ACCOUNTS=$(docker exec $AGGLAYER_CONTAINER \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# admin_* methods require Bearer auth (R1). ADMIN_API_KEY comes from fixtures/.env.
: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY — run scripts/ensure-e2e-secrets.sh}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"

# ── Isolated bridge wallet (single-owner-store policy) ───────────────────────
# CRITICAL: every bridge-out runs as an independent client against its OWN
# sqlite store — never the proxy's /var/lib store (single-owner policy).
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-cantina10}"
B2AGG_FRESH=1
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH" \
    || fail "could not provision isolated bridge-out wallet"

log "======================================================================"
log "  Cantina #10 — concurrent first-claim faucet registration E2E"
log "======================================================================"
log "Wallet:  $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"
log "Amount:  2 × $BRIDGE_AMOUNT base units (expect $EXPECTED_L2_BALANCE Miden units total)"

# ── Step 1: Deploy ONE fresh TestToken ───────────────────────────────────────
log "Step 1/5: Deploying a fresh TestToken ERC-20 (18 decimals) on Anvil..."
DEPLOY_OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
    --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    --broadcast \
    --constructor-args "Cantina10Token" "C10" "$TOKEN_DECIMALS" "$TOKEN_INITIAL_SUPPLY" 2>&1)
TOKEN_ADDR=$(echo "$DEPLOY_OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -z "$TOKEN_ADDR" ]] && fail "Failed to deploy TestToken: $DEPLOY_OUT"
TOKEN_ADDR_LC=$(printf '%s' "$TOKEN_ADDR" | tr 'A-F' 'a-f')
pass "TestToken deployed at $TOKEN_ADDR (origin for the faucet race)"

# The origin must be UNSEEN: no faucet may already route this token. A fresh
# deploy guarantees a new address, but assert it anyway (belt + braces).
FAUCETS_BEFORE=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | python3 -c "import json,sys; r=json.load(sys.stdin); t='$TOKEN_ADDR_LC'; print(sum(1 for f in r.get('result',[]) if (f.get('origin_address') or '').lower()==t))")
[[ "$FAUCETS_BEFORE" == "0" ]] || fail "origin $TOKEN_ADDR already has $FAUCETS_BEFORE faucet(s) — not an unseen origin"
log "Confirmed origin is unseen (0 faucets route it)"

# ── Step 2: Approve the bridge for 2× the per-tx amount ──────────────────────
log "Step 2/5: Approving the bridge to spend 2× $BRIDGE_AMOUNT ($APPROVE_AMOUNT) base units..."
cast send --rpc-url "$L1_RPC" \
    --private-key "$FUNDED_KEY" \
    "$TOKEN_ADDR" \
    "approve(address,uint256)" "$BRIDGE_ADDRESS" "$APPROVE_AMOUNT" \
    >/dev/null 2>&1 || fail "approve failed"
pass "Approved bridge for $APPROVE_AMOUNT base units"

# ── Step 3: Fire TWO parallel bridgeAsset() with explicit sequential nonces ───
# Explicit --nonce N and N+1, backgrounded, so both are accepted immediately and
# pack the same/adjacent L1 block. Anvil (instant mining) puts sequential-nonce
# txs from one sender in adjacent blocks; both deposits then become
# ready_for_claim within one poll of each other → the two first-claims race.
NONCE0=$(cast nonce --rpc-url "$L1_RPC" "$FUNDED_ADDR")
NONCE1=$((NONCE0 + 1))
log "Step 3/5: Firing TWO parallel bridgeAsset() txs (nonces $NONCE0, $NONCE1) — same token, same dest..."

bridge_tx() {
    local nonce="$1"
    # --gas-limit: two same-block deposits each grow the bridge exit-tree, so the
    # deposit mined later in the block costs more gas than cast's pre-block
    # estimate → out-of-gas revert (status 0). A generous fixed limit sidesteps
    # the under-estimate (Anvil gas is free). Same fix as e2e-bridge-loadtest.sh.
    cast send --rpc-url "$L1_RPC" \
        --private-key "$FUNDED_KEY" \
        --nonce "$nonce" --gas-limit 2000000 \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$BRIDGE_AMOUNT" \
        "$TOKEN_ADDR" true 0x >"/tmp/c10-tx-$nonce.out" 2>&1
    echo "$?" >"/tmp/c10-tx-$nonce.rc"
}

bridge_tx "$NONCE0" &
PID0=$!
bridge_tx "$NONCE1" &
PID1=$!
wait "$PID0" || true
wait "$PID1" || true

for n in "$NONCE0" "$NONCE1"; do
    rc=$(cat "/tmp/c10-tx-$n.rc" 2>/dev/null || echo 1)
    status=$(awk '$1=="status"{print $2; exit}' "/tmp/c10-tx-$n.out" 2>/dev/null || echo "")
    [[ "$rc" == "0" && "$status" == "1" ]] \
        || fail "bridgeAsset tx (nonce $n) failed (rc=$rc status=$status): $(cat /tmp/c10-tx-$n.out 2>/dev/null)"
done
pass "Both bridgeAsset txs mined (nonces $NONCE0, $NONCE1) — origin race is live"

# ── Step 4: Wait for both deposits ready_for_claim + claimed (autoclaim) ──────
log "Step 4/5: Waiting for BOTH deposits to be ready_for_claim..."
wait_for "2 deposits ready_for_claim for this token" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); n=sum(1 for dep in d['deposits'] if dep['ready_for_claim'] and dep['amount']=='$BRIDGE_AMOUNT'); exit(0 if n>=2 else 1)\"" \
    240 5
pass "Both deposits are ready_for_claim"

log "Waiting for faucet auto-creation (the racing first-claims) + CLAIM notes..."
wait_for "auto-creating faucet" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    180 5
pass "Faucet auto-creation triggered"

wait_for "claim tx committed" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    180 5
pass "CLAIM(s) committed to Miden block"

# ── Step 5a: THE CORE RACE ASSERTION — exactly ONE faucet for this origin ─────
log "Step 5/5 (a): Asserting EXACTLY ONE faucet routes origin $TOKEN_ADDR..."
FAUCETS_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}')
ORIGIN_FAUCETS=$(echo "$FAUCETS_JSON" | python3 -c "
import json, sys
r = json.load(sys.stdin)
t = '$TOKEN_ADDR_LC'
ids = [f['faucet_id'] for f in r.get('result', []) if (f.get('origin_address') or '').lower() == t]
print(len(ids))
for i in ids:
    print(i)
")
ORIGIN_COUNT=$(echo "$ORIGIN_FAUCETS" | head -1)
NEW_FAUCET_ID=$(echo "$ORIGIN_FAUCETS" | sed -n '2p')
log "Faucets routing $TOKEN_ADDR: $ORIGIN_COUNT"
if [[ "$ORIGIN_COUNT" -gt 1 ]]; then
    echo "$ORIGIN_FAUCETS" | tail -n +2 | sed 's/^/    faucet: /' >&2
    fail "Cantina #10: origin $TOKEN_ADDR is routed by $ORIGIN_COUNT faucets — the concurrent \
first-claims deployed a SPLIT faucet set. Local registry and the on-chain route can now \
disagree, silently dropping later bridge-outs from the losing faucet."
fi
[[ "$ORIGIN_COUNT" == "1" ]] || fail "Cantina #10: expected exactly 1 faucet for the origin, got $ORIGIN_COUNT"
[[ -n "$NEW_FAUCET_ID" ]] || fail "could not read the converged faucet id"
pass "Cantina #10: EXACTLY ONE faucet ($NEW_FAUCET_ID) routes the origin — no split"

# ── Step 5b: isolated-wallet L2 balance == sum of BOTH deposits ──────────────
log "Step 5/5 (b): Verifying L2 balance == sum of BOTH deposits ($EXPECTED_L2_BALANCE)..."
BALANCE=0
for attempt in $(seq 1 18); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$NEW_FAUCET_ID")
    log "Attempt $attempt/18: balance = ${BALANCE:-0}"
    if [[ -n "$BALANCE" && "$BALANCE" == "$EXPECTED_L2_BALANCE" ]]; then
        break
    fi
done
[[ -n "$BALANCE" && "$BALANCE" != "0" ]] || fail "Wallet balance still 0 after waiting"
if [[ "$BALANCE" -ne "$EXPECTED_L2_BALANCE" ]]; then
    fail "Cantina #10: L2 balance $BALANCE != $EXPECTED_L2_BALANCE (sum of both deposits). \
A stranded/split faucet would mint only one deposit's worth under the routed faucet."
fi
pass "Cantina #10: both deposits minted under the ONE faucet — balance = $BALANCE Miden units"

# ── Step 5c: bridge OUT proves the converged route is NOT stranded ───────────
# The finding's downstream damage: a bridge-out from a locally-unknown-but-live
# faucet fails resolve_faucet_origin and emits NO synthetic BridgeEvent, so
# L2→L1 settlement never sees it. Bridge out half the balance and assert a
# BridgeEvent IS emitted — the converged route is live end-to-end.
BRIDGE_OUT_AMOUNT=$((BALANCE / 2))
log "Step 5/5 (c): Bridging OUT $BRIDGE_OUT_AMOUNT Miden units — asserting a BridgeEvent fires..."
iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$NEW_FAUCET_ID" \
    --amount "$BRIDGE_OUT_AMOUNT" --dest-address "$FUNDED_ADDR" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG bridge-out note created for the converged faucet"

wait_for "BridgeEvent emitted for the bridge-out" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'emitted BridgeEvent'" \
    180 5
pass "Cantina #10: BridgeEvent emitted — the converged route is NOT stranded (no silent drop)"

echo ""
log "======================================================================"
log "  CANTINA #10 CONCURRENT FAUCET E2E DONE"
log "======================================================================"
pass "  origin $TOKEN_ADDR → exactly 1 faucet ($NEW_FAUCET_ID)"
pass "  both deposits minted: $EXPECTED_L2_BALANCE Miden units"
pass "  bridge-out emitted a BridgeEvent (route live, not stranded)"
