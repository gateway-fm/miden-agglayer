#!/usr/bin/env bash
# Full e2e battery x4 for PR #176 / issue #167.
# Sequential, unattended, resumable. Records every target to results.tsv and
# regenerates MATRIX.md after each one. Continues past a FAIL so the battery
# makes progress; failures are diagnosed and re-run out of band and then marked.
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TS="${BATTERY_TS:-$(date -u +%Y%m%dT%H%M%SZ)}"; R="e2e-results/${BATTERY_TAG:-167}-$TS"; mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'iter\ttarget\tstatus\tsecs\tlog\n' > "$TSV"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)

matrix() { "$PWD/scripts/e2e-battery-matrix.py" "$TSV" > "$R/MATRIX.md" 2>/dev/null; }

down() { "${BASE_ENV[@]}" make e2e-down >>"$R/logs/down.log" 2>&1; }

# run <iter> <label> <keep|fresh> <command...>
run() {
    local iter="$1" label="$2" mode="$3"; shift 3
    local log="$R/logs/i${iter}-${label}.log" t0 t1 rc
    [[ "$mode" == fresh ]] && down
    echo "[$(date -u +%H:%M:%SZ)] i$iter $label START" | tee -a "$R/battery.log"
    t0=$(date +%s)
    "${BASE_ENV[@]}" "$@" > "$log" 2>&1; rc=$?
    t1=$(date +%s)
    local st=PASS; (( rc != 0 )) && st=FAIL
    printf '%s\t%s\t%s\t%s\t%s\n' "$iter" "$label" "$st" "$((t1-t0))" "$log" >> "$TSV"
    matrix
    echo "[$(date -u +%H:%M:%SZ)] i$iter $label $st rc=$rc ($((t1-t0))s)" | tee -a "$R/battery.log"
}

# scenario targets that provision their own stack (need a teardown first)
FRESH_TARGETS=(
  e2e-l1-to-l2 e2e-claim-watcher e2e-claim-watcher-synthesis
  e2e-l2-to-l1 e2e-l2-to-l1-autoclaim e2e-restore
  e2e-cantina6-faucet-identity-restore e2e-cantina10
  e2e-cantina12-getlogs-returns-all e2e-cantina13
  e2e-ger-decomposition e2e-security e2e-fuzz
  e2e-reconciler-private-note e2e-reconciler-cursor-persistence
  e2e-rd913-restart-burn-collision e2e-rd940
)

for iter in $(seq 1 "${ITERATIONS:-4}"); do
  echo "=== ITERATION $iter start $(date -u +%FT%TZ) ===" | tee -a "$R/battery.log"
  down; "${BASE_ENV[@]}" make e2e-clean-data >>"$R/logs/down.log" 2>&1

  # (a) full suite — self-contained (brings up, tests, tears down)
  run "$iter" "test-e2e" fresh make test-e2e

  # (b) scenario targets
  for t in "${FRESH_TARGETS[@]}"; do
      run "$iter" "$t" fresh make "$t"
      # e2e-claim-provenance needs a stack ALREADY up; l1-to-l2 leaves one
      if [[ "$t" == e2e-l1-to-l2 ]]; then
          run "$iter" "e2e-claim-provenance" keep make e2e-claim-provenance
      fi
  done
  # l2l2 group: bring up its own stack, then run the group
  run "$iter" "e2e-l2l2-up" fresh make e2e-l2l2-up
  run "$iter" "e2e-l2l2"    keep  make e2e-l2l2
  # recovery-readiness is DESTRUCTIVE and provisions via e2e-l2l2-up
  run "$iter" "e2e-recovery-readiness" fresh make e2e-recovery-readiness

  # (c) full-DB-loss drill on a stack carrying real round-trip state
  run "$iter" "fixture-l2-to-l1" fresh make e2e-l2-to-l1
  run "$iter" "full-db-loss-recovery" keep ./scripts/e2e-full-db-loss-recovery.sh

  # (d) load + completeness on that stack
  run "$iter" "loadtest-N30" keep env N=30 ./scripts/e2e-bridge-loadtest-isolated.sh
  run "$iter" "verify-event-completeness" keep ./scripts/verify-event-completeness.sh

  # (e) chaos, then the drill again on the messy post-chaos state
  run "$iter" "chaos-soak" keep env N=30 CHAOS_DURATION=300 GARBO_DURATION=300 ./scripts/e2e-chaos-soak.sh
  run "$iter" "full-db-loss-recovery-postchaos" keep ./scripts/e2e-full-db-loss-recovery.sh

  echo "=== ITERATION $iter done $(date -u +%FT%TZ) ===" | tee -a "$R/battery.log"
done
echo "BATTERY COMPLETE $(date -u +%FT%TZ)" | tee -a "$R/battery.log"
