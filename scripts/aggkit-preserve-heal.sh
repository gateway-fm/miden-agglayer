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

# ── the heal: preserve every DB except the poisoned monitor DB ───────────────
B="$(mktemp -d)"; trap 'rm -rf "$B"' EXIT
docker stop "$C" >/dev/null 2>&1

# ── STAGE, and verify the CRITICAL set is staged BEFORE anything destructive ──
# PR#164 re-review: previously every `docker cp` failure was ignored, `saved`
# could be 0, and the script force-recreated the ONLY copy of the state anyway —
# destroying exactly what it claims to preserve, then reporting success. The
# critical DBs are the ones whose loss is unrecoverable: aggsender's cert lineage
# (#89) and the bridgesync cursors (#87, anvil's 256-state wall). Those MUST be
# staged or we abort while the container is still intact and restartable.
CRITICAL=(aggsender bridgel2sync)
OPTIONAL=(L1InfoTreeSync reorgdetectorl1 reorgdetectorl2)
staged=()
stage_one() { # $1 = basename; echoes nothing, returns 0 if the main .sqlite staged
    local f="$1" got_main=1 ext
    for ext in sqlite sqlite-wal sqlite-shm; do
        if docker cp "$C:/tmp/$f.$ext" "$B/" >/dev/null 2>&1; then
            staged+=("$f.$ext")
            [ "$ext" = sqlite ] && got_main=0
        fi
    done
    return $got_main
}
for f in "${CRITICAL[@]}"; do
    stage_one "$f" || {
        log "FATAL: could not stage critical DB /tmp/$f.sqlite from $C."
        log "       Refusing to force-recreate: that would destroy the only copy of"
        log "       $f state (cert lineage #89 / bridgesync cursors #87). Container left"
        log "       stopped-but-intact; restart it with: docker start $C"
        exit 1
    }
done
for f in "${OPTIONAL[@]}"; do stage_one "$f" || true; done
staged_count=${#staged[@]}
[ "$staged_count" -gt 0 ] || { log "FATAL: staged 0 files — refusing to recreate"; exit 1; }
log "staged $staged_count file(s): ${staged[*]}"

COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    create --force-recreate --no-deps "$SVC" >/dev/null 2>&1 \
    || { log "recreate failed"; exit 1; }

# ── RESTORE, and require EVERY staged file back before starting ──────────────
# A partial restore is worse than no heal: the container would come up with a
# half-populated state directory and diverge silently.
restored=0; missing=()
for name in "${staged[@]}"; do
    if docker cp "$B/$name" "$C:/tmp/$name" >/dev/null 2>&1; then
        restored=$((restored+1))
    else
        missing+=("$name")
    fi
done
if [ "$restored" -ne "$staged_count" ]; then
    log "FATAL: restored $restored/$staged_count staged file(s); missing: ${missing[*]}"
    log "       NOT starting $SVC — a half-restored state directory would diverge silently."
    log "       Staged copies are in $B (this dir is removed on exit; copy them NOW if needed)."
    trap - EXIT
    log "       Preserved staging dir: $B"
    exit 1
fi
# Ownership must be usable by the container's runtime user, or the service starts
# and then fails to open its own DBs — a "successful" heal that is not one.
docker cp "$B/." "$C:/tmp/" >/dev/null 2>&1 || true

COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    start "$SVC" >/dev/null 2>&1 \
    || { log "start failed"; exit 1; }
log "preserve-healed (staged=$staged_count restored=$restored, ethtxmanager-aggoracle.sqlite wiped)"
exit 0
