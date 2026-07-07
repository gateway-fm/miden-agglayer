#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# RD-862 focused repro: rapid-deposit GER-injection race.
#
# Measures the conversion rate from L1 bridgeAsset → bridge-service
# ready_for_claim under burst load, and enumerates orphan GERs (committed to
# our proxy but for which zkevm_getExitRootsByGER returns null — mainnet and/or
# rollup root unresolved).
#
# The hypothesis (per Linear RD-862): under rapid L1 deposits the aggoracle's
# fire-and-forget ProcessGER goroutines race, and by the time the proxy
# receives insertGlobalExitRoot(combinedGER) the live L1 lastMainnet/lastRollup
# pair has advanced past the combinedGER. fetch_l1_exit_roots fails the
# keccak(m||r)==combinedGER check and stores roots as None. Nothing ever
# retries, so the deposit covered by that GER never becomes ready_for_claim.
#
# Usage:
#   ./scripts/e2e-rd862-repro.sh                          # defaults
#   N_DEPOSITS=20 INTER_DELAY_MS=50 ./scripts/e2e-rd862-repro.sh
#
# Env:
#   N_DEPOSITS       number of back-to-back bridgeAsset calls (default 10)
#   INTER_DELAY_MS   sleep between L1 calls, millis (default 0 = back-to-back)
#   POLL_TIMEOUT     seconds to wait for all deposits to flip ready_for_claim
#                    (default 180)
#   POLL_INTERVAL    seconds between readiness polls (default 3)
#   DEPOSIT_WEI      wei per deposit (default 10000000000 = 1 Miden unit)
#
# Requires: stack already up (`make e2e-up` or equivalent).
#
# Exit codes:
#   0  all deposits reached ready_for_claim within the timeout
#   1  at least one deposit did NOT reach ready_for_claim (repro confirmed)
#   2  pre-flight failed (stack not up, bridge-service unreachable, etc.)
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
# shellcheck source=/dev/null
source "$FIXTURES_DIR/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1  # Miden network id — local topology patch pins MIDEN_NETWORK_ID=1 (see fixtures/patches)
BRIDGE_ADDRESS=$(grep -E '^BRIDGE_ADDRESS=' "$FIXTURES_DIR/.env" 2>/dev/null | head -1 | cut -d= -f2 | tr -d '"' || echo "0xC8cbEBf950B9Df44d987c8619f092beA980fF038")

N_DEPOSITS="${N_DEPOSITS:-10}"
INTER_DELAY_MS="${INTER_DELAY_MS:-0}"
POLL_TIMEOUT="${POLL_TIMEOUT:-180}"
POLL_INTERVAL="${POLL_INTERVAL:-3}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] RD-862:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || { fail "cast (foundry) not found"; exit 2; }
command -v jq >/dev/null || { fail "jq not found"; exit 2; }
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 \
    || { fail "L1 not reachable at $L1_RPC"; exit 2; }
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || { fail "L2 proxy not reachable at $L2_RPC"; exit 2; }
# Bridge-service reports compose-Healthy before its host-port endpoint is
# reliably serving, so a single probe races and flakes. Retry up to 60s.
bs_ok=false
for _ in $(seq 1 30); do
    if curl -sf "$BRIDGE_SERVICE_URL/" >/dev/null 2>&1 \
        || curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000000000000000000000000000" >/dev/null 2>&1; then
        bs_ok=true; break
    fi
    sleep 2
done
[[ "$bs_ok" == true ]] || { fail "bridge-service not reachable at $BRIDGE_SERVICE_URL (after 60s)"; exit 2; }
docker inspect "$AGGLAYER_CONTAINER" >/dev/null 2>&1 \
    || { fail "agglayer container '$AGGLAYER_CONTAINER' not running"; exit 2; }

# Resolve the deposit destination via the ISOLATED-wallet pattern (the proxy's
# sqlite store has a single owner; e2e clients never touch it). The bridge +
# eth-faucet ids from the toml (a config file, not the store) are only used to
# validate a reused store.
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || { fail "Could not read bridge_accounts.toml"; exit 2; }
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')
[[ -z "$BRIDGE_ID" || -z "$FAUCET_ID" ]] \
    && { fail "Could not parse bridge_accounts.toml"; exit 2; }

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-rd862-repro}"
ISO_NODE_URL="${ISO_NODE_URL:-${MIDEN_NODE_URL:-http://miden-node:57291}}"  # keep MIDEN_NODE_URL override working
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ID" \
    || { fail "Could not provision isolated bridge-out wallet"; exit 2; }

# ── Helpers ───────────────────────────────────────────────────────────────────
bridge_eth_tx() {
    local amount="$1"
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$amount" \
        0x0000000000000000000000000000000000000000 true 0x \
        --value "$amount" \
        --json 2>/dev/null \
        | jq -r '.transactionHash // empty'
}

