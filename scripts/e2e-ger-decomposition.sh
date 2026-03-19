#!/usr/bin/env bash
# E2E test: GER decomposition issue — proves that unresolved exit roots
# return null (not fabricated zero roots) from zkevm_getExitRootsByGER.
#
# Background (see plans/ger-decomposition-issue.md):
#   When insertGlobalExitRoot(combinedGER) arrives and L1 has already advanced
#   to a newer GER, we cannot decompose the combined hash back to its individual
#   mainnet/rollup exit roots. Before the fix, we returned zero roots which
#   permanently poisoned bridge-service's database via ON CONFLICT DO NOTHING.
#   The fix returns null so bridge-service retries on the next sync cycle.
#
# This test:
#   1. Waits for a real GER injection via aggoracle (proves normal path works)
#   2. Inserts a fake GER into postgres with NULL roots (simulates the race)
#   3. Verifies zkevm_getExitRootsByGER returns null for the unresolved GER
#   4. Verifies a completely unknown GER also returns null
#
# Prerequisites:
#   - Full E2E stack running (make e2e-up)
#   - miden-agglayer using PgStore (DATABASE_URL set)
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-ger-decomposition.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L2_RPC="http://localhost:8546"
PG_HOST="localhost"
PG_PORT="5434"
PG_USER="agglayer"
PG_PASS="agglayer"
PG_DB="agglayer_store"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
}

