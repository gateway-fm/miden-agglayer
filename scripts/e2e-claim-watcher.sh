#!/usr/bin/env bash
# E2E smoke for the CLAIM watcher (src/claim_watcher.rs).
#
# This script is intended to run AFTER a successful L1→L2 e2e (e.g. `make
# e2e-l1-to-l2`). At that point the proxy's normal `eth_sendRawTransaction`
# path has already submitted at least one CLAIM tx and recorded the
# ClaimEvent log. The chain-tail watcher should observe the consumed CLAIM
# on its next sync tick, find that the ClaimEvent already exists in the
# store, mark the note processed, and increment
# `claim_watcher_already_recorded_total` (the dedup-hit counter).
#
# Pass conditions:
#   1. `claim_watcher_already_recorded_total` >= 1
#      OR `claim_watcher_synthesised_total` >= 1 (covers the crash-recovery
#      path if the proxy was killed between submission and ClaimEvent write).
#   2. `claim_watcher_storage_decode_total` == 0
#      AND `claim_watcher_unrecoverable_total` == 0 (no malformed CLAIMs).
#
# This does NOT exercise the foreign-CLAIM path (operator submitting a CLAIM
# via a separate miden-client). That requires a dedicated bypass tool —
# out of scope for the watcher-only PR; see plan.md notes.
#
# Usage:
#   bash scripts/e2e-l1-to-l2.sh
#   bash scripts/e2e-claim-watcher.sh   # this script
#
# Or wire as a follow-on step in `make test-e2e`.
set -euo pipefail

L2_RPC="${L2_RPC:-http://localhost:8546}"
SYNC_WAIT_SECS="${SYNC_WAIT_SECS:-15}"  # one ~5s tick plus headroom

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }

# Pull a Prometheus counter value (single un-labeled sample). Returns 0 if absent.
counter() {
    local name="$1" body value
    # STOPPER on unreachable /metrics (task #26 sweep): pre-fix, a down proxy
    # read as 0 — a baseline taken against a dead endpoint could false-PASS
    # delta assertions. Absent metric stays a legit 0 (never-incremented).
    body=$(curl -sf "${L2_RPC}/metrics") || fail "metrics endpoint unreachable: ${L2_RPC}/metrics"
    value=$(awk -v n="$name" '
        $0 ~ ("^" n " ") { print $2; found=1; exit }
        END { if (!found) print 0 }
    ' <<<"$body")
    echo "${value%.*}"
}

log "Waiting ${SYNC_WAIT_SECS}s for at least one Miden sync tick so the watcher can observe consumed CLAIMs..."
sleep "${SYNC_WAIT_SECS}"

log "Sampling /metrics from ${L2_RPC} ..."
ALREADY=$(counter claim_watcher_already_recorded_total)
SYNTH=$(counter claim_watcher_synthesised_total)
DECODE_ERR=$(counter claim_watcher_storage_decode_total)
UNRECOV=$(counter claim_watcher_unrecoverable_total)

log "  claim_watcher_already_recorded_total = ${ALREADY}"
log "  claim_watcher_synthesised_total      = ${SYNTH}"
log "  claim_watcher_storage_decode_total   = ${DECODE_ERR}"
log "  claim_watcher_unrecoverable_total    = ${UNRECOV}"

if [[ "${ALREADY}" -lt 1 && "${SYNTH}" -lt 1 ]]; then
    fail "watcher never observed a consumed CLAIM (neither already_recorded nor synthesised fired). \
Either the watcher is not running, the prior e2e didn't actually submit a CLAIM, or sync hasn't \
caught up. Try increasing SYNC_WAIT_SECS or check 'docker logs miden-agglayer | grep claim_watcher'."
fi

if [[ "${DECODE_ERR}" -gt 0 ]]; then
    fail "watcher could not decode ${DECODE_ERR} consumed CLAIM(s). Investigate — \
either upstream ClaimNoteStorage layout drifted or a malformed note hit the stack."
fi

if [[ "${UNRECOV}" -gt 0 ]]; then
    fail "watcher reported ${UNRECOV} unrecoverable CLAIM(s). Investigate."
fi

log "claim_watcher smoke OK."
