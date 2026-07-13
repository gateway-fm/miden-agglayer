#!/usr/bin/env bash
# e2e-l2l2-back.sh — SIMPLE, DETERMINISTIC L2<->L2 back scenario
# ("l2-to-l2-back") — the canonical reverse leg of the l2l2 group (leg 4).
#
# Precondition: e2e-l2l2-forward.sh ran and left a Miden wallet holding wrapped
# OPT0 (state file written to the shared isolated store).
#
#   bridge-out wrapped OPT0 Miden -> L2B (destNet=2)
#   -> certificate settle -> Miden->L2B deposit ready_for_claim
#   -> claim on L2B (direct proof-backed claimAsset; L2B autoclaim out of scope)
#   -> ASSERT: net-zero round trip — L2B holder balance restored to its
#      pre-forward value AND the Miden wrapped balance fully burned.
#
# Usage: after e2e-l2l2-forward.sh, ./scripts/e2e-l2l2-back.sh
#        (or ./scripts/e2e-test.sh l2l2)
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
# Validate ALL required fields before doing anything (fail closed on a truncated file).
for _f in OPT0 OPT0_LOWER OPT0_HEX OPT0_FAUCET_ID WALLET_ID BRIDGE_ID DEST_ADDR FWD_MIDEN_UNITS; do
    [[ -n "${!_f:-}" ]] || fail "scenario state incomplete: missing $_f in $STATE_FILE"
done
# Reject a state file left over from a DIFFERENT stack/run: its chain fingerprint
# must match the chains we're about to drive.
_NOW_L1=$(cast chain-id --rpc-url "$L1_RPC" 2>/dev/null || echo "?")
_NOW_L2B=$(cast chain-id --rpc-url "$L2B_RPC" 2>/dev/null || echo "?")
[[ "${STATE_L1_CHAINID:-}" == "$_NOW_L1" && "${STATE_L2B_CHAINID:-}" == "$_NOW_L2B" ]] \
    || fail "stale scenario state: fingerprint L1=${STATE_L1_CHAINID:-?}/L2B=${STATE_L2B_CHAINID:-?} != current L1=$_NOW_L1/L2B=$_NOW_L2B (re-run forward on THIS stack)"

for c in cast forge psql curl python3 docker; do command -v "$c" >/dev/null || fail "$c not found"; done
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable at $L1_RPC"

log "======================================================================"
log "  L2<->L2 BACK (Miden -> L2B): bridge-out wrapped OPT0, claim, net-zero"
log "======================================================================"
log "  OPT0=$OPT0  faucet=$OPT0_FAUCET_ID  wallet=$WALLET_ID"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
evidence_init
# rollup_register + exit-root baseline (so `back` standalone is also self-complete).
evidence_rollup_register "leg4"
evidence_exit_root "leg4" back pre-back
# NOTE: no cert-nudge token here. The reverse claim is a DIRECT proof-backed
# claimAsset on L2B (L2B autoclaim is intentionally out of scope), and the back GER
# reaches L2B via aggoracle-l2b watching L1 — neither needs an L2B->L1 nudge cert.
BACK_AMOUNT="$FWD_MIDEN_UNITS"     # forward credited exactly this
ADMIN_LOWER=$(echo "$ADMIN" | tr 'A-F' 'a-f')
LEG4_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BE_ROWS_BEFORE=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}';")

# Sanity: the wallet actually holds the wrapped OPT0 we intend to bridge back.
WRAPPED_NOW=$(iso_wallet_balance "$BRIDGE_ID" "$OPT0_FAUCET_ID"); WRAPPED_NOW="${WRAPPED_NOW:-0}"
[[ "$WRAPPED_NOW" -ge "$BACK_AMOUNT" ]] \
    || fail "wallet holds $WRAPPED_NOW wrapped OPT0, need $BACK_AMOUNT — run forward first"

