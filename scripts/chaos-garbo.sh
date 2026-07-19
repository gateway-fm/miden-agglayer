#!/usr/bin/env bash
# chaos-garbo.sh — ADVERSARIAL "garbo" injector: at random intervals during the
# soak it fires JUNK / adversarial traffic at Miden using the SHIPPED tooling,
# each with a benign EXPECTED outcome (skipped / quarantined / never projected).
# The caller asserts CONTAINMENT afterwards: no garbo input ever became a real
# BridgeEvent/ClaimEvent, never advanced deposit_counter, and each provenance
# gate fired.
#
# Garbo classes (self-contained, no permanent corruption):
#   private   — bridge-out-tool --send-private-note: a PRIVATE tag-0 note the
#               note-visibility reconciler must SKIP (not wedge). Metric
#               synthetic_reconciler_private_skipped_total; never projected.
#               Doubles as the "random tag-0 spam" class (fired repeatedly).
#   foreign   — a fully independent FOREIGN agglayer deployment on the same
#               Miden chain drives a claim through ITS OWN bridge
#               (--create-foreign-bridge + --submit-foreign-claim). Our proxy's
#               provenance gate must skip it: claim_event_foreign_skipped_total;
#               ZERO synthetic_logs ClaimEvent rows for the foreign global index.
#
# (The "unknown-faucet quarantine" class is intentionally NOT automated here:
#  deleting a faucet_registry row does NOT reliably reach UnknownFaucet because
#  the metadata-recovery path REBUILDS the row from the bridge's authoritative
#  faucet_metadata_map (src/metadata_recovery.rs finding_6) — a genuinely
#  unknown faucet can't be produced with the shipped bridge-out-tool. See the
#  soak report punch list.)
#
# Emits: GARBO_LOG (human timeline) + GARBO_SUMMARY (env-parseable: counts,
# foreign global indexes, baselines) for the soak's containment verdict.
#
# Usage: GARBO_DURATION=600 ./scripts/chaos-garbo.sh
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

GARBO_DURATION="${GARBO_DURATION:-600}"
GARBO_MIN_GAP="${GARBO_MIN_GAP:-30}"
GARBO_MAX_GAP="${GARBO_MAX_GAP:-70}"
GARBO_LOG="${GARBO_LOG:-/tmp/chaos-garbo.log}"
GARBO_SUMMARY="${GARBO_SUMMARY:-/tmp/chaos-garbo-summary.env}"
GARBO_FOREIGN="${GARBO_FOREIGN:-1}"     # fire the (heavy) foreign-claim class once
FOREIGN_NETWORK_ID="${FOREIGN_NETWORK_ID:-3}"   # an id our stack does NOT serve (1=Miden,2=L2B)
SEED="${GARBO_SEED:-$$}"; RANDOM=$SEED

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/chaos-garbo}"
GARBO_WALLET_STORE="$B2AGG_STORE_DIR"
FOREIGN_STORE="$PROJECT_DIR/.b2agg-store/chaos-garbo-foreign"

source "$SCRIPT_DIR/lib-l2l2.sh"          # constants, pgq, log helpers, containers
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

FUNDED_KEY="${FUNDED_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
DEST_NETWORK=1
FUND_WEI="${FUND_WEI:-100000000000000}"        # 1e14 wei -> 10000 units for private-note sends
DEPOSIT_AMOUNT="10000000000000"                # foreign-claim leaf amount (10^13 wei)

glog() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$GARBO_LOG"; }

counter() {
    local name="$1" body value
    body=$(curl -sf "${L2_RPC}/metrics" 2>/dev/null) || { echo 0; return; }
    value=$(awk -v n="$name" '$0 ~ ("^" n " ") { print $2; found=1; exit } END { if (!found) print 0 }' <<<"$body")
    echo "${value%.*}"
}

PRIVATE_FIRED=0
FOREIGN_FIRED=0
FOREIGN_GIS=""
PRIVATE_ATTEMPTS=0
FOREIGN_ATTEMPTS=0
START=0   # set before the fire window opens; used by the retry helpers

# ── #41: the injectors must actually FIRE under faults ───────────────────────
# Under chaos (proxy restarts, pg pauses) a one-shot injection attempt fails and
# a whole run used to end with private=0 foreign=0, so the soak's containment
# check (c) could never assert. Every injection now (a) waits for the proxy to
# be reachable, (b) retries the SAME op every ~10s until it lands or the
# GARBO_DURATION window runs out. Attempts vs fired are both reported.
window_remaining() {
    local now; now=$(date +%s)
    echo $(( GARBO_DURATION - (now - START) ))
}

proxy_ready() {
    curl -sf -m 3 "${L2_RPC}/metrics" >/dev/null 2>&1
}

wait_proxy_ready() {
    while [ "$(window_remaining)" -gt 0 ]; do
        proxy_ready && return 0
        sleep 5
    done
    return 1
}

