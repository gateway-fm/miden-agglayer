#!/usr/bin/env bash
# aggkit-preserve-heal.sh — versioned, self-contained heal for the aggkit
# "lost-in-transit GER-inject tx" wedge (PR#164 blocker #8; findings #70/#89).
#
# THE WEDGE: a proxy restart can lose an aggoracle GER-inject tx in transit —
# aggkit's ephemeral monitoring DB marks it sent, the proxy never durably
# admitted it, and aggkit's deterministic tx-ID dedup blocks any re-send
# forever. GER injection freezes.
#
# WHY NOT A BLANKID RECREATE: a plain `force-recreate` wipes the WHOLE container
# fs — including aggsender's cert lineage (unrecoverable after an L2 history
# shift, #89) and the bridgesync cursors (cold resync into anvil's 256-state
# wall permanently halts L2<->L2, #87). The ONLY poisoned file is
# /tmp/ethtxmanager-aggoracle.sqlite.
#
# THIS HEAL: stop -> copy every /tmp/*.sqlite* OUT except ethtxmanager* ->
# `compose create --force-recreate` (NOT started) -> copy the preserved DBs back
# -> start. The monitor DB is cleared; cert lineage + sync cursors survive.
#
# Usage: PROJECT=<compose-project> ./scripts/aggkit-preserve-heal.sh <aggkit|aggkit-l2b>
# Returns 0 on heal, 2 if there is no wedge to heal (no-op), 1 on error.
set -uo pipefail

SVC="${1:?usage: aggkit-preserve-heal.sh <aggkit|aggkit-l2b>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="${PROJECT:-${COMPOSE_PROJECT_NAME:-miden-agglayer}}"
ENV_FILE="${ENV_FILE:-$PROJECT_DIR/fixtures/.env}"
C="$PROJECT-$SVC-1"
PG="$PROJECT-agglayer-postgres-1"
REPEATS_MIN="${REPEATS_MIN:-5}"       # same-hash repeats/60s to call it wedged
FORCE="${FORCE:-0}"                    # 1 = heal without the wedge precheck

COMPOSE=(-f "$PROJECT_DIR/docker-compose.e2e.yml")
[[ -f "$PROJECT_DIR/docker-compose.l2l2.yml" ]] && COMPOSE+=(-f "$PROJECT_DIR/docker-compose.l2l2.yml")
[[ -f "$PROJECT_DIR/docker-compose.web3signer.yml" ]] \
    && docker ps --format '{{.Names}}' | grep -q "^$PROJECT-web3signer-1$" \
    && { COMPOSE+=(-f "$PROJECT_DIR/docker-compose.web3signer.yml")
         [[ -f "$PROJECT_DIR/fixtures/web3signer-keys.env" ]] && { set -a; . "$PROJECT_DIR/fixtures/web3signer-keys.env"; set +a; } }

log() { echo "[$(date '+%H:%M:%S')] preserve-heal($SVC): $*"; }

docker inspect "$C" >/dev/null 2>&1 || { log "container $C not found"; exit 1; }

# ── wedge detection: SAME inject-tx ID repeating AND unknown to the proxy ─────
if [[ "$FORCE" != "1" ]]; then
    top="$(docker logs "$C" --since 60s 2>&1 \
        | grep 'already exists in monitoring DB' \
        | grep -oE 'ID: 0x[a-f0-9]{64}' | awk '{print $2}' \
        | sort | uniq -c | sort -rn | head -1)"
    cnt="$(awk '{print $1}' <<<"$top")"; tx="$(awk '{print $2}' <<<"$top")"
    if [[ -z "${cnt:-}" || "${cnt:-0}" -lt "$REPEATS_MIN" || -z "${tx:-}" ]]; then
        log "no lost-tx wedge signature (repeats=${cnt:-0} < $REPEATS_MIN) — nothing to heal"
        exit 2
    fi
    known="$(docker exec "$PG" psql -U agglayer -d agglayer_store -tAc \
        "SELECT count(*) FROM transactions WHERE tx_hash='$tx'" 2>/dev/null)"
    if [[ "${known:-1}" != "0" ]]; then
        log "tx ${tx:0:18}… is known to the proxy — recovery's job, not a lost-tx wedge; no heal"
        exit 2
    fi
    log "lost-in-transit wedge: tx ${tx:0:18}… x$cnt/60s, unknown to proxy"
