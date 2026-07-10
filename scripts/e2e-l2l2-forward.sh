#!/usr/bin/env bash
# e2e-l2l2-forward.sh — SIMPLE, DETERMINISTIC L2<->L2 forward scenario
# ("l2-to-l2-forward"), decomposed from e2e-l2-to-l2.sh legs 1+2+2b.
#
#   deploy OPT0 on L2B (origin_network = 2, NOT L1)
#   -> bridgeAsset(destNet=1) L2B -> Miden
#   -> GER propagation (cert settle -> L1 GER -> Miden aggoracle)
#   -> ClaimTxManager auto-claim on Miden
#   -> ASSERT: foreign-origin faucet keyed (OPT0, net 2); wrapped balance
#      credited to the destination wallet; a ClaimEvent row exists for the
#      deposit's global index.
#
# Writes a small state file (OPT0 addr/faucet, wallet, pre-forward L2B balance,
# wrapped amount) so e2e-l2l2-back.sh can round-trip the SAME wrapped token.
#
# Usage: base stack up (make e2e-up), then ./scripts/e2e-l2l2-forward.sh
#        (or ./scripts/e2e-test.sh l2l2)  — the L2B overlay is brought up
#        idempotently (reused if already registered).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Shared store across forward+back: the wallet that receives the wrapped OPT0
# here must be able to spend it in the back scenario.
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-l2l2}"
STATE_FILE="${STATE_FILE:-$B2AGG_STORE_DIR/l2l2-scenario-state.env}"

source "$SCRIPT_DIR/lib-l2l2.sh"

FWD_AMOUNT_WEI="${FWD_AMOUNT_WEI:-1000000000000000}"        # 0.001 OPT0 forward
FWD_MIDEN_UNITS=$((FWD_AMOUNT_WEI / WEI_PER_MIDEN_UNIT))    # 100000 units

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable at $L1_RPC"

log "======================================================================"
log "  L2<->L2 FORWARD (L2B -> Miden): deploy, bridge, claim, foreign faucet"
log "======================================================================"

l2l2_ensure_stack
# Fail-loud preflight before ANY test step (skipped if the e2e-test.sh group
# already ran it this invocation).
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
evidence_init
# rollup #2 registration tx (CreateNewAggchain on L1) + exit-root baseline.
evidence_rollup_register "leg0"
evidence_exit_root "leg0" forward pre-forward
l2l2_miden_identities

# ── Leg 1: deploy OPT0 on L2B (origin_network = 2) ───────────────────────────
step "Leg 1: deploying OPT0 on L2B"
OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
    --private-key "$ADMIN_KEY" --broadcast \
    --constructor-args "L2BToken" "OPT0" 18 "$TOKEN_SUPPLY" 2>&1) || true
OPT0=$(echo "$OUT" | awk '/Deployed to:/{print $NF}')
[[ -n "$OPT0" ]] || fail "OPT0 deploy failed: $(echo "$OUT" | tail -2)"
OPT0_DEPLOY_TX=$(echo "$OUT" | awk '/Transaction hash:/{print $NF; exit}')
OPT0_LOWER=$(echo "$OPT0" | tr 'A-F' 'a-f'); OPT0_HEX="${OPT0_LOWER#0x}"
pass "OPT0 deployed on L2B: $OPT0 (origin network $L2B_NETWORK_ID)"
evidence_tx "leg1" forward L2B deploy "$L2B_RPC" "$OPT0_DEPLOY_TX" "$OPT0" \
    "token=OPT0 originNetwork=$L2B_NETWORK_ID supply=$TOKEN_SUPPLY"
l2l2_deploy_nudge_token
evidence_tx "leg1" forward L2B deploy "$L2B_RPC" "$NDG_DEPLOY_TX" "$NDG" "token=NDG role=cert-nudge"

