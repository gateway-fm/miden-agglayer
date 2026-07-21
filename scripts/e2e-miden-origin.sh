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

# The native token's chosen 20-byte "L1 representation" address (the origin_address the
# bridge registry records for this native faucet). admin_registerNativeFaucet is idempotent
# BY ORIGIN, so a FRESH origin per run keeps the e2e repeatable on a warm stack (a fixed
# origin would collide with a prior run's stale faucet_id and short-circuit registration).
# Recognizable 0x…0d1de0 prefix marker + random tail; override to pin a specific origin.
NATIVE_ORIGIN_ADDR="${NATIVE_ORIGIN_ADDR:-0x0d1de0$(python3 -c "import secrets;print(secrets.token_hex(17))")}"
MINT_UNITS="${MINT_UNITS:-500000}"          # initial native supply minted to the wallet
OUT_UNITS="${OUT_UNITS:-100000}"            # units bridged out Miden->dest (then back)

# ── Destination selector: DEST=l2b (default) | l1 ────────────────────────────
# The native round-trip is identical for both destinations except for the target
# chain. For L1 the return (dest->Miden) deposit is network_id=0 and is indexed by
# the MIDEN bridge-service (which indexes L1); for L2B it is network_id=2 indexed by
# the L2B service. The bridge contract is at the same address on all three chains.
DEST="${DEST:-l2b}"
if [[ "$DEST" == "l1" ]]; then
    DEST_NET=0;                  DEST_RPC="$L1_RPC"
    DEST_SVC="$BRIDGE_SERVICE_URL";       DEST_LABEL="L1"
else
    DEST_NET="$L2B_NETWORK_ID";  DEST_RPC="$L2B_RPC"
    DEST_SVC="$L2B_BRIDGE_SERVICE_URL";   DEST_LABEL="L2B"
fi

log "======================================================================"
log "  MIDEN-ORIGINATED TOKEN (native lock/unlock): Miden -> $DEST_LABEL -> Miden"
log "  proxy network id (origin for native) = $MIDEN_NETWORK_ID (configured, not hardcoded)"
log "  destination = $DEST_LABEL (net=$DEST_NET, rpc=$DEST_RPC)"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
l2l2_miden_identities
evidence_init 2>/dev/null || true
# Deploy the NDG nudge token (sets $NDG) — step 5's nudge_until drives L2B cert cycles
# so the covering GER reaches Miden for the native claim-back (proxy C6 has_seen_ger gate).
l2l2_deploy_nudge_token

# ── 1. Two-party setup: external deploy, then proxy (admin) allowlist ─────────
# The Miden bridge is a PERMISSIONED ALLOWLIST: a faucet is non-bridgeable until the
# bridge admin registers it (ConfigAggBridgeNote, admin-only). The admin is the
# proxy's `service` account — so only the PROXY can register. Realistic flow:
#   1a. EXTERNAL party (bridge-out tool) DEPLOYS an operator faucet + mints. No admin.
#   1b. PROXY (admin) REGISTERS/allowlists it native on the bridge + records it.
step "1a. External (bridge-out tool) deploys an operator faucet on Miden + mints $MINT_UNITS (custom symbol MDN)"
# RED infra: bridge-out tool --create-native-faucet deploys an operator-owned faucet
# with a CUSTOM symbol/decimals and mints MINT_UNITS to the wallet. Prints faucet-id.
# Capture the tool's full output so a failure is DIAGNOSABLE — the old
# `2>&1 | awk` swallowed the tool's error, leaving "did not print a faucet-id"
# with no cause. Tee to a log, extract the id from it, dump it on failure.
_nf_log="$(mktemp)"
iso_tool --create-native-faucet --native-symbol "MDN" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" > "$_nf_log" 2>&1 || true
NATIVE_FAUCET_ID=$(awk '/faucet-id:/{print $NF}' "$_nf_log")
[[ -n "$NATIVE_FAUCET_ID" ]] || { echo "─── bridge-out-tool --create-native-faucet output ───"; cat "$_nf_log"; echo "─── end tool output ───"; rm -f "$_nf_log"; fail "bridge-out-tool --create-native-faucet did not print a faucet-id — deploy/mint failed (see tool output above)"; }
rm -f "$_nf_log"
pass "external party deployed native faucet: $NATIVE_FAUCET_ID + minted $MINT_UNITS MDN"

