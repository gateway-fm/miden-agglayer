#!/usr/bin/env bash
# RD-940 / IAIC regression sentinel — CI-friendly fixture builder
#
# `scripts/e2e-iaic-mempool-conflict.sh MODE=expect_no_iaic` is the canonical
# regression test that proves the channel-of-1 invariant is held — IAIC
# (IncorrectAccountInitialCommitment) cannot recur. Today it SKIPs in CI
# because it requires a hand-built CLAIM_REPLAY_FILE: one signed claimAsset
# envelope per line, all targeting the same bridge account from distinct
# globalIndexes, ready to fire concurrently.
#
# This script builds that fixture programmatically against a running stack:
#   1. Seed N L1 deposits (re-uses scripts/deposit.sh).
#   2. Wait for the L2 proxy to observe their GERs.
#   3. Extract the claimAsset calldata each aggsender would submit.
#   4. Sign them with the aggoracle keystore (or a configured replay signer)
#      into N raw transaction envelopes.
#   5. Write them, one per line, to /tmp/iaic-fixture.txt.
#
# Then `e2e-iaic-mempool-conflict.sh CLAIM_REPLAY_FILE=/tmp/iaic-fixture.txt
# MODE=expect_no_iaic PARALLEL=10` runs strict and a regression of the
# channel-of-1 invariant (Spec E) fails the build.
#
# Current state: this script is a SCAFFOLD. The deposit + signing pipeline
# needs the aggsender's keystore + private keys to be exposed in fixtures/,
# which is a deploy-time question (Igor + ops). Tagged as the v1 follow-up
# in docs/operations/runbook.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck disable=SC1091
source "$PROJECT_DIR/fixtures/.env" 2>/dev/null || true

N_DEPOSITS="${N_DEPOSITS:-10}"
FIXTURE_OUT="${FIXTURE_OUT:-/tmp/iaic-fixture.txt}"

YELLOW='\033[0;33m'; NC='\033[0m'
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }

warn "scripts/setup-iaic-fixture.sh — scaffold only (v1.5 follow-up)."
warn ""
warn "To run e2e-iaic-mempool-conflict.sh strict in CI you need:"
warn "  1. Aggsender / aggoracle signing key exposed in fixtures/"
warn "     (currently keystored, not raw-hex)."
warn "  2. A driver that produces N distinct claimAssetCall payloads, signs"
warn "     them with that key, and writes them to \$FIXTURE_OUT one per line."
warn ""
warn "Until then, the IAIC sentinel SKIPs in CI as a documented gap; the"
warn "channel-of-1 invariant is still covered by writer_worker::tests' worker"
warn "dispatch tests + the existing service_send_raw_txn::tests"
warn "r4_followup_concurrent_same_nonce_serialised test."
warn ""
warn "N_DEPOSITS=$N_DEPOSITS FIXTURE_OUT=$FIXTURE_OUT"
exit 0