deposit_counts() {
    # Returns "<total> <ready>" for this DEST_ADDR.
    local body
    body=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR?limit=100&offset=0" 2>/dev/null || echo '{}')
    local total ready
    total=$(jq -r '(.deposits // []) | length' <<<"$body" 2>/dev/null || echo 0)
    ready=$(jq -r '[.deposits[]? | select(.ready_for_claim == true)] | length' <<<"$body" 2>/dev/null || echo 0)
    echo "$total $ready"
}

# Query zkevm_getExitRootsByGER for a given GER hex (0x...). Returns:
#   "resolved" if both roots present
#   "null"     if the RPC returns JSON null (orphan)
#   "missing"  if the GER isn't even in the proxy's store
# We can't directly distinguish null from missing via the RPC (both return
# null), but we correlate with the committed-GER log set — anything committed
# but returning null is an orphan.
query_ger() {
    local ger="$1"
    curl -sf "$L2_RPC" -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"zkevm_getExitRootsByGER\",\"params\":[\"$ger\"],\"id\":1}" 2>/dev/null \
        | jq -r 'if .result == null then "null" else "resolved" end' 2>/dev/null \
        || echo "error"
}

# Extract committed GERs from the agglayer container logs since $1 (ISO-8601).
# tracing-subscriber emits `ger: HEX` (ANSI-bold label, then space + value).
# We strip ANSI and match ger followed by `:` or `=` then optional whitespace.
#
# IMPORTANT: use `.*` not `[^\n]*` to match "rest of line". macOS BSD grep
# treats `[^\n]` as "not the literal characters \ or n", so any line containing
# an `n` (e.g. "Miden") gets truncated before reaching the `ger: HEX` tag and
# the inner grep silently returns nothing. `.` already excludes newlines in
# basic/extended regex, so `.*` is the portable form.
committed_gers_since() {
    local since="$1"
    docker logs --since "$since" "$AGGLAYER_CONTAINER" 2>&1 \
        | sed -E 's/\x1b\[[0-9;]*[a-zA-Z]//g' \
        | grep -oE '(GER injection: submitting|UpdateGerNote (submitted|created|transaction committed)).*' \
        | grep -oE 'ger[:=][[:space:]]*[0-9a-f]{64}' \
        | sed -E 's/^ger[:=][[:space:]]*/0x/' | sort -u || true
}

# ── Go ────────────────────────────────────────────────────────────────────────
log "════════════════════════════════════════════════════════════════════"
log "  RD-862 rapid-deposit GER-injection race repro"
log "════════════════════════════════════════════════════════════════════"
log "  N_DEPOSITS=$N_DEPOSITS  INTER_DELAY_MS=$INTER_DELAY_MS"
log "  POLL_TIMEOUT=${POLL_TIMEOUT}s  DEPOSIT_WEI=$DEPOSIT_WEI"
log "  DEST_ADDR=$DEST_ADDR"
echo ""

# Baseline
read -r BASELINE_TOTAL BASELINE_READY <<<"$(deposit_counts)"
log "Baseline: $BASELINE_TOTAL deposits seen, $BASELINE_READY ready_for_claim"
START_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Fire burst
step "Firing $N_DEPOSITS deposits back-to-back..."
SUBMITTED_TXS=()
SUBMITTED_OK=0
for i in $(seq 1 "$N_DEPOSITS"); do
    # Vary the amount so every deposit produces a distinct L1 leaf/GER.
    amount=$((DEPOSIT_WEI + i))
    if tx=$(bridge_eth_tx "$amount"); then
        if [[ -n "$tx" ]]; then
            SUBMITTED_TXS+=("$tx")
            SUBMITTED_OK=$((SUBMITTED_OK + 1))
        fi
    fi
    if [[ "$INTER_DELAY_MS" -gt 0 ]]; then
        # bash `sleep` accepts fractional seconds.
        python3 -c "import time; time.sleep($INTER_DELAY_MS / 1000)"
    fi
done
pass "Submitted: $SUBMITTED_OK / $N_DEPOSITS L1 bridgeAsset txs"
[[ "$SUBMITTED_OK" -eq 0 ]] && { fail "No deposits submitted; aborting"; exit 2; }

# Target counts
TARGET_TOTAL=$((BASELINE_TOTAL + SUBMITTED_OK))
TARGET_READY=$((BASELINE_READY + SUBMITTED_OK))

step "Polling bridge-service (timeout ${POLL_TIMEOUT}s, interval ${POLL_INTERVAL}s)..."
elapsed=0
FINAL_TOTAL=$BASELINE_TOTAL
FINAL_READY=$BASELINE_READY
while [[ $elapsed -lt $POLL_TIMEOUT ]]; do
    read -r FINAL_TOTAL FINAL_READY <<<"$(deposit_counts)"
    log "  t+${elapsed}s  seen=$FINAL_TOTAL/$TARGET_TOTAL  ready=$FINAL_READY/$TARGET_READY"
    if [[ "$FINAL_READY" -ge "$TARGET_READY" ]]; then
        break
    fi
    sleep "$POLL_INTERVAL"
    elapsed=$((elapsed + POLL_INTERVAL))
