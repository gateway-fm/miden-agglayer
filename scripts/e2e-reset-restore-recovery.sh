#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-reset-restore-recovery.sh — operator-faithful `--reset-miden-store
#                                  --restore` recovery repro
#
# Reproduces the EXACT operator action that flipped bali from IAIC to
# AccountDataNotFound at 2026-05-14 18:45:18Z (per Loki):
#
#   18:45:18Z  pod restart
#   18:45:18Z  reset_miden_store: deleted /var/lib/.../store.sqlite3
#   18:45:18Z  reset_miden_store: removed 1 sqlite file(s); keystore preserved
#   18:45:21Z  pod restart + reset_miden_store again
#   18:45:39Z  pod restart + reset_miden_store again
#
# Pre-`55fa17a` behaviour (the bug):
#   1. baseline L1 deposit lands clean (STATE A)
#   2. operator runs proxy with --reset-miden-store --restore
#      - reset wipes store.sqlite3
#      - restore Phase 1 calls client.sync_state() ONLY — NEVER imports the
#        bridge accounts (latest_account_headers stays empty)
#   3. operator restarts proxy normally
#   4. next aggoracle push → submit_new_transaction → AccountDataNotFound at
#      src/service.rs:180  ("account data wasn't found for account id ...")
#   5. deposit STUCK (ready_for_claim stays false)
#
# Post-`55fa17a` behaviour (the cure):
#   1-3 same setup
#   2'. restore Phase 0 (new) calls reimport_known_accounts BEFORE sync_state:
#       - import_account_by_id for each entry in bridge_accounts.toml
#       - latest_account_headers re-populated for every infrastructure account
#   3'. operator restarts proxy normally
#   4'. next aggoracle push → submit_new_transaction → SUCCESS
#   5'. deposit READY
#
# Runs in two modes, set via MODE env var:
#
#   MODE=expect_failure   — build/image is PRE-55fa17a. Script PASSES if
#                           AccountDataNotFound fires in proxy logs AND the
#                           STATE B deposit stays stuck. FAILS if either of
#                           those don't hold (i.e. the bug isn't reproducing
#                           and you're not actually running pre-55fa17a code).
#
#   MODE=expect_recovery  — build/image is POST-55fa17a. Script PASSES if
#                           the restore Phase 0 logs the reimport pass AND
#                           the post-reset deposit becomes ready_for_claim
#                           within 60s of the operator restart. FAILS if
#                           AccountDataNotFound fires (cure didn't land) or
#                           the deposit stays stuck.
#
# The script does NOT switch branches — that's the operator's job. Build the
# image from the branch you want to test, then run this script with the
# matching MODE.
#
# Evidence captured to /tmp/repro-evidence-reset-restore-${MODE}-${RUN_SUFFIX}.txt
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

MODE="${MODE:-}"
case "$MODE" in
  expect_failure|expect_recovery) ;;
  *) echo "MODE must be 'expect_failure' or 'expect_recovery' (got: '$MODE')" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yml"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

# Required by docker-compose.e2e.yml's miden-node build args (`${VAR:?...}`).
# Without these, even `docker compose run --no-deps miden-agglayer ...` fails at
# compose-file parse time because the whole file is interpolated regardless of
# which service the run targets. Defaults match the Makefile's MIDEN_NODE_GIT_*
# (the source of truth — bump both together).
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/miden-node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.14.10}"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
SIGNER_KEY="${SIGNER_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"
PROXY_CONTAINER="${PROXY_CONTAINER:-miden-agglayer-miden-agglayer-1}"

RUN_SUFFIX="$(date +%s)"
EVIDENCE="/tmp/repro-evidence-reset-restore-${MODE}-${RUN_SUFFIX}.txt"
SQLITE_PATH="$PROJECT_DIR/.miden-agglayer-data/store.sqlite3"

if [[ -t 1 ]]; then
  R=$'\033[0;31m'; G=$'\033[0;32m'; Y=$'\033[0;33m'; C=$'\033[0;36m'; B=$'\033[1m'; N=$'\033[0m'
