#!/usr/bin/env bash
# All Docker operations are stubbed. No live container or chain is contacted.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib-chaos-owned.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
PROJECT_DIR="$TMP"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
CALLS="$TMP/docker-calls"
docker() { printf '%s\n' "$*" >> "$CALLS"; }

# The additive zero-fee manifest must not cause a zero-value funding send.
mkdir -p "$PROJECT_DIR/.miden-agglayer-data"
cat > "$PROJECT_DIR/.miden-agglayer-data/funding.toml" <<'MANIFEST'
fee_faucet_id = "0x1234"
max_fee_per_txn = 0
MANIFEST
iso_fund_fee_asset wallet
[[ ! -e "$CALLS" ]]

_iso_docker_run --rm image command
[[ "$(cat "$CALLS")" == 'run --rm image command' ]]
: > "$CALLS"
ISO_CHAOS_OWNER=test-owned-run
_iso_docker_run --rm image command
[[ "$(cat "$CALLS")" == 'run --label miden.chaos-owner=test-owned-run --rm image command' ]]

# Stopped children are only waited for, never signalled via their numeric PID.
CONTROL="$TMP/stop"
printf '%s\n' '{"cleanup_ok":true}' > "$CONTROL.stopped"
(sleep 0.01) & TEST_PID=$!
chaos_wait_owned TEST_PID "$CONTROL"
[[ -z "$TEST_PID" ]]
chaos_request_stop "$CONTROL"
chaos_wait_owned TEST_PID "$CONTROL"
[[ -z "$TEST_PID" ]]

# Exact run-labelled containers only; query failure is not successful cleanup.
OWNED_ID=aaaaaaaaaaaa
REMOVED="$TMP/removed"
docker() {
    case "$*" in
        'ps -aq --filter label=miden.chaos-owner=test-owned-run') [[ -f "$REMOVED" ]] || echo "$OWNED_ID" ;;
        "rm -f $OWNED_ID") touch "$REMOVED" ;;
        *) echo "unexpected Docker operation: $*" >&2; return 1 ;;
    esac
}
chaos_remove_owned_containers test-owned-run
[[ -f "$REMOVED" ]]
chaos_remove_owned_containers test-owned-run
! chaos_remove_owned_containers somebody-else 2>/dev/null

docker() {
    case "$1" in
        inspect) echo "${CONTAINER_ID:-original}" ;;
        logs)
            [[ "$*" == 'logs --since start-time original' ]] || return 1
            [[ "${LOGS_UNAVAILABLE:-0}" == 0 ]] || return 1
            [[ "${EMIT_ALERT:-0}" == 0 ]] || echo 'CLAIM consumption was not observed within the watchdog threshold'
            echo 'ordinary restart log'
            ;;
        *) return 1 ;;
    esac
}
[[ "$(chaos_claim_alert_count proxy original start-time "$TMP/evidence")" == 0 ]]
EMIT_ALERT=1
[[ "$(chaos_claim_alert_count proxy original start-time "$TMP/evidence")" == 1 ]]
# The pending SQL row may already have been acknowledged/deleted; the emitted
# alert is still found in retained logs across an ordinary container restart.
CONTAINER_ID=replaced
! chaos_claim_alert_count proxy original start-time "$TMP/evidence"
CONTAINER_ID=original; LOGS_UNAVAILABLE=1
! chaos_claim_alert_count proxy original start-time "$TMP/evidence"
printf 'chaos owned-process helper tests passed\n'