done
echo ""

DELTA_SEEN=$((FINAL_TOTAL - BASELINE_TOTAL))
DELTA_READY=$((FINAL_READY - BASELINE_READY))

# ── Orphan enumeration ────────────────────────────────────────────────────────
# RD-862's canonical metric is the *permanent* orphan: a GER whose roots NEVER
# resolve because "nothing ever retries". With the L1InfoTreeIndexer in place,
# the retry exists — it backfills (mainnet, rollup) from `UpdateL1InfoTree`
# events at a 1s cadence. Resolution is therefore eventually-consistent and
# asynchronous: a GER whose L1 leaf was emitted in the last second can be
# momentarily null at the instant we enumerate, then resolve a beat later. That
# transient is NOT the bug. So re-poll each null GER over a bounded grace window
# (ORPHAN_GRACE_SECS, default 45 — comfortably above the ~1s indexer cadence
# plus aggoracle jitter); only a GER still null after the window is a true
# orphan. A genuine RD-862 phantom GER (a (mainnet,rollup) pair L1 never
# emitted) never appears as an `UpdateL1InfoTree` leaf, so it stays null through
# the whole window and is still caught.
ORPHAN_GRACE_SECS="${ORPHAN_GRACE_SECS:-45}"
step "Enumerating GERs committed by the proxy since $START_TS (orphan grace ${ORPHAN_GRACE_SECS}s)..."
mapfile -t GERS < <(committed_gers_since "$START_TS" | sort -u)
ORPHANS=()
RESOLVED=0
for g in "${GERS[@]}"; do
    [[ -z "$g" ]] && continue
    status=$(query_ger "$g")
    if [[ "$status" == "null" ]]; then
        # Give the async indexer time to backfill before declaring it orphaned.
        waited=0
        while [[ "$status" == "null" && $waited -lt $ORPHAN_GRACE_SECS ]]; do
            sleep 3
            waited=$((waited + 3))
            status=$(query_ger "$g")
        done
        [[ "$status" == "resolved" ]] \
            && log "  GER $g resolved after ${waited}s (indexer backfill — not an orphan)"
    fi
    case "$status" in
        resolved) RESOLVED=$((RESOLVED + 1)) ;;
        null)     ORPHANS+=("$g") ;;
        *)        warn "  unexpected zkevm_getExitRootsByGER status for $g: $status" ;;
    esac
done

# ── Report ────────────────────────────────────────────────────────────────────
echo ""
log "════════════════════════════════════════════════════════════════════"
log "  RD-862 Repro Report"
log "════════════════════════════════════════════════════════════════════"
log "  Deposits submitted:            $SUBMITTED_OK / $N_DEPOSITS"
log "  Deposits seen by bridge-svc:   $DELTA_SEEN  (Δ over baseline)"
log "  Deposits ready_for_claim:      $DELTA_READY / $SUBMITTED_OK"
log "  Conversion rate:               $(awk "BEGIN{printf \"%.1f%%\", ($DELTA_READY/$SUBMITTED_OK)*100}")"
log "  GERs committed by proxy:       ${#GERS[@]}"
log "  GERs resolved (both roots):    $RESOLVED"
log "  GERs orphaned (roots = null):  ${#ORPHANS[@]}"
if [[ ${#ORPHANS[@]} -gt 0 ]]; then
    log "  Orphan GERs:"
    for g in "${ORPHANS[@]}"; do
        log "    $g"
    done
fi
echo ""

ALL_READY=0
[[ "$DELTA_READY" -ge "$SUBMITTED_OK" ]] && ALL_READY=1

# Pass criterion per tests/baselines/baseline-rd862-repro.json:
#   "metric_to_drive_to_zero": "orphan_rate_pct"
#   "Conversion rate is informational; orphan rate is canonical."
#
# Orphan rate > 0 = the GER decomposition race manifested → HARD FAIL (CI gate).
# Conversion rate < 100% under burst load is normal: aggoracle injects ~one GER
# per ~30-60s and only the LATEST L1InfoTree leaf at inject time is committed,
# so a 30-deposit burst takes multiple aggoracle cycles to fully cover. The
# conversion-rate warning is preserved as a perf signal but no longer fails CI.
if [[ ${#ORPHANS[@]} -gt 0 ]]; then
    fail "RD-862 race MANIFESTED: ${#ORPHANS[@]}/${#GERS[@]} GERs orphaned"
    exit 1
fi

if [[ "$ALL_READY" -ne 1 ]]; then
    warn "conversion rate < 100% ($((SUBMITTED_OK - DELTA_READY))/$SUBMITTED_OK deposits not yet ready_for_claim) — informational, not a fail. aggoracle cadence is the bottleneck under burst load. Bump POLL_TIMEOUT or reduce N_DEPOSITS to drive conversion to 100% for perf-tracking runs."
fi

pass "Zero orphan GERs — RD-862 race did NOT manifest (canonical metric)."
exit 0
