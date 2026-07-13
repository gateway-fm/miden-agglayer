#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-manual-user-claim.sh — MANUAL USER CLAIM against the live proxy
#
# There is no sponsor concept in the proxy: an ordinary USER key's claimAsset
# takes the identical eth_sendRawTransaction path as the bridge-service
# ClaimTxManager sponsor's, and the claim dedup lock is keyed by globalIndex
# only (signer-agnostic). This script drives that end-to-end:
#
#   Leg 1 — manual user claim wins:
#     1. Bridge L1→L2 (bridgeAsset on Anvil) to an isolated Miden wallet.
#     2. A USER key — the anvil dev key, NOT the claimsponsor keystore —
#        pre-signs claimAsset (proof fetched from bridge-service) and submits
#        it via raw eth_sendRawTransaction in a tight retry loop (retrying the
#        C6 "GER not observed yet" rejections), racing the sponsor's 2s
#        monitor. The user's 1s loop should land first; if the sponsor wins a
#        round anyway, a fresh deposit is tried (up to MAX_LEG1_ATTEMPTS).
#     3. Assert: user tx receipt (status 1), exactly ONE ClaimEvent for the
#        globalIndex, the event under the USER's tx hash, and the wrapped
#        balance delta on the Miden wallet.
#
#   Leg 2 — dedup race on the same globalIndex:
#     4. Second deposit; wait until it is ready_for_claim (sponsor is now
#        actively trying), then fire the user's manual claim on the SAME
#        globalIndex. Whoever wins, assert: exactly ONE ClaimEvent for the gi;
#        the loser observed the "claim already submitted" dedup rejection
#        (the user's own rejected submission if the sponsor won; a
#        deterministic extra user submission — plus a best-effort proxy-log
#        grep for the sponsor's rejection — if the user won); and the winner's
#        tx hash is the one carrying the ClaimEvent + receipt.
#
# USER key: anvil dev key #0 — a TEST-ONLY kurtosis credential already used by
# e2e-security.sh and scripts/claim.sh. Fine in fixtures/scripts; never prod.
#
# Stack-reuse-safe: baselines deposit counts and balances; never restarts
# containers. Run with COMPOSE_PROJECT_NAME=<project> to target a named stack.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

# TEST-ONLY anvil dev key #0 (same fixture credential as e2e-security.sh /
# scripts/claim.sh). Distinct from the stack's claimsponsor keystore signer.
USER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
# The L1-funded deposit key (same as e2e-l1-to-l2.sh).
FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"

DEST_NETWORK=1
DEPOSIT_AMOUNT="10000000000000" # 10^13 wei → 1000 Miden units (scale 10^10)
WEI_PER_MIDEN_UNIT=10000000000
EXPECTED_UNITS_PER_DEPOSIT=$((DEPOSIT_AMOUNT / WEI_PER_MIDEN_UNIT))

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
MAX_LEG1_ATTEMPTS="${MAX_LEG1_ATTEMPTS:-3}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

# Strip ANSI colour escapes before any log assertion (docker logs are
# colourised; raw greps on field patterns silently miss).
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

rpc() { # rpc <method> <params-json>
    curl -s -m 300 -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$1\",\"params\":$2,\"id\":1}"
}

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
command -v cast    >/dev/null || fail "cast (foundry) not found"
command -v curl    >/dev/null || fail "curl not found"
command -v python3 >/dev/null || fail "python3 not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
docker inspect "$AGGLAYER_CONTAINER" >/dev/null 2>&1 \
    || fail "proxy container $AGGLAYER_CONTAINER not found"

BRIDGE_UP=false
for _ in $(seq 1 30); do
    if curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1; then
        BRIDGE_UP=true; break
    fi
    sleep 2
done
[[ "$BRIDGE_UP" == "true" ]] || fail "bridge-service not reachable at $BRIDGE_SERVICE_URL"

CHAIN_ID_HEX=$(rpc eth_chainId '[]' | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])")
CHAIN_ID=$((CHAIN_ID_HEX))
USER_ADDR=$(cast wallet address --private-key "$USER_KEY")
log "proxy chain id: $CHAIN_ID; user (manual claimant): $USER_ADDR"

# ── Infra account ids + isolated destination wallet ──────────────────────────
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-manual-user-claim}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ID" \
    || fail "could not provision isolated destination wallet"

log "======================================================================"
log "  MANUAL USER CLAIM e2e"
log "======================================================================"
log "Wallet:  $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"
log "User:    $USER_ADDR (manual claimant, anvil dev key)"

