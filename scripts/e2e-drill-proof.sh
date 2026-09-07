#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-drill-proof.sh — prove the full-DB-loss drill is REPEATABLE.
#
# The drill had exactly ONE clean comparison before the quiesce rewrite; every
# other attempt died inside the harness, not the product. One green is an
# anecdote. This runs it N times back to back, each on a FRESH L2->L1 fixture
# (torn-down stack -> make e2e-l2-to-l1 -> drill), and records every run to the
# battery results.tsv so the matrix tells the truth about repeatability.
#
#   RESULTS_DIR=e2e-results/167-<ts> ./scripts/e2e-drill-proof.sh [runs]
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

R="${RESULTS_DIR:?set RESULTS_DIR to the battery results directory}"
RUNS="${1:-3}"
mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'iter\ttarget\tstatus\tsecs\tlog\n' > "$TSV"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)

record() { printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$TSV"
           ./scripts/e2e-battery-matrix.py "$TSV" > "$R/MATRIX.md" 2>/dev/null; }

for i in $(seq 1 "$RUNS"); do
    echo "=== DRILL PROOF $i/$RUNS start $(date -u +%FT%TZ) ===" | tee -a "$R/drill-proof.log"

    # A drill baseline must be built LIVE. Tear the stack down first so the
    # fixture is a genuine fresh round trip, never the residue of the run
    # before it (and never a store that is itself restore output — the drill
    # refuses that anyway, via nonce_ledger_rebuilt).
    "${BASE_ENV[@]}" make e2e-down >>"$R/logs/down.log" 2>&1

    log="$R/logs/proof$i-fixture-l2-to-l1.log"; t0=$SECONDS
    if "${BASE_ENV[@]}" make e2e-l2-to-l1 >"$log" 2>&1; then st=PASS; else st=FAIL; fi
    record "$i" "fixture-l2-to-l1" "$st" "$((SECONDS-t0))" "$log"
    echo "  fixture-l2-to-l1: $st ($((SECONDS-t0))s)" | tee -a "$R/drill-proof.log"
    # No fixture, no drill: running it anyway would fingerprint whatever the
    # half-built stack happened to hold and call the result evidence.
    [[ "$st" == PASS ]] || { echo "  SKIPPING drill $i — no fixture" | tee -a "$R/drill-proof.log"; continue; }

    log="$R/logs/proof$i-full-db-loss-recovery.log"; t0=$SECONDS
    if ./scripts/e2e-full-db-loss-recovery.sh >"$log" 2>&1; then st=PASS; else st=FAIL; fi
    record "$i" "full-db-loss-recovery" "$st" "$((SECONDS-t0))" "$log"
    echo "  full-db-loss-recovery: $st ($((SECONDS-t0))s)" | tee -a "$R/drill-proof.log"
    echo "=== DRILL PROOF $i/$RUNS done $(date -u +%FT%TZ) ===" | tee -a "$R/drill-proof.log"
done
echo "DRILL PROOF COMPLETE $(date -u +%FT%TZ)" | tee -a "$R/drill-proof.log"
