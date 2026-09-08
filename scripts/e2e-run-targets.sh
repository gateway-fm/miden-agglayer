#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-run-targets.sh — run named make targets and record each to the battery
# results.tsv, regenerating MATRIX.md after every one.
#
# For re-running a target out of band after a fix, without hand-editing the TSV
# and without re-running the whole battery around it. Every target here
# provisions its own stack, so each gets a teardown first: `e2e-clean-data`
# hard-stops while a previous stack still mounts node_data.
#
#   RESULTS_DIR=e2e-results/167-<ts> ITER=1 LOG_PREFIX=rerun \
#     ./scripts/e2e-run-targets.sh e2e-claim-watcher-synthesis e2e-recovery-readiness
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

R="${RESULTS_DIR:?set RESULTS_DIR to the battery results directory}"
ITER="${ITER:-1}"
PREFIX="${LOG_PREFIX:-rerun}"
mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'iter\ttarget\tstatus\tsecs\tlog\n' > "$TSV"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)

for t in "$@"; do
    "${BASE_ENV[@]}" make e2e-down >>"$R/logs/down.log" 2>&1
    log="$R/logs/${PREFIX}-${t}.log"; t0=$SECONDS
    echo "=== $t start $(date -u +%FT%TZ) ===" | tee -a "$R/run-targets.log"
    if "${BASE_ENV[@]}" make "$t" >"$log" 2>&1; then st=PASS; else st=FAIL; fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$ITER" "$t" "$st" "$((SECONDS-t0))" "$log" >> "$TSV"
    ./scripts/e2e-battery-matrix.py "$TSV" > "$R/MATRIX.md" 2>/dev/null
    echo "  $t: $st ($((SECONDS-t0))s) -> $log" | tee -a "$R/run-targets.log"
done
echo "RUN-TARGETS COMPLETE $(date -u +%FT%TZ)" | tee -a "$R/run-targets.log"
