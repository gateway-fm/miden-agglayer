#!/usr/bin/env bash
# e2e-l2l2-clash.sh — same-address / different-origin faucet isolation (#15/#108),
# the canonical same-address faucet-isolation leg of the l2l2 group (leg 3).
#
# Precondition: e2e-l2l2-forward.sh ran and bridged OPT0 in as a net-2-ONLY asset
# (used as the negative control below). Reads the shared scenario state file.
#
#   deploy the SAME 20-byte token address (COL) on BOTH L1 and L2B (fresh key at
#     nonce 0 on both chains -> identical CREATE address)
#   bridge COL into Miden FROM BOTH origins (L1 net 0 AND L2B net 2)
#   -> ASSERT: TWO DISTINCT faucets keyed (COL, net 0) vs (COL, net 2); a shared
#      faucet id is a hard FAIL (the #108 collision). Negative control: (OPT0,
#      net 0) resolves to NOTHING and OPT0's address has exactly one row (net 2).
#
# The COL claims settle ON MIDEN via the proxy's foreign-faucet auto-creation (same
# path the forward leg proved): the L1-origin COL is AUTO-claimed (canonical L1->L2),
# and the L2B-origin COL is client-submitted (proof from the L2B service -> proxy),
# since per-rollup isolation means the Miden service no longer indexes L2B. Assertions
# read the proxy store faucet_registry
# (PG state, not logs) so they're robust under load / repeated runs (COL is a fresh
# random address each run -> naturally N=20-safe).
#
# Usage: after e2e-l2l2-forward.sh, ./scripts/e2e-l2l2-clash.sh (or e2e-test.sh l2l2)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-l2l2}"
STATE_FILE="${STATE_FILE:-$B2AGG_STORE_DIR/l2l2-scenario-state.env}"

source "$SCRIPT_DIR/lib-l2l2.sh"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

[[ -f "$STATE_FILE" ]] || fail "no scenario state at $STATE_FILE — run e2e-l2l2-forward.sh first"
# shellcheck disable=SC1090
source "$STATE_FILE"
[[ -n "${OPT0_HEX:-}" && -n "${DEST_ADDR:-}" && -n "${BRIDGE_ID:-}" ]] \
    || fail "scenario state incomplete (need OPT0_HEX + DEST_ADDR + BRIDGE_ID): $STATE_FILE"

for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable at $L1_RPC"

log "======================================================================"
log "  L2<->L2 CLASH: same-address (L1 net0 vs L2B net2) faucet isolation"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
evidence_init
evidence_rollup_register "leg3"

# DISTINCT amounts per origin so a swapped/cross-contaminated mint can't pass: the
# net-0 and net-2 wrapped balances below must match THEIR OWN origin's amount.
COL_L1_WEI="${COL_L1_WEI:-1000000000000000}"     # net 0 -> 1e5 Miden units
COL_L2B_WEI="${COL_L2B_WEI:-3000000000000000}"   # net 2 -> 3e5 Miden units (distinct)
COL_L1_UNITS=$((COL_L1_WEI / WEI_PER_MIDEN_UNIT))
COL_L2B_UNITS=$((COL_L2B_WEI / WEI_PER_MIDEN_UNIT))
l2l2_deploy_nudge_token                          # NDG, so nudge_cert can force cert cycles
evidence_tx "leg3" clash L2B deploy "$L2B_RPC" "$NDG_DEPLOY_TX" "$NDG" "token=NDG role=cert-nudge"

# ── Deploy COL at the SAME address on L1 AND L2B ─────────────────────────────
# CREATE addresses derive from (sender, nonce): a FRESH key at nonce 0 on both
# chains yields the SAME 20-byte token address — the exact #108 collision the
# (origin_address, origin_network) faucet key must disambiguate.
step "Deploying COL at one address on both chains"
KEY_OUT=$(cast wallet new)
COL_DEPLOYER=$(echo "$KEY_OUT" | awk '/Address:/{print $2}')
COL_KEY=$(echo "$KEY_OUT" | awk '/Private key:/{print $3}')
[[ -n "$COL_DEPLOYER" && -n "$COL_KEY" ]] || fail "could not parse cast wallet new output"
cast send --rpc-url "$L1_RPC" --private-key "$ADMIN_KEY" --value 1ether "$COL_DEPLOYER" >/dev/null \
    || fail "funding COL deployer on L1"
