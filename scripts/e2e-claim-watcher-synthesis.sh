#!/usr/bin/env bash
# E2E synthesis-path test for the CLAIM watcher (src/claim_watcher.rs).
#
# Companion to scripts/e2e-claim-watcher.sh — that one verifies the happy path
# (watcher observes a CLAIM whose ClaimEvent was already written, increments
# `claim_watcher_already_recorded_total`). This one verifies the failure-mode
# path the watcher actually exists to fix: a CLAIM note has been consumed on
# Miden but the corresponding ClaimEvent is MISSING from the store. The
# watcher must detect that and write a synthetic ClaimEvent (incrementing
# `claim_watcher_synthesised_total`).
#
# Reproduces the production EFAD-style desync (see RD-862 / RD-860 follow-up):
# `claimed_indices` row exists, normal-path `publish_claim` crashed before
# `txn_commit` wrote the synthetic_log → bridge-service is unaware → users
# stuck `ready_for_claim` forever. This test confirms the watcher closes that
# loop without operator intervention.
#
# Pre-condition: a prior L1→L2 e2e run completed, leaving:
#   - one row in `synthetic_logs` with the ClaimEvent topic (the normal-path emission)
#   - one row in `claim_watcher_processed` (the watcher's `already_recorded` hit
#     from `scripts/e2e-claim-watcher.sh`)
#
# Steps:
#   1. Snapshot baseline `claim_watcher_synthesised_total`.
#   2. Locate the ClaimEvent row in `synthetic_logs` and its global_index.
#   3. Locate the matching `claim_watcher_processed` row by global_index.
#   4. DELETE both rows — this simulates the crash-recovery / desync state
#      where the CLAIM is consumed on Miden but the proxy's store has no
#      record of the ClaimEvent.
#   5. Rewind the persisted projector cursor to just before the claim's
#      consumption block and RESTART the proxy, which is how that block gets
#      re-projected (see "WHY A REWIND" below).
#   6. Verify a ClaimEvent was re-synthesised, BYTE-IDENTICAL to the row that
#      was deleted, is recoverable via `has_claim_event_for_global_index`, and
#      that no decode/unrecoverable counters fired.
#
# WHY A REWIND, NOT A SYNC-TICK WAIT
#
# This test used to delete the rows and wait for the next Miden sync tick, on
# the premise that a live `ClaimWatcher` SyncListener re-enumerated consumed
# notes every tick and re-synthesised anything missing from the store. That
# listener no longer exists: the synthetic-indexer redesign made the
# SyntheticProjector the SOLE synthetic-event producer (see the module header
# of src/claim_watcher.rs), and the projector is CURSOR-DRIVEN and FORWARD-ONLY
# — `tick_pass` projects blocks strictly above an in-memory cursor. A row
# deleted behind that cursor is never revisited, BY DESIGN: emitting into a
# sealed block would break eth_getLogs immutability, which is why the
# completeness auditor alarms on a miss and explicitly never heals it late.
#
# So the old wait was unsatisfiable, and it failed for a second reason too: it
# waited on `miden_sync_state_duration_seconds_count`, a histogram recorded only
# on the commit-wait hot path, which does not move at all when the test
# generates no traffic. It observed "only 0 tick(s) in 90s" and then failed with
# "Sync ticks ARE observed ... so a longer wait is NOT the fix" — a message that
# contradicted its own measurement.
#
# Re-projection is what heals this state, and it needs the cursor moved back.
# The cursor is an AtomicU64 cached in the projector and loaded once in `new()`,
# so rewinding the persisted value requires a process restart to take effect —
# rewind-then-restart IS the operator recovery action, and that is what this
# test now exercises.
#
# Usage:
#   make e2e-l1-to-l2 && make e2e-claim-watcher && bash scripts/e2e-claim-watcher-synthesis.sh
#
set -euo pipefail

L2_RPC="${L2_RPC:-http://localhost:8546}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"
SYNC_WAIT_SECS="${SYNC_WAIT_SECS:-90}"   # DEADLINE for the re-projection wait, not a fixed sleep
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-miden-agglayer-miden-agglayer-1}"
CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }

