#!/usr/bin/env bash
# e2e-l2l2-forward.sh — SIMPLE, DETERMINISTIC L2<->L2 forward scenario
# ("l2-to-l2-forward") — the canonical forward leg of the l2l2 group (legs 1+2+2b).
#
#   deploy OPT0 on L2B (origin_network = 2, NOT L1)
#   -> bridgeAsset(destNet=1) L2B -> Miden
#   -> GER propagation (cert settle -> L1 GER -> Miden aggoracle)
#   -> submit a proof-backed claimAsset to the Miden proxy (canonical client claim:
#      per-rollup isolation means the Miden service doesn't index L2B, so nothing
#      auto-claims — fetch the L2B proof from the L2B service, submit to the proxy)
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
# Remove any leftover state from a prior run up front: if THIS forward dies before
# it atomically writes a fresh file, a stale one must not be consumed by clash/back.
rm -f "$STATE_FILE" "$STATE_FILE".tmp.* 2>/dev/null || true

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

# ── Leg 2b: claim on Miden — canonical client-submitted claimAsset ───────────
# With per-rollup bridge-service isolation (canonical kurtosis: one service per
# chain, each indexing L1 + ONLY its own L2) the Miden service does NOT index L2B,
# so nothing auto-submits this forward claim — the shared service that auto-claimed
# it was the non-canonical shortcut. The canonical flow (a bridge client, and our
# back leg): fetch the L2B deposit's proof from the L2B service and submit a
# proof-backed claimAsset to the Miden proxy, which accepts it
# (worker_handle_claim_asset) and auto-creates the foreign faucet + mints.
step "Leg 2b: submit proof-backed claimAsset to Miden proxy + (OPT0, net $L2B_NETWORK_ID) faucet asserts"
# Do not passively wait for ready_for_claim: nudge_until below is the bounded
# recovery when bridge-service indexes the L2 GER just before the matching L1 GER.
wait_for "L2B->Miden deposit indexed in bridge-service" 120 5 \
    _pred_deposit_indexed "$DEST_ADDR" "$L2B_NETWORK_ID" "$OPT0_LOWER" "$L2B_BRIDGE_SERVICE_URL"
