#!/usr/bin/env bash
# Unique control files and run labels identify resources owned by this chaos
# run. Never signal a retained PID: Bash may already have reaped that child.
chaos_request_stop() { [[ -z "$1" ]] || : > "$1"; }

chaos_wait_owned() { # <pid-variable> <control-file>
    local variable="$1" control="$2" pid="${!1:-}"
    [[ -z "$pid" ]] || wait "$pid" 2>/dev/null || true
    printf -v "$variable" '%s' ''
    python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("cleanup_ok") is True else 1)' \
        "${control}.stopped" 2>/dev/null
}

chaos_remove_owned_containers() { # <unique run label>
    local owner="$1" ids id empty=0 attempt
    [[ -n "$owner" ]] || return 1
    for attempt in 1 2 3 4 5; do
        ids=$(docker ps -aq --filter "label=miden.chaos-owner=$owner") || return 1
        if [[ -z "$ids" ]]; then
            empty=$((empty + 1))
            [[ "$empty" -ge 2 ]] && return 0
        else
            empty=0
            for id in $ids; do
                [[ "$id" =~ ^[a-f0-9]{12,64}$ ]] || return 1
                docker rm -f "$id" >/dev/null || return 1
            done
        fi
        sleep 0.2
    done
    return 1
}

# Acknowledged alerts leave no SQL row. Logs from this exact container cover
# emitted alerts across restarts; generation changes or unavailable logs fail.
chaos_claim_alert_count() { # <container> <original-id> <since> <evidence-file>
    local container="$1" original="$2" since="$3" evidence="$4" current
    current=$(docker inspect "$container" --format '{{.Id}}') || return 1
    [[ "$current" == "$original" ]] || return 1
    docker logs --since "$since" "$original" >"$evidence" 2>&1 || return 1
    grep -cF 'CLAIM consumption was not observed within the watchdog threshold' "$evidence" || true
}
