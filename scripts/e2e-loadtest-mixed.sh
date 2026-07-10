#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-loadtest-mixed.sh — TIER 2: MIXED real-traffic loadtest.
#
# Drives, CONCURRENTLY, both directions of BOTH bridges plus a same-address
# clash, then asserts every legitimate event landed exact-block:
#
#   • L1<->Miden bulk load — the proven isolated loadtest
#     (e2e-bridge-loadtest-isolated.sh) in the background (N ops, both
#     directions, parallel L1->L2 + sequential L2->L1).
#   • L2B->Miden forward deposits  (bridgeAsset destNet=1 on the L2B bridge)
#     claimed on Miden (ClaimTxManager + nudge), producing ClaimEvents.
#   • Miden->L2B back bridge-outs  (bridge-out-tool --dest-network 2) claimed on
#     L2B (ClaimTxManager autoclaim + nudge).
#   • ADDRESS CLASH under concurrency: a token deployed at the SAME CREATE
#     address on L1 AND L2B (fresh key, nonce 0), bridged from BOTH origins —
#     assert the two faucets stay DISTINCT (the (addr, origin_network) key).
#
# Verdict (on the settled stack):
#   (1) verify-event-completeness PASS — one cross-check covers net-0/1/2
#       (every bridge-consumed B2AGG/CLAIM/GER note has its synthetic log at the
#       exact consumption block; 0 missing / 0 extra).
#   (2) every submitted L2<->L2 op reached claimed (per-op success).
#   (3) the clash faucets are distinct.
#   (4) 0 proxy store-locks.
#
# Usage: base+L2B stack up, then
#   N=60 L2L2_FWD=2 L2L2_BACK=2 ./scripts/e2e-loadtest-mixed.sh
# (env: SKIP_L1_LOAD=1 to run only the L2<->L2 workload + clash.)
# set -uo pipefail (NOT -e): a single failed op is a MEASURED signal, not an abort.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-loadtest-mixed}"
source "$SCRIPT_DIR/lib-l2l2.sh"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

N="${N:-60}"
L2L2_FWD="${L2L2_FWD:-2}"          # extra L2B->Miden forward deposits during load
L2L2_BACK="${L2L2_BACK:-2}"        # Miden->L2B back bridge-outs during load
SKIP_L1_LOAD="${SKIP_L1_LOAD:-0}"
TOOL_BIN="${TOOL_BIN:-/home/mandrigin/miden-agglayer/target/debug/bridge-out-tool}"

FWD_SEED_WEI="${FWD_SEED_WEI:-5000000000000000}"    # 0.005 MOP -> 500000 units (faucet + pool)
FWD_OP_WEI="${FWD_OP_WEI:-1000000000000000}"        # 0.001 MOP -> 100000 units per fwd op
BACK_OP_UNITS="${BACK_OP_UNITS:-50000}"             # units per back bridge-out
COL_L1_WEI="${COL_L1_WEI:-1000000000000000}"
COL_L2B_WEI="${COL_L2B_WEI:-2000000000000000}"      # distinct amount from L1 origin

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done

MIX_LOG="${MIX_LOG:-/tmp/mixed-l2l2.log}"; : > "$MIX_LOG"
mix() { echo -e "${CYAN}[$(date +%H:%M:%S)] MIX:${NC} $*" | tee -a "$MIX_LOG"; }

log "======================================================================"
log "  MIXED LOADTEST — L1<->Miden (N=$N) + L2<->L2 (fwd=$L2L2_FWD back=$L2L2_BACK) + clash"
log "======================================================================"

l2l2_ensure_stack
l2l2_miden_identities
l2l2_deploy_nudge_token

# ── Seed: deploy MOP on L2B + forward-bridge to create (MOP, net2) faucet + pool
step "Seed: deploy MOP on L2B + forward-bridge $FWD_SEED_WEI wei (faucet + wrapped pool)"
OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
    --private-key "$ADMIN_KEY" --broadcast \
    --constructor-args "MixToken" "MOP" 18 "$TOKEN_SUPPLY" 2>&1)
