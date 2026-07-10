#!/usr/bin/env bash
# chaos-seeder.sh — randomized, external fault injection against the PROXY and
# its dependencies while a loadtest drives real traffic. External only (docker
# pause/kill/network) — no in-binary failpoints, so we fault the SHIPPED artifact.
#
# Runs a fault loop for CHAOS_DURATION seconds, one random fault per cycle, and
# ALWAYS restores every fault it applied (bounded per-fault, plus an EXIT trap
# that unpauses/reconnects/restarts anything left in a faulted state). Each fault
# event is logged with a timestamp to CHAOS_LOG. The caller asserts correctness
# (zero lost events) with verify-event-completeness AFTER this exits + settles.
#
# The fault menu targets the proxy's resilience contracts proven this week:
#   pause-pg    -> proxy store stall           (tests store retry / atomicity, H1/H2)
#   kill-prover -> prover death mid-proof       (tests remote-prover retry)
#   restart-proxy -> crash mid-flight           (tests cursor persistence + late-sweep heal, #27)
#   partition-node -> proxy<->node net cut      (tests sync_with_retry, #26)
# Deliberately does NOT fault the node/anvil/aggkit destructively — those are the
# chain-of-record; we certify the PROXY's survival, not the chain's.
#
# Usage: CHAOS_DURATION=600 PROJECT=miden-agglayer ./scripts/chaos-seeder.sh
set -uo pipefail

PROJECT="${PROJECT:-miden-agglayer}"
CHAOS_DURATION="${CHAOS_DURATION:-600}"
CHAOS_MIN_GAP="${CHAOS_MIN_GAP:-25}"    # min seconds between faults
CHAOS_MAX_GAP="${CHAOS_MAX_GAP:-55}"    # max seconds between faults
CHAOS_LOG="${CHAOS_LOG:-/tmp/chaos-events.log}"
SEED="${CHAOS_SEED:-$$}"                # reproducible-ish ordering per run
RANDOM=$SEED

# The Miden proxy's store lives in the agglayer_store DB on the "agglayer-postgres"
# container (published :5434) — NOT a "miden-agglayer-postgres" container (which
# does not exist). Pausing it stalls the proxy's store writes (the intended
# fault). Overridable via PG_CONTAINER for non-standard topologies.
PG="${PG_CONTAINER:-${PROJECT}-agglayer-postgres-1}"
PROVER="${PROJECT}-tx-prover-1"
PROXY="${PROJECT}-miden-agglayer-1"
NODE="${PROJECT}-miden-node-1"
# the docker network the proxy<->node talk over (compose default)
NET="$(docker inspect "$PROXY" --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' 2>/dev/null | awk '{print $1}')"

log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$CHAOS_LOG"; }

# ── restore-everything safety net (runs on ANY exit) ────────────────────────
restore_all() {
    docker unpause "$PG" >/dev/null 2>&1 || true
    [ -n "${NET:-}" ] && docker network connect "$NET" "$NODE" >/dev/null 2>&1 || true
    # ensure the faultable services are running
    for c in "$PROVER" "$PROXY"; do
        docker start "$c" >/dev/null 2>&1 || true
    done
    log "restore_all: unpaused pg, reconnected node, ensured prover+proxy up"
}
trap restore_all EXIT

# ── individual faults (each self-bounded + self-restoring) ──────────────────
fault_pause_pg() {
    local dur=$(( 4 + RANDOM % 8 ))   # 4-11s stall
    log "FAULT pause-pg ($dur s) — proxy store unavailable"
    docker pause "$PG" >/dev/null 2>&1
    sleep "$dur"
    docker unpause "$PG" >/dev/null 2>&1
    log "  -> pg unpaused"
}
fault_kill_prover() {
    log "FAULT kill-prover — restart tx-prover mid-proof"
    docker restart -t 2 "$PROVER" >/dev/null 2>&1
    log "  -> prover restarted"
}
fault_restart_proxy() {
    log "FAULT restart-proxy — crash the proxy mid-flight (tests cursor persist + late-sweep heal)"
    docker restart -t 2 "$PROXY" >/dev/null 2>&1
    log "  -> proxy restarted"
}
fault_partition_node() {
    local dur=$(( 5 + RANDOM % 10 ))  # 5-14s partition
    [ -z "${NET:-}" ] && { log "FAULT partition-node SKIPPED (no network found)"; return; }
    log "FAULT partition-node ($dur s) — cut proxy<->node link"
    docker network disconnect "$NET" "$NODE" >/dev/null 2>&1
    sleep "$dur"
    docker network connect "$NET" "$NODE" >/dev/null 2>&1
    log "  -> node reconnected"
}

FAULTS=(fault_pause_pg fault_kill_prover fault_restart_proxy fault_partition_node)

log "=== chaos-seeder start (project=$PROJECT dur=${CHAOS_DURATION}s net=${NET:-?} seed=$SEED) ==="
log "  targets: pg=$PG prover=$PROVER proxy=$PROXY node=$NODE"
START=$(date +%s)
count=0
while [ $(( $(date +%s) - START )) -lt "$CHAOS_DURATION" ]; do
    gap=$(( CHAOS_MIN_GAP + RANDOM % (CHAOS_MAX_GAP - CHAOS_MIN_GAP + 1) ))
    sleep "$gap"
    [ $(( $(date +%s) - START )) -ge "$CHAOS_DURATION" ] && break
    f="${FAULTS[$(( RANDOM % ${#FAULTS[@]} ))]}"
    "$f"
    count=$(( count + 1 ))
done
log "=== chaos-seeder done: $count faults injected over ${CHAOS_DURATION}s ==="
# EXIT trap restores everything
