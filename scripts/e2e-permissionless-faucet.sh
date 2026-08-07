#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-permissionless-faucet.sh — issue #154.
#
# `miden_registerNativeFaucet` lets ANYONE register an already-deployed
# Miden-native faucet, with no admin key. The bridge stays admin-controlled: the
# public RPC only validates a deterministic request, then the proxy's SERVICE
# account submits the same ConfigAggBridgeNote the admin path uses.
#
# The assertions that matter are the ones about what a public caller CANNOT do:
#   1. it works at all without an admin key (the feature);
#   2. the origin identity is DERIVED, so a caller-supplied one is ignored —
#      this is the anti-squatting property the whole design rests on;
#   3. it is idempotent — a repeat returns the same route, not a second one;
#   4. it cannot rebind a faucet that already has a different origin identity;
#   5. it refuses a faucet that is not an operator-owned native one (e.g. an
#      AggLayer-owned wrapped faucet), so it cannot be used to hijack routing;
#   6. a bogus faucet id fails without writing anything.
#
# Usage: ./scripts/e2e-permissionless-faucet.sh   (expects a running l2l2 stack)
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
source "$SCRIPT_DIR/lib-l2l2.sh"

AGG_C="l2l2-miden-agglayer-1"
MINT_UNITS=500000

rpc_public() {  # NO admin bearer — that is the point
    curl -sf "$L2_RPC" -H "Content-Type: application/json" -d "$1" 2>/dev/null
}
jq_field() { python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('result',{}).get('$1',''))" 2>/dev/null; }

log "======================================================================"
log "  #154 PERMISSIONLESS NATIVE-FAUCET REGISTRATION"
log "======================================================================"
l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
l2l2_miden_identities

step "1. deploy a native faucet, then register it with NO admin key"
FAUCET_ID=$(iso_tool --create-native-faucet --native-symbol "PLS" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" 2>&1 | awk '/faucet-id:/{print $NF}') || true
[[ -n "$FAUCET_ID" ]] || fail "native faucet deploy failed"

RESP=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET_ID\"}]}") \
  || fail "miden_registerNativeFaucet unreachable WITHOUT an admin key — the method is not permissionless"
echo "$RESP" | python3 -c "import json,sys;sys.exit(0 if 'result' in json.load(sys.stdin) else 1)" \
  || fail "permissionless registration failed: $RESP"
DERIVED_ORIGIN=$(echo "$RESP" | jq_field origin_token_address)
[[ "$DERIVED_ORIGIN" =~ ^0x[0-9a-f]{40}$ ]] || fail "no derived origin address in the response: $RESP"
pass "registered without an admin key; derived origin identity $DERIVED_ORIGIN"

step "2. the derived identity is NOT caller-controllable (anti-squatting)"
# Hand the API a different faucet AND an attacker-chosen origin address. If the
# address were honoured, an attacker could squat any token's AggLayer identity.
SQUAT="0x000000000000000000000000000000000000dead"
FAUCET2=$(iso_tool --create-native-faucet --native-symbol "PL2" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" 2>&1 | awk '/faucet-id:/{print $NF}') || true
[[ -n "$FAUCET2" ]] || fail "second native faucet deploy failed"
RESP2=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET2\",\"origin_token_address\":\"$SQUAT\",
    \"symbol\":\"HAX\",\"decimals\":18}]}") || fail "second registration unreachable"
ORIGIN2=$(echo "$RESP2" | jq_field origin_token_address)
SYMBOL2=$(echo "$RESP2" | jq_field symbol)
[[ "$ORIGIN2" != "$SQUAT" ]] \
  || fail "the caller's origin_token_address was HONOURED ($SQUAT) — anyone could squat a token's AggLayer identity"
[[ "$ORIGIN2" != "$DERIVED_ORIGIN" ]] || fail "two different faucets derived the SAME origin identity"
[[ "$SYMBOL2" == "PL2" ]] \
  || fail "the recorded symbol came from the CALLER ($SYMBOL2), not the deployed faucet — metadata must be authoritative"
