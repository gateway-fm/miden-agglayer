#!/usr/bin/env bash
# E2E Restore Test — verifies disaster recovery from PostgreSQL data loss.
#
# Flow:
#   1. Run L1→L2 bridge (deposit + claim) to populate state
#   2. Wipe all PostgreSQL tables (simulate data loss)
#   3. Run --restore to reconstruct state from miden node + L1
#   4. Verify restored state is functional by running L2→L1 bridge-out
#   5. Verify bridge-service can still sync logs from restored state
#
# Prerequisites:
#   - Full E2E stack running (docker compose up -d)
#   - miden-agglayer using PgStore (DATABASE_URL set)
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-restore.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
PG_HOST="localhost"
PG_PORT="5434"
PG_USER="agglayer"
PG_PASS="agglayer"
PG_DB="agglayer_store"
CONTAINER="miden-agglayer-miden-agglayer-1"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! eval "$cmd" 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
}

# ── Pre-flight checks ────────────────────────────────────────────────────────
command -v cast >/dev/null || fail "cast (foundry) not found"
command -v psql >/dev/null || fail "psql not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 not reachable"
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || fail "L2 (miden-agglayer) not reachable"
pgquery "SELECT 1" >/dev/null || fail "PostgreSQL not reachable on $PG_HOST:$PG_PORT"

log "======================================================================"
log "  Miden Bridge E2E Restore Test"
log "======================================================================"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# PART 1: Populate state with a normal L1→L2 bridge
# ══════════════════════════════════════════════════════════════════════════════
step "Part 1: Running L1→L2 bridge to populate state..."
"$SCRIPT_DIR/e2e-l1-to-l2.sh"
echo ""

# ── Capture pre-wipe state ────────────────────────────────────────────────────
step "Capturing pre-wipe PostgreSQL state..."
PRE_CLAIMS=$(pgquery "SELECT COUNT(*) FROM claimed_indices")
PRE_LOGS=$(pgquery "SELECT COUNT(*) FROM synthetic_logs")
PRE_GERS=$(pgquery "SELECT COUNT(*) FROM ger_entries")
PRE_NOTES=$(pgquery "SELECT COUNT(*) FROM bridge_out_processed")
PRE_BLOCK=$(pgquery "SELECT latest_block_number FROM service_state WHERE id = 1")

log "  Pre-wipe: claims=$PRE_CLAIMS, logs=$PRE_LOGS, gers=$PRE_GERS, notes=$PRE_NOTES, block=$PRE_BLOCK"
[[ "$PRE_CLAIMS" -gt 0 ]] || fail "Expected at least 1 claim before wipe"
[[ "$PRE_LOGS" -gt 0 ]] || fail "Expected at least 1 log before wipe"
[[ "$PRE_GERS" -gt 0 ]] || fail "Expected at least 1 GER before wipe"
pass "State populated"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# PART 2: Wipe PostgreSQL tables (simulate disaster)
# ══════════════════════════════════════════════════════════════════════════════
step "Part 2: Wiping all PostgreSQL tables (simulating data loss)..."
pgquery "TRUNCATE service_state, synthetic_logs, ger_entries, transactions, transaction_logs, nonces, claimed_indices, address_mappings, bridge_out_processed, block_transactions CASCADE"
# Re-insert the singleton service_state row
pgquery "INSERT INTO service_state (id) VALUES (1)"

POST_CLAIMS=$(pgquery "SELECT COUNT(*) FROM claimed_indices")
POST_LOGS=$(pgquery "SELECT COUNT(*) FROM synthetic_logs")
[[ "$POST_CLAIMS" -eq 0 ]] || fail "Tables not wiped"
[[ "$POST_LOGS" -eq 0 ]] || fail "Tables not wiped"
pass "PostgreSQL wiped — all tables empty"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# PART 3: Stop miden-agglayer, run --restore, restart
# ══════════════════════════════════════════════════════════════════════════════
step "Part 3: Running --restore inside the container..."

