#!/usr/bin/env bash
# lib-isolated-wallet.sh — shared helpers for the ISOLATED bridge-out wallet
# pattern (sourced, not executed).
#
# POLICY: the proxy's sqlite store (/var/lib/miden-agglayer-service) has a
# SINGLE owner — the proxy process. Cross-process sharing is UNSUPPORTED.
# Every e2e bridge-out (and every wallet-hex/balance read that used to go
# through the proxy's store) therefore runs bridge-out-tool as a fully
# independent client in a throwaway container against its OWN sqlite store
# (a host bind mount). This mirrors production, where the B2AGG wallet is
# independent and the proxy's store has NO external accessor.
#
# Usage (from an e2e script, after PROJECT_DIR is set):
#   B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/<script-name>}"
#   source "$SCRIPT_DIR/lib-isolated-wallet.sh"
#   provision_isolated_wallet [<probe_bridge_id> <probe_faucet_id>]
#     → sets WALLET_ID / WALLET_HEX / DEST_ADDR (zero-padded L1→L2 dest)
#   iso_tool <bridge-out-tool args...>
#   iso_wallet_balance <bridge_id> <faucet_id>   → prints balance (or "")
#
# Store-dir convention: each script defaults to its OWN subdir under
# $PROJECT_DIR/.b2agg-store/ so runs stay independent. Coupled flows that
# must share one funded wallet (e2e-l1-to-l2 funds, e2e-l2-to-l1* spends)
# default to the same "e2e-suite" subdir. All of it is env-overridable via
# B2AGG_STORE_DIR.
#
# Env knobs (all overridable):
#   ISO_IMAGE       image containing bridge-out-tool (miden-agglayer-e2e:latest)
#   ISO_NETWORK     docker network of the e2e stack   (miden-e2e)
#   ISO_NODE_URL    miden-node gRPC URL on that net   (http://miden-node:57291)
#   ISO_PROVER_URL  remote tx prover URL              (http://tx-prover:50051)
#   B2AGG_STORE_DIR host dir bind-mounted at /store   (see convention above)
#   B2AGG_FRESH     1 = wipe any existing store before provisioning (default 0:
#                   reuse an already-provisioned wallet so multi-script runs
#                   like e2e-test.sh don't re-provision each time)

ISO_IMAGE="${ISO_IMAGE:-miden-agglayer-e2e:latest}"
ISO_NETWORK="${ISO_NETWORK:-miden-e2e}"
ISO_NODE_URL="${ISO_NODE_URL:-http://miden-node:57291}"
ISO_PROVER_URL="${ISO_PROVER_URL:-http://tx-prover:50051}"

if [[ -z "${PROJECT_DIR:-}" ]]; then
    echo "lib-isolated-wallet.sh: PROJECT_DIR must be set before sourcing" >&2
    return 1 2>/dev/null || exit 1
fi
# Default: per-caller subdir (BASH_SOURCE[1] is the sourcing script).
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/$(basename "${BASH_SOURCE[1]:-standalone}" .sh)}"

# iso_tool <args...> : run bridge-out-tool in a throwaway container against the
# ISOLATED store. TMPDIR is kept on the same bind-mounted device as the store
# to avoid rusqlite's "Invalid cross-device link" on atomic rename.
iso_tool() {
    docker run --rm --network "$ISO_NETWORK" \
        -v "$B2AGG_STORE_DIR:/store" \
        -e "MIDEN_PROVER_URL=$ISO_PROVER_URL" \
        -e "TMPDIR=/store/tmp" \
        --entrypoint bridge-out-tool \
        "$ISO_IMAGE" \
        --store-dir /store --node-url "$ISO_NODE_URL" \
        --miden-prover-url "$ISO_PROVER_URL" "$@"
}