step "Leg 4: bridge-out wrapped OPT0 (destNet=$L2B_NETWORK_ID) + claim on L2B"
iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$OPT0_FAUCET_ID" \
    --amount "$BACK_AMOUNT" --dest-address "$ADMIN" --dest-network "$L2B_NETWORK_ID" 2>&1 \
    || fail "bridge-out-tool failed (destNet=$L2B_NETWORK_ID)"
pass "B2AGG note created for wrapped OPT0 -> L2B"

# Bind the NEW BridgeEvent to THIS run's transfer — not just "a count bump plus any
# historical OPT0 row". The newest BridgeEvent's ABI data must, in ONE row, carry:
# origin token OPT0, destination ADMIN, destinationNetwork=2 (L2B), and the exact
# amount (Miden units -> origin wei). Amount is re-checked against the bridge-service
# deposit leaf below (EXPECTED_BACK_WEI), and cnt/global-index are pinned there too.
EXPECTED_BACK_WEI=$(python3 -c "print($BACK_AMOUNT * $WEI_PER_MIDEN_UNIT)")
BACK_AMT_HEX=$(python3 -c "print(format($EXPECTED_BACK_WEI, '064x'))")
DESTNET_HEX=$(python3 -c "print(format($L2B_NETWORK_ID, '064x'))")
wait_for "synthetic BridgeEvent row (PG count +1)" 300 5 \
    _pred_pg_gt "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}';" "${BE_ROWS_BEFORE:-0}"
NEW_BE=$(pgq "SELECT lower(data) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}' ORDER BY block_number DESC, log_index DESC LIMIT 1;")
[[ -n "$NEW_BE" ]]                            || fail "no synthetic BridgeEvent row found"
[[ "$NEW_BE" == *"${OPT0_HEX}"* ]]            || fail "newest BridgeEvent does not carry OPT0 origin ($OPT0_HEX)"
[[ "$NEW_BE" == *"${ADMIN_LOWER#0x}"* ]]      || fail "newest BridgeEvent does not carry ADMIN destination"
[[ "$NEW_BE" == *"${DESTNET_HEX}"* ]]         || fail "newest BridgeEvent does not carry destNet=$L2B_NETWORK_ID"
[[ "$NEW_BE" == *"${BACK_AMT_HEX}"* ]]        || fail "newest BridgeEvent does not carry amount $EXPECTED_BACK_WEI wei"
pass "synthetic BridgeEvent bound to this run: OPT0 origin, ADMIN dest, destNet=$L2B_NETWORK_ID, amount=$EXPECTED_BACK_WEI"

# Match a Settled cert with a NON-ZERO NewLocalExitRoot since LEG4_START. The old
# `0x[^2]` was doubly wrong: it REJECTED a valid root beginning with '2' and
# ACCEPTED the all-zero root (which starts with '0'). `0x0*[1-9a-f]` = optional
# leading zeros then at least one non-zero hex digit => any genuinely non-zero root
# (incl. ones starting with 2), never the zero root.
wait_for "Miden certificate settled on L1 (non-zero exit root, since leg4 start)" 900 10 \
    _pred_log_grep "$AGGKIT_CONTAINER" "$LEG4_START" "changed status.*Settled.*NewLocalExitRoot: 0x0*[1-9a-f]"
pass "certificate settled"
# CERTIFICATE SETTLEMENT (back): the Miden (network 1) cert whose settlement on
# L1 carried the back-bridge exit root. Grep the Miden aggsender, verify on L1.
evidence_settlement "leg4" back "$AGGKIT_CONTAINER" "$LEG4_START" 1 || true

wait_for "Miden->L2B deposit ready_for_claim" 600 5 \
    _pred_deposit_ready "$ADMIN" "$MIDEN_NETWORK_ID" "$OPT0_LOWER" "$L2B_NETWORK_ID"