else R=''; G=''; Y=''; C=''; B=''; N=''; fi

ts()   { date +%H:%M:%S; }
say()  { printf '%s[%s]%s %s\n' "$G" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }
step() { printf '\n%s[%s] %s%s%s\n' "$C" "$(ts)" "$B" "$*" "$N" | tee -a "$EVIDENCE"; }
warn() { printf '%s[%s] WARN:%s %s\n' "$Y" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }
fail() { printf '%s[%s] FAIL:%s %s\n' "$R" "$(ts)" "$N" "$*" >&2; printf 'FAIL %s\n' "$*" >>"$EVIDENCE"; exit 1; }
pass() { printf '%s[%s] PASS:%s %s\n' "$G" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }

cleanup() {
  # Restart proxy if we stopped it and never brought it back. Idempotent.
  docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" start miden-agglayer >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── Helpers ───────────────────────────────────────────────────────────────────
depo() {
  curl -s "$BRIDGE_SERVICE_URL/bridge?net_id=0&deposit_cnt=$1" \
    | jq -r 'if .deposit then (.deposit.ready_for_claim | tostring) else "<missing>" end'
}

wait_for_proxy_healthy() {
  local timeout="${1:-90}"
  local deadline=$((SECONDS + timeout))
  while :; do
    [[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY_CONTAINER" 2>/dev/null || echo none)" == "healthy" ]] && return 0
    (( SECONDS >= deadline )) && return 1
    sleep 2
  done
}

count_account_rows() {
  # Returns number of rows in latest_account_headers. Uses sudo because the
  # docker bind mount makes the file root-owned.
  sudo sqlite3 "$SQLITE_PATH" "SELECT COUNT(*) FROM latest_account_headers;" 2>/dev/null || echo "0"
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null    || fail "cast not in PATH"
command -v jq >/dev/null      || fail "jq not in PATH"
command -v sqlite3 >/dev/null || fail "sqlite3 not in PATH"
docker inspect "$PROXY_CONTAINER" >/dev/null 2>&1 \
  || fail "proxy container $PROXY_CONTAINER not found — run 'make e2e-up' first"

printf '## evidence run %s, MODE=%s\n' "$RUN_SUFFIX" "$MODE" >"$EVIDENCE"
say "MODE = $MODE"
say "  expect_failure  → bug fires, AccountDataNotFound observed, deposit stays stuck"
say "  expect_recovery → cure works, Phase 0 reimport logs, deposit becomes ready"

# Tag the test start point in the log stream so all later greps can scope
# to lines after this marker (avoids matching incidental boot logs).
MARKER="reset-restore-repro-${RUN_SUFFIX}"
docker exec "$PROXY_CONTAINER" sh -c "echo '=== MARKER: $MARKER ===' 1>&2" 2>/dev/null || true
LOGS_BEFORE=$(docker logs "$PROXY_CONTAINER" 2>&1 | wc -l)
say "log line count before run: $LOGS_BEFORE"

# ── STATE A: baseline deposit, healthy ───────────────────────────────────────
step "Phase 1 — STATE A: baseline L1→L2 deposit must land clean"
START_CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
DEST_A="0x000000000000000000000000${RUN_SUFFIX: -8}deadbeef"
DEST_A=$(echo "$DEST_A" | head -c 42)
say "    sending baseline bridgeAsset cnt=$START_CNT dest=$DEST_A"
cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST_A" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

deadline=$((SECONDS + 60))
while :; do
  [[ "$(depo "$START_CNT")" == "true" ]] && break
  (( SECONDS >= deadline )) && fail "STATE A baseline did not reach ready_for_claim in 60s (stack may not be healthy)"
  sleep 2
done
pass "STATE A baseline deposit cnt=$START_CNT ready_for_claim=true"

# Snapshot account rows before reset.
BEFORE_ROWS=$(count_account_rows)
say "    latest_account_headers rows BEFORE reset = $BEFORE_ROWS"
[[ "$BEFORE_ROWS" -gt 0 ]] || fail "no accounts in latest_account_headers before reset — init wasn't run?"

# ── Operator action: run with --reset-miden-store --restore ─────────────────
step "Phase 2 — invoke the documented operator recovery: --reset-miden-store --restore"
say "    stopping proxy"
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" stop miden-agglayer >/dev/null

say "    running one-shot: docker compose run --rm --no-deps miden-agglayer --reset-miden-store --restore"
RESTORE_LOG="/tmp/reset-restore-${RUN_SUFFIX}.log"
set +e
# `docker compose run` with --rm clones the service config + entrypoint and
# overrides the command. The container exits after restore completes.
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" \
  run --rm --no-deps miden-agglayer \
  --port=8546 \
  --miden-node=http://miden-node:57291 \
  --miden-store-dir=/var/lib/miden-agglayer-service \
  --l1-rpc-url=http://anvil:8545 \
  --ger-l1-address=0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674 \
  --reset-miden-store \
  --restore \
  >"$RESTORE_LOG" 2>&1
RESTORE_EXIT=$?
set -e
say "    restore one-shot exit code = $RESTORE_EXIT"

# Inspect restore log for the markers that prove which code path ran.
RESET_MARKER=$(grep -c 'reset_miden_store: deleted' "$RESTORE_LOG" || true)
PHASE0_MARKER=$(grep -c 'Phase 0: re-importing bridge accounts' "$RESTORE_LOG" || true)
REIMPORT_MARKER=$(grep -c 'reimported from node' "$RESTORE_LOG" || true)
RESTORE_COMPLETE=$(grep -c 'RESTORE: complete' "$RESTORE_LOG" || true)

say "    restore log markers:"
say "      reset_miden_store deleted   = $RESET_MARKER  (expected: ≥1)"
say "      Phase 0 reimport pass start = $PHASE0_MARKER  ${MODE} → ${MODE/expect_failure/expected 0}${MODE/expect_recovery/expected ≥1}"
say "      'reimported from node'      = $REIMPORT_MARKER  (post-55fa17a: ≥1 per network-tracked account)"
say "      RESTORE: complete           = $RESTORE_COMPLETE  (expected: 1)"

[[ "$RESET_MARKER" -ge 1 ]] || fail "reset_miden_store delete marker missing — did --reset-miden-store run?"
[[ "$RESTORE_COMPLETE" -ge 1 ]] || fail "RESTORE: complete marker missing — restore didn't finish"

# Phase 0 is the load-bearing differentiator between pre-55fa17a and post.
if [[ "$MODE" == "expect_failure" ]]; then
  [[ "$PHASE0_MARKER" -eq 0 ]] || fail "MODE=expect_failure but Phase 0 reimport ran — is 55fa17a in this build?"
else
  [[ "$PHASE0_MARKER" -ge 1 ]] || fail "MODE=expect_recovery but Phase 0 reimport did NOT run — is 55fa17a missing?"
fi

# Inspect the sqlite directly after restore.
AFTER_ROWS=$(count_account_rows)
say "    latest_account_headers rows AFTER reset+restore = $AFTER_ROWS"
case "$MODE" in
  expect_failure)
    # Pre-55fa17a: restore only calls sync_state, which doesn't import unknown
    # accounts. latest_account_headers should be empty (or near-empty).
    [[ "$AFTER_ROWS" -lt "$BEFORE_ROWS" ]] \
      || fail "latest_account_headers did NOT shrink after reset+restore (before=$BEFORE_ROWS after=$AFTER_ROWS) — is reset_miden_store actually running?"
    say "    bug confirmed: latest_account_headers shrunk from $BEFORE_ROWS to $AFTER_ROWS"
    ;;
  expect_recovery)
    # Post-55fa17a: Phase 0 reimport re-populates. Should be close to BEFORE_ROWS
    # (some accounts may not be network-tracked — wallet_hardhat especially —
    # so we allow a small loss; the network-tracked accounts MUST be present).
    [[ "$AFTER_ROWS" -gt 0 ]] \
      || fail "MODE=expect_recovery but latest_account_headers is empty after restore — Phase 0 didn't reimport"
    say "    cure confirmed: latest_account_headers has $AFTER_ROWS rows after restore (down from $BEFORE_ROWS but non-empty)"
    ;;
