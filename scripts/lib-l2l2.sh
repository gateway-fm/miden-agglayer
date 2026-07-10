#!/usr/bin/env bash
# lib-l2l2.sh — shared constants + helpers for the L2<->L2 (Miden <-> "L2B")
# scenarios. SOURCED, not executed. THE single source of truth consumed by the
# canonical l2l2 group (e2e-l2l2-forward.sh / e2e-l2l2-clash.sh / e2e-l2l2-back.sh)
# and the mixed loadtest/chaos tiers, so they all
# share ONE definition of the L2B contract topology, GER-propagation waits,
# ready_for_claim polling, the client-submitted L2->L2 claimAsset flow, and the
# nudge-cert dance that drives L2B cert cycles so the covering GER reaches Miden.
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
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"          # Miden bridge-service (indexes L1 + Miden)
L2B_BRIDGE_SERVICE_URL="${L2B_BRIDGE_SERVICE_URL:-http://localhost:28080}"   # ISOLATED L2B bridge-service (indexes L1 + L2B)

PG_HOST="${PG_HOST:-localhost}"; PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"; PG_PASS="${PG_PASS:-agglayer}"; PG_DB="${PG_DB:-agglayer_store}"

# ── Compose project auto-detect (worktree dirs derive distinct project names;
#    the l2l2 worktree -> "l2l2", the chaos worktree -> "chaos", main ->
#    "miden-agglayer"). Detect from the live proxy container, same pattern as
#    e2e-l2-to-l1.sh. ──────────────────────────────────────────────────────────
# `set +o pipefail` + `|| true`: with no matching proxy container `grep` exits 1
# (and `head` closing the pipe early can SIGPIPE `grep`=141) — under the caller's
# `set -euo pipefail` a bare `_DETECTED_PROJECT=$(… failing pipeline …)` would exit
# the script BEFORE the explicit COMPOSE_PROJECT_NAME / fallback below can apply.
_DETECTED_PROJECT=$( ( set +o pipefail; docker ps --format '{{.Names}}' 2>/dev/null | grep -E -- '-miden-agglayer-1$' | head -1 | sed 's/-miden-agglayer-1$//' ) || true )
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