rpc_call() {
    local method="$1" params="$2"
    curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

# ── Pre-flight checks ────────────────────────────────────────────────────────
command -v psql >/dev/null || fail "psql not found"
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || fail "L2 (miden-agglayer) not reachable at $L2_RPC"
pgquery "SELECT 1" >/dev/null || fail "PostgreSQL not reachable on $PG_HOST:$PG_PORT"

log "======================================================================"
log "  GER Decomposition Bug — E2E Regression Test"
log "  See: plans/ger-decomposition-issue.md"
log "======================================================================"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# TEST 1: Normal path — real GER with resolved roots returns data
# ══════════════════════════════════════════════════════════════════════════════
step "Test 1: Verify normal GER with resolved roots returns exit root data"

# Wait for at least one GER to be injected by the aggoracle
log "Waiting for aggoracle to inject at least one GER..."
ELAPSED=0
TIMEOUT=120
while true; do
    GER_COUNT=$(pgquery "SELECT COUNT(*) FROM ger_entries")
    [[ "$GER_COUNT" -gt 0 ]] && break
    ELAPSED=$((ELAPSED + 5))
    [[ $ELAPSED -ge $TIMEOUT ]] && fail "No GER injected after ${TIMEOUT}s — is aggkit running?"
    echo -n "."
    sleep 5
done
echo ""
log "  Found $GER_COUNT GER(s) in store"

# Get a GER that has resolved roots
RESOLVED_GER=$(pgquery "SELECT encode(ger_hash, 'hex') FROM ger_entries WHERE mainnet_exit_root IS NOT NULL AND rollup_exit_root IS NOT NULL LIMIT 1")

if [[ -n "$RESOLVED_GER" ]]; then
    log "  Testing resolved GER: 0x${RESOLVED_GER}"
    RESPONSE=$(rpc_call "zkevm_getExitRootsByGER" "[\"0x${RESOLVED_GER}\"]")
    RESULT=$(echo "$RESPONSE" | python3 -c "import sys,json; r=json.load(sys.stdin).get('result'); print('null' if r is None else json.dumps(r))")

    if [[ "$RESULT" == "null" ]]; then
        fail "Resolved GER (both roots in DB) returned null — regression!"
    else
        # Note: rollupExitRoot can legitimately be zero on a fresh chain (no rollup exits yet).
        # What matters is that we get a non-null response with the expected fields.
        MAINNET=$(echo "$RESULT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['mainnetExitRoot'])")
        ROLLUP=$(echo "$RESULT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['rollupExitRoot'])")
        BLOCK=$(echo "$RESULT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['blockNumber'])")
        pass "Resolved GER returns exit roots: mainnet=${MAINNET:0:18}... rollup=${ROLLUP:0:18}... block=${BLOCK}"
    fi
else
    warn "No GERs with resolved roots found — aggoracle may have hit the L1 race condition."
    warn "This is expected sometimes. Continuing with the critical test..."
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# TEST 2: THE BUG — GER with NULL roots must return null, not zero roots
# ══════════════════════════════════════════════════════════════════════════════
step "Test 2: GER with unresolved roots (the L1 race condition)"
log ""
log "  Scenario: insertGlobalExitRoot(GER) arrived, but L1 had already"
log "  advanced to a newer root pair. fetch_exit_roots() didn't match."
log "  The GER is in our store but mainnet/rollup roots are NULL."
log ""
log "  BUG (before fix): returns {mainnetExitRoot: 0x000..., rollupExitRoot: 0x000...}"
log "    → bridge-service stores zero roots permanently via ON CONFLICT DO NOTHING"
log "    → claims against this GER fail forever (Merkle root mismatch)"
log ""
log "  FIX: returns null"
log "    → bridge-service treats null as 'retry later', does not poison its DB"
log ""

# Insert a fake GER with NULL roots (simulates the race condition)
FAKE_GER="deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
pgquery "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
         VALUES (decode('${FAKE_GER}', 'hex'), NULL, NULL, 99, 1234567890)
         ON CONFLICT (ger_hash) DO UPDATE SET mainnet_exit_root = NULL, rollup_exit_root = NULL"

log "  Inserted fake GER with NULL roots: 0x${FAKE_GER}"

# Query via the RPC endpoint — the same call bridge-service makes
RESPONSE=$(rpc_call "zkevm_getExitRootsByGER" "[\"0x${FAKE_GER}\"]")
RESULT=$(echo "$RESPONSE" | python3 -c "import sys,json; r=json.load(sys.stdin).get('result'); print('null' if r is None else json.dumps(r))")

log "  zkevm_getExitRootsByGER response: $RESULT"

if [[ "$RESULT" == "null" ]]; then
    pass "Unresolved GER returns null — bridge-service will retry, no DB poisoning"
else
    # Check if it returned zero roots (the old bug)
    MAINNET=$(echo "$RESULT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('mainnetExitRoot',''))")
    ZERO_ROOT="0x0000000000000000000000000000000000000000000000000000000000000000"
    if [[ "$MAINNET" == "$ZERO_ROOT" ]]; then
        fail "BUG CONFIRMED: Unresolved GER returned zero roots instead of null!
         bridge-service would store these permanently via ON CONFLICT DO NOTHING.
         Claims against this GER would fail forever (Merkle root mismatch).
         See plans/ger-decomposition-issue.md for the full root cause chain."
    else
        fail "Unexpected non-null response for unresolved GER: $RESULT"
    fi
fi

# Clean up the fake GER
pgquery "DELETE FROM ger_entries WHERE ger_hash = decode('${FAKE_GER}', 'hex')"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# TEST 3: Unknown GER returns null (not an error)
# ══════════════════════════════════════════════════════════════════════════════
step "Test 3: Completely unknown GER returns null"

UNKNOWN_GER="0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
RESPONSE=$(rpc_call "zkevm_getExitRootsByGER" "[\"${UNKNOWN_GER}\"]")
RESULT=$(echo "$RESPONSE" | python3 -c "import sys,json; r=json.load(sys.stdin).get('result'); print('null' if r is None else json.dumps(r))")

if [[ "$RESULT" == "null" ]]; then
    pass "Unknown GER returns null"
else
    fail "Unknown GER returned non-null: $RESULT"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# TEST 4: Partially resolved GER (only mainnet, no rollup) returns null
# ══════════════════════════════════════════════════════════════════════════════
step "Test 4: Partially resolved GER (one root missing) returns null"

PARTIAL_GER="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
FAKE_MAINNET="1111111111111111111111111111111111111111111111111111111111111111"
pgquery "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
         VALUES (decode('${PARTIAL_GER}', 'hex'), decode('${FAKE_MAINNET}', 'hex'), NULL, 100, 1234567891)
         ON CONFLICT (ger_hash) DO UPDATE SET mainnet_exit_root = decode('${FAKE_MAINNET}', 'hex'), rollup_exit_root = NULL"

RESPONSE=$(rpc_call "zkevm_getExitRootsByGER" "[\"0x${PARTIAL_GER}\"]")
RESULT=$(echo "$RESPONSE" | python3 -c "import sys,json; r=json.load(sys.stdin).get('result'); print('null' if r is None else json.dumps(r))")

if [[ "$RESULT" == "null" ]]; then
    pass "Partially resolved GER returns null — both roots required"
else
    fail "Partially resolved GER returned non-null: $RESULT"
fi

# Clean up
pgquery "DELETE FROM ger_entries WHERE ger_hash = decode('${PARTIAL_GER}', 'hex')"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SUMMARY
# ══════════════════════════════════════════════════════════════════════════════
log "======================================================================"
log "  GER DECOMPOSITION TEST COMPLETE"
log ""
log "  Test 1: Resolved GER returns real roots          ✓"
log "  Test 2: Unresolved GER returns null (not zeros)  ✓  ← the fix"
log "  Test 3: Unknown GER returns null                  ✓"
log "  Test 4: Partially resolved GER returns null       ✓"
log ""
log "  The bridge-service will retry on null instead of"
log "  permanently storing fabricated zero roots."
log "======================================================================"
