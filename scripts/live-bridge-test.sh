#!/usr/bin/env bash
# ════════════════════════════════════════════════════════════════════════════
# Live bridge test — BOTH directions, the SAME two wallets, against a live
# 0.15.x install.
#
#   DIRECTION=l1-to-l2   (default)   Sepolia EOA ──ETH──▶ Miden target wallet
#   DIRECTION=l2-to-l1               Miden target wallet ──ETH──▶ Sepolia EOA
#
# Flow per direction:
#   1. submit the bridge tx on the source chain
#   2. watch the bridge API: appears → ready_for_claim → claimed
#      (the bridge API is keyed by the DESTINATION address)
#   3. confirm the funds landed on the destination wallet
#
# Config:  fixtures/live-test.env.  Needs: cast, python3, miden-client 0.15.x
# (init'd against the live node), and for L2→L1 the repo's bridge-out-tool +
# BRIDGE_ID (Miden bridge account) + FAUCET_ID (auto-derived after L1→L2).
#
# Usage:   ./scripts/live-bridge-test.sh                 # L1→L2
#          DIRECTION=l2-to-l1 ./scripts/live-bridge-test.sh
# ════════════════════════════════════════════════════════════════════════════
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="${ENV_FILE:-$HERE/../fixtures/live-test.env}"
# shellcheck disable=SC1090
source "$ENV_FILE"

