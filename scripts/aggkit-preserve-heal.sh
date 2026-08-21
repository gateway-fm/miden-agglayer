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
REPEATS_MIN="${REPEATS_MIN:-5}"       # same-hash repeats/60s to call it wedged
FORCE="${FORCE:-0}"                    # 1 = heal without the wedge precheck
# FORCE=1 callers (the full-DB-loss drill) never supply a wedged tx id, and the
# proof block below now runs for them too — under `set -u` a bare "$WEDGE_TX"
# expansion there would abort the healer instead of proving recovery. Normalise
# once so every later reference is safe and "unset" means "no exact target".
WEDGE_TX="${WEDGE_TX:-}"

COMPOSE=(-f "$PROJECT_DIR/docker-compose.e2e.yml")
[[ -f "$PROJECT_DIR/docker-compose.l2l2.yml" ]] && COMPOSE+=(-f "$PROJECT_DIR/docker-compose.l2l2.yml")
[[ -f "$PROJECT_DIR/docker-compose.web3signer.yml" ]] \
    && docker ps --format '{{.Names}}' | grep -q "^$PROJECT-web3signer-1$" \
    && { COMPOSE+=(-f "$PROJECT_DIR/docker-compose.web3signer.yml")
         [[ -f "$PROJECT_DIR/fixtures/web3signer-keys.env" ]] && { set -a; . "$PROJECT_DIR/fixtures/web3signer-keys.env"; set +a; } }

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

