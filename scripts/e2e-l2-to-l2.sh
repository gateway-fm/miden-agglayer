#!/usr/bin/env bash
# L2->L2 e2e (Miden <-> "L2B" second EVM rollup) — task #25, absorbs task #15.
#
# Topology: base stack (make e2e-up) + docker-compose.l2l2.yml overlay:
#   L2B = plain anvil chain (chain-id 31338, :9545) registered as agglayer
#   rollup #2 (networkID=2) via the real kurtosis sovereign flow — the
#   agglayer-contracts image's 4_createRollup.ts on L1 plus the generated
#   sovereign genesis (REAL AgglayerBridgeL2 + AgglayerGERL2) injected on L2B
#   (scripts/setup-l2b.sh) — and its own aggkit (aggoracle+aggsender).
#
# Legs:
#   0   bring up L2B services + register rollup #2 (idempotent)
#   1   deploy ERC-20 "OPT0" on L2B (origin_network = 2, NOT L1)
#   2   forward bridge L2B -> Miden: bridgeAsset(destNet=1) -> aggsender-l2b
#       cert -> agglayer settle -> L1 GER -> Miden aggoracle   [PROVEN LIVE]
#   2b  claim on Miden: bridge-service sync + ClaimTxManager auto-claim ->
#       proxy provisions a foreign-origin faucet keyed (OPT0, net 2) (#108),
#       mints wrapped balance, emits ClaimEvent. Assert via RPC + PG.
#   3   faucet isolation (#15): deploy the SAME 20-byte token address on L1
#       AND L2B (fresh key, nonce 0 on both chains -> identical CREATE addr),
#       bridge both in, assert TWO DISTINCT faucets keyed (addr, net 0) vs
#       (addr, net 2), and that (OPT0, net 0) resolves to NO faucet.
#   4   back-bridge Miden -> L2B: bridge-out the wrapped OPT0 with
#       --dest-network 2, cert settle, claim on L2B (bridge-service
#       ClaimTxManager autoclaim, manual claimAsset fallback), assert the
#       net-zero round trip (L2B holder restored, Miden wrapped back to base).
#   5   completeness: 0 proxy store-locks, synthetic tip advancing, optional
#       exact-block event verification (verify-event-completeness.sh).
#
# Assertion policy: state over logs — /metrics, PG (proxy store on :5434) and
# RPC are authoritative; docker-log greps are used only where the sibling
# suites already proved them stable ('submitted claim note txn' etc.).
# Fail-closed: every curl is -sf, every psql failure surfaces via pgq.
#
# Usage: base stack up (make e2e-up), then ./scripts/e2e-l2-to-l2.sh
#        (or ./scripts/e2e-test.sh l2-to-l2)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
REPO="$PROJECT_DIR"

source "$FIXTURES_DIR/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"          # Miden proxy
L2B_RPC="${L2B_RPC:-http://localhost:9545}"        # anvil-l2b
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"

# Compose derives the project name from the CHECKOUT DIRECTORY (main repo ->
# "miden-agglayer", the l2l2 worktree -> "l2l2"), so never assume — detect it
# from the live proxy container, falling back to the main-repo default.
_DETECTED_PROJECT=$(docker ps --format '{{.Names}}' 2>/dev/null | grep -E -- '-miden-agglayer-1$' | head -1 | sed 's/-miden-agglayer-1$//')
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-${_DETECTED_PROJECT:-miden-agglayer}}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
AGGKIT_L2B_CONTAINER="${AGGKIT_L2B_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-l2b-1}"

# L2B contract topology (see setup-l2b.sh; addresses are snapshot-deterministic)
BRIDGE=0xC8cbEBf950B9Df44d987c8619f092beA980fF038      # AgglayerBridge(L2) proxy on BOTH L1 and L2B
GER_L1=0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674      # L1 global exit root (AgglayerGER)
L2B_GER=0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA     # real AgglayerGERL2 proxy on L2B
ROLLUP_MANAGER=0x6c6c009cC348976dB4A908c92B24433d4F6edA43
L2B_NETWORK_ID=2
MIDEN_NETWORK_ID=1

# TEST-ONLY keys (kurtosis-cdk standard; see fixtures/agglayer-config.toml warning)
ADMIN=0xE34aaF64b29273B7D567FCFc40544c014EEe9970
ADMIN_KEY=0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625

# Decimals: OPT0/COL are 18-decimal ERC-20s; Miden wraps at 8 -> scale 10^10.
WEI_PER_MIDEN_UNIT=10000000000
FWD_AMOUNT_WEI=1000000000000000        # 0.001 OPT0 forward L2B -> Miden
FWD_MIDEN_UNITS=$((FWD_AMOUNT_WEI / WEI_PER_MIDEN_UNIT))   # 100000
COL_L1_WEI=1000000000000000            # COL bridged from L1 (net 0)
COL_L1_UNITS=$((COL_L1_WEI / WEI_PER_MIDEN_UNIT))
COL_L2B_WEI=2000000000000000           # COL bridged from L2B (net 2) — distinct amount
COL_L2B_UNITS=$((COL_L2B_WEI / WEI_PER_MIDEN_UNIT))
TOKEN_SUPPLY=1000000000000000000000000 # 1M tokens @ 18 decimals

BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    # Subshell with pipefail off — see e2e-dynamic-erc20.sh::wait_for for the
    # SIGPIPE rationale (grep -q closing docker-logs pipes early).
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)
# stderr dropped: locale-warning noise corrupts captures (see sibling scripts);
# a FAILED psql still propagates its non-zero exit to the caller.
pgq() { "${PSQL[@]}" -c "$1" 2>/dev/null; }

l2_tip() {
    curl -sf -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'
}

# find_deposit <dest_addr> <source_network_id> <orig_addr_lower> — prints the
# newest matching deposit JSON ("" when absent). limit=100 dodges the 25-row
# default pagination (which silently hides older deposits).
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

# claim_event_rows <global_index_decimal> — count of ClaimEvent synthetic_logs
# rows whose data word 0 (globalIndex) matches. PG errors propagate (pgq).
claim_event_rows() {
    local gi_hex
    gi_hex=$(python3 -c "print(format(int('$1'),'064x'))")
    pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${gi_hex}%';"
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast    >/dev/null || fail "cast (foundry) not found"
command -v forge   >/dev/null || fail "forge (foundry) not found"
command -v psql    >/dev/null || fail "psql not found (apt-get install postgresql-client)"
command -v curl    >/dev/null || fail "curl not found"
command -v python3 >/dev/null || fail "python3 not found"
command -v docker  >/dev/null || fail "docker not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable at $L1_RPC"
: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY — run scripts/ensure-e2e-secrets.sh}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"

log "======================================================================"
log "  L2->L2 E2E (Miden <-> L2B rollup #2) — forward, isolation, round trip"
log "======================================================================"

# ── Leg 0: bring up the L2B-extended stack + register rollup #2 ──────────────
# (assumes the base stack is ALREADY up healthy via `make e2e-up`; this adds
#  the L2B services on top and runs the one-time L1/L2B setup — all idempotent)
step "Leg 0: L2B services + rollup #2 registration"
"$SCRIPT_DIR/gen-l2b-configs.sh"
docker compose -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
  --env-file "$REPO/fixtures/.env" up -d anvil-l2b aggkit-l2b agglayer bridge-service
wait_for "anvil-l2b reachable at $L2B_RPC" \
    "cast chain-id --rpc-url '$L2B_RPC' >/dev/null 2>&1" 60 2
L2B_RPC="$L2B_RPC" "$SCRIPT_DIR/setup-l2b.sh"
# ClaimTxManager sponsors claimAsset on EVERY configured L2 — including L2B.
# Its keystore account exists only on L1/Miden fixtures; fund it on L2B or the
# leg-4 autoclaim silently starves for gas.
: "${SPONSOR_PRIVATE_KEY:?fixtures/.env must define SPONSOR_PRIVATE_KEY — run scripts/ensure-sponsor-key.sh}"
SPONSOR_ADDR=$(cast wallet address --private-key "$SPONSOR_PRIVATE_KEY")
cast rpc anvil_setBalance "$SPONSOR_ADDR" 0x21e19e0c9bab2400000 --rpc-url "$L2B_RPC" >/dev/null
log "  claim sponsor $SPONSOR_ADDR funded on L2B"
# bridge-service validates contract code at startup and exits if the L2B bridge
# doesn't exist yet — (re)start it AFTER setup-l2b so both networks index.
docker compose -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
  --env-file "$REPO/fixtures/.env" up -d --force-recreate bridge-service
wait_for "bridge-service HTTP API up (post-recreate)" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000' >/dev/null" 120 3
pass "Leg 0 done: rollup #2 registered, L2B bridge/GER live, bridge-service indexing both networks"

# ── Miden-side identities: infra accounts + isolated destination wallet ──────
# The proxy writes bridge_accounts.toml ~45s AFTER reporting healthy (account
# deploy + registry init) — wait for it instead of failing on a fresh stack.
ACCOUNTS=""
for _ in $(seq 1 30); do
    ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
        cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) && break
    sleep 5
done
[[ -n "$ACCOUNTS" ]] || fail "miden-agglayer not initialized within 150s (bridge_accounts.toml absent)"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')
[[ -n "$BRIDGE_ID" && -n "$FAUCET_ETH" ]] || fail "could not read bridge account ids"

# Isolated bridge wallet (single-owner store policy): the claim destination and
# the leg-4 bridge-out sender. Self-funding — own store subdir.
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-l2-to-l2}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH" \
    || fail "could not provision isolated bridge-out wallet"
log "Wallet: $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
log "Dest:   $DEST_ADDR (zero-padded, network $MIDEN_NETWORK_ID)"

# ── Leg 1: deploy OPT0 on L2B (origin_network = 2, not L1) ───────────────────
# Deploy + forward-bridge PROVEN LIVE 2026-07-09 (docs/l2-to-l2-notes.md UPD 3).
step "Leg 1: deploying OPT0 on L2B"
OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
    --private-key $ADMIN_KEY --broadcast \
    --constructor-args "L2BToken" "OPT0" 18 "$TOKEN_SUPPLY" 2>&1)
