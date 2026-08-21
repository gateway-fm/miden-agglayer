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
PSQL_ERR=""
psql_b() {
    local out rc
    out=$(docker exec "$PG_C" psql -U bridge_user -d bridge_db -v ON_ERROR_STOP=1 -tAX -c "$1" 2>&1)
    rc=$?
    if [[ $rc -ne 0 ]]; then
        PSQL_ERR="$out"
        return $rc
    fi
    PSQL_ERR=""
    printf '%s' "$out"
}
# Numeric-or-die: every count this script branches on must be a number that
# actually came back from the database.
#
# The exit status is the contract, NOT an `exit` inside the function: callers
# use `x=$(psql_num ...)`, which runs the function in a SUBSHELL, so an `exit`
# there kills only the substitution and the script sails on with an empty
# value — the exact fail-open shape this helper exists to prevent (and the
# script has no `set -e` to catch it). Every call site must therefore be
# written `x=$(psql_num ...) || exit 1`, which `psql_die` enforces below.
psql_num() {
    local v
    v=$(psql_b "$1" | tr -d '[:space:]') || {
        log "FATAL: query failed: $1"
        log "       psql said: ${PSQL_ERR:-<no output>}"
        return 1
    }
    [[ "$v" =~ ^[0-9]+$ ]] || {
        log "FATAL: query did not return a number (got '${v}'): $1"
        return 1
    }
    printf '%s' "$v"
}

# Assign-or-abort. Usage: psql_die VAR "SELECT ..."
# Runs the query in THIS shell's context for the abort decision, so a failure
# really does stop the script.
psql_die() {
    local __var="$1" __sql="$2" __val
    __val=$(psql_num "$__sql") || {
        log "       aborting: this script deletes state and must never proceed on unreadable data"
        exit 1
    }
    printf -v "$__var" '%s' "$__val"
}

# STRING MATCHING — deliberate, and the only option here. The subject is
# zkevm-bridge-service, a separate Go process: it exposes no typed error, no
# metric and no table column for "this send was rejected on nonce", so its log
# is the only channel. The patterns are quoted from its own classifier,
# `isNonceError` in claimtxman (matches "nonce too low" / "invalid nonce" /
# "txnonce"), plus the "nonce mismatch for" line its sender emits; they are the
# strings that component both produces and reacts to. If it ever grows a
# structured signal, switch to it.
# Prints the count, or FAILS (non-zero) if the log could not be read at all.
# `docker logs | grep -c || true` turned an unreadable log into the string "0",
# which the quiet-state check reads as "no nonce errors" — a dead container and
# a healthy one produce the identical answer.
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
        log "no stranded-nonce wedge (created=${stranded:-0} fresh_mismatches=${mismatches:-0}) — nothing to heal"
        exit 0
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
