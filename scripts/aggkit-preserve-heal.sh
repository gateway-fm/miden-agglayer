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
# Exit codes:
#   0  healed, and an injection was observed proving the pipeline resumed
#   1  error / unconfirmed (see the log line; the service is left as described)
#   2  no wedge to heal (no-op precheck)
#   3  healed and running, but NO injection was observed to prove it — only
#      possible with HEAL_ALLOW_DEFERRED_PROOF=1, which asserts the CALLER will
#      prove the pipeline itself. Callers must treat 3 as distinct from both 0
#      and 1; `set -e` callers must use `if ...; then rc=0; else rc=$?; fi`.
set -uo pipefail

SVC="${1:?usage: aggkit-preserve-heal.sh <aggkit|aggkit-l2b>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="${PROJECT:-${COMPOSE_PROJECT_NAME:-miden-agglayer}}"
ENV_FILE="${ENV_FILE:-$PROJECT_DIR/fixtures/.env}"
C="$PROJECT-$SVC-1"
PG="$PROJECT-agglayer-postgres-1"
REPEATS_MIN="${REPEATS_MIN:-10}"      # repeated injection evidence, not total log count
FORCE="${FORCE:-0}"                    # 1 = heal without the wedge precheck
# FORCE is reserved for explicit full-DB-loss recovery, whose caller supplies
# independent post-heal liveness proof. Automatic watchdog calls use prechecks.
source "$SCRIPT_DIR/lib-aggkit-recovery.sh"

# This healer FORCE-RECREATES a container, so it must carry every overlay the
# stack was brought up with — dropping the custody overlay here would recreate
# a service without its signer wiring.
. "$PROJECT_DIR/scripts/lib-compose.sh"
compose_env_load
mapfile -t COMPOSE < <(compose_files)

log() { echo "[$(date '+%H:%M:%S')] preserve-heal($SVC): $*"; }

# Only the two aggkit services are healable by this script; it force-recreates
# the container it is given, so an arbitrary compose service name here would
# destroy something else entirely.
case "$SVC" in
    aggkit) ;;
    aggkit-l2b)
        # REFUSED, deliberately. Everything this script uses to decide and to
        # PROVE a heal is wired to the BASE proxy: the wedge precheck and the
        # positive-admission probe both read `$PROJECT-agglayer-postgres-1`,
        # and the injected-GER counter it falls back to counts base injections.
        # aggkit-l2b submits to anvil-l2b instead, so against it every healthy
        # transaction is "unknown to the proxy" by construction AND unrelated
        # base activity can certify a dead L2B aggoracle — a false positive
        # where the honest answer is "this tool cannot tell".
        #
        # Supporting it needs an L2B-side admission probe and an L2B database
        # handle; filed in docs/development/followups-h6-evidence-provenance.md.
        log "REFUSED: this healer's wedge detection and positive proof both read the BASE proxy"
        log "         database, while aggkit-l2b submits to anvil-l2b. Healing it from here would"
        log "         decide and certify from the wrong chain. Restore L2B coverage by adding an"
        log "         L2B-side admission probe (see docs/development/followups-h6-evidence-provenance.md)."
        exit 1
        ;;
    *) log "FATAL: '$SVC' is not a healable service (expected: aggkit)"; exit 1 ;;
esac

docker inspect "$C" >/dev/null 2>&1 || { log "container $C not found"; exit 1; }

# The destructive targets come from environment variables — verify each one
# actually belongs to the compose project we were told to heal before stopping
# or recreating anything.
for c in "$C" "$PG"; do
    docker inspect "$c" >/dev/null 2>&1 || { log "FATAL: container $c not found"; exit 1; }
    owner=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$c" 2>/dev/null)
    [[ "$owner" == "$PROJECT" ]] || {
        log "FATAL: refusing to touch $c — it belongs to compose project '${owner:-<none>}', not '$PROJECT'."
        exit 1
    }
done

# One heal at a time per project+service. The chaos watchdog, the recovery
# drill and a manual run can all fire at once; a second run entering while the
# first is between "stop" and "restore" would recreate the container out from
# under it and restore an older snapshot over newer state.
LOCK="/tmp/.aggkit-preserve-heal.$PROJECT.$SVC.lock"
exec 9>"$LOCK"
flock -n 9 || { log "another preserve-heal is already running for $PROJECT/$SVC (lock: $LOCK) — refusing to run concurrently"; exit 1; }

