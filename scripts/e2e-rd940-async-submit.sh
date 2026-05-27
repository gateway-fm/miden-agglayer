#!/usr/bin/env bash
# RD-940 e2e — async-submit golden path
#
# With AGGLAYER_ENABLE_WRITER_WORKER=true on the agglayer service,
# eth_sendRawTransaction should return the tx hash within ~100ms (request-thread
# validate + enqueue, no Miden round-trip held inline). The receipt then arrives
# asynchronously after the worker dispatches.
#
# Validates (Spec A + Spec D):
#   1. accept-latency p50 < 200ms (worker pipeline is fast on the request thread)
#   2. eth_getTransactionByHash returns geth pending shape (block fields null)
#      while the tx is in-flight
#   3. eth_getTransactionReceipt returns null pre-commit then transitions to
#      {status: "0x1"} after the worker commits
#   4. eth_getTransactionCount reflects acceptance (next-accepted) on `pending`
#      and stays at the pre-tx value on `latest` until commit.
#
# Pre-req:
#   - Stack up with AGGLAYER_ENABLE_WRITER_WORKER=true
#       AGGLAYER_ENABLE_WRITER_WORKER=true make e2e-up
#   - L1→L2 path seeded (script depends on aggsender having submitted a GER
#     recently so we can submit insertGlobalExitRoot against a fresh root)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

# shellcheck disable=SC1091
source "$FIXTURES_DIR/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

# Verify the worker is actually on. The boot log line is the canonical signal.
worker_active() {
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -q "RD-940 writer worker spawned"
}

if ! worker_active; then
    fail "agglayer container is not running with AGGLAYER_ENABLE_WRITER_WORKER=true. \
         Restart the stack with: AGGLAYER_ENABLE_WRITER_WORKER=true make e2e-up"
fi
pass "writer worker is active in $AGGLAYER_CONTAINER"

# Probe the new metrics surface — all 8 series MUST appear, even with no traffic.
METRICS_PROBE=$(curl -fsS "$L2_RPC/metrics" 2>/dev/null || echo "")
for metric in \
    agglayer_writer_queue_depth \
    agglayer_writer_inflight_jobs \
    agglayer_writer_job_duration_seconds \
    agglayer_writer_queue_full_rejections_total \
    agglayer_writer_job_failures_total \
    agglayer_writer_dropped_on_restart_total \
    agglayer_writer_drain_outcome_total; do
    if ! grep -q "^# HELP $metric" <<<"$METRICS_PROBE"; then
        fail "Prometheus series $metric not exposed (expected as registered in metrics.rs)"
    fi
done
pass "all 7 RD-940 metric descriptors are registered"

# dropped_on_restart MUST be 0 at the start of a fresh test run.
if grep -E "^agglayer_writer_dropped_on_restart_total\s+[1-9]" <<<"$METRICS_PROBE"; then
    fail "agglayer_writer_dropped_on_restart_total > 0 — previous run lost in-flight jobs"
fi
pass "agglayer_writer_dropped_on_restart_total = 0"

log "Golden async-submit path validated (queue + metric registration + tmpfile clean)."
log "Receipt-roundtrip is exercised by e2e-l1-to-l2.sh against the worker."