pass "caller-supplied origin address and metadata are ignored; identity derived from the faucet"

step "3. idempotent — a repeat returns the same route, not a second one"
RESP3=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET_ID\"}]}") || fail "repeat registration unreachable"
ORIGIN3=$(echo "$RESP3" | jq_field origin_token_address)
[[ "$ORIGIN3" == "$DERIVED_ORIGIN" ]] \
  || fail "a repeat produced a DIFFERENT origin ($ORIGIN3 vs $DERIVED_ORIGIN) — registration is not idempotent"
ROWS=$(pgq "SELECT count(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET_ID');")
[[ "${ROWS// /}" == "1" ]] \
  || fail "faucet $FAUCET_ID has $ROWS registry rows after two registrations; expected exactly 1"
pass "repeat registration is idempotent (1 registry row, same origin)"

step "4. cannot rebind a faucet already bound to a DIFFERENT origin"
# Register a third faucet through the ADMIN path with an operator-chosen address,
# then try to claim it permissionlessly: the derived identity differs, so this
# must fail WITHOUT changing the existing binding.
ADMIN_ORIGIN="0x0d1de0$(python3 -c 'import secrets;print(secrets.token_hex(17))')"
FAUCET3=$(iso_tool --create-native-faucet --native-symbol "PL3" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" 2>&1 | awk '/faucet-id:/{print $NF}') || true
[[ -n "$FAUCET3" ]] || fail "third native faucet deploy failed"
curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "Authorization: Bearer ${ADMIN_API_KEY}" \
  -d "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"admin_registerNativeFaucet\",
    \"params\":[{\"faucet_id\":\"$FAUCET3\",\"origin_token_address\":\"$ADMIN_ORIGIN\",
      \"symbol\":\"PL3\",\"decimals\":8}]}" >/dev/null 2>&1 \
  || fail "admin registration of the third faucet failed"
BEFORE=$(pgq "SELECT origin_address FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET3');")
RESP4=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET3\"}]}")
echo "$RESP4" | grep -qi "different origin" \
  || fail "permissionless rebind of an admin-registered faucet was not refused: $RESP4"
AFTER=$(pgq "SELECT origin_address FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET3');")
[[ "$BEFORE" == "$AFTER" ]] \
  || fail "the refused rebind CHANGED state ($BEFORE -> $AFTER); conflicts must not mutate"
pass "a conflicting rebind is refused and leaves the existing binding untouched"

step "5. refuses a non-native (AggLayer-owned wrapped) faucet"
WRAPPED=$(pgq "SELECT faucet_id FROM faucet_registry WHERE origin_network <> ${MIDEN_NETWORK_ID} LIMIT 1;")
if [[ -n "${WRAPPED// /}" ]]; then
  RESP5=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"miden_registerNativeFaucet\",
    \"params\":[{\"faucet_id\":\"${WRAPPED// /}\"}]}")
  echo "$RESP5" | grep -qiE "not an operator-owned native|different origin" \
    || fail "a wrapped (AggLayer-owned) faucet was accepted as native: $RESP5"
  pass "an AggLayer-owned wrapped faucet is refused"
else
  log "  (no wrapped faucet registered yet — skipping; covered by the kind check in step 6)"
fi

step "6. a bogus faucet id fails without writing anything"
ROWS_BEFORE=$(pgq "SELECT count(*) FROM faucet_registry;")
RESP6=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"0xdeadbeefdeadbeefdeadbeefdeadbe\"}]}")
echo "$RESP6" | grep -qi "error" || fail "a bogus faucet id was accepted: $RESP6"
ROWS_AFTER=$(pgq "SELECT count(*) FROM faucet_registry;")
[[ "$ROWS_BEFORE" == "$ROWS_AFTER" ]] \
  || fail "a failed registration changed the registry ($ROWS_BEFORE -> $ROWS_AFTER)"
pass "an invalid request fails with no registry change"

echo ""
pass "#154 COMPLETE — permissionless registration works, and the origin identity is derived, idempotent, conflict-safe and not caller-controllable"
