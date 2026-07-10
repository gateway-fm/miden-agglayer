#!/usr/bin/env bash
# lib-l2l2.sh — shared constants + helpers for the L2<->L2 (Miden <-> "L2B")
# scenarios. SOURCED, not executed. Extracted verbatim from the mechanics proven
# by the monolithic e2e-l2-to-l2.sh so the decomposed simple scenarios
# (e2e-l2l2-forward.sh / e2e-l2l2-back.sh) and the mixed loadtest/chaos tiers all
# share ONE source of truth for the L2B contract topology, GER-propagation waits,
# ready_for_claim polling and the AreClaimsBetweenL2sEnabled nudge-cert dance.
#
# Contract: the SOURCING script sets PROJECT_DIR (repo root) and SCRIPT_DIR, then
#   source "$SCRIPT_DIR/lib-l2l2.sh"
# The lib sources fixtures/.env, defines the colour log helpers (log/step/warn/
# fail/pass), the L2B addresses/topics, and the helper functions below.

# ── Config the sourcing script must have set ────────────────────────────────
: "${PROJECT_DIR:?lib-l2l2.sh: PROJECT_DIR must be set before sourcing}"
: "${SCRIPT_DIR:?lib-l2l2.sh: SCRIPT_DIR must be set before sourcing}"
REPO="$PROJECT_DIR"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
source "$FIXTURES_DIR/.env"

# ── Endpoints ───────────────────────────────────────────────────────────────
L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"          # Miden proxy synthetic RPC
L2B_RPC="${L2B_RPC:-http://localhost:9545}"        # anvil-l2b
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

PG_HOST="${PG_HOST:-localhost}"; PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"; PG_PASS="${PG_PASS:-agglayer}"; PG_DB="${PG_DB:-agglayer_store}"

# ── Compose project auto-detect (worktree dirs derive distinct project names;
#    the l2l2 worktree -> "l2l2", the chaos worktree -> "chaos", main ->
#    "miden-agglayer"). Detect from the live proxy container, same pattern as
#    e2e-l2-to-l2.sh / e2e-l2-to-l1.sh. ───────────────────────────────────────
_DETECTED_PROJECT=$(docker ps --format '{{.Names}}' 2>/dev/null | grep -E -- '-miden-agglayer-1$' | head -1 | sed 's/-miden-agglayer-1$//')
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-${_DETECTED_PROJECT:-miden-agglayer}}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
AGGKIT_L2B_CONTAINER="${AGGKIT_L2B_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-l2b-1}"
NODE_CONTAINER="${NODE_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-node-1}"

# ── L2B contract topology (snapshot-deterministic; see setup-l2b.sh) ─────────
BRIDGE=0xC8cbEBf950B9Df44d987c8619f092beA980fF038      # AgglayerBridge(L2) proxy on BOTH L1 and L2B
GER_L1=0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674       # L1 global exit root (AgglayerGER)
L2B_GER=0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA      # real AgglayerGERL2 proxy on L2B
ROLLUP_MANAGER=0x6c6c009cC348976dB4A908c92B24433d4F6edA43
L2B_NETWORK_ID=2
MIDEN_NETWORK_ID=1
BRIDGE_ADDRESS="${BRIDGE_ADDRESS:-$BRIDGE}"             # L1 bridge (== BRIDGE proxy addr)

# TEST-ONLY keys (kurtosis-cdk standard)
ADMIN=0xE34aaF64b29273B7D567FCFc40544c014EEe9970
ADMIN_KEY=0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625

# Decimals: OPT0/COL are 18-decimal ERC-20s; Miden wraps at 8 -> scale 10^10.
WEI_PER_MIDEN_UNIT=10000000000
TOKEN_SUPPLY=1000000000000000000000000  # 1M tokens @ 18 decimals

BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)
pgq() { "${PSQL[@]}" -c "$1" 2>/dev/null; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."; sleep "$interval"
    done
    echo ""
}

l2_tip() {
    curl -sf -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'
}

# find_deposit <dest_addr> <source_network_id> <orig_addr_lower> — newest match.
find_deposit() {
    local dest="$1" netid="$2" orig="$3"
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$dest?limit=100" 2>/dev/null | python3 -c "
import json, sys
try: d = json.load(sys.stdin)
except Exception: sys.exit(0)
best = None
for dep in d.get('deposits', []):
    if dep.get('network_id') != $netid: continue
    if (dep.get('orig_addr') or '').lower() != '$orig': continue
    if best is None or dep.get('deposit_cnt', 0) > best.get('deposit_cnt', 0):
        best = dep
if best: print(json.dumps(best))
" || true
}
dep_field() { echo "$1" | python3 -c "import json,sys; print(json.load(sys.stdin)['$2'])"; }

claim_event_rows() {
    local gi_hex
    gi_hex=$(python3 -c "print(format(int('$1'),'064x'))")
    pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${gi_hex}%';"
}

# ── L2B stack lifecycle ──────────────────────────────────────────────────────
# _l2l2_stack_ready — 0 when the L2B overlay is already up + rollup #2
# registered + bridge-service indexing (so ensure can SKIP the costly bring-up).
_l2l2_stack_ready() {
    cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1 || return 1
    local rc; rc=$(cast call "$ROLLUP_MANAGER" 'rollupCount()(uint32)' --rpc-url "$L1_RPC" 2>/dev/null | awk '{print $1}')
    [[ "${rc:-0}" -ge 2 ]] || return 1
    [[ "$(cast code "$BRIDGE" --rpc-url "$L2B_RPC" 2>/dev/null | head -c 4)" == "0x60" ]] || return 1
    curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1 || return 1
    return 0
}