# `cmd` / `check` are STATIC, TEST-AUTHORED condition strings assembled inside
# this repo's e2e scripts — never external/runtime input. `eval` (not `bash -c`)
# is deliberate: the conditions defer $(pg ...) / $(l2b_* ...) calls that are
# shell FUNCTIONS in the sourcing script, invisible to a child bash (no
# `export -f`). The sub-shell isolates `set +o pipefail` and stderr. This is the
# same idiom as scripts/e2e-bridge-loadtest-isolated.sh::wait_for.
# ── Predicate helpers for wait_for/nudge_until ──────────────────────────────
# Named predicate functions so the pollers invoke a FUNCTION (+ args) rather than
# eval() a runtime-built string — no dynamic string execution (closes the eval
# audit finding) and predicate failure semantics are explicit. Each returns 0 when
# satisfied. They run inside the pollers' `( set +o pipefail; … )` subshell.
_pred_rpc_reachable() { cast chain-id --rpc-url "$1" >/dev/null 2>&1; }          # <rpc>
_pred_http_ok()       { curl -sf "$1" >/dev/null 2>&1; }                          # <url>
_pred_pg_eq()         { [[ "$(pgq "$1")" == "$2" ]]; }                            # <sql> <expected>
_pred_pg_gt()         { local v; v=$(pgq "$1"); [[ "${v:-0}" -gt "$2" ]]; }       # <sql> <threshold>
_pred_log_grep()      { docker logs --since "$2" "$1" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' | grep -qiE "$3"; }  # <container> <since> <ere>
_pred_deposit_ready() {                                                          # <dest> <netid> <orig> [wanted_dest_net] [service-url]
    local dep; dep=$(find_deposit "$1" "$2" "$3" "${5:-$BRIDGE_SERVICE_URL}")
    [[ -n "$dep" ]] || return 1
    printf '%s' "$dep" | python3 -c "
import json, sys
d = json.load(sys.stdin)
want = sys.argv[1] if len(sys.argv) > 1 and sys.argv[1] != '' else None
sys.exit(0 if bool(d.get('ready_for_claim')) and (want is None or str(d.get('dest_net')) == want) else 1)
" "${4:-}"
}

# wait_for <desc> <timeout> <interval> <predicate-fn> [args...]
# Polls the NAMED predicate function until it succeeds or <timeout>s elapse.
wait_for() {
    local desc="$1" timeout="$2" interval="${3:-5}"; shift 3
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! ( set +o pipefail; "$@" ) 2>/dev/null; do
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
    # <dest> <netid> <orig> [service-url]. Per-rollup isolation: a deposit is
    # indexed by the bridge-service watching the chain it was MADE on — L2B
    # deposits (network_id=2) live in the L2B service (pass $L2B_BRIDGE_SERVICE_URL),
    # Miden deposits (network_id=1) in the default Miden service. L1 deposits
    # (network_id=0) are indexed by both, so either URL works.
    local dest="$1" netid="$2" orig="$3" svc="${4:-$BRIDGE_SERVICE_URL}"
    curl -sf "$svc/bridges/$dest?limit=100" 2>/dev/null | python3 -c "
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

# _pred_submit_forward_claim <fwd_cnt> <global_index> <orig_net> <orig_token> <dest_net> <dest_addr> <amount_wei> <metadata>
# Canonical forward (L2B->Miden) claim. With per-rollup bridge-service isolation
# (canonical kurtosis-cdk: ONE bridge-service per chain, each indexing L1 + ONLY its
# own L2) the Miden service does NOT index L2B, so nothing auto-submits this claim —
# the shared service that auto-claimed it was the non-canonical shortcut, and canonical
# kurtosis sets NO AreClaimsBetweenL2sEnabled anywhere (claimtxman is L1->L2 only).
# So the claim is client-submitted, exactly like the back leg: fetch the L2B deposit's
# proof from the L2B service (which is the only thing indexing L2B) and submit a
# proof-backed claimAsset to the Miden proxy (:8546), which accepts it
# (worker_handle_claim_asset) and auto-creates the foreign faucet + mints. The proof
# verifies against L1's GER, but is SERVED by the source (L2B) service.
# Reports success iff the synthetic ClaimEvent row for this global index exists.
# Re-fetches a FRESH proof each call: the proxy gates on has_seen_ger(combined(main,
# rollup)) (service_send_raw_txn.rs C6) and rejects until Miden has the covering GER,
# which LAGS the deposit turning ready — nudge_until drives L2B cert cycles to
# propagate it, and each retry re-reads the (advancing) served proof.
_pred_submit_forward_claim() {
    local cnt="$1" gi="$2" onet="$3" otok="$4" dnet="$5" daddr="$6" amt="$7" meta="$8"
    [[ "$(claim_event_rows "$gi")" -ge 1 ]] && return 0        # already landed (idempotent)
    local pj mer rer smtL smtR
    pj=$(curl -sf "$L2B_BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$cnt&net_id=$L2B_NETWORK_ID" 2>/dev/null || true)
    [[ -n "$pj" ]] || return 1
    mer=$(echo "$pj" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])" 2>/dev/null || true)
    rer=$(echo "$pj" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])" 2>/dev/null || true)
    smtL=$(echo "$pj" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
    smtR=$(echo "$pj" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
    [[ -n "$mer" && -n "$rer" && -n "$smtL" && -n "$smtR" ]] || return 1
    # --legacy + explicit gas: the Miden proxy's synthetic RPC does NOT implement
    # eth_feeHistory (cast's default EIP-1559 fee path -32601s before submitting) and
    # eth_estimateGas returns 0 — so force legacy pricing off eth_gasPrice (served: 1
    # gwei) and a fixed gas limit. --async: don't depend on the proxy's synthetic-
    # receipt semantics; the ClaimEvent row (checked below / next poll) is the source
    # of truth for both the sync and writer-worker dispatch paths. A premature submit
    # (covering GER not yet seen) just fails and we retry with a fresher proof.
    cast send --async --rpc-url "$L2_RPC" --private-key "$ADMIN_KEY" \
        --legacy --gas-price "${MIDEN_CLAIM_GAS_PRICE:-1000000000}" --gas-limit "${MIDEN_CLAIM_GAS_LIMIT:-3000000}" "$BRIDGE" \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$smtL" "$smtR" "$gi" "$mer" "$rer" "$onet" "$otok" "$dnet" "$daddr" "$amt" "$meta" >/dev/null 2>&1 || true
    [[ "$(claim_event_rows "$gi")" -ge 1 ]]
}

# submit_back_claim <cnt> <gi> <orig_net> <orig_token> <dest_net> <dest_addr> <amount_wei> <metadata>
# Client-submit a Miden->L2B (back) claim: fetch the Miden deposit's proof from the
# Miden bridge-service (net_id=1) and submit a proof-backed claimAsset to the REAL
# anvil-l2b bridge (EIP-1559 fine — a real EVM chain, unlike the proxy, so NO --legacy).
# The served proof's roots + their L2B-GER injection LAG the deposit turning ready and
# the exit tree advances, so each attempt RE-FETCHES a fresh proof; `cast send` gas-
# estimates first, so a not-yet-settleable claim reverts FAST without submitting ->
# retry with a newer proof until the covering GER is injected on L2B. Prints the claim
# tx hash on success (empty + rc 1 if it never settles within the retry budget). Shared
# by e2e-l2l2-back.sh (leg 4) and e2e-loadtest-mixed.sh (back ops).
submit_back_claim() {
    local cnt="$1" gi="$2" onet="$3" otok="$4" dnet="$5" daddr="$6" amt="$7" meta="$8"
    local tries="${BACK_CLAIM_TRIES:-30}" interval="${BACK_CLAIM_INTERVAL:-15}"
    local attempt pj mer rer smtL smtR out txh
    for attempt in $(seq 1 "$tries"); do
        pj=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$cnt&net_id=$MIDEN_NETWORK_ID" 2>/dev/null || true)
        if [[ -n "$pj" ]]; then
            mer=$(echo "$pj" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])" 2>/dev/null || true)
            rer=$(echo "$pj" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])" 2>/dev/null || true)
            smtL=$(echo "$pj" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
            smtR=$(echo "$pj" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
            if [[ -n "$smtL" && -n "$smtR" && -n "$mer" ]]; then
                out=$(cast send --rpc-url "$L2B_RPC" --private-key "$ADMIN_KEY" --json "$BRIDGE" \
                    'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
                    "$smtL" "$smtR" "$gi" "$mer" "$rer" "$onet" "$otok" "$dnet" "$daddr" "$amt" "$meta" 2>/dev/null || true)
                txh=$(echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('transactionHash','') if str(d.get('status','')) in ('0x1','1','true') else '')" 2>/dev/null || true)
                [[ -n "$txh" ]] && { echo "$txh"; return 0; }
            fi
        fi
        sleep "$interval"
    done
    return 1
}

# ── L2B stack lifecycle ──────────────────────────────────────────────────────
# _l2l2_stack_ready — 0 when the L2B overlay is already up + rollup #2
# registered + bridge-service indexing (so ensure can SKIP the costly bring-up).
_l2l2_stack_ready() {
    cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1 || return 1
    local rc; rc=$(cast call "$ROLLUP_MANAGER" 'rollupCount()(uint32)' --rpc-url "$L1_RPC" 2>/dev/null | awk '{print $1}')
    [[ "${rc:-0}" -ge 2 ]] || return 1
    # Identity, not just presence: rollup #2 must be OUR L2B (chainID 31338), the
    # L2B bridge must self-report networkID 2 (a leftover/foreign bridge at this
    # address would not), and the real GER contract must have code — otherwise a
    # stale/wrong overlay would be silently REUSED. These are cheap and distinguish
    # a correct reused overlay from one that must be re-brought-up + reconciled.
    local r2chain
    r2chain=$( ( set +o pipefail; cast call "$ROLLUP_MANAGER" \
        'rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint8,uint8)' 2 \
        --rpc-url "$L1_RPC" 2>/dev/null | sed -n '2p' | awk '{print $1}' ) || true )
    [[ "$r2chain" == "31338" ]] || return 1
    [[ "$(cast code "$BRIDGE" --rpc-url "$L2B_RPC" 2>/dev/null | head -c 4)" == "0x60" ]] || return 1
    [[ "$(cast code "$L2B_GER" --rpc-url "$L2B_RPC" 2>/dev/null | head -c 4)" == "0x60" ]] || return 1
    [[ "$(cast call "$BRIDGE" 'networkID()(uint32)' --rpc-url "$L2B_RPC" 2>/dev/null | awk '{print $1}')" == "2" ]] || return 1
    # BOTH isolated bridge-services must be serving: Miden (:18080) and L2B (:28080).
    curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1 || return 1
    curl -sf "$L2B_BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1 || return 1
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
    # Bring up the L2B overlay: anvil-l2b, aggkit-l2b, the config-swapped agglayer,
    # the base (Miden) bridge-service, AND the isolated L2B bridge-service + its DB.
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" docker compose \
        -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
        --env-file "$REPO/fixtures/.env" up -d anvil-l2b aggkit-l2b agglayer bridge-service postgres-l2b bridge-service-l2b
    wait_for "anvil-l2b reachable at $L2B_RPC" 60 2 _pred_rpc_reachable "$L2B_RPC"
    L2B_RPC="$L2B_RPC" "$SCRIPT_DIR/setup-l2b.sh"
    : "${SPONSOR_PRIVATE_KEY:?fixtures/.env must define SPONSOR_PRIVATE_KEY}"
    local sponsor_addr; sponsor_addr=$(cast wallet address --private-key "$SPONSOR_PRIVATE_KEY")
    cast rpc anvil_setBalance "$sponsor_addr" 0x21e19e0c9bab2400000 --rpc-url "$L2B_RPC" >/dev/null
    log "  claim sponsor $sponsor_addr funded on L2B"
    # Re-create ONLY the L2B bridge-service so it (re)indexes network 2 now that
    # rollup #2 is registered + the L2B bridge/GER exist. The Miden bridge-service
    # indexes L1 + Miden and is unaffected by rollup #2, so it is left running.
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" docker compose \
        -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
        --env-file "$REPO/fixtures/.env" up -d --force-recreate bridge-service-l2b
    wait_for "L2B bridge-service HTTP API up (post-recreate)" 120 3 \
        _pred_http_ok "$L2B_BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000"
    pass "Leg 0 done: rollup #2 registered, L2B bridge/GER live, isolated Miden + L2B bridge-services indexing"
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

# ── Nudge-cert mechanics (drive L2B cert cycles) ─────────────────────────────
# An L2B->Miden claim can only be accepted once Miden has SEEN the GER covering the
# deposit's L2B exit root (proxy C6 has_seen_ger gate). On a quiet L2B chain the
# covering exit root reaches L1 (and then Miden) only when the NEXT certificate
# settles. nudge_cert forces that next cycle by bridging 1 wei of a dedicated NDG
# token L2B->L1 (dest L1 => no claimtxman/Miden side effects); the client-submit
# claim predicate then re-fetches a fresh proof and retries until it sticks.
# Deploy NDG once per script via l2l2_deploy_nudge_token (sets NDG).
l2l2_deploy_nudge_token() {
    local out
    # `|| true` on the substitution so a forge failure is CAPTURED (not a set -e
    # exit), and the grep runs in a pipefail-disabled subshell so a changed/absent
    # "Deployed to:" line yields an empty NDG that reaches the explicit diagnostic
    # below — same fix already applied to the OPT0 parser.
    out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
        --private-key "$ADMIN_KEY" --broadcast \
        --constructor-args "NudgeToken" "NDG" 18 1000000000000000000 2>&1) || true
    NDG=$( ( set +o pipefail; echo "$out" | grep "Deployed to:" | awk '{print $NF}' ) || true )
    NDG_DEPLOY_TX=$( ( set +o pipefail; echo "$out" | awk '/Transaction hash:/{print $NF; exit}' ) || true )
    [[ -n "$NDG" ]] || fail "NDG deploy failed: $(echo "$out" | tail -3)"
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
# nudge_until <desc> <predicate-fn> [args...] — nudge a cert cycle, then poll the
# NAMED predicate function (no eval) for up to 75s; repeat up to NUDGE_TRIES times.
nudge_until() {
    local desc="$1"; shift
    local tries="${NUDGE_TRIES:-6}" t waited
    for t in $(seq 1 "$tries"); do
        nudge_cert
        waited=0
        while [[ $waited -lt 75 ]]; do
            if ( set +o pipefail; "$@" ) 2>/dev/null; then
                log "  nudge round $t unblocked: $desc"; return 0
            fi
            sleep 5; waited=$((waited + 5)); echo -n "."
        done
        echo ""
        warn "nudge round $t/$tries did not unblock: $desc — re-nudging"
    done
    return 1
}

# ── STACK VALIDATION (preflight, fail-loud) ─────────────────────────────────
# l2l2_validate_stack — asserts the L2B overlay is COMPLETE + HEALTHY before any
# test step runs, so nothing executes against a half-configured/port-colliding
# stack. Every check prints a PASS/FAIL line; failures are accumulated (so the
# operator sees ALL problems at once) then the whole preflight fails loud.
# CreateNewAggchain event topic (RollupManager) — also used by evidence_rollup_register.
CREATE_AGGCHAIN_TOPIC="0x144e3f9b5c63682a3bb7e9ad31e99c043890d3d540cd79dcebc3b5bdfba94c9b"
_PF_FAILS=0
_pf_pass() { echo -e "  ${GREEN}PASS${NC} $*"; }
_pf_fail() { echo -e "  ${RED}FAIL${NC} $*"; _PF_FAILS=$((_PF_FAILS + 1)); }

# _pf_log_has <container> <ere-pattern> <tail-lines> <desc> — assert a pattern
# appears in a container's recent logs, with retries. Uses a BOUNDED --tail (not
# the whole multi-100k-line log) so it's fast AND immune to a transient docker
# hiccup on a busy shared host returning an empty/partial read (which otherwise
# flakes an all-log capture). Patterns checked here recur throughout the log, so
# a healthy stack always has them within the tail window.
# tail="all" streams the whole log but grep -q short-circuits at the first match
# (cheap when the pattern appears early, e.g. a start-up GER injection whose later
# recurrences are buried under unrelated high-volume module spam).
_pf_log_has() {
    local container="$1" pattern="$2" tail="$3" desc="$4" i
    local -a args=(logs)
    [[ "$tail" != "all" ]] && args+=(--tail "$tail")
    args+=("$container")
    for i in 1 2 3 4 5; do
        # Subshell + `set +o pipefail`: `grep -q` short-circuits on the first
        # match and SIGPIPE-kills `docker logs`/`sed` upstream; under the caller's
        # `set -o pipefail` that 141 would masquerade as a pipeline failure and
        # spuriously fail an otherwise-passing check. Same idiom as wait_for.
        if ( set +o pipefail; docker "${args[@]}" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' | grep -qE "$pattern" ); then
            _pf_pass "$desc"
            return 0
        fi
        sleep 2
    done
    _pf_fail "$desc — pattern '/$pattern/' absent from ${tail} log lines of $container after 5 tries"
}

# Container is OK if its healthcheck reports "healthy" OR (no healthcheck AND it
# is "running"). Anything else (starting/unhealthy/exited/absent) is a failure.
_pf_container() {
    local svc="$1" name="${COMPOSE_PROJECT_NAME}-$1-1" st
    st=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$name" 2>/dev/null) || st="absent"
    case "$st" in
        healthy|running) _pf_pass "container $svc ($name): $st" ;;
        *)               _pf_fail "container $svc ($name): $st" ;;
    esac
}

# Detect a foreign container squatting a host port THIS stack needs (the
# leftover-:5433/:5434 hygiene failure). Owner must belong to our project.
_pf_port_owner_ok() {
    local port="$1" want="$2" owner
    owner=$(docker ps --format '{{.Names}} {{.Ports}}' 2>/dev/null | grep -E "(:|^)$port->" | awk '{print $1}' | head -1)
    if [[ -z "$owner" ]]; then
        _pf_fail "host port $port ($want) not published by any container"
    elif [[ "$owner" != "$COMPOSE_PROJECT_NAME-"* ]]; then
        _pf_fail "host port $port ($want) held by FOREIGN container '$owner' (expected project '$COMPOSE_PROJECT_NAME') — leftover/colliding stack"
    else
        _pf_pass "host port $port ($want) owned by $owner"
    fi
}

# _pf_bridge_fresh — assert bridge-service is actively logging (newest log line
# within PF_BRIDGE_FRESH_MAX seconds). A frozen synchronizer stops emitting lines
# while the container stays "Up"; this is the liveness gate that catches it.
_pf_bridge_fresh() {
    local container="${1:-${COMPOSE_PROJECT_NAME}-bridge-service-1}" iso ts now age
    local label="${2:-bridge-service}"
    iso=$( ( set +o pipefail; docker logs --tail 8 "$container" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' \
        | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z' | tail -1 ) || true )
    if [[ -z "$iso" ]]; then
        _pf_fail "$label liveness: no parseable log timestamp in recent output (frozen?)"
        return
    fi
    ts=$(python3 -c "import datetime; print(int(datetime.datetime.fromisoformat('${iso}'.replace('Z','+00:00')).timestamp()))" 2>/dev/null || true)
    now=$(date -u +%s)
    if [[ -z "$ts" ]]; then
        _pf_fail "$label liveness: unparsable log timestamp '$iso'"
        return
    fi
    age=$(( now - ts ))
    if [[ "$age" -le "${PF_BRIDGE_FRESH_MAX:-240}" ]]; then
        _pf_pass "$label actively syncing (newest log line ${age}s ago)"
    else
        _pf_fail "$label FROZEN — newest log line is ${age}s old (>${PF_BRIDGE_FRESH_MAX:-240}s); synchronizer wedged, deposits will never reach ready_for_claim"
    fi
}

# _pf_sync_lag <net-id> <rpc> <label> — assert bridge-service's synchronizer for a
# network has CAUGHT UP. A synchronizer can keep logging (passes freshness) yet be
# stuck re-checking one block and never advance (observed after a chain reset
# desynced the claimtxman); such a stall stops new deposits from ever reaching
# ready_for_claim.
#
# We read the synchronizer's OWN authoritative catch-up state from its bridge_db
# `sync.status` (network_id, percentage, remaining_blocks, synced) — updated every
# sync cycle. This replaces the earlier "newest checkReorg block vs chain tip"
# heuristic, which FALSE-POSITIVED on a quiet fresh L1: sync.block / checkReorg only
# track EVENT-BEARING blocks, so with sparse L1 GER events the checkReorg block sits
# thousands of (auto-mined, empty) blocks below the tip even when fully synced.
# sync.status.synced is immune to that and still flips false when a reset wedges the
# synchronizer mid-reorg (remaining_blocks climbs / synced=false). $rpc is unused
# now but kept for call-site compatibility. Retries for PF_SYNC_SETTLE seconds so a
# legitimate initial catch-up right after (re)create isn't flagged as a wedge.
_pf_sync_lag() {
    # <net-id> <rpc> <label> [pg-container]. Per-rollup isolation: net 2 (L2B)
    # sync.status lives in the isolated L2B bridge_db (postgres-l2b); net 0/1 in the
    # base bridge_db (postgres). Pass the right container per network.
    local net="$1" _rpc="$2" label="$3" pg="${4:-${COMPOSE_PROJECT_NAME}-postgres-1}"
    local max="${PF_BRIDGE_LAG_MAX:-400}" deadline row synced remaining
    deadline=$(( $(date +%s) + ${PF_SYNC_SETTLE:-90} ))
    while :; do
        row=$( ( set +o pipefail; docker exec "$pg" psql -U bridge_user -d bridge_db -tAX \
            -c "SELECT synced||'|'||remaining_blocks FROM sync.status WHERE network_id=$net" 2>/dev/null ) | tr -d '[:space:]' )
        synced="${row%%|*}"; remaining="${row##*|}"
        [[ "$synced" == "true" && "${remaining:-999999}" -le "$max" ]] && break
        [[ $(date +%s) -ge $deadline ]] && break
        sleep 3
    done
    if [[ -z "$row" ]]; then
        _pf_fail "bridge-service $label sync: no sync.status row for network $net in bridge_db (synchronizer not started?)"
    elif [[ "$synced" == "true" && "${remaining:-999999}" -le "$max" ]]; then
        _pf_pass "bridge-service $label synced (sync.status: synced=$synced remaining_blocks=$remaining)"
    else
        _pf_fail "bridge-service $label sync STALLED — sync.status synced=${synced:-?} remaining_blocks=${remaining:-?} after ${PF_SYNC_SETTLE:-90}s (>$max); synchronizer wedged, deposits won't reach ready_for_claim"
    fi
}

l2l2_validate_stack() {
    _PF_FAILS=0
    step "PREFLIGHT: validating l2l2 stack (project=$COMPOSE_PROJECT_NAME)"

    # Fixtures the stack cannot come up without (missing l1-raw-txs.txt = a
    # worktree that was never `make e2e-setup`).
    if [[ -s "$FIXTURES_DIR/l1-raw-txs.txt" ]]; then
        _pf_pass "fixture l1-raw-txs.txt present ($(wc -l <"$FIXTURES_DIR/l1-raw-txs.txt") lines)"
    else
        _pf_fail "fixture l1-raw-txs.txt missing/empty — run 'make e2e-setup' in this worktree"
    fi

    # (a) all l2l2 containers healthy
    local c
    for c in miden-agglayer miden-node tx-prover anvil anvil-l2b aggkit aggkit-l2b \
             bridge-service agglayer postgres agglayer-postgres; do
        _pf_container "$c"
    done
    # port-collision hygiene for the ports the flows dial
    _pf_port_owner_ok 8545 anvil-L1
    _pf_port_owner_ok 9545 anvil-l2b
    _pf_port_owner_ok 18080 bridge-service
    _pf_port_owner_ok 5434 agglayer-postgres

    # (b) rollup #2 registered on L1
    local rd sovc rchain rvtype
    rd=$(cast call "$ROLLUP_MANAGER" \
        'rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint64,uint64,uint8)' \
        "$L2B_NETWORK_ID" --rpc-url "$L1_RPC" 2>/dev/null) || rd=""
    sovc=$(echo "$rd"  | sed -n '1p' | awk '{print $1}')
    rchain=$(echo "$rd" | sed -n '2p' | awk '{print $1}')
    rvtype=$(echo "$rd" | sed -n '12p' | awk '{print $1}')
    if [[ -n "$sovc" && "$sovc" != "0x0000000000000000000000000000000000000000" ]]; then
        _pf_pass "rollup #$L2B_NETWORK_ID sovereignRollupContract=$sovc"
    else
        _pf_fail "rollup #$L2B_NETWORK_ID sovereignRollupContract is zero/absent (not registered)"
    fi
    [[ "$rchain" == "31338" ]] && _pf_pass "rollup #$L2B_NETWORK_ID rollupChainID=31338" \
        || _pf_fail "rollup #$L2B_NETWORK_ID rollupChainID=${rchain:-<none>}, expected 31338"
    [[ "$rvtype" == "2" ]] && _pf_pass "rollup #$L2B_NETWORK_ID rollupVerifierType=2 (ALGateway)" \
        || _pf_fail "rollup #$L2B_NETWORK_ID rollupVerifierType=${rvtype:-<none>}, expected 2"

    # (c) L2B bridge + GER have code deployed on :9545
    local bcode gcode
    bcode=$(cast code "$BRIDGE" --rpc-url "$L2B_RPC" 2>/dev/null | head -c 4)
    gcode=$(cast code "$L2B_GER" --rpc-url "$L2B_RPC" 2>/dev/null | head -c 4)
    [[ "$bcode" == "0x60" || "$bcode" == "0x36" || "$bcode" == "0x73" ]] \
        && _pf_pass "L2B bridge $BRIDGE has code on :9545" \
        || _pf_fail "L2B bridge $BRIDGE has NO code on :9545 (got '${bcode:-<none>}')"
    [[ "$gcode" == "0x60" || "$gcode" == "0x36" || "$gcode" == "0x73" ]] \
        && _pf_pass "L2B GER $L2B_GER has code on :9545" \
        || _pf_fail "L2B GER $L2B_GER has NO code on :9545 (got '${gcode:-<none>}')"

    # (d) ISOLATED per-rollup bridge-services: the base (Miden) service indexes
    # L1 + Miden (NetworkID 0 + 1); the SEPARATE L2B service indexes L1 + L2B
    # (NetworkID 0 + 2). Each is checked on its OWN container/DB.
    _pf_log_has "${COMPOSE_PROJECT_NAME}-bridge-service-1" 'NetworkID: 1[,)]' 8000 \
        "Miden bridge-service indexing network 1"
    _pf_log_has "${COMPOSE_PROJECT_NAME}-bridge-service-l2b-1" 'NetworkID: 2[,)]' 8000 \
        "L2B bridge-service indexing network 2"
    # ...and it is CURRENTLY advancing, not frozen. A wedged synchronizer keeps
    # the container "Up" and its historical NetworkID:1/2 lines intact (so the two
    # checks above still pass) yet indexes nothing new — deposits never reach
    # ready_for_claim. Two liveness gates: (i) newest log line is fresh (catches a
    # total log-freeze); (ii) each synchronizer is near its chain tip (catches a
    # stuck-but-still-logging synchronizer).
    _pf_bridge_fresh "${COMPOSE_PROJECT_NAME}-bridge-service-1"     "Miden bridge-service"
    _pf_bridge_fresh "${COMPOSE_PROJECT_NAME}-bridge-service-l2b-1" "L2B bridge-service"
    # net 0/1 on the base bridge_db (postgres); net 2 on the isolated L2B bridge_db (postgres-l2b)
    _pf_sync_lag 0 "$L1_RPC"  "L1 (Miden svc)"  "${COMPOSE_PROJECT_NAME}-postgres-1"
    _pf_sync_lag 2 "$L2B_RPC" "L2B (L2B svc)"   "${COMPOSE_PROJECT_NAME}-postgres-l2b-1"

    # (e) aggkit-l2b aggoracle alive — GER injection into L2B GER. A quiet stack
    # legitimately has no RECENT inject (aggoracle only fires on a new L1 GER), so
    # a historical injection proves the component is wired + working; the running
    # container was already asserted in (a). Streamed full-log grep short-circuits
    # at the first (early) injection.
    _pf_log_has "${COMPOSE_PROJECT_NAME}-aggkit-l2b-1" 'inject GER transaction (submitted|already exists)' all \
        "aggkit-l2b aggoracle alive (GER injection observed)"

    if [[ "$_PF_FAILS" -gt 0 ]]; then
        fail "PREFLIGHT FAILED — $_PF_FAILS check(s) failed; refusing to run l2l2 tests against a half-configured stack"
    fi
    pass "PREFLIGHT PASSED — l2l2 stack healthy, rollup #2 registered, both networks indexed"
}

# ── RUNTIME EVIDENCE — record every on-chain action to a durable NDJSON file ──
# One line per action: {step,direction,chain,kind,tx_hash,block,contract,status,extra}.
# The file is per-RUN (EVIDENCE_RUN_TS pinned by the caller so forward+back of ONE
# l2l2 group share ONE file; separate invocations get separate files — no append
# soup across the 3x cert). REQUIRED kinds: deploy deposit ger_inject claim
# cert_settlement rollup_register exit_root.
EVIDENCE_DIR="${EVIDENCE_DIR:-$PROJECT_DIR/.l2l2-evidence}"
: "${EVIDENCE_RUN_TS:=$(date +%s)}"
EVIDENCE_FILE="${EVIDENCE_FILE:-$EVIDENCE_DIR/run-${EVIDENCE_RUN_TS}.ndjson}"
EVIDENCE_REQUIRED_KINDS=(deploy deposit ger_inject claim cert_settlement rollup_register exit_root)

evidence_init() {
    mkdir -p "$EVIDENCE_DIR"
    : >>"$EVIDENCE_FILE"
    log "evidence NDJSON -> $EVIDENCE_FILE"
}

# evidence_record <step> <direction> <chain> <kind> <tx_hash> <block> <contract> <status> [extra]
evidence_record() {
    mkdir -p "$EVIDENCE_DIR"
    EV_STEP="$1" EV_DIR="$2" EV_CHAIN="$3" EV_KIND="$4" EV_TX="$5" EV_BLOCK="$6" \
    EV_CONTRACT="$7" EV_STATUS="$8" EV_EXTRA="${9:-}" \
    python3 - >>"$EVIDENCE_FILE" <<'PY'
import json, os, time
m = [("step","EV_STEP"),("direction","EV_DIR"),("chain","EV_CHAIN"),("kind","EV_KIND"),
     ("tx_hash","EV_TX"),("block","EV_BLOCK"),("contract","EV_CONTRACT"),
     ("status","EV_STATUS"),("extra","EV_EXTRA")]
rec = {"ts": int(time.time())}
for k, e in m:
    rec[k] = os.environ.get(e, "")
print(json.dumps(rec, separators=(",", ":")))
PY
    log "  evidence[$4/$2/$3] tx=${5:--} block=${6:--} status=${8:--}${9:+ ($9)}"
}

# evidence_tx <step> <direction> <chain> <kind> <rpc> <tx_hash> <contract> [extra]
# — fetch the receipt from <rpc> (block + status) so the recorded tx is
# on-chain-verified, then record it.
evidence_tx() {
    local step="$1" direction="$2" chain="$3" kind="$4" rpc="$5" tx="$6" contract="$7" extra="${8:-}"
    local block="" status="norcpt" st
    if [[ -n "$tx" && "$tx" != "0x" ]]; then
        # `timeout` is REQUIRED, not just `|| true`: `cast receipt` WAITS (polls)
        # for the tx to be mined and hangs INDEFINITELY when the tx is not on this
        # rpc's chain (e.g. an aggoracle inject-GER tx id that isn't an L2B tx). A
        # bare `|| true` never fires because the call never returns. Bound it so
        # best-effort evidence never wedges the test; a miss just records
        # "unverified". `|| true` still guards the pipe under `set -e -o pipefail`.
        block=$(timeout 15 cast receipt "$tx" blockNumber --rpc-url "$rpc" 2>/dev/null | awk '{print $1}' || true)
        st=$(timeout 15 cast receipt "$tx" status --rpc-url "$rpc" 2>/dev/null | awk '{print $1}' || true)
        case "$st" in
            1|0x1|true) status="success" ;;
            "")         status="unverified" ;;
            *)          status="failed(${st})" ;;
        esac
    fi
    evidence_record "$step" "$direction" "$chain" "$kind" "$tx" "$block" "$contract" "$status" "$extra"
}

