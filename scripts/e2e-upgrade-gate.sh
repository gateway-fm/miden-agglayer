#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Upgrade gate: the regular "3x e2e + growing soak" run.
#
#   Phase A  3x `make test-e2e`, each on a FRESH chain (wiped before, torn down
#            after by the target itself).
#   Phase B  ONE fresh chain, never reset afterwards, cycled until halted:
#              N=30 loadtest -> event completeness -> full-DB-loss drill -> chaos-soak
#
# Phase B halts (stack left UP for diagnosis) when:
#   - the same step FAILs in HALT_AFTER_REPEATS consecutive cycles (default 2) —
#     one red is evidence, a repeated red is a wedge every later cycle would only
#     re-measure (the 0.17 account-tag cap ran 13 broken cycles overnight before
#     this existed);
#   - the stack itself is INFRA/WEDGED (scripts/lib-stack-health.sh);
#   - `touch $RESULTS/STOP` (checked between steps);
#   - MAX_CYCLES is reached (default: unlimited).
# A single failed step never stops the run: its post-mortem is captured while the
# stack is still up and the cycle continues.
#
# Every cycle records the synthetic tip AND the proxy's tracked account count
# (bridge_fee_vault_expected_accounts) — the chain must be seen to grow, and the
# account count is what crossed miden-client's 128 account-tag cap.
#
# The DB-loss drill is never forced with ALLOW_RESTORED_BASELINE: each cycle's
# N=30 runs first, so every drill has organic history above the last restore and
# classifies itself "mixed" (fidelity above, idempotence below) on its own.
#
# Env: GATE_TAG (results dir prefix, default "gate"), GATE_TS (resume into an
# existing dir), SKIP_PHASE_A=1, HALT_AFTER_REPEATS, MAX_CYCLES, PREFLIGHT_GRACE
# (seconds a non-healthy stack may take to recover before a cycle, default 1800).
# Results: e2e-results/<GATE_TAG>-<TS>/{results.tsv,growth.tsv,gate.log,logs/}
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || exit 1
TS="${GATE_TS:-$(date -u +%Y%m%dT%H%M%SZ)}"
R="e2e-results/${GATE_TAG:-gate}-$TS"; mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'phase\tstep\tstatus\tsecs\tlog\n' > "$TSV"
GROWTH="$R/growth.tsv"; [[ -f "$GROWTH" ]] || printf 'when\tmark\tsynthetic_tip\ttracked_accounts\n' > "$GROWTH"
HALT_AFTER_REPEATS="${HALT_AFTER_REPEATS:-2}"
MAX_CYCLES="${MAX_CYCLES:-0}"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)
export BATTERY_RESULTS_DIR="$PWD/$R"
# shellcheck source=scripts/lib-stack-health.sh
. scripts/lib-stack-health.sh

log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$R/gate.log"; }
pg() { local p; p="$(docker ps --format '{{.Names}}' | grep -E -- '-miden-agglayer-1$' | head -1)"; echo "${p%-miden-agglayer-1}-agglayer-postgres-1"; }
mark() { # $1 label
    local tip accts
    tip="$(docker exec "$(pg)" psql -U agglayer -d agglayer_store -tAc \
        'SELECT latest_block_number FROM service_state WHERE id=1' 2>/dev/null | tr -d '[:space:]')"
    accts="$(curl -sf --max-time 5 http://localhost:8546/metrics 2>/dev/null \
        | awk '/^bridge_fee_vault_expected_accounts /{print $2}')"
    printf '%s\t%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$1" "${tip:-?}" "${accts:-?}" >> "$GROWTH"
    log "growth: $1 -> tip ${tip:-?}, tracked accounts ${accts:-?}"
}
stop_requested() { [[ -f "$R/STOP" ]] && { log "STOP file present — ending cleanly"; return 0; }; return 1; }

post_mortem() { # $1 label
    local out="$R/logs/$1-postmortem.txt"
    {
        echo "post-mortem $1 — $(date -u +%FT%TZ)"
        docker ps -a --format '{{.Names}}\t{{.Status}}' | grep -E '^miden-agglayer-' || true
        docker exec "$(pg)" psql -U agglayer -d agglayer_store -c \
          "SELECT projector_cursor, reconcile_cursor, latest_block_number, restored_at_cursor FROM service_state WHERE id=1" 2>&1 || true
        docker exec "$(pg)" psql -U agglayer -d agglayer_store -c \
          "SELECT tx_hash, status, error_message FROM transactions WHERE status='pending'" 2>&1 || true
        docker exec "$(pg)" psql -U agglayer -d agglayer_store -c \
          "SELECT signer, nonce, tx_hash FROM queued_txns ORDER BY created_at" 2>&1 || true
        echo "== proxy ERROR/WARN classes, last 30 min (counted, ids masked)"
        docker logs --since 30m miden-agglayer-miden-agglayer-1 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
          | grep -E ' (WARN|ERROR) ' | sed -E 's/^[^ ]+ +//; s/0x[0-9a-fA-F]{8,}/0x…/g; s/[0-9]{3,}/N/g' \
          | cut -c1-200 | sort | uniq -c | sort -rn | head -20 || true
        local svc   # NOT `c`: that is the global cycle counter (the stub test caught it)
        for svc in miden-agglayer miden-node ntx-builder validator aggkit bridge-service; do
            echo "== $svc log tail"; docker logs --tail 120 "miden-agglayer-$svc-1" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g'
        done
    } > "$out" 2>&1
    log "  post-mortem: $out"
}

