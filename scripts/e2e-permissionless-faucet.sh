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
#   6. a bogus faucet id fails without writing anything;
#   7. concurrent registrations of the same faucet collapse to ONE row (the
#      bounded, shared single-flight from PR#164 #3 — try_lock sheds, never
#      queues, never double-writes);
#   8. a registration is DURABLE across a proxy restart, and after a local
#      DB loss a re-registration RE-HEALS from the authoritative bridge binding
#      (already_registered, no rebinding note) — the #3 on-chain preflight;
#   9. the derived-origin faucet is actually USABLE: a full Miden->L2B->Miden
#      round-trip (bridge-out + return claim) runs against the origin the
#      permissionless RPC derived (delegated to e2e-miden-origin.sh in
#      REGISTER_MODE=permissionless).
#
# Usage: ./scripts/e2e-permissionless-faucet.sh   (expects a running l2l2 stack)
#   SKIP_PERMISSIONLESS_ROUNDTRIP=1 skips step 9 when the suite already runs the
#   permissionless round-trip as its own tier.
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
# Capture the tool output: piping straight into awk discards the reason a deploy
# failed, which is exactly what made the first run of this test undiagnosable.
_deploy_log=$(mktemp)
iso_tool --create-native-faucet --native-symbol "PLS" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" > "$_deploy_log" 2>&1
FAUCET_ID=$(awk '/faucet-id:/{print $NF}' "$_deploy_log")
if [[ -z "$FAUCET_ID" ]]; then
    echo "─── bridge-out-tool --create-native-faucet output ───"; cat "$_deploy_log"
    echo "─── end tool output ───"; rm -f "$_deploy_log"
    fail "native faucet deploy failed (tool output above)"
fi
rm -f "$_deploy_log"

RESP=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET_ID\"}]}") \
  || fail "miden_registerNativeFaucet unreachable WITHOUT an admin key — the method is not permissionless"
echo "$RESP" | python3 -c "import json,sys;sys.exit(0 if 'result' in json.load(sys.stdin) else 1)" \
  || fail "permissionless registration failed: $RESP"
DERIVED_ORIGIN=$(echo "$RESP" | jq_field origin_token_address)
[[ "$DERIVED_ORIGIN" =~ ^0x[0-9a-f]{40}$ ]] || fail "no derived origin address in the response: $RESP"
pass "registered without an admin key; derived origin identity $DERIVED_ORIGIN"

step "2. the derived identity is NOT caller-controllable (anti-squatting)"
# Re-register the SAME faucet while supplying a hostile origin address AND
# hostile metadata. If either were honoured, a public caller could squat a
# token's AggLayer identity or misdescribe it. Reusing the faucet keeps this
# test to ONE deploy — the "two faucets derive different origins" property needs
# no chain state and is unit-tested (origin_identity_is_deterministic_and_faucet_bound).
SQUAT="0x000000000000000000000000000000000000dead"
RESP2=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET_ID\",\"origin_token_address\":\"$SQUAT\",
    \"symbol\":\"XXX\",\"decimals\":18}]}") || fail "hostile re-registration unreachable"
ORIGIN2=$(echo "$RESP2" | jq_field origin_token_address)
SYMBOL2=$(echo "$RESP2" | jq_field symbol)
[[ "$ORIGIN2" != "$SQUAT" ]] \
  || fail "the caller's origin_token_address was HONOURED ($SQUAT) — anyone could squat a token's AggLayer identity"
[[ "$ORIGIN2" == "$DERIVED_ORIGIN" ]] \
  || fail "the origin changed under hostile input ($DERIVED_ORIGIN -> $ORIGIN2); it must be a pure function of the faucet"
[[ "$SYMBOL2" == "PLS" ]] \
  || fail "the recorded symbol came from the CALLER ($SYMBOL2), not the deployed faucet — metadata must be authoritative"