# ── Leg 2: forward bridgeAsset(destNet=1) + GER propagation ──────────────────
step "Leg 2: bridgeAsset(destNet=$MIDEN_NETWORK_ID, $FWD_AMOUNT_WEI OPT0 wei) on L2B"
L2B_BAL_BEFORE_FORWARD=$(cast call "$OPT0" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
L1GER_PRE=$(cast call "$GER_L1" 'getLastGlobalExitRoot()(bytes32)' --rpc-url "$L1_RPC")
log "  L2B OPT0 holder balance before forward: $L2B_BAL_BEFORE_FORWARD"

cast send "$OPT0" "approve(address,uint256)" "$BRIDGE" "$FWD_AMOUNT_WEI" \
    --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "OPT0 approve on L2B"
TX=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$FWD_AMOUNT_WEI" "$OPT0" true 0x \
    --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "bridgeAsset on L2B failed (status=$STATUS): $TX"
FWD_TX_HASH=$(printf '%s\n' "$TX" | awk '$1=="transactionHash"{print $2; exit}')
# BridgeEvent must be emitted by the canonical AgglayerBridgeL2 proxy.
BE_EMITTER=$(cast receipt "$FWD_TX_HASH" --json --rpc-url "$L2B_RPC" | python3 -c "
import json, sys
r = json.load(sys.stdin)
be = [l for l in r['logs'] if l['topics'][0] == '$BRIDGE_EVENT_TOPIC']
print(be[0]['address'] if be else '')")
[[ "$(echo "$BE_EMITTER" | tr 'A-F' 'a-f')" == "$(echo "$BRIDGE" | tr 'A-F' 'a-f')" ]] \
    || fail "bridgeAsset tx $FWD_TX_HASH has no BridgeEvent from $BRIDGE (emitter: ${BE_EMITTER:-<none>})"
pass "L2B BridgeEvent in tx $FWD_TX_HASH"
evidence_tx "leg2" forward L2B deposit "$L2B_RPC" "$FWD_TX_HASH" "$BRIDGE" \
    "token=$OPT0 amountWei=$FWD_AMOUNT_WEI destNet=$MIDEN_NETWORK_ID destAddr=$DEST_ADDR"

GER_TIMEOUT="${GER_TIMEOUT:-600}"
log "  waiting for GER propagation L2B -> L1 -> Miden (cert settle, <=${GER_TIMEOUT}s)..."
DEADLINE=$(( $(date +%s) + GER_TIMEOUT )); MIDENGER=""; L1GER="$L1GER_PRE"
while [[ "$(date +%s)" -lt "$DEADLINE" ]]; do
    L1GER=$(cast call "$GER_L1" 'getLastGlobalExitRoot()(bytes32)' --rpc-url "$L1_RPC")
    MIDENGER=$(curl -sf "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"zkevm_getLatestGlobalExitRoot","params":[]}' \
        | python3 -c "import json,sys;print(json.load(sys.stdin).get('result',''))" 2>/dev/null || true)
    [[ -n "$MIDENGER" && "$MIDENGER" == "$L1GER" && "$L1GER" != "$L1GER_PRE" ]] && break
    sleep 5; echo -n "."
done
echo ""
[[ -n "$MIDENGER" && "$MIDENGER" == "$L1GER" && "$L1GER" != "$L1GER_PRE" ]] \
    || fail "GER did not propagate to Miden within ${GER_TIMEOUT}s (pre=$L1GER_PRE L1=$L1GER miden=${MIDENGER:-<none>})"
pass "Leg 2 done: cross-L2 GER on Miden: $MIDENGER"
# GER INJECTION (forward): aggoracle (aggkit #1) injects the L1 GER into Miden.
# Miden's GER lives behind the synthetic proxy RPC (not a cast-reachable EVM
# contract), so verification is: the value the proxy reports == the injected GER,
# plus the aggoracle inject-tx id from its logs (best-effort).
# `|| true`: a no-match grep here (the Miden aggoracle may not log this exact GER
# value — Miden's GER arrives via the proxy, not always aggkit-1) must NOT abort
# the run under `set -e -o pipefail`; the record is still made (rpc-verified).
FWD_GER_INJECT_TX=$( ( set +o pipefail; docker logs "${COMPOSE_PROJECT_NAME}-aggkit-1" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' \
    | grep -iE "inject GER transaction.*GER: ${MIDENGER}$" | grep -oE 'ID: 0x[0-9a-fA-F]{64}' | tail -1 | awk '{print $2}' ) || true )
evidence_record "leg2" forward Miden ger_inject "${FWD_GER_INJECT_TX:-}" "" "miden-ger(proxy)" \
    "rpc-verified" "ger=$MIDENGER source=aggoracle(miden) verifiedVia=zkevm_getLatestGlobalExitRoot"
# CERTIFICATE SETTLEMENT (forward): the network-2 (L2B) cert whose settlement on
# L1 carried the forward deposit's exit root into the L1 GER.
evidence_settlement "leg2" forward "${COMPOSE_PROJECT_NAME}-aggkit-l2b-1" "$TEST_START_TIME" 2 || true

# ── Leg 2b: claim on Miden — foreign-origin faucet + wrapped balance ─────────
step "Leg 2b: claim on Miden (auto-claim) + (OPT0, net $L2B_NETWORK_ID) faucet asserts"
wait_for "L2B->Miden deposit ready_for_claim in bridge-service" \
    "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$OPT0_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') and d.get('dest_net')==$MIDEN_NETWORK_ID else 1)\"" \
    600 5
FWD_DEPOSIT=$(find_deposit "$DEST_ADDR" $L2B_NETWORK_ID "$OPT0_LOWER")
[[ -n "$FWD_DEPOSIT" ]] || fail "forward deposit vanished from bridge-service"
FWD_GI=$(dep_field "$FWD_DEPOSIT" global_index)
log "  forward deposit: cnt=$(dep_field "$FWD_DEPOSIT" deposit_cnt) globalIndex=$FWD_GI"

# Deposit ready — force settle cycles until the claim scan picks it up.
# Scope the auto-create gate to THIS run's OPT0 token_address — a bare
# 'auto-creating faucet' grep cross-matches any other foreign token being claimed
# concurrently (e.g. under N=20 address-clash load, or leftover state), letting the
# test false-progress on someone else's claim and then fail the OPT0-specific
# assert below. Match the claim.rs:487 line "…token_address: 0x<OPT0>, symbol:…".
nudge_until "faucet auto-creation for OPT0 (claim scan)" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' | grep -qi \"auto-creating faucet.*token_address: $OPT0\"" \
    || fail "claim scan never picked up the forward deposit (OPT0 $OPT0) despite repeated nudges"
wait_for "claim tx committed on Miden" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    300 5

# (a) Faucet keyed (OPT0, net 2) — RPC view + PG truth must agree.
FAUCETS_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}') || fail "admin_listFaucets unreachable"
OPT0_FAUCET_ID=$(echo "$FAUCETS_JSON" | python3 -c "
import json, sys
for f in json.load(sys.stdin).get('result', []):
    if f.get('origin_address','').lower() == '$OPT0_LOWER' and f.get('origin_network') == $L2B_NETWORK_ID:
        print(f['faucet_id']); break")
[[ -n "$OPT0_FAUCET_ID" ]] || fail "no faucet for (OPT0, net $L2B_NETWORK_ID) in admin_listFaucets"
PG_OPT0_FID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}' AND origin_network = ${L2B_NETWORK_ID};")
[[ -n "$PG_OPT0_FID" ]] || fail "no faucet_registry row for (OPT0, net $L2B_NETWORK_ID) in PG"
[[ "$(echo "$OPT0_FAUCET_ID" | tr 'A-F' 'a-f')" == "$PG_OPT0_FID" ]] \
    || fail "faucet id mismatch RPC=$OPT0_FAUCET_ID vs PG=$PG_OPT0_FID"
# Negative control: (OPT0, net 0) must resolve to NOTHING (key includes network).
OPT0_NET0_ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}' AND origin_network = 0;")
[[ "$OPT0_NET0_ROWS" == "0" ]] || fail "(OPT0, net 0) unexpectedly resolves to a faucet — keying broken"
pass "foreign-origin faucet keyed (OPT0, net $L2B_NETWORK_ID): $OPT0_FAUCET_ID (net-0 lookup empty)"

# (b) Wrapped balance credited to the destination wallet.
BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$OPT0_FAUCET_ID"); BALANCE="${BALANCE:-0}"
    log "  attempt $attempt/15: wrapped OPT0 balance = $BALANCE"
    [[ "$BALANCE" -gt 0 ]] && break
done
[[ "$BALANCE" -eq "$FWD_MIDEN_UNITS" ]] \
    || fail "wrapped balance mismatch: got $BALANCE, expected $FWD_MIDEN_UNITS"
pass "wrapped OPT0 credited: $BALANCE Miden units"

# (c) ClaimEvent row exists for this deposit's global index.
CLAIM_ROWS=$(claim_event_rows "$FWD_GI")
[[ "${CLAIM_ROWS:-0}" -ge 1 ]] || fail "no ClaimEvent synthetic_logs row for globalIndex $FWD_GI"
FWD_CLAIM_BLOCK=$(pgq "SELECT block_number FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x$(python3 -c "print(format(int('$FWD_GI'),'064x'))")%' ORDER BY block_number LIMIT 1;")
pass "ClaimEvent at synthetic block ${FWD_CLAIM_BLOCK:-?} (rows=$CLAIM_ROWS)"
# CLAIM (forward): auto-claimed on Miden. The Miden claim is an internal Miden tx
# (no cast receipt); it is verified by the ClaimEvent synthetic_logs row above.
evidence_record "leg2b" forward Miden claim "" "${FWD_CLAIM_BLOCK:-}" "$BRIDGE_ID" \
    "ClaimEvent-present rows=$CLAIM_ROWS" "globalIndex=$FWD_GI faucet=$OPT0_FAUCET_ID units=$FWD_MIDEN_UNITS"
evidence_exit_root "leg2b" forward post-forward-claim

# ── Persist state for the back scenario ──────────────────────────────────────
mkdir -p "$B2AGG_STORE_DIR"
cat > "$STATE_FILE" <<EOF
# written by e2e-l2l2-forward.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)
OPT0=$OPT0
OPT0_LOWER=$OPT0_LOWER
OPT0_HEX=$OPT0_HEX
OPT0_FAUCET_ID=$OPT0_FAUCET_ID
WALLET_ID=$WALLET_ID
BRIDGE_ID=$BRIDGE_ID
DEST_ADDR=$DEST_ADDR
L2B_BAL_BEFORE_FORWARD=$L2B_BAL_BEFORE_FORWARD
FWD_AMOUNT_WEI=$FWD_AMOUNT_WEI
FWD_MIDEN_UNITS=$FWD_MIDEN_UNITS
EOF
log "  scenario state -> $STATE_FILE"

evidence_summary

log "======================================================================"
log "  L2<->L2 FORWARD PASS"
log "    OPT0 (origin net $L2B_NETWORK_ID): $OPT0"
log "    forward:              $FWD_AMOUNT_WEI wei -> $FWD_MIDEN_UNITS units (gi $FWD_GI)"
log "    foreign-origin faucet: $OPT0_FAUCET_ID"
log "    ClaimEvent block:     ${FWD_CLAIM_BLOCK:-?}"
log "    evidence NDJSON:      $EVIDENCE_FILE"
log "======================================================================"
