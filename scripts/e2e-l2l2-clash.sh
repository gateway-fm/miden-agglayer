#!/usr/bin/env bash
# e2e-l2l2-clash.sh — same-address / different-origin faucet isolation (#15/#108),
# decomposed from e2e-l2-to-l2.sh leg 3 into the l2l2 group.
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
# The COL claims settle ON MIDEN via the proxy's foreign-faucet auto-creation
# (same path the forward leg proved); the L2B-origin COL needs the event-driven
# L2->L2 scan woken by nudge certs. Assertions read the proxy store faucet_registry
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

[[ -f "$STATE_FILE" ]] || fail "no scenario state at $STATE_FILE — run e2e-l2l2-forward.sh first"
# shellcheck disable=SC1090
source "$STATE_FILE"
[[ -n "${OPT0_HEX:-}" && -n "${DEST_ADDR:-}" ]] \
    || fail "scenario state incomplete (need OPT0_HEX + DEST_ADDR): $STATE_FILE"

for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable at $L1_RPC"

log "======================================================================"
log "  L2<->L2 CLASH: same-address (L1 net0 vs L2B net2) faucet isolation"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
evidence_init
evidence_rollup_register "leg3"

COL_L1_WEI="${COL_L1_WEI:-1000000000000000}"     # COL bridged from L1 (net 0)
COL_L2B_WEI="${COL_L2B_WEI:-1000000000000000}"   # COL bridged from L2B (net 2)
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
# (COL, net 2) is event-driven off the L1 rollup-exit-root update (needs a nudge);
# (COL, net 0) rides the mainnet-exit-root path. Both auto-create faucets on Miden.
wait_for "COL net-2 deposit ready_for_claim" \
    "find_deposit '$DEST_ADDR' $L2B_NETWORK_ID '$COL_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') else 1)\"" \
    600 5
nudge_until "TWO faucet_registry rows for COL (claim scan)" \
    "[ \"\$(pgq \"SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}';\")\" = \"2\" ]" \
    || fail "claim scan never produced both COL faucets despite repeated nudges"
wait_for "TWO faucet_registry rows for COL (net 0 + net 2)" \
    "[ \"\$(pgq \"SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}';\")\" = \"2\" ]" \
    900 10

# ── Assert isolation: distinct faucets + negative control ───────────────────
COL_FID_NET0=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = 0;")
COL_FID_NET2=$(pgq "SELECT lower(faucet_id) FROM faucet_registry WHERE encode(origin_address,'hex') = '${COL_HEX}' AND origin_network = ${L2B_NETWORK_ID};")
[[ -n "$COL_FID_NET0" && -n "$COL_FID_NET2" ]] \
    || fail "COL faucet rows incomplete: net0='$COL_FID_NET0' net2='$COL_FID_NET2'"
[[ "$COL_FID_NET0" != "$COL_FID_NET2" ]] \
    || fail "FAUCET COLLISION: (COL, net 0) and (COL, net 2) share faucet $COL_FID_NET0"
pass "distinct faucets for one address: net0=$COL_FID_NET0 net2=$COL_FID_NET2"
evidence_record "leg3" clash Miden claim "" "" "$COL" "isolated" \
    "COL faucets net0=$COL_FID_NET0 net2=$COL_FID_NET2 (distinct)"

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