# evidence_rollup_register <step> — retro-locate the CreateNewAggchain(rollupID=2)
# tx on L1 and record it (verified via receipt; must hit the RollupManager).
evidence_rollup_register() {
    local step="$1" rid_topic tx
    rid_topic="0x$(printf '%064x' "$L2B_NETWORK_ID")"
    tx=$( ( set +o pipefail; cast rpc --raw eth_getLogs \
        "[{\"fromBlock\":\"0x0\",\"toBlock\":\"latest\",\"address\":\"$ROLLUP_MANAGER\",\"topics\":[\"$CREATE_AGGCHAIN_TOPIC\",\"$rid_topic\"]}]" \
        --rpc-url "$L1_RPC" 2>/dev/null \
        | python3 -c "import json,sys; l=json.load(sys.stdin); print(l[-1]['transactionHash'] if l else '')" 2>/dev/null ) || true )
    if [[ -z "$tx" ]]; then
        warn "evidence: no CreateNewAggchain(rollupID=$L2B_NETWORK_ID) event on L1 — rollup_register NOT recorded"
        return 1
    fi
    evidence_tx "$step" both L1 rollup_register "$L1_RPC" "$tx" "$ROLLUP_MANAGER" \
        "event=CreateNewAggchain rollupID=$L2B_NETWORK_ID chainID=31338"
}