# retry_until_landed <fn> — re-run <fn> (a garbo class that returns 0 iff the
# injection landed) every ~10s, gated on proxy reachability, until it lands or
# the window closes.
retry_until_landed() {
    local fn="$1"
    while [ "$(window_remaining)" -gt 0 ]; do
        wait_proxy_ready || return 1
        if "$fn"; then return 0; fi
        glog "GARBO retry: $fn did not land — retrying in 10s ($(window_remaining)s left)"
        sleep 10
    done
    return 1
}

# ── setup: provision + fund a garbo wallet (for private-note sends) ──────────
setup_garbo() {
    local accounts=""
    for _ in $(seq 1 30); do
        accounts=$(docker exec "$AGGLAYER_CONTAINER" \
            cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) && break
        sleep 5
    done
    [[ -n "$accounts" ]] || { glog "GARBO setup FAILED: bridge_accounts.toml absent"; return 1; }
    BRIDGE_ID=$(echo "$accounts" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
    FAUCET_ETH=$(echo "$accounts" | grep faucet_eth | sed 's/.*= "//;s/"//')
    B2AGG_STORE_DIR="$GARBO_WALLET_STORE"
    provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH" || { glog "GARBO wallet provisioning failed"; return 1; }
    glog "garbo wallet $WALLET_ID (store $GARBO_WALLET_STORE)"
    local bal; bal=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ETH"); bal="${bal:-0}"
    if [[ "$bal" -eq 0 ]]; then
        glog "funding garbo wallet via L1->L2 native deposit ($FUND_WEI wei)"
        cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" "$BRIDGE_ADDRESS" \
            'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
            "$DEST_NETWORK" "$DEST_ADDR" "$FUND_WEI" \
            0x0000000000000000000000000000000000000000 true 0x --value "$FUND_WEI" >/dev/null 2>&1
        for _ in $(seq 1 30); do
            sleep 10
            bal=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ETH"); bal="${bal:-0}"
            [[ "$bal" -gt 0 ]] && break
        done
    fi
    [[ "$bal" -gt 0 ]] || { glog "GARBO wallet not funded (bal=$bal) — private-note class disabled"; return 1; }
    GARBO_FAUCET_ETH="$FAUCET_ETH"
    glog "garbo wallet funded (balance $bal)"
    return 0
}

# ── class: private / tag-0 spam note ─────────────────────────────────────────
garbo_private_note() {
    B2AGG_STORE_DIR="$GARBO_WALLET_STORE"
    local out note_id
    PRIVATE_ATTEMPTS=$((PRIVATE_ATTEMPTS + 1))
    out=$(iso_tool --send-private-note --wallet-id "$WALLET_ID" 2>&1) || {
        glog "GARBO private-note: send FAILED (transient?) — $(echo "$out" | tail -1)"; return 1; }
    note_id=$(echo "$out" | grep '\[private-note\] note-id:' | awk '{print $NF}')
    PRIVATE_FIRED=$((PRIVATE_FIRED + 1))
    glog "GARBO private-note #$PRIVATE_FIRED id=${note_id:-?} — EXPECT: reconciler skips (synthetic_reconciler_private_skipped_total++), NEVER projected as a BridgeEvent/ClaimEvent"
}

# ── class: foreign-deployment claim (provenance gate) ────────────────────────
garbo_foreign_claim() {
    B2AGG_STORE_DIR="$FOREIGN_STORE"
    FOREIGN_ATTEMPTS=$((FOREIGN_ATTEMPTS + 1))
    _iso_wipe_store; mkdir -p "$B2AGG_STORE_DIR/tmp"
    local fb_out fs fg fbid ffaucet
    fb_out=$(iso_tool --create-foreign-bridge --foreign-network-id "$FOREIGN_NETWORK_ID" 2>&1) || {
        glog "GARBO foreign-claim: --create-foreign-bridge FAILED — $(echo "$fb_out" | tail -2)"; return 1; }
    fs=$(echo "$fb_out" | grep "service-id:" | awk '{print $NF}')
    fg=$(echo "$fb_out" | grep "ger-manager-id:" | awk '{print $NF}')
    fbid=$(echo "$fb_out" | grep -w "bridge-id:" | awk '{print $NF}')
    [[ -n "$fs" && -n "$fg" && -n "$fbid" ]] || { glog "GARBO foreign-claim: could not parse foreign ids"; return 1; }
    local fs_inner="${fs#0x}"
    local fdest="0x00000000${fs_inner:0:16}${fs_inner:16:14}00"

    # Fabricate the foreign leaf + depth-32 proof + exit roots (see e2e-claim-provenance.sh).
    local dcnt gi zero empty_meta amt_hex leaf_packed leaf node idx mner rer smt calldata
    dcnt=$(date +%s); gi=$(python3 -c "print(2**64 + $dcnt)")
    zero="$(printf '0%.0s' {1..64})"
    empty_meta=$(cast keccak 0x)
    amt_hex=$(printf '%064x' "$DEPOSIT_AMOUNT")
    leaf_packed="0x00$(printf '%08x' 0)$(printf '0%.0s' {1..40})$(printf '%08x' "$FOREIGN_NETWORK_ID")${fdest#0x}${amt_hex}${empty_meta#0x}"
    [[ ${#leaf_packed} -eq 228 ]] || { glog "GARBO foreign-claim: bad packed leaf len"; return 1; }
    leaf=$(cast keccak "$leaf_packed")
    node="${leaf#0x}"; idx="$dcnt"
    for _ in $(seq 1 32); do
        if (( idx & 1 )); then node=$(cast keccak "0x${zero}${node}"); else node=$(cast keccak "0x${node}${zero}"); fi
        node="${node#0x}"; idx=$(( idx >> 1 ))
    done
    mner="0x${node}"; rer="0x${zero}"
    smt=$(python3 -c "print('[' + ','.join(['0x' + '00'*32]*32) + ']')")
    calldata=$(cast calldata \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$smt" "$smt" "$gi" "$mner" "$rer" 0 0x0000000000000000000000000000000000000000 \
        "$FOREIGN_NETWORK_ID" "$fdest" "$DEPOSIT_AMOUNT" 0x)
    echo "$calldata" > "$B2AGG_STORE_DIR/foreign-claim-calldata.hex"
    local fc_out fgi
    fc_out=$(iso_tool --submit-foreign-claim \
        --claim-calldata-file /store/foreign-claim-calldata.hex \
        --foreign-bridge-id "$fbid" --foreign-service-id "$fs" --foreign-ger-manager-id "$fg" \
        --scale-exp 10 2>&1) || { glog "GARBO foreign-claim: --submit-foreign-claim FAILED — $(echo "$fc_out" | tail -3)"; return 1; }
    fgi=$(echo "$fc_out" | grep "global-index:" | awk '{print $NF}')
    [[ -n "$fgi" ]] || { glog "GARBO foreign-claim: could not parse foreign global index"; return 1; }
    FOREIGN_FIRED=$((FOREIGN_FIRED + 1))
    FOREIGN_GIS="$FOREIGN_GIS ${fgi#0x}"
    glog "GARBO foreign-claim #$FOREIGN_FIRED bridge=$fbid net=$FOREIGN_NETWORK_ID gi=$fgi — EXPECT: our proxy skips it (claim_event_foreign_skipped_total++), ZERO synthetic_logs ClaimEvent rows for gi ${fgi#0x}"
    B2AGG_STORE_DIR="$GARBO_WALLET_STORE"
}

write_summary() {
    {
        echo "# chaos-garbo summary $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "GARBO_PRIVATE_FIRED=$PRIVATE_FIRED"
        echo "GARBO_FOREIGN_FIRED=$FOREIGN_FIRED"
        echo "GARBO_FOREIGN_GIS=\"$(echo $FOREIGN_GIS | xargs)\""
        echo "GARBO_PRIVATE_ATTEMPTS=$PRIVATE_ATTEMPTS"
        echo "GARBO_FOREIGN_ATTEMPTS=$FOREIGN_ATTEMPTS"
    } > "$GARBO_SUMMARY"
    glog "summary -> $GARBO_SUMMARY (private=$PRIVATE_FIRED foreign=$FOREIGN_FIRED)"
}
trap write_summary EXIT

: > "$GARBO_LOG"
glog "=== chaos-garbo start (dur=${GARBO_DURATION}s seed=$SEED foreign=$GARBO_FOREIGN net=$FOREIGN_NETWORK_ID) ==="
if ! setup_garbo; then
    glog "setup incomplete — will retry setup inside the fire window (#41)"
fi

START=$(date +%s)
# Fire the heavy foreign-claim class ONCE early (it needs several minutes).
# #41: retried until it actually lands (or the window closes) — a one-shot
# attempt during a proxy restart used to end the run with foreign=0.
if [[ "$GARBO_FOREIGN" == "1" ]]; then
    retry_until_landed garbo_foreign_claim \
        || glog "GARBO foreign-claim: window closed before it landed (attempts=$FOREIGN_ATTEMPTS)"
fi
# Then spam private / tag-0 notes at random intervals for the rest of the window.
# #41: each slot retries the SAME note until it lands; a run that got wallet
# setup interrupted retries setup here instead of firing nothing forever.
while [ $(( $(date +%s) - START )) -lt "$GARBO_DURATION" ]; do
    gap=$(( GARBO_MIN_GAP + RANDOM % (GARBO_MAX_GAP - GARBO_MIN_GAP + 1) ))
    sleep "$gap"
    [ $(( $(date +%s) - START )) -ge "$GARBO_DURATION" ] && break
    if [[ -z "${WALLET_ID:-}" ]]; then
        wait_proxy_ready && setup_garbo || glog "GARBO setup retry failed — will retry next slot"
        continue
    fi
    retry_until_landed garbo_private_note || break
done
glog "=== chaos-garbo done: private=$PRIVATE_FIRED foreign=$FOREIGN_FIRED ==="
glog "garbo attempts vs fired: private=$PRIVATE_ATTEMPTS/$PRIVATE_FIRED foreign=$FOREIGN_ATTEMPTS/$FOREIGN_FIRED"
# EXIT trap writes the summary.
