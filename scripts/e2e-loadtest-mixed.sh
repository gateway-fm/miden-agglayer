#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-loadtest-mixed.sh — MIXED four-direction reliability loadtest.
#
# Drives, CONCURRENTLY, all FOUR bridge directions with an EXACT per-direction
# split (default N=30), plus a same-address clash, then confirms every submitted
# op landed and asserts event-completeness:
#
#   • L1->Miden   (N_L1_FWD, default 10) — bridgeAsset on L1, AUTO-claimed on Miden
#   • Miden->L1   (N_L1_BACK, default 10) — bridge-out-tool, AUTO-claimed on L1
#       (the two above are the proven isolated L1<->Miden loadtest run in the
#        background with an EXACT direction split via PLAN_L1/PLAN_L2 targets)
#   • L2B->Miden  (L2L2_FWD, default 5) — bridgeAsset destNet=1 on the L2B bridge,
#       CLIENT-submitted claimAsset on the Miden proxy (per-rollup isolation: the
#       Miden service does not index L2B, so nothing auto-claims — the client fetches
#       the proof from the L2B service and submits, exactly like the forward leg)
#   • Miden->L2B  (L2L2_BACK, default 5) — bridge-out-tool --dest-network 2,
#       CLIENT-submitted claimAsset on real anvil-l2b (submit_back_claim)
#   • ADDRESS CLASH under concurrency: a token at the SAME CREATE address on L1 AND
#       L2B (fresh key, nonce 0), bridged from BOTH origins — the two faucets must
#       stay DISTINCT (the (addr, origin_network) key). net-0 auto-claims; net-2 is
#       client-submitted.
#
# Verdict (on the settled stack):
#   (1) L1<->Miden loadtest rc == 0.
#   (2) every submitted L2<->L2 op reached claimed (fwd ClaimEvent; back holder
#       balance rose by the full bridged-out total).
#   (3) the clash faucets are distinct.
#   (4) event-completeness PASS (0 missing / 0 extra across net-0/1/2), if TOOL_BIN.
#   (5) 0 proxy store-locks.
#
# Usage: base+L2B stack up (make e2e-l2l2-up), then
#   ./scripts/e2e-loadtest-mixed.sh                         # default 10/10/5/5
#   N_L1_FWD=2 N_L1_BACK=2 L2L2_FWD=1 L2L2_BACK=1 ./scripts/e2e-loadtest-mixed.sh  # smoke
# (env: SKIP_L1_LOAD=1 runs only the L2<->L2 workload + clash.)
# set -uo pipefail (NOT -e): a single failed op is a MEASURED signal, not an abort.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-loadtest-mixed}"
source "$SCRIPT_DIR/lib-l2l2.sh"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

# ── The exact direction split (user spec: 10 / 10 / 5 / 5 = N=30) ─────────────
N_L1_FWD="${N_L1_FWD:-10}"          # L1->Miden deposits (parallel on L1, auto-claim)
N_L1_BACK="${N_L1_BACK:-10}"        # Miden->L1 bridge-outs (sequential, auto-claim on L1)
L2L2_FWD="${L2L2_FWD:-5}"           # L2B->Miden deposits (client-submit claim on Miden)
L2L2_BACK="${L2L2_BACK:-5}"         # Miden->L2B bridge-outs (client-submit claim on L2B)
SKIP_L1_LOAD="${SKIP_L1_LOAD:-0}"
TOOL_BIN="${TOOL_BIN:-$PROJECT_DIR/target/debug/bridge-out-tool}"   # repo-local default; override with $TOOL_BIN

FWD_SEED_WEI="${FWD_SEED_WEI:-5000000000000000}"    # 0.005 MOP -> 500000 units (faucet + pool for back ops)
FWD_OP_WEI="${FWD_OP_WEI:-1000000000000000}"        # 0.001 MOP -> 100000 units per L2B->Miden op
BACK_OP_UNITS="${BACK_OP_UNITS:-50000}"             # units per Miden->L2B bridge-out
BACK_OP_WEI=$((BACK_OP_UNITS * WEI_PER_MIDEN_UNIT))
COL_L1_WEI="${COL_L1_WEI:-1000000000000000}"
COL_L2B_WEI="${COL_L2B_WEI:-2000000000000000}"      # distinct amount from the L1 origin

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done

