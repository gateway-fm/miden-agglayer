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

# CLAIM ON L2B — we claim the back deposit DIRECTLY via claimAsset (robust manual
# claim) rather than depending on the vendored bridge-service ClaimTxManager
# autoclaim. The upstream L2->L2 autoclaim only re-scans on an L1 rollup-exit-root
# update, so a deposit that turns ready BETWEEN scans can sit unclaimed
# indefinitely (an upstream timing quirk we deliberately do not gate the test on;
# fixing it is out of scope). The manual claim is still fully on-chain e2e: a real
# claimAsset against the bridge-service-served merkle proof + the aggoracle-injected
# L2B GER, with the real net-zero settlement asserted below. If the autoclaimer
# happened to beat us to it, we adopt that tx instead (also e2e).
CLAIM_TX_HASH=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; print(json.load(sys.stdin).get('claim_tx_hash') or '')" 2>/dev/null || true)

if [[ -n "$CLAIM_TX_HASH" ]]; then
    log "  autoclaimed on L2B (tx $CLAIM_TX_HASH); verifying receipt..."
    RECEIPT_STATUS=$(cast receipt --rpc-url "$L2B_RPC" "$CLAIM_TX_HASH" status 2>/dev/null || echo "")
    [[ "$RECEIPT_STATUS" == *1* || "$RECEIPT_STATUS" == *true* ]] \
        || fail "L2B autoclaim tx $CLAIM_TX_HASH receipt status not success: ${RECEIPT_STATUS:-<none>}"
    pass "claim on L2B via ClaimTxManager autoclaim"
else
    log "  claiming the back deposit directly on L2B via claimAsset (robust: fresh proof + retry until settleable)"
    ORIG_NET=$(dep_field "$BACK_DEPOSIT" orig_net)
    DEST_NET=$(dep_field "$BACK_DEPOSIT" dest_net)
    DEST_ADDR_CLAIM=$(dep_field "$BACK_DEPOSIT" dest_addr)
    METADATA_CLAIM=$(echo "$BACK_DEPOSIT" | python3 -c "import json,sys; m=json.load(sys.stdin).get('metadata','0x'); print(m if m and m != '0x' else '0x')")
    CLAIM_TX_HASH=""
    # The proof's roots and their L2B-GER injection LAG the deposit turning ready,
    # and the served proof advances as the exit tree grows. So rather than fetch one
    # proof and wait on one specific GER (racy), each attempt RE-FETCHES a fresh
    # proof and tries claimAsset. `cast send` gas-estimates first, so a not-yet-
    # settleable claim (covering GER not injected on L2B, or a stale sibling) reverts
    # in estimation and fails FAST without submitting -> retry with a newer proof.
    # Once the covering GER is injected on L2B, estimation passes and it settles.
    for attempt in $(seq 1 30); do   # ~7.5 min worst case
        PROOF_JSON=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$BACK_CNT&net_id=$MIDEN_NETWORK_ID" 2>/dev/null || true)
        if [[ -n "$PROOF_JSON" ]]; then
            MAINNET_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['main_exit_root'])" 2>/dev/null || true)
            ROLLUP_EXIT_ROOT=$(echo "$PROOF_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['proof']['rollup_exit_root'])" 2>/dev/null || true)
            SMT_LOCAL=$(echo "$PROOF_JSON" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
            SMT_ROLLUP=$(echo "$PROOF_JSON" | python3 -c "
import json,sys
p=json.load(sys.stdin)['proof']['rollup_merkle_proof']
while len(p)<32:p.append('0x'+'00'*32)
print('['+','.join(p[:32])+']')" 2>/dev/null || true)
            if [[ -n "$SMT_LOCAL" && -n "$SMT_ROLLUP" && -n "$MAINNET_EXIT_ROOT" ]]; then
                CLAIM_OUT=$(cast send --rpc-url "$L2B_RPC" --private-key "$ADMIN_KEY" --json "$BRIDGE" \
                    'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
                    "$SMT_LOCAL" "$SMT_ROLLUP" "$BACK_GI" "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
                    "$ORIG_NET" "$OPT0" "$DEST_NET" "$DEST_ADDR_CLAIM" "$BACK_AMOUNT_WEI" "$METADATA_CLAIM" 2>/dev/null || true)
                CLAIM_TX_HASH=$(echo "$CLAIM_OUT" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('transactionHash','') if str(d.get('status','')) in ('0x1','1','true') else '')" 2>/dev/null || true)
                [[ -n "$CLAIM_TX_HASH" ]] && break
            fi
        fi
        log "  attempt $attempt/30: back deposit not settleable on L2B yet (GER-injection/proof lag) — retrying in 15s"
        sleep 15
    done
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

evidence_summary

log "======================================================================"
log "  L2<->L2 BACK PASS — net-zero round trip L2B -> Miden -> L2B"
log "    back: $BACK_AMOUNT units -> $BACK_AMOUNT_WEI wei (gi $BACK_GI)"
log "    L2B holder net-zero: $L2B_BAL_BEFORE_FORWARD == $L2B_BAL_FINAL"
log "    evidence NDJSON:     $EVIDENCE_FILE"
log "======================================================================"
