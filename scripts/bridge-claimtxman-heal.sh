#!/usr/bin/env bash
# bridge-claimtxman-heal.sh — heal the bridge-service ClaimTxManager after a
# proxy restore (FINDING #111, 2026-08-18 loop cycles 1-3).
#
# THE WEDGE: a full-DB-loss restore rebuilds the proxy chain from canonical
# history, which COMPACTS the ClaimTxManager sponsor's tx count (live ~177 →
# restored 66 observed). The claimtxman allocates nonces from its own
# sync.monitored_txs history, not the chain, so every post-restore claim it
# creates carries a pre-restore-scale nonce → R4 "nonce mismatch" on every
# send, forever. The retry storm (60s pool-wait per tx per round) starves the
# SAME synchronizer loop that indexes L1 GERs, so the L1-GER table falls
# behind unboundedly and /merkle-proof starts returning code=2 "l1GER not
# found" for EVERY net-1 deposit — all Miden->L2B claims become impossible
# while ready_for_claim still reads true.
#
# THE HEAL: stop the bridge-service, wipe the monitored-tx tables (fully
# re-derivable: the DEPOSITS remain and claimtxman re-detects unclaimed ready
# deposits and re-creates claims; checkIfClaimed re-confirms landed ones; the
# RESTART also clears the in-memory NonceCache LRU whose ratchet is the actual
# allocator poison — see nonce_cache.go GetNextNonce), start the service, and require the positive
# outcome: the L1 sync cursor must ADVANCE within the confirm window.
#
# Usage: PROJECT=<compose-project> ./scripts/bridge-claimtxman-heal.sh
#   FORCE=1        heal without the wedge precheck (drill epilogue uses this:
#                  post-restore the stranded-nonce state is deterministic)
#   PG_CONTAINER   override postgres container (default $PROJECT-postgres-1)
# Exit codes:
#   0  healed, and the L1 sync cursor advanced (positive proof it recovered)
#   1  error, or the service is not stably running afterwards
#   2  no action taken (no wedge to heal) — NOT a statement about health
#   3  wipe completed and the service is running, but NO local proof was
#      available (a quiet stack moves no cursor). The CALLER must prove the
#      claim pipeline. `set -e` callers must use
#      `if ...; then rc=0; else rc=$?; fi` — a bare call exits before $? is read.
set -uo pipefail

PROJECT="${PROJECT:-${COMPOSE_PROJECT_NAME:-miden-agglayer}}"
SVC_C="$PROJECT-bridge-service-1"
PG_C="${PG_CONTAINER:-$PROJECT-postgres-1}"
FORCE="${FORCE:-0}"
L1_RPC="${L1_RPC:-http://localhost:8545}"

log() { echo "[$(date '+%H:%M:%S')] claimtxman-heal: $*"; }

# FAIL-CLOSED SQL. The old helper sent stderr to /dev/null with no
# ON_ERROR_STOP, so a broken query, a wrong database or an unreachable
# container all returned the empty string — which `${x:-0}` then turned into a
# legitimate-looking zero. That made the wedge precheck read "0 stranded rows,
# nothing to heal" and exit 0 on an error, and made the post-heal checks pass
# on unreadable state. Now: errors abort the statement, stderr is captured, and
# a failed query is a failed query.
# FAIL-CLOSED SQL, with diagnostics that survive.
#
# Two earlier attempts lost the evidence on a path that DELETES state: the
# error text was set inside a `$( )`/pipeline SUBSHELL and discarded, and a
# temp-file hop then swallowed the log lines and returned success on an
# unchecked read. The fix is to stop routing diagnostics through stdout at all:
# VALUES go to stdout (so `v=$(psql_num ...)` captures them), DIAGNOSTICS go to
# stderr (so nothing can capture them by accident and they always reach the
# operator).
logerr() { echo "[$(date '+%H:%M:%S')] claimtxman-heal: $*" >&2; }

# Prints the query result on stdout; on failure prints psql's own error on
# stderr and returns non-zero.
psql_b() {
    local out rc=0
    out=$(docker exec "$PG_C" psql -U bridge_user -d bridge_db -v ON_ERROR_STOP=1 -tAX -c "$1" 2>&1) || rc=$?
    if [[ $rc -ne 0 ]]; then
        logerr "FATAL: query failed (rc=$rc): $1"
        logerr "       psql said: ${out//$'\n'/ }"
        return $rc
    fi
    printf '%s' "$out"
}

# Numeric-or-die. Value on stdout, diagnostics on stderr, non-zero on failure.
psql_num() {
    local raw v rc=0
    raw=$(psql_b "$1") || return $?
    v=$(printf '%s' "$raw" | tr -d '[:space:]')
    if [[ ! "$v" =~ ^[0-9]+$ ]]; then
        logerr "FATAL: query did not return a number (got '${v}'): $1"
        return 64
    fi
    printf '%s' "$v"
    return $rc
}