BAL_BEFORE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
BAL_BEFORE="${BAL_BEFORE:-0}"
log "L2 wallet balance before: $BAL_BEFORE"

# ── Helpers ───────────────────────────────────────────────────────────────────

# known_deposit_cnts → space-separated deposit_cnt list currently visible for
# DEST_ADDR (the stack-reuse baseline).
known_deposit_cnts() {
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR" 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(' '.join(str(dep['deposit_cnt']) for dep in d.get('deposits', [])))
except Exception:
    pass
"
}

# new_deposit_json <baseline-cnts> → the deposit JSON for OUR new L1→L2 deposit
# (network_id 0, matching amount, cnt not in baseline), or "".
new_deposit_json() {
    local baseline="$1"
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR" 2>/dev/null | python3 -c "
import json, sys
baseline = set('$baseline'.split())
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for dep in d.get('deposits', []):
    if str(dep['deposit_cnt']) in baseline:
        continue
    if dep.get('network_id') != 0:
        continue
    if dep.get('amount') != '$DEPOSIT_AMOUNT':
        continue
    print(json.dumps(dep))
    break
"
}

dep_field() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

# do_l1_deposit → sends bridgeAsset on L1; fails the script on error.
do_l1_deposit() {
    local tx status
    tx=$(cast send --rpc-url "$L1_RPC" \
        --private-key "$FUNDED_KEY" \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$DEPOSIT_AMOUNT" \
        0x0000000000000000000000000000000000000000 true 0x \
        --value "$DEPOSIT_AMOUNT" 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || fail "L1 deposit tx failed (status=$status): $tx"
}

# build_user_claim_raw <deposit-json> → prints the pre-signed raw claimAsset tx
# for the USER key (empty on proof-not-ready). Nonce is re-read per call.
build_user_claim_raw() {
    local dep="$1" cnt gi orig_net orig_addr dest_net dest_addr amount metadata
    local proof mer rer smt_local smt_rollup calldata nonce_hex nonce
    cnt=$(echo "$dep" | dep_field deposit_cnt)
    gi=$(echo "$dep" | dep_field global_index)
    orig_net=$(echo "$dep" | dep_field orig_net)
    orig_addr=$(echo "$dep" | dep_field orig_addr)
    dest_net=$(echo "$dep" | dep_field dest_net)
    dest_addr=$(echo "$dep" | dep_field dest_addr)
    amount=$(echo "$dep" | dep_field amount)
    metadata=$(echo "$dep" | dep_field metadata)
    [[ -z "$metadata" || "$metadata" == "None" ]] && metadata="0x"

    proof=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$cnt&net_id=0" 2>/dev/null) || return 0
    [[ -z "$proof" ]] && return 0
    mer=$(echo "$proof" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])") || return 0
    rer=$(echo "$proof" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])") || return 0
    smt_local=$(echo "$proof" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
") || return 0
    smt_rollup=$(echo "$proof" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')
") || return 0

    calldata=$(cast calldata \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$smt_local" "$smt_rollup" "$gi" "$mer" "$rer" \
        "$orig_net" "$orig_addr" "$dest_net" "$dest_addr" "$amount" "$metadata") || return 0

    nonce_hex=$(rpc eth_getTransactionCount "[\"$USER_ADDR\",\"latest\"]" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])") || return 0
    nonce=$((nonce_hex))

    cast mktx --private-key "$USER_KEY" --chain "$CHAIN_ID" --nonce "$nonce" \
        --legacy --gas-price 1000000000 --gas-limit 5000000 \
        "$BRIDGE_ADDRESS" "$calldata" 2>/dev/null || return 0
}

# submit_user_claim <deposit-json> <timeout-secs>
# Tight retry loop: rebuild + submit the user's claim until accepted or the
# dedup rejection fires. Sets:
#   SUBMIT_OUTCOME = user_won | dedup_rejected | timeout
#   USER_TX        = the user's accepted tx hash (user_won only)
#   LAST_ERR       = last JSON-RPC error message
submit_user_claim() {
    local dep="$1" timeout="$2" started raw resp result errmsg
    SUBMIT_OUTCOME="timeout"; USER_TX=""; LAST_ERR=""
    started=$(date +%s)
    while (( $(date +%s) - started < timeout )); do
        raw=$(build_user_claim_raw "$dep")
        if [[ -z "$raw" ]]; then
            sleep 1; continue    # proof not available yet
        fi
        resp=$(rpc eth_sendRawTransaction "[\"$raw\"]")
        result=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('result') or '')" 2>/dev/null || true)
        errmsg=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('error',{}).get('message') or '')" 2>/dev/null || true)
        if [[ -n "$result" ]]; then
            SUBMIT_OUTCOME="user_won"; USER_TX="$result"; return 0
        fi
        LAST_ERR="$errmsg"
        if [[ "$errmsg" == *"already submitted"* ]]; then
            SUBMIT_OUTCOME="dedup_rejected"; return 0
        fi
        # C6 GER-not-seen and transient rejections: retry.
        sleep 1
    done
    return 0
}