OPT0=$(echo "$OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -n "$OPT0" ]] || fail "OPT0 deploy failed: $(echo "$OUT" | tail -2)"
OPT0_LOWER=$(echo "$OPT0" | tr 'A-F' 'a-f')
OPT0_HEX="${OPT0_LOWER#0x}"
pass "OPT0 deployed on L2B: $OPT0 (origin network $L2B_NETWORK_ID)"

# NDG — dedicated nudge token (see nudge_cert below). Distinct from OPT0 so
# nudges never disturb the leg-4 net-zero balance assert.
OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
    --private-key $ADMIN_KEY --broadcast \
    --constructor-args "NudgeToken" "NDG" 18 1000000000000000000 2>&1)
NDG=$(echo "$OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -n "$NDG" ]] || fail "NDG deploy failed: $(echo "$OUT" | tail -2)"

# The upstream ClaimTxManager is EVENT-driven: it scans for L2->L2 claims only
# when a NEW rollup exit root lands on L1 (claimtxman.go:190,
# GetDepositsFromOtherL2ToClaim), and a deposit only turns ready_for_claim
# AFTER its own settle cycle round-trips through the destination's trusted GER
# (claimtxman.go:418). A single L2->L2 transfer therefore sits ready but
# unscanned until the NEXT certificate settles. nudge_cert forces that next
# cycle: bridge 1 wei of NDG L2B->L1 — destination L1 means no claimtxman /
# Miden-proxy side effects (L1 claims are manual and we never claim it), but
# the resulting cert advances the L1 rollups exit root and wakes the scan.
# Call it only AFTER observing ready_for_claim, or the nudge folds into the
# same cert and triggers nothing.
nudge_cert() {
    cast send "$NDG" "approve(address,uint256)" $BRIDGE 1 \
        --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null || fail "NDG approve (nudge)"
    cast send $BRIDGE "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        0 "$ADMIN" 1 "$NDG" true 0x \
        --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null || fail "NDG bridgeAsset (nudge)"
    log "  nudge cert sent (1 wei NDG L2B->L1) — wakes the L2->L2 claim scan"
}

# ── Leg 2 (forward): bridgeAsset L2B -> Miden + GER propagation ──────────────
step "Leg 2: bridgeAsset(destNet=$MIDEN_NETWORK_ID, $FWD_AMOUNT_WEI OPT0 wei) on L2B"
# Snapshot the L2B holder BEFORE the forward bridge — leg 4 asserts the
# round trip restores exactly this balance.
L2B_BAL_BEFORE_FORWARD=$(cast call "$OPT0" 'balanceOf(address)(uint256)' $ADMIN --rpc-url "$L2B_RPC" | awk '{print $1}')
L1GER_PRE=$(cast call $GER_L1 'getLastGlobalExitRoot()(bytes32)' --rpc-url "$L1_RPC")
# L1-traceability baselines: rollup #2's last local exit root recorded on the
# RollupManager, the L1 GER's rollup exit root, and the L1 block height — the
# cert settlement MUST move all of them (asserted after propagation).
rollup2_ler() {
    cast call $ROLLUP_MANAGER \
        "rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint64,uint64,uint8)" \
        "$L2B_NETWORK_ID" --rpc-url "$L1_RPC" | sed -n 5p
}
LER2_PRE=$(rollup2_ler)
L1_RER_PRE=$(cast call $GER_L1 'lastRollupExitRoot()(bytes32)' --rpc-url "$L1_RPC")
LEG2_L1_BLOCK=$(cast block-number --rpc-url "$L1_RPC")
log "  L2B OPT0 holder balance before forward: $L2B_BAL_BEFORE_FORWARD"
log "  rollup#2 lastLocalExitRoot pre: $LER2_PRE (L1 block $LEG2_L1_BLOCK)"

cast send "$OPT0" "approve(address,uint256)" $BRIDGE "$FWD_AMOUNT_WEI" \
    --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null || fail "OPT0 approve on L2B"
TX=$(cast send $BRIDGE "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$FWD_AMOUNT_WEI" "$OPT0" true 0x \
    --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "bridgeAsset on L2B failed (status=$STATUS): $TX"
DC=$(cast call $BRIDGE 'depositCount()(uint256)' --rpc-url "$L2B_RPC")
log "  L2B depositCount: $DC"

# aggsender-l2b cert -> agglayer settle -> L1 GER update -> Miden aggoracle.
# Two-part criterion: the L1 GER must MOVE off its pre-bridge value (our cert
# settled) AND Miden must report that same GER (aggoracle injected it).
GER_TIMEOUT="${GER_TIMEOUT:-600}"
log "  waiting for GER propagation L2B -> L1 -> Miden (cert settle, <=${GER_TIMEOUT}s)..."
DEADLINE=$(( $(date +%s) + GER_TIMEOUT ))
MIDENGER=""; L1GER="$L1GER_PRE"
while [[ "$(date +%s)" -lt "$DEADLINE" ]]; do
    L1GER=$(cast call $GER_L1 'getLastGlobalExitRoot()(bytes32)' --rpc-url "$L1_RPC")
    MIDENGER=$(curl -sf "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"zkevm_getLatestGlobalExitRoot","params":[]}' \
        | python3 -c "import json,sys;print(json.load(sys.stdin).get('result',''))" 2>/dev/null || true)
    if [[ -n "$MIDENGER" && "$MIDENGER" == "$L1GER" && "$L1GER" != "$L1GER_PRE" ]]; then
        break
    fi
    sleep 5; echo -n "."
done
echo ""
[[ -n "$MIDENGER" && "$MIDENGER" == "$L1GER" && "$L1GER" != "$L1GER_PRE" ]] \
    || fail "GER did not propagate to Miden within ${GER_TIMEOUT}s (pre=$L1GER_PRE L1=$L1GER miden=${MIDENGER:-<none>})"

# ── Leg 2 L1-traceability: the settlement must be evidenced ON the L1 chain ──
# (1) rollup #2's lastLocalExitRoot on the RollupManager moved off its baseline.
LER2_POST=$(rollup2_ler)
[[ "$LER2_POST" != "$LER2_PRE" ]] \
    || fail "rollupIDToRollupData($L2B_NETWORK_ID).lastLocalExitRoot did not move ($LER2_PRE) — cert not settled on L1"
pass "L1 RollupManager: rollup#2 lastLocalExitRoot $LER2_PRE -> $LER2_POST"
# (2) the L1 settlement tx exists: a RollupManager event since leg-2 start
# carries rollupID 2 as an indexed topic; its receipt must be status 1.
SETTLE_LOGS=$(cast rpc --raw eth_getLogs "[{\"fromBlock\":\"$(printf 0x%x "$LEG2_L1_BLOCK")\",\"toBlock\":\"latest\",\"address\":\"$ROLLUP_MANAGER\"}]" --rpc-url "$L1_RPC")
SETTLE_TX=$(echo "$SETTLE_LOGS" | python3 -c "
import json, sys
rid = format($L2B_NETWORK_ID, 'x').rjust(64, '0')
txs = [lg['transactionHash'] for lg in json.load(sys.stdin) if any(t[2:] == rid for t in lg['topics'][1:])]
print(txs[-1] if txs else '')
")
[[ -n "$SETTLE_TX" ]] || fail "no RollupManager event with rollupID $L2B_NETWORK_ID since L1 block $LEG2_L1_BLOCK — settlement tx not found on L1"
SETTLE_STATUS=$(cast receipt "$SETTLE_TX" status --rpc-url "$L1_RPC")
[[ "$SETTLE_STATUS" == *1* ]] || fail "L1 settlement tx $SETTLE_TX receipt status: $SETTLE_STATUS"
SETTLE_TO=$(cast receipt "$SETTLE_TX" --json --rpc-url "$L1_RPC" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("to"))')
pass "L1 settlement tx: $SETTLE_TX (status 1, to=$SETTLE_TO)"
# (3) the L1 GER contract absorbed the new rollup exit root.
L1_RER_POST=$(cast call $GER_L1 'lastRollupExitRoot()(bytes32)' --rpc-url "$L1_RPC")
[[ "$L1_RER_POST" != "$L1_RER_PRE" ]] \
    || fail "L1 GER lastRollupExitRoot did not move ($L1_RER_PRE) — exit-root propagation broken"
pass "L1 GER ($GER_L1): lastRollupExitRoot $L1_RER_PRE -> $L1_RER_POST"
pass "Leg 2 done: cross-L2 GER on Miden: $MIDENGER"

# ── Leg 2b: claim on Miden — foreign-origin faucet + wrapped balance ─────────
step "Leg 2b: claim on Miden (auto-claim) + (OPT0, net $L2B_NETWORK_ID) faucet asserts"

# Baselines BEFORE the claim lands (wallet may be reused across runs).
FAUCETS_BEFORE=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | python3 -c "import json,sys; r=json.load(sys.stdin); print(len(r.get('result',[])))") \
    || fail "admin_listFaucets unreachable"
log "  faucets registered before claim: $FAUCETS_BEFORE"

wait_for "L2B->Miden deposit ready_for_claim in bridge-service" \
    "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$OPT0_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') and d.get('dest_net')==$MIDEN_NETWORK_ID else 1)\"" \
    600 5
FWD_DEPOSIT=$(find_deposit "$DEST_ADDR" $L2B_NETWORK_ID "$OPT0_LOWER")
[[ -n "$FWD_DEPOSIT" ]] || fail "forward deposit vanished from bridge-service"
FWD_GI=$(dep_field "$FWD_DEPOSIT" global_index)
log "  forward deposit: cnt=$(dep_field "$FWD_DEPOSIT" deposit_cnt) globalIndex=$FWD_GI"
nudge_cert   # deposit is ready — force the next settle so the claim scan runs

# ClaimTxManager submits claimAsset to the proxy; proxy auto-creates the
# faucet and mints. Proven log lines from the l1-to-l2/dynamic-erc20 suites.
wait_for "faucet auto-creation for OPT0" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    300 5
wait_for "claim tx submitted on Miden" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'submitted claim note txn'" \
    180 5
wait_for "claim tx committed on Miden" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    120 3

# (a) Faucet keyed (OPT0, net 2) — RPC view + PG truth must agree.
FAUCETS_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}') \
    || fail "admin_listFaucets unreachable after claim"
OPT0_FAUCET_ID=$(echo "$FAUCETS_JSON" | python3 -c "
import json, sys
for f in json.load(sys.stdin).get('result', []):
    if f.get('origin_address','').lower() == '$OPT0_LOWER' and f.get('origin_network') == $L2B_NETWORK_ID:
        print(f['faucet_id']); break
")
[[ -n "$OPT0_FAUCET_ID" ]] || fail "no faucet for (OPT0, net $L2B_NETWORK_ID) in admin_listFaucets"
PG_OPT0_FID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}' AND origin_network = ${L2B_NETWORK_ID};")
[[ -n "$PG_OPT0_FID" ]] || fail "no faucet_registry row for (OPT0, net $L2B_NETWORK_ID) in PG"
[[ "$(echo "$OPT0_FAUCET_ID" | tr 'A-F' 'a-f')" == "$PG_OPT0_FID" ]] \
    || fail "faucet id mismatch RPC=$OPT0_FAUCET_ID vs PG=$PG_OPT0_FID"
pass "foreign-origin faucet auto-created + keyed (OPT0, net $L2B_NETWORK_ID): $OPT0_FAUCET_ID"

# (b) Wrapped balance credited to the destination wallet (isolated dry-probe).
# Fresh faucet this run -> the wallet's balance for it starts at 0.
WRAPPED_BASELINE=0
BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$OPT0_FAUCET_ID")
    BALANCE="${BALANCE:-0}"
    log "  attempt $attempt/15: wrapped OPT0 balance = $BALANCE"
    [[ "$BALANCE" -gt "$WRAPPED_BASELINE" ]] && break
done
[[ "$BALANCE" -eq "$FWD_MIDEN_UNITS" ]] \
    || fail "wrapped balance mismatch: got $BALANCE, expected $FWD_MIDEN_UNITS ($FWD_AMOUNT_WEI wei / 10^10)"
pass "wrapped OPT0 credited: $BALANCE Miden units"

# (c) ClaimEvent row exists for this deposit's global index.
CLAIM_ROWS=$(claim_event_rows "$FWD_GI")
[[ "${CLAIM_ROWS:-0}" -ge 1 ]] || fail "no ClaimEvent synthetic_logs row for globalIndex $FWD_GI"
FWD_CLAIM_BLOCK=$(pgq "SELECT block_number FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x$(python3 -c "print(format(int('$FWD_GI'),'064x'))")%' ORDER BY block_number LIMIT 1;")
pass "Leg 2b done: ClaimEvent at synthetic block ${FWD_CLAIM_BLOCK:-?} (rows=$CLAIM_ROWS)"

# ── Leg 3: faucet isolation (#15) — same address, different origin network ───
step "Leg 3: same-address/different-origin faucet isolation"
# CREATE addresses derive from (sender, nonce): a FRESH key deploying at nonce
# 0 on both chains yields the SAME 20-byte token address on L1 and L2B — the
# exact collision #108's (addr, origin_network) keying must disambiguate.
KEY_OUT=$(cast wallet new)
COL_DEPLOYER=$(echo "$KEY_OUT" | awk '/Address:/{print $2}')
COL_KEY=$(echo "$KEY_OUT" | awk '/Private key:/{print $3}')
[[ -n "$COL_DEPLOYER" && -n "$COL_KEY" ]] || fail "could not parse cast wallet new output"
cast send --rpc-url "$L1_RPC" --private-key "$ADMIN_KEY" --value 1ether "$COL_DEPLOYER" >/dev/null \
    || fail "funding COL deployer on L1"
cast rpc anvil_setBalance "$COL_DEPLOYER" 0xde0b6b3a7640000 --rpc-url "$L2B_RPC" >/dev/null \
    || fail "funding COL deployer on L2B"
[[ "$(cast nonce "$COL_DEPLOYER" --rpc-url "$L1_RPC")" == "0" ]] || fail "COL deployer nonce non-zero on L1"
[[ "$(cast nonce "$COL_DEPLOYER" --rpc-url "$L2B_RPC")" == "0" ]] || fail "COL deployer nonce non-zero on L2B"

deploy_col() { # $1 = rpc url
    local out
    out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$1" \
        --private-key "$COL_KEY" --broadcast \
        --constructor-args "CollideToken" "COL" 18 "$TOKEN_SUPPLY" 2>&1) || { echo ""; return; }
    echo "$out" | grep "Deployed to:" | awk '{print $NF}'
}
COL_L1=$(deploy_col "$L1_RPC");  [[ -n "$COL_L1" ]]  || fail "COL deploy on L1 failed"
COL_L2B=$(deploy_col "$L2B_RPC"); [[ -n "$COL_L2B" ]] || fail "COL deploy on L2B failed"
[[ "$(echo "$COL_L1" | tr 'A-F' 'a-f')" == "$(echo "$COL_L2B" | tr 'A-F' 'a-f')" ]] \
    || fail "CREATE address mismatch: L1=$COL_L1 L2B=$COL_L2B (nonce drift?)"
COL="$COL_L1"
COL_LOWER=$(echo "$COL" | tr 'A-F' 'a-f')
COL_HEX="${COL_LOWER#0x}"
pass "COL deployed at the SAME address on both chains: $COL"

# Bridge the L2B-origin COL first (needs a cert-settle round; slowest leg),
# then the L1-origin COL (ready within seconds of GER injection).
cast send "$COL" "approve(address,uint256)" $BRIDGE "$COL_L2B_WEI" \
    --private-key "$COL_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "COL approve on L2B"
TX=$(cast send $BRIDGE "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L2B_WEI" "$COL" true 0x \
    --private-key "$COL_KEY" --rpc-url "$L2B_RPC" 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "COL bridgeAsset on L2B failed (status=$STATUS): $TX"

cast send --rpc-url "$L1_RPC" --private-key "$COL_KEY" \
    "$COL" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$COL_L1_WEI" >/dev/null \
    || fail "COL approve on L1"
TX=$(cast send --rpc-url "$L1_RPC" --private-key "$COL_KEY" \
    "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L1_WEI" "$COL" true 0x 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "COL bridgeAsset on L1 failed (status=$STATUS): $TX"
log "  COL bridged from BOTH origins (net 0: $COL_L1_WEI wei, net 2: $COL_L2B_WEI wei)"

# The (COL, net 2) claim needs the event-driven scan (see nudge_cert): wait
# for its readiness, then force the next settle cycle. The (COL, net 0) claim
# rides the mainnet-exit-root path and needs no nudge.
wait_for "COL net-2 deposit ready_for_claim" \
    "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$COL_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') else 1)\"" \
    600 5
nudge_cert

# Both claims auto-create faucets; wait on the PG registry (state, not logs).
wait_for "TWO faucet_registry rows for COL (net 0 + net 2)" \
    "[ \"\$(pgq \"SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}';\")\" = \"2\" ]" \
    900 10

COL_FID_NET0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = 0;")
COL_FID_NET2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = ${L2B_NETWORK_ID};")
[[ -n "$COL_FID_NET0" && -n "$COL_FID_NET2" ]] \
    || fail "COL faucet rows incomplete: net0='$COL_FID_NET0' net2='$COL_FID_NET2'"
[[ "$COL_FID_NET0" != "$COL_FID_NET2" ]] \
    || fail "FAUCET COLLISION: (COL, net 0) and (COL, net 2) share faucet $COL_FID_NET0"
pass "distinct faucets for one address: net0=$COL_FID_NET0 net2=$COL_FID_NET2"

# Negative control: OPT0 exists ONLY as an origin-network-2 asset — a lookup
# under origin_network=0 must yield NOTHING (proves the key includes network).
OPT0_NET0_ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}' AND origin_network = 0;")
[[ "$OPT0_NET0_ROWS" == "0" ]] \
    || fail "(OPT0, net 0) unexpectedly resolves to a faucet ($OPT0_NET0_ROWS rows) — keying broken"
OPT0_ALL_ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}';")
[[ "$OPT0_ALL_ROWS" == "1" ]] \
    || fail "expected exactly 1 faucet row for OPT0's address, got $OPT0_ALL_ROWS"
pass "negative control: (OPT0, net 0) -> no faucet; OPT0 address has exactly 1 row (net 2)"

# RPC view agrees with PG (admin_listFaucets carries origin_address+network).
RPC_COL_COUNT=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | python3 -c "
import json, sys
fs = [f for f in json.load(sys.stdin).get('result', [])
      if f.get('origin_address','').lower() == '$COL_LOWER']
nets = sorted(f['origin_network'] for f in fs)
ids = {f['faucet_id'].lower() for f in fs}
print(len(fs) if nets == [0, $L2B_NETWORK_ID] and len(ids) == 2 else -1)
")
[[ "$RPC_COL_COUNT" == "2" ]] || fail "admin_listFaucets does not show 2 distinct COL faucets on nets {0,$L2B_NETWORK_ID}"

# Each faucet minted its OWN amount to the wallet — no cross-contamination.
COL_BAL_NET0=0; COL_BAL_NET2=0
for attempt in $(seq 1 15); do
    sleep 10
    COL_BAL_NET0=$(iso_wallet_balance "$BRIDGE_ID" "$COL_FID_NET0"); COL_BAL_NET0="${COL_BAL_NET0:-0}"
    COL_BAL_NET2=$(iso_wallet_balance "$BRIDGE_ID" "$COL_FID_NET2"); COL_BAL_NET2="${COL_BAL_NET2:-0}"
    log "  attempt $attempt/15: COL wrapped balances net0=$COL_BAL_NET0 net2=$COL_BAL_NET2"
    [[ "$COL_BAL_NET0" -gt 0 && "$COL_BAL_NET2" -gt 0 ]] && break
done
[[ "$COL_BAL_NET0" -eq "$COL_L1_UNITS" ]] \
    || fail "(COL, net 0) wrapped balance: got $COL_BAL_NET0, expected $COL_L1_UNITS"
[[ "$COL_BAL_NET2" -eq "$COL_L2B_UNITS" ]] \
    || fail "(COL, net 2) wrapped balance: got $COL_BAL_NET2, expected $COL_L2B_UNITS"
pass "Leg 3 done: (addr, origin_network) keying proven end-to-end — distinct faucets, distinct balances"

# ── Leg 4: back-bridge Miden -> L2B (burn wrapped, claim original) ───────────
step "Leg 4: bridge-out wrapped OPT0 (destNet=$L2B_NETWORK_ID) + claim on L2B"
LEG4_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BACK_AMOUNT="$FWD_MIDEN_UNITS"   # leg 2b asserted the wallet holds exactly this
ADMIN_LOWER=$(echo "$ADMIN" | tr 'A-F' 'a-f')
# BridgeEvent baseline (PG count) — the bridge-out must add exactly one row.
BE_ROWS_BEFORE=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}';")

iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$OPT0_FAUCET_ID" \
    --amount "$BACK_AMOUNT" --dest-address "$ADMIN" --dest-network "$L2B_NETWORK_ID" 2>&1 \
    || fail "bridge-out-tool failed (destNet=$L2B_NETWORK_ID)"
pass "B2AGG note created for wrapped OPT0 -> L2B"

# BridgeEvent synthesized by the proxy — it must carry origin (OPT0, net 2).
wait_for "synthetic BridgeEvent row (PG count +1)" \
    "[ \"\$(pgq \"SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}';\")\" -gt \"${BE_ROWS_BEFORE:-0}\" ]" \
    300 5
BE_ORIGIN_OK=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}' AND lower(data) LIKE '%${OPT0_HEX}%';")
[[ "${BE_ORIGIN_OK:-0}" -ge 1 ]] \
    || fail "no BridgeEvent row carries the OPT0 origin address — wrapped bridge-out lost its (addr, net) identity"

