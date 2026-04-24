#!/usr/bin/env bash
# Top up the disposable bridge wallet from FUNDER_PRIVATE_KEY when its balance
# is below what's needed for the next deposit (amount + gas buffer).
#
# Reads from env (loaded by the skill before invoking this script):
#   FUNDER_PRIVATE_KEY            — funded Sepolia EOA (USER provides)
#   BRIDGE_WALLET_PRIVATE_KEY     — disposable EOA used for the deposit
#                                   (generated + persisted to .env.local on first run)
#   SEPOLIA_RPC_URL               — RPC with eth_sendRawTransaction
#   AMOUNT_ETH                    — deposit amount; default 0.001
#   GAS_BUFFER_ETH                — extra balance for gas; default 0.005
#   ENV_LOCAL_FILE                — path to .env.local for persisting BRIDGE_WALLET_PRIVATE_KEY
#
# On success: BRIDGE_WALLET_PRIVATE_KEY is exported, balance is at least
# AMOUNT_ETH + GAS_BUFFER_ETH, and the funding tx (if any) is printed.

set -euo pipefail

: "${FUNDER_PRIVATE_KEY:?FUNDER_PRIVATE_KEY must be set in your shell or .env.local}"
: "${SEPOLIA_RPC_URL:?SEPOLIA_RPC_URL must be set in your shell or .env.local}"
: "${ENV_LOCAL_FILE:?ENV_LOCAL_FILE must be set by the caller}"
AMOUNT_ETH="${AMOUNT_ETH:-0.001}"
GAS_BUFFER_ETH="${GAS_BUFFER_ETH:-0.005}"

command -v cast >/dev/null || { echo "ERROR: foundry's 'cast' not on PATH" >&2; exit 1; }

# ── 1. Ensure a bridge-wallet key exists; generate + persist if not ──────────
if [[ -z "${BRIDGE_WALLET_PRIVATE_KEY:-}" ]]; then
    echo "[fund] no BRIDGE_WALLET_PRIVATE_KEY found — generating a fresh one"
    BRIDGE_WALLET_PRIVATE_KEY="$(cast wallet new --json | jq -r '.[0].private_key')"
    [[ "$BRIDGE_WALLET_PRIVATE_KEY" =~ ^0x[0-9a-fA-F]{64}$ ]] \
        || { echo "ERROR: cast wallet new produced unexpected output" >&2; exit 1; }
    {
        echo
        echo "# Generated $(date -u +%Y-%m-%dT%H:%M:%SZ) — disposable Sepolia bridge wallet"
        echo "BRIDGE_WALLET_PRIVATE_KEY=\"$BRIDGE_WALLET_PRIVATE_KEY\""
    } >> "$ENV_LOCAL_FILE"
    chmod 600 "$ENV_LOCAL_FILE"
    export BRIDGE_WALLET_PRIVATE_KEY
fi

FUNDER_ADDR="$(cast wallet address "$FUNDER_PRIVATE_KEY")"
BRIDGE_ADDR="$(cast wallet address "$BRIDGE_WALLET_PRIVATE_KEY")"

# ── 2. Compute required and current balance ─────────────────────────────────
AMOUNT_WEI="$(cast --to-wei "$AMOUNT_ETH" eth)"
BUFFER_WEI="$(cast --to-wei "$GAS_BUFFER_ETH" eth)"
NEEDED_WEI="$(python3 -c "print(int('$AMOUNT_WEI') + int('$BUFFER_WEI'))")"
NEEDED_ETH="$(cast --from-wei "$NEEDED_WEI" eth)"

CURRENT_WEI="$(cast balance "$BRIDGE_ADDR" --rpc-url "$SEPOLIA_RPC_URL")"
CURRENT_ETH="$(cast --from-wei "$CURRENT_WEI" eth)"

cat <<EOF
[fund] Funder address  : $FUNDER_ADDR
[fund] Bridge wallet   : $BRIDGE_ADDR
[fund] Needed          : $NEEDED_ETH ETH ($NEEDED_WEI wei)  [deposit + gas buffer]
[fund] Current balance : $CURRENT_ETH ETH ($CURRENT_WEI wei)
EOF

# ── 3. Top up if short ──────────────────────────────────────────────────────
if (( $(python3 -c "print(1 if int('$CURRENT_WEI') >= int('$NEEDED_WEI') else 0)") )); then
    echo "[fund] bridge wallet already has enough — no top-up needed"
    exit 0
fi

SHORT_WEI="$(python3 -c "print(int('$NEEDED_WEI') - int('$CURRENT_WEI'))")"
SHORT_ETH="$(cast --from-wei "$SHORT_WEI" eth)"

# Fund a little extra so we don't hit the same threshold next run.
TOPUP_WEI="$(python3 -c "print(int('$SHORT_WEI') + int('$BUFFER_WEI'))")"
TOPUP_ETH="$(cast --from-wei "$TOPUP_WEI" eth)"

FUNDER_BAL_WEI="$(cast balance "$FUNDER_ADDR" --rpc-url "$SEPOLIA_RPC_URL")"
if (( $(python3 -c "print(1 if int('$FUNDER_BAL_WEI') < int('$TOPUP_WEI') else 0)") )); then
    FUNDER_BAL_ETH="$(cast --from-wei "$FUNDER_BAL_WEI" eth)"
    echo "ERROR: funder $FUNDER_ADDR has $FUNDER_BAL_ETH ETH; needs $TOPUP_ETH ETH" >&2
    exit 1
fi

echo "[fund] short by $SHORT_ETH ETH — sending $TOPUP_ETH ETH from funder"
TX_JSON="$(cast send "$BRIDGE_ADDR" \
    --value "$TOPUP_WEI" \
    --private-key "$FUNDER_PRIVATE_KEY" \
    --rpc-url "$SEPOLIA_RPC_URL" \
    --json)"
TX_HASH="$(echo "$TX_JSON" | jq -r '.transactionHash // empty')"
[[ -n "$TX_HASH" ]] || { echo "ERROR: cast send returned no tx hash" >&2; echo "$TX_JSON" >&2; exit 1; }

echo "[fund] funding tx: $TX_HASH"
echo "[fund] waiting for confirmation..."
cast receipt "$TX_HASH" --rpc-url "$SEPOLIA_RPC_URL" --confirmations 1 >/dev/null

NEW_BAL_WEI="$(cast balance "$BRIDGE_ADDR" --rpc-url "$SEPOLIA_RPC_URL")"
NEW_BAL_ETH="$(cast --from-wei "$NEW_BAL_WEI" eth)"
echo "[fund] bridge wallet balance now: $NEW_BAL_ETH ETH"