step "1b. Proxy (bridge ADMIN) allowlists the faucet as native (is_native=true, origin_network=$MIDEN_NETWORK_ID)"
# Only the proxy (bridge admin = service account) can register. admin_registerNativeFaucet
# takes the EXTERNALLY-deployed faucet_id + its chosen origin_address and sends the
# admin ConfigAggBridgeNote (is_native=true, origin_network = the CONFIGURED net id).
REG_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$NATIVE_FAUCET_ID\",\"origin_token_address\":\"$NATIVE_ORIGIN_ADDR\",
    \"symbol\":\"MDN\",\"decimals\":8}]}" 2>/dev/null) \
  || fail "admin_registerNativeFaucet unreachable — check the proxy is up on $L2_RPC and ADMIN_API_KEY is valid"
echo "$REG_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if 'result' in d else 1)" \
  || fail "admin_registerNativeFaucet failed: $REG_JSON"
# The proxy records origin_network == its OWN configured network id (is_native is
# derived from origin_network == service.network_id — no separate column).
# admin_registerNativeFaucet is ASYNC: the RPC returns `result` before the on-chain
# ConfigAggBridgeNote lands + the store row commits, so POLL for the row rather than
# querying once (an immediate read races the write and reads empty).
NATIVE_NET=""
for _i in $(seq 1 40); do
    NATIVE_NET=$(pgq "SELECT origin_network FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
    [[ "$NATIVE_NET" == "$MIDEN_NETWORK_ID" ]] && break
    sleep 3
done
[[ "$NATIVE_NET" == "$MIDEN_NETWORK_ID" ]] \
  || fail "native faucet origin_network='$NATIVE_NET', expected $MIDEN_NETWORK_ID (proxy must record the CONFIGURED net id)"
pass "proxy allowlisted native faucet on the bridge (origin_network=$MIDEN_NETWORK_ID)"

# ── 1c. #149 — registration validates against the deployed faucet account ─────
# The deployed MDN faucet is authoritative (symbol=MDN, decimals=8). A registration
# whose metadata DIFFERS from the faucet account must be REJECTED before any state
# change, so recovery never has to reconstruct an unreconstructable preimage. Use a
# FRESH origin address each time (idempotency keys on (origin_address, net) — a repeat
# on the SAME origin would short-circuit and return the existing route without
# validating). Assert: JSON-RPC error with the specific reason AND no registry row.
step "1c. #149: mismatched native-faucet metadata is rejected before any state change"
_reg_mismatch() { # $1=symbol $2=decimals $3=origin_addr  -> echoes the JSON response
  curl -s "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
    \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
    \"params\":[{\"faucet_id\":\"$NATIVE_FAUCET_ID\",\"origin_token_address\":\"$3\",
      \"symbol\":\"$1\",\"decimals\":$2}]}" 2>/dev/null
}
BAD_SYM_ORIGIN="0x0bad51$(python3 -c "import secrets;print(secrets.token_hex(17))")"
BAD_DEC_ORIGIN="0x0badde$(python3 -c "import secrets;print(secrets.token_hex(17))")"
BAD_NAME_ORIGIN="0x0bad1a$(python3 -c "import secrets;print(secrets.token_hex(17))")"
# origin_address is BYTEA: `lower(origin_address)` is an invalid Postgres call that
# pgq suppresses → empty output → a FALSE-GREEN "no row" even if one exists (PR #150
# review). Compare `encode(origin_address,'hex')` against the hex origin instead.
# A rejected registration must leave EXACTLY zero rows. Use COUNT(*) and require it
# to equal "0": a pgq/connection/query error (stderr suppressed) yields a nonzero
# exit or empty output — both must FAIL, not silently pass a `-z` check (PR #150).
_no_registry_row() {
  local n
  n=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE encode(origin_address,'hex')=lower('${1#0x}');") || return 1
  [[ "$n" == "0" ]]
}
# wrong symbol → 'symbol mismatch', and NO registry row
RESP=$(_reg_mismatch "WRONG" 8 "$BAD_SYM_ORIGIN")
echo "$RESP" | grep -qi 'symbol mismatch' \
  || fail "#149: wrong-symbol registration should error 'symbol mismatch', got: $RESP"
_no_registry_row "$BAD_SYM_ORIGIN" || fail "#149: wrong-symbol registration must leave NO registry row"
# wrong decimals → 'decimals mismatch', and NO registry row
RESP=$(_reg_mismatch "MDN" 6 "$BAD_DEC_ORIGIN")
echo "$RESP" | grep -qi 'decimals mismatch' \
  || fail "#149: wrong-decimals registration should error 'decimals mismatch', got: $RESP"
_no_registry_row "$BAD_DEC_ORIGIN" || fail "#149: wrong-decimals registration must leave NO registry row"
# wrong (non-matching) explicit name → 'name mismatch', and NO registry row
RESP=$(curl -s "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$NATIVE_FAUCET_ID\",\"origin_token_address\":\"$BAD_NAME_ORIGIN\",
    \"symbol\":\"MDN\",\"decimals\":8,\"name\":\"Not The Real Name\"}]}" 2>/dev/null)
echo "$RESP" | grep -qi 'name mismatch' \
  || fail "#149: wrong-name registration should error 'name mismatch', got: $RESP"
_no_registry_row "$BAD_NAME_ORIGIN" || fail "#149: wrong-name registration must leave NO registry row"
# No-bridge-note EVIDENCE (PR #150 re-review): register_faucet_in_bridge (the
# ConfigAggBridgeNote) runs only AFTER validation passes. To prove the ON-CHAIN
# config/token map has no entry for these origins (not merely that the direct
# store-write was skipped), wait one full faucet-reconciler cycle (default 30s
# poll) and re-assert NO row: the reconciler reads the bridge's on-chain config
# and materializes a store row for ANY note it finds, so a row appearing here
# would mean a config note WAS emitted for a rejected origin.
sleep 35
_no_registry_row "$BAD_SYM_ORIGIN"  || fail "#149: wrong-symbol registration emitted an on-chain bridge config note (row appeared after a reconciler poll)"
_no_registry_row "$BAD_DEC_ORIGIN"  || fail "#149: wrong-decimals registration emitted an on-chain bridge config note (row appeared after a reconciler poll)"
_no_registry_row "$BAD_NAME_ORIGIN" || fail "#149: wrong-name registration emitted an on-chain bridge config note (row appeared after a reconciler poll)"
# So the no-row assertions above (COUNT(*) == 0, error-safe, post-reconciler-poll)
# evidence that no bridge configuration was emitted for these origins. The
# per-field mismatch REJECTION is unit-tested directly (native_metadata_*_mismatch_*
# on resolve_native_faucet_metadata, the sole validation gate the endpoint calls via
# `?` before any bridge/store write).
pass "#149: mismatched symbol/decimals/name each rejected before any state change — NO registry row for any (⇒ no bridge config emitted)"

# ── 1d. #149 — a custom-name (name != symbol) faucet is preserved exactly ──────
# Deploy a SECOND native faucet whose on-chain token NAME differs from its symbol
# (--native-name), register it with `name` OMITTED so the proxy ADOPTS the authoritative
# on-chain name, and assert the persisted metadata preimage carries the custom name
# (never normalized to the symbol) — so its keccak hash is reconstructable on restore.
step "1d. #149: custom-name (name != symbol) native faucet — name adopted + preserved"
CUSTOM_NAME="Wrapped Midnight"
CUSTOM_SYMBOL="WMDN"
_cn_log="$(mktemp)"
iso_tool --create-native-faucet --native-name "$CUSTOM_NAME" --native-symbol "$CUSTOM_SYMBOL" \
    --native-decimals 8 --mint-units 0 --wallet-id "$WALLET_ID" > "$_cn_log" 2>&1 || true
CUSTOM_FAUCET_ID=$(awk '/faucet-id:/{print $NF}' "$_cn_log")
[[ -n "$CUSTOM_FAUCET_ID" ]] || { cat "$_cn_log"; rm -f "$_cn_log"; fail "#149: custom-name faucet deploy failed"; }
rm -f "$_cn_log"
CUSTOM_ORIGIN="0x0c0574$(python3 -c "import secrets;print(secrets.token_hex(17))")"
# Register with name OMITTED — the proxy must adopt the on-chain name "Wrapped Midnight".
RESP=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$CUSTOM_FAUCET_ID\",\"origin_token_address\":\"$CUSTOM_ORIGIN\",
    \"symbol\":\"$CUSTOM_SYMBOL\",\"decimals\":8}]}" 2>/dev/null) \
  || fail "#149: custom-name registration (name omitted) should SUCCEED, got: $RESP"
