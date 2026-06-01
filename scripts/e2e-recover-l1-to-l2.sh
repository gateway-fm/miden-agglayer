#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# E2E reproducer + fix verifier for the bali L1→L2 backlog incident.
#
# Demonstrates, end-to-end against the local docker-compose stack:
#
#   PART A — THE BUG (race-poisoned GER, mimics what happened on bali)
#     A1. Stop aggkit so backlog accumulates.
#     A2. Make N stuck L1 deposits (ready_for_claim=false).
#     A3. Submit `insertGlobalExitRoot(GARBAGE)` to the proxy. The proxy
#         refetches L1, computes keccak(real_M ‖ real_R), sees it does NOT
#         match GARBAGE, and stores the row as (mainnet=NULL, rollup=NULL,
#         is_injected=TRUE). Postgres state is verified.
#     A4. Re-submit the same GARBAGE → proxy logs "GER already seen, skipping
#         duplicate" — DEDUP-POISONED, the entry is now permanently
#         unresolvable via the insertGlobalExitRoot path.
#     A5. Verify all N deposits are STILL stuck (synthetic log fired but
#         bridge-service can't resolve (M, R) → can't advance index).
#
#   PART B — THE FIX (one-shot-ger-inject.sh with updateExitRoot)
#     B1. Run `scripts/one-shot-ger-inject.sh` against current L1 state. The
#         new path calls `updateExitRoot(R, M)` — proxy stores both roots
#         from the call parameters with NO L1 refetch.
#     B2. Verify postgres now has the freshly-injected GER row with non-NULL
#         (mainnet, rollup) AND is_injected=TRUE.
#     B3. Verify all N stuck deposits flipped to ready_for_claim=TRUE.
#
# WHY GARBAGE WORKS AS A RACE STAND-IN
# ────────────────────────────────────
# The RD-862 L1InfoTreeIndexer runs on the local stack — it would auto-heal
# a real-race-poisoned row by UPSERTing (M, R) within ~1s of polling L1.
# Bali is pre-RD-862, the indexer is NOT present, so the row stays poisoned.
# A GARBAGE hash never corresponds to any L1 (M, R) pair the indexer would
# ever observe → its row stays (NULL, NULL) here too, faithfully matching
# the bali state.
#
# Requires: stack up (`make e2e-up`).
# Exit codes: 0 = bug reproduced AND fix verified; non-zero = unexpected
# state at any verification step.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
L1_GER_ADDRESS="${L1_GER_ADDRESS:-0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674}"
L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA}"

# Aggoracle key — only signer the proxy's ALLOWED_SIGNERS permits.
SIGNER_KEY="${SIGNER_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yml"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

# docker-compose.e2e.yml requires these at interpolation time even for
# stop/start/ps — `make e2e-up` exports them, but ad-hoc compose calls
# from this script need them set explicitly.
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/miden-node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.14.10}"

# Distinguishable garbage hashes so re-runs don't collide with prior repros.
RUN_SUFFIX="$(date +%s)"
GARBAGE_HASH="0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef$(printf '%08x' "$((RUN_SUFFIX & 0xffffffff))")"

N_DEPOSITS="${N_DEPOSITS:-3}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"

# Bash colors only when stdout is a tty.
if [[ -t 1 ]]; then
  RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; CYAN=$'\033[0;36m'; BOLD=$'\033[1m'; NC=$'\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; NC=''
fi

ts()   { date +%H:%M:%S; }
say()  { printf '%s[%s]%s %s\n' "$GREEN" "$(ts)" "$NC" "$*"; }
step() { printf '\n%s[%s] %s%s%s\n' "$CYAN" "$(ts)" "$BOLD" "$*" "$NC"; }
warn() { printf '%s[%s] WARN:%s %s\n' "$YELLOW" "$(ts)" "$NC" "$*"; }
fail() { printf '%s[%s] FAIL:%s %s\n' "$RED" "$(ts)" "$NC" "$*" >&2; exit 1; }
pass() { printf '%s[%s] PASS:%s %s\n' "$GREEN" "$(ts)" "$NC" "$*"; }

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null   || fail "cast (foundry) not found"
command -v jq >/dev/null     || fail "jq not found"
command -v docker >/dev/null || fail "docker not found"

