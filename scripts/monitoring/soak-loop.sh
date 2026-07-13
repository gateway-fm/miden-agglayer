#!/bin/bash
# Pre-release soak: alternate CLEAN N=50 and CHAOS N=50 phases until the user stops it
# (touch $SP/SOAK_STOP). Runs on the CURRENT stack (post-upgrade-test: branch image on a
# release-born store — the most production-faithful state we have). Per-phase ledger.
#
# Doctrine: a phase FAIL followed by a clean phase = self-heal = acceptable (recorded).
# HARD STOP only on: getLogs immutability violation, genuine completeness violation
# (watcher VIOLATION), or unrecoverable stack (proxy unhealthy > 10 min).
set -u
SP="${SOAK_DIR:-/tmp/miden-soak}"
mkdir -p "$SP"
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}"
LEDGER="$SP/soak-ledger.tsv"
WOUT="${WATCHER_OUTPUT:-$SP/watch.output}"
IOUT="${IMMUT_OUTPUT:-$SP/immut.output}"
PROXY="${COMPOSE_PROJECT_NAME}-miden-agglayer-1"
ts() { TZ=Europe/Berlin date '+%H:%M:%S'; }
violations() { grep -ac "COMPLETENESS VIOLATION" "$WOUT" 2>/dev/null | head -1; }
immut_bad()  { grep -acE "VIOLATION|CHANGED|mismatch" "$IOUT" 2>/dev/null | head -1; }
tip() { curl -s -m5 -X POST http://127.0.0.1:8546 -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))' 2>/dev/null; }

wait_healthy() {
  local deadline=$((SECONDS + ${1:-600})) a b
  while [ $SECONDS -lt "$deadline" ]; do
    a=$(tip); [ -n "$a" ] && sleep 6 && b=$(tip) && [ -n "$b" ] && [ "$b" -gt "$a" ] && return 0
    sleep 5
  done
  return 1
}

chaos_side() {  # injected during CHAOS phases: 2 restarts + 1 pause, spaced out
  sleep $((90 + RANDOM % 60));  echo "[$(ts)] CHAOS: docker restart $PROXY";  docker restart "$PROXY" >/dev/null 2>&1
  sleep $((120 + RANDOM % 90)); echo "[$(ts)] CHAOS: docker pause 20s";       docker pause "$PROXY" >/dev/null 2>&1; sleep 20; docker unpause "$PROXY" >/dev/null 2>&1
  sleep $((120 + RANDOM % 90)); echo "[$(ts)] CHAOS: docker restart $PROXY";  docker restart "$PROXY" >/dev/null 2>&1
}

[ -f "$LEDGER" ] || echo -e "phase\ttype\tstart\tend\trc\tb2agg(notes/logs/exact/late/miss/defer)\tviolations\timmut\tverdict" > "$LEDGER"
echo "[$(ts)] ════ SOAK LOOP: alternating CLEAN-N50 / CHAOS-N50 until $SP/SOAK_STOP exists ════"

P=${START_PHASE:-0}
while [ ! -f "$SP/SOAK_STOP" ]; do
  P=$((P+1))
  if [ $((P % 2)) -eq 1 ]; then TYPE=CLEAN; else TYPE=CHAOS; fi
  START=$(ts); V0=$(violations); I0=$(immut_bad)
  echo "[$(ts)] ──── phase $P ($TYPE, N=50) ────"

  CPID=""
  if [ "$TYPE" = CHAOS ]; then chaos_side & CPID=$!; fi
  N_L1_FWD=20 N_L1_BACK=20 L2L2_FWD=5 L2L2_BACK=5 \
    ./scripts/e2e-loadtest-mixed.sh > "$SP/soak-phase-$P.log" 2>&1
  RC=$?
  [ -n "$CPID" ] && wait "$CPID" 2>/dev/null

  B2ROW=$(sed 's/\x1b\[[0-9;]*m//g' /tmp/mixed-verify.out 2>/dev/null | grep -a "B2AGG->" | tail -1 | awk '{print $2"/"$3"/"$4"/"$5"/"$6"/"$7}')
  V1=$(violations); I1=$(immut_bad)
  NEWV=$((V1 - V0)); NEWI=$((I1 - I0))

  VERDICT=OK
  [ "$RC" -ne 0 ] && VERDICT=PHASE-FAIL
  [ "$NEWV" -gt 0 ] && VERDICT=STOP-COMPLETENESS
  [ "$NEWI" -gt 0 ] && VERDICT=STOP-IMMUTABILITY
  echo -e "$P\t$TYPE\t$START\t$(ts)\t$RC\t${B2ROW:-?}\t$NEWV\t$NEWI\t$VERDICT" >> "$LEDGER"
  echo "[$(ts)] phase $P: rc=$RC b2agg=${B2ROW:-?} newViolations=$NEWV newImmut=$NEWI → $VERDICT"

  case "$VERDICT" in
    STOP-*) echo "[$(ts)] ████ HARD STOP: $VERDICT ████"; exit 1;;
  esac
  if ! wait_healthy 600; then
    echo -e "$P\t$TYPE\t-\t$(ts)\t-\t-\t-\t-\tSTOP-UNRECOVERABLE" >> "$LEDGER"
    echo "[$(ts)] ████ HARD STOP: stack unrecoverable (tip frozen >10min) ████"; exit 1
  fi
done
echo "[$(ts)] SOAK_STOP found — stopping gracefully after phase $P."