pass "caller-supplied origin address and metadata are ignored; identity stays $DERIVED_ORIGIN"

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
# Reuse a native faucet the earlier miden-origin tests registered through the
# ADMIN path with an operator-chosen address — its stored origin therefore does
# NOT equal what we would derive, which is exactly the conflict case. No extra
# deploy needed.
ADMIN_REGISTERED=$(pgq "SELECT faucet_id FROM faucet_registry
  WHERE origin_network = ${MIDEN_NETWORK_ID} AND lower(faucet_id) <> lower('$FAUCET_ID') LIMIT 1;")
ADMIN_REGISTERED="${ADMIN_REGISTERED// /}"
if [[ -n "$ADMIN_REGISTERED" ]]; then
  BEFORE=$(pgq "SELECT origin_address FROM faucet_registry WHERE lower(faucet_id)=lower('$ADMIN_REGISTERED');")
  RESP4=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"miden_registerNativeFaucet\",
    \"params\":[{\"faucet_id\":\"$ADMIN_REGISTERED\"}]}")
  # Match on wording that survives the RPC error redactor. It scrubs any
  # ALL-CAPS word of 4+ chars as a suspected env var, so emphasis words are not
  # safe anchors — "DIFFERENT" came back as "<redacted>" and false-failed this.
  echo "$RESP4" | grep -qi "cannot change" \
    || fail "permissionless rebind of an admin-registered faucet was not refused: $RESP4"
  echo "$RESP4" | grep -qi "no state was changed" \
    || fail "the refusal did not state that no state changed: $RESP4"
  AFTER=$(pgq "SELECT origin_address FROM faucet_registry WHERE lower(faucet_id)=lower('$ADMIN_REGISTERED');")
  [[ "$BEFORE" == "$AFTER" ]] \
    || fail "the refused rebind CHANGED state ($BEFORE -> $AFTER); conflicts must not mutate"
  pass "a conflicting rebind is refused and leaves the existing binding untouched"
else
  fail "no admin-registered native faucet found to test the rebind conflict against — this \
assertion must not be silently skipped"
fi

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

step "7. concurrent registrations of the SAME faucet collapse to ONE row (bounded single-flight)"
# PR#164 #3 made admission BOUNDED + shared: try_lock sheds concurrent public
# callers instead of queueing, and the read-decide-write is serialized. Fire many
# simultaneous registrations of ONE fresh faucet and assert the outcome is exactly
# one registry row and one origin — every response must be either a success/
# already_registered OR the explicit "another registration is in progress" shed,
# never a second row and never a rebind.
_cc_log=$(mktemp)
iso_tool --create-native-faucet --native-symbol "CCX" --native-decimals 8 \
    --mint-units 0 --wallet-id "$WALLET_ID" > "$_cc_log" 2>&1
CC_FAUCET=$(awk '/faucet-id:/{print $NF}' "$_cc_log")
[[ -n "$CC_FAUCET" ]] || { cat "$_cc_log"; rm -f "$_cc_log"; fail "concurrent-case faucet deploy failed"; }
rm -f "$_cc_log"
CC_DIR=$(mktemp -d)
for i in $(seq 1 8); do
  ( rpc_public "{\"jsonrpc\":\"2.0\",\"id\":$i,\"method\":\"miden_registerNativeFaucet\",
      \"params\":[{\"faucet_id\":\"$CC_FAUCET\"}]}" > "$CC_DIR/resp.$i" 2>&1 ) &
done
wait
# Every response must be well-formed and either accept or explicitly shed — never
# a raw crash and never a distinct second origin.
CC_ORIGINS=$(mktemp)
for f in "$CC_DIR"/resp.*; do
  body=$(cat "$f")
  if echo "$body" | grep -qiE "in progress|not queued|retry shortly"; then
    continue   # bounded-admission shed — acceptable
  fi
  o=$(echo "$body" | jq_field origin_token_address)
  [[ "$o" =~ ^0x[0-9a-f]{40}$ ]] \
    || fail "a concurrent registration neither succeeded nor shed cleanly: $body"
  echo "$o" >> "$CC_ORIGINS"
done
# All successful responses must agree on ONE derived origin.
DISTINCT=$(sort -u "$CC_ORIGINS" | grep -c . || true)
[[ "$DISTINCT" -le 1 ]] || fail "concurrent registrations produced $DISTINCT distinct origins — single-flight broken"
rm -f "$CC_ORIGINS"; rm -rf "$CC_DIR"
CC_ROWS=$(pgq "SELECT count(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$CC_FAUCET');")
[[ "${CC_ROWS// /}" == "1" ]] \
  || fail "faucet $CC_FAUCET has ${CC_ROWS// /} rows after 8 concurrent registrations; expected exactly 1"