# l2l2_ensure_stack — idempotent leg 0. If the L2B overlay is already up (e.g. a
# reused stack) it SKIPS; otherwise it generates configs, brings up the overlay
# under the current compose project, registers rollup #2 and funds the L2B claim
# sponsor. Requires the BASE stack (make e2e-up) already healthy.
l2l2_ensure_stack() {
    if _l2l2_stack_ready; then
        log "L2B overlay already up (project=$COMPOSE_PROJECT_NAME, rollup #2 registered) — reusing"
        return 0
    fi
    step "Leg 0: bringing up the L2B overlay + registering rollup #2"
    "$SCRIPT_DIR/gen-l2b-configs.sh"
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" docker compose \
        -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
        --env-file "$REPO/fixtures/.env" up -d anvil-l2b aggkit-l2b agglayer bridge-service
    wait_for "anvil-l2b reachable at $L2B_RPC" \
        "cast chain-id --rpc-url '$L2B_RPC' >/dev/null 2>&1" 60 2
    L2B_RPC="$L2B_RPC" "$SCRIPT_DIR/setup-l2b.sh"
    : "${SPONSOR_PRIVATE_KEY:?fixtures/.env must define SPONSOR_PRIVATE_KEY}"
    local sponsor_addr; sponsor_addr=$(cast wallet address --private-key "$SPONSOR_PRIVATE_KEY")
    cast rpc anvil_setBalance "$sponsor_addr" 0x21e19e0c9bab2400000 --rpc-url "$L2B_RPC" >/dev/null
    log "  claim sponsor $sponsor_addr funded on L2B"
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" docker compose \
        -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
        --env-file "$REPO/fixtures/.env" up -d --force-recreate bridge-service
    wait_for "bridge-service HTTP API up (post-recreate)" \
        "curl -sf '$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000' >/dev/null" 120 3
    pass "Leg 0 done: rollup #2 registered, L2B bridge/GER live, bridge-service indexing both networks"
}

# l2l2_miden_identities — read the proxy's bridge account ids and provision the
# isolated destination/bridge-out wallet. Sets BRIDGE_ID, FAUCET_ETH, WALLET_ID,
# WALLET_HEX, DEST_ADDR. B2AGG_STORE_DIR must be set by the caller (shared across
# forward+back so the wallet that receives the wrapped token can later spend it).
l2l2_miden_identities() {
    local accounts=""
    for _ in $(seq 1 30); do
        accounts=$(docker exec "$AGGLAYER_CONTAINER" \
            cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) && break
        sleep 5
    done
    [[ -n "$accounts" ]] || fail "miden-agglayer not initialized within 150s (bridge_accounts.toml absent)"
    BRIDGE_ID=$(echo "$accounts" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
    FAUCET_ETH=$(echo "$accounts" | grep faucet_eth | sed 's/.*= "//;s/"//')
    [[ -n "$BRIDGE_ID" && -n "$FAUCET_ETH" ]] || fail "could not read bridge account ids"
    source "$SCRIPT_DIR/lib-isolated-wallet.sh"
    provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH" \
        || fail "could not provision isolated bridge-out wallet"
    log "Wallet: $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
    log "Dest:   $DEST_ADDR (zero-padded, network $MIDEN_NETWORK_ID)"
}

# ── Nudge-cert mechanics (AreClaimsBetweenL2sEnabled) ────────────────────────
# The upstream ClaimTxManager scans L2->L2 claims only when a NEW rollup exit
# root lands on L1. A single L2->L2 transfer sits ready but unscanned until the
# NEXT certificate settles. nudge_cert forces that next cycle by bridging 1 wei
# of a dedicated NDG token L2B->L1 (dest L1 => no claimtxman/Miden side effects).
# Deploy NDG once per script via l2l2_deploy_nudge_token (sets NDG).
l2l2_deploy_nudge_token() {
    local out
    out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
        --private-key "$ADMIN_KEY" --broadcast \
        --constructor-args "NudgeToken" "NDG" 18 1000000000000000000 2>&1)
    NDG=$(echo "$out" | grep "Deployed to:" | awk '{print $NF}')
    [[ -n "$NDG" ]] || fail "NDG deploy failed: $(echo "$out" | tail -2)"
    log "  nudge token NDG deployed on L2B: $NDG"
}
nudge_cert() {
    cast send "$NDG" "approve(address,uint256)" "$BRIDGE" 1 \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "NDG approve (nudge)"
    cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        0 "$ADMIN" 1 "$NDG" true 0x \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "NDG bridgeAsset (nudge)"
    log "  nudge cert sent (1 wei NDG L2B->L1) — wakes the L2->L2 claim scan"
}
# nudge_until <desc> <check-cmd> — nudge, poll up to 75s, repeat NUDGE_TRIES
# rounds (a single nudge can lose a second race vs the trusted-GER sync).
nudge_until() {
    local desc="$1" check="$2" tries="${NUDGE_TRIES:-6}" t waited
    for t in $(seq 1 "$tries"); do
        nudge_cert
        waited=0
        while [[ $waited -lt 75 ]]; do
            if ( set +o pipefail; eval "$check" ) 2>/dev/null; then
                log "  nudge round $t unblocked: $desc"; return 0
            fi
            sleep 5; waited=$((waited + 5)); echo -n "."
        done
        echo ""
        warn "nudge round $t/$tries did not unblock: $desc — re-nudging"
    done
    return 1
}
