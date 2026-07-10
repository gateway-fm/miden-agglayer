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

# ── L2<->L2 forward op: bridge another chunk, confirm claimed (ClaimEvent) ────
l2l2_fwd_op() {
    local i="$1" gi
    bump fwd_sub
    forward_bridge "$FWD_OP_WEI" >/dev/null || { mix "fwd#$i submit FAILED"; return; }
    # newest ready MOP deposit's gi, then nudge until its ClaimEvent lands.
    if ! wait_for "fwd#$i ready_for_claim" \
        "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$MOP_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') else 1)\"" \
        600 5; then mix "fwd#$i never ready"; return; fi
    gi=$(dep_field "$(find_deposit "$DEST_ADDR" $L2B_NETWORK_ID "$MOP_LOWER")" global_index)
    if nudge_until "fwd#$i ClaimEvent (gi $gi)" \
        "[ \"\$(claim_event_rows $gi)\" -ge 1 ]"; then
        bump fwd_ok; mix "fwd#$i CLAIMED (gi $gi)"
    else mix "fwd#$i claim did not land (gi $gi)"; fi
}

# ── L2<->L2 back op: bridge-out to L2B ADMIN, confirm claimed (balance delta) ──
l2l2_back_op() {
    local i="$1" before after
    bump back_sub
    before=$(cast call "$MOP" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
    if ! iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$MOP_FAUCET_ID" \
        --amount "$BACK_OP_UNITS" --dest-address "$ADMIN" --dest-network "$L2B_NETWORK_ID" >>"$MIX_LOG" 2>&1; then
        mix "back#$i bridge-out FAILED"; return
    fi
    # wait for settle + ready, nudge L2B scan, confirm ADMIN balance rose.
    if ! wait_for "back#$i ready_for_claim" \
        "find_deposit '$ADMIN' $MIDEN_NETWORK_ID '$MOP_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') and d.get('dest_net')==$L2B_NETWORK_ID else 1)\"" \
        900 10; then mix "back#$i never ready"; return; fi
    local want=$(python3 -c "print(int('$before') + $BACK_OP_UNITS * $WEI_PER_MIDEN_UNIT)")
    if nudge_until "back#$i L2B claim (balance $before -> $want)" \
        "[ \"\$(cast call $MOP 'balanceOf(address)(uint256)' $ADMIN --rpc-url $L2B_RPC | awk '{print \$1}')\" = \"$want\" ]"; then
        bump back_ok; mix "back#$i CLAIMED on L2B"
    else
        after=$(cast call "$MOP" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
        mix "back#$i claim not confirmed (bal $before -> $after, want $want)"
    fi
}

# ── ADDRESS CLASH under concurrency: same CREATE addr on L1 + L2B ────────────
clash_check() {
    mix "clash: deploying COL at the same address on L1 + L2B (nonce-0 fresh key)"
    local kout deployer key col_l1 col_l2b col_lower col_hex tx status
    kout=$(cast wallet new)
    deployer=$(echo "$kout" | awk '/Address:/{print $2}')
    key=$(echo "$kout" | awk '/Private key:/{print $3}')
    cast send --rpc-url "$L1_RPC" --private-key "$ADMIN_KEY" --value 1ether "$deployer" >/dev/null 2>&1
    cast rpc anvil_setBalance "$deployer" 0xde0b6b3a7640000 --rpc-url "$L2B_RPC" >/dev/null 2>&1
    _deploy() { forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$1" --private-key "$key" \
        --broadcast --constructor-args "CollideToken" "COL" 18 "$TOKEN_SUPPLY" 2>&1 | grep "Deployed to:" | awk '{print $NF}'; }
    col_l1=$(_deploy "$L1_RPC"); col_l2b=$(_deploy "$L2B_RPC")
    if [[ -z "$col_l1" || "$(echo "$col_l1" | tr 'A-F' 'a-f')" != "$(echo "$col_l2b" | tr 'A-F' 'a-f')" ]]; then
        echo "addr-mismatch" > "$CNT_DIR/clash"; mix "clash: CREATE address mismatch L1=$col_l1 L2B=$col_l2b"; return
    fi
    col_lower=$(echo "$col_l1" | tr 'A-F' 'a-f'); col_hex="${col_lower#0x}"
    mix "clash: COL at $col_l1 on both chains"
    # bridge from L2B origin (net 2)
    cast send "$col_l1" "approve(address,uint256)" "$BRIDGE" "$COL_L2B_WEI" --private-key "$key" --rpc-url "$L2B_RPC" >/dev/null 2>&1
    tx=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L2B_WEI" "$col_l1" true 0x --private-key "$key" --rpc-url "$L2B_RPC" 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || { echo "l2b-bridge-fail" > "$CNT_DIR/clash"; mix "clash: L2B bridge failed"; return; }
    # bridge from L1 origin (net 0)
    cast send --rpc-url "$L1_RPC" --private-key "$key" "$col_l1" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$COL_L1_WEI" >/dev/null 2>&1
    tx=$(cast send --rpc-url "$L1_RPC" --private-key "$key" "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L1_WEI" "$col_l1" true 0x 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || { echo "l1-bridge-fail" > "$CNT_DIR/clash"; mix "clash: L1 bridge failed"; return; }
    # wait for both faucets (net2 needs the scan nudge)
    if ! wait_for "clash: COL net-2 ready" \
        "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$col_lower' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') else 1)\"" 600 5; then
        echo "net2-not-ready" > "$CNT_DIR/clash"; return; fi
    if ! nudge_until "clash: TWO COL faucet rows" \
        "[ \"\$(pgq \"SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex')='${col_hex}';\")\" = \"2\" ]"; then
        echo "faucets-incomplete" > "$CNT_DIR/clash"; mix "clash: two faucet rows never appeared"; return; fi
    local f0 f2
    f0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${col_hex}' AND origin_network=0;")
    f2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${col_hex}' AND origin_network=${L2B_NETWORK_ID};")
    if [[ -n "$f0" && -n "$f2" && "$f0" != "$f2" ]]; then
        echo "distinct" > "$CNT_DIR/clash"; mix "clash: DISTINCT faucets net0=$f0 net2=$f2"
    else
        echo "collision" > "$CNT_DIR/clash"; mix "clash: COLLISION net0=$f0 net2=$f2"
    fi
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

# ── Concurrent L2<->L2 workload + clash ──────────────────────────────────────
step "Driving L2<->L2 workload concurrently ($L2L2_FWD fwd, $L2L2_BACK back, 1 clash)"
clash_check &                       # run the clash concurrently with the fwd/back ops
CLASH_PID=$!
for i in $(seq 1 "$L2L2_FWD"); do l2l2_fwd_op "$i"; done
for i in $(seq 1 "$L2L2_BACK"); do l2l2_back_op "$i"; done
wait "$CLASH_PID" 2>/dev/null || true

# ── Wait for the L1<->Miden load to finish ───────────────────────────────────
if [[ -n "$LT_PID" ]]; then
    log "Waiting for L1<->Miden loadtest (PID $LT_PID) to settle..."
    wait "$LT_PID"; LT_RC=$?
    log "  L1<->Miden loadtest exited rc=$LT_RC"
    grep -aE "OVERALL RELIABILITY|database is locked count" "$LT_OUT" | tail -3 || true
else
    LT_RC=0
fi

# ── Verdict ──────────────────────────────────────────────────────────────────
FWD_SUB=$(cat "$CNT_DIR/fwd_sub"); FWD_OK=$(cat "$CNT_DIR/fwd_ok")
BACK_SUB=$(cat "$CNT_DIR/back_sub"); BACK_OK=$(cat "$CNT_DIR/back_ok")
CLASH=$(cat "$CNT_DIR/clash")

step "Verdict: settle margin, then event-completeness across net-0/1/2"
sleep "${MIX_SETTLE:-60}"
VC_RC=2
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