MIX_LOG="${MIX_LOG:-/tmp/mixed-l2l2.log}"; : > "$MIX_LOG"
mix() { echo -e "${CYAN:-}[$(date +%H:%M:%S)] MIX:${NC:-} $*" | tee -a "$MIX_LOG"; }

# ── #41: settle-aware nudge (chaos-tolerant) ─────────────────────────────────
# nudge_until's hard NUDGE_TRIES cap false-fails whole phases under chaos: the
# claim is often ACCEPTED on Miden (its ClaimEvent lands) while the cross-chain
# settlement merely needs a few more cert cycles than the cap allows.
# nudge_until_settled keeps the exact nudge cadence (nudge_cert + 75s poll per
# round) and the tight NUDGE_TRIES fast path, but past the cap it extends up to
# a DEADLINE (L2L2_SETTLE_TIMEOUT, default 900s) — and only while there is
# OBSERVABLE PROGRESS (agglayer cert height advancing in aggkit logs, or new
# ClaimEvents landing in synthetic_logs). Healthy stacks still resolve in the
# first rounds, so the non-chaos suite is not slowed. The failure message
# distinguishes "accepted but not settled" from "never accepted".
L2L2_SETTLE_TIMEOUT="${L2L2_SETTLE_TIMEOUT:-900}"
_settle_cert_height() {
    ( set +o pipefail; docker logs --tail 400 "${COMPOSE_PROJECT_NAME}-aggkit-1" 2>&1 \
        | grep -aoE 'Height: [0-9]+' | tail -1 | awk '{print $2}' ) 2>/dev/null
}
_settle_claim_count() {
    pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%';" 2>/dev/null
}
nudge_until_settled() {
    local desc="$1"; shift
    local base_tries="${NUDGE_TRIES:-6}" t=0 waited start now elapsed
    local h_prev c_prev c_init h_now c_now stall=0
    start=$(date +%s)
    h_prev=$(_settle_cert_height); h_prev="${h_prev:-0}"
    c_init=$(_settle_claim_count); c_init="${c_init:-0}"; c_prev="$c_init"
    while :; do
        t=$((t + 1))
        nudge_cert
        waited=0
        while [[ $waited -lt 75 ]]; do
            if ( set +o pipefail; "$@" ) 2>/dev/null; then
                log "  nudge round $t unblocked: $desc"; return 0
            fi
            sleep 5; waited=$((waited + 5)); echo -n "."
        done
        echo ""
        now=$(date +%s); elapsed=$((now - start))
        if [[ $t -lt $base_tries ]]; then
            warn "nudge round $t/$base_tries did not unblock: $desc — re-nudging"
            continue
        fi
        [[ $elapsed -ge $L2L2_SETTLE_TIMEOUT ]] && break
        h_now=$(_settle_cert_height); h_now="${h_now:-0}"
        c_now=$(_settle_claim_count); c_now="${c_now:-0}"
        if [[ "$h_now" -gt "$h_prev" || "$c_now" -gt "$c_prev" ]] 2>/dev/null; then
            stall=0
            warn "nudge round $t: not settled but PROGRESS observed (cert height ${h_prev}→${h_now}, claims ${c_prev}→${c_now}) — extending (${elapsed}s/${L2L2_SETTLE_TIMEOUT}s): $desc"
        else
            stall=$((stall + 1))
            warn "nudge round $t: no observable progress (cert height ${h_now}, claims ${c_now}; stall ${stall}/2) — $desc"
            [[ $stall -ge 2 ]] && break
        fi
        h_prev="$h_now"; c_prev="$c_now"
    done
    now=$(date +%s); elapsed=$((now - start))
    c_now=$(_settle_claim_count); c_now="${c_now:-0}"
    if [[ "$c_now" -gt "$c_init" ]] 2>/dev/null; then
        warn "SETTLE-TIMEOUT: accepted on Miden (ClaimEvents ${c_init}→${c_now}) but NOT settled within ${elapsed}s (L2L2_SETTLE_TIMEOUT=${L2L2_SETTLE_TIMEOUT}, rounds=$t): $desc"
    else
        warn "SETTLE-TIMEOUT: never accepted (no ClaimEvent landed in ${elapsed}s, rounds=$t): $desc"
    fi
    return 1
}