command -v psql >/dev/null || fail "psql not found (apt-get install postgresql-client)"
command -v curl >/dev/null || fail "curl not found"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)

# Run a psql query and emit ONLY the result on stdout. We deliberately drop
# stderr because `psql` on systems where the locale isn't generated (common
# on minimal LXCs / CI runners) emits multi-line perl warnings on stderr,
# and the prior `2>&1` capture pattern was concatenating them into the
# query result, breaking downstream regex extraction. Real psql errors
# manifest as empty output, which every caller already checks via `[[ -z ]]`
# or by validating the result shape.
pgq() {
    # STOPPER on DB error (task #26 sweep): pre-fix `2>/dev/null` turned a dead
    # Postgres into an empty string, which ${VAR:-0} then misread as "0 rows".
    # stderr stays SEPARATE from the capture (locale warnings are rc=0 noise
    # that must not corrupt numeric parses — see header comment) and is
    # surfaced only when psql actually fails.
    local out errf rc
    errf="$(mktemp)"
    out=$("${PSQL[@]}" -c "$1" 2>"$errf"); rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "pgq FAILED (rc=$rc): $(cat "$errf")" >&2
        rm -f "$errf"
        return 1
    fi
    rm -f "$errf"
    printf '%s\n' "$out"
}

# Bootstrap: if the script is run on a fresh stack with no prior L1→L2
# deposit, the ClaimEvent row this test relies on doesn't exist yet.
# Auto-run the prerequisites unless the caller disables it.
AUTO_BOOTSTRAP="${AUTO_BOOTSTRAP:-1}"

ensure_prereq_state() {
    local existing
    existing=$(pgq "SELECT 1 FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' LIMIT 1;")
    if [[ -n "$existing" ]]; then
        log "ClaimEvent row already present in synthetic_logs — skipping bootstrap"
        return 0
    fi
    if [[ "$AUTO_BOOTSTRAP" != "1" ]]; then
        fail "no ClaimEvent in synthetic_logs and AUTO_BOOTSTRAP=0 — run 'make e2e-l1-to-l2 && make e2e-claim-watcher' first"
    fi
    step "Bootstrap: no ClaimEvent yet — running e2e-l1-to-l2 + e2e-claim-watcher"
    "$SCRIPT_DIR/e2e-l1-to-l2.sh" >/dev/null
    "$SCRIPT_DIR/e2e-claim-watcher.sh" >/dev/null
    log "Bootstrap complete"
}

# Pull a Prometheus counter value (single un-labeled sample). Returns 0 if absent.
counter() {
    local name="$1" body value
    # STOPPER on unreachable /metrics (task #26 sweep): pre-fix, a down proxy
    # read as 0 — a baseline taken against a dead endpoint could false-PASS
    # delta assertions. Absent metric stays a legit 0 (never-incremented).
    body=$(curl -sf "${L2_RPC}/metrics") || fail "metrics endpoint unreachable: ${L2_RPC}/metrics"
    value=$(awk -v n="$name" '
        $0 ~ ("^" n " ") { print $2; found=1; exit }
        END { if (!found) print 0 }
    ' <<<"$body")
    echo "${value%.*}"
}

# ── Step 0: Ensure prereq state exists (bootstrap on fresh stack) ─────────────
ensure_prereq_state

# ── Step 1: Snapshot baseline counters + log offset ───────────────────────────
# We assert against DB state and proxy log emissions, not /metrics counters.
# Background: `claim_watcher_synthesised_total` (defined in src/claim_watcher.rs
# at the counter! macro call site) is observed to NOT increment past 1 even when
# the synthesis path fires multiple times — likely a Rust `metrics` crate
# handle-sharing issue, deferred to a follow-up. DB state and structured logs
# are the load-bearing observability for this regression.
step "Snapshotting baseline /metrics + DB state"
BASE_SYNTH=$(counter claim_watcher_synthesised_total)
BASE_ALREADY=$(counter claim_watcher_already_recorded_total)
BASE_DECODE=$(counter claim_watcher_storage_decode_total)
BASE_UNRECOV=$(counter claim_watcher_unrecoverable_total)
log "  baseline /metrics: synthesised=${BASE_SYNTH} already=${BASE_ALREADY} decode_err=${BASE_DECODE} unrecov=${BASE_UNRECOV}"
LOG_OFFSET=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "synthesised ClaimEvent" || true)
log "  baseline synthesised log lines: ${LOG_OFFSET}"

