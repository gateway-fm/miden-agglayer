#!/usr/bin/env bash
# E2E — Cantina finding #12 (redesign): eth_getLogs returns ALL matches, never truncates.
#
# WHAT THIS PROVES (and what it can't):
#   The redesign (commit a5bd007) removed the normal-operation row cap. get_logs
#   now pushes a SAFE SUPERSET of the address/topic filter into SQL, streams the
#   WHOLE result set, and applies the exact `matches()` filter — so a dense range
#   with thousands of matching logs is returned IN FULL, not truncated and not
#   errored. The ONLY limit is an OOM backstop, GETLOGS_SAFETY_CEILING (500_000),
#   whose error path is covered by the UNIT tests
#   `finding_12_getlogs_returns_all_no_row_cap` (returns-all) and the
#   property-based `getlogs_equivalence_matches_oracle_*` (exact matches()) in
#   src/store/memory.rs + src/store/postgres_tests.rs.
#
#   Seeding 500_000 rows just to trip the ceiling is impractical, so this e2e does
#   NOT exercise the ceiling. Instead it proves the important new contract against
#   the LIVE RPC: a MODERATELY DENSE range (well over the OLD 1000-row cap) is
#   returned WHOLE — count matches, no truncation, no JSON-RPC error. This is the
#   exact OPPOSITE of the pre-redesign assertion (which expected an error at
#   CAP+1). It seeds the PgStore's `synthetic_logs` table directly (the same table
#   restore replay and every bridge/GER event write to), queries eth_getLogs
#   end-to-end through the real RPC handler + PgStore, and cleans up its rows.
#
# Prerequisites:
#   - Full E2E stack running (make e2e-up)
#   - miden-agglayer using PgStore (DATABASE_URL / --database-url)
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-cantina12-getlogs-returns-all.sh
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
    curl -s --max-time 30 "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

# The redesign's only limit is the OOM backstop. Read the NEW constant so the
# script tracks the source; the seed just needs to be comfortably over the OLD
# 1000-row cap (to prove "no truncation") while staying far under the ceiling.
CEILING="$(grep -oP 'GETLOGS_SAFETY_CEILING: usize = \K[0-9_]+' "$PROJECT_DIR/src/store/mod.rs" | tr -d '_' || echo 500000)"
[[ "$CEILING" =~ ^[0-9]+$ ]] || CEILING=500000

# Moderately dense: > OLD cap (1000), << ceiling. This is the count the old code
# would have ERRORED on and truncated; the redesign must return all of them.
DENSE=2500

# Pick per-run-unique high block numbers so we never collide with real data.
BASE=$(( 900000000 + (RANDOM * 1000) + RANDOM ))
BLOCK_DENSE=$BASE                                     # holds $DENSE rows -> must ALL return
ADDR="0x$(printf '%08x' "$((RANDOM * RANDOM))")dead"  # per-run-unique address
ZHASH=$(printf '0%.0s' {1..64})                       # 32-byte all-zero block_hash as hex

cleanup() {
    pgquery "DELETE FROM synthetic_logs WHERE block_number = $BLOCK_DENSE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── Pre-flight ───────────────────────────────────────────────────────────────
command -v psql >/dev/null || { echo "psql not found"; exit 1; }
command -v curl >/dev/null || { echo "curl not found"; exit 1; }
command -v python3 >/dev/null || { echo "python3 not found"; exit 1; }
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || { echo "L2 (miden-agglayer) not reachable at $L2_RPC"; exit 1; }
pgquery "SELECT 1" >/dev/null || { echo "PostgreSQL not reachable on $PG_HOST:$PG_PORT"; exit 1; }

log "======================================================================"
log "  Cantina #12 — eth_getLogs returns ALL matches (never truncates)"
log "  dense seed = $DENSE rows (> old 1000-row cap, << ceiling $CEILING)"
log "======================================================================"
echo ""

# ── Seed synthetic_logs directly ─────────────────────────────────────────────
step "Seeding synthetic_logs: $DENSE rows @ block $BLOCK_DENSE, address $ADDR"
cleanup
pgquery "INSERT INTO synthetic_logs
           (log_index, address, topics, data, block_number, block_hash, transaction_hash, transaction_index, removed)
         SELECT g, '$ADDR', ARRAY['0xabcd'], '0x', $BLOCK_DENSE, decode('$ZHASH','hex'),
                '0xc12_all_' || g, 0, false
         FROM generate_series(1, $DENSE) AS g" >/dev/null

SEEDED=$(pgquery "SELECT count(*) FROM synthetic_logs WHERE block_number = $BLOCK_DENSE")
if [[ "$SEEDED" == "$DENSE" ]]; then
    pass "Seed: block $BLOCK_DENSE holds $SEEDED logs (> old cap 1000)"
else
    fail "Seed: expected $DENSE rows at block $BLOCK_DENSE, got $SEEDED"
fi
echo ""

HEX_DENSE=$(printf '0x%x' "$BLOCK_DENSE")

# ── Test 1: dense range returns ALL rows — no error, no truncation ───────────
step "Test 1: eth_getLogs over a >1000-row range must return ALL $DENSE logs"
RESP=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$HEX_DENSE\",\"toBlock\":\"$HEX_DENSE\",\"address\":\"$ADDR\"}]")

# 1a. It must be a SUCCESS array, NOT a JSON-RPC error (the OLD behaviour).
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d and isinstance(d['result'],list) and 'error' not in d" 2>/dev/null; then
    pass "1a Dense range returns a success array (no false-positive row-cap error)"
else
    ERRMSG=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error',{}).get('message',''))" 2>/dev/null || echo "")
    fail "1a Dense range returned an ERROR instead of results — this is the pre-redesign row-cap regression. Error: \"$ERRMSG\" Resp head: ${RESP:0:200}"
fi

# 1b. The array must hold EVERY seeded row — no silent truncation.
RLEN=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d.get('result'); print(len(r) if isinstance(r,list) else -1)" 2>/dev/null || echo "-1")
if [[ "$RLEN" == "$DENSE" ]]; then
    pass "1b Returned ALL $RLEN logs (no truncation) — the finding-#12 contract"
else
    fail "1b Expected $DENSE logs, got $RLEN — truncation/loss detected. Resp head: ${RESP:0:200}"
fi

# 1c. Results must be ordered by (blockNumber, logIndex) — the eth_getLogs contract.
if echo "$RESP" | python3 -c "
import sys,json
r=json.load(sys.stdin)['result']
keys=[(int(x['blockNumber'],16), int(x['logIndex'],16)) for x in r]
assert keys==sorted(keys), 'not ordered by (blockNumber, logIndex)'
" 2>/dev/null; then
    pass "1c Results ordered by (blockNumber, logIndex)"
else
    fail "1c Results NOT ordered by (blockNumber, logIndex)"
fi
echo ""

# ── Summary ──────────────────────────────────────────────────────────────────
TOTAL=$((PASS_COUNT + FAIL_COUNT))
log "======================================================================"
log "  Cantina #12 getLogs returns-all E2E complete — Passed: $PASS_COUNT / $TOTAL"
if [[ $FAIL_COUNT -gt 0 ]]; then
    echo -e "${RED}  Failed: $FAIL_COUNT / $TOTAL${NC}"
else
    log "  Failed: 0 / $TOTAL"
fi
log "  (ceiling-error path at GETLOGS_SAFETY_CEILING=$CEILING is covered by unit tests)"
log "======================================================================"
exit "$FAIL_COUNT"
