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
    # The standalone path knows the exact wedged tx — make it the default for
    # the exact-outcome health probe below (review 0814e).
    WEDGE_TX="${WEDGE_TX:-$tx}"
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
# Review 0814: the stop must be CONFIRMED before snapshotting — copying live
# SQLite (mid-write WAL) and then destroying the source ships a corrupt-only
# copy of the state.
docker stop "$C" >/dev/null 2>&1 || true
STOP_STATE=$(docker inspect -f '{{.State.Status}}' "$C" 2>/dev/null || echo gone)
[ "$STOP_STATE" = "exited" ] || {
    log "FATAL: $C did not stop (state: $STOP_STATE) — refusing to snapshot a live DB."
    exit 1
}

POISON=ethtxmanager-aggoracle.sqlite
# --archive: preserve UID/GID — the pinned aggkit image runs as a non-root
# user, and a root-owned restore would leave its own DBs unwritable.
docker cp --archive "$C:/tmp/." "$B/stage" >/dev/null 2>&1 || {
    log "FATAL: could not stage the aggkit state dir from $C."
    log "       Refusing to force-recreate: that would destroy the only copy of the"
    log "       cert lineage (#89) / bridgesync cursors (#87). Container left"
    log "       stopped-but-intact; restart it with: docker start $C"
    exit 1
}
# Checked deletion (review 0814c): --archive preserves ownership, so an rm
# can fail (EPERM) and a poison copy would then be RESTORED into the fresh
# container — the heal would reinstall the wedge it exists to clear. Delete
# with verification and assert absence before anything is restored.
for pf in "$POISON" "$POISON-wal" "$POISON-shm"; do
    rm -f "$B/stage/$pf" || true
    [ ! -e "$B/stage/$pf" ] || {
        log "FATAL: cannot delete staged poison file $pf — refusing to restore a copy of the wedge."
        exit 1
    }
done
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
# --archive both ways: the copy back must land with the ORIGINAL UID/GID (the
# aggkit image runs as a non-root user; a root-owned restore is unwritable by
# the service — a "successful" heal that is not one).
docker cp --archive "$B/stage/." "$C:/tmp/" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: restore copy into the recreated container failed — staging dir retained"; exit 1; }
# Verify the restore round-trips by CONTENT, not existence (review 0814): read
# the state dir back and require byte-identical files (the recreated container
# may lack a shell — distroless — so verification goes through docker cp + a
# host-side recursive diff, which compares contents).
docker cp --archive "$C:/tmp/." "$B/readback" >/dev/null 2>&1 \
    || { KEEP_STAGE=1; log "FATAL: cannot read back the restored state dir — staging dir retained"; exit 1; }
if ! DIFF_OUT=$(diff -r "$B/stage" "$B/readback" 2>&1); then
    KEEP_STAGE=1
    log "FATAL: restored state differs from the staged manifest — NOT healthy to start:"
    echo "$DIFF_OUT" | head -10 | while IFS= read -r l; do log "       $l"; done
    log "       Stopping $SVC (a diverging half-restore must not run); staging dir retained."
    docker stop "$C" >/dev/null 2>&1 || true
    exit 1
fi
# Ownership/mode manifest (review 0814c/d): content equality does not prove the
# service can OPEN its DBs — --archive round-trips uid/gid/mode through tar, so
# compare stat manifests (directories AND files, numeric uid:gid:mode) of stage
# vs read-back. Manifests are materialized to files with CHECKED rcs — a failed
# producer inside process substitution is invisible, so none is used.
stat_manifest() { (cd "$1" && find . \( -type f -o -type d \) -printf '%y %U:%G:%m %p\n' | LC_ALL=C sort); }
if ! stat_manifest "$B/stage" > "$B/manifest.stage"; then
    KEEP_STAGE=1
    log "FATAL: cannot build the staged ownership manifest — stopping $SVC; staging dir retained."
    docker stop "$C" >/dev/null 2>&1 || true
    exit 1
fi
if ! stat_manifest "$B/readback" > "$B/manifest.readback"; then
    KEEP_STAGE=1
    log "FATAL: cannot build the read-back ownership manifest — stopping $SVC; staging dir retained."
    docker stop "$C" >/dev/null 2>&1 || true
    exit 1
fi
if ! OWN_DIFF=$(diff "$B/manifest.stage" "$B/manifest.readback" 2>&1); then
    KEEP_STAGE=1
    log "FATAL: restored ownership/modes differ from the staged manifest:"
    echo "$OWN_DIFF" | head -10 | while IFS= read -r l; do log "       $l"; done
    log "       Stopping $SVC; staging dir retained."
    docker stop "$C" >/dev/null 2>&1 || true
    exit 1
fi

BASE_RESTARTS=$(docker inspect -f '{{.RestartCount}}' "$C" 2>/dev/null || echo 0)
COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    start "$SVC" >/dev/null 2>&1 \
    || {
        KEEP_STAGE=1
        log "FATAL: start failed — stopping any partially-started $SVC; staging dir retained"
        docker stop "$C" >/dev/null 2>&1 || true
        exit 1
    }