FWD_DEPOSIT=$(find_deposit "$DEST_ADDR" $L2B_NETWORK_ID "$OPT0_LOWER" "$L2B_BRIDGE_SERVICE_URL")
[[ -n "$FWD_DEPOSIT" ]] || fail "forward deposit vanished from bridge-service"
FWD_GI=$(dep_field "$FWD_DEPOSIT" global_index)
FWD_CNT=$(dep_field "$FWD_DEPOSIT" deposit_cnt)
FWD_ORIG_NET=$(dep_field "$FWD_DEPOSIT" orig_net)
FWD_DEST_NET=$(dep_field "$FWD_DEPOSIT" dest_net)
FWD_DEST_ADDR=$(dep_field "$FWD_DEPOSIT" dest_addr)
FWD_METADATA=$(echo "$FWD_DEPOSIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
log "  forward deposit: cnt=$FWD_CNT globalIndex=$FWD_GI origNet=$FWD_ORIG_NET destNet=$FWD_DEST_NET"

# Submit the claimAsset to the Miden proxy, retrying with a FRESH proof each round
# while nudge_cert drives L2B cert cycles so Miden sees the covering GER (proxy C6
# has_seen_ger gate). The predicate reports success when the synthetic ClaimEvent
# for this globalIndex lands — the on-Miden proof of a completed claim.
nudge_until "forward claimAsset accepted on Miden (ClaimEvent for globalIndex $FWD_GI)" \
    _pred_submit_forward_claim "$FWD_CNT" "$FWD_GI" "$FWD_ORIG_NET" "$OPT0" "$FWD_DEST_NET" "$FWD_DEST_ADDR" "$FWD_AMOUNT_WEI" "$FWD_METADATA" \
    || fail "forward claimAsset never accepted on the Miden proxy (globalIndex $FWD_GI) despite repeated nudges"

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

# (b') #147 — the wrapped OPT0 faucet must expose wallet-resolvable metadata. A fresh
# client resolves the display symbol/decimals from the public faucet ACCOUNT. The proxy
# creates the faucet symbol via faucet_ops::sanitise_token_symbol (Miden TokenSymbol
# keeps only A-Z), so the on-chain OPT0 symbol resolves to "OPT" (the digit is stripped)
# with decimals min(18,8)=8; identity is exact (origin_network=$L2B_NETWORK_ID, OPT0).
assert_faucet_symbol "$OPT0_FAUCET_ID" "OPT" "8" "L2B OPT0 ERC-20 (origin net=$L2B_NETWORK_ID, addr=$OPT0)"

# (c) ClaimEvent row exists for this deposit's global index.
CLAIM_ROWS=$(claim_event_rows "$FWD_GI")
[[ "${CLAIM_ROWS:-0}" -ge 1 ]] || fail "no ClaimEvent synthetic_logs row for globalIndex $FWD_GI"
FWD_CLAIM_BLOCK=$(pgq "SELECT block_number FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x$(python3 -c "print(format(int('$FWD_GI'),'064x'))")%' ORDER BY block_number LIMIT 1;")
pass "ClaimEvent at synthetic block ${FWD_CLAIM_BLOCK:-?} (rows=$CLAIM_ROWS)"
# CLAIM (forward): client-submitted proof-backed claimAsset to the Miden proxy (see
# leg 2b). The resulting Miden claim is an internal Miden tx (no cast receipt); it is
# verified by the ClaimEvent synthetic_logs row above.
evidence_record "leg2b" forward Miden claim "" "${FWD_CLAIM_BLOCK:-}" "$BRIDGE_ID" \
    "ClaimEvent-present rows=$CLAIM_ROWS" "globalIndex=$FWD_GI faucet=$OPT0_FAUCET_ID units=$FWD_MIDEN_UNITS"
evidence_exit_root "leg2b" forward post-forward-claim

# ── #147 Leg 3 (NEW): L2B NATIVE ETH → Miden — the missing native-L2B leg ─────
# Bridge NATIVE ETH (address(0), metadata 0x, msg.value) L2B → Miden, claim on the
# proxy, and assert the received faucet resolves ETH/8 from public account state on
# a fresh wallet AND is a DISTINCT faucet from the L1 native-ETH faucet: (0,0x0) and
# ($L2B_NETWORK_ID,0x0) are different origins even though both display ETH.
#
# ENV-GATED (RUN_L2B_NATIVE_ETH=1, default OFF). #77 root cause (was mis-attributed
# to gasTokenAddress=0x0): with a CUSTOM gas token configured (setup-l2b.sh deploys
# L2BGAS on L1 + sets it as the L2B bridge gasTokenAddress, WETH auto-deployed),
# `bridgeAsset(destNet, address(0), amount, --value=amount)` STILL reverts — with
# custom error 0x14603c01 = LocalBalanceTreeUnderflow(originNet, gasTokenAddress,
# amount, currentBalance). Cause: the L2B sovereign bridge tracks a Local Balance
# Tree and only lets an origin token be bridged OUT up to what was bridged IN. The
# chain's genesis-minted native gas balance is NOT LBT-backed (currentBalance=0), so
# the FIRST out-bridge underflows. Fix: seed LBT(0, gasTokenAddress) by bridging the
# gas token L1->L2B + claiming it on L2B (seed_l2b_gastoken_lbt) BEFORE bridging out.
#
# HEADLINE (#147/#77) observable: the gas token's on-chain gasTokenMetadata
# (name/symbol/decimals) propagates L2B deposit -> Miden faucet, but Miden's
# TokenSymbol is A-Z ONLY, so `sanitise_token_symbol` DROPS digits/punctuation:
# "L2BGAS" -> Miden display symbol "LBGAS" (decimals 18 -> cap 8). This leg asserts
# the SANITISED symbol (deterministic, matches src/faucet_ops.rs) — proving the
# metadata propagates AND documenting the digit-stripping. A bridgeAsset revert must
# fail LOUD with this context, never a cryptic "bridgeAsset failed" or a false PASS.
if [[ "${RUN_L2B_NATIVE_ETH:-0}" == "1" ]]; then
    step "#147 Leg 3: bridge native ETH L2B -> Miden + assert ETH/8, distinct from L1 ETH faucet"
    ZERO_ADDR="0x0000000000000000000000000000000000000000"
    ETH_WEI="${L2B_ETH_WEI:-1000000000000000}"   # 0.001 ETH
    # ── #77 model: L2B is a CUSTOM-GAS-TOKEN sovereign chain. Bridging its NATIVE
    # currency (address(0) locally) does NOT emit an ETH/(net2,0x0) deposit — the
    # bridge stamps the deposit with the GAS TOKEN's origin + its on-chain
    # `gasTokenMetadata = abi.encode(name, symbol, decimals)` (set at chain init).
    # So the Miden faucet is keyed by the gas token's ORIGIN tuple
    # (gasTokenNetwork, gasTokenAddress) and carries the gas token's SYMBOL — this
    # is the headline #147/#77 observable: a NON-ERC-20-symbol()-resolvable token
    # whose display metadata must still propagate L2B → deposit metadata → the
    # Miden wrapped faucet, and resolve on a fresh wallet (never "Unknown").
    #
    # The setup (setup-l2b.sh / create_rollup_parameters.json) deploys the gas
    # token on L1 and sets these; the e2e reads them (defaults match the setup):
    GAS_ORIGIN_NET="${L2B_GAS_ORIGIN_NET:-0}"                      # gasTokenNetwork (L1-origin)
    GAS_TOKEN_ADDR="${L2B_GAS_TOKEN_ADDR:?set L2B_GAS_TOKEN_ADDR to the L1 gas-token ERC-20 (== bridge gasTokenAddress)}"
    GAS_ORIGIN_SYMBOL="${L2B_GAS_SYMBOL:-L2BGAS}"                  # symbol in gasTokenMetadata (origin)
    # Miden's TokenSymbol is A-Z ONLY: sanitise_token_symbol (src/faucet_ops.rs) keeps
    # only uppercased ASCII letters, DROPPING digits/punctuation. Mirror it here so the
    # assertion expects the ACTUAL Miden display symbol (e.g. "L2BGAS" -> "LBGAS"). If
    # sanitisation empties the symbol the proxy falls back to T+addr; assert that too.
    GAS_MIDEN_SYMBOL="$(echo "$GAS_ORIGIN_SYMBOL" | tr 'a-z' 'A-Z' | tr -cd 'A-Z' | cut -c1-6)"
    if [[ -z "$GAS_MIDEN_SYMBOL" ]]; then
        GAS_MIDEN_SYMBOL="T$(echo "${GAS_TOKEN_ADDR#0x}" | tr 'A-F' 'a-f' | cut -c1-4 | tr 'a-z' 'A-Z')"
    fi
    GAS_ORIGIN_DECIMALS="${L2B_GAS_DECIMALS:-18}"                   # gas-token decimals (18 exercises the min(,8) cap)
    GAS_EXPECT_DECIMALS=$(( GAS_ORIGIN_DECIMALS < 8 ? GAS_ORIGIN_DECIMALS : 8 ))
    GAS_ADDR_HEX="$(echo "${GAS_TOKEN_ADDR#0x}" | tr 'A-F' 'a-f')"       # no 0x — for pg encode(origin_address,'hex')
    GAS_ADDR_LOWER="$(echo "$GAS_TOKEN_ADDR" | tr 'A-F' 'a-f')"          # WITH 0x — find_deposit exact-compares orig_addr
    # Sanity: the bridge must actually be a custom-gas-token chain (finding #77 fix
    # applied). If gasTokenAddress is still 0x0, the sovereign chain is misconfigured.
    ONCHAIN_GAS=$(cast call "$BRIDGE" 'gasTokenAddress()(address)' --rpc-url "$L2B_RPC" 2>/dev/null | tr 'A-F' 'a-f')
    [[ "$ONCHAIN_GAS" != "0x0000000000000000000000000000000000000000" ]] \
        || fail "#147/leg3: L2B bridge gasTokenAddress is 0x0 — the custom-gas-token setup (finding #77) is not applied; native bridging reverts (0x14603c01). Fix setup-l2b.sh / create_rollup_parameters.json."
    ETH_MIDEN_UNITS=$(( ETH_WEI / 10000000000 ))
    # #77: seed LBT(0, gasTokenAddress) BEFORE the out-bridge (else LocalBalanceTreeUnderflow
    # / 0x14603c01). Seed 4x the out amount so the single out-bridge has ample margin.
    step "#147/leg3: seeding L2B Local Balance Tree for the gas token (bridge L1->L2B + claim on L2B)"
    seed_l2b_gastoken_lbt "$(( ETH_WEI * 4 ))"
    # Bridge the L2B native gas token OUT to Miden (address(0), msg.value == amount).
    # Robust capture: check status==0x1 AND a non-empty tx hash; surface the on-chain
    # revert reason on failure (never a cryptic message, never a false PASS on empty).
    NETH_OUT=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$ETH_WEI" "$ZERO_ADDR" true 0x \
        --value "$ETH_WEI" --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" --json 2>&1) || true
    NETH_TX=$(echo "$NETH_OUT" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    print(d.get('transactionHash','') if str(d.get('status','')) in ('0x1','1','true') else '')
except Exception:
    print('')" 2>/dev/null)
    [[ -n "$NETH_TX" ]] \
        || fail "#147/leg3: L2B native gas-token bridgeAsset OUT failed/reverted after LBT seeding — $(echo "$NETH_OUT" | tr '\n' ' ' | tail -c 240)"
    pass "#147/leg3: L2B native gas token locked (tx $NETH_TX, $ETH_WEI wei)"
    # The deposit was MADE on L2B (network_id=$L2B_NETWORK_ID) and its TOKEN origin is
    # (orig_net=$GAS_ORIGIN_NET, orig_addr=$GAS_ADDR_LOWER). find_deposit filters on
    # network_id (the SOURCE chain, = L2B) and exact-compares orig_addr (WITH 0x).
    wait_for "L2B->Miden gas-token deposit indexed (src net=$L2B_NETWORK_ID token=$GAS_ADDR_LOWER)" 180 5 \
        _pred_deposit_indexed "$DEST_ADDR" "$L2B_NETWORK_ID" "$GAS_ADDR_LOWER" "$L2B_BRIDGE_SERVICE_URL"
    GAS_DEP=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$GAS_ADDR_LOWER" "$L2B_BRIDGE_SERVICE_URL")
    [[ -n "$GAS_DEP" ]] || fail "#147/leg3: L2B gas-token deposit not indexed (src net=$L2B_NETWORK_ID token=$GAS_ADDR_LOWER)"
    GAS_GI=$(dep_field "$GAS_DEP" global_index); GAS_CNT=$(dep_field "$GAS_DEP" deposit_cnt)
    GAS_DNET=$(dep_field "$GAS_DEP" dest_net); GAS_DADDR=$(dep_field "$GAS_DEP" dest_addr)
    GAS_AMT=$(dep_field "$GAS_DEP" amount)
    GAS_META=$(echo "$GAS_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m!='0x' else '0x')")
    log "#147/leg3: deposit metadata (gasTokenMetadata carried in the leaf) = $GAS_META"
    # HEADLINE edge (#147 raison d'être): a gas token that ships EMPTY on-chain
    # metadata is exactly the wallet-'Unknown' failure mode — fail LOUD, don't skip.
    [[ "$GAS_META" != "0x" ]] \
        || fail "#147/leg3: L2B gas token has EMPTY on-chain gasTokenMetadata — a fresh wallet resolves it as 'Unknown'. Set gasTokenMetadata (name/symbol/decimals) at chain init so the symbol propagates."
    nudge_until "L2B gas-token claimAsset accepted on Miden (ClaimEvent gi $GAS_GI)" \
        _pred_submit_forward_claim "$GAS_CNT" "$GAS_GI" "$GAS_ORIGIN_NET" "0x$GAS_ADDR_HEX" "$GAS_DNET" "$GAS_DADDR" "$GAS_AMT" "$GAS_META" \
        || fail "#147/leg3: L2B gas-token claim never accepted on the proxy (gi $GAS_GI)"
    # Resolve the Miden faucet for the gas token's origin tuple + assert it is a
    # DISTINCT faucet from the L1-ETH one (different origin address).
    GAS_FID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '$GAS_ADDR_HEX' AND origin_network = ${GAS_ORIGIN_NET};")
    [[ -n "$GAS_FID" ]] || fail "#147/leg3: no faucet_registry row for the L2B gas token (net $GAS_ORIGIN_NET, addr 0x$GAS_ADDR_HEX)"
    L1_ETH_FID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '0000000000000000000000000000000000000000' AND origin_network = 0;")
    [[ -n "$L1_ETH_FID" && "$GAS_FID" != "$L1_ETH_FID" ]] \
        || fail "#147/leg3: L2B gas-token faucet ($GAS_FID) must be DISTINCT from the L1-ETH faucet ($L1_ETH_FID)"
    # THE HEADLINE ASSERTION: a FRESH client (no preloaded symbol map) fetches the
    # Miden faucet's public metadata and must resolve the gas token's SANITISED symbol
    # (origin "$GAS_ORIGIN_SYMBOL" -> Miden "$GAS_MIDEN_SYMBOL": Miden TokenSymbol is
    # A-Z only, so digits/punctuation are dropped) + min(decimals,8) — NOT 'Unknown'.
    # assert_faucet_symbol prints faucet_id + fetched symbol/decimals on pass AND fail.
    log "#147/leg3: gas-token symbol propagation — origin (net=$GAS_ORIGIN_NET, addr=0x$GAS_ADDR_HEX), faucet=$GAS_FID, origin symbol='$GAS_ORIGIN_SYMBOL' -> Miden display symbol='$GAS_MIDEN_SYMBOL' decimals=$GAS_EXPECT_DECIMALS (origin $GAS_ORIGIN_DECIMALS capped)"
    # Received-asset linkage (PR #152): the wallet must actually hold the bridged units of
    # this gas-token faucet (consumed its P2ID note) AND that faucet resolves the sanitised
    # symbol — not just a registry row. Mirrors leg 2b's balance+symbol check for OPT0.
    assert_received_faucet "$BRIDGE_ID" "$GAS_FID" "$GAS_MIDEN_SYMBOL" "$GAS_EXPECT_DECIMALS" "$ETH_MIDEN_UNITS" "L2B gas token (origin net=$GAS_ORIGIN_NET, addr=0x$GAS_ADDR_HEX)"
    # Cross-check the store row: symbol column = SANITISED display symbol, origin_decimals
    # = origin (18), miden_decimals = cap (8). Constrained to origin_network=$GAS_ORIGIN_NET.
    GAS_ROW=$(pgq "SELECT symbol||'|'||origin_decimals||'|'||miden_decimals FROM faucet_registry WHERE encode(origin_address,'hex')='$GAS_ADDR_HEX' AND origin_network=${GAS_ORIGIN_NET};")
    [[ "$GAS_ROW" == "${GAS_MIDEN_SYMBOL}|${GAS_ORIGIN_DECIMALS}|${GAS_EXPECT_DECIMALS}" ]] \
        || fail "#147/leg3: gas-token registry row mismatch — got '$GAS_ROW' want '${GAS_MIDEN_SYMBOL}|${GAS_ORIGIN_DECIMALS}|${GAS_EXPECT_DECIMALS}'"
    # 3rd defect fix: assert the claim actually LANDED for THIS deposit's global index
    # (>=1 ClaimEvent row for $GAS_GI) — not just that nudge_until returned. (The exact
    # received-amount vs ETH_MIDEN_UNITS is asserted by the received-asset linkage check
    # that derives the faucet from the consumed P2ID note — PR #152 follow-up.)
    GAS_CLAIM_ROWS=$(claim_event_rows "$GAS_GI")
    [[ "${GAS_CLAIM_ROWS:-0}" -ge 1 ]] \
        || fail "#147/leg3: no ClaimEvent row for the gas-token deposit (gi=$GAS_GI) — claim did not land"
    log "#147/leg3: gas-token ClaimEvent rows for gi=$GAS_GI = $GAS_CLAIM_ROWS (expected credit ETH_MIDEN_UNITS=$ETH_MIDEN_UNITS)"
    pass "#147/leg3: L2B gas-token symbol '$GAS_ORIGIN_SYMBOL'->'$GAS_MIDEN_SYMBOL'/$GAS_EXPECT_DECIMALS propagated L2B→Miden (digit-stripped by Miden TokenSymbol) + distinct from L1 ETH ($GAS_FID != $L1_ETH_FID)"
fi

# ── Persist state for the clash + back scenarios (atomic + fingerprinted) ────
# Write to a temp file and atomically rename so a consumer never sees a partial
# file, and stamp the chain fingerprint (L1+L2B chain ids) + this run's id so the
# back/clash legs can reject a state file left over from a DIFFERENT stack/run.
mkdir -p "$B2AGG_STORE_DIR"
L1_CHAINID=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null || echo "?")
L2B_CHAINID=$(cast chain-id --rpc-url "$L2B_RPC" 2>/dev/null || echo "?")
STATE_TMP="$STATE_FILE.tmp.$$"
cat > "$STATE_TMP" <<EOF
# written by e2e-l2l2-forward.sh $(date -u +%Y-%m-%dT%H:%M:%SZ)
STATE_RUN_ID=${EVIDENCE_RUN_TS:-$$}
STATE_L1_CHAINID=$L1_CHAINID
STATE_L2B_CHAINID=$L2B_CHAINID
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
mv -f "$STATE_TMP" "$STATE_FILE"
log "  scenario state -> $STATE_FILE (run=${EVIDENCE_RUN_TS:-$$} L1=$L1_CHAINID L2B=$L2B_CHAINID)"

evidence_summary

log "======================================================================"
log "  L2<->L2 FORWARD PASS"
log "    OPT0 (origin net $L2B_NETWORK_ID): $OPT0"
log "    forward:              $FWD_AMOUNT_WEI wei -> $FWD_MIDEN_UNITS units (gi $FWD_GI)"
log "    foreign-origin faucet: $OPT0_FAUCET_ID"
log "    ClaimEvent block:     ${FWD_CLAIM_BLOCK:-?}"
log "    evidence NDJSON:      $EVIDENCE_FILE"
log "======================================================================"
