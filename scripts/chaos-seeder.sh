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
# CRITICAL: a plain `docker network connect` does NOT restore the compose service
# alias (e.g. 'miden-node') — only the container-name alias — so after a
# partition fault nothing could resolve the node and the whole stack wedges
# permanently. Capture the node's non-container-name aliases on this net and
# ALWAYS reconnect with them. (Default to the compose service name 'miden-node'.)
_node_aliases() {
    docker inspect "$NODE" --format \
      "{{range \$k,\$v := .NetworkSettings.Networks}}{{if eq \$k \"$NET\"}}{{range \$v.Aliases}}{{.}} {{end}}{{end}}{{end}}" 2>/dev/null \
      | tr ' ' '\n' | grep -vx "$NODE" | grep -v '^$'
}
NODE_ALIASES="$(_node_aliases)"
[ -z "$NODE_ALIASES" ] && NODE_ALIASES="miden-node"
_reconnect_node() {
    local a args=()
    for a in $NODE_ALIASES; do args+=(--alias "$a"); done
    docker network connect "${args[@]}" "$NET" "$NODE" >/dev/null 2>&1
}

log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$CHAOS_LOG"; }

# ── restore-everything safety net (runs on ANY exit) ────────────────────────
restore_all() {
    docker unpause "$PG" >/dev/null 2>&1 || true
    [ -n "${NET:-}" ] && _reconnect_node || true
    # ensure the faultable services are running
    for c in "$PROVER" "$PROXY"; do
        docker start "$c" >/dev/null 2>&1 || true
    done
    log "restore_all: unpaused pg, reconnected node, ensured prover+proxy up"
}
trap restore_all EXIT

# ── individual faults (each self-bounded + self-restoring) ──────────────────
# The soak counts INJECTED faults via `grep -c "FAULT "`. This script runs WITHOUT
# `set -e` and docker exit codes are otherwise unchecked, so the "FAULT " (counted)
# marker is emitted ONLY AFTER the docker operation that actually injects the fault
# SUCCEEDS. A failed docker pause/restart/disconnect logs "SKIP" (not counted), so a
# fault that never took effect can't satisfy the soak's CHAOS_OK "chaos actually fired"
# gate. Recovery ops (unpause/reconnect) are best-effort and don't gate the count.
fault_pause_pg() {
    local dur=$(( 4 + RANDOM % 8 ))   # 4-11s stall
    if docker pause "$PG" >/dev/null 2>&1; then
        log "FAULT pause-pg ($dur s) — proxy store unavailable (injected)"
        sleep "$dur"
        docker unpause "$PG" >/dev/null 2>&1 || log "  WARN: pg unpause failed"
        log "  -> pg unpaused"
    else
        log "SKIP pause-pg (docker pause $PG failed — not injected)"
    fi
}
fault_kill_prover() {
    if docker restart -t 2 "$PROVER" >/dev/null 2>&1; then
        log "FAULT kill-prover — restart tx-prover mid-proof (injected)"
    else
        log "SKIP kill-prover (docker restart $PROVER failed — not injected)"
    fi
}
fault_restart_proxy() {
    if docker restart -t 2 "$PROXY" >/dev/null 2>&1; then
        log "FAULT restart-proxy — crash the proxy mid-flight (injected; tests cursor persist + late-sweep heal)"
    else
        log "SKIP restart-proxy (docker restart $PROXY failed — not injected)"
    fi
}
fault_partition_node() {
    local dur=$(( 5 + RANDOM % 10 ))  # 5-14s partition
    [ -z "${NET:-}" ] && { log "SKIP partition-node (no network found — not injected)"; return; }
    if docker network disconnect "$NET" "$NODE" >/dev/null 2>&1; then
        log "FAULT partition-node ($dur s) — cut proxy<->node link (injected)"
        sleep "$dur"
        _reconnect_node    # restore WITH the compose alias(es) — else name resolution stays broken
        log "  -> node reconnected (aliases: $NODE_ALIASES)"
    else
        log "SKIP partition-node (docker network disconnect failed — not injected)"
    fi
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