# Certificate settle (aggsender #1 -> agglayer -> L1). 900s: cold prover.
wait_for "Miden certificate settled on L1" \
    "docker logs --since $LEG4_START $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    900 10
pass "certificate settled"

# bridge-service must sync the exit AND flip ready_for_claim once aggoracle-l2b
# injects the new GER into the real AgglayerGERL2 on L2B.
wait_for "Miden->L2B deposit ready_for_claim" \
    "find_deposit '$ADMIN' $MIDEN_NETWORK_ID '$OPT0_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') and d.get('dest_net')==$L2B_NETWORK_ID else 1)\"" \
    600 5
BACK_DEPOSIT=$(find_deposit "$ADMIN" $MIDEN_NETWORK_ID "$OPT0_LOWER")
[[ -n "$BACK_DEPOSIT" ]] || fail "back deposit vanished from bridge-service"
BACK_CNT=$(dep_field "$BACK_DEPOSIT" deposit_cnt)
BACK_GI=$(dep_field "$BACK_DEPOSIT" global_index)
BACK_AMOUNT_WEI=$(dep_field "$BACK_DEPOSIT" amount)
log "  back deposit: cnt=$BACK_CNT globalIndex=$BACK_GI amount=$BACK_AMOUNT_WEI wei"
EXPECTED_BACK_WEI=$(python3 -c "print($BACK_AMOUNT * $WEI_PER_MIDEN_UNIT)")
[[ "$BACK_AMOUNT_WEI" == "$EXPECTED_BACK_WEI" ]] \
    || fail "back-bridge amount mismatch: exit leaf carries $BACK_AMOUNT_WEI wei, expected $EXPECTED_BACK_WEI"