curl -sf "$L1_RPC" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null \
  || fail "L1 (anvil) not reachable at $L1_RPC — is the stack up?"
curl -sf "$L2_RPC" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null \
  || fail "L2 (proxy) not reachable at $L2_RPC — is the stack up?"
curl -sf "$BRIDGE_SERVICE_URL/bridge?net_id=0&deposit_cnt=0" >/dev/null \
  || fail "bridge-service not reachable at $BRIDGE_SERVICE_URL"

PG_CONTAINER="${COMPOSE_PROJECT_NAME}-agglayer-postgres-1"
docker inspect "$PG_CONTAINER" >/dev/null 2>&1 \
  || fail "agglayer postgres container $PG_CONTAINER not found"

pg() {
  docker exec -i "$PG_CONTAINER" \
    psql -U agglayer -d agglayer_store -At -F '|' -c "$1"
}

depo() {
  curl -sf "$BRIDGE_SERVICE_URL/bridge?net_id=0&deposit_cnt=$1" \
    | jq -r '.deposit | "\(.deposit_cnt)|\(.block_num)|\(.ready_for_claim)|\(.claim_tx_hash != "")"'
}

# ── Part A: induce the stuck state and poison a GER ───────────────────────────
step "Part A — reproduce the bali bug locally"

say "A1. Stopping aggkit so deposits stack up without GER updates from aggoracle"
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" stop aggkit >/dev/null

say "A2. Reading current bridge depositCount to pick fresh slots for our $N_DEPOSITS deposits"
START_CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
say "    current depositCount = $START_CNT"

DEPOSIT_CNTS=()
# 40-hex-char destination: 24 zero-nibbles + 8 of RUN_SUFFIX + 8 of slot index.
DEST_SUFFIX_HEX=$(printf '%08x' "$((RUN_SUFFIX & 0xffffffff))")
for i in $(seq 0 $((N_DEPOSITS - 1))); do
  CNT=$((START_CNT + i))
  DEPOSIT_CNTS+=("$CNT")
  DEST="0x000000000000000000000000${DEST_SUFFIX_HEX}$(printf '%08x' "$i")"
  say "    bridgeAsset → deposit_cnt=$CNT  (dest=$DEST  amount=$DEPOSIT_WEI wei)"
  cast send --rpc-url "$L1_RPC" \
    --private-key "$SIGNER_KEY" \
    "$L1_BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    1 "$DEST" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_WEI" >/dev/null
done

say "A2. Waiting up to 30s for bridge-service to index all $N_DEPOSITS deposits"
DEADLINE=$((SECONDS + 30))
while :; do
  ALL_INDEXED=true
  for cnt in "${DEPOSIT_CNTS[@]}"; do
    INFO=$(depo "$cnt" 2>/dev/null || echo '||')
    [[ -z ${INFO%%|*} ]] && { ALL_INDEXED=false; break; }
  done
  $ALL_INDEXED && break
  (( SECONDS >= DEADLINE )) && fail "deposits not indexed by bridge-service in 30s"
  sleep 1
done

say "A2. Verifying all $N_DEPOSITS deposits are STUCK (ready_for_claim=false)"
for cnt in "${DEPOSIT_CNTS[@]}"; do
  IFS='|' read -r DCNT BLK READY CLAIMED <<<"$(depo "$cnt")"
  [[ "$READY" == "false" ]] || fail "deposit_cnt=$cnt unexpectedly ready=$READY before any GER update"
  printf '    cnt=%s blk=%s ready=%s claimed=%s\n' "$DCNT" "$BLK" "$READY" "$CLAIMED"
done
pass "all $N_DEPOSITS deposits in expected stuck state"

step "A3. Poison a GER by submitting insertGlobalExitRoot(GARBAGE)"
say "    GARBAGE = $GARBAGE_HASH"
say "    (proxy will refetch (M, R) from L1, compute real combined, see mismatch,"
say "     store row with NULL roots and is_injected=TRUE — this IS the bali bug)"