# ── mixed-specific predicate helpers (named callbacks for wait_for/nudge_until) ─
_mixed_wallet_ge() { [[ "$(iso_wallet_balance "$1" "$2")" -ge "$3" ]]; }                 # <bridge_id> <faucet_id> <min_units>
_mixed_l2b_balance_eq() {                                                                # <token> <holder> <want_wei>
    [[ "$(cast call "$1" 'balanceOf(address)(uint256)' "$2" --rpc-url "$L2B_RPC" | awk '{print $1}')" == "$3" ]]
}
_mixed_back_ready() {                                                                    # <dest> <orig_lower> <want_count>
    local n
    n=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$1?limit=200" 2>/dev/null | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(0); sys.exit()
print(len([x for x in d.get('deposits',[]) if x.get('network_id')==$MIDEN_NETWORK_ID and (x.get('orig_addr') or '').lower()=='$2' and x.get('ready_for_claim')]))" 2>/dev/null || echo 0)
    [[ "${n:-0}" -ge "$3" ]]
}
_mixed_dep_count_ge() {                                                                  # <dest> <netid> <orig_lower> <svc> <want_count>
    local c
    c=$(curl -sf "$4/bridges/$1?limit=200" 2>/dev/null | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(0); sys.exit()
print(len([x for x in d.get('deposits',[]) if x.get('network_id')==$2 and (x.get('orig_addr') or '').lower()=='$3']))" 2>/dev/null || echo 0)
    [[ "${c:-0}" -ge "$5" ]]
}

# clash_submit — deploy COL at the SAME CREATE addr on L1+L2B (fresh nonce-0 key),
# bridge it into Miden from BOTH origins. Sets globals COL/COL_HEX + $CNT_DIR/clash.
clash_submit() {
    mix "clash: deploying COL at the same address on L1 + L2B (nonce-0 fresh key)"
    local kout deployer key col_l1 col_l2b tx status
    kout=$(cast wallet new)
    deployer=$(echo "$kout" | awk '/Address:/{print $2}')
    key=$(echo "$kout" | awk '/Private key:/{print $3}')
    # Fund via anvil_setBalance (NOT an ADMIN cast-send) — the L1 bulk load spends the
    # SAME ADMIN key on L1, so an ADMIN L1 tx here would race its nonce.
    cast rpc anvil_setBalance "$deployer" 0xde0b6b3a7640000 --rpc-url "$L1_RPC"  >/dev/null 2>&1
    cast rpc anvil_setBalance "$deployer" 0xde0b6b3a7640000 --rpc-url "$L2B_RPC" >/dev/null 2>&1
    _deploy() { forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$1" --private-key "$key" \
        --broadcast --constructor-args "CollideToken" "COL" 18 "$TOKEN_SUPPLY" 2>&1 | grep "Deployed to:" | awk '{print $NF}'; }
    col_l1=$(_deploy "$L1_RPC"); col_l2b=$(_deploy "$L2B_RPC")
    if [[ -z "$col_l1" || "$(echo "$col_l1" | tr 'A-F' 'a-f')" != "$(echo "$col_l2b" | tr 'A-F' 'a-f')" ]]; then
        echo "addr-mismatch" > "$CNT_DIR/clash"; mix "clash: CREATE address mismatch L1=$col_l1 L2B=$col_l2b"; return
    fi
    COL="$col_l1"; COL_HEX="$(echo "${col_l1#0x}" | tr 'A-F' 'a-f')"
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

log "======================================================================"
log "  MIXED LOADTEST — L1<->Miden ($N_L1_FWD/$N_L1_BACK) + L2<->L2 (fwd=$L2L2_FWD back=$L2L2_BACK) + clash"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
l2l2_miden_identities
l2l2_deploy_nudge_token

# ── Seed: deploy MOP on L2B + forward-bridge to create (MOP,net2) faucet + a
#    wrapped pool big enough to fund all $L2L2_BACK back bridge-outs ────────────
step "Seed: deploy MOP on L2B + forward-bridge $FWD_SEED_WEI wei (faucet + back-op pool)"
OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L2B_RPC" \
    --private-key "$ADMIN_KEY" --broadcast \
    --constructor-args "MixToken" "MOP" 18 "$TOKEN_SUPPLY" 2>&1)
MOP=$( ( set +o pipefail; echo "$OUT" | grep "Deployed to:" | awk '{print $NF}' ) || true )
[[ -n "$MOP" ]] || fail "MOP deploy failed: $(echo "$OUT" | tail -2)"
MOP_LOWER=$(echo "$MOP" | tr 'A-F' 'a-f'); MOP_HEX="${MOP_LOWER#0x}"
log "  MOP: $MOP"

# forward_bridge <amount_wei> — approve + bridgeAsset MOP L2B->Miden. rc 0 on success.
forward_bridge() {
    local amt="$1" tx status
    cast send "$MOP" "approve(address,uint256)" "$BRIDGE" "$amt" \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null 2>&1 || return 1
    tx=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$amt" "$MOP" true 0x \
        --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]]
}
forward_bridge "$FWD_SEED_WEI" || fail "seed forward bridge failed"

# Wait only for source-chain indexing. The bounded nudge_until below deliberately
# drives ready_for_claim too: bridge-service can observe an L2 GER just before its
# matching L1 GER and needs the next cert cycle to repair that external ordering
# race. Waiting passively here would deadlock before the recovery loop can start.
wait_for "seed L2B->Miden deposit indexed" 120 5 \
    _pred_deposit_indexed "$DEST_ADDR" "$L2B_NETWORK_ID" "$MOP_LOWER" "$L2B_BRIDGE_SERVICE_URL"
SEED_DEP=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$MOP_LOWER" "$L2B_BRIDGE_SERVICE_URL")
[[ -n "$SEED_DEP" ]] || fail "seed deposit not indexed on the L2B service"
SEED_CNT=$(dep_field "$SEED_DEP" deposit_cnt); SEED_GI=$(dep_field "$SEED_DEP" global_index)
SEED_META=$(echo "$SEED_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
nudge_until_settled "seed (MOP,net2) claim accepted on Miden (ClaimEvent gi $SEED_GI)" \
    _pred_submit_forward_claim "$SEED_CNT" "$SEED_GI" 2 "$MOP" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$FWD_SEED_WEI" "$SEED_META" \
    || fail "seed (MOP,net2) claim never landed on Miden"

wait_for "(MOP,net2) faucet_registry row" 300 5 \
    _pred_pg_gt "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex')='${MOP_HEX}' AND origin_network=${L2B_NETWORK_ID};" 0
MOP_FAUCET_ID=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${MOP_HEX}' AND origin_network=${L2B_NETWORK_ID};")
[[ -n "$MOP_FAUCET_ID" && "$MOP_FAUCET_ID" == 0x* ]] || fail "could not resolve MOP faucet id (got '$MOP_FAUCET_ID')"
SEED_UNITS=$((FWD_SEED_WEI / WEI_PER_MIDEN_UNIT))
NEED_BACK_UNITS=$((L2L2_BACK * BACK_OP_UNITS))
[[ "$SEED_UNITS" -ge "$NEED_BACK_UNITS" ]] || fail "seed pool $SEED_UNITS < needed $NEED_BACK_UNITS for $L2L2_BACK back ops"
wait_for "seed wrapped MOP pool credited (>=$SEED_UNITS)" 300 15 \
    _mixed_wallet_ge "$BRIDGE_ID" "$MOP_FAUCET_ID" "$SEED_UNITS"
pass "seed done: (MOP,net2) faucet $MOP_FAUCET_ID, wrapped pool >= $SEED_UNITS units"

# ── op counters (files: subshells/background can't write parent vars) ─────────
CNT_DIR="$(mktemp -d)"; trap 'rm -rf "$CNT_DIR"' EXIT
echo 0 > "$CNT_DIR/fwd_sub"; echo 0 > "$CNT_DIR/fwd_ok"
echo 0 > "$CNT_DIR/back_sub"; echo 0 > "$CNT_DIR/back_ok"
echo pending > "$CNT_DIR/clash"
bump() { local f="$CNT_DIR/$1"; echo $(( $(cat "$f") + 1 )) > "$f"; }

COL_HEX=""; COL=""; BACK_DEST=""

# ── Launch the L1<->Miden bulk load in the background (exact 10/10 split) ─────
LT_OUT=/tmp/mixed-l1-loadtest.out; LT_PID=""
if [[ "$SKIP_L1_LOAD" != "1" ]]; then
    step "Launching L1<->Miden bulk load ($N_L1_FWD L1->Miden + $N_L1_BACK Miden->L1) in background"
    COMPOSE_PROJECT_NAME="$COMPOSE_PROJECT_NAME" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        PLAN_L1_TARGET="$N_L1_FWD" PLAN_L2_TARGET="$N_L1_BACK" VERIFY=0 ALLOW_LATE=1 \
        "$SCRIPT_DIR/e2e-bridge-loadtest-isolated.sh" > "$LT_OUT" 2>&1 &
    LT_PID=$!
    log "  L1<->Miden loadtest PID $LT_PID (log: $LT_OUT)"
fi

# ── SUBMIT phase: fire all L2<->L2 traffic + the clash, concurrently with the L1
#    bulk load. Ops are sequential AMONG THEMSELVES (they share the ADMIN account on
#    L2B / the single Miden prover) but run WHILE the L1 load hammers the same
#    bridge/proxy — the real "under concurrency". Claims are deferred to the drain.
step "SUBMIT L2<->L2 traffic ($L2L2_FWD fwd, $L2L2_BACK back, 1 clash) — shuffled + jittered — under the L1 load"

# fwd FIRE-and-forget: approve + bridgeAsset MOP L2B->Miden. Deposit params are
# enumerated in the drain (not captured here) so ops don't block each other — a
# denser, more genuinely-mixed burst. (fwd fires share the ADMIN nonce on L2B, so
# they serialize among themselves, but interleave with back/clash + jitter.)
l2l2_fwd_fire() {
    bump fwd_sub
    forward_bridge "$FWD_OP_WEI" && mix "fwd#$1 fired (L2B->Miden $FWD_OP_WEI wei)" || mix "fwd#$1 submit FAILED"
}
# back FIRE: bridge-out wrapped MOP Miden->L2B ADMIN (creates the B2AGG note).
l2l2_back_fire() {
    bump back_sub
    if iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$MOP_FAUCET_ID" \
        --amount "$BACK_OP_UNITS" --dest-address "$BACK_DEST" --dest-network "$L2B_NETWORK_ID" >>"$MIX_LOG" 2>&1; then
        mix "back#$1 bridged out ($BACK_OP_UNITS units -> L2B $BACK_DEST)"
    else mix "back#$1 bridge-out FAILED"; fi
}

# Build a MIXED schedule of every L2<->L2 op + the clash, then Fisher-Yates shuffle it
# so fwd / back / clash genuinely INTERLEAVE (not all-fwd-then-clash-then-all-back), and
# space each op start with random jitter — so "which op begins when" is randomized, not
# a fixed back-to-back sequence. (RANDOM seeds from the shell; export MIX_SEED to pin a
# reproducible order.) The whole burst runs concurrently with the L1<->Miden background.
[[ -n "${MIX_SEED:-}" ]] && RANDOM="$MIX_SEED"
declare -a SCHED=()
for _ in $(seq 1 "$L2L2_FWD"); do SCHED+=("F"); done
for _ in $(seq 1 "$L2L2_BACK"); do SCHED+=("B"); done
SCHED+=("C")
for ((idx=${#SCHED[@]}-1; idx>0; idx--)); do
    j=$((RANDOM % (idx + 1))); tmp="${SCHED[idx]}"; SCHED[idx]="${SCHED[j]}"; SCHED[j]="$tmp"
done
mix "shuffled L2<->L2 schedule: ${SCHED[*]}  (jitter 0-${MIX_JITTER_MAX:-8}s between starts)"

# Back ops release MOP to a DEDICATED fresh L2B address (not ADMIN) so the back
# assertion is isolated from ADMIN's MOP balance, which the fwd bridges also move
# (with shuffling a fwd op may fire at any time relative to the back ops).
BACK_DEST=$(cast wallet new | awk '/Address:/{print $2}')
[[ -n "$BACK_DEST" && "$BACK_DEST" == 0x* ]] || fail "could not generate fresh BACK_DEST address"
mix "back dest (fresh, isolated from ADMIN): $BACK_DEST"
fwd_n=0; back_n=0
for op in "${SCHED[@]}"; do
    case "$op" in
        F) fwd_n=$((fwd_n + 1)); l2l2_fwd_fire "$fwd_n" ;;
        B) back_n=$((back_n + 1)); l2l2_back_fire "$back_n" ;;
        C) clash_submit ;;
    esac
    sleep $((RANDOM % (${MIX_JITTER_MAX:-8} + 1)))
done

# ── Wait for the L1<->Miden load to finish (frees the prover for the drain) ──
if [[ -n "$LT_PID" ]]; then
    log "Waiting for L1<->Miden loadtest (PID $LT_PID) to settle..."
    wait "$LT_PID"; LT_RC=$?
    log "  L1<->Miden loadtest exited rc=$LT_RC"
    ( set +o pipefail; grep -aE "OVERALL RELIABILITY|database is locked count" "$LT_OUT" | tail -3 ) || true
else
    LT_RC=0
fi

# ── DRAIN phase: with the prover freed, client-submit + confirm every L2<->L2 op.
step "DRAIN: client-submitting + confirming L2<->L2 claims (prover freed)"
export NUDGE_TRIES="${DRAIN_NUDGE_TRIES:-10}"

# fwd: client-submit every L2B->Miden claim. Enumerate all (MOP,net2,DEST_ADDR)
# deposits beyond the seed (cnt > SEED_CNT) once they're all indexed — fire-and-forget
# submit means params are read here, not captured at submit time.
FWD_SUB=$(cat "$CNT_DIR/fwd_sub")
if [[ "$FWD_SUB" -gt 0 ]]; then
    wait_for "all $FWD_SUB L2B->Miden fwd deposits indexed" 600 10 \
        _mixed_dep_count_ge "$DEST_ADDR" "$L2B_NETWORK_ID" "$MOP_LOWER" "$L2B_BRIDGE_SERVICE_URL" "$((1 + FWD_SUB))" || true
    mapfile -t FWD_ROWS < <(curl -sf "$L2B_BRIDGE_SERVICE_URL/bridges/$DEST_ADDR?limit=200" 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
for dep in sorted(d.get('deposits',[]), key=lambda x:x.get('deposit_cnt',0)):
    if dep.get('network_id')==$L2B_NETWORK_ID and (dep.get('orig_addr') or '').lower()=='$MOP_LOWER' and dep.get('deposit_cnt',0) > $SEED_CNT:
        print('%s|%s|%s' % (dep['deposit_cnt'], dep['global_index'], dep.get('metadata','0x') or '0x'))" 2>/dev/null)
    for entry in ${FWD_ROWS[@]+"${FWD_ROWS[@]}"}; do
        IFS='|' read -r cnt gi meta <<<"$entry"
        if nudge_until_settled "fwd ClaimEvent (gi $gi)" \
            _pred_submit_forward_claim "$cnt" "$gi" 2 "$MOP" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$FWD_OP_WEI" "$meta"; then
            bump fwd_ok; mix "fwd CLAIMED (gi $gi)"
        else mix "fwd claim did NOT land (gi $gi)"; fi
    done
fi

# clash: net-0 auto-claims (L1->L2); client-submit net-2 (L2B->Miden), then require
# TWO DISTINCT faucet rows for the one COL address.
if [[ "$(cat "$CNT_DIR/clash")" == "submitted" ]]; then
    cdep=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$(echo "$COL" | tr 'A-F' 'a-f')" "$L2B_BRIDGE_SERVICE_URL")
    if [[ -n "$cdep" ]]; then
        ccnt=$(dep_field "$cdep" deposit_cnt); cgi=$(dep_field "$cdep" global_index)
        cmeta=$(echo "$cdep" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
        nudge_until_settled "clash net-2 (COL) claim accepted on Miden (gi $cgi)" \
            _pred_submit_forward_claim "$ccnt" "$cgi" 2 "$COL" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L2B_WEI" "$cmeta" \
            || mix "clash net-2 claim did NOT land (gi $cgi)"
    else mix "clash net-2 COL deposit not indexed on the L2B service"; fi
    if nudge_until_settled "clash: TWO COL faucet rows" \
        _pred_pg_eq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}';" "2"; then
        f0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}' AND origin_network=0;")
        f2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex')='${COL_HEX}' AND origin_network=${L2B_NETWORK_ID};")
        if [[ -n "$f0" && -n "$f2" && "$f0" != "$f2" ]]; then
            echo "distinct" > "$CNT_DIR/clash"; mix "clash: DISTINCT faucets net0=$f0 net2=$f2"
        else echo "collision" > "$CNT_DIR/clash"; mix "clash: COLLISION net0=$f0 net2=$f2"; fi
    else echo "faucets-incomplete" > "$CNT_DIR/clash"; mix "clash: two faucet rows never appeared"; fi
fi

# back: client-submit claimAsset on L2B for every ready Miden->L2B MOP deposit, then
# require the L2B holder balance to have risen by the full bridged-out total.
BACK_N=$(cat "$CNT_DIR/back_sub")
if [[ "$BACK_N" -gt 0 ]]; then
    # Wait for all back deposits (to BACK_DEST) to be indexed + ready on the Miden service.
    wait_for "all $BACK_N Miden->L2B deposits ready_for_claim" 900 10 \
        _mixed_back_ready "$BACK_DEST" "$MOP_LOWER" "$BACK_N"
    # Enumerate every ready back deposit and claim it on L2B (releases MOP to BACK_DEST).
    mapfile -t BACK_ROWS < <(curl -sf "$BRIDGE_SERVICE_URL/bridges/$BACK_DEST?limit=200" 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
for dep in sorted(d.get('deposits',[]), key=lambda x:x.get('deposit_cnt',0)):
    if dep.get('network_id')==$MIDEN_NETWORK_ID and (dep.get('orig_addr') or '').lower()=='$MOP_LOWER' and dep.get('ready_for_claim'):
        print('%s|%s|%s|%s|%s|%s' % (dep['deposit_cnt'], dep['global_index'], dep.get('orig_net',2), dep.get('dest_net',$L2B_NETWORK_ID), dep.get('dest_addr','$BACK_DEST'), dep.get('metadata','0x') or '0x'))" 2>/dev/null)
    for row in ${BACK_ROWS[@]+"${BACK_ROWS[@]}"}; do
        IFS='|' read -r bcnt bgi bonet bdnet bdaddr bmeta <<<"$row"
        if txh=$(submit_back_claim "$bcnt" "$bgi" "$bonet" "$MOP" "$bdnet" "$bdaddr" "$BACK_OP_WEI" "$bmeta"); then
            mix "back CLAIMED on L2B (gi $bgi, tx $txh)"
        else mix "back claim did NOT settle on L2B (gi $bgi)"; fi
    done
    # Verdict: BACK_DEST starts at 0 MOP, so its balance must equal the full released total.
    want=$((BACK_N * BACK_OP_WEI))
    if wait_for "back: BACK_DEST holds $((BACK_N * BACK_OP_UNITS)) units (== $want wei)" 300 10 \
        _mixed_l2b_balance_eq "$MOP" "$BACK_DEST" "$want"; then
        echo "$BACK_N" > "$CNT_DIR/back_ok"; mix "back: all $BACK_N released on L2B (BACK_DEST == $want wei)"
    else
        now=$(cast call "$MOP" 'balanceOf(address)(uint256)' "$BACK_DEST" --rpc-url "$L2B_RPC" | awk '{print $1}')
        mix "back: not all released (BACK_DEST MOP=$now, want $want)"
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
    log "MIX_VERIFY=0 — skipping internal completeness (caller verifies post-heal)"
    log "  L1<->Miden rc=$LT_RC  fwd=$FWD_OK/$FWD_SUB  back=$BACK_OK/$BACK_SUB  clash=$CLASH"
    exit 0
fi
if [[ -x "$TOOL_BIN" ]]; then
    ALLOW_LATE="${ALLOW_LATE:-1}" TOOL_BIN="$TOOL_BIN" \
        NODE_CONTAINER="$NODE_CONTAINER" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        "$SCRIPT_DIR/verify-event-completeness.sh" > /tmp/mixed-verify.out 2>&1
    VC_RC=$?
    ( set +o pipefail; grep -aE "TYPE|B2AGG->|CLAIM->|GER->|VERDICT|SANITY" /tmp/mixed-verify.out | tail -8 ) || true
else
    warn "TOOL_BIN $TOOL_BIN not found — completeness skipped"; VC_RC=0
fi
# grep -c prints "0" AND exits 1 on zero matches; `|| true` swallows the exit WITHOUT
# emitting a second "0" (an `|| echo 0` here would make LOCKS="0\n0" and fail the check).
LOCKS=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)

log "======================================================================"
log "  MIXED LOADTEST RESULT"
log "    L1<->Miden loadtest rc      = $LT_RC ($N_L1_FWD L1->Miden + $N_L1_BACK Miden->L1)"
log "    L2B->Miden forward ops      = $FWD_OK/$FWD_SUB claimed"
log "    Miden->L2B back ops         = $BACK_OK/$BACK_SUB released"
log "    address clash               = $CLASH (want: distinct)"
log "    event-completeness rc       = $VC_RC (0 = PASS)"
log "    proxy store-locks           = $LOCKS"
OK=1
[[ "$FWD_OK" == "$FWD_SUB" && "$FWD_SUB" -gt 0 ]] || OK=0
[[ "$BACK_OK" == "$BACK_SUB" && "$BACK_SUB" -gt 0 ]] || OK=0
[[ "$CLASH" == "distinct" ]] || OK=0
[[ "$VC_RC" == "0" ]] || OK=0
[[ "${LOCKS:-1}" == "0" ]] || OK=0
[[ "$LT_RC" == "0" || "$SKIP_L1_LOAD" == "1" ]] || OK=0
if [[ "$OK" == "1" ]]; then
    log "  >>> MIXED LOADTEST PASS — all 4 directions landed + clash distinct <<<"
    log "======================================================================"
    exit 0
else
    log "  >>> MIXED LOADTEST NOT-GREEN — inspect /tmp/mixed-verify.out + $MIX_LOG + $LT_OUT <<<"
    log "======================================================================"
    exit 1
fi