nudge_cert   # back deposit is ready — wake the rollupID-2 claim scan on L2B

# Claim on L2B: prefer the bridge-service ClaimTxManager autoclaim; fall back
# to a manual claimAsset (proof from /merkle-proof, GER gated on AgglayerGERL2).
CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')")
if [[ -z "$CLAIM_TX_HASH" ]]; then
    log "  waiting up to 180s for ClaimTxManager autoclaim on L2B..."
    for _ in $(seq 1 36); do
        sleep 5
        BACK_DEPOSIT=$(find_deposit "$ADMIN" $MIDEN_NETWORK_ID "$OPT0_LOWER")
        CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')" 2>/dev/null || true)
        [[ -n "$CLAIM_TX_HASH" ]] && break
        echo -n "."
    done
    echo ""
fi

if [[ -n "$CLAIM_TX_HASH" ]]; then
    log "  autoclaimed on L2B (tx $CLAIM_TX_HASH); verifying receipt..."
    RECEIPT_STATUS=$(cast receipt --rpc-url "$L2B_RPC" "$CLAIM_TX_HASH" status 2>/dev/null || echo "")
    [[ "$RECEIPT_STATUS" == *1* || "$RECEIPT_STATUS" == *true* ]] \
        || fail "L2B autoclaim tx $CLAIM_TX_HASH receipt status not success: ${RECEIPT_STATUS:-<none>}"
    pass "claim on L2B via ClaimTxManager autoclaim"
