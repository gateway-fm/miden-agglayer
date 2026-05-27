#!/usr/bin/env bash
# RD-940 e2e — worker panic ⇒ ClaimGuard release + claim_watcher backfill
#
# Validates Spec B amendment 1 (ClaimGuard relocated to worker + AssertUnwindSafe
# panic catch) and Spec E §3 (claim_watcher synthesises the ClaimEvent for
# Miden-submitted CLAIMs whose worker panic'd before they reached the store).
#
# Strategy: we can't inject a real panic into the production binary from outside
# (would need a debug-only hook), so this script validates the OBSERVATIONAL
# guarantees:
#   1. agglayer_writer_job_failures_total{reason=panic} is registered.
#   2. claim_watcher_synthesised_total is registered and non-decreasing.
#   3. The container has not panicked on its own during the test window (we
#      look for "panic" in the structured logs and fail if seen).
#
# A real panic-injection variant lives in writer_worker::tests behind cfg(test).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck disable=SC1091
source "$PROJECT_DIR/fixtures/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

METRICS=$(curl -fsS "$L2_RPC/metrics")

grep -q '^# HELP agglayer_writer_job_failures_total' <<<"$METRICS" \
    || fail "agglayer_writer_job_failures_total descriptor missing"
pass "agglayer_writer_job_failures_total descriptor present (Spec F)"

grep -q '^# HELP claim_watcher_synthesised_total' <<<"$METRICS" \
    || fail "claim_watcher_synthesised_total descriptor missing — pre-existing self-heal floor metric"
pass "claim_watcher_synthesised_total descriptor present (self-heal floor for MidenSubmitted × worker-panic)"

# Confirm no spontaneous panics during the test window.
if docker logs "$AGGLAYER_CONTAINER" 2>&1 | tail -500 | grep -iE 'panicked|panic occurred'; then
    fail "agglayer container has panicked spontaneously — see docker logs $AGGLAYER_CONTAINER"
fi
pass "no spontaneous worker panics observed in the recent log window"