# Assign-or-abort: `psql_die VAR "SELECT ..."`.
#
# `x=$(psql_num ...)` alone is not enough — call sites must abort the REAL
# shell, and this script has no `set -e`, so an `exit` inside the function
# would only kill the substitution. The value is captured here and the status
# checked explicitly; diagnostics already went to stderr, so nothing is hidden
# by the substitution.
psql_die() {
    local __var="$1" __sql="$2" __val __rc=0
    __val=$(psql_num "$__sql") || __rc=$?
    if [[ $__rc -ne 0 ]]; then
        logerr "       aborting: this script deletes state and must never proceed on unreadable data"
        exit 1
    fi
    if [[ ! "$__val" =~ ^[0-9]+$ ]]; then
        logerr "FATAL: internal: psql_num returned success with a non-numeric value '${__val}'"
        exit 1
    fi
    printf -v "$__var" '%s' "$__val"
}

# STRING MATCHING — deliberate, documented, and the only channel available.
# The subject is zkevm-bridge-service, a separate Go process: it exposes no
# typed error, no metric and no table column for "this send was rejected on
# nonce", so its log is the only signal. The patterns are quoted from that
# component's OWN classifier (`isNonceError` in claimtxman: "nonce too low" /
# "invalid nonce" / "txnonce") plus the "nonce mismatch for" line its sender
# emits — the strings it both produces and reacts to. Used ONLY to decide
# whether a wedge exists in the non-FORCE precheck; it is never used to certify
# a heal as successful (that shape was removed). Restore a typed signal here if
# upstream ever grows one.
#
# Prints the count on stdout; FAILS (non-zero) if the log could not be read at
# all, because "0 errors" and "cannot read the log" are the same string
# otherwise — and a dead container produces both.
nonce_error_lines() {
    local out
    out=$(docker logs --since "${1}s" "$SVC_C" 2>&1) || return 1
    printf '%s' "$out" \
        | grep -cE "nonce too low|nonce too high|invalid nonce|txnonce|nonce mismatch for" \
        || true
}

docker inspect "$SVC_C" >/dev/null 2>&1 || { log "container $SVC_C not found"; exit 1; }
docker inspect "$PG_C" >/dev/null 2>&1 || { log "postgres container $PG_C not found"; exit 1; }

# This script STOPS a service and DELETES rows, with both targets coming from
# environment variables. Verify each one actually belongs to the compose
# project we were told to heal before touching it — a stale/typo'd PROJECT or
# PG_CONTAINER would otherwise stop one stack's bridge-service while wiping a
# different stack's tables.
for c in "$SVC_C" "$PG_C"; do
    owner=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$c" 2>/dev/null)
    [[ "$owner" == "$PROJECT" ]] || {
        log "FATAL: refusing to touch $c — it belongs to compose project '${owner:-<none>}', not '$PROJECT'."
        log "       Set PROJECT/PG_CONTAINER to the stack you actually mean to heal."
        exit 1
    }
done

# One heal at a time per project. The chaos watchdog, the recovery drill and a
# manual run can all decide to heal at once; concurrent runs would delete rows
# the other just let the service recreate, or restart it mid-wipe.
LOCK="/tmp/.claimtxman-heal.$PROJECT.lock"
exec 9>"$LOCK"
flock -n 9 || { log "another claimtxman heal is already running for project $PROJECT (lock: $LOCK) — refusing to run concurrently"; exit 1; }

psql_die stranded "SELECT count(*) FROM sync.monitored_txs WHERE status='created'"
if [[ "$FORCE" != "1" ]]; then
    # Wedge signature: stranded created rows AND fresh nonce-mismatch sends.
    if ! mismatches=$(nonce_error_lines 120); then
        log "FATAL: cannot read $SVC_C logs for the wedge precheck"
        exit 1
    fi
    if [[ "${stranded:-0}" -eq 0 || "${mismatches:-0}" -eq 0 ]]; then
        # Exit 2 = NO ACTION, not 0. Exit 0 is documented as "healed, with
        # proven L1 progress"; returning it here let a STOPPED container with
        # readable old logs and zero matching rows read as a successful heal —
        # the same dead-container false success the post-wipe rewrite removed.
        log "no stranded-nonce wedge (created=${stranded:-0} fresh_mismatches=${mismatches:-0}) — nothing to heal (NO ACTION)"
        exit 2
    fi
    log "wedge: $stranded stranded created tx(s), $mismatches nonce-mismatch send(s)/120s"
else
    log "FORCE=1 (post-restore): clearing ${stranded:-0} created monitored tx(s) unconditionally"
fi

psql_die l1_cursor_before "SELECT coalesce(max(block_num),0) FROM sync.block WHERE network_id=0"

