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
    mismatches=$(docker logs --since 120s "$SVC_C" 2>&1 | grep -cE "nonce too low|nonce too high|nonce mismatch for" || true)
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
# Whole-table wipe (maintainer decision 2026-08-18): monitored txs are fully
# re-derivable — sync.deposit + on-chain isClaimed/ClaimEvents are the source
# of truth, and claimtxman's checkIfClaimed re-confirms landed claims. Keeping
# 'confirmed' rows adds nothing and a partial wipe leaves more ways to be
# inconsistent. The group table goes with it.
deleted=$(psql_b "DELETE FROM sync.monitored_txs RETURNING 1" | grep -c 1 || true)
psql_b "DELETE FROM sync.monitored_txs_group" >/dev/null || true
docker start "$SVC_C" >/dev/null 2>&1 || { log "FATAL: $SVC_C did not start"; exit 1; }
log "deleted ${deleted:-0} stranded row(s); service restarted"

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
    cur=$(psql_b "SELECT coalesce(max(block_num),0) FROM sync.block WHERE network_id=0" | tr -d '[:space:]')
    if [[ "${cur:-0}" -gt "${l1_cursor_before:-0}" ]]; then
        log "positive outcome: L1 sync cursor advanced ${l1_cursor_before} -> ${cur}"
        exit 0
    fi
    orphans=$(psql_b "
        SELECT count(*) FROM sync.exit_root l2
        LEFT JOIN sync.exit_root l1
          ON l1.network_id=0 AND l1.global_exit_root=l2.global_exit_root
        WHERE l2.network_id=1 AND l1.id IS NULL
          AND l2.id <= (SELECT coalesce(max(id),0)-2 FROM sync.exit_root WHERE network_id=1)" \
        | tr -d '[:space:]')
    fresh_err=$(docker logs --since 60s "$SVC_C" 2>&1 \
        | grep -cE "nonce too low|nonce too high|nonce mismatch for" || true)
    if [[ "${orphans:-1}" == "0" && "${fresh_err:-1}" -eq 0 ]]; then
        log "positive outcome: L1-GER join consistent (0 orphans) and no fresh nonce errors (quiet-stack shape; cursor ${l1_cursor_before} unchanged is event-sparse, not starvation)"
        exit 0
    fi
    [[ $(date +%s) -ge $deadline ]] && break
    sleep 5
done
log "UNCONFIRMED: neither cursor advance nor a consistent quiet state within ${HEAL_CONFIRM_TIMEOUT:-180}s (service left RUNNING; orphans=${orphans:-?} fresh_nonce_errs=${fresh_err:-?})"
exit 1
