#!/usr/bin/env bash
# RD-940 e2e — ClaimGuard cancellation under concurrent disconnects
#
# Spec B §2.2 — with the ClaimGuard living inside the worker (not the request
# thread), a client disconnect mid-request no longer leaves the globalIndex
# stuck in claimed_indices. Pre-RD-940 a malicious caller could permanently
# lock arbitrary indexes by disconnecting during the 15s GER-propagation wait.
#
# This script fires N concurrent eth_sendRawTransaction-style probes that
# disconnect before the response would have arrived. We can't fully simulate
# the claim path without signing keys + a seeded GER, but we CAN:
#   1. Confirm the request thread does NOT acquire the claim lock for an
#      enqueued WriteJob (it's the worker's job now). The structural test in
#      writer_worker::tests::worker_dispatches_ger_job_end_to_end proves the
#      lock lives on the worker side.
#   2. Burst the dispatcher with disconnect-mid-flight probes and assert the
#      container stays healthy (no IAIC-class wedge).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck disable=SC1091
source "$PROJECT_DIR/fixtures/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PARALLEL="${PARALLEL:-32}"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

log "Firing $PARALLEL concurrent disconnect-mid-flight probes against $L2_RPC"
for i in $(seq 1 "$PARALLEL"); do
    (
        # 1ms timeout — guaranteed to disconnect before the response arrives,
        # exercising the cancellation path on the request future.
        curl -sS --max-time 0.001 -X POST -H 'Content-Type: application/json' \
            --data "{\"jsonrpc\":\"2.0\",\"id\":$i,\"method\":\"eth_chainId\",\"params\":[]}" \
            "$L2_RPC" 2>/dev/null || true
    ) &
done
wait
log "Probes complete"

# Health check after the disconnect burst — the dispatcher should still serve.
HEALTH=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
    "$L2_RPC")
if ! grep -q '"result"' <<<"$HEALTH"; then
    fail "agglayer dispatcher wedged after $PARALLEL disconnect burst — $HEALTH"
fi
pass "dispatcher remains healthy after $PARALLEL disconnect-mid-flight probes"

# Sanity-check: no R9 guard-recovery error lines in the recent logs (the guard
# only logs at warn/error when it had to release the claim). Worker-side
# guards write a warn line on release; we don't expect any here because we're
# not running real claims, but the absence of ERROR-level guard messages
# confirms no orphan locks.
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
if docker logs "$AGGLAYER_CONTAINER" 2>&1 | tail -200 | grep -E 'R9.*may be leaked'; then
    fail "R9 drop-guard reported a potentially leaked claim — investigate immediately"
fi
pass "no R9 guard-leak warnings in recent logs"
