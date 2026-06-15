#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-account-reimport.sh — bali "missing GER manager account" repro/heal test
#
# Faithfully reproduces the bali production failure observed 2026-05-18:
#   proxy returns `account data wasn't found for account id <ger_manager>`
#   on every aggoracle insertGlobalExitRoot push, with this state:
#     - bridge_accounts.toml intact
#     - keystore/ intact
#     - store.sqlite3 missing the ger_manager rows
#
# How: take a healthy stack, drop ger_manager rows from the proxy's local
# sqlite (while leaving keystore + toml alone), restart the proxy, then push
# one updateExitRoot via the recovery script. Assert the deposit STAYS stuck
# and the proxy logs the exact bali error.
#
# The script supports two modes:
#   MODE=expect_failure    (default) — repro on `main`, expects bug.
#   MODE=expect_self_heal  — runs the same sequence and expects the fix
#                             branch to recover automatically (no manual reset).
#
# Exit codes:
#   0 — expected behaviour observed (bug repro'd in expect_failure mode, OR
#       self-heal succeeded in expect_self_heal mode)
#   1 — unexpected: bug did not repro / heal did not work
#   2 — pre-flight failed
#
# Requires the stack to already be up (`make e2e-up`).
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="${MODE:-expect_failure}"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yml"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.15.0}"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
L1_GER_ADDRESS="${L1_GER_ADDRESS:-0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674}"
L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA}"
SIGNER_KEY="${SIGNER_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"

PROXY_CONTAINER="${PROXY_CONTAINER:-miden-agglayer-miden-agglayer-1}"
SQLITE_PATH="${SQLITE_PATH:-/var/lib/miden-agglayer-service/store.sqlite3}"
TOML_PATH="${TOML_PATH:-/var/lib/miden-agglayer-service/bridge_accounts.toml}"

DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"
RUN_SUFFIX="$(date +%s)"

if [[ -t 1 ]]; then
  R=$'\033[0;31m'; G=$'\033[0;32m'; Y=$'\033[0;33m'; C=$'\033[0;36m'; B=$'\033[1m'; N=$'\033[0m'
else R=''; G=''; Y=''; C=''; B=''; N=''; fi