else
    warn "no autoclaim within 180s — claiming manually on L2B"
    PROOF_JSON=""
    for _ in $(seq 1 18); do
        PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$BACK_CNT&net_id=$MIDEN_NETWORK_ID" 2>/dev/null || true)
        [[ -n "$PROOF_JSON" ]] && break
        sleep 5
    done
    [[ -n "$PROOF_JSON" ]] || fail "could not fetch merkle proof for back deposit after 90s"
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
    # claimAsset on L2B verifies the GER exists in the real AgglayerGERL2 —
    # aggoracle-l2b injects it after settlement. Gate on its globalExitRootMap.
    BACK_GER=$(cast keccak "0x${MAINNET_EXIT_ROOT#0x}${ROLLUP_EXIT_ROOT#0x}")
    wait_for "GER $BACK_GER injected into L2B AgglayerGERL2 (aggoracle-l2b)" \
        "[ \"\$(cast call $L2B_GER 'globalExitRootMap(bytes32)(uint256)' $BACK_GER --rpc-url '$L2B_RPC' | awk '{print \$1}')\" != \"0\" ]" \
        300 5
    ORIG_NET=$(dep_field "$BACK_DEPOSIT" orig_net)
    DEST_NET=$(dep_field "$BACK_DEPOSIT" dest_net)
    DEST_ADDR_CLAIM=$(dep_field "$BACK_DEPOSIT" dest_addr)
    METADATA_CLAIM=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
    CLAIM_TX=$(cast send --rpc-url "$L2B_RPC" --private-key "$ADMIN_KEY" \
        "$BRIDGE" \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$SMT_LOCAL" "$SMT_ROLLUP" "$BACK_GI" \
        "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
        "$ORIG_NET" "$OPT0" \
        "$DEST_NET" "$DEST_ADDR_CLAIM" \
        "$BACK_AMOUNT_WEI" "$METADATA_CLAIM" 2>&1) || true
    STATUS=$(printf '%s\n' "$CLAIM_TX" | awk '$1=="status"{print $2; exit}')
    [[ "$STATUS" == "1" ]] || { warn "L2B claim tx output: $CLAIM_TX"; fail "manual claimAsset on L2B failed"; }
    pass "claim on L2B via manual claimAsset"
