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
# THE HEAL: stop the bridge-service, delete the stranded status='created'
# monitored txs (they are unsendable; the DEPOSITS remain and claimtxman
# re-detects + re-creates them with a chain-derived nonce once its table no
# longer poisons allocation), start the service, and require the positive
# outcome: the L1 sync cursor must ADVANCE within the confirm window.
# 'confirmed' rows are history and are kept.
#
# Usage: PROJECT=<compose-project> ./scripts/bridge-claimtxman-heal.sh
#   FORCE=1        heal without the wedge precheck (drill epilogue uses this:
#                  post-restore the stranded-nonce state is deterministic)
#   PG_CONTAINER   override postgres container (default $PROJECT-postgres-1)
# Returns 0 on heal (or clean no-op with FORCE=0), 1 on error.
set -uo pipefail

PROJECT="${PROJECT:-${COMPOSE_PROJECT_NAME:-miden-agglayer}}"
SVC_C="$PROJECT-bridge-service-1"
PG_C="${PG_CONTAINER:-$PROJECT-postgres-1}"
FORCE="${FORCE:-0}"
L1_RPC="${L1_RPC:-http://localhost:8545}"

log() { echo "[$(date '+%H:%M:%S')] claimtxman-heal: $*"; }
psql_b() { docker exec "$PG_C" psql -U bridge_user -d bridge_db -tAX -c "$1" 2>/dev/null; }

docker inspect "$SVC_C" >/dev/null 2>&1 || { log "container $SVC_C not found"; exit 1; }

stranded=$(psql_b "SELECT count(*) FROM sync.monitored_txs WHERE status='created'" | tr -d '[:space:]')
if [[ "$FORCE" != "1" ]]; then
    # Wedge signature: stranded created rows AND fresh nonce-mismatch sends.
    mismatches=$(docker logs --since 120s "$SVC_C" 2>&1 | grep -c "nonce mismatch for" || true)
    if [[ "${stranded:-0}" -eq 0 || "${mismatches:-0}" -eq 0 ]]; then
        log "no stranded-nonce wedge (created=${stranded:-0} fresh_mismatches=${mismatches:-0}) — nothing to heal"
        exit 0
    fi
    log "wedge: $stranded stranded created tx(s), $mismatches nonce-mismatch send(s)/120s"
else
    log "FORCE=1 (post-restore): clearing ${stranded:-0} created monitored tx(s) unconditionally"
fi

l1_cursor_before=$(psql_b "SELECT coalesce(max(block_num),0) FROM sync.block WHERE network_id=0" | tr -d '[:space:]')

docker stop "$SVC_C" >/dev/null 2>&1
[[ "$(docker inspect -f '{{.State.Status}}' "$SVC_C" 2>/dev/null)" == "exited" ]] \
    || { log "FATAL: $SVC_C did not stop"; exit 1; }
deleted=$(psql_b "DELETE FROM sync.monitored_txs WHERE status='created' RETURNING 1" | grep -c 1 || true)
docker start "$SVC_C" >/dev/null 2>&1 || { log "FATAL: $SVC_C did not start"; exit 1; }
log "deleted ${deleted:-0} stranded row(s); service restarted"

# Positive outcome: the previously starved L1 syncer must ADVANCE. (The
# sync.status 'synced' flag is the starved component's own stale self-report —
# see the cycle-1/2 false PASS — so require cursor movement, not the flag.)
deadline=$(( $(date +%s) + ${HEAL_CONFIRM_TIMEOUT:-180} ))
while :; do
    cur=$(psql_b "SELECT coalesce(max(block_num),0) FROM sync.block WHERE network_id=0" | tr -d '[:space:]')
    if [[ "${cur:-0}" -gt "${l1_cursor_before:-0}" ]]; then
        log "positive outcome: L1 sync cursor advanced ${l1_cursor_before} -> ${cur}"
        exit 0
    fi
    [[ $(date +%s) -ge $deadline ]] && break
    sleep 5
done
log "UNCONFIRMED: L1 sync cursor did not advance within ${HEAL_CONFIRM_TIMEOUT:-180}s (service left RUNNING)"
exit 1
