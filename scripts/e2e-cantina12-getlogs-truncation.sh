#!/usr/bin/env bash
# E2E — Cantina finding #12: eth_getLogs must ERROR (not silently truncate) at the row cap.
#
# WHAT THIS PROVES (and what it can't):
#   The authoritative coverage for finding #12 is the UNIT test
#   `finding_12_getlogs_over_cap_errors_not_truncates` in
#   src/store/postgres_tests.rs (PgStore) and src/store/memory.rs (InMemoryStore),
#   which insert CAP+1 logs into a range and assert get_logs returns an Err whose
#   message matches aggkit's `reMaxRange` parser.
#
#   Bridging >1000 REAL exits through the live stack to organically fill one
#   getLogs window is impractical (slow + expensive), and — post PR #94 — each
#   exit lands in its OWN synthetic block (1:1 Miden→block projection), so no
#   single real block ever approaches the cap anyway. So this e2e instead seeds
#   the PgStore's `synthetic_logs` table DIRECTLY (the same table the restore
#   replay and every bridge/GER event write to) with CAP+1 rows in one block, then
#   asserts the LIVE eth_getLogs JSON-RPC endpoint returns a JSON-RPC ERROR for
#   that range — NOT a truncated 1000-element success array. It is a contract
#   smoke of the getLogs row-cap surface, exercised end-to-end through the real
#   RPC handler + PgStore, and it cleans up the rows it inserts.
#
# Prerequisites:
#   - Full E2E stack running (make e2e-up)
#   - miden-agglayer using PgStore (DATABASE_URL / --database-url)
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-cantina12-getlogs-truncation.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

# shellcheck disable=SC1091
[[ -f "$FIXTURES_DIR/.env" ]] && source "$FIXTURES_DIR/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

