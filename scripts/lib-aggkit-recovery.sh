# shellcheck shell=bash
# Shared evidence and health checks for the watchdog and preserve-healer.
AGGKIT_EVIDENCE_PARSER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/aggkit-log-evidence.py"

aggkit_known_hashes() {
    local pg="$1" hashes="$2" sql_hashes
    # Only signed transaction hashes emitted by the parser may reach SQL.
    [[ "$hashes" =~ ^0x[0-9a-f]{64}(,0x[0-9a-f]{64})*$ ]] || return 1
    sql_hashes="'${hashes//,/\',\'}'"
    docker exec "$pg" psql -v ON_ERROR_STOP=1 -U agglayer -d agglayer_store -tAc \
        "SELECT count(*) FROM transactions WHERE tx_hash IN ($sql_hashes)"
}

# Return 0 only for a repeated, aged injection whose SIGNED hashes are all
# absent. Return 2 for no action, 1 for an unavailable/invalid probe. The raw
# snapshot and decision are retained by the caller, including skipped cases.
aggkit_probe_wedge() {
    local container="$1" pg="$2" snapshot="$3" record known
    if ! docker logs --timestamps --since "${AGGKIT_LOG_LOOKBACK:-15m}" "$container" >"$snapshot" 2>&1; then
        log "decision=probe-unavailable probe=container-logs container=$container"
        return 1
    fi
    record=$(python3 "$AGGKIT_EVIDENCE_PARSER" \
        --min-repeats "${REPEATS_MIN:-10}" --min-age "${WEDGE_MIN_AGE:-60}" <"$snapshot") || return 1
    IFS=$'\t' read -r WEDGE_MONITOR WEDGE_GER WEDGE_HASHES repeats span age decision <<<"$record"
    log "decision=$decision monitor=$WEDGE_MONITOR ger=$WEDGE_GER signed_hashes=$WEDGE_HASHES repeats=$repeats span_s=$span last_retry_age_s=$age"
    [[ "$decision" == candidate ]] || return 2
    if ! known=$(aggkit_known_hashes "$pg" "$WEDGE_HASHES") || [[ ! "$known" =~ ^[0-9]+$ ]]; then
        log "decision=probe-unavailable probe=proxy-admission monitor=$WEDGE_MONITOR"
        return 1
    fi
    log "decision=admission-probe monitor=$WEDGE_MONITOR known_signed_hashes=$known"
    [[ "$known" == 0 ]] || return 2
}

# A restart during chaos is not permanent failure. Require a NEW uninterrupted
# 25s window, within a bounded timeout; logs from an earlier process generation
# cannot poison the new window. This function never stops a restored service.
# On success, RECENT and NOW_RESTARTS belong to the verified stable generation.
aggkit_wait_stable() {
    local container="$1" timeout="${2:-120}" window="${3:-25}"
    local start now stable_since=0 generation="" observed id state restarting started restarts after
    start=$(date +%s)
    while true; do
        now=$(date +%s)
        if observed=$(docker inspect -f '{{.Id}} {{.State.Status}} {{.State.Restarting}} {{.RestartCount}} {{.State.StartedAt}}' "$container"); then
            read -r id state restarting restarts started <<<"$observed"
            if [[ "$state" == running && "$restarting" == false && "$restarts" =~ ^[0-9]+$ && -n "$started" ]]; then
                if [[ "$generation" != "$observed" ]]; then
                    log "health=waiting container=$container generation=$id started_at=$started restarts=$restarts stable_window_s=$window"
                    generation="$observed"; stable_since=$now
                elif (( now - stable_since >= window )); then
                    if RECENT=$(docker logs --since "$stable_since" "$container" 2>&1) \
                       && after=$(docker inspect -f '{{.Id}} {{.State.Status}} {{.State.Restarting}} {{.RestartCount}} {{.State.StartedAt}}' "$container") \
                       && [[ "$after" == "$generation" ]] \
                       && [[ -n "$RECENT" ]] \
                       && ! grep -qiE 'panic|fatal error|level=fatal|FATAL' <<<"$RECENT"; then
                        NOW_RESTARTS="$restarts"
                        log "health=stable container=$container started_at=$started restarts=$restarts stable_for_s=$((now-stable_since))"
                        return 0
                    fi
                    log "health=unconfirmed container=$container reason=logs-or-generation-changed"
                    generation=""; stable_since=0
                fi
            else
                log "health=waiting container=$container state=$state restarting=$restarting restarts=$restarts"
                generation=""; stable_since=0
            fi
        else
            log "health=probe-unavailable container=$container"
            generation=""; stable_since=0
        fi
        (( now - start < timeout )) || { log "health=timeout container=$container waited_s=$((now-start))"; return 1; }
        sleep 5
    done
}