pass "8 concurrent registrations collapsed to exactly 1 row / 1 origin (bounded single-flight holds)"

step "8. durability + DB-loss reconcile — registration survives a proxy restart and re-heals from the bridge"
# The registration must be DURABLE across a proxy crash, and — because the local
# registry can be lost/lagged after a full DB loss while the bridge still binds the
# faucet — a re-registration must NOT emit a duplicate/rebinding note; the #3
# authoritative on-chain preflight must detect the existing bridge binding and
# RECONCILE the local row instead (already_registered=true).
docker restart -t 10 "$AGG_C" >/dev/null 2>&1 || fail "could not restart proxy $AGG_C"
# Wait for the proxy RPC to answer again (a bogus-id call returns 200+error once up).
_up=0
for _i in $(seq 1 60); do
  if rpc_public "{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"miden_registerNativeFaucet\",
      \"params\":[{\"faucet_id\":\"0x00\"}]}" >/dev/null 2>&1; then _up=1; break; fi
  sleep 3
done
[[ "$_up" == "1" ]] || fail "proxy $AGG_C did not come back up after restart"
# Durability: the row registered in step 1 is still present.
SURV=$(pgq "SELECT count(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET_ID');")
[[ "${SURV// /}" == "1" ]] || fail "faucet $FAUCET_ID registration did NOT survive the proxy restart (${SURV// /} rows)"
# DB-loss reconcile: delete the local row (simulate loss/lag), then re-register.
# The bridge still binds the faucet, so the preflight must return already_registered
# and re-materialise the local row WITHOUT a second bridge note.
pgq "DELETE FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET_ID');" >/dev/null 2>&1
GONE=$(pgq "SELECT count(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET_ID');")
[[ "${GONE// /}" == "0" ]] || fail "test setup: local row for $FAUCET_ID was not deleted"
RESP8=$(rpc_public "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"miden_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FAUCET_ID\"}]}") || fail "reconcile re-registration unreachable"
ALREADY=$(echo "$RESP8" | python3 -c "import json,sys;print(json.load(sys.stdin).get('result',{}).get('already_registered',''))" 2>/dev/null)
ORIGIN8=$(echo "$RESP8" | jq_field origin_token_address)
[[ "$ALREADY" == "True" ]] \
  || fail "after DB loss the re-registration must report already_registered=true (bridge preflight), got: $RESP8"
[[ "$ORIGIN8" == "$DERIVED_ORIGIN" ]] \
  || fail "reconcile produced a different origin ($ORIGIN8 vs $DERIVED_ORIGIN)"
RE_ROWS=$(pgq "SELECT count(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$FAUCET_ID');")
[[ "${RE_ROWS// /}" == "1" ]] \
  || fail "DB-loss reconcile did not re-materialise exactly 1 local row (${RE_ROWS// /})"
pass "registration is durable across restart AND re-heals from the authoritative bridge binding after DB loss (no rebind)"

step "9. FULL round-trip: register via miden_registerNativeFaucet, then bridge Miden->L2B->Miden (both directions)"
# The completeness assertion the reviewer required: the registered faucet must be
# USABLE end-to-end, not merely present in the DB. Delegate to the parameterized
# Miden-origin round-trip in PERMISSIONLESS mode, which registers through
# miden_registerNativeFaucet (deriving the origin) and exercises bridge-out +
# return claim. Skipped only when the suite already ran it as its own tier.
if [[ "${SKIP_PERMISSIONLESS_ROUNDTRIP:-0}" == "1" ]]; then
  log "  (SKIP_PERMISSIONLESS_ROUNDTRIP=1 — the suite runs the permissionless round-trip as its own tier)"
else
  REGISTER_MODE=permissionless DEST=l2b bash "$SCRIPT_DIR/e2e-miden-origin.sh" \
    || fail "permissionless Miden->L2B->Miden round-trip failed — the derived-origin faucet is not usable end-to-end"
  pass "permissionless-registered native faucet completed a full Miden->L2B->Miden round-trip (both directions)"
fi

echo ""
pass "#154 COMPLETE — permissionless registration works; the derived origin is idempotent, conflict-safe, \
concurrency-bounded, restart-durable, DB-loss self-healing, and usable for a full bridge round-trip"