BACK_DEPOSIT=$(find_deposit "$ADMIN" $MIDEN_NETWORK_ID "$OPT0_LOWER")
[[ -n "$BACK_DEPOSIT" ]] || fail "back deposit vanished from bridge-service"
BACK_CNT=$(dep_field "$BACK_DEPOSIT" deposit_cnt)
BACK_GI=$(dep_field "$BACK_DEPOSIT" global_index)
BACK_AMOUNT_WEI=$(dep_field "$BACK_DEPOSIT" amount)
log "  back deposit: cnt=$BACK_CNT globalIndex=$BACK_GI amount=$BACK_AMOUNT_WEI wei"
EXPECTED_BACK_WEI=$(python3 -c "print($BACK_AMOUNT * $WEI_PER_MIDEN_UNIT)")
[[ "$BACK_AMOUNT_WEI" == "$EXPECTED_BACK_WEI" ]] \
    || fail "back-bridge amount mismatch: exit leaf carries $BACK_AMOUNT_WEI wei, expected $EXPECTED_BACK_WEI"
# DEPOSIT (back): the Miden->L2B bridge-out (B2AGG). Created as a Miden note (no
# EVM tx); verified via the synthetic BridgeEvent row + bridge-service deposit.
evidence_record "leg4" back Miden deposit "" "" "$BRIDGE_ID" "bridged-out ready_for_claim" \
    "token=$OPT0 amountWei=$BACK_AMOUNT_WEI destNet=$L2B_NETWORK_ID globalIndex=$BACK_GI depositCnt=$BACK_CNT"
# GER INJECTION (back): aggoracle-l2b injects the new GER into the REAL L2B GER
# contract — a cast-receiptable L2B tx. Grep aggkit-l2b's aggoracle log.
# `|| true`: no-match grep must not abort under `set -e -o pipefail`.
BACK_GER_INJECT_TX=$( ( set +o pipefail; docker logs --since "$LEG4_START" "$AGGKIT_L2B_CONTAINER" 2>&1 | sed -r 's/\x1B\[[0-9;]*[mK]//g' \
    | grep -oiE 'inject GER transaction submitted with ID: 0x[0-9a-f]{64}' | tail -1 | grep -oiE '0x[0-9a-f]{64}' ) || true )
if [[ -n "$BACK_GER_INJECT_TX" ]]; then
    evidence_tx "leg4" back L2B ger_inject "$L2B_RPC" "$BACK_GER_INJECT_TX" "$L2B_GER" "source=aggoracle-l2b"
else
    warn "evidence: no aggoracle-l2b GER-inject tx found since $LEG4_START — ger_inject(back) not recorded"
fi

# CLAIM ON L2B — the reverse-path ACCEPTANCE is a direct, proof-backed
# `AgglayerBridgeL2.claimAsset` on L2B. L2B ClaimTxManager autoclaim is
# intentionally out of scope for this test (the upstream L2->L2 autoclaim only
# re-scans on an L1 rollup-exit-root update and can skip a deposit that turns ready
# between scans), so this is the expected reverse claim, NOT a fallback. The
# (dormant) autoclaim-adoption below is a pure defensive shortcut: if a claim tx
# ever did appear it is simply verified and reused; on this stack it never does,
# and the direct claimAsset runs.
CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')" 2>/dev/null || true)

if [[ -n "$CLAIM_TX_HASH" ]]; then
    log "  a claim tx already exists on L2B (tx $CLAIM_TX_HASH); verifying receipt..."
    RECEIPT_STATUS=$(cast receipt --rpc-url "$L2B_RPC" "$CLAIM_TX_HASH" status 2>/dev/null || echo "")
    [[ "$RECEIPT_STATUS" == *1* || "$RECEIPT_STATUS" == *true* ]] \
        || fail "L2B claim tx $CLAIM_TX_HASH receipt status not success: ${RECEIPT_STATUS:-<none>}"
    pass "claim on L2B verified (pre-existing claim tx)"