# Safety: ALWAYS try to restart the proxy on exit, even on failure mid-Phase 2,
# so a botched run doesn't leave the stack with the proxy down.
PROXY_STOPPED=false
cleanup() {
  if [[ "$PROXY_STOPPED" == "true" ]]; then
    echo "[cleanup] ensuring proxy is back up" >&2
    docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" start miden-agglayer >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

ts()   { date +%H:%M:%S; }
say()  { printf '%s[%s]%s %s\n' "$G" "$(ts)" "$N" "$*"; }
step() { printf '\n%s[%s] %s%s%s\n' "$C" "$(ts)" "$B" "$*" "$N"; }
warn() { printf '%s[%s] WARN:%s %s\n' "$Y" "$(ts)" "$N" "$*"; }
fail() { printf '%s[%s] FAIL:%s %s\n' "$R" "$(ts)" "$N" "$*" >&2; exit 1; }
pass() { printf '%s[%s] PASS:%s %s\n' "$G" "$(ts)" "$N" "$*"; }

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || { fail "cast (foundry) not in PATH"; }
command -v jq >/dev/null   || { fail "jq not in PATH"; }
docker inspect "$PROXY_CONTAINER" >/dev/null 2>&1 \
  || fail "proxy container $PROXY_CONTAINER not found — is the stack up?"

curl -sf "$L1_RPC" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null \
  || fail "L1 not reachable at $L1_RPC"
curl -sf "$L2_RPC" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null \
  || fail "L2 proxy not reachable at $L2_RPC"

# ── Phase 0 — resolve the ger_manager hex account id ──────────────────────────
step "Phase 0 — resolving ger_manager account id"
GER_MANAGER_HEX=$(
  docker logs "$PROXY_CONTAINER" 2>&1 \
    | grep -oE 'deploying ger_manager account 0x[0-9a-f]+' \
    | head -1 | awk '{print $NF}' || true
)
if [[ -z "${GER_MANAGER_HEX:-}" ]]; then
  fail "could not extract ger_manager hex id from proxy init logs — did init run on this stack?"
fi
say "    ger_manager hex id = $GER_MANAGER_HEX"

GER_MANAGER_BECH32=$(docker exec "$PROXY_CONTAINER" awk -F'"' '/^ger_manager/{print $2}' "$TOML_PATH")
say "    ger_manager bech32 = $GER_MANAGER_BECH32"

# ── Phase 1 — baseline: confirm the stack is healthy before we break it ───────
step "Phase 1 — baseline sanity (one L1→L2 deposit must reach ready_for_claim)"
START_CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
DEST_PRE="0x000000000000000000000000${RUN_SUFFIX: -8}deadbeef"
DEST_PRE=$(echo "$DEST_PRE" | head -c 42)
say "    baseline bridgeAsset cnt=$START_CNT dest=$DEST_PRE"
cast send --rpc-url "$L1_RPC" \
  --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST_PRE" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

# Wait up to 30s for aggoracle to push a GER and bridge-service to flip
DEADLINE=$((SECONDS + 45))
while :; do
  R_BASE=$(curl -s "$BRIDGE_SERVICE_URL/bridge?net_id=0&deposit_cnt=$START_CNT" \
    | jq -r '.deposit.ready_for_claim // empty')
  [[ "$R_BASE" == "true" ]] && break
  (( SECONDS >= DEADLINE )) && fail "baseline deposit cnt=$START_CNT did not reach ready_for_claim in 45s — stack is already broken"
  sleep 2
done
pass "baseline deposit cnt=$START_CNT reached ready_for_claim=true"

# ── Phase 2 — surgical break: drop ger_manager rows from sqlite, keep keystore ──
step "Phase 2 — reproducing bali's 'missing account' state"
say "    stopping proxy"
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" stop miden-agglayer >/dev/null
PROXY_STOPPED=true

# Confirm ger_manager EXISTS before delete (proof we found the right row)
EXISTS_BEFORE=$(
  sudo sqlite3 "$PROJECT_DIR/.miden-agglayer-data/store.sqlite3" "SELECT count(*) FROM latest_account_headers WHERE id = '$GER_MANAGER_HEX';" \
    2>/dev/null | tail -1
)
[[ "$EXISTS_BEFORE" == "1" ]] || fail "expected ger_manager row to exist pre-delete (got count=$EXISTS_BEFORE)"
say "    confirmed ger_manager row exists in latest_account_headers (count=1)"

# Delete every row that references this account id, across all relevant tables.
# This mirrors the bali state: the keystore + toml stay, the sqlite state for this
# one account is missing.
say "    deleting ger_manager from sqlite (keystore + bridge_accounts.toml preserved)"
sudo sqlite3 "$PROJECT_DIR/.miden-agglayer-data/store.sqlite3" <<EOSQL 2>&1 | sed 's/^/      sqlite: /' || true
DELETE FROM latest_account_headers     WHERE id         = '$GER_MANAGER_HEX';
DELETE FROM latest_account_assets      WHERE account_id = '$GER_MANAGER_HEX';
DELETE FROM latest_account_storage     WHERE account_id = '$GER_MANAGER_HEX';
DELETE FROM historical_account_headers WHERE account_id = '$GER_MANAGER_HEX';
DELETE FROM historical_account_storage WHERE account_id = '$GER_MANAGER_HEX';
DELETE FROM historical_account_assets  WHERE account_id = '$GER_MANAGER_HEX';
EOSQL

EXISTS_AFTER=$(
  sudo sqlite3 "$PROJECT_DIR/.miden-agglayer-data/store.sqlite3" "SELECT count(*) FROM latest_account_headers WHERE id = '$GER_MANAGER_HEX';" \
    2>/dev/null | tail -1
)
[[ "$EXISTS_AFTER" == "0" ]] || fail "delete failed (count_after=$EXISTS_AFTER)"
say "    confirmed ger_manager row absent post-delete (count=0)"
say "    keystore still has key file:"
docker exec "$PROXY_CONTAINER" ls /var/lib/miden-agglayer-service/keystore/ 2>/dev/null | sed 's/^/      /' || true
say "    bridge_accounts.toml still references ger_manager:"
docker exec "$PROXY_CONTAINER" grep '^ger_manager' "$TOML_PATH" 2>/dev/null | sed 's/^/      /' || true

say "    restarting proxy"
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" start miden-agglayer >/dev/null
PROXY_STOPPED=false

# Wait for healthy
DEADLINE=$((SECONDS + 90))
while :; do
  HEALTH=$(docker inspect -f '{{.State.Health.Status}}' "$PROXY_CONTAINER" 2>/dev/null || echo "none")
  [[ "$HEALTH" == "healthy" ]] && break
  (( SECONDS >= DEADLINE )) && fail "proxy did not become healthy within 90s after restart"
  sleep 2
done
pass "proxy restarted (healthy)"

# ── Phase 3 — trigger a GER push and observe ─────────────────────────────────
step "Phase 3 — pushing a GER via updateExitRoot (mirrors bali's aggoracle attempt)"
LOGS_BEFORE_LINES=$(docker logs "$PROXY_CONTAINER" 2>&1 | wc -l)

# Read CURRENT (M, R) from L1 and submit updateExitRoot.
MAIN=$(cast call "$L1_GER_ADDRESS" "lastMainnetExitRoot()(bytes32)" --rpc-url "$L1_RPC")
ROLL=$(cast call "$L1_GER_ADDRESS" "lastRollupExitRoot()(bytes32)"  --rpc-url "$L1_RPC")
say "    M=$MAIN"
say "    R=$ROLL"

CAST_OUT=$(
  cast send "$L2_GER_ADDRESS" "updateExitRoot(bytes32,bytes32)" "$ROLL" "$MAIN" \
    --rpc-url "$L2_RPC" --private-key "$SIGNER_KEY" \
    --legacy --gas-price 1000000000 --gas-limit 1000000 2>&1 || true
)
echo "$CAST_OUT" | sed 's/^/    /' | tail -10

# Drop a stuck L1→L2 deposit so we can observe whether bridge-service flips it
STUCK_CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
say "    triggering a stuck deposit at cnt=$STUCK_CNT"
cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST_PRE" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

sleep 8  # let bridge-service ingest

# ── Phase 4 — verdict ────────────────────────────────────────────────────────
step "Phase 4 — verdict"
ERR_LINE=$(docker logs "$PROXY_CONTAINER" 2>&1 \
  | tail -n +$((LOGS_BEFORE_LINES + 1)) \
  | grep -E "account data wasn't found for account id $GER_MANAGER_HEX" \
  | head -1 || true)

REIMPORT_LINE=$(docker logs "$PROXY_CONTAINER" 2>&1 \
  | tail -n +$((LOGS_BEFORE_LINES + 1)) \
  | grep -E "account missing from local store, importing from node|reimported account from node" \
  | head -1 || true)

STUCK_READY=$(curl -s "$BRIDGE_SERVICE_URL/bridge?net_id=0&deposit_cnt=$STUCK_CNT" \
  | jq -r '.deposit.ready_for_claim // "indexed?"')

say "    stuck deposit cnt=$STUCK_CNT ready_for_claim=$STUCK_READY"

case "$MODE" in
  expect_failure)
    if [[ -n "$ERR_LINE" && "$STUCK_READY" != "true" ]]; then
      pass "BUG REPRODUCED (expected on \`main\`):"
      printf '      %s\n' "$ERR_LINE"
      pass "deposit stayed stuck (ready=$STUCK_READY) — matches bali"
      exit 0
    fi
    if [[ -z "$ERR_LINE" ]]; then
      fail "expected 'account data wasn't found' in proxy logs but did not see it (got self-heal? maybe fix branch is checked out)"
    else
      fail "saw the error but deposit somehow flipped ready=$STUCK_READY"
    fi
    ;;
  expect_self_heal)
    if [[ "$STUCK_READY" == "true" ]]; then
      pass "SELF-HEAL VERIFIED: stuck deposit flipped ready=true after restart"
      if [[ -n "$REIMPORT_LINE" ]]; then
        pass "and reimport log line present:"
        printf '      %s\n' "$REIMPORT_LINE"
      else
        warn "no reimport log line found; verify the fix path emits a recognisable log"
      fi
      exit 0
    fi
    fail "expected self-heal — deposit cnt=$STUCK_CNT still ready=$STUCK_READY. error line: ${ERR_LINE:-<none>}"
    ;;
  *)
    fail "unknown MODE=$MODE (use expect_failure or expect_self_heal)"
    ;;
esac
