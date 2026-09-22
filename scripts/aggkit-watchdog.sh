#!/usr/bin/env bash
# Recovery decisions and every helper outcome are evidence, not discarded output.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-aggkit-recovery.sh"

log() { printf '%s WATCHDOG-PROBE: %s\n' "$(date -u +%FT%TZ)" "$*"; }

watchdog_tick() {
    local rc attempt_dir
    if aggkit_probe_wedge "$AK" "$PG" "$WATCHDOG_DIR/latest.log"; then :; else
        return 0 # Skipped/unavailable probes never authorize a heal.
    fi
    [[ -z "${seen[$WEDGE_HASHES]:-}" ]] || return 0
    if (( attempts >= WATCHDOG_MAX_ATTEMPTS )); then
        if [[ "$budget_reported" == 0 ]]; then
            echo "$(date -u +%FT%TZ) WATCHDOG-BUDGET-EXHAUSTED: $attempts attempts consumed" | tee -a "$WATCHDOG_HEALS_FILE"
            budget_reported=1
        fi
        return 0
    fi
    seen[$WEDGE_HASHES]=1
    attempts=$((attempts + 1))
    attempt_dir="$WATCHDOG_DIR/attempt-$attempts"
    mkdir -p "$attempt_dir" || return 1
    gzip -c "$WATCHDOG_DIR/latest.log" >"$attempt_dir/trigger.log.gz" || return 1
    docker inspect -f '{{json .State}}' "$AK" >"$attempt_dir/before-state.json" 2>&1 || true
    echo "$(date -u +%FT%TZ) WATCHDOG-ATTEMPT: monitor=$WEDGE_MONITOR signed_hashes=$WEDGE_HASHES ger=$WEDGE_GER evidence=$attempt_dir" | tee -a "$WATCHDOG_HEALS_FILE"
    # Re-check under the healer's lock: admission may have completed since our
    # snapshot. FORCE=1 bypassed that protection in the old watchdog.
    if PROJECT="$PROJECT" FORCE=0 HEAL_DIAGNOSTIC_DIR="$attempt_dir" \
        AGGKIT_EVIDENCE_STATE="${AGGKIT_EVIDENCE_STATE:-}" \
        "$SCRIPT_DIR/aggkit-preserve-heal.sh" aggkit >"$attempt_dir/heal.log" 2>&1; then rc=0; else rc=$?; fi
    # The helper may exit during its lock/precheck, before installing its own
    # diagnostic trap. Preserve lifecycle evidence for those outcomes too.
    docker inspect -f '{{json .State}}' "$AK" >"$attempt_dir/container-state.json" 2>&1 || true
    docker inspect -f '{{.Id}} {{.RestartCount}} {{.State.StartedAt}}' "$AK" >"$attempt_dir/container-generation.txt" 2>&1 || true
    case "$rc" in
        0) echo "$(date -u +%FT%TZ) WATCHDOG: preserve-heal SUCCEEDED evidence=$attempt_dir" ;;
        2) echo "$(date -u +%FT%TZ) WATCHDOG-SKIPPED: precheck no longer authorizes recovery evidence=$attempt_dir" ;;
        *) echo "$(date -u +%FT%TZ) WATCHDOG-FAILED: preserve-heal rc=$rc evidence=$attempt_dir" ;;
    esac | tee -a "$WATCHDOG_HEALS_FILE"
}

watchdog_main() {
    PROJECT="${PROJECT:-miden-agglayer}"
    AK="$PROJECT-aggkit-1"; PG="$PROJECT-agglayer-postgres-1"
    WATCHDOG_HEALS_FILE="${WATCHDOG_HEALS_FILE:-/tmp/chaos-watchdog-heals}"
    WATCHDOG_MAX_ATTEMPTS="${WATCHDOG_MAX_ATTEMPTS:-6}"
    [[ "$WATCHDOG_MAX_ATTEMPTS" =~ ^[1-9][0-9]*$ ]] || { log 'invalid WATCHDOG_MAX_ATTEMPTS'; return 1; }
    umask 077
    WATCHDOG_DIR="${WATCHDOG_DIR:-$(mktemp -d /tmp/chaos-watchdog.XXXXXX)}"
    mkdir -p "$WATCHDOG_DIR" || return 1
    # Persist only identities within one container/process generation. A failed
    # send can age out of the rolling log window while dedup retries continue.
    AGGKIT_EVIDENCE_STATE="$WATCHDOG_DIR/signed-identities.json"
    : >"$WATCHDOG_HEALS_FILE"
    declare -A seen=()
    attempts=0; budget_reported=0
    log "evidence=$WATCHDOG_DIR max_attempts=$WATCHDOG_MAX_ATTEMPTS"
    while true; do
        sleep 30
        watchdog_tick || return 1
    done
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then watchdog_main; fi