# claim_events_for_gi <global_index> → prints "<count> <tx_hash_of_first>"
# from the proxy's eth_getLogs for the ClaimEvent topic. globalIndex is the
# first 32-byte word of the (all-non-indexed) event data.
claim_events_for_gi() {
    local gi="$1"
    rpc eth_getLogs "[{\"fromBlock\":\"0x0\",\"toBlock\":\"latest\",\"topics\":[\"$CLAIM_EVENT_TOPIC\"]}]" \
        | python3 -c "
import json, sys
gi = int('$gi')
try:
    logs = json.load(sys.stdin).get('result') or []
except Exception:
    print('0 -'); sys.exit(0)
hits = [l for l in logs if len(l.get('data','')) >= 66 and int(l['data'][2:66], 16) == gi]
print(len(hits), hits[0]['transactionHash'] if hits else '-')
"
}

# ══════════════════════════════════════════════════════════════════════════════
# Leg 1 — manual user claim (user submits instead of waiting for the sponsor)
# ══════════════════════════════════════════════════════════════════════════════
step "Leg 1 — deposit L1→L2, then the USER claims it manually"

LEG1_GI=""; LEG1_TX=""
for attempt in $(seq 1 "$MAX_LEG1_ATTEMPTS"); do
    log "Leg 1 attempt $attempt/$MAX_LEG1_ATTEMPTS"
    BASELINE_CNTS=$(known_deposit_cnts)
    do_l1_deposit
    pass "L1 deposit sent"

    DEP_JSON=""
    wait_for "deposit visible to bridge-service" \
        "DEP_JSON=\$(new_deposit_json \"$BASELINE_CNTS\"); [[ -n \"\$DEP_JSON\" ]]" 300 5
    DEP_JSON=$(new_deposit_json "$BASELINE_CNTS")
    GI=$(echo "$DEP_JSON" | dep_field global_index)
    CNT=$(echo "$DEP_JSON" | dep_field deposit_cnt)
    log "deposit_cnt=$CNT globalIndex=$GI"

    # Start submitting IMMEDIATELY (before ready_for_claim): the loop retries
    # the C6 "GER not observed yet" rejections every 1s, so the user grabs the
    # claim lock the moment the GER lands — usually beating the sponsor's 2s
    # monitor.
    submit_user_claim "$DEP_JSON" 420
    case "$SUBMIT_OUTCOME" in
        user_won)
            LEG1_GI="$GI"; LEG1_TX="$USER_TX"
            pass "USER's manual claim accepted: tx=$LEG1_TX (gi=$GI)"
            break
            ;;
        dedup_rejected)
            warn "sponsor won the claim for gi=$GI (user got the dedup rejection: '$LAST_ERR')"
            warn "retrying leg 1 with a fresh deposit"
            ;;
        timeout)
            fail "user claim never accepted nor dedup-rejected within 420s (last error: '$LAST_ERR')"
            ;;
    esac
done
[[ -n "$LEG1_TX" ]] || fail "user never won a manual claim in $MAX_LEG1_ATTEMPTS attempts — cannot demonstrate the manual-user-claim path"

# Receipt: pending until the SyntheticProjector observes the CLAIM note
# consumed; then status must be success and the ClaimEvent rides this tx.
wait_for "user claim receipt (projector finalisation)" \
    "rpc eth_getTransactionReceipt '[\"$LEG1_TX\"]' | python3 -c \"import json,sys; r=json.load(sys.stdin).get('result'); exit(0 if r and r.get('status')=='0x1' else 1)\"" \
    420 5
pass "user claim receipt landed (status 0x1)"