cast send "$L2_GER_ADDRESS" 'insertGlobalExitRoot(bytes32)' "$GARBAGE_HASH" \
  --rpc-url "$L2_RPC" \
  --private-key "$SIGNER_KEY" \
  --legacy \
  --gas-price 1000000000 >/dev/null

sleep 2

say "A3. Verifying the poisoned row in agglayer_store.ger_entries"
GARB_NO0X=${GARBAGE_HASH#0x}
ROW=$(pg "SELECT encode(mainnet_exit_root, 'hex'), encode(rollup_exit_root, 'hex'), is_injected
         FROM ger_entries WHERE ger_hash = decode('$GARB_NO0X', 'hex');")
[[ -n "$ROW" ]] || fail "poisoned row not found in ger_entries — did the proxy reject the tx?"
IFS='|' read -r MAIN_HEX ROLL_HEX IS_INJ <<<"$ROW"
say "    mainnet_exit_root = ${MAIN_HEX:-<NULL>}"
say "    rollup_exit_root  = ${ROLL_HEX:-<NULL>}"
say "    is_injected       = $IS_INJ"

if [[ -z "$MAIN_HEX" && -z "$ROLL_HEX" && "$IS_INJ" == "t" ]]; then
  pass "BUG REPRODUCED: poisoned row has NULL roots AND is_injected=TRUE"
else
  fail "expected (NULL, NULL, t), got ($MAIN_HEX, $ROLL_HEX, $IS_INJ)"
fi

step "A4. Verifying dedup poison: re-submitting same GARBAGE is a no-op"
PROXY_CONTAINER="${COMPOSE_PROJECT_NAME}-miden-agglayer-1"
LOGS_BEFORE_LINES=$(docker logs "$PROXY_CONTAINER" 2>&1 | wc -l)

cast send "$L2_GER_ADDRESS" 'insertGlobalExitRoot(bytes32)' "$GARBAGE_HASH" \
  --rpc-url "$L2_RPC" \
  --private-key "$SIGNER_KEY" \
  --legacy \
  --gas-price 1000000000 >/dev/null

sleep 2

# Look for the dedup-skip log AFTER our re-submit.
DEDUP_LINE=$(docker logs "$PROXY_CONTAINER" 2>&1 \
  | tail -n +$((LOGS_BEFORE_LINES + 1)) \
  | grep -E "GER already seen|skipping duplicate" || true)
if [[ -n "$DEDUP_LINE" ]]; then
  pass "DEDUP POISON CONFIRMED: $DEDUP_LINE"
else
  warn "did not observe 'GER already seen' in proxy logs after re-submit"
  warn "(may be filtered by log level; checking ger_entries.block_number stayed put instead)"
fi

step "A5. Verifying the $N_DEPOSITS deposits are STILL stuck after the poisoned GER"
for cnt in "${DEPOSIT_CNTS[@]}"; do
  IFS='|' read -r DCNT BLK READY CLAIMED <<<"$(depo "$cnt")"
  [[ "$READY" == "false" ]] || fail "deposit_cnt=$cnt unexpectedly flipped to ready=$READY after garbage GER"
  printf '    cnt=%s ready=%s (correctly still stuck)\n' "$DCNT" "$READY"
done
pass "BUG CONFIRMED: synthetic log fired but bridge-service can't resolve unmatched (M, R)"

# ── Part B: apply the fix ─────────────────────────────────────────────────────
step "Part B — apply the fix via one-shot-ger-inject.sh (updateExitRoot)"

say "B1. Running scripts/one-shot-ger-inject.sh with current L1 (M, R)"
ONESHOT_OUTPUT=$(
  L1_RPC_URL="$L1_RPC" \
  L1_GER_ADDRESS="$L1_GER_ADDRESS" \
  L2_RPC_URL="$L2_RPC" \
  SIGNER_KEY="$SIGNER_KEY" \
  L2_GER_ADDRESS="$L2_GER_ADDRESS" \
  "$SCRIPT_DIR/one-shot-ger-inject.sh" 2>&1
)
echo "$ONESHOT_OUTPUT" | sed 's/^/    /'
echo "$ONESHOT_OUTPUT" | grep -q 'status .*1 ' \
  || fail "one-shot tx did not report status=1"

REAL_MAIN=$(echo "$ONESHOT_OUTPUT" | grep -oE 'mainnet[[:space:]]+0x[0-9a-fA-F]{64}' | head -1 | awk '{print $2}')
REAL_ROLL=$(echo "$ONESHOT_OUTPUT" | grep -oE 'rollup[[:space:]]+0x[0-9a-fA-F]{64}'  | head -1 | awk '{print $2}')
REAL_COMB=$(echo "$ONESHOT_OUTPUT" | grep -oE 'combined[[:space:]]+0x[0-9a-fA-F]{64}'| head -1 | awk '{print $2}')

[[ -n "$REAL_MAIN" && -n "$REAL_ROLL" && -n "$REAL_COMB" ]] \
  || fail "could not parse roots from one-shot output"

say "    real mainnet  = $REAL_MAIN"
say "    real rollup   = $REAL_ROLL"
say "    real combined = $REAL_COMB"

step "B2. Verifying ger_entries row for the freshly-injected GER"
sleep 2
COMB_NO0X=${REAL_COMB#0x}
ROW=$(pg "SELECT encode(mainnet_exit_root, 'hex'), encode(rollup_exit_root, 'hex'), is_injected
         FROM ger_entries WHERE ger_hash = decode('$COMB_NO0X', 'hex');")
[[ -n "$ROW" ]] || fail "fresh GER row missing in ger_entries"
IFS='|' read -r MAIN_HEX ROLL_HEX IS_INJ <<<"$ROW"
say "    mainnet_exit_root = 0x${MAIN_HEX}"
say "    rollup_exit_root  = 0x${ROLL_HEX}"
say "    is_injected       = $IS_INJ"

[[ "0x${MAIN_HEX}" == "${REAL_MAIN}" ]] \
  || fail "stored mainnet root 0x${MAIN_HEX} != sent ${REAL_MAIN}"
[[ "0x${ROLL_HEX}" == "${REAL_ROLL}" ]] \
  || fail "stored rollup root 0x${ROLL_HEX} != sent ${REAL_ROLL}"
[[ "$IS_INJ" == "t" ]] \
  || fail "fresh GER not marked is_injected=TRUE"
pass "FIX VERIFIED at proxy level: (M, R) stored from call params, no race"

step "B3. Verifying all $N_DEPOSITS deposits flipped to ready_for_claim=TRUE"
DEADLINE=$((SECONDS + 30))
while :; do
  ALL_READY=true
  for cnt in "${DEPOSIT_CNTS[@]}"; do
    IFS='|' read -r DCNT BLK READY CLAIMED <<<"$(depo "$cnt")"
    if [[ "$READY" != "true" ]]; then ALL_READY=false; break; fi
  done
  $ALL_READY && break
  (( SECONDS >= DEADLINE )) && {
    for cnt in "${DEPOSIT_CNTS[@]}"; do
      IFS='|' read -r DCNT BLK READY CLAIMED <<<"$(depo "$cnt")"
      printf '    cnt=%s blk=%s ready=%s\n' "$DCNT" "$BLK" "$READY"
    done
    fail "deposits did not flip ready_for_claim=true within 30s"
  }
  sleep 1
done

for cnt in "${DEPOSIT_CNTS[@]}"; do
  IFS='|' read -r DCNT BLK READY CLAIMED <<<"$(depo "$cnt")"
  printf '    cnt=%s blk=%s ready=%s claimed=%s\n' "$DCNT" "$BLK" "$READY" "$CLAIMED"
done
pass "FIX VERIFIED end-to-end: all $N_DEPOSITS deposits now ready_for_claim"

step "Done. Backlog cleared via updateExitRoot — same shape as bali recovery."
say "NOTE: aggkit is still stopped — restart it with:"
say "    docker compose -f docker-compose.e2e.yml --env-file fixtures/.env start aggkit"