cast rpc anvil_setBalance "$COL_DEPLOYER" 0xde0b6b3a7640000 --rpc-url "$L2B_RPC" >/dev/null \
    || fail "funding COL deployer on L2B"
[[ "$(cast nonce "$COL_DEPLOYER" --rpc-url "$L1_RPC")" == "0" ]]  || fail "COL deployer nonce non-zero on L1"
[[ "$(cast nonce "$COL_DEPLOYER" --rpc-url "$L2B_RPC")" == "0" ]] || fail "COL deployer nonce non-zero on L2B"

deploy_col() { # $1 = rpc url
    local out
    out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$1" \
        --private-key "$COL_KEY" --broadcast \
        --constructor-args "CollideToken" "COL" 18 "$TOKEN_SUPPLY" 2>&1) || { echo ""; return; }
    echo "$out" | awk '/Deployed to:/{print $NF}'
}
COL_L1=$(deploy_col "$L1_RPC");  [[ -n "$COL_L1" ]]  || fail "COL deploy on L1 failed"
COL_L2B=$(deploy_col "$L2B_RPC"); [[ -n "$COL_L2B" ]] || fail "COL deploy on L2B failed"
[[ "$(echo "$COL_L1" | tr 'A-F' 'a-f')" == "$(echo "$COL_L2B" | tr 'A-F' 'a-f')" ]] \
    || fail "CREATE address mismatch: L1=$COL_L1 L2B=$COL_L2B (nonce drift?)"
COL="$COL_L1"
COL_LOWER=$(echo "$COL" | tr 'A-F' 'a-f'); COL_HEX="${COL_LOWER#0x}"
pass "COL deployed at the SAME address on both chains: $COL"
evidence_record "leg3" clash L1  deploy "" "" "$COL" "success" "token=COL originNetwork=0 (L1 copy)"
evidence_record "leg3" clash L2B deploy "" "" "$COL" "success" "token=COL originNetwork=2 (L2B copy)"

# ── Bridge COL into Miden from BOTH origins ─────────────────────────────────
step "Bridging COL into Miden from BOTH origins (L1 net0 + L2B net2)"
cast send "$COL" "approve(address,uint256)" "$BRIDGE" "$COL_L2B_WEI" \
    --private-key "$COL_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "COL approve on L2B"
TX=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L2B_WEI" "$COL" true 0x \
    --private-key "$COL_KEY" --rpc-url "$L2B_RPC" 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "COL bridgeAsset on L2B failed (status=$STATUS): $TX"
COL_L2B_TX=$(printf '%s\n' "$TX" | awk '$1=="transactionHash"{print $2; exit}')
evidence_tx "leg3" clash L2B deposit "$L2B_RPC" "$COL_L2B_TX" "$BRIDGE" "token=COL origin=net2 destNet=$MIDEN_NETWORK_ID amountWei=$COL_L2B_WEI"

cast send --rpc-url "$L1_RPC" --private-key "$COL_KEY" \
    "$COL" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$COL_L1_WEI" >/dev/null \
    || fail "COL approve on L1"