# ── wedge detection: exact injection -> signed hashes -> durable admission ──
if [[ "$FORCE" != 1 ]]; then
    PROBE_LOG=$(mktemp)
    if aggkit_probe_wedge "$C" "$PG" "$PROBE_LOG"; then probe_rc=0; else probe_rc=$?; fi
    if [[ -n "${HEAL_DIAGNOSTIC_DIR:-}" ]]; then
        mkdir -p "$HEAL_DIAGNOSTIC_DIR"
        gzip -c "$PROBE_LOG" >"$HEAL_DIAGNOSTIC_DIR/precheck.log.gz"
    fi
    rm -f "$PROBE_LOG"
    [[ "$probe_rc" == 0 ]] || exit "$probe_rc"
    log "confirmed absent signed hashes: monitor=$WEDGE_MONITOR ger=$WEDGE_GER hashes=$WEDGE_HASHES"
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
    local diagnostics="${HEAL_DIAGNOSTIC_DIR:-$B/diagnostics}"
    if [[ "$KEEP_STAGE" == 1 || -n "${HEAL_DIAGNOSTIC_DIR:-}" ]]; then
        mkdir -p "$diagnostics"
        docker inspect -f '{{json .State}}' "$C" >"$diagnostics/container-state.json" 2>&1 || true
        docker inspect -f '{{.Id}} {{.RestartCount}} {{.State.StartedAt}}' "$C" >"$diagnostics/container-generation.txt" 2>&1 || true
        docker logs --timestamps --since "${HEAL_T0:-60s}" "$C" 2>&1 | gzip >"$diagnostics/container.log.gz" || true
        log "diagnostics=$diagnostics"
    fi
    if [ "$KEEP_STAGE" -eq 1 ]; then
        log "Preserved staging dir (contains the ONLY copy of the aggkit state): $B"
    else
        rm -rf "$B"
    fi
}
trap cleanup EXIT
# A signal must never be quieter than an error. Without explicit INT/TERM
# handling bash runs the EXIT trap on the default signal action anyway, but
# NOT with a non-zero-ish state we can distinguish — so pin the retention here
# too: once we are past the destructive step, any interruption must keep $B.
# Before it, there is nothing to keep and the temp dir is still cleaned up.
on_signal() {
    log "interrupted by signal — staging dir retention is KEEP_STAGE=$KEEP_STAGE"
    exit 130
}
trap on_signal INT TERM
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

# `up --no-start` (not `create`): compose v2.40 rejects `create --no-deps`
# ("unknown flag"), so this line failed unconditionally AFTER the container was
# stopped — the chaos watchdog's heal left aggkit down (2026-08-18 cycle-1
# chaos NOT-GREEN, verdict d). `up --no-start --no-deps --force-recreate` is
# the supported spelling of create-without-starting-deps. Keep the output: a
# silently-discarded recreate error is what hid this.
# Arm the retention BEFORE the destructive step, not only on its error paths.
# From the instant --force-recreate starts, $B holds the ONLY copy of the
# certificate lineage and bridge-sync cursors. A SIGTERM/SIGINT in that window
# (runner timeout, Ctrl-C, watchdog kill) fires the EXIT trap with KEEP_STAGE=0
# and `rm -rf $B` destroys unrecoverable state — setting it on the `||` branch
# covers a FAILED recreate but not a KILLED one.
KEEP_STAGE=1
COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    up --no-start --no-deps --force-recreate "$SVC" >"$B/recreate.log" 2>&1 \
    || { log "FATAL: recreate failed — staging dir retained; see $B/recreate.log"; exit 1; }

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

# Once content and metadata round-trip correctly, health/proof failures retain
# the staged evidence and leave Docker's restart policy working. They must not
# stop an intact instance that recovered from a deliberate dependency outage.
HEAL_T0=$(date +%s)
fail_soft() {
    KEEP_STAGE=1
    log "UNCONFIRMED: $1 — leaving the restored $SVC under its restart policy; staging dir retained."
    exit 1
}
docker start "$C" || fail_soft "start failed"
aggkit_wait_stable "$C" "${HEAL_HEALTH_TIMEOUT:-120}" 25 \
    || fail_soft "no uninterrupted healthy window before the health deadline"