# Health gate (review 0814): a heal that starts a crash-looping service is not
# a heal. After a settle window the container must be RUNNING with NO restart
# growth, producing fresh log output, and that output must not be a crash loop
# (fatal/panic markers). On any failed validation the recreated service is
# STOPPED — a diverging instance must not keep serving.
HEAL_T0=$(date +%s)
sleep 25
STATE=$(docker inspect -f '{{.State.Status}} {{.State.Restarting}}' "$C" 2>/dev/null || echo "gone")
NOW_RESTARTS=$(docker inspect -f '{{.RestartCount}}' "$C" 2>/dev/null || echo 999)
RECENT=$(docker logs --since 25s "$C" 2>&1 || true)
RECENT_LINES=$(printf '%s' "$RECENT" | grep -c . || true)
CRASH_MARKERS=$(printf '%s' "$RECENT" | grep -ciE "panic|fatal error|level=fatal|FATAL" || true)
# Wedge-specific probe (review 0814c): the caller passes the signature that
# triggered the heal (chaos watchdog: the monitoring-DB loop). Fresh logs
# still matching it mean the wedge did NOT clear — retry/error chatter must
# not read as health. PROGRESS_PATTERN requires actual component work.
WEDGE_MATCHES=0
[ -n "${WEDGE_PATTERN:-}" ] && WEDGE_MATCHES=$(printf '%s' "$RECENT" | grep -cE "$WEDGE_PATTERN" || true)
# Exact wedge-state check (review 0814e): the exact tx counts against health
# ONLY when paired with the wedge error — AggKit success logs legitimately
# mention the submitted tx id, so a bare-substring match would reject the
# successful path.
WEDGE_TX_MATCHES=0
[ -n "${WEDGE_TX:-}" ] && WEDGE_TX_MATCHES=$(printf '%s' "$RECENT" | grep -F "$WEDGE_TX" \
    | grep -cE "${WEDGE_PATTERN:-already exists in monitoring DB}" || true)
PROGRESS_MATCHES=$(printf '%s' "$RECENT" | grep -ciE "${PROGRESS_PATTERN:-level=info|INFO}" || true)
fail_health() {
    KEEP_STAGE=1
    log "FATAL: $1 — stopping $SVC; staging dir retained."
    docker stop "$C" >/dev/null 2>&1 || true
    exit 1
}
[ "$STATE" = "running false" ] \
    || fail_health "$SVC is not stably running after the heal (state: $STATE)"
[ "$NOW_RESTARTS" -le "$BASE_RESTARTS" ] \
    || fail_health "$SVC restarted during the settle window ($BASE_RESTARTS -> $NOW_RESTARTS restarts)"
[ "${RECENT_LINES:-0}" -gt 0 ] \
    || fail_health "$SVC produced no log output in the settle window (no proof the process works)"
[ "${CRASH_MARKERS:-0}" -eq 0 ] \
    || fail_health "$SVC logs show $CRASH_MARKERS fatal/panic marker(s) in the settle window"
[ "${WEDGE_MATCHES:-0}" -eq 0 ] \
    || fail_health "$SVC still logs the wedge signature ($WEDGE_MATCHES match(es) of WEDGE_PATTERN) — the heal did not clear it"
[ "${WEDGE_TX_MATCHES:-0}" -eq 0 ] \
    || fail_health "$SVC still pairs the EXACT lost tx ${WEDGE_TX:-} with the wedge error ($WEDGE_TX_MATCHES match(es)) — the wedge re-formed"
# POSITIVE exact outcome (review 0814e): a quiet window is not success — the
# heal exists so the resent tx gets durably admitted by the proxy. Require the
# success-specific transition: the proxy's transactions table knows WEDGE_TX
# within HEAL_CONFIRM_TIMEOUT, or the wedge-paired error reappears (fail).
if [ -n "${WEDGE_TX:-}" ]; then
    CONFIRM_TIMEOUT="${HEAL_CONFIRM_TIMEOUT:-120}"
    waited=0
    confirmed=0
    while [ "$waited" -lt "$CONFIRM_TIMEOUT" ]; do
        known=$(docker exec "$PG" psql -U agglayer -d agglayer_store -tAc \
            "SELECT count(*) FROM transactions WHERE tx_hash='$WEDGE_TX'" 2>/dev/null || echo "")
        if [ "${known:-0}" != "" ] && [ "${known:-0}" -gt 0 ] 2>/dev/null; then
            confirmed=1
            break
        fi
        rewedged=$(docker logs --since 10s "$C" 2>&1 | grep -F "$WEDGE_TX" \
            | grep -cE "${WEDGE_PATTERN:-already exists in monitoring DB}" || true)
        [ "${rewedged:-0}" -eq 0 ] \
            || fail_health "$SVC re-wedged on the exact tx ${WEDGE_TX} while waiting for durable admission"
        sleep 5
        waited=$((waited + 5))
    done
    [ "$confirmed" -eq 1 ] \
        || fail_health "the proxy never durably admitted the resent tx ${WEDGE_TX} within ${CONFIRM_TIMEOUT}s — no positive proof the wedge cleared"
    log "positive exact outcome: proxy durably admitted ${WEDGE_TX:0:18}… after ${waited}s"
fi
[ "${PROGRESS_MATCHES:-0}" -gt 0 ] \
    || fail_health "$SVC produced no progress output (PROGRESS_PATTERN) in the settle window"
log "preserve-healed (manifest=$manifest_count files restored+content-verified, $POISON wiped, health confirmed after $(( $(date +%s) - HEAL_T0 ))s: running, restarts stable at $NOW_RESTARTS, ${RECENT_LINES} fresh log lines, 0 crash markers)"
exit 0
