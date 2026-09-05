#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-load-chaos-run.sh — the battery's load+chaos tail, runnable standalone.
#
# Runs against the LIVE stack, in the battery's order:
#   1. N=30 isolated bridge loadtest
#   2. verify-event-completeness
#   3. N=30 chaos soak (CHAOS_DURATION / GARBO_DURATION)
#   4. the full-DB-loss drill on the resulting messy post-chaos state
#
# Exists so this ~70-minute tail can be proven once BEFORE committing ~20 hours
# to the four-iteration battery that contains it.
#
#   RESULTS_DIR=e2e-results/167-<ts> ITER=1 ./scripts/e2e-load-chaos-run.sh
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

R="${RESULTS_DIR:?set RESULTS_DIR to the battery results directory}"
ITER="${ITER:-1}"
PREFIX="${LOG_PREFIX:-loadchaos}"
mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'iter\ttarget\tstatus\tsecs\tlog\n' > "$TSV"

record() { printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$TSV"
           ./scripts/e2e-battery-matrix.py "$TSV" > "$R/MATRIX.md" 2>/dev/null; }

# ONLY="chaos-soak full-db-loss-recovery-postchaos" runs just those targets, so a
# harness fix can be re-proven without repeating the 38-minute loadtest in front
# of it. Unset runs the whole tail.
ONLY="${ONLY:-}"

# run <target> <command...>
run() {
    local target="$1"; shift
    if [[ -n "$ONLY" && " $ONLY " != *" $target "* ]]; then
        echo "  $target: skipped (ONLY='$ONLY')" | tee -a "$R/load-chaos.log"
        return 0
    fi
    local log="$R/logs/${PREFIX}${ITER}-${target}.log" t0=$SECONDS st
    echo "=== $target start $(date -u +%FT%TZ) ===" | tee -a "$R/load-chaos.log"
    if "$@" >"$log" 2>&1; then st=PASS; else st=FAIL; fi
    record "$ITER" "$target" "$st" "$((SECONDS-t0))" "$log"
    echo "  $target: $st ($((SECONDS-t0))s) -> $log" | tee -a "$R/load-chaos.log"
}

run "loadtest-N30"              env N=30 ./scripts/e2e-bridge-loadtest-isolated.sh
run "verify-event-completeness" ./scripts/verify-event-completeness.sh
run "chaos-soak"                env N=30 CHAOS_DURATION=300 GARBO_DURATION=300 ./scripts/e2e-chaos-soak.sh
# See e2e-battery.sh for why ALLOW_RESTORED_BASELINE=1 is correct here: the
# store already carries the pre-chaos drill's restore output, so this is a
# recovery from a MESSY history, not a fresh live-vs-restore fidelity run.
run "full-db-loss-recovery-postchaos" env ALLOW_RESTORED_BASELINE=1 ./scripts/e2e-full-db-loss-recovery.sh
echo "LOAD+CHAOS COMPLETE $(date -u +%FT%TZ)" | tee -a "$R/load-chaos.log"