# ── Step 2: Locate the ClaimEvent in synthetic_logs ───────────────────────────
step "Locating ClaimEvent row in synthetic_logs"
LOG_ROW=$(pgq "SELECT data FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' ORDER BY block_number DESC LIMIT 1;")
[[ -z "$LOG_ROW" ]] && fail "no ClaimEvent in synthetic_logs — run 'make e2e-l1-to-l2' first"
# ABI data layout: 0x + 32-byte global_index + ... (the rest is origin_network, etc.)
GI_HEX=$(echo "$LOG_ROW" | sed -E 's/^0x([0-9a-f]{64}).*/\1/')
[[ ${#GI_HEX} -ne 64 ]] && fail "could not extract global_index from synthetic_logs data row: $LOG_ROW"
log "  global_index = 0x${GI_HEX}"

# ── Step 3: Locate the matching row in claim_watcher_processed ────────────────
step "Locating claim_watcher_processed row for this global_index"
NOTE_ID=$(pgq "SELECT note_id FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex') LIMIT 1;" || true)
if [[ -n "$NOTE_ID" ]]; then
    log "  note_id = ${NOTE_ID}"
else
    warn "  no claim_watcher_processed row — watcher may not have ticked yet. Proceeding (only synthetic_logs needs deletion to trigger synthesis path)."
fi

# ── Step 4: Simulate the desync — delete both rows ────────────────────────────
step "Deleting synthetic_logs ClaimEvent row to simulate crash-recovery desync"
# Fingerprint the row BEFORE deleting it. "A log line appeared" only proves the
# projector wrote SOMETHING; the claim this test makes is that re-projection
# reproduces the SAME event, so capture every field a consumer reads and
# compare it back at the end.
CLAIM_ROW_BEFORE=$(pgq "SELECT block_number || '|' || transaction_hash || '|' || encode(block_hash,'hex') \
    || '|' || address || '|' || array_to_string(topics, ',') || '|' || transaction_index \
    || '|' || removed || '|' || data \
    FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND data LIKE '0x${GI_HEX}%' \
    ORDER BY block_number DESC LIMIT 1;")
[[ -n "$CLAIM_ROW_BEFORE" ]] || fail "could not fingerprint the ClaimEvent row before deleting it"
CLAIM_BLOCK="${CLAIM_ROW_BEFORE%%|*}"
[[ "$CLAIM_BLOCK" =~ ^[0-9]+$ ]] || fail "could not read the ClaimEvent's block_number (got '$CLAIM_BLOCK')"
log "  ClaimEvent is at Miden block ${CLAIM_BLOCK}"

DEL_LOGS=$(pgq "DELETE FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND data LIKE '0x${GI_HEX}%' RETURNING block_number;")
log "  deleted synthetic_logs rows: $(echo "$DEL_LOGS" | wc -l)"

if [[ -n "${NOTE_ID:-}" ]]; then
    step "Deleting claim_watcher_processed row so the watcher re-evaluates this CLAIM"
    DEL_WATCHER=$(pgq "DELETE FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex') RETURNING note_id;")
    log "  deleted claim_watcher_processed rows: $(echo "$DEL_WATCHER" | wc -l)"
fi

# Sanity-check: the predicate the watcher uses MUST now return false.
HAS_AFTER_DELETE=$(pgq "SELECT EXISTS(SELECT 1 FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex')) OR EXISTS(SELECT 1 FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${GI_HEX}%');")
[[ "$HAS_AFTER_DELETE" != "f" ]] && fail "ClaimEvent state still recoverable after delete (got '$HAS_AFTER_DELETE') — test setup broken; check schema"
log "  has_claim_event_for_global_index simulated → false ✓"

# ── Step 5: Rewind the projector cursor and restart, so the block re-projects ─
# The projector caches its cursor in memory (loaded in `new()`), so the
# persisted rewind only takes effect on the next boot. Rewind to CLAIM_BLOCK-1:
# the minimum window that contains the deleted event. Re-projecting the blocks
# above it is safe and is the documented crash-recovery shape — every other note
# in that range carries its processed marker (claim_watcher_processed,
# bridge_out_processed, ger_entries.is_injected) and is an idempotent no-op;
# only THIS claim, whose marker the test deleted, is re-emitted.
step "Rewinding projector_cursor to $((CLAIM_BLOCK - 1)) and restarting the proxy"
CURSOR_BEFORE=$(pgq "SELECT projector_cursor FROM service_state WHERE id = 1;")
log "  projector_cursor: ${CURSOR_BEFORE} -> $((CLAIM_BLOCK - 1))"
pgq "UPDATE service_state SET projector_cursor = $((CLAIM_BLOCK - 1)) WHERE id = 1;" >/dev/null \
    || fail "could not rewind projector_cursor — the block would never be re-projected"

docker restart "$AGGLAYER_CONTAINER" >/dev/null \
    || fail "could not restart $AGGLAYER_CONTAINER"
# Wait for the process to be serving again before timing anything on it.
_w=0
while (( _w < 180 )); do
    [[ "$(curl -s -m5 -o /dev/null -w '%{http_code}' "${L2_RPC}/health" 2>/dev/null)" != "000" ]] && break
    sleep 3; _w=$((_w+3))
done
(( _w < 180 )) || fail "proxy did not serve /health within 180s after the restart"
log "  proxy back up after ${_w}s"

step "Waiting (<=${SYNC_WAIT_SECS}s) for the projector to re-project block ${CLAIM_BLOCK}"
_w=0; CURSOR_NOW=""
while (( _w < SYNC_WAIT_SECS )); do
    CURSOR_NOW=$(pgq "SELECT projector_cursor FROM service_state WHERE id = 1;" || echo "")
    if [[ "$CURSOR_NOW" =~ ^[0-9]+$ ]] && (( CURSOR_NOW >= CLAIM_BLOCK )); then
        log "  projector_cursor reached ${CURSOR_NOW} (>= ${CLAIM_BLOCK}) after ${_w}s"
        break
    fi
    sleep 3; _w=$((_w+3))
done
[[ "$CURSOR_NOW" =~ ^[0-9]+$ ]] && (( CURSOR_NOW >= CLAIM_BLOCK )) \
    || fail "projector_cursor stalled at '${CURSOR_NOW}' (needed >= ${CLAIM_BLOCK}) within ${SYNC_WAIT_SECS}s — \
the projector never re-reached the claim's block, so nothing could have re-synthesised. \
Check: docker logs ${AGGLAYER_CONTAINER} 2>&1 | grep -i 'projector' | tail -20"

# ── Step 6: Verify synthesis fired (DB state + log emission, NOT /metrics) ───
step "Sampling /metrics + DB + proxy logs after synthesis window"
NEW_SYNTH=$(counter claim_watcher_synthesised_total)
NEW_ALREADY=$(counter claim_watcher_already_recorded_total)
NEW_DECODE=$(counter claim_watcher_storage_decode_total)
NEW_UNRECOV=$(counter claim_watcher_unrecoverable_total)
LOG_NEW=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "synthesised ClaimEvent" || true)
log "  after    /metrics: synthesised=${NEW_SYNTH} already=${NEW_ALREADY} decode_err=${NEW_DECODE} unrecov=${NEW_UNRECOV}"
log "  after    synthesised log lines: ${LOG_NEW} (was ${LOG_OFFSET})"

DELTA_LOG=$((LOG_NEW - LOG_OFFSET))
DELTA_DECODE=$((NEW_DECODE - BASE_DECODE))
DELTA_UNRECOV=$((NEW_UNRECOV - BASE_UNRECOV))

# ── Assertions ────────────────────────────────────────────────────────────────
# Authoritative pass-condition: a NEW synthesised-ClaimEvent log line emitted
# AND the DB row was rewritten (we deleted everything pre-test, so any present
# row post-test is fresh). /metrics counter delta is informational-only because
# of the known counter bug above.
if [[ "$DELTA_LOG" -lt 1 ]]; then
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -iE 'restore::claims|project_claim|fail-closed' | tail -20 | sed 's/^/    | /'
    fail "the projector re-reached block ${CLAIM_BLOCK} (cursor ${CURSOR_NOW}) but did NOT log a new \
synthesis (Δlog=${DELTA_LOG}). Re-projection ran, so this is not a timing problem: either the consumed \
CLAIM note is no longer in miden-client's sqlite for the projector to decode, or project_claim_note \
skipped/fail-closed on it. The proxy lines above say which."
fi

if [[ "$DELTA_DECODE" -gt 0 ]]; then
    fail "watcher hit ${DELTA_DECODE} decode error(s) — investigate ClaimNoteStorage layout"
fi
if [[ "$DELTA_UNRECOV" -gt 0 ]]; then
    fail "watcher reported ${DELTA_UNRECOV} unrecoverable CLAIM(s) — investigate"
fi

# Confirm the ClaimEvent is recoverable again via the same predicate the
# watcher uses to dedup. Either watcher-emitted row OR synthetic_logs match.
HAS_RECOVERED=$(pgq "SELECT EXISTS(SELECT 1 FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex')) OR EXISTS(SELECT 1 FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${GI_HEX}%');")
[[ "$HAS_RECOVERED" != "t" ]] && fail "synthesis log fired but ClaimEvent still not recoverable (got '$HAS_RECOVERED') — atomic commit may be broken"

# The load-bearing claim: re-projection reproduced the SAME event, not merely
# "an" event. Every field a consumer reads — block identity, tx hash, address,
# topics, tx index, removal flag, data — must come back byte-identical.
CLAIM_ROW_AFTER=$(pgq "SELECT block_number || '|' || transaction_hash || '|' || encode(block_hash,'hex') \
    || '|' || address || '|' || array_to_string(topics, ',') || '|' || transaction_index \
    || '|' || removed || '|' || data \
    FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND data LIKE '0x${GI_HEX}%' \
    ORDER BY block_number DESC LIMIT 1;")
if [[ "$CLAIM_ROW_AFTER" != "$CLAIM_ROW_BEFORE" ]]; then
    echo "  before: $CLAIM_ROW_BEFORE"
    echo "  after : $CLAIM_ROW_AFTER"
    fail "the re-synthesised ClaimEvent is NOT byte-identical to the one that was deleted \
(fields in order: block|tx_hash|block_hash|address|topics|tx_index|removed|data) — a consumer \
replaying eth_getLogs across the recovery would see a DIFFERENT event"
fi
log "  re-synthesised ClaimEvent is byte-identical to the deleted row ✓"

# Sanity: at least one fresh row in claim_watcher_processed for this gi.
FRESH_WATCHER_ROW=$(pgq "SELECT COUNT(*) FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex');")
[[ "$FRESH_WATCHER_ROW" -lt 1 ]] && fail "no fresh claim_watcher_processed row after synthesis (got $FRESH_WATCHER_ROW)"

# Note the metrics-bug warning so reviewers don't chase a phantom regression.
DELTA_SYNTH=$((NEW_SYNTH - BASE_SYNTH))
if [[ "$DELTA_LOG" -ge 1 && "$DELTA_SYNTH" -lt 1 ]]; then
    warn "claim_watcher_synthesised_total /metrics counter did NOT increment (Δ=$DELTA_SYNTH) despite $DELTA_LOG new synthesis log line(s). This is a known counter-handle bug in src/claim_watcher.rs:346 (filed as follow-up). DB + log assertions above are authoritative."
fi

log "════════════════════════════════════════════════════════════════════"
log "  claim_watcher SYNTHESIS-PATH PASS"
log "    Δsynthesised_log     = ${DELTA_LOG}"
log "    Δsynthesised_metric  = ${DELTA_SYNTH}  (known broken — see warning)"
log "    Δdecode_errors       = ${DELTA_DECODE}"
log "    Δunrecoverable       = ${DELTA_UNRECOV}"
log "    fresh watcher rows   = ${FRESH_WATCHER_ROW}"
log "    ClaimEvent for 0x${GI_HEX:0:16}... recovered via watcher"
log "════════════════════════════════════════════════════════════════════"