echo "$RESP" | python3 -c "import json,sys; sys.exit(0 if 'result' in json.load(sys.stdin) else 1)" \
  || fail "#149: custom-name registration returned no result: $RESP"
# Poll for the persisted metadata preimage, then assert it carries the custom NAME
# (never collapsed to the symbol). The preimage is abi.encode(name, symbol, decimals),
# so the UTF-8 bytes of both strings appear in it; name_hex must be present AND differ
# from symbol_hex (proving name != symbol was preserved, not normalized).
NAME_HEX=$(python3 -c "print('$CUSTOM_NAME'.encode().hex())")
SYMBOL_HEX=$(python3 -c "print('$CUSTOM_SYMBOL'.encode().hex())")
META_HEX=""
for _i in $(seq 1 40); do
    META_HEX=$(pgq "SELECT encode(metadata,'hex') FROM faucet_registry WHERE lower(faucet_id)=lower('$CUSTOM_FAUCET_ID');")
    [[ -n "$META_HEX" && "$META_HEX" == *"$NAME_HEX"* ]] && break
    sleep 3
done
[[ -n "$META_HEX" ]] || fail "#149: custom-name faucet_registry.metadata never persisted"
[[ "$META_HEX" == *"$NAME_HEX"* ]] \
  || fail "#149: persisted preimage must contain the custom name '$CUSTOM_NAME' (name_hex=$NAME_HEX); got metadata=$META_HEX"
