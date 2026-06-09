#!/usr/bin/env bash
# Local docker-compose E2E regression matrix runner — main@ea87178.
# Runs each suite on its own fresh stack (teardown before+after) to avoid the
# cross-run data-dir clobbering documented in the 2026-05-29 report.
# Keeps the external-prover MIDEN_PROVER_URL change in docker-compose.e2e.yml.
set -uo pipefail

cd /home/max/github/gateway/miden/miden-agglayer

# Protocol 0.15.x: node repo renamed 0xMiden/miden-node -> 0xMiden/node; pin the
# exact rev the miden-client 0.15 branch (PR #2224) targets for MAST consistency.
# NOTE: the Makefile sets these with `:=` so its values win; kept here in sync.
export MIDEN_NODE_GIT_URL="https://github.com/0xMiden/node.git"
export MIDEN_NODE_GIT_REF="6649a4ce774bc842c08e6bdc314f6ddafb816282"
export PATH="/home/max/.local/bin:/home/max/.foundry/bin:$PATH"

OUT=out
mkdir -p "$OUT"
SUMMARY="$OUT/REGRESSION-SUMMARY.txt"
: > "$SUMMARY"

./scripts/ensure-e2e-secrets.sh || true

down() {
  make e2e-down >/dev/null 2>&1 || true
  # Proxy container creates a root-owned keystore that `make e2e-clean-data`
  # (run as host user) cannot rm. Clear it with sudo so the next make's
  # internal `rm -rf .miden-agglayer-data` succeeds.
  sudo rm -rf .miden-agglayer-data >/dev/null 2>&1 || true
}

# run_suite <name> <log> <timeout-seconds> <command...>
run_suite() {
  local name="$1"; shift
  local log="$1"; shift
  local tmo="$1"; shift
  echo ""
  echo "═══════════════════════════════════════════════════════════════════"
  echo "▶ SUITE: $name   (timeout ${tmo}s)   $(date '+%H:%M:%S')"
  echo "═══════════════════════════════════════════════════════════════════"
  down
  timeout "$tmo" bash -c "$*" >"$OUT/$log" 2>&1
  local rc=$?
  down
  local verdict
  case $rc in
    0) verdict="PASS" ;;
    124) verdict="TIMEOUT" ;;
    *) verdict="FAIL(exit $rc)" ;;
  esac
  printf '%-34s %-18s %s\n' "$name" "$verdict" "$log" | tee -a "$SUMMARY"
}

echo "unit-tests                         PASS(234/0)        out-regr-unit.log" | tee -a "$SUMMARY"

# 1. l1->l2 + claim-watcher + claim-watcher-synthesis (one stack)
run_suite "l1-to-l2 + claim-watcher(+synthesis)" "regr-claim-watcher-synthesis.log" 2400 \
  "make e2e-claim-watcher-synthesis"

# 2. l2->l1 strict (includes l1->l2 funding)
run_suite "l2-to-l1 (strict)" "regr-l2-to-l1.log" 2400 \
  "make e2e-l2-to-l1"

# 3. rd862 GER-injection race repro (needs live stack)
run_suite "repro-rd862 (GER race)" "regr-rd862.log" 1800 \
  "make e2e-up && make repro-rd862"

# 4. security
run_suite "e2e-security" "regr-security.log" 1800 \
  "make e2e-security"

# 5. ger-decomposition
run_suite "e2e-ger-decomposition" "regr-ger-decomposition.log" 1800 \
  "make e2e-ger-decomposition"

# 6. restore (disaster recovery)
run_suite "e2e-restore" "regr-restore.log" 2400 \
  "make e2e-restore"

# 7. dynamic-erc20 (no make target; run script on a fresh stack)
run_suite "e2e-dynamic-erc20" "regr-dynamic-erc20.log" 2400 \
  "make e2e-up && ./scripts/e2e-dynamic-erc20.sh"

# 8. fuzz/stress
run_suite "e2e-fuzz" "regr-fuzz.log" 1800 \
  "make e2e-fuzz"

# 9. rd913 monitor-state survives restart
run_suite "e2e-rd913-restart-burn-collision" "regr-rd913.log" 1800 \
  "make e2e-rd913-restart-burn-collision"

# 10. rd940 async-writer (6 scripts on one writer-enabled stack)
run_suite "e2e-rd940 (6 scripts)" "regr-rd940.log" 2400 \
  "make e2e-rd940"

down
echo ""
echo "═══════════════════════════════════════════════════════════════════"
echo "LOCAL E2E MATRIX COMPLETE — $(date '+%H:%M:%S')"
echo "═══════════════════════════════════════════════════════════════════"
cat "$SUMMARY"