fi

# Net-zero round trip: original holder restored on L2B...
L2B_BAL_FINAL=$(cast call "$OPT0" 'balanceOf(address)(uint256)' $ADMIN --rpc-url "$L2B_RPC" | awk '{print $1}')
python3 -c "exit(0 if int('$L2B_BAL_FINAL') == int('$L2B_BAL_BEFORE_FORWARD') else 1)" \
    || fail "L2B round trip NOT net-zero: before-forward=$L2B_BAL_BEFORE_FORWARD final=$L2B_BAL_FINAL"
pass "L2B OPT0 holder restored: $L2B_BAL_FINAL (== pre-forward balance)"

# ...and the Miden wrapped balance back to its pre-bridge baseline.
WRAPPED_FINAL="$BACK_AMOUNT"
for attempt in $(seq 1 12); do
    WRAPPED_FINAL=$(iso_wallet_balance "$BRIDGE_ID" "$OPT0_FAUCET_ID")
    WRAPPED_FINAL="${WRAPPED_FINAL:-0}"
    [[ "$WRAPPED_FINAL" -eq "$WRAPPED_BASELINE" ]] && break
    log "  attempt $attempt/12: wrapped OPT0 balance = $WRAPPED_FINAL (want $WRAPPED_BASELINE)"
    sleep 10