FAIL_COUNT=0
PASS_COUNT=0
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; FAIL_COUNT=$((FAIL_COUNT + 1)); }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; PASS_COUNT=$((PASS_COUNT + 1)); }

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1"
}
rpc_call() {
    local method="$1" params="$2"
    curl -s --max-time 15 "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

# Keep the cap in lockstep with the source constant so this test can never drift.
ROW_CAP="$(grep -oP 'GETLOGS_ROW_CAP: usize = \K[0-9]+' "$PROJECT_DIR/src/store/mod.rs" || echo 1000)"
[[ "$ROW_CAP" =~ ^[0-9]+$ ]] || ROW_CAP=1000

# Pick per-run-unique high block numbers so we never collide with real data.
BASE=$(( 900000000 + (RANDOM * 1000) + RANDOM ))
BLOCK_OVER=$BASE                 # will hold CAP+1 rows  -> must ERROR
BLOCK_AT=$((BASE + 1))           # will hold exactly CAP -> must SUCCEED (boundary)
ZHASH=$(printf '0%.0s' {1..64})  # 32-byte all-zero block_hash as hex

cleanup() {
    pgquery "DELETE FROM synthetic_logs WHERE block_number IN ($BLOCK_OVER, $BLOCK_AT)" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── Pre-flight ───────────────────────────────────────────────────────────────
command -v psql >/dev/null || { echo "psql not found"; exit 1; }
command -v curl >/dev/null || { echo "curl not found"; exit 1; }
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || { echo "L2 (miden-agglayer) not reachable at $L2_RPC"; exit 1; }
pgquery "SELECT 1" >/dev/null || { echo "PostgreSQL not reachable on $PG_HOST:$PG_PORT"; exit 1; }

log "======================================================================"
log "  Cantina #12 — eth_getLogs row-cap: error, not silent truncation"
log "  row cap = $ROW_CAP (from src/store/mod.rs::GETLOGS_ROW_CAP)"
log "======================================================================"
echo ""

# ── Seed synthetic_logs directly ─────────────────────────────────────────────
step "Seeding synthetic_logs: $((ROW_CAP + 1)) rows @ block $BLOCK_OVER, $ROW_CAP rows @ block $BLOCK_AT"
cleanup
pgquery "INSERT INTO synthetic_logs
           (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
         SELECT g, '0xdead', ARRAY['0xabcd'], '0x', $BLOCK_OVER, decode('$ZHASH','hex'),
                '0xc12_over_' || g, 0, false
         FROM generate_series(1, $((ROW_CAP + 1))) AS g" >/dev/null
pgquery "INSERT INTO synthetic_logs
           (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
         SELECT g, '0xdead', ARRAY['0xabcd'], '0x', $BLOCK_AT, decode('$ZHASH','hex'),
                '0xc12_at_' || g, 0, false
         FROM generate_series(1, $ROW_CAP) AS g" >/dev/null

SEEDED_OVER=$(pgquery "SELECT count(*) FROM synthetic_logs WHERE block_number = $BLOCK_OVER")
if [[ "$SEEDED_OVER" == "$((ROW_CAP + 1))" ]]; then
    pass "Seed: block $BLOCK_OVER holds $SEEDED_OVER logs (> cap $ROW_CAP)"
else
    fail "Seed: expected $((ROW_CAP + 1)) rows at block $BLOCK_OVER, got $SEEDED_OVER"
fi
echo ""

# ── Test 1: over-cap range MUST error (not truncate) ─────────────────────────
step "Test 1: eth_getLogs over the row cap must return a JSON-RPC ERROR"
HEX_OVER=$(printf '0x%x' "$BLOCK_OVER")
RESP=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$HEX_OVER\",\"toBlock\":\"$HEX_OVER\"}]")

# 1a. It must be an error object, NOT a successful array of (truncated) logs.
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d and 'result' not in d" 2>/dev/null; then
    pass "1a Over-cap range returns a JSON-RPC error (no silent truncation)"
else
    RLEN=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('result',[])) if isinstance(d.get('result'),list) else -1)" 2>/dev/null || echo "?")
    fail "1a Over-cap range did NOT error — silently returned result (len=$RLEN). This is the finding-#12 regression. Resp: $RESP"
fi

# 1b. The error string must match aggkit's `reMaxRange` parser so a real consumer
#     (aggsender bridge/GER reader) re-chunks its request. Regex copied verbatim
#     from aggkit/common/errors.go:12.
ERRMSG=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error',{}).get('message',''))" 2>/dev/null || echo "")
if echo "$ERRMSG" | grep -qP 'block range too large, max range:\s*[0-9]+'; then
    pass "1b Error matches aggkit reMaxRange parser: \"$ERRMSG\""
else
    fail "1b Error does not match aggkit reMaxRange (consumer won't re-chunk). Got: \"$ERRMSG\""
fi
echo ""

# ── Test 2: exactly-at-cap range still succeeds (boundary) ───────────────────
step "Test 2: eth_getLogs at exactly the cap still returns a full success array"
HEX_AT=$(printf '0x%x' "$BLOCK_AT")
RESP=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$HEX_AT\",\"toBlock\":\"$HEX_AT\"}]")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d.get('result'); assert isinstance(r,list) and len(r)==$ROW_CAP" 2>/dev/null; then
    pass "2 At-cap range returns all $ROW_CAP logs (no false-positive error, no truncation)"
else
    RLEN=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('result',[])) if isinstance(d.get('result'),list) else 'err')" 2>/dev/null || echo "?")
    fail "2 At-cap range should return exactly $ROW_CAP logs, got len=$RLEN. Resp: $RESP"
fi
echo ""

# ── Summary ──────────────────────────────────────────────────────────────────
TOTAL=$((PASS_COUNT + FAIL_COUNT))
log "======================================================================"
log "  Cantina #12 getLogs truncation E2E complete — Passed: $PASS_COUNT / $TOTAL"
if [[ $FAIL_COUNT -gt 0 ]]; then
    echo -e "${RED}  Failed: $FAIL_COUNT / $TOTAL${NC}"
else
    log "  Failed: 0 / $TOTAL"
fi
log "======================================================================"
exit "$FAIL_COUNT"