RECENT_LINES=$(printf '%s' "$RECENT" | grep -c . || true)
PROGRESS_MATCHES=$(printf '%s' "$RECENT" | grep -ciE "${PROGRESS_PATTERN:-level=info|INFO}" || true)
[[ "${PROGRESS_MATCHES:-0}" -gt 0 ]] || fail_soft "no progress output in the stable window"

# The positive proof uses a signed broadcast hash linked to an aggoracle
# injection in THIS heal's logs. A monitoring ID never appears in the proxy's
# transactions table. Dedup chatter alone is normal for a pending injection.
CONFIRM_TIMEOUT="${HEAL_CONFIRM_TIMEOUT:-120}"
confirmed=0; waited=0; NO_TARGET=1
while (( waited <= CONFIRM_TIMEOUT )); do
    if ! docker logs --timestamps --since "$HEAL_T0" "$C" >"$B/admission.log" 2>&1; then
        fail_soft "cannot read post-heal logs for the admission proof"
    fi
    record=$(python3 "$AGGKIT_EVIDENCE_PARSER" --latest <"$B/admission.log") \
        || fail_soft "invalid post-heal evidence"
    IFS=$'\t' read -r monitor ger hashes repeats span age decision <<<"$record"
    log "proof=$decision monitor=$monitor ger=$ger signed_hashes=$hashes waited_s=$waited"
    if [[ "$decision" == candidate ]]; then
        NO_TARGET=0
        if ! known=$(aggkit_known_hashes "$PG" "$hashes") || [[ ! "$known" =~ ^[0-9]+$ ]]; then
            log "proof=probe-unavailable probe=proxy-admission"
        elif (( known > 0 )); then
            confirmed=1
            log "positive exact outcome: proxy admitted a signed hash in [$hashes] for monitor=$monitor ger=$ger"
            break
        fi
    fi
    (( waited < CONFIRM_TIMEOUT )) || break
    sleep 5; waited=$((waited + 5))
done
if [[ "$confirmed" != 1 ]]; then
    if [[ "${HEAL_ALLOW_DEFERRED_PROOF:-0}" == 1 ]]; then
        log "no signed injection was confirmed within ${waited}s (no_target=$NO_TARGET); positive proof deferred to the caller"
        PROOF_DEFERRED=1
    else
        fail_soft "no signed injection was durably admitted within ${waited}s (no_target=$NO_TARGET)"
    fi
fi
# Admission can take time: do not certify a service that lost health meanwhile.
aggkit_wait_stable "$C" "${HEAL_HEALTH_TIMEOUT:-120}" 25 \
    || fail_soft "service did not remain healthy through admission verification"
# Every success proof has now passed and the restored state lives in the
# running container, so the staging copy is no longer the only copy — disarm
# the retention that was armed before the destructive step. Without this, each
# successful heal leaves a complete aggkit snapshot (cert lineage, sync
# cursors) in /tmp forever: unbounded disk growth and sensitive state kept
# around with no owner.
KEEP_STAGE=0
if [ "${PROOF_DEFERRED:-0}" = "1" ]; then
    # The negative gates all passed and the state was restored, but NOTHING
    # proved the injection pipeline actually resumed — the caller asked to
    # prove that itself. Say so, and exit with a DISTINCT code so a future
    # caller cannot read this as a confirmed heal by checking `rc == 0`.
    log "preserve-healed but UNPROVEN (manifest=$manifest_count files restored+content-verified, $POISON wiped, service running with restarts stable at $NOW_RESTARTS; no EXACT injection was confirmed here — the caller must prove the pipeline)"
    exit 3
fi
log "preserve-healed (manifest=$manifest_count files restored+content-verified, $POISON wiped, health confirmed after $(( $(date +%s) - HEAL_T0 ))s: running, restarts stable at $NOW_RESTARTS, ${RECENT_LINES} fresh log lines, 0 crash markers)"
exit 0