[[ "$NAME_HEX" != "$SYMBOL_HEX" ]] \
  || fail "#149: test setup error — custom name must differ from symbol"
pass "#149: custom name '$CUSTOM_NAME' (!= symbol '$CUSTOM_SYMBOL') adopted + preserved in the metadata preimage"
# The custom-name (WMDN, name != symbol) native row created here is what the
# restore-survival assertion in e2e-cantina13-metadata-recovery.sh reuses (its
# coordinated proxy + bridge-service drop+restore proves the row survives). The
# destructive in-script restore leg was removed per the PR #150 re-review.

# ── 2. Bridge OUT Miden -> L2B (bridge locks the native asset) ────────────────
step "2. Bridge out $OUT_UNITS native MDN Miden -> $DEST_LABEL (bridge LOCKS; proxy emits originNetwork=$MIDEN_NETWORK_ID)"
# Bridge OUT to a FRESH funded account whose key we KEEP, so step 4 can approve + bridge
# the wrapped token BACK. NOT ADMIN: ADMIN is the proxy-admin of bridge-deployed wrapped
# tokens (TransparentUpgradeableProxy blocks its admin from calling ERC20 fns like approve).
BACK_KEYS=$(cast wallet new)
BACK_DEST=$(echo "$BACK_KEYS" | awk '/Address:/{print $2}')
BACK_KEY=$(echo "$BACK_KEYS" | awk '/Private key:/{print $3}')
[[ -n "$BACK_DEST" && -n "$BACK_KEY" ]] || fail "could not generate BACK_DEST account"
cast rpc anvil_setBalance "$BACK_DEST" 0xde0b6b3a7640000 --rpc-url "$DEST_RPC" >/dev/null 2>&1 || true
# --asset-callbacks-disabled: a native operator faucet mints via FungibleAsset::new
# (callbacks DISABLED), so its assets live in the disabled vault slot (AggLayer wrapped
# faucets use the enabled slot). Bridge OUT from the matching slot.
iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$NATIVE_FAUCET_ID" \
    --amount "$OUT_UNITS" --dest-address "$BACK_DEST" --dest-network "$DEST_NET" \
    --asset-callbacks-disabled \
    || fail "bridge-out-tool failed for native faucet (destNet=$DEST_NET)"
# DISCOVERY assertion: the synthetic BridgeEvent for this bridge-out must carry
# originNetwork == the proxy's configured net id (read from the discovered native
# registration) — NOT 0/2 (which is what a missing-discovery fallback would emit).
wait_for "native bridge-out BridgeEvent with originNetwork=$MIDEN_NETWORK_ID" 300 5 \
    _pred_native_bridgeevent_origin "$NATIVE_ORIGIN_ADDR" "$MIDEN_NETWORK_ID"