DIRECTION="${DIRECTION:-${1:-l1-to-l2}}"
log(){ printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
die(){ printf '[%s] FATAL: %s\n' "$(date +%H:%M:%S)" "$*" >&2; exit 1; }

: "${SEPOLIA_RPC:?}"; : "${BRIDGE_SERVICE_URL:?}"; : "${BRIDGE_ADDRESS:?}"
: "${DEST_NETWORK:?}"; : "${LIVE_WALLET_KEY:?}"
TOKEN_ADDR="${TOKEN_ADDR:-0x0000000000000000000000000000000000000000}"
MIDEN_NODE_URL="${MIDEN_NODE_URL:-rpc.testnet.miden.io:443}"
MIDEN_STORE="${MIDEN_STORE:-$HOME/.miden}"
BRIDGE_OUT_TOOL="${BRIDGE_OUT_TOOL:-$HERE/../target/release/bridge-out-tool}"
EOA="$(cast wallet address --private-key "$LIVE_WALLET_KEY")"
# miden-client runs against an isolated store (HOME override) so we never touch ~/.miden
MIDEN_HOME="${MIDEN_HOME:-$HOME}"
mc(){ HOME="$MIDEN_HOME" miden-client "$@"; }

# ── shared helpers ───────────────────────────────────────────────────────────
ensure_target_wallet(){
    if [[ -z "${TARGET_WALLET_ID:-}" ]]; then
        log "creating a fresh public Miden wallet (target)..."
        mc sync >/dev/null 2>&1 || true
        local out; out="$(mc new-wallet --account-type public 2>&1)"
        TARGET_WALLET_ID="$(printf '%s' "$out" | grep -oE '0x[0-9a-f]+' | head -1)"
        [[ -n "$TARGET_WALLET_ID" ]] || die "wallet creation failed:\n$out"
        grep -q '^TARGET_WALLET_ID=' "$ENV_FILE" \
            && sed -i '' "s|^TARGET_WALLET_ID=.*|TARGET_WALLET_ID=$TARGET_WALLET_ID|" "$ENV_FILE" \
            || echo "TARGET_WALLET_ID=$TARGET_WALLET_ID" >> "$ENV_FILE"
        log "created + persisted target wallet: $TARGET_WALLET_ID"
    fi
    local id="${TARGET_WALLET_ID#0x}"
    [[ ${#id} -eq 30 ]] || log "WARN: account id ${#id} hex chars (expected 30)"
    DEST_ADDR="0x00000000${id}00"   # MASM to_account_id: 0x00000000||id(15B)||0x00
}

# bridge API counts for a destination address + origin network (0=L1, 1=Miden)
api_counts(){  # <dest_addr> <want_net> -> "total ready claimed"
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$1" 2>/dev/null | python3 -c '
import json,sys
net=int(sys.argv[1]); x=json.load(sys.stdin).get("deposits",[])
x=[d for d in x if d.get("network_id")==net]
print(len(x), sum(1 for i in x if i.get("ready_for_claim")),
      sum(1 for i in x if (i.get("claim_tx_hash") or "") not in ("","0x")))' "$2" 2>/dev/null || echo "0 0 0"
}

miden_balance_raw(){  # sync + consume owned notes, dump the account view
    mc sync >/dev/null 2>&1 || true
    mc consume-notes "$TARGET_WALLET_ID" >/dev/null 2>&1 || true
    mc account -s "$TARGET_WALLET_ID" 2>/dev/null
}

wait_for_claim(){  # <dest_addr> <net> <baseline_claimed>
    log "tracking bridge API for $1 (net $2)..."
    local end=$(( $(date +%s) + ${TIMEOUT:-1800} ))
    while (( $(date +%s) < end )); do
        read -r c r cl < <(api_counts "$1" "$2")
        log "  deposits=$c ready=$r claimed=$cl"
        [[ "${cl:-0}" -gt "${3:-0}" ]] && { log "✅ CLAIMED on the destination"; return 0; }
        sleep 20
    done
    return 1
}

# ── L1 → L2 ──────────────────────────────────────────────────────────────────
do_l1_to_l2(){
    ensure_target_wallet
    log "════ L1→L2: Sepolia $EOA → Miden $TARGET_WALLET_ID (dest $DEST_ADDR) ════"
    local amt="${AMOUNT:?set AMOUNT (wei)}"
    local bw; bw="$(cast balance --rpc-url "$SEPOLIA_RPC" "$EOA")"
    [[ "$bw" -gt "$amt" ]] || die "EOA underfunded ($bw wei)"
    log "Miden balance BEFORE:"; miden_balance_raw | sed 's/^/    /'
    read -r _ _ base < <(api_counts "$DEST_ADDR" 0)

    log "depositing $(cast from-wei "$amt") ETH on Sepolia → $DEST_ADDR ..."
    local tx; tx="$(cast send --rpc-url "$SEPOLIA_RPC" --private-key "$LIVE_WALLET_KEY" \
        --value "$amt" --json "$BRIDGE_ADDRESS" \
        "bridgeAsset(uint32,address,uint256,address,bool,bytes)" \
        "$DEST_NETWORK" "$DEST_ADDR" "$amt" "$TOKEN_ADDR" true 0x 2>&1 \
        | python3 -c 'import json,sys;print(json.load(sys.stdin).get("transactionHash",""))' 2>/dev/null)"
    [[ "$tx" == 0x* ]] || die "deposit tx failed: $tx"
    log "deposit tx: $tx"
    wait_for_claim "$DEST_ADDR" 0 "$base" || die "deposit not claimed within timeout"

    log "confirming funds on the Miden target wallet..."
    local end=$(( $(date +%s) + ${BAL_TIMEOUT:-900} ))
    while (( $(date +%s) < end )); do
        local out; out="$(miden_balance_raw)"
        if printf '%s' "$out" | grep -qiE '[1-9][0-9]*'; then
            log "✅ funds arrived. Miden balance AFTER:"; printf '%s\n' "$out" | sed 's/^/    /'
            # surface the faucet id so L2→L1 can reuse it
            local fid; fid="$(printf '%s' "$out" | grep -oE '0x[0-9a-f]{20,}' | head -1)"
            [[ -n "$fid" ]] && { log "asset faucet id = $fid (persisting as FAUCET_ID)"
                grep -q '^FAUCET_ID=' "$ENV_FILE" && sed -i '' "s|^FAUCET_ID=.*|FAUCET_ID=$fid|" "$ENV_FILE" || echo "FAUCET_ID=$fid" >> "$ENV_FILE"; }
            log "════ L1→L2 COMPLETE — tx $tx ════"; return 0
        fi
        sleep 20
    done
    die "funds did not arrive on Miden within timeout (tx $tx)"
}

# ── L2 → L1 ──────────────────────────────────────────────────────────────────
do_l2_to_l1(){
    ensure_target_wallet
    : "${BRIDGE_ID:?set BRIDGE_ID (Miden bridge account id) in live-test.env}"
    : "${FAUCET_ID:?set FAUCET_ID (auto-set by an L1→L2 run) in live-test.env}"
    : "${L2_AMOUNT:?set L2_AMOUNT (Miden asset units to bridge back)}"
    [[ -x "$BRIDGE_OUT_TOOL" ]] || die "bridge-out-tool not built: $BRIDGE_OUT_TOOL (cargo build --release --bin bridge-out-tool)"
    log "════ L2→L1: Miden $TARGET_WALLET_ID → Sepolia $EOA ════"
    local before; before="$(cast balance --rpc-url "$SEPOLIA_RPC" "$EOA")"
    log "EOA Sepolia balance BEFORE: $(cast from-wei "$before") ETH"
    read -r _ _ base < <(api_counts "$EOA" 1)

    log "bridging out $L2_AMOUNT units from Miden → $EOA (network 0)..."
    "$BRIDGE_OUT_TOOL" --store-dir "$MIDEN_STORE" --node-url "$MIDEN_NODE_URL" \
        --wallet-id "$TARGET_WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
        --amount "$L2_AMOUNT" --dest-address "$EOA" --dest-network 0 2>&1 | tee /tmp/l2l1.out | tail -8
    grep -qiE 'submitted|success|note' /tmp/l2l1.out || die "bridge-out failed (see output)"

    wait_for_claim "$EOA" 1 "$base" || log "WARN: not auto-claimed on L1 within timeout — may need a manual claimAsset on Sepolia"
    log "waiting for the EOA Sepolia balance to rise..."
    local end=$(( $(date +%s) + ${BAL_TIMEOUT:-900} ))
    while (( $(date +%s) < end )); do
        local now; now="$(cast balance --rpc-url "$SEPOLIA_RPC" "$EOA")"
        if [[ "$now" -gt "$before" ]]; then
            log "✅ funds arrived on Sepolia. EOA balance: $(cast from-wei "$now") ETH (was $(cast from-wei "$before"))"
            log "════ L2→L1 COMPLETE ════"; return 0
        fi
        sleep 20
    done
    die "EOA Sepolia balance did not rise within timeout"
}

# ── dispatch ─────────────────────────────────────────────────────────────────
[[ "$(cast chain-id --rpc-url "$SEPOLIA_RPC" 2>/dev/null)" == "11155111" ]] || die "Sepolia RPC not chain 11155111"
curl -sf "$BRIDGE_SERVICE_URL/bridges/$EOA" >/dev/null 2>&1 || die "bridge API unreachable"
case "$DIRECTION" in
    l1-to-l2) do_l1_to_l2 ;;
    l2-to-l1) do_l2_to_l1 ;;
    *) die "unknown DIRECTION '$DIRECTION' (use l1-to-l2 | l2-to-l1)" ;;
esac