read -r EV_COUNT EV_TX <<<"$(claim_events_for_gi "$LEG1_GI")"
[[ "$EV_COUNT" == "1" ]] || fail "expected exactly 1 ClaimEvent for gi=$LEG1_GI, got $EV_COUNT"
[[ "${EV_TX,,}" == "${LEG1_TX,,}" ]] || fail "ClaimEvent tx hash $EV_TX != user's tx $LEG1_TX"
pass "exactly ONE ClaimEvent for gi=$LEG1_GI, under the USER's tx hash"

# Receipt 'from' must be the USER — no sponsor substitution anywhere.
RECEIPT_FROM=$(rpc eth_getTransactionReceipt "[\"$LEG1_TX\"]" \
    | python3 -c "import json,sys; print((json.load(sys.stdin)['result'].get('from') or '').lower())")
if [[ -n "$RECEIPT_FROM" && "$RECEIPT_FROM" != "null" ]]; then
    [[ "$RECEIPT_FROM" == "${USER_ADDR,,}" ]] \
        || fail "receipt 'from' is $RECEIPT_FROM, expected the user $USER_ADDR"
    pass "receipt signer is the USER ($USER_ADDR)"
else
    warn "receipt carries no 'from' field — skipping the signer assertion"
fi

# Wrapped balance: the deposit must have been minted to the Miden wallet.
log "Checking wrapped balance (sync + consume P2ID notes)..."
BALANCE="$BAL_BEFORE"
for i in $(seq 1 18); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
    BALANCE="${BALANCE:-0}"
    log "attempt $i/18: balance = $BALANCE (was $BAL_BEFORE)"
    [[ "$BALANCE" -ge $((BAL_BEFORE + EXPECTED_UNITS_PER_DEPOSIT)) ]] && break
done
[[ "$BALANCE" -ge $((BAL_BEFORE + EXPECTED_UNITS_PER_DEPOSIT)) ]] \
    || fail "wrapped balance did not increase by $EXPECTED_UNITS_PER_DEPOSIT (before=$BAL_BEFORE, now=$BALANCE)"
pass "wrapped balance delta OK: $BAL_BEFORE → $BALANCE (user-claimed deposit minted)"
BAL_AFTER_LEG1="$BALANCE"

# ══════════════════════════════════════════════════════════════════════════════
# Leg 2 — dedup race: user's manual claim vs sponsor autoclaim, same gi
# ══════════════════════════════════════════════════════════════════════════════
step "Leg 2 — race the sponsor on the SAME globalIndex"

BASELINE_CNTS=$(known_deposit_cnts)
do_l1_deposit
pass "second L1 deposit sent"

DEP_JSON=""
wait_for "second deposit visible to bridge-service" \
    "DEP_JSON=\$(new_deposit_json \"$BASELINE_CNTS\"); [[ -n \"\$DEP_JSON\" ]]" 300 5
DEP_JSON=$(new_deposit_json "$BASELINE_CNTS")
GI2=$(echo "$DEP_JSON" | dep_field global_index)
CNT2=$(echo "$DEP_JSON" | dep_field deposit_cnt)
log "race deposit_cnt=$CNT2 globalIndex=$GI2"

# This time WAIT for ready_for_claim first, so the sponsor's ClaimTxManager
# (2s monitor) is actively claiming when the user's submission goes in — a
# genuine race on the same globalIndex.
wait_for "race deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['deposit_cnt']==$CNT2 and dep['ready_for_claim'] for dep in d['deposits']) else 1)\"" \
    600 5
pass "race deposit is ready_for_claim — sponsor is live on it now"

# Anchor the proxy log BEFORE the race so the dedup-rejection grep is scoped.
LOGS_BEFORE=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | wc -l)

submit_user_claim "$DEP_JSON" 420
WINNER=""; WINNER_TX=""
case "$SUBMIT_OUTCOME" in
    user_won)
        WINNER="user"; WINNER_TX="$USER_TX"
        pass "race: USER won (tx=$USER_TX); sponsor is the loser"
        ;;
    dedup_rejected)
        WINNER="sponsor"
        [[ "$LAST_ERR" == *"already submitted"* ]] \
            || fail "loser's rejection is not the dedup path: '$LAST_ERR'"
        pass "race: SPONSOR won; user (loser) got the dedup rejection: '$LAST_ERR'"
        ;;
    timeout)
        fail "race leg: user claim neither accepted nor dedup-rejected in 420s (last: '$LAST_ERR')"
        ;;
esac