# evidence_settlement <step> <direction> <container> <since> <network_label> —
# grep the newest SettlementTxnHash from an aggsender's logs, cast-receipt it on
# L1 and assert it hit the RollupManager; record kind=cert_settlement.
evidence_settlement() {
    local step="$1" direction="$2" container="$3" since="$4" netlabel="$5" tx to hits=no
    tx=$( ( set +o pipefail; docker logs --since "$since" "$container" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' \
        | grep -oE 'SettlementTxnHash: 0x[0-9a-fA-F]{64}' | tail -1 | awk '{print $2}' ) || true )
    if [[ -z "$tx" ]]; then
        warn "evidence: no SettlementTxnHash in $container logs since $since (network=$netlabel)"
        return 1
    fi
    # `to` is not a valid `cast receipt` field selector (unlike status/blockNumber)
    # — parse it out of the full receipt table.
    to=$(cast receipt "$tx" --rpc-url "$L1_RPC" 2>/dev/null | awk '$1=="to"{print $2}' || true)
    [[ "$(echo "$to" | tr 'A-F' 'a-f')" == "$(echo "$ROLLUP_MANAGER" | tr 'A-F' 'a-f')" ]] && hits=yes
    evidence_tx "$step" "$direction" L1 cert_settlement "$L1_RPC" "$tx" "$ROLLUP_MANAGER" \
        "network=$netlabel hitsRollupManager=$hits"
    [[ "$hits" == yes ]] || warn "evidence: settlement tx $tx 'to'=$to is NOT the RollupManager"
    return 0
}

# evidence_exit_root <step> <direction> <phase> — snapshot rollup #2's
# lastLocalExitRoot AND the L1 GER's lastRollupExitRoot at a point in the flow.
evidence_exit_root() {
    local step="$1" direction="$2" phase="$3" ller l1rer bn
    ller=$( ( set +o pipefail; cast call "$ROLLUP_MANAGER" \
        'rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint64,uint64,uint8)' \
        "$L2B_NETWORK_ID" --rpc-url "$L1_RPC" 2>/dev/null | sed -n '5p' | awk '{print $1}' ) || true )
    l1rer=$(cast call "$GER_L1" 'lastRollupExitRoot()(bytes32)' --rpc-url "$L1_RPC" 2>/dev/null || true)
    bn=$(cast block-number --rpc-url "$L1_RPC" 2>/dev/null || true)
    evidence_record "$step" "$direction" L1 exit_root "" "$bn" "$ROLLUP_MANAGER" "$phase" \
        "phase=$phase rollup2LastLocalExitRoot=${ller:-?} l1LastRollupExitRoot=${l1rer:-?}"
}

# evidence_summary [required-kind...] — print a per-kind count and FAIL if any
# required kind is missing (≥1 each). Leaves the NDJSON file for inspection.
evidence_summary() {
    local required=("$@")
    [[ ${#required[@]} -gt 0 ]] || required=("${EVIDENCE_REQUIRED_KINDS[@]}")
    echo ""
    log "======================================================================"
    log "  EVIDENCE SUMMARY — $EVIDENCE_FILE"
    log "======================================================================"
    [[ -s "$EVIDENCE_FILE" ]] || fail "evidence file empty/missing: $EVIDENCE_FILE"
    if python3 - "$EVIDENCE_FILE" "${required[@]}" <<'PY'
import json, sys
path, required = sys.argv[1], sys.argv[2:]
# A required token is either "kind" (any direction) or "direction:kind" (exact).
# An entry only COUNTS toward a requirement if it is not a failed/receipt-less
# record; a cert_settlement additionally must have actually hit the RollupManager.
# This makes the summary fail closed and DIRECTIONAL: forward evidence can no
# longer satisfy a required back-direction (ger_inject / cert_settlement / claim).
BAD_PREFIX = ("failed", "norcpt")
counts, by_kind, by_pair, total, bad = {}, {}, {}, 0, []
with open(path) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        r = json.loads(line); total += 1
        k = r.get("kind", "?"); d = r.get("direction", "?")
        st = (r.get("status") or "").lower(); ex = (r.get("extra") or "").lower()
        counts[k] = counts.get(k, 0) + 1
        if any(st.startswith(p) for p in BAD_PREFIX):
            bad.append(f"{d}:{k}[status={st}]"); continue
        if k == "cert_settlement" and "hitsrollupmanager=yes" not in ex:
            bad.append(f"{d}:{k}[not on RollupManager]"); continue
        by_kind[k] = by_kind.get(k, 0) + 1
        by_pair[(d, k)] = by_pair.get((d, k), 0) + 1
for k in sorted(counts):
    print(f"  {k:16s} {counts[k]}")
print(f"  {'TOTAL':16s} {total}")
if bad:
    print("  REJECTED (failed / no-receipt / settlement-not-on-RollupManager): " + "; ".join(bad[:8]))
missing = []
for tok in required:
    if ":" in tok:
        d, k = tok.split(":", 1)
        if by_pair.get((d, k), 0) < 1:
            missing.append(tok)
    elif by_kind.get(tok, 0) < 1:
        missing.append(tok)
if missing:
    print("  MISSING REQUIRED: " + ", ".join(missing))
    sys.exit(3)
print("  ALL REQUIRED PRESENT (good status): " + ", ".join(required))
PY
    then
        pass "evidence complete — every required kind present; audit trail at $EVIDENCE_FILE"
    else
        fail "EVIDENCE INCOMPLETE — a required tx kind was never recorded (see above); NDJSON left at $EVIDENCE_FILE"
    fi
}