done
[[ "$WRAPPED_FINAL" -eq "$WRAPPED_BASELINE" ]] \
    || fail "Miden wrapped OPT0 not fully burned: $WRAPPED_FINAL remains (baseline $WRAPPED_BASELINE)"
pass "Leg 4 done: net-zero round trip L2B -> Miden -> L2B"

# ── Leg 5: completeness + health ─────────────────────────────────────────────
step "Leg 5: exact-block completeness + proxy health"

# (a) 0 store-locks in the proxy for the whole run — the store has a single
# owner (the proxy); any 'database is locked' is an internal regression.
LOCKS=$(docker logs --since "$TEST_START_TIME" "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)
[[ "${LOCKS:-0}" -eq 0 ]] || fail "proxy logged $LOCKS 'database is locked' error(s) during the run"
pass "0 store-locks"

# (b) synthetic tip still advancing after the whole round trip.
TIP_A=$(l2_tip) || fail "eth_blockNumber unreachable after test"
sleep 12
TIP_B=$(l2_tip) || fail "eth_blockNumber unreachable after test"
[[ "$TIP_B" -gt "$TIP_A" ]] || fail "synthetic tip frozen at $TIP_A — proxy unhealthy after cross-L2 traffic"
pass "tip advancing: $TIP_A -> $TIP_B"

# (c) exact-block event completeness (BridgeEvent/ClaimEvent at the note's
# consumption block, no missing/extra) — the independent node-DB cross-check.
# Needs the locally built bridge-out-tool for the canonical script roots.
TOOL_BIN="${TOOL_BIN:-$PROJECT_DIR/target/debug/bridge-out-tool}"
if [[ -x "$TOOL_BIN" ]]; then
    BRIDGE_ID="$BRIDGE_ID" ALLOW_LATE="${ALLOW_LATE:-1}" TOOL_BIN="$TOOL_BIN" \
        "$SCRIPT_DIR/verify-event-completeness.sh" \
        || fail "event-completeness verification failed"
    pass "exact-block event completeness verified"
elif [[ "${STRICT_COMPLETENESS:-0}" == "1" ]]; then
    fail "STRICT_COMPLETENESS=1 but $TOOL_BIN is not built (cargo build --bin bridge-out-tool)"
else
    warn "skipping exact-block completeness: $TOOL_BIN not built (set STRICT_COMPLETENESS=1 to require it)"
fi

log "======================================================================"
log "  L2->L2 E2E PASS"
log "    OPT0 (origin net $L2B_NETWORK_ID):   $OPT0"
log "    forward:                 $FWD_AMOUNT_WEI wei -> $FWD_MIDEN_UNITS Miden units (gi $FWD_GI)"
log "    foreign-origin faucet:   $OPT0_FAUCET_ID"
log "    collision token COL:     $COL (net0 faucet $COL_FID_NET0, net2 faucet $COL_FID_NET2)"
log "    back:                    $BACK_AMOUNT Miden units -> $BACK_AMOUNT_WEI wei (gi $BACK_GI)"
log "    L2B holder net-zero:     $L2B_BAL_BEFORE_FORWARD == $L2B_BAL_FINAL"
log "======================================================================"
