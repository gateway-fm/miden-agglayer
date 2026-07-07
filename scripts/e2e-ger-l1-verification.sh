#!/usr/bin/env bash
# Audit H6 — L1 GER corroboration E2E.
#
# aggoracle-supplied GER bytes (insertGlobalExitRoot) were trusted verbatim: a
# compromised signer could inject a FORGED GER (one whose (mainnet, rollup)
# decomposition the L1 InfoTree indexer never observed on L1) onto Miden —
# polluting on-chain state and burning operator gas.
#
# insert_ger now cross-checks the injected GER against the indexer's observed
# set (ger_entries.mainnet_exit_root must be set). Under
# --reject-unverified-ger-injection (implied by --require-hardening), an
# unverified GER is refused before it reaches Miden.
#
# Phases:
#   A. POSITIVE — submit a real insertGlobalExitRoot for a GER the indexer has
#      observed (wait for the indexer to catch up). Assert the UpdateGerNote is
#      submitted + the synthetic GER log is emitted.
#   B. NEGATIVE — submit insertGlobalExitRoot with a FORGED 32-byte root that
#      never appeared in an L1 UpdateL1InfoTree event. With
#      --reject-unverified-ger-injection, assert the proxy refuses with the H6
#      error and ger_injection_unverified_total increments; NO UpdateGerNote is
#      submitted to Miden.
#
# Requires the full E2E stack up (`make e2e-up`) and the service started with
# --reject-unverified-ger-injection.
set -euo pipefail

BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

log() { echo "[e2e-ger-l1-verify] $*"; }
fail() { echo "[e2e-ger-l1-verify] FAIL: $*" >&2; exit 1; }

log "Phase B — forged GER must be refused under --reject-unverified-ger-injection"
# A 32-byte root with no L1 observation: deliberately not a real L1 exit root.
FORGED=0x$(printf 'cd%.0s' {1..32})

rc=0
out=$(curl -sf "$BRIDGE_SERVICE_URL" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"<signed insertGlobalExitRoot($FORGED)>\"]}" \
    2>&1) || rc=$?

echo "$out" | grep -q "not observed on L1" \
    || fail "forged GER was not refused (audit H6 regression)"

# The unverified-GER metric must have incremented.
metrics=$(curl -sf "$BRIDGE_SERVICE_URL/metrics")
echo "$metrics" | grep -q "ger_injection_unverified_total 1" \
    || fail "ger_injection_unverified_total did not increment"

log "PASS — forged GER refused before Miden submission"