# Exactly ONE ClaimEvent for gi2, and its tx is the winner's.
wait_for "ClaimEvent for the race gi" \
    "read -r c t <<<\"\$(claim_events_for_gi '$GI2')\"; [[ \"\$c\" -ge 1 ]]" 420 5
read -r EV2_COUNT EV2_TX <<<"$(claim_events_for_gi "$GI2")"
[[ "$EV2_COUNT" == "1" ]] || fail "expected exactly 1 ClaimEvent for gi=$GI2, got $EV2_COUNT"
pass "exactly ONE ClaimEvent for the raced gi=$GI2"

if [[ "$WINNER" == "user" ]]; then
    [[ "${EV2_TX,,}" == "${WINNER_TX,,}" ]] \
        || fail "ClaimEvent tx $EV2_TX != the winning user's tx $WINNER_TX"
else
    WINNER_TX="$EV2_TX"
    # The user never got a tx accepted, so the event tx must be someone else's
    # (the sponsor's) — sanity: the winner's receipt must exist.
    log "sponsor's winning tx: $WINNER_TX"
fi
wait_for "winner's receipt (status 0x1)" \
    "rpc eth_getTransactionReceipt '[\"$WINNER_TX\"]' | python3 -c \"import json,sys; r=json.load(sys.stdin).get('result'); exit(0 if r and r.get('status')=='0x1' else 1)\"" \
    300 5
pass "winner's tx hash $WINNER_TX carries the receipt + ClaimEvent"

# The loser's dedup rejection, as observed BY THE PROXY. If the sponsor lost,
# its rejected attempt shows up in the proxy log; if the user lost we already
# asserted the JSON-RPC error above. Either way, force a DETERMINISTIC loser
# too: one more user submission for the same (now claimed) gi must be
# dedup-rejected.
RAW=$(build_user_claim_raw "$DEP_JSON")
if [[ -n "$RAW" ]]; then
    RESP=$(rpc eth_sendRawTransaction "[\"$RAW\"]")
    ERRMSG=$(echo "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin).get('error',{}).get('message') or '')" 2>/dev/null || true)
    [[ "$ERRMSG" == *"already submitted"* ]] \
        || fail "post-race resubmission for gi=$GI2 was not dedup-rejected (got: '$ERRMSG', resp: $RESP)"
    pass "post-race user resubmission dedup-rejected: '$ERRMSG'"
else
    warn "could not rebuild the user claim for the deterministic dedup check"
fi

# Best-effort: the proxy-side view of the loser (ANSI stripped, anchored on
# the exact dedup message + this gi — never a bare FAIL/error grep).
DEDUP_LOG_HITS=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 \
    | tail -n +$((LOGS_BEFORE + 1)) \
    | strip_ansi \
    | grep -c "already submitted for global_index ${GI2}" || true)
log "proxy log dedup rejections for gi=$GI2 since race start: $DEDUP_LOG_HITS"
[[ "$DEDUP_LOG_HITS" -ge 1 ]] \
    || fail "no dedup rejection for gi=$GI2 in the proxy log — the race never produced a loser?"
pass "proxy log shows the dedup rejection for gi=$GI2 ($DEDUP_LOG_HITS hits)"

# Final balance: both deposits (user-claimed + raced) must be minted.
log "Final wrapped-balance check (both deposits)..."
BALANCE="$BAL_AFTER_LEG1"
for i in $(seq 1 18); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
    BALANCE="${BALANCE:-0}"
    log "attempt $i/18: balance = $BALANCE (leg-1 level $BAL_AFTER_LEG1)"
    [[ "$BALANCE" -ge $((BAL_AFTER_LEG1 + EXPECTED_UNITS_PER_DEPOSIT)) ]] && break
done
[[ "$BALANCE" -ge $((BAL_AFTER_LEG1 + EXPECTED_UNITS_PER_DEPOSIT)) ]] \
    || fail "raced deposit never minted (balance $BALANCE, expected ≥ $((BAL_AFTER_LEG1 + EXPECTED_UNITS_PER_DEPOSIT)))"
pass "raced deposit minted exactly once: balance $BAL_AFTER_LEG1 → $BALANCE"

echo ""
log "======================================================================"
log "  MANUAL USER CLAIM e2e DONE"
log "    leg 1: user tx $LEG1_TX claimed gi=$LEG1_GI"
log "    leg 2: winner=$WINNER tx=$WINNER_TX gi=$GI2 (single ClaimEvent, loser dedup-rejected)"
log "======================================================================"
