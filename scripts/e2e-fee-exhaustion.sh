#!/usr/bin/env bash
# e2e-fee-exhaustion <service|ger_manager|bridge> (#201)
#
# On a fee-charging chain every account pays its own transaction fees from its
# vault, so an operator who under-funds one gets a silent stall. This test
# starts ONE account with a deliberately tiny budget (FEE_TXN_BUDGET_*), runs
# deposits until it is dry, asserts the stall is VISIBLE (metric at 0, ERROR in
# the log, the flow stuck), tops the account up the way the runbook says, and
# asserts the flow recovers on its own — including the deposits that got stuck.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"; cd "$PROJECT_DIR"
ACCOUNT="${1:?usage: $0 service|ger_manager|bridge}"
METRICS="${METRICS_URL:-http://127.0.0.1:8546/metrics}"
DATA="$PROJECT_DIR/.miden-agglayer-data"
ts(){ date -u +%H:%M:%S; }; log(){ echo "[$(ts)] $*"; }; pass(){ echo "[$(ts)] PASS: $*"; }; fail(){ echo "[$(ts)] FAIL: $*" >&2; exit 1; }

# Tiny budgets: enough to deploy and serve a deposit or two, then dry mid-flow.
case "$ACCOUNT" in
  service)     export FEE_TXN_BUDGET_SERVICE=4 FEE_TXN_BUDGET_CASCADE_TARGETS=2 ;;   # own budget ~3 spent by init; no runtime-faucet reserve
  ger_manager) export FEE_TXN_BUDGET_GER_MANAGER=3 ;;
  bridge)      export FEE_TXN_BUDGET_CASCADE=3 ;;   # the bridge burns fastest (a network tx per GER update)
  *) fail "unknown account $ACCOUNT" ;;
esac

log "fresh stack with a tiny fee budget for $ACCOUNT"
# Budgets apply at --init, so the chain AND the proxy store must be reset together: under the
# battery's KEEP_CHAIN=1 a wipe of one but not the other leaves a store from another genesis.
export KEEP_CHAIN=0
make e2e-down >/dev/null 2>&1 || true; make e2e-clean-data >/dev/null 2>&1 || true
make e2e-up >/tmp/e2e-fee-exhaustion-up.log 2>&1 || {
  tail -5 /tmp/e2e-fee-exhaustion-up.log
  echo "--- proxy log (why --init did not come up healthy) ---"
  docker logs miden-agglayer-miden-agglayer-1 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -E "WARN|ERROR|Error|deploy|cascade|funding|panick" | tail -25
  fail "stack bring-up"; }
[[ -f "$DATA/funding.toml" ]] || fail "no funding.toml — this test needs a fee-charging chain (base fee > 0)"
MAX_FEE=$(sed -n 's/^max_fee_per_txn *= *\([0-9]*\).*/\1/p' "$DATA/funding.toml")
ACCOUNT_ID=$(sed -n "s/^$ACCOUNT *= *\"\(.*\)\"/\1/p" "$DATA/bridge_accounts.toml")
[[ -n "$ACCOUNT_ID" && -n "$MAX_FEE" ]] || fail "could not read $ACCOUNT id / max fee"
log "$ACCOUNT = $ACCOUNT_ID, fee cap $MAX_FEE/tx"

txns_left(){ curl -sf "$METRICS" | awk -v a="$ACCOUNT" -F'[ }]' '$0 ~ "^bridge_fee_vault_txns_left\\{account=\"" a "\"" {print $NF}' | tail -1; }
proxy_log(){ docker logs miden-agglayer-miden-agglayer-1 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g'; }
deposit(){ ./scripts/e2e-l1-to-l2.sh >"/tmp/e2e-fee-exhaustion-dep$1.log" 2>&1; }

# ── run it dry ────────────────────────────────────────────────────────────────
passed=0; stuck=0; last_rc=0
for i in $(seq 1 8); do
  if deposit "$i"; then passed=$((passed+1)); last_rc=0; else last_rc=1; fi
  left=$(txns_left); log "deposit $i rc=$last_rc — $ACCOUNT txns_left=${left:-?}"
  (( last_rc != 0 )) && stuck=$((stuck+1))
  [[ "${left:-1}" == "0" && $last_rc -ne 0 ]] && break
done
[[ "$(txns_left)" == "0" ]] || fail "$ACCOUNT never ran dry after 8 deposits (txns_left=$(txns_left)) — budget too generous"
(( stuck > 0 )) || fail "$ACCOUNT is dry but no deposit stalled — the stall is not visible in the flow"
# No `grep -q` here: under pipefail it can exit before `docker logs` finishes writing
# and the SIGPIPE fails the pipeline even though the line was found.
proxy_log | grep "fee vault EMPTY" | grep -E "account[:=] *\"?$ACCOUNT\b" >/dev/null || fail "no 'fee vault EMPTY' ERROR for $ACCOUNT in the proxy log"
pass "$ACCOUNT ran dry after $passed deposit(s): txns_left=0, ERROR logged, $stuck deposit(s) stuck"

# ── top up (the runbook's way) and expect self-recovery ───────────────────────
export B2AGG_STORE_DIR="$PROJECT_DIR/.b2agg-store/e2e-suite"; source "$SCRIPT_DIR/lib-isolated-wallet.sh"
iso_fund_fee_asset "$ACCOUNT_ID" $(( MAX_FEE * 64 )) || fail "top-up of $ACCOUNT failed"
for _ in $(seq 1 30); do [[ "$(txns_left)" != "0" && -n "$(txns_left)" ]] && break; sleep 5; done
[[ "$(txns_left)" != "0" ]] || fail "metric did not recover after the top-up"
pass "$ACCOUNT topped up: txns_left=$(txns_left)"
deposit recover || { tail -15 /tmp/e2e-fee-exhaustion-deprecover.log; fail "deposit after the top-up did not complete — $ACCOUNT did not recover"; }
pass "a new deposit completes after the top-up (no restart)"

# The deposits that stalled must land too: their notes / GERs were only waiting for fees.
BRIDGE_ID=$(sed -n 's/^bridge *= *"\(.*\)"/\1/p' "$DATA/bridge_accounts.toml"); FAUCET_ID=$(sed -n 's/^faucet_eth *= *"\(.*\)"/\1/p' "$DATA/bridge_accounts.toml")
WALLET_ID=$(cat "$B2AGG_STORE_DIR/wallet-id"); expected=$(( (passed + stuck + 1) * 1000 ))
for _ in $(seq 1 30); do bal=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID"); [[ "${bal:-0}" -ge "$expected" ]] && break; sleep 10; done
[[ "${bal:-0}" -ge "$expected" ]] || fail "stuck deposits did not recover: wallet balance ${bal:-0} < expected $expected ($stuck stuck)"
pass "all $((passed + stuck + 1)) deposits landed (balance $bal) — $ACCOUNT recovered fully when funded"