LAST_RC=0
run() { # run <phase> <step> <cmd...>   (sets LAST_RC)
    local phase="$1" step="$2"; shift 2
    local lg="$R/logs/$phase-$step.log" t0 st
    log "$phase $step START"
    t0=$(date +%s); "${BASE_ENV[@]}" "$@" > "$lg" 2>&1; LAST_RC=$?
    st=PASS; (( LAST_RC != 0 )) && st=FAIL
    printf '%s\t%s\t%s\t%s\t%s\n' "$phase" "$step" "$st" "$(( $(date +%s) - t0 ))" "$lg" >> "$TSV"
    log "$phase $step $st rc=$LAST_RC ($(( $(date +%s) - t0 ))s)"
    [[ "$st" == FAIL ]] && post_mortem "$phase-$step"
    return 0
}

log "=== UPGRADE GATE start — $(git rev-parse --short HEAD) on $(git rev-parse --abbrev-ref HEAD) ==="

# ── Phase A: 3x full suite, fresh chain each ───────────────────────────────────
if [[ "${SKIP_PHASE_A:-0}" != "1" ]]; then
    for i in 1 2 3; do
        "${BASE_ENV[@]}" KEEP_CHAIN=0 make e2e-down >> "$R/logs/down.log" 2>&1 || true
        run A "test-e2e-$i" env KEEP_CHAIN=0 make test-e2e
        stop_requested && exit 0
    done
fi

# ── Phase B: one growing chain ─────────────────────────────────────────────────
log "=== Phase B: fresh genesis ONCE, then never reset ==="
"${BASE_ENV[@]}" KEEP_CHAIN=0 make e2e-down >> "$R/logs/down.log" 2>&1 || true
run B "stack-up" env KEEP_CHAIN=0 make e2e-l2l2-up
(( LAST_RC == 0 )) || { log "Phase B stack did not come up — halting"; exit 1; }
export KEEP_CHAIN=1
BASE_ENV+=(KEEP_CHAIN=1)

declare -A FAILED_LAST=() FAILED_NOW=()
STEPS=(loadtest-N30 verify-event-completeness full-db-loss-recovery chaos-soak)
step_cmd() {
    case "$1" in
        loadtest-N30)              echo "env N=30 ./scripts/e2e-bridge-loadtest-isolated.sh" ;;
        verify-event-completeness) echo "./scripts/verify-event-completeness.sh" ;;
        full-db-loss-recovery)     echo "./scripts/e2e-full-db-loss-recovery.sh" ;;
        chaos-soak)                echo "env N=30 CHAOS_DURATION=300 GARBO_DURATION=300 ./scripts/e2e-chaos-soak.sh" ;;
    esac
}

# A chaos-soak step deliberately crashes services; the preflight that follows can
# catch the proxy mid-recovery. Wait up to PREFLIGHT_GRACE seconds for the stack to
# report healthy before halting — and LOG how long recovery took, so a slow recovery
# is evidence in gate.log, never silently absorbed (0.17.1 gate c1: a 10 s node
# partition kept the proxy's Miden client down ~15 min — no gRPC/TCP keepalive).
PREFLIGHT_GRACE="${PREFLIGHT_GRACE:-1800}"
preflight() { # sets h / hrc
    local t0=$SECONDS first=""
    while true; do
        h="$(stack_health)"; hrc=$?
        (( hrc == 0 )) && break
        [[ -z "$first" ]] && { first="$h"; log "preflight: stack is $h — waiting up to ${PREFLIGHT_GRACE}s for recovery"; }
        (( SECONDS - t0 >= PREFLIGHT_GRACE )) && return 0
        sleep 30
    done
    [[ -n "$first" ]] && log "preflight: recovered from $first after $(( SECONDS - t0 ))s"
    return 0
}

c=1
while (( MAX_CYCLES == 0 || c <= MAX_CYCLES )); do
    preflight
    if (( hrc != 0 )); then
        log "stack is $h before cycle $c after a ${PREFLIGHT_GRACE}s grace — HALT (every later step would measure the wedge)"
        post_mortem "B-c$c-preflight"
        printf '%s\t%s\t%s\t%s\t%s\n' "B" "c$c-preflight" "HALT:$h" 0 "$R/gate.log" >> "$TSV"
        exit 1
    fi
    log "=== CYCLE $c (stack $h) ==="
    mark "c$c start"
    FAILED_NOW=()
    for s in "${STEPS[@]}"; do
        # shellcheck disable=SC2046
        run B "c$c-$s" $(step_cmd "$s")
        (( LAST_RC != 0 )) && FAILED_NOW[$s]=1
        [[ "$s" == verify-event-completeness ]] && mark "c$c after N=30"
        [[ "$s" == full-db-loss-recovery ]] && mark "c$c after db-loss"
        stop_requested && exit 0
    done
    mark "c$c end"
    # A step red in this cycle AND the previous (HALT_AFTER_REPEATS-1) cycles is a wedge.
    repeat=""
    for s in "${!FAILED_NOW[@]}"; do
        n=$(( ${FAILED_LAST[$s]:-0} + 1 ))
        FAILED_NOW[$s]=$n
        (( n >= HALT_AFTER_REPEATS )) && repeat+=" $s(x$n)"
    done
    FAILED_LAST=()
    for s in "${!FAILED_NOW[@]}"; do FAILED_LAST[$s]=${FAILED_NOW[$s]}; done
    if [[ -n "$repeat" ]]; then
        log "HALT: repeated failure in consecutive cycles:$repeat — stack left UP for diagnosis"
        printf '%s\t%s\t%s\t%s\t%s\n' "B" "c$c-halt" "HALT:repeat$repeat" 0 "$R/gate.log" >> "$TSV"
        exit 1
    fi
    log "=== CYCLE $c done ==="
    c=$((c + 1))
done
log "=== MAX_CYCLES=$MAX_CYCLES reached — gate complete ==="