# Stop the running service
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    stop miden-agglayer >/dev/null 2>&1
sleep 2

# Run restore as a one-shot container (inherits volumes, env, network from compose)
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    run --rm --no-deps miden-agglayer \
    --miden-node=http://miden-node:57291 \
    --miden-store-dir=/var/lib/miden-agglayer-service \
    --restore 2>&1 | while IFS= read -r line; do echo "  [restore] $line"; done

RESTORE_EXIT=${PIPESTATUS[0]}
[[ "$RESTORE_EXIT" -eq 0 ]] || fail "Restore exited with code $RESTORE_EXIT"
pass "Restore completed successfully"

# ── Verify restored state ─────────────────────────────────────────────────────
step "Verifying restored PostgreSQL state..."
RST_CLAIMS=$(pgquery "SELECT COUNT(*) FROM claimed_indices")
RST_LOGS=$(pgquery "SELECT COUNT(*) FROM synthetic_logs")
RST_GERS=$(pgquery "SELECT COUNT(*) FROM ger_entries")
RST_NOTES=$(pgquery "SELECT COUNT(*) FROM bridge_out_processed")
RST_BLOCK=$(pgquery "SELECT latest_block_number FROM service_state WHERE id = 1")

log "  Restored: claims=$RST_CLAIMS, logs=$RST_LOGS, gers=$RST_GERS, notes=$RST_NOTES, block=$RST_BLOCK"

[[ "$RST_GERS" -gt 0 ]] || fail "No GERs restored"
[[ "$RST_BLOCK" -gt 0 ]] || fail "Block number not restored"
[[ "$RST_CLAIMS" -eq 0 ]] && warn "Claims not restored (OK — bridge-service will re-drive unclaimed deposits)"
pass "State restored from miden node + L1"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# PART 4: Restart miden-agglayer and verify it serves RPCs
# ══════════════════════════════════════════════════════════════════════════════
step "Part 4: Restarting miden-agglayer..."
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    start miden-agglayer >/dev/null 2>&1

wait_for "miden-agglayer healthy" \
    "curl -sf $L2_RPC -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}'" \
    60 3

# Verify RPC responds with restored block number
BLOCK_HEX=$(curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['result'])")
BLOCK_DEC=$((BLOCK_HEX))
log "  eth_blockNumber: $BLOCK_HEX ($BLOCK_DEC)"
[[ "$BLOCK_DEC" -gt 0 ]] || fail "Block number is 0 after restore"

# Verify GER endpoint works
GER=$(curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"zkevm_getLatestGlobalExitRoot","params":[],"id":1}' \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['result'])")
log "  Latest GER: $GER"
[[ "$GER" != "0x0000000000000000000000000000000000000000000000000000000000000000" ]] \
    || warn "Latest GER is zero — may be OK if aggoracle hasn't re-injected yet"

pass "miden-agglayer serving RPCs with restored state"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# PART 5: Run L2→L1 bridge-out on top of restored state
# ══════════════════════════════════════════════════════════════════════════════
step "Part 5: Running L2→L1 bridge-out on restored state..."

# Restart dependent services to pick up the restored miden-agglayer
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    restart bridge-service aggkit >/dev/null 2>&1
sleep 10

"$SCRIPT_DIR/e2e-l2-to-l1.sh"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# DONE
# ══════════════════════════════════════════════════════════════════════════════
log "======================================================================"
log "  RESTORE TEST COMPLETE"
log "  "
log "  Pre-wipe:  claims=$PRE_CLAIMS logs=$PRE_LOGS gers=$PRE_GERS block=$PRE_BLOCK"
log "  Restored:  claims=$RST_CLAIMS logs=$RST_LOGS gers=$RST_GERS block=$RST_BLOCK"
log "  L2→L1 bridge-out succeeded on restored state"
log "======================================================================"
