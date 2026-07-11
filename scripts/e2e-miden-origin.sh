#!/usr/bin/env bash
# e2e-miden-origin.sh — a token that ORIGINATES on Miden, bridged OUT and BACK.
# ============================================================================
# RED-GREEN-REFACTOR acceptance test for Miden-originated (NATIVE) tokens (#35).
#
# Unlike every other e2e (which bridges an L1/L2B-origin token INTO Miden as a
# bridge-owned wrapped/mint-burn asset), this exercises a token whose ORIGIN is
# Miden itself: an operator-owned faucet registered on the bridge with
# is_native=true. The on-chain bridge LOCKs it on bridge-out and UNLOCKs it on
# claim-back (is_faucet_native branch); the proxy's only job is to DISCOVER the
# native registration + ROUTE correctly:
#   - bridge-out: emit a synthetic BridgeEvent whose originNetwork == the proxy's
#     CONFIGURED network id (NOT hardcoded 1 — read from the discovered registry).
#   - claim-back: recognise originNetwork == self.network_id => native => route to
#     the existing native faucet (bridge unlocks); do NOT auto-create a wrapped one.
#
# Flow (Miden -> L2B -> Miden, reusing the l2l2 harness):
#   1. Register a NATIVE Miden faucet on the bridge + mint an initial supply to
#      the destination wallet.  [admin_registerNativeFaucet — GREEN work]
#   2. Bridge OUT Miden -> L2B (bridge-out-tool). Bridge LOCKS the native asset.
#      ASSERT: synthetic BridgeEvent with originNetwork == $MIDEN_NETWORK_ID and
#      the registered native origin_address.
#   3. Claim on L2B (proof-backed claimAsset): a WRAPPED native-Miden token is
#      minted on L2B (foreign origin = Miden's network id).
#   4. Bridge BACK L2B -> Miden (bridgeAsset destNet=Miden): burn the wrapped on L2B.
#   5. Claim on Miden (claimAsset to the proxy): the bridge UNLOCKS the native asset.
#      ASSERT: the proxy did NOT auto-create a wrapped faucet (native routing), and
#      a ClaimEvent for the native origin landed.
#   6. ASSERT net-zero: the native holder's Miden balance is restored; the L2B
#      wrapped supply is fully burned.
#
# Usage: base+L2B stack up (make e2e-l2l2-up), then ./scripts/e2e-miden-origin.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-miden-origin}"
source "$SCRIPT_DIR/lib-l2l2.sh"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done

# The native token's chosen 20-byte "L1 representation" address (the origin_address
# the bridge registry records for this native faucet). Distinct, deterministic.
NATIVE_ORIGIN_ADDR="${NATIVE_ORIGIN_ADDR:-0x00000000000000000000000000000000000d1de0}"
MINT_UNITS="${MINT_UNITS:-500000}"          # initial native supply minted to the wallet
OUT_UNITS="${OUT_UNITS:-100000}"            # units bridged out Miden->L2B (then back)

log "======================================================================"
log "  MIDEN-ORIGINATED TOKEN (native lock/unlock): Miden -> L2B -> Miden"
log "  proxy network id (origin for native) = $MIDEN_NETWORK_ID (configured, not hardcoded)"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
l2l2_miden_identities
evidence_init 2>/dev/null || true

# ── 1. Register a NATIVE Miden faucet on the bridge + mint initial supply ─────
# GREEN work: admin_registerNativeFaucet creates an OPERATOR-owned faucet (not a
# bridge mint/burn faucet), registers it on the bridge with is_native=true +
# origin_network == the proxy's configured net id + NATIVE_ORIGIN_ADDR, and mints
# MINT_UNITS to $BRIDGE_ID's wallet. Returns {faucet_id}.
step "1. Register native Miden faucet (is_native=true, origin_network=$MIDEN_NETWORK_ID) + mint $MINT_UNITS"
REG_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"symbol\":\"MDN\",\"name\":\"MidenNative\",\"decimals\":8,
    \"origin_token_address\":\"$NATIVE_ORIGIN_ADDR\",\"mint_units\":$MINT_UNITS,
    \"dest\":\"$BRIDGE_ID\"}]}" 2>/dev/null) \
  || fail "admin_registerNativeFaucet unreachable (NOT YET IMPLEMENTED — this is the RED)"