esac

# ── Bring proxy back up and trigger STATE B deposit ──────────────────────────
step "Phase 3 — restart proxy normally and trigger a follow-up deposit"
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" start miden-agglayer >/dev/null
wait_for_proxy_healthy 90 || fail "proxy did not come back healthy in 90s after restore"
pass "proxy healthy after restart"

# Snapshot logs at this point so subsequent greps see only post-restart lines.
LOGS_AFTER_RESTART=$(docker logs "$PROXY_CONTAINER" 2>&1 | wc -l)

STATE_B_CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
say "    sending follow-up bridgeAsset cnt=$STATE_B_CNT"
cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST_A" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

# ── Observe and assert ───────────────────────────────────────────────────────
step "Phase 4 — observe AccountDataNotFound vs. clean recovery"
deadline=$((SECONDS + 60))
while :; do
  DEPO=$(depo "$STATE_B_CNT")
  ADNF_LINES=$(docker logs "$PROXY_CONTAINER" 2>&1 | tail -n +$((LOGS_AFTER_RESTART + 1)) | grep -c "account data wasn't found" || true)
  if [[ "$DEPO" == "true" || "$ADNF_LINES" -gt 0 ]]; then break; fi
  (( SECONDS >= deadline )) && break
  sleep 3
done

DEPO_FINAL=$(depo "$STATE_B_CNT")
ADNF_FINAL=$(docker logs "$PROXY_CONTAINER" 2>&1 | tail -n +$((LOGS_AFTER_RESTART + 1)) | grep -c "account data wasn't found" || true)