fi

# ── the heal: preserve EVERYTHING except the poisoned monitor DB ─────────────
# Review 0814 (blocking): a hard-coded basename list silently lost every state
# file it did not know about — AggKit v0.8.3-rc1 also keeps bridgel1sync.sqlite,
# l2gersync.sqlite and certificates/ in the state dir (config/default.go).
# Stage the WHOLE directory as a manifest, delete only the exact poisoned DB
# from the stage, and retain the staging dir on EVERY post-destructive error.
B="$(mktemp -d)"
KEEP_STAGE=0
cleanup() {
    if [ "$KEEP_STAGE" -eq 1 ]; then
        log "Preserved staging dir (contains the ONLY copy of the aggkit state): $B"
    else
        rm -rf "$B"
    fi
}
trap cleanup EXIT
docker stop "$C" >/dev/null 2>&1

POISON=ethtxmanager-aggoracle.sqlite
docker cp "$C:/tmp/." "$B/stage" >/dev/null 2>&1 || {
    log "FATAL: could not stage the aggkit state dir from $C."
    log "       Refusing to force-recreate: that would destroy the only copy of the"
    log "       cert lineage (#89) / bridgesync cursors (#87). Container left"
    log "       stopped-but-intact; restart it with: docker start $C"
    exit 1
}
rm -f "$B/stage/$POISON" "$B/stage/$POISON-wal" "$B/stage/$POISON-shm"
# The critical, unrecoverable-if-lost members must actually be in the manifest.
for f in aggsender.sqlite bridgel2sync.sqlite; do
    [ -f "$B/stage/$f" ] || {
        log "FATAL: staged manifest lacks critical $f — refusing to recreate."
        log "       Container left stopped-but-intact; restart it with: docker start $C"
        exit 1
    }
done
manifest_count=$(find "$B/stage" -type f | wc -l)
log "staged manifest: $manifest_count file(s) (whole state dir minus $POISON)"

COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    create --force-recreate --no-deps "$SVC" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: recreate failed — staging dir retained"; exit 1; }

# ── RESTORE the manifest, VERIFY it round-trips, then require health ─────────
# From here on the original state exists only in $B — every failure path keeps it.
docker cp "$B/stage/." "$C:/tmp/" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: restore copy into the recreated container failed — staging dir retained"; exit 1; }
# Verify the restore round-trips: read the state dir back and require every
# staged file to be present (the recreated container may lack a shell —
# distroless — so verification goes through docker cp, not exec).
docker cp "$C:/tmp/." "$B/readback" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: cannot read back the restored state dir — staging dir retained"; exit 1; }
unrestored=$(cd "$B/stage" && find . -type f | while read -r f; do
    [ -f "$B/readback/$f" ] || echo "$f"
done)
if [ -n "$unrestored" ]; then
    KEEP_STAGE=1
    log "FATAL: restored state is INCOMPLETE — missing: $(echo "$unrestored" | tr '\n' ' ')"
    log "       NOT starting $SVC — a half-restored state directory would diverge silently."
    exit 1
fi

COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    start "$SVC" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: start failed — staging dir retained"; exit 1; }

# Health gate: a heal that starts a crash-looping service is not a heal. The
# container must still be RUNNING (not restarted) after a settle window, and
# must be producing fresh log output (proof the process is alive and working,
# usable even on distroless images with no shell).
HEAL_T0=$(date +%s)
sleep 25
STATE=$(docker inspect -f '{{.State.Status}} {{.State.Restarting}} {{.RestartCount}}' "$C" 2>/dev/null || echo "gone")
RECENT_LOGS=$(docker logs --since "$((25))s" "$C" 2>&1 | head -5 | wc -l)
case "$STATE" in
    "running false"*)
        if [ "$RECENT_LOGS" -eq 0 ]; then
            KEEP_STAGE=1
            log "FATAL: $SVC is running but produced no log output in the settle window —"
            log "       cannot confirm the process is healthy; staging dir retained."
            exit 1
        fi
        ;;
    *)
        KEEP_STAGE=1
        log "FATAL: $SVC is not stably running after the heal (state: $STATE) — staging dir retained"
        exit 1
        ;;
esac
log "preserve-healed (manifest=$manifest_count files restored+verified, $POISON wiped, health confirmed after $(( $(date +%s) - HEAL_T0 ))s)"
exit 0