else
    log "  claiming the back deposit directly on L2B via claimAsset (proof-backed; fresh proof + retry until settleable)"
    ORIG_NET=$(dep_field "$BACK_DEPOSIT" orig_net)
    DEST_NET=$(dep_field "$BACK_DEPOSIT" dest_net)
    DEST_ADDR_CLAIM=$(dep_field "$BACK_DEPOSIT" dest_addr)
    METADATA_CLAIM=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
    # submit_back_claim (lib-l2l2.sh) runs the fresh-proof retry loop: fetch the Miden
    # deposit's proof, submit claimAsset to the real anvil-l2b, retry until the covering
    # GER is injected on L2B (~7.5 min budget). Shared with the mixed loadtest's back ops.
    CLAIM_TX_HASH=$(submit_back_claim "$BACK_CNT" "$BACK_GI" "$ORIG_NET" "$OPT0" \
        "$DEST_NET" "$DEST_ADDR_CLAIM" "$BACK_AMOUNT_WEI" "$METADATA_CLAIM") || true
    [[ -n "$CLAIM_TX_HASH" ]] || fail "manual claimAsset on L2B did not settle after 30 attempts (~7.5m)"
    pass "claim on L2B via robust manual claimAsset (tx $CLAIM_TX_HASH)"
fi
# CLAIM (back): on L2B via the AgglayerBridgeL2 proxy — cast-receiptable tx.
evidence_tx "leg4" back L2B claim "$L2B_RPC" "$CLAIM_TX_HASH" "$BRIDGE" \
    "globalIndex=$BACK_GI token=$OPT0 destNet=$L2B_NETWORK_ID"

# ── Net-zero round trip ──────────────────────────────────────────────────────
L2B_BAL_FINAL=$(cast call "$OPT0" 'balanceOf(address)(uint256)' "$ADMIN" --rpc-url "$L2B_RPC" | awk '{print $1}')
python3 -c "exit(0 if int('$L2B_BAL_FINAL') == int('$L2B_BAL_BEFORE_FORWARD') else 1)" \
    || fail "L2B round trip NOT net-zero: before-forward=$L2B_BAL_BEFORE_FORWARD final=$L2B_BAL_FINAL"
pass "L2B OPT0 holder restored: $L2B_BAL_FINAL (== pre-forward balance)"

WRAPPED_FINAL="$BACK_AMOUNT"
for attempt in $(seq 1 12); do
    WRAPPED_FINAL=$(iso_wallet_balance "$BRIDGE_ID" "$OPT0_FAUCET_ID"); WRAPPED_FINAL="${WRAPPED_FINAL:-0}"
    [[ "$WRAPPED_FINAL" -eq 0 ]] && break
    log "  attempt $attempt/12: wrapped OPT0 balance = $WRAPPED_FINAL (want 0)"
    sleep 10
done
[[ "$WRAPPED_FINAL" -eq 0 ]] || fail "Miden wrapped OPT0 not fully burned: $WRAPPED_FINAL remains"
pass "Miden wrapped OPT0 fully burned"
evidence_exit_root "leg4" back post-back-claim

# Final, DIRECTIONAL evidence gate for the whole group: require BOTH directions'
# deposit/ger_inject/cert_settlement/claim (forward evidence can't cover a missing
# back-direction record) plus rollup_register/deploy/exit_root (any direction). The
# summary rejects failed/receipt-less records and settlements that didn't hit the
# RollupManager, so a required (direction,kind) present only as a bad record fails.
evidence_summary \
    forward:deposit forward:ger_inject forward:cert_settlement forward:claim \
    back:deposit    back:ger_inject    back:cert_settlement    back:claim \
    rollup_register deploy exit_root

log "======================================================================"
log "  L2<->L2 BACK PASS — net-zero round trip L2B -> Miden -> L2B"
log "    back: $BACK_AMOUNT units -> $BACK_AMOUNT_WEI wei (gi $BACK_GI)"
log "    L2B holder net-zero: $L2B_BAL_BEFORE_FORWARD == $L2B_BAL_FINAL"
log "    evidence NDJSON:     $EVIDENCE_FILE"
log "======================================================================"