NATIVE_FAUCET_ID=$(echo "$REG_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin).get('result',{}).get('faucet_id',''))" 2>/dev/null)
[[ -n "$NATIVE_FAUCET_ID" ]] || fail "admin_registerNativeFaucet returned no faucet_id: $REG_JSON"
pass "native faucet registered: $NATIVE_FAUCET_ID (origin_network=$MIDEN_NETWORK_ID)"

# The proxy MUST record is_native via origin_network == its own network id (not a
# separate column). Verify the registry row reads back native.
NATIVE_NET=$(pgq "SELECT origin_network FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
[[ "$NATIVE_NET" == "$MIDEN_NETWORK_ID" ]] \
  || fail "native faucet origin_network=$NATIVE_NET, expected $MIDEN_NETWORK_ID (proxy must record the CONFIGURED net id)"

# ── 2. Bridge OUT Miden -> L2B (bridge locks the native asset) ────────────────
step "2. Bridge out $OUT_UNITS native MDN Miden -> L2B (bridge LOCKS; proxy emits originNetwork=$MIDEN_NETWORK_ID)"
BACK_DEST=$(cast wallet new | awk '/Address:/{print $2}')
iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$NATIVE_FAUCET_ID" \
    --amount "$OUT_UNITS" --dest-address "$BACK_DEST" --dest-network "$L2B_NETWORK_ID" \
    || fail "bridge-out-tool failed for native faucet (destNet=$L2B_NETWORK_ID)"
# DISCOVERY assertion: the synthetic BridgeEvent for this bridge-out must carry
# originNetwork == the proxy's configured net id (read from the discovered native
# registration) — NOT 0/2 (which is what a missing-discovery fallback would emit).
wait_for "native bridge-out BridgeEvent with originNetwork=$MIDEN_NETWORK_ID" 300 5 \
    _pred_native_bridgeevent_origin "$NATIVE_ORIGIN_ADDR" "$MIDEN_NETWORK_ID"
pass "native bridge-out emitted BridgeEvent originNetwork=$MIDEN_NETWORK_ID (discovery OK)"

# ── 3. Claim on L2B — wrapped native-Miden token minted (foreign origin) ──────
step "3. Claim on L2B (wrapped native-Miden minted, origin_network=$MIDEN_NETWORK_ID)"
wait_for "Miden->L2B native deposit ready_for_claim" 600 5 \
    _pred_deposit_ready "$BACK_DEST" "$MIDEN_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$L2B_NETWORK_ID"
BACK_DEP=$(find_deposit "$BACK_DEST" "$MIDEN_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')")
[[ -n "$BACK_DEP" ]] || fail "native Miden->L2B deposit not indexed"
OUT_CNT=$(dep_field "$BACK_DEP" deposit_cnt); OUT_GI=$(dep_field "$BACK_DEP" global_index)
OUT_META=$(echo "$BACK_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m!='0x' else '0x')")
OUT_WEI=$((OUT_UNITS * WEI_PER_MIDEN_UNIT))
CLAIM_TX=$(submit_back_claim "$OUT_CNT" "$OUT_GI" "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" "$L2B_NETWORK_ID" "$BACK_DEST" "$OUT_WEI" "$OUT_META") \
    || fail "claim of native-origin deposit on L2B never settled"
pass "wrapped native-Miden minted on L2B (claim tx $CLAIM_TX)"

# ── 4. Bridge BACK L2B -> Miden (burn the wrapped) ───────────────────────────
step "4. Bridge back L2B -> Miden (burn wrapped native-Miden)"
# The wrapped token address on L2B == a bridge-deployed wrapped ERC20 for
# (NATIVE_ORIGIN_ADDR, origin_network=$MIDEN_NETWORK_ID). Look it up from the L2B bridge.
WRAPPED_L2B=$(cast call "$BRIDGE" "getTokenWrappedAddress(uint32,address)(address)" \
    "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" --rpc-url "$L2B_RPC" 2>/dev/null | awk '{print $1}')
[[ -n "$WRAPPED_L2B" && "$WRAPPED_L2B" != 0x0000000000000000000000000000000000000000 ]] \
    || fail "no wrapped native-Miden token on L2B for ($NATIVE_ORIGIN_ADDR, net $MIDEN_NETWORK_ID)"
cast send "$WRAPPED_L2B" "approve(address,uint256)" "$BRIDGE" "$OUT_WEI" \
    --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" >/dev/null || fail "approve wrapped on L2B"
BACK_TX=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$OUT_WEI" "$WRAPPED_L2B" true 0x \
    --private-key "$ADMIN_KEY" --rpc-url "$L2B_RPC" --json 2>/dev/null | python3 -c "import json,sys;print(json.load(sys.stdin).get('transactionHash',''))") \
    || fail "bridgeAsset (wrapped back) on L2B failed"
pass "wrapped burned + bridged L2B -> Miden (tx $BACK_TX)"

# ── 5. Claim on Miden — bridge UNLOCKS the native asset (native routing) ──────
step "5. Claim on Miden (bridge UNLOCKS native; proxy must NOT auto-create a wrapped faucet)"
FAUCETS_BEFORE=$(pgq "SELECT COUNT(*) FROM faucet_registry;")
# Client-submit the claimAsset for the L2B->Miden deposit of the wrapped-native token.
# originNetwork == $MIDEN_NETWORK_ID (native) => proxy routes to the existing native
# faucet + the bridge unlocks; it must NOT provision a new wrapped faucet.
wait_for "L2B->Miden wrapped deposit ready_for_claim" 600 5 \
    _pred_deposit_ready "$DEST_ADDR" "$L2B_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$MIDEN_NETWORK_ID" "$L2B_BRIDGE_SERVICE_URL"
BACK2_DEP=$(find_deposit "$DEST_ADDR" "$L2B_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$L2B_BRIDGE_SERVICE_URL")
BK_CNT=$(dep_field "$BACK2_DEP" deposit_cnt); BK_GI=$(dep_field "$BACK2_DEP" global_index)
BK_META=$(echo "$BACK2_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m!='0x' else '0x')")
nudge_until "native claim UNLOCKED on Miden (ClaimEvent gi $BK_GI)" \
    _pred_submit_forward_claim "$BK_CNT" "$BK_GI" "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$OUT_WEI" "$BK_META" \
    || fail "native claim never unlocked on Miden (gi $BK_GI)"
FAUCETS_AFTER=$(pgq "SELECT COUNT(*) FROM faucet_registry;")
[[ "$FAUCETS_AFTER" == "$FAUCETS_BEFORE" ]] \
    || fail "native claim provisioned a NEW faucet ($FAUCETS_BEFORE -> $FAUCETS_AFTER) — must UNLOCK the existing native faucet, not wrap"
pass "native asset UNLOCKED on Miden (no new faucet: $FAUCETS_AFTER == $FAUCETS_BEFORE)"

# ── 6. Net-zero assertions ───────────────────────────────────────────────────
step "6. Net-zero: native holder restored on Miden; wrapped fully burned on L2B"
NATIVE_BAL=$(iso_wallet_balance "$BRIDGE_ID" "$NATIVE_FAUCET_ID"); NATIVE_BAL="${NATIVE_BAL:-0}"
[[ "$NATIVE_BAL" -eq "$MINT_UNITS" ]] \
    || fail "native holder balance $NATIVE_BAL != minted $MINT_UNITS (round-trip not net-zero)"
WRAPPED_SUPPLY=$(cast call "$WRAPPED_L2B" "totalSupply()(uint256)" --rpc-url "$L2B_RPC" 2>/dev/null | awk '{print $1}')
[[ "${WRAPPED_SUPPLY:-0}" -eq 0 ]] \
    || fail "wrapped native-Miden supply on L2B = $WRAPPED_SUPPLY, expected 0 (not fully burned)"
pass "NET-ZERO: native holder = $NATIVE_BAL units; wrapped L2B supply = 0"

log "======================================================================"
log "  MIDEN-ORIGINATED ROUND-TRIP PASS — native lock/unlock, exact-block, net-zero"
log "======================================================================"
