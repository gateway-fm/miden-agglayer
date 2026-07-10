#!/usr/bin/env bash
# e2e-l2l2-back.sh — SIMPLE, DETERMINISTIC L2<->L2 back scenario
# ("l2-to-l2-back"), decomposed from e2e-l2-to-l2.sh leg 4.
#
# Precondition: e2e-l2l2-forward.sh ran and left a Miden wallet holding wrapped
# OPT0 (state file written to the shared isolated store).
#
#   bridge-out wrapped OPT0 Miden -> L2B (destNet=2)
#   -> certificate settle -> Miden->L2B deposit ready_for_claim
#   -> claim on L2B (ClaimTxManager autoclaim; manual claimAsset fallback)
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
[[ -n "${OPT0:-}" && -n "${OPT0_FAUCET_ID:-}" && -n "${WALLET_ID:-}" && -n "${BRIDGE_ID:-}" ]] \
    || fail "scenario state incomplete: $STATE_FILE"

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
l2l2_deploy_nudge_token
evidence_tx "leg4" back L2B deploy "$L2B_RPC" "$NDG_DEPLOY_TX" "$NDG" "token=NDG role=cert-nudge"

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

# Synthetic BridgeEvent must carry origin (OPT0, net 2).
wait_for "synthetic BridgeEvent row (PG count +1)" \
    "[ \"\$(pgq \"SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}';\")\" -gt \"${BE_ROWS_BEFORE:-0}\" ]" \
    300 5
BE_ORIGIN_OK=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${BRIDGE_EVENT_TOPIC}' AND lower(data) LIKE '%${OPT0_HEX}%';")
[[ "${BE_ORIGIN_OK:-0}" -ge 1 ]] || fail "no BridgeEvent row carries the OPT0 origin address"
pass "synthetic BridgeEvent carries OPT0 origin"

wait_for "Miden certificate settled on L1" \
    "docker logs --since $LEG4_START $AGGKIT_CONTAINER 2>&1 | grep -q 'changed status.*Settled.*NewLocalExitRoot: 0x[^2]'" \
    900 10
pass "certificate settled"
# CERTIFICATE SETTLEMENT (back): the Miden (network 1) cert whose settlement on
# L1 carried the back-bridge exit root. Grep the Miden aggsender, verify on L1.
evidence_settlement "leg4" back "$AGGKIT_CONTAINER" "$LEG4_START" 1 || true

wait_for "Miden->L2B deposit ready_for_claim" \
    "find_deposit '$ADMIN' $MIDEN_NETWORK_ID '$OPT0_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('ready_for_claim') and d.get('dest_net')==$L2B_NETWORK_ID else 1)\"" \
    600 5
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

# Wake the rollupID-2 claim scan on L2B (non-fatal — manual fallback covers a miss).
nudge_until "L2B autoclaim of the back deposit" \
    "find_deposit '$ADMIN' $MIDEN_NETWORK_ID '$OPT0_LOWER' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if d.get('claim_tx_hash') else 1)\"" \
    || warn "autoclaim scan not woken by nudges — relying on the manual-claim fallback"

CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')")
if [[ -z "$CLAIM_TX_HASH" ]]; then
    log "  waiting up to 180s for ClaimTxManager autoclaim on L2B..."
    for _ in $(seq 1 36); do
        sleep 5
        BACK_DEPOSIT=$(find_deposit "$ADMIN" $MIDEN_NETWORK_ID "$OPT0_LOWER")
        CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')" 2>/dev/null || true)
        [[ -n "$CLAIM_TX_HASH" ]] && break
        echo -n "."
    done
    echo ""
fi

if [[ -n "$CLAIM_TX_HASH" ]]; then
    log "  autoclaimed on L2B (tx $CLAIM_TX_HASH); verifying receipt..."
    RECEIPT_STATUS=$(cast receipt --rpc-url "$L2B_RPC" "$CLAIM_TX_HASH" status 2>/dev/null || echo "")
    [[ "$RECEIPT_STATUS" == *1* || "$RECEIPT_STATUS" == *true* ]] \
        || fail "L2B autoclaim tx $CLAIM_TX_HASH receipt status not success: ${RECEIPT_STATUS:-<none>}"
    pass "claim on L2B via ClaimTxManager autoclaim"
else
    warn "no autoclaim within 180s — claiming manually on L2B"
    PROOF_JSON=""
    for _ in $(seq 1 18); do
        PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$BACK_CNT&net_id=$MIDEN_NETWORK_ID" 2>/dev/null || true)
        [[ -n "$PROOF_JSON" ]] && break
        sleep 5
    done
    [[ -n "$PROOF_JSON" ]] || fail "could not fetch merkle proof for back deposit after 90s"
    MAINNET_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])")
    ROLLUP_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])")
    SMT_LOCAL=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')")
    SMT_ROLLUP=$(echo "$PROOF_JSON" | python3 -c "
import json, sys
p = json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p) < 32: p.append('0x' + '00' * 32)
print('[' + ','.join(p[:32]) + ']')")
    BACK_GER=$(cast keccak "0x${MAINNET_EXIT_ROOT#0x}${ROLLUP_EXIT_ROOT#0x}")
    wait_for "GER $BACK_GER injected into L2B AgglayerGERL2 (aggoracle-l2b)" \
        "_g=\$(cast call $L2B_GER 'globalExitRootMap(bytes32)(uint256)' $BACK_GER --rpc-url '$L2B_RPC' 2>/dev/null | awk '{print \$1}'); [ -n \"\$_g\" ] && [ \"\$_g\" != \"0\" ]" \
        300 5
    ORIG_NET=$(dep_field "$BACK_DEPOSIT" orig_net)
    DEST_NET=$(dep_field "$BACK_DEPOSIT" dest_net)
    DEST_ADDR_CLAIM=$(dep_field "$BACK_DEPOSIT" dest_addr)
    METADATA_CLAIM=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
    CLAIM_TX=$(cast send --rpc-url "$L2B_RPC" --private-key "$ADMIN_KEY" \
        "$BRIDGE" \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$SMT_LOCAL" "$SMT_ROLLUP" "$BACK_GI" \
        "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
        "$ORIG_NET" "$OPT0" "$DEST_NET" "$DEST_ADDR_CLAIM" \
        "$BACK_AMOUNT_WEI" "$METADATA_CLAIM" 2>&1) || true
    STATUS=$(printf '%s\n' "$CLAIM_TX" | awk '$1=="status"{print $2; exit}')
    [[ "$STATUS" == "1" ]] || { warn "L2B claim tx output: $CLAIM_TX"; fail "manual claimAsset on L2B failed"; }
    CLAIM_TX_HASH=$(printf '%s\n' "$CLAIM_TX" | awk '$1=="transactionHash"{print $2; exit}')
    pass "claim on L2B via manual claimAsset"
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

evidence_summary

log "======================================================================"
log "  L2<->L2 BACK PASS — net-zero round trip L2B -> Miden -> L2B"
log "    back: $BACK_AMOUNT units -> $BACK_AMOUNT_WEI wei (gi $BACK_GI)"
log "    L2B holder net-zero: $L2B_BAL_BEFORE_FORWARD == $L2B_BAL_FINAL"
log "    evidence NDJSON:     $EVIDENCE_FILE"
log "======================================================================"
