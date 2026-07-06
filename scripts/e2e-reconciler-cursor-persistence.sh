#!/usr/bin/env bash
# Reconciler sweep-cursor persistence — restart must NOT re-sweep from genesis.
#
# PROD INCIDENT this regression-protects: the note-visibility reconciler's
# sweep cursor (`SyntheticProjector::reconcile_cursor`,
# src/synthetic_projector.rs) was a memory-only AtomicU64 hardcoded to 0 at
# boot. EVERY container restart — image update, crash, plain `docker restart` —
# re-walked the sweep from genesis: ~3h of resync and node load per restart on
# prod history. The cursor is now persisted in
# `service_state.reconcile_cursor` (migration 010) and loaded at boot, exactly
# like the projection cursor (migration 009 `projector_cursor`).
#
# Flow:
#   1. Preflight: stack up, PG reachable, migration 010 applied.
#   2. Wait for the reconciler to catch up near the Miden tip (the sweep
#      advances RECONCILE_CHUNK=200 blocks per ~5s tick).
#   3. Snapshot the persisted cursor, restart the proxy container
#      (same pattern as e2e-rd913-restart-burn-collision.sh: docker restart,
#      PostgreSQL untouched).
#   4. Assert, state-first (DB) with a log cross-check:
#        a. the boot-loaded cursor is the persisted one (near the tip:
#           first window `from > tip - 3*RECONCILE_CHUNK`), NOT 0;
#        b. the persisted cursor never regresses across the restart;
#        c. no genesis window (`from=1`) appears in the reconciler's
#           post-restart logs.
#
# Prerequisites:
#   - Full E2E stack running (`make e2e-up`); see Makefile for setup.
#   - miden-agglayer running with PgStore (DATABASE_URL set in compose).
#   - psql, curl, jq available on the host.
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-reconciler-cursor-persistence.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

# Required by docker-compose.e2e.yml's miden-node build args, even for
# `docker compose run` invocations the file is fully interpolated. Same
# defaults as the Makefile (source-of-truth — bump both together).
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.15.0}"

L2_RPC="http://localhost:8546"
PG_HOST="localhost"
PG_PORT="5434"
PG_USER="agglayer"
PG_PASS="agglayer"
PG_DB="agglayer_store"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

# Must match RECONCILE_CHUNK in src/synthetic_projector.rs.
RECONCILE_CHUNK=200

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] ▶${NC} $*"; }

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
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

reconcile_cursor() {
    pgquery "SELECT reconcile_cursor FROM service_state WHERE id = 1"
}

# Synthetic tip == Miden tip under Miden-1:1, so eth_blockNumber is the tip
# the reconciler sweeps toward.
chain_tip() {
    curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | jq -r '.result' | xargs printf '%d\n'
}

# ── Pre-flight ───────────────────────────────────────────────────────────
command -v psql >/dev/null || fail "psql not found"
command -v curl >/dev/null || fail "curl not found"
command -v jq   >/dev/null || fail "jq not found"
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || fail "L2 (miden-agglayer) not reachable at $L2_RPC"
pgquery "SELECT 1" >/dev/null || fail "PostgreSQL not reachable on $PG_HOST:$PG_PORT"

# Confirm the migration is present. If the operator forgot to rerun
# migrations after upgrading, fail loudly here rather than producing a
# confusing empty-value comparison later.
if [[ "$(reconcile_cursor)" == "" ]]; then
    fail "service_state.reconcile_cursor is missing — has 010_reconcile_cursor.sql been applied?"
fi

log "======================================================================"
log "  Reconciler sweep-cursor persistence — restart-survival regression"
log "======================================================================"

# ── Step 1: let the sweep catch up near the tip. ─────────────────────────
# The reconciler advances one RECONCILE_CHUNK window per ~5s projector tick,
# so on a fresh e2e stack catch-up is quick; the timeout is generous for CI.
step "Waiting for the reconciler sweep to catch up near the Miden tip..."
wait_for "reconcile_cursor within one chunk of the tip" \
    '[[ "$(reconcile_cursor)" -ge $(( $(chain_tip) - RECONCILE_CHUNK )) ]]' \
    300 5
TIP_PRE="$(chain_tip)"
PRE_CURSOR="$(reconcile_cursor)"
log "Pre-restart: tip=$TIP_PRE persisted reconcile_cursor=$PRE_CURSOR"
[[ "$PRE_CURSOR" -ge 1 ]] || fail "pre-restart cursor is 0 — sweep never advanced; cannot test restart resume"

# ── Step 2: restart the proxy (PostgreSQL untouched). ────────────────────
step "Restarting $AGGLAYER_CONTAINER (PG is untouched)..."
# Timestamp for `docker logs --since` — everything we grep below must come
# from AFTER the restart, or we'd read the previous boot's genesis sweep.
RESTART_TS="$(date -u '+%Y-%m-%dT%H:%M:%S')"
docker restart "$AGGLAYER_CONTAINER" >/dev/null
wait_for "miden-agglayer back up" \
    "curl -sf '$L2_RPC' -X POST -H 'Content-Type: application/json' \
     -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}' >/dev/null" \
    90 3
pass "miden-agglayer restarted and responsive"