MOP=$(echo "$OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -n "$MOP" ]] || fail "MOP deploy failed: $(echo "$OUT" | tail -2)"
MOP_LOWER=$(echo "$MOP" | tr 'A-F' 'a-f'); MOP_HEX="${MOP_LOWER#0x}"
log "  MOP: $MOP"

# forward_bridge <amount_wei> — bridgeAsset MOP L2B->Miden, print the deposit gi.
forward_bridge() {
    local amt="$1" tx status
    cast send "$MOP" "approve(address,uint256)" "$BRIDGE" "$amt" \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null 2>&1 || { echo ""; return 1; }
    tx=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$amt" "$MOP" true 0x \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || { echo ""; return 1; }
    echo "ok"
}
forward_bridge "$FWD_SEED_WEI" >/dev/null || fail "seed forward bridge failed"

wait_for "seed L2B->Miden deposit ready_for_claim" \
    "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$MOP_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') else 1)\"" \
    600 5
nudge_until "MOP faucet auto-creation (seed claim scan)" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    || fail "seed claim scan never fired"
# Resolve the (MOP, net2) faucet id from PG once it exists.
wait_for "(MOP, net2) faucet_registry row" \
    "[ -n \"\$(pgq \"SELECT faucet_id FROM faucet_registry WHERE encode(origin_address,'hex')='${MOP_HEX}' AND origin_network=${L2B_NETWORK_ID};\")\" ]" \
    300 5
MOP_FAUCET_ID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${MOP_HEX}' AND origin_network=${L2B_NETWORK_ID};")
[[ -n "$MOP_FAUCET_ID" && "$MOP_FAUCET_ID" == 0x* ]] || fail "could not resolve MOP faucet id (got '$MOP_FAUCET_ID')"
log "  MOP faucet (net 2): $MOP_FAUCET_ID"
# Wait for the seed wrapped pool to land in the wallet.
SEED_UNITS=$((FWD_SEED_WEI / WEI_PER_MIDEN_UNIT))
wait_for "seed wrapped MOP pool credited (>=$SEED_UNITS)" \
    "[ \"\$(iso_wallet_balance '$BRIDGE_ID' '$MOP_FAUCET_ID')\" -ge $SEED_UNITS ]" 300 15
pass "seed done: (MOP,net2) faucet $MOP_FAUCET_ID, wrapped pool >= $SEED_UNITS units"

# ── op counters (files: subshells can't write parent vars) ───────────────────
CNT_DIR="$(mktemp -d)"; trap 'rm -rf "$CNT_DIR"' EXIT
echo 0 > "$CNT_DIR/fwd_sub"; echo 0 > "$CNT_DIR/fwd_ok"
echo 0 > "$CNT_DIR/back_sub"; echo 0 > "$CNT_DIR/back_ok"
echo pending > "$CNT_DIR/clash"
bump() { local f="$CNT_DIR/$1"; echo $(( $(cat "$f") + 1 )) > "$f"; }

# ══════════════════════════════════════════════════════════════════════════════
# DECOUPLED L2<->L2: SUBMIT fast during the load, CONFIRM in a drain phase after
# the L1 bulk load settles. The Miden prover is a single shared bottleneck — an
# L2<->L2 claim queued behind a burst of L1 claims cannot land within a short
# per-op window, so gating each op on its claim inline serialized the workload
# into a ~40-min wait. Submitting all ops up front (they ARE the concurrent mixed
# traffic) and confirming once the prover drains is both faster and a truer test.
# ══════════════════════════════════════════════════════════════════════════════
declare -a FWD_GIS BACK_TAG
COL_HEX=""; BACK_BEFORE0=""

# forward SUBMIT: bridge a chunk L2B->Miden, record its global index once indexed.
l2l2_fwd_submit() {
    local i="$1" gi
    bump fwd_sub
    forward_bridge "$FWD_OP_WEI" >/dev/null || { mix "fwd#$i submit FAILED"; return; }
    # brief wait for bridge-service to index the new deposit (NOT ready — that is
    # cert/prover-driven and confirmed in the drain), then record its gi.
    wait_for "fwd#$i indexed" \
        "[ \"\$(find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$MOP_LOWER' | python3 -c \"import json,sys; print(json.load(sys.stdin).get('global_index',''))\" 2>/dev/null)\" != '' ]" \
        120 5 || { mix "fwd#$i not indexed"; return; }
    gi=$(dep_field "$(find_deposit "$DEST_ADDR" $L2B_NETWORK_ID "$MOP_LOWER")" global_index)
    FWD_GIS+=("$gi"); mix "fwd#$i submitted (gi $gi)"
}

# back SUBMIT: bridge-out wrapped MOP Miden->L2B ADMIN (creates the B2AGG note).
l2l2_back_submit() {
    local i="$1"
    [[ -z "$BACK_BEFORE0" ]] && BACK_BEFORE0=$(cast call "$MOP" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
    bump back_sub
    if iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$MOP_FAUCET_ID" \
        --amount "$BACK_OP_UNITS" --dest-address "$ADMIN" --dest-network "$L2B_NETWORK_ID" >>"$MIX_LOG" 2>&1; then
        BACK_TAG+=("ok"); mix "back#$i bridged out ($BACK_OP_UNITS units -> L2B)"
    else
        mix "back#$i bridge-out FAILED"
    fi
}

# clash SUBMIT: deploy COL at the SAME CREATE addr on L1+L2B, bridge from BOTH.
clash_submit() {
    mix "clash: deploying COL at the same address on L1 + L2B (nonce-0 fresh key)"
    local kout deployer key col_l1 col_l2b tx status
    kout=$(cast wallet new)
    deployer=$(echo "$kout" | awk '/Address:/{print $2}')
    key=$(echo "$kout" | awk '/Private key:/{print $3}')
    # Fund the deployer via anvil_setBalance (NOT an ADMIN cast-send) — the L1
    # bulk load spends the SAME key (ADMIN==FUNDED) on L1, so an ADMIN L1 tx here
    # would race its nonce ("replacement tx underpriced").
    cast rpc anvil_setBalance "$deployer" 0xde0b6b3a7640000 --rpc-url "$L1_RPC"  >/dev/null 2>&1
    cast rpc anvil_setBalance "$deployer" 0xde0b6b3a7640000 --rpc-url "$L2B_RPC" >/dev/null 2>&1
    _deploy() { forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$1" --private-key "$key" \
        --broadcast --constructor-args "CollideToken" "COL" 18 "$TOKEN_SUPPLY" 2>&1 | grep "Deployed to:" | awk '{print $NF}'; }
    col_l1=$(_deploy "$L1_RPC"); col_l2b=$(_deploy "$L2B_RPC")
    if [[ -z "$col_l1" || "$(echo "$col_l1" | tr 'A-F' 'a-f')" != "$(echo "$col_l2b" | tr 'A-F' 'a-f')" ]]; then
        echo "addr-mismatch" > "$CNT_DIR/clash"; mix "clash: CREATE address mismatch L1=$col_l1 L2B=$col_l2b"; return
    fi
    COL_HEX="$(echo "${col_l1#0x}" | tr 'A-F' 'a-f')"
    mix "clash: COL at $col_l1 on both chains"
    cast send "$col_l1" "approve(address,uint256)" "$BRIDGE" "$COL_L2B_WEI" --private-key "$key" --rpc-url "$L2B_RPC" >/dev/null 2>&1
    tx=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L2B_WEI" "$col_l1" true 0x --private-key "$key" --rpc-url "$L2B_RPC" 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || { echo "l2b-bridge-fail" > "$CNT_DIR/clash"; mix "clash: L2B bridge failed"; return; }
    cast send --rpc-url "$L1_RPC" --private-key "$key" "$col_l1" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$COL_L1_WEI" >/dev/null 2>&1
    tx=$(cast send --rpc-url "$L1_RPC" --private-key "$key" "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L1_WEI" "$col_l1" true 0x 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || { echo "l1-bridge-fail" > "$CNT_DIR/clash"; mix "clash: L1 bridge failed"; return; }
    echo "submitted" > "$CNT_DIR/clash"; mix "clash: COL bridged from BOTH origins (net0 + net2)"
}

# ── Launch the L1<->Miden bulk load in the background ────────────────────────
LT_OUT=/tmp/mixed-l1-loadtest.out; LT_PID=""
if [[ "$SKIP_L1_LOAD" != "1" ]]; then
    step "Launching L1<->Miden bulk load (N=$N) in background"
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        N="$N" VERIFY=0 ALLOW_LATE=1 \
        "$SCRIPT_DIR/e2e-bridge-loadtest-isolated.sh" > "$LT_OUT" 2>&1 &
    LT_PID=$!
    log "  L1<->Miden loadtest PID $LT_PID (log: $LT_OUT)"
fi

# ── SUBMIT phase: fire all L2<->L2 traffic + the clash, concurrently with the
#    background L1 bulk load. Ops are sequential AMONG THEMSELVES (they share the
#    ADMIN account on L2B — concurrent cast-sends would race the nonce), but all
#    run while the L1<->Miden load hammers the same bridge/proxy — the real
#    "under concurrency" the clash asserts. Confirmation is deferred to the drain.
step "SUBMIT L2<->L2 traffic ($L2L2_FWD fwd, $L2L2_BACK back, 1 clash) under the L1 bulk load"
for i in $(seq 1 "$L2L2_FWD"); do l2l2_fwd_submit "$i"; done
clash_submit
for i in $(seq 1 "$L2L2_BACK"); do l2l2_back_submit "$i"; done

# ── Wait for the L1<->Miden load to finish (frees the prover for the drain) ──
if [[ -n "$LT_PID" ]]; then
    log "Waiting for L1<->Miden loadtest (PID $LT_PID) to settle..."
    wait "$LT_PID"; LT_RC=$?
    log "  L1<->Miden loadtest exited rc=$LT_RC"
    grep -aE "OVERALL RELIABILITY|database is locked count" "$LT_OUT" | tail -3 || true
else
    LT_RC=0
fi

# ── DRAIN phase: with the prover freed, confirm every submitted L2<->L2 op landed.
# Generous nudge budget (the claim scan is event-driven; each round forces a
# settle cycle). Confirmation is state-based: fwd -> its ClaimEvent row on Miden;
# back -> the L2B holder balance rose by the full bridged-out total; clash -> two
# DISTINCT faucet rows for one address.
step "DRAIN: confirming L2<->L2 claims (prover freed)"
export NUDGE_TRIES="${DRAIN_NUDGE_TRIES:-10}"
# NDG nudges wake BOTH the Miden-side (fwd) and L2B-side (back/clash) claim scans.
for gi in ${FWD_GIS[@]+"${FWD_GIS[@]}"}; do
    if nudge_until "fwd ClaimEvent (gi $gi)" "[ \"\$(claim_event_rows $gi)\" -ge 1 ]"; then
        bump fwd_ok; mix "fwd CLAIMED (gi $gi)"
    else mix "fwd claim did NOT land (gi $gi)"; fi
done
if [[ "$(cat "$CNT_DIR/clash")" == "submitted" ]]; then
    if nudge_until "clash: TWO COL faucet rows" \
        "[ \"\$(pgq \"SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}';\")\" = \"2\" ]"; then
        f0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}' AND origin_network=0;")
        f2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}' AND origin_network=${L2B_NETWORK_ID};")
        if [[ -n "$f0" && -n "$f2" && "$f0" != "$f2" ]]; then
            echo "distinct" > "$CNT_DIR/clash"; mix "clash: DISTINCT faucets net0=$f0 net2=$f2"
        else echo "collision" > "$CNT_DIR/clash"; mix "clash: COLLISION net0=$f0 net2=$f2"; fi
    else echo "faucets-incomplete" > "$CNT_DIR/clash"; mix "clash: two faucet rows never appeared"; fi
fi
BACK_N=$(cat "$CNT_DIR/back_sub")
if [[ "$BACK_N" -gt 0 && -n "$BACK_BEFORE0" ]]; then
    want=$(python3 -c "print(int('$BACK_BEFORE0') + $BACK_N * $BACK_OP_UNITS * $WEI_PER_MIDEN_UNIT)")
    if nudge_until "back: L2B holder +$((BACK_N * BACK_OP_UNITS)) units (-> $want)" \
        "[ \"\$(cast call $MOP 'balanceOf(address)(uint256)' $ADMIN --rpc-url $L2B_RPC | awk '{print \$1}')\" = \"$want\" ]"; then
        echo "$BACK_N" > "$CNT_DIR/back_ok"; mix "back: all $BACK_N claimed on L2B (holder balance == $want)"
    else
        now=$(cast call "$MOP" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
        mix "back: not all claimed (L2B holder $BACK_BEFORE0 -> $now, want $want)"
    fi
fi

# ── Verdict ──────────────────────────────────────────────────────────────────
FWD_SUB=$(cat "$CNT_DIR/fwd_sub"); FWD_OK=$(cat "$CNT_DIR/fwd_ok")
BACK_SUB=$(cat "$CNT_DIR/back_sub"); BACK_OK=$(cat "$CNT_DIR/back_ok")
CLASH=$(cat "$CNT_DIR/clash")

step "Verdict: settle margin, then event-completeness across net-0/1/2"
sleep "${MIX_SETTLE:-60}"
VC_RC=2
if [[ "${MIX_VERIFY:-1}" != "1" ]]; then
    # The chaos soak suppresses the mixed loadtest's own verify and runs ONE
    # authoritative verify post-heal. Report per-op + clash here; leave the
    # completeness verdict to the caller.
    log "MIX_VERIFY=0 — skipping internal completeness (caller verifies post-heal)"
    log "  L1<->Miden rc=$LT_RC  fwd=$FWD_OK/$FWD_SUB  back=$BACK_OK/$BACK_SUB  clash=$CLASH"
    exit 0
fi
if [[ -x "$TOOL_BIN" ]]; then
    ALLOW_LATE="${ALLOW_LATE:-1}" TOOL_BIN="$TOOL_BIN" \
        NODE_CONTAINER="$NODE_CONTAINER" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        "$SCRIPT_DIR/verify-event-completeness.sh" > /tmp/mixed-verify.out 2>&1
    VC_RC=$?
    grep -aE "TYPE|B2AGG->|CLAIM->|GER->|VERDICT|SANITY" /tmp/mixed-verify.out | tail -8
else
    warn "TOOL_BIN $TOOL_BIN not found — completeness skipped"
fi
LOCKS=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)

log "======================================================================"
log "  MIXED LOADTEST RESULT"
log "    L1<->Miden loadtest rc = $LT_RC"
log "    L2B->Miden forward ops = $FWD_OK/$FWD_SUB claimed"
log "    Miden->L2B back ops    = $BACK_OK/$BACK_SUB claimed"
log "    address clash          = $CLASH (want: distinct)"
log "    event-completeness rc  = $VC_RC (0 = PASS)"
log "    proxy store-locks      = $LOCKS"
OK=1
[[ "$FWD_OK" == "$FWD_SUB" && "$FWD_SUB" -gt 0 ]] || OK=0
[[ "$BACK_OK" == "$BACK_SUB" && "$BACK_SUB" -gt 0 ]] || OK=0
[[ "$CLASH" == "distinct" ]] || OK=0
[[ "$VC_RC" == "0" ]] || OK=0
[[ "${LOCKS:-1}" == "0" ]] || OK=0
[[ "$LT_RC" == "0" || "$SKIP_L1_LOAD" == "1" ]] || OK=0
if [[ "$OK" == "1" ]]; then
    log "  >>> MIXED LOADTEST PASS — all legit L1+L2<->L2 events landed exact-block, clash distinct <<<"
    log "======================================================================"
    exit 0
else
    log "  >>> MIXED LOADTEST NOT-GREEN — inspect /tmp/mixed-verify.out + $MIX_LOG + $LT_OUT <<<"
    log "======================================================================"
    exit 1
fi