say "    final deposit cnt=$STATE_B_CNT ready_for_claim=$DEPO_FINAL"
say "    AccountDataNotFound log lines since restart = $ADNF_FINAL"

case "$MODE" in
  expect_failure)
    [[ "$DEPO_FINAL" != "true" ]] \
      || fail "MODE=expect_failure but the deposit went ready — is 55fa17a present in this build (it shouldn't be)?"
    [[ "$ADNF_FINAL" -ge 1 ]] \
      || fail "MODE=expect_failure but no AccountDataNotFound observed — bug isn't reproducing"
    pass "BUG REPRODUCED: AccountDataNotFound fired ($ADNF_FINAL times), deposit stuck (ready_for_claim=$DEPO_FINAL)"
    ;;
  expect_recovery)
    [[ "$ADNF_FINAL" -eq 0 ]] \
      || fail "MODE=expect_recovery but AccountDataNotFound fired ($ADNF_FINAL times) — cure didn't land"
    [[ "$DEPO_FINAL" == "true" ]] \
      || fail "MODE=expect_recovery but deposit is not ready (ready_for_claim=$DEPO_FINAL)"
    pass "CURE VERIFIED: zero AccountDataNotFound; deposit cnt=$STATE_B_CNT ready_for_claim=true"
    ;;
esac

step "Done. Evidence captured to $EVIDENCE"
say "summary:"
say "  MODE                       = $MODE"
say "  rows BEFORE reset          = $BEFORE_ROWS"
say "  rows AFTER reset+restore   = $AFTER_ROWS"
say "  Phase 0 reimport marker    = $PHASE0_MARKER (expected ${MODE/expect_failure/0}${MODE/expect_recovery/≥1})"
say "  AccountDataNotFound count  = $ADNF_FINAL (expected ${MODE/expect_failure/≥1}${MODE/expect_recovery/0})"
say "  STATE B deposit ready      = $DEPO_FINAL (expected ${MODE/expect_failure/false}${MODE/expect_recovery/true})"