docker stop "$SVC_C" >/dev/null 2>&1
[[ "$(docker inspect -f '{{.State.Status}}' "$SVC_C" 2>/dev/null)" == "exited" ]] \
    || { log "FATAL: $SVC_C did not stop"; exit 1; }
# Whole-table wipe (maintainer decision 2026-08-18): monitored txs are fully
# re-derivable — sync.deposit + on-chain isClaimed/ClaimEvents are the source
# of truth, and claimtxman's checkIfClaimed re-confirms landed claims. Keeping
# 'confirmed' rows adds nothing and a partial wipe leaves more ways to be
# inconsistent. The group table goes with it.
# ONE transaction, FK-safe order (group rows reference monitored txs), aborting
# on the first error. The previous form ran two independent statements with
# their failures swallowed by `|| true`, so a half-wipe — or no wipe at all —
# restarted the service and reported success.
DEL_OUT=$(docker exec "$PG_C" psql -U bridge_user -d bridge_db -v ON_ERROR_STOP=1 -tAX \
    -c "BEGIN; DELETE FROM sync.monitored_txs_group; DELETE FROM sync.monitored_txs; COMMIT;" 2>&1) || {
    log "FATAL: the monitored-tx wipe FAILED and was rolled back: $DEL_OUT"
    log "       $SVC_C is left STOPPED; start it with: docker start $SVC_C"
    exit 1
}
# Prove the wipe before restarting: restarting on top of surviving rows
# recreates the exact divergence this heal exists to clear.
for t in sync.monitored_txs sync.monitored_txs_group; do
    psql_die left "SELECT count(*) FROM $t"
    [[ "$left" -eq 0 ]] || {
        log "FATAL: $t still has $left row(s) after the wipe — refusing to restart into a partially-cleared state"
        exit 1
    }
done
docker start "$SVC_C" >/dev/null 2>&1 || { log "FATAL: $SVC_C did not start"; exit 1; }
log "cleared ${stranded:-0} stranded created tx(s); both monitored-tx tables verified empty; service restarted"

# Positive outcome, two accepted shapes. sync.status 'synced' is the starved
# component's own stale self-report (cycle-1/2 false PASS) so it is never
# consulted; but sync.block records only EVENT-BEARING blocks, so on a quiet
# stack (e.g. right after a drill — 2026-08-18 08:46 live false-negative) the
# cursor legitimately does not move. Success is therefore EITHER:
#   (a) the cursor advanced (the starved-syncer recovery shape), OR
#   (b) the harmful state is provably absent: every settled L2-observed GER
#       has its L1 row (join orphans == 0, allowing the 2 newest to lag) AND
#       no fresh nonce-error sends — the healthy-quiet shape.
deadline=$(( $(date +%s) + ${HEAL_CONFIRM_TIMEOUT:-180} ))
while :; do
    # Liveness first, and continuously: a cursor that advanced while the
    # container has since exited is not a healed service.
    svc_state=$(docker inspect -f '{{.State.Status}} {{.State.Restarting}}' "$SVC_C" 2>/dev/null || echo "gone")
    if [[ "$svc_state" != "running false" ]]; then
        log "FATAL: $SVC_C is not stably running after the heal (state: $svc_state)"
        exit 1
    fi
    psql_die cur "SELECT coalesce(max(block_num),0) FROM sync.block WHERE network_id=0"
    if [[ "${cur:-0}" -gt "${l1_cursor_before:-0}" ]]; then
        # The ONLY positive proof this healer can produce locally: the L1
        # synchronizer — the component that was starving — made forward
        # progress after the restart.
        log "positive outcome: L1 sync cursor advanced ${l1_cursor_before} -> ${cur} (service stably running)"
        exit 0
    fi
    # The former "quiet state" success shape is GONE. It claimed health from
    # (zero join orphans) + (no fresh nonce errors), and neither is evidence:
    # the orphan query excluded the newest rows by RANK — rank is not age, so a
    # row stuck indefinitely sits in that window forever — and a container that
    # has just restarted has produced no errors yet precisely because it has
    # produced nothing. Sampled right after the restart, a completely dead
    # bridge-service satisfies both.
    [[ $(date +%s) -ge $deadline ]] && break
    sleep 5
done

# No local proof available. Say so and exit with a DISTINCT code, so a caller
# cannot read "healed" from `rc == 0`. sync.block records only event-bearing
# blocks, so on a genuinely quiet stack the cursor legitimately does not move —
# which is exactly why this is UNPROVEN rather than FAILED, and why the caller
# (which can drive a claim and watch it land) must be the one to conclude.
log "UNPROVEN: the monitored-tx wipe completed and $SVC_C is stably running, but no L1 sync progress was observed within ${HEAL_CONFIRM_TIMEOUT:-180}s. On a quiet stack that is expected (sync.block records only event-bearing blocks); the CALLER must prove the claim pipeline."
exit 3