pass "native bridge-out emitted BridgeEvent originNetwork=$MIDEN_NETWORK_ID (discovery OK)"

# ── 3. Claim on L2B — wrapped native-Miden token minted (foreign origin) ──────
step "3. Claim on $DEST_LABEL (wrapped native-Miden minted, origin_network=$MIDEN_NETWORK_ID)"
# Native tokens are registered scale=0, so amounts stay UNSCALED (Miden units) end-to-end.
# The claimAsset amount MUST equal the deposit's leaf amount — using OUT_UNITS*WEI_PER_MIDEN_UNIT
# would make the L2B bridge compute a different leaf and revert with InvalidSmtProof.
# Timeout is generous: a single native deposit on an otherwise-quiet chain certifies slower
# than the l2l2 group (which generates constant cert-triggering traffic).
wait_for "Miden->$DEST_LABEL native deposit ready_for_claim" 1200 5 \
    _pred_deposit_ready "$BACK_DEST" "$MIDEN_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$DEST_NET"
BACK_DEP=$(find_deposit "$BACK_DEST" "$MIDEN_NETWORK_ID" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')")
[[ -n "$BACK_DEP" ]] || fail "native Miden->L2B deposit not indexed"
OUT_CNT=$(dep_field "$BACK_DEP" deposit_cnt); OUT_GI=$(dep_field "$BACK_DEP" global_index)
OUT_AMT=$(dep_field "$BACK_DEP" amount)   # leaf-authoritative (unscaled for native)
OUT_META=$(echo "$BACK_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m!='0x' else '0x')")
if [[ "$DEST" == "l1" ]]; then
    # L1: the Miden<->L1 autoclaim service (l2l2-bridge-autoclaim-1, --network-id=1) claims
    # Miden->L1 deposits ON L1 automatically. Wait for it to mint the wrapped token to
    # BACK_DEST rather than racing it with a manual submit (a double-claim would revert).
    CLAIM_TX=$(wait_wrapped_mint "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" "$BACK_DEST" "$OUT_AMT" "$DEST_RPC" 1200) \
        || fail "autoclaim never minted wrapped native-Miden on L1 (holder $BACK_DEST, amount $OUT_AMT)"
else
    CLAIM_TX=$(submit_back_claim "$OUT_CNT" "$OUT_GI" "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" "$DEST_NET" "$BACK_DEST" "$OUT_AMT" "$OUT_META" "$DEST_RPC") \
        || fail "claim of native-origin deposit on $DEST_LABEL never settled"
fi
pass "wrapped native-Miden minted on $DEST_LABEL (amount $OUT_AMT, claim tx $CLAIM_TX)"

# ── 4. Bridge BACK dest -> Miden (burn the wrapped) ──────────────────────────
step "4. Bridge back $DEST_LABEL -> Miden (burn wrapped native-Miden)"
# The wrapped token address on the dest chain == a bridge-deployed wrapped ERC20 for
# (NATIVE_ORIGIN_ADDR, origin_network=$MIDEN_NETWORK_ID). Look it up from the dest bridge.
WRAPPED_L2B=$(cast call "$BRIDGE" "getTokenWrappedAddress(uint32,address)(address)" \
    "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" --rpc-url "$DEST_RPC" 2>/dev/null | awk '{print $1}')
[[ -n "$WRAPPED_L2B" && "$WRAPPED_L2B" != 0x0000000000000000000000000000000000000000 ]] \
    || fail "no wrapped native-Miden token on $DEST_LABEL for ($NATIVE_ORIGIN_ADDR, net $MIDEN_NETWORK_ID)"
# Use BACK_KEY (the wrapped-token holder), NOT ADMIN_KEY — ADMIN is the wrapped-proxy admin.
cast send "$WRAPPED_L2B" "approve(address,uint256)" "$BRIDGE" "$OUT_AMT" \
    --private-key "$BACK_KEY" --rpc-url "$DEST_RPC" >/dev/null || fail "approve wrapped on $DEST_LABEL"
BACK_TX=$(cast send "$BRIDGE" "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
    "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$OUT_AMT" "$WRAPPED_L2B" true 0x \
    --private-key "$BACK_KEY" --rpc-url "$DEST_RPC" --json 2>/dev/null | python3 -c "import json,sys;print(json.load(sys.stdin).get('transactionHash',''))") \
    || fail "bridgeAsset (wrapped back) on $DEST_LABEL failed"
pass "wrapped burned + bridged $DEST_LABEL -> Miden (tx $BACK_TX)"

# ── 5. Claim on Miden — bridge UNLOCKS the native asset (native routing) ──────
step "5. Claim on Miden (bridge UNLOCKS native; proxy must NOT auto-create a wrapped faucet)"
FAUCETS_BEFORE=$(pgq "SELECT COUNT(*) FROM faucet_registry;")
# Client-submit the claimAsset for the L2B->Miden deposit of the wrapped-native token.
# originNetwork == $MIDEN_NETWORK_ID (native) => proxy resolves the EXISTING native faucet
# (from the discovery/registry entry) + the bridge unlocks; it must NOT provision a wrapped one.
wait_for "$DEST_LABEL->Miden wrapped deposit ready_for_claim" 1200 5 \
    _pred_deposit_ready "$DEST_ADDR" "$DEST_NET" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$MIDEN_NETWORK_ID" "$DEST_SVC"
BACK2_DEP=$(find_deposit "$DEST_ADDR" "$DEST_NET" "$(echo "$NATIVE_ORIGIN_ADDR" | tr 'A-F' 'a-f')" "$DEST_SVC")
BK_CNT=$(dep_field "$BACK2_DEP" deposit_cnt); BK_GI=$(dep_field "$BACK2_DEP" global_index)
BK_AMT=$(dep_field "$BACK2_DEP" amount)   # leaf-authoritative (unscaled for native)
BK_META=$(echo "$BACK2_DEP" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m!='0x' else '0x')")
if [[ "$DEST" == "l1" ]]; then
    # L1: the autoclaim service submits the L1->Miden claim to the proxy for us (the
    # aggoracle injects the L1 GER into Miden). Wait for the native UNLOCK (balance
    # restored) rather than racing a manual submit that would revert once auto-claimed.
    wait_native_unlock "$BRIDGE_ID" "$NATIVE_FAUCET_ID" "$MINT_UNITS" 1200 \
        || fail "autoclaim never unlocked the native asset on Miden (gi $BK_GI)"
else
    nudge_until "native claim UNLOCKED on Miden (ClaimEvent gi $BK_GI)" \
        _pred_submit_forward_claim "$BK_CNT" "$BK_GI" "$MIDEN_NETWORK_ID" "$NATIVE_ORIGIN_ADDR" "$MIDEN_NETWORK_ID" "$DEST_ADDR" "$BK_AMT" "$BK_META" \
        || fail "native claim never unlocked on Miden (gi $BK_GI)"
fi
FAUCETS_AFTER=$(pgq "SELECT COUNT(*) FROM faucet_registry;")
[[ "$FAUCETS_AFTER" == "$FAUCETS_BEFORE" ]] \
    || fail "native claim provisioned a NEW faucet ($FAUCETS_BEFORE -> $FAUCETS_AFTER) — must UNLOCK the existing native faucet, not wrap"
pass "native asset UNLOCKED on Miden (no new faucet: $FAUCETS_AFTER == $FAUCETS_BEFORE)"

# ── 6. Net-zero assertions ───────────────────────────────────────────────────
step "6. Net-zero: native holder restored on Miden; wrapped fully burned on $DEST_LABEL"
NATIVE_BAL=$(iso_wallet_balance "$BRIDGE_ID" "$NATIVE_FAUCET_ID"); NATIVE_BAL="${NATIVE_BAL:-0}"
[[ "$NATIVE_BAL" -eq "$MINT_UNITS" ]] \
    || fail "native holder balance $NATIVE_BAL != minted $MINT_UNITS (round-trip not net-zero)"
WRAPPED_SUPPLY=$(cast call "$WRAPPED_L2B" "totalSupply()(uint256)" --rpc-url "$DEST_RPC" 2>/dev/null | awk '{print $1}')
[[ "${WRAPPED_SUPPLY:-0}" -eq 0 ]] \
    || fail "wrapped native-Miden supply on L2B = $WRAPPED_SUPPLY, expected 0 (not fully burned)"
pass "NET-ZERO: native holder = $NATIVE_BAL units; wrapped $DEST_LABEL supply = 0"

log "======================================================================"
log "  MIDEN-ORIGINATED ROUND-TRIP PASS — native lock/unlock, exact-block, net-zero"
log "======================================================================"