TX=$(cast send --rpc-url "$L1_RPC" --private-key "$COL_KEY" \
    "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$COL_L1_WEI" "$COL" true 0x 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "COL bridgeAsset on L1 failed (status=$STATUS): $TX"
COL_L1_TX=$(printf '%s\n' "$TX" | awk '$1=="transactionHash"{print $2; exit}')
evidence_tx "leg3" clash L1 deposit "$L1_RPC" "$COL_L1_TX" "$BRIDGE_ADDRESS" "token=COL origin=net0 destNet=$MIDEN_NETWORK_ID amountWei=$COL_L1_WEI"
log "  COL bridged from BOTH origins (net 0: $COL_L1_WEI wei, net 2: $COL_L2B_WEI wei)"

# ── Drive both claims on Miden -> two faucet rows for COL ────────────────────
# (COL, net 0) is an L1->Miden claim: AUTO-claimed by the Miden ClaimTxManager (the
# canonical L1->L2 autoclaim, still enabled — proven: it rides the mainnet-exit-root
# path and creates its faucet without help). (COL, net 2) is an L2B->Miden claim:
# with per-rollup bridge-service isolation the Miden service does NOT index L2B, so —
# exactly like the forward leg — we client-submit a proof-backed claimAsset (proof
# fetched from the L2B service) to the Miden proxy. nudge_until drives L2B cert cycles
# so Miden sees the covering GER (proxy C6 has_seen_ger gate).
wait_for "COL net-2 deposit ready_for_claim" 600 5 \
    _pred_deposit_ready "$DEST_ADDR" "$L2B_NETWORK_ID" "$COL_LOWER" "" "$L2B_BRIDGE_SERVICE_URL"
COL_DEP_NET2_SUBMIT=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$COL_LOWER" "$L2B_BRIDGE_SERVICE_URL")
[[ -n "$COL_DEP_NET2_SUBMIT" ]] || fail "COL net-2 deposit not found in L2B service for claim submission"
CN2_CNT=$(dep_field "$COL_DEP_NET2_SUBMIT" deposit_cnt)
CN2_GI=$(dep_field "$COL_DEP_NET2_SUBMIT" global_index)
CN2_ONET=$(dep_field "$COL_DEP_NET2_SUBMIT" orig_net)
CN2_DNET=$(dep_field "$COL_DEP_NET2_SUBMIT" dest_net)
CN2_DADDR=$(dep_field "$COL_DEP_NET2_SUBMIT" dest_addr)
CN2_META=$(echo "$COL_DEP_NET2_SUBMIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
nudge_until "COL net-2 claimAsset accepted on Miden (ClaimEvent for gi $CN2_GI)" \
    _pred_submit_forward_claim "$CN2_CNT" "$CN2_GI" "$CN2_ONET" "$COL" "$CN2_DNET" "$CN2_DADDR" "$COL_L2B_WEI" "$CN2_META" \
    || fail "COL net-2 claimAsset never accepted on the Miden proxy (gi $CN2_GI) despite repeated nudges"
# Both must now be present: net-2 just client-submitted, net-0 auto-claimed (L1->L2).
wait_for "TWO faucet_registry rows for COL (net 0 + net 2)" 900 10 \
    _pred_pg_eq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}';" "2"

# ── Assert isolation: distinct faucets + negative control ───────────────────
COL_FID_NET0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = 0;")
COL_FID_NET2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = ${L2B_NETWORK_ID};")
[[ -n "$COL_FID_NET0" && -n "$COL_FID_NET2" ]] \
    || fail "COL faucet rows incomplete: net0='$COL_FID_NET0' net2='$COL_FID_NET2'"
[[ "$COL_FID_NET0" != "$COL_FID_NET2" ]] \
    || fail "FAUCET COLLISION: (COL, net 0) and (COL, net 2) share faucet $COL_FID_NET0"
pass "distinct faucets for one address: net0=$COL_FID_NET0 net2=$COL_FID_NET2"

# Registry rows precede the mint, and equal amounts would hide a swapped route — so
# require BOTH claims to actually COMPLETE with the correct PER-ORIGIN amount, and
# pin each to its exact deposit global index + ClaimEvent (not "a row exists").
# net-0 (L1) COL deposit is indexed by both services (both watch L1) -> Miden svc;
# net-2 (L2B) COL deposit lives in the isolated L2B service.
COL_DEP_NET0=$(find_deposit "$DEST_ADDR" 0 "$COL_LOWER")
COL_DEP_NET2=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$COL_LOWER" "$L2B_BRIDGE_SERVICE_URL")
[[ -n "$COL_DEP_NET0" && -n "$COL_DEP_NET2" ]] \
    || fail "COL deposit missing in bridge-service: net0='${COL_DEP_NET0:+present}' net2='${COL_DEP_NET2:+present}'"
COL_GI_NET0=$(dep_field "$COL_DEP_NET0" global_index)
COL_GI_NET2=$(dep_field "$COL_DEP_NET2" global_index)
[[ -n "$COL_GI_NET0" && -n "$COL_GI_NET2" && "$COL_GI_NET0" != "$COL_GI_NET2" ]] \
    || fail "COL net0/net2 global index degenerate: net0=$COL_GI_NET0 net2=$COL_GI_NET2"

# Per-faucet WRAPPED BALANCE must equal THAT origin's amount (distinct amounts =>
# a swap or cross-mint is caught here, and a non-zero balance proves the mint
# actually completed — not merely that the faucet was registered).
CB0=0; CB2=0
for attempt in $(seq 1 18); do
    sleep 10
    CB0=$(iso_wallet_balance "$BRIDGE_ID" "$COL_FID_NET0"); CB0="${CB0:-0}"
    CB2=$(iso_wallet_balance "$BRIDGE_ID" "$COL_FID_NET2"); CB2="${CB2:-0}"
    log "  attempt $attempt/18: COL wrapped balances net0=$CB0 (want $COL_L1_UNITS) net2=$CB2 (want $COL_L2B_UNITS)"
    [[ "$CB0" -gt 0 && "$CB2" -gt 0 ]] && break
done
[[ "$CB0" -eq "$COL_L1_UNITS" ]]  || fail "(COL, net 0) wrapped balance: got $CB0, expected $COL_L1_UNITS (routing/mint wrong)"
[[ "$CB2" -eq "$COL_L2B_UNITS" ]] || fail "(COL, net 2) wrapped balance: got $CB2, expected $COL_L2B_UNITS (routing/mint wrong)"
pass "distinct mints: (COL,net0)=$CB0 via $COL_FID_NET0 ; (COL,net2)=$CB2 via $COL_FID_NET2"

# ClaimEvent pinned to each COL deposit's EXACT global index (not any COL row).
CR0=$(claim_event_rows "$COL_GI_NET0"); CR2=$(claim_event_rows "$COL_GI_NET2")
[[ "${CR0:-0}" -ge 1 ]] || fail "no ClaimEvent for (COL,net0) globalIndex $COL_GI_NET0"
[[ "${CR2:-0}" -ge 1 ]] || fail "no ClaimEvent for (COL,net2) globalIndex $COL_GI_NET2"
pass "ClaimEvent pinned: net0 gi=$COL_GI_NET0 (rows=$CR0), net2 gi=$COL_GI_NET2 (rows=$CR2)"
evidence_record "leg3" clash Miden claim "" "" "$COL" "isolated+minted" \
    "net0 faucet=$COL_FID_NET0 bal=$CB0 gi=$COL_GI_NET0 ; net2 faucet=$COL_FID_NET2 bal=$CB2 gi=$COL_GI_NET2"

# Negative control: OPT0 (from the forward leg) exists ONLY as an origin-network-2
# asset — a lookup under origin_network=0 must yield NOTHING, and exactly one row.
OPT0_NET0_ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}' AND origin_network = 0;")
[[ "$OPT0_NET0_ROWS" == "0" ]] \
    || fail "(OPT0, net 0) unexpectedly resolves to a faucet ($OPT0_NET0_ROWS rows) — keying broken"
OPT0_ALL_ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${OPT0_HEX}';")
[[ "$OPT0_ALL_ROWS" == "1" ]] \
    || fail "expected exactly 1 faucet row for OPT0's address, got $OPT0_ALL_ROWS"
pass "negative control: (OPT0, net 0) -> no faucet; OPT0 address has exactly 1 row (net 2)"

log "======================================================================"
log "  L2<->L2 CLASH PASS — one address, two origins, two isolated faucets"
log "======================================================================"