# Remove the isolated store. Container-created files inside it are root-owned
# on the host, so fall back to a root busybox container when plain rm fails.
_iso_wipe_store() {
    [[ -e "$B2AGG_STORE_DIR" ]] || return 0
    if ! rm -rf "$B2AGG_STORE_DIR" 2>/dev/null; then
        local parent base
        parent="$(dirname "$B2AGG_STORE_DIR")"
        base="$(basename "$B2AGG_STORE_DIR")"
        docker run --rm -v "$parent:/work" busybox rm -rf "/work/$base"
    fi
}

# iso_wallet_balance <bridge_id> <faucet_id> → prints the isolated wallet's
# balance for <faucet_id> ("" when it could not be read). Side effect: the
# tool syncs and consumes any pending P2ID notes before the balance check;
# the absurd amount then makes the actual bridge-out fail, which is expected
# (hence the `|| true`).
iso_wallet_balance() {
    local out
    out=$(iso_tool \
        --wallet-id "$WALLET_ID" --bridge-id "$1" --faucet-id "$2" \
        --amount 999999999999999 --dest-address 0xdead --dest-network 0 2>&1 || true)
    # `|| true`: a no-match grep must not trip the caller's set -eo pipefail.
    echo "$out" | grep "wallet balance:" | head -1 | awk '{print $NF}' || true
}

# provision_isolated_wallet [<probe_bridge_id> <probe_faucet_id>]
#
# Ensures $B2AGG_STORE_DIR holds a provisioned, usable wallet and exports:
#   WALLET_ID   — the wallet account id (0x-hex, as printed by --create-wallet)
#   WALLET_HEX  — same value (kept for call-site compatibility)
#   DEST_ADDR   — zero-padded Ethereum address form for L1→L2 bridgeAsset
#
# Reuses an existing wallet when the store was provisioned by a prior run
# (wallet id persisted in $B2AGG_STORE_DIR/wallet-id). When probe ids are
# given, the reused store is validated with a balance probe against the LIVE
# stack; a failed probe (e.g. stale store from a previous chain) triggers a
# wipe + fresh provisioning. B2AGG_FRESH=1 forces a wipe up front.
provision_isolated_wallet() {
    local probe_bridge="${1:-}" probe_faucet="${2:-}"
    local idfile="$B2AGG_STORE_DIR/wallet-id"

    if [[ "${B2AGG_FRESH:-0}" == "1" ]]; then
        _iso_wipe_store
    fi
    mkdir -p "$B2AGG_STORE_DIR/tmp"

    WALLET_ID=""
    if [[ -s "$idfile" ]]; then
        WALLET_ID="$(cat "$idfile")"
        if [[ -n "$probe_bridge" && -n "$probe_faucet" ]]; then
            local bal
            bal=$(iso_wallet_balance "$probe_bridge" "$probe_faucet")
            if [[ -z "$bal" ]]; then
                echo "isolated-wallet: store at $B2AGG_STORE_DIR looks stale (balance probe failed) — re-provisioning fresh" >&2
                _iso_wipe_store
                mkdir -p "$B2AGG_STORE_DIR/tmp"
                WALLET_ID=""
            fi
        fi
    fi

    if [[ -z "$WALLET_ID" ]]; then
        local out
        if ! out=$(iso_tool --create-wallet 2>&1); then
            echo "$out" | tail -20 >&2
            echo "isolated-wallet: wallet provisioning failed" >&2
            return 1
        fi
        WALLET_ID=$(echo "$out" | grep "wallet-id:" | awk '{print $NF}' || true)
        if [[ -z "$WALLET_ID" ]]; then
            echo "$out" | tail -20 >&2
            echo "isolated-wallet: could not parse provisioned wallet id" >&2
            return 1
        fi
        echo "$WALLET_ID" > "$idfile"
    fi

    WALLET_HEX="$WALLET_ID"
    local inner="${WALLET_HEX#0x}"
    DEST_ADDR="0x00000000${inner:0:16}${inner:16:14}00"
}