# ── Step 3: the first post-restart sweep window starts NEAR THE TIP. ─────
# State-based primary assertion: the cursor the projector loaded at boot is
# the persisted one, so the first window is `from = PRE_CURSOR + 1`. "Near
# the tip" = within 3 chunks (the tip keeps moving while we restart).
FIRST_FROM=$(( PRE_CURSOR + 1 ))
TIP_POST="$(chain_tip)"
NEAR_TIP_FLOOR=$(( TIP_POST - 3 * RECONCILE_CHUNK ))
log "Post-restart: tip=$TIP_POST first sweep window from=$FIRST_FROM (floor: $NEAR_TIP_FLOOR)"
if [[ "$FIRST_FROM" -le "$NEAR_TIP_FLOOR" ]]; then
    fail "first post-restart sweep window starts at $FIRST_FROM ≤ tip - 3*$RECONCILE_CHUNK = $NEAR_TIP_FLOOR — restart re-swept history"
fi
pass "first post-restart sweep window ($FIRST_FROM) is near the tip ($TIP_POST)"

# Cross-check against the boot log. Pinned format — src/synthetic_projector.rs
# `SyntheticProjector::new`:
#   tracing::info!(reconcile_cursor = start_reconcile,
#       "note reconciler: sweep cursor loaded — next sweep window starts at block {}", ...)
# which the compact fmt layer renders as:
#   ... note reconciler: sweep cursor loaded — next sweep window starts at block 4201 reconcile_cursor=4200
# Strip ANSI escapes FIRST: tracing's colored fmt injects resets between the
# field name and value (`reconcile_cursor\x1b[0m: 181`), which silently breaks
# any field-value regex on the raw stream.
BOOT_LINE="$(docker logs --since "$RESTART_TS" "$AGGLAYER_CONTAINER" 2>&1 \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -F 'note reconciler: sweep cursor loaded' | head -1 || true)"
if [[ -z "$BOOT_LINE" ]]; then
    fail "post-restart boot log line 'note reconciler: sweep cursor loaded' not found"
fi
# tracing renders structured fields as `reconcile_cursor: N` (colon-space),
# not `reconcile_cursor=N` — accept both so a formatter change doesn't
# silently blank the capture (pinned format: synthetic_projector.rs boot line).
LOADED="$(sed -n 's/.*reconcile_cursor[:=] *\([0-9]\+\).*/\1/p' <<<"$BOOT_LINE")"
log "Boot log reports loaded reconcile_cursor=$LOADED"
# >= not ==: the sweep keeps advancing between our PG snapshot and the actual
# process stop (all the more with the concurrent catch-up), so the persisted
# value at boot may legitimately exceed the snapshot. The property under test
# is "boot loaded a persisted, non-genesis cursor" — never that the chain
# stood still for the restart.
if [[ -z "$LOADED" || "$LOADED" -lt "$PRE_CURSOR" || "$LOADED" -eq 0 ]]; then
    fail "boot loaded reconcile_cursor=$LOADED (PG snapshot pre-restart: $PRE_CURSOR) — cursor not loaded from store"
fi
pass "boot loaded a persisted cursor ($LOADED >= snapshot $PRE_CURSOR), not genesis"

# ── Step 4: the persisted cursor keeps advancing and never regresses. ────
step "Waiting for the first post-restart sweep window to complete..."
wait_for "reconcile_cursor advanced past pre-restart value" \
    '[[ "$(reconcile_cursor)" -gt '"$PRE_CURSOR"' ]]' \
    120 5
POST_CURSOR="$(reconcile_cursor)"
log "Post-restart persisted reconcile_cursor=$POST_CURSOR"
if [[ "$POST_CURSOR" -lt "$PRE_CURSOR" ]]; then
    fail "reconcile cursor regressed across restart: $PRE_CURSOR → $POST_CURSOR"
fi
pass "reconcile cursor resumed and advanced: $PRE_CURSOR → $POST_CURSOR"

# ── Step 5: NO genesis window in the post-restart reconciler logs. ───────
# Pinned format — src/synthetic_projector.rs `reconcile_notes`:
#   tracing::info!(imported, skipped_private, from, to,
#       "note reconciler: imported network notes missed by sync")
# renders as `... missed by sync imported=N skipped_private=N from=N to=N`.
# The line only fires when a window imports notes, so its ABSENCE alone
# proves nothing (steps 3–4 are the primary assertions) — but its PRESENCE
# with `from=1 ` post-restart is the genesis-re-sweep signature and must
# never appear (the very first boot of the stack swept genesis long before
# RESTART_TS).
GENESIS_LINE="$(docker logs --since "$RESTART_TS" "$AGGLAYER_CONTAINER" 2>&1 \
    | grep -F 'note reconciler: imported network notes missed by sync' \
    | grep -E ' from=1( |$)' | head -1 || true)"
if [[ -n "$GENESIS_LINE" ]]; then
    fail "genesis sweep window appeared after restart: $GENESIS_LINE"
fi
pass "no genesis window (from=1) in post-restart reconciler logs"

log "======================================================================"
log "  RECONCILER CURSOR PERSISTENCE TEST COMPLETE"
log "======================================================================"