BASE_RESTARTS=$(docker inspect -f '{{.RestartCount}}' "$C" 2>/dev/null || echo 0)
# `docker start` (plain), NOT `docker compose start`: compose honours
# depends_on and BLOCKS until this service's dependencies report healthy
# ("Container <proxy> Healthy" before "Container <aggkit> Starting"). The
# chaos watchdog calls this heal precisely WHILE faults are active — the
# proxy is paused/killed/unhealthy by design — so the compose form fails,
# the container is left in state "created", and the whole run is crippled
# (2026-08-21 chaos: services_down='aggkit=created', loadtest and fresh-op
# both failed downstream; same cause as the 2026-08-20 manual "FATAL: start
# failed"). The container was already created with the correct config by the
# recreate above, so a plain start needs no dependency graph — and this heal's
# job is to restore THIS service, not to require a healthy stack.
docker start "$C" >/dev/null 2>&1 \
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
# Post-restore soft failure (2026-08-18 cycle-2 chaos, heal attempt 3): once
# the restore is content-verified and the service is RUNNING, an unproven or
# re-forming wedge must not stop it — attempt 3 cleared the wedge but its
# 120s admission confirm overlapped an active chaos prover-kill, timed out,
# and fail_health then STOPPED a healthy aggkit, manufacturing the verdict-d
# outage. Leave the service up (a re-wedge just fires the watchdog again),
# return rc=1 so the caller does NOT count a confirmed heal.
fail_soft() {
    KEEP_STAGE=1
    log "UNCONFIRMED: $1 — leaving $SVC RUNNING (soft-fail; not counted as a heal); staging dir retained."
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
# Bare-pattern gate ONLY when no exact tx is known (2026-08-18 cycle-1 live
# false-positive): 'already exists in monitoring DB' is aggoracle's NORMAL
# per-tick dedup line for its CURRENT healthy pending injection — after a wipe
# the fresh replacement tx (a NEW id, the pending GER moved on while aggkit was
# down) legitimately produces it. With WEDGE_TX known, the exact-pair check and
# the positive-outcome wait below are the real verification.
if [ -z "${WEDGE_TX:-}" ]; then
    [ "${WEDGE_MATCHES:-0}" -eq 0 ] \
        || fail_soft "$SVC still logs the wedge signature ($WEDGE_MATCHES match(es) of WEDGE_PATTERN) — the heal did not clear it"
fi
[ "${WEDGE_TX_MATCHES:-0}" -eq 0 ] \
    || fail_soft "$SVC still pairs the EXACT lost tx ${WEDGE_TX:-} with the wedge error ($WEDGE_TX_MATCHES match(es)) — the wedge re-formed"
# POSITIVE exact outcome (review 0814e): a quiet window is not success — the
# heal exists so the resent tx gets durably admitted by the proxy. Require the
# success-specific transition: the proxy's transactions table knows WEDGE_TX
# within HEAL_CONFIRM_TIMEOUT, or the wedge-paired error reappears (fail).
# The proof runs whether or not a WEDGE_TX was supplied. Gating the whole
# block on WEDGE_TX meant the FORCE=1 caller (full-DB-loss recovery) got NO
# functional verification at all — it passed on "container is up and printing
# INFO lines", which a dead aggoracle also does.
if true; then
    # The resent injection keeps WEDGE_TX's deterministic id ONLY if the same
    # GER is still the one being injected. If newer GERs superseded it while
    # the service was wedged/down, the post-wipe replacement is a NEW id
    # (2026-08-18 cycle-1: wedged 0xe30cf8… replaced by fresh 0x89edca…) and
    # WEDGE_TX will never reach the proxy — so the positive proof is: the
    # CURRENT pending injection (fresh-log id, falling back to WEDGE_TX) gets
    # durably admitted. Re-wedge on the exact old id still fails immediately.
    TARGET_TX=$(printf '%s' "$RECENT" \
        | grep -aE "${WEDGE_PATTERN:-already exists in monitoring DB}" \
        | grep -aoE 'ID: 0x[0-9a-fA-F]{64}' | tail -1 | cut -d' ' -f2)
    # No pending injection in the settle window at all (the wiped record was
    # not re-added — the L2 target is already current, tonight's second live
    # shape): there is nothing to await. Waiting on the ORIGINAL id here is
    # provably wrong — a superseded injection never resends it — so pass on
    # the negative gates and defer positive proof to live traffic (the loop's
    # next N=30 leg hard-fails if GER injection is actually broken).
    if [ -z "$TARGET_TX" ]; then
        log "no pending injection observed post-heal (wedge-pair absent, service stable) — positive admission proof deferred to live traffic"
    fi
    [ -z "$TARGET_TX" ] || [ "$TARGET_TX" = "$WEDGE_TX" ] \
        || log "pending injection superseded the wedged id: waiting on ${TARGET_TX:0:18}… (was ${WEDGE_TX:0:18}…)"
    CONFIRM_TIMEOUT="${HEAL_CONFIRM_TIMEOUT:-120}"
    waited=0
    confirmed=0
    # With no pending injection to await there is nothing here to prove, and
    # nothing this script can substitute for it — see the removed
    # aggregate-counter fallback. The wait below is meaningful only when an
    # exact TARGET_TX exists; otherwise it ends immediately as UNPROVEN and the
    # caller (which can drive an injection and watch it land) concludes.
    NO_TARGET=0
    [ -n "$TARGET_TX" ] || { NO_TARGET=1; waited="$CONFIRM_TIMEOUT"; }
    while [ "$confirmed" -eq 0 ] && [ "$waited" -lt "$CONFIRM_TIMEOUT" ]; do
        known=$(docker exec "$PG" psql -U agglayer -d agglayer_store -tAc \
            "SELECT count(*) FROM transactions WHERE tx_hash='$TARGET_TX'" 2>/dev/null || echo "")
        if [ "${known:-0}" != "" ] && [ "${known:-0}" -gt 0 ] 2>/dev/null; then
            confirmed=1; CONFIRMED_BY=exact
            break
        fi
        # The aggregate injected-GER counter fallback is GONE. It accepted ANY
        # increase in the global count as proof this aggoracle recovered, but
        # the count carries no identity: the restore projector can independently
        # mark an already-consumed, pre-heal GER as injected, advancing the
        # counter while the restarted aggoracle stays dead and the target stays
        # unadmitted. The log line that went with it also asserted a
        # "superseding injection" the counter cannot identify.
        #
        # Proof here is the EXACT target being durably admitted, or nothing —
        # in which case the caller is told so (exit 3) and proves the pipeline
        # itself.
        if [ -n "$WEDGE_TX" ]; then
            rewedged=$(docker logs --since 10s "$C" 2>&1 | grep -F "$WEDGE_TX" \
                | grep -cE "${WEDGE_PATTERN:-already exists in monitoring DB}" || true)
            [ "${rewedged:-0}" -eq 0 ] \
                || fail_soft "$SVC re-wedged on the exact tx ${WEDGE_TX} while waiting for durable admission"
        fi
        sleep 5
        waited=$((waited + 5))
    done
    if [ "$confirmed" -ne 1 ]; then
        if [ "${HEAL_ALLOW_DEFERRED_PROOF:-0}" = "1" ]; then
            # Quiet stack with nothing to inject: the caller has said it will
            # prove liveness itself (the drill's own post-heal GER leg).
            if [ "${NO_TARGET:-0}" = "1" ]; then
                # No wait happened — there was nothing to wait FOR. Reporting
                # "within ${CONFIRM_TIMEOUT}s" here would describe a
                # confirmation window that never ran.
                log "no pending injection existed to confirm against (no wait performed) and HEAL_ALLOW_DEFERRED_PROOF=1 — positive proof deferred to the caller"
            else
                log "no injection observed within ${waited}s and HEAL_ALLOW_DEFERRED_PROOF=1 — positive proof deferred to the caller"
            fi
            PROOF_DEFERRED=1
        else
            if [ "${NO_TARGET:-0}" = "1" ]; then
                fail_soft "no pending injection existed to confirm against (no wait performed) — no positive proof the injection pipeline recovered"
            else
                fail_soft "the exact target ${TARGET_TX} was not durably admitted within ${waited}s — no positive proof the injection pipeline recovered"
            fi
        fi
    fi
    # Only claim the EXACT transaction when the exact-transaction check is what
    # confirmed it. When the injected-GER counter fallback fired, a DIFFERENT
    # (superseding) GER advanced it — saying "the proxy durably admitted
    # TARGET_TX" there is simply false, and this file's log lines are read as
    # evidence.
    if [ "${CONFIRMED_BY:-}" = "exact" ] && [ -n "$TARGET_TX" ]; then
        log "positive exact outcome: proxy durably admitted ${TARGET_TX:0:18}… after ${waited}s"
    fi
fi
[ "${PROGRESS_MATCHES:-0}" -gt 0 ] \
    || fail_soft "$SVC produced no progress output (PROGRESS_PATTERN) in the settle window"
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
    log "preserve-healed but UNPROVEN (manifest=$manifest_count files restored+content-verified, $POISON wiped, service running with restarts stable at $NOW_RESTARTS; NO injection observed — the caller must prove the pipeline)"
    exit 3
fi
log "preserve-healed (manifest=$manifest_count files restored+content-verified, $POISON wiped, health confirmed after $(( $(date +%s) - HEAL_T0 ))s: running, restarts stable at $NOW_RESTARTS, ${RECENT_LINES} fresh log lines, 0 crash markers)"
exit 0
