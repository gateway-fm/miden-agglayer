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

# iso_inspect_faucet <faucet_id> → resolve a faucet's WALLET-RESOLVABLE display
# metadata (symbol/decimals) on a FRESH client store with NO preloaded token map —
# exactly what a cold receiving wallet does to render an incoming fungible P2ID
# asset (#147). Echoes the tool's `inspect-faucet: faucet_id=.. symbol=.. decimals=..`
# line on success (rc 0); returns NON-ZERO (echoing the ERROR line) when the metadata
# is unresolvable (the wallet's `Unknown`) or the RPC fails — never a silent empty.
# A throwaway store dir on the same bind-mount device is created + wiped each call.
iso_inspect_faucet() {
    local fid="$1" fresh rc out
    fresh="$B2AGG_STORE_DIR/inspect-$$-${RANDOM}"
    mkdir -p "$fresh/tmp"
    # Capture the container's exit code WITHOUT tripping the caller's set -e: a bare
    # `out=$(docker ...); rc=$?` aborts under errexit before rc is read on nonzero.
    if out=$(docker run --rm --network "$ISO_NETWORK" \
        -v "$fresh:/store" -e "TMPDIR=/store/tmp" \
        --entrypoint bridge-out-tool "$ISO_IMAGE" \
        --store-dir /store --node-url "$ISO_NODE_URL" \
        --inspect-faucet "$fid" 2>&1); then rc=0; else rc=$?; fi
    # container-created files are root-owned; wipe via busybox if plain rm fails.
    rm -rf "$fresh" 2>/dev/null || docker run --rm -v "$B2AGG_STORE_DIR:/w" busybox rm -rf "/w/$(basename "$fresh")" >/dev/null 2>&1 || true
    echo "$out"
    return "$rc"
}

# assert_faucet_symbol <faucet_id> <expected_symbol> <expected_decimals> <origin-label>
# → inspect the faucet on a fresh client and fail (with full diagnostics) unless it
# resolves to exactly <expected_symbol>/<expected_decimals>. #147: a fresh wallet must
# NOT render this asset as `Unknown`. Exports INSPECT_SYMBOL/INSPECT_DECIMALS.
assert_faucet_symbol() {
    local fid="$1" want_sym="$2" want_dec="$3" label="$4" insp
    insp=$(iso_inspect_faucet "$fid") \
        || fail "#147: faucet metadata UNRESOLVABLE (a cold wallet would show 'Unknown') for $label faucet=$fid — $insp"
    INSPECT_SYMBOL=$(printf '%s\n' "$insp" | grep -oE 'symbol=[^ ]+' | head -1 | cut -d= -f2)
    INSPECT_DECIMALS=$(printf '%s\n' "$insp" | grep -oE 'decimals=[0-9]+' | head -1 | cut -d= -f2)
    [[ "$INSPECT_SYMBOL" == "$want_sym" && "$INSPECT_DECIMALS" == "$want_dec" ]] \
        || fail "#147: $label faucet must resolve $want_sym/$want_dec but a fresh client got symbol='$INSPECT_SYMBOL' decimals='$INSPECT_DECIMALS' (faucet=$fid; full: $insp)"
    pass "#147: $label faucet $fid resolves $INSPECT_SYMBOL/$INSPECT_DECIMALS from public account state (cold-wallet safe)"
}

# iso_wallet_faucets → run bridge-out-tool --list-wallet-faucets: sync the isolated wallet,
# CONSUME its pending P2ID notes, then print its on-chain vault holdings as one
# "faucet_id amount" line per held fungible faucet (id lowercased). Errexit-safe. Empty
# output with rc 0 = the wallet holds nothing yet (valid). A tool run that does not reach
# the `done` terminator is a hard rc!=0 (a truncated run must not read as "held nothing").
iso_wallet_faucets() {
    local out rc
    if out=$(iso_tool --wallet-id "$WALLET_ID" --list-wallet-faucets 2>&1); then rc=0; else rc=$?; fi
    printf '%s\n' "$out" \
        | sed -nE 's/^wallet-faucet: faucet_id=(0x[0-9a-fA-F]+) amount=([0-9]+)$/\1 \2/p' \
        | tr 'A-F' 'a-f'
    printf '%s\n' "$out" | grep -q '^wallet-faucet: done' || {
        echo "iso_wallet_faucets: --list-wallet-faucets did not complete (rc=$rc): $out" >&2
        return 1
    }
    return 0
}

# _faucet_amt <snapshot> <faucet_id> → the amount held for <faucet_id> in a snapshot
# (0 when absent). Snapshot = newline-separated "faucet_id amount" lines from iso_wallet_faucets.
_faucet_amt() {
    local f; f="0x$(echo "${2#0x}" | tr 'A-F' 'a-f')"
    awk -v f="$f" '$1==f{print $2; found=1} END{if(!found)print 0}' <<<"$1"
}

# assert_received_faucet <before_snapshot> <expected_faucet_id> <expected_symbol> \
#                        <expected_decimals> <expected_amount> <origin-label>
# #147/PR#152 received-asset linkage — DERIVE the received faucet from the receiving wallet's
# VAULT DELTA, not from a caller-supplied known id. The caller captures <before_snapshot> =
# $(iso_wallet_faucets) BEFORE the claim delivers the asset; this call polls the AFTER
# snapshot, finds the faucet whose held balance ROSE by >= <expected_amount> (the actually-
# received faucet), asserts that DERIVED id equals the expected origin faucet, and runs the
# cold-wallet metadata (symbol/decimals) assertion on that DERIVED id. Because it matches the
# DELTA (not an absolute balance), a retained/accumulated balance cannot false-pass. Fails
# LOUD with before/after snapshots + derived/expected/origin diagnostics. Uses ambient WALLET_ID.
assert_received_faucet() {
    local before="$1" want_fid="$2" want_sym="$3" want_dec="$4" want_amt="$5" label="$6"
    local want_fid_lc; want_fid_lc="0x$(echo "${want_fid#0x}" | tr 'A-F' 'a-f')"
    local after="" derived="" attempt fid aamt bamt delta
    # POLL: the just-claimed P2ID note is not consumed/visible instantly. Each snapshot
    # re-syncs + consumes, so retry until a faucet's delta reaches the received amount.
    for attempt in $(seq 1 "${RECV_POLL_TRIES:-15}"); do
        after="$(iso_wallet_faucets)" || { sleep "${RECV_POLL_INTERVAL:-10}"; continue; }
        derived=""
        while read -r fid aamt; do
            [[ -z "$fid" ]] && continue
            bamt="$(_faucet_amt "$before" "$fid")"
            delta=$(( aamt - bamt ))
            if [[ "$delta" -ge "$want_amt" ]]; then derived="$fid"; break; fi
        done <<<"$after"
        [[ -n "$derived" ]] && break
        sleep "${RECV_POLL_INTERVAL:-10}"
    done
    [[ -n "$derived" ]] \
        || fail "#147/link: $label — NO wallet faucet's vault balance rose by >= $want_amt after the claim (wallet=$WALLET_ID). BEFORE=[$(printf '%s' "$before" | tr '\n' ';')] AFTER=[$(printf '%s' "$after" | tr '\n' ';')] expected-origin-faucet=$want_fid_lc symbol=$want_sym/$want_dec — the received asset was linked to no faucet"
    [[ "$derived" == "$want_fid_lc" ]] \
        || fail "#147/link: $label — the RECEIVED asset's faucet (DERIVED $derived, balance rose >= $want_amt) != the expected origin faucet ($want_fid_lc). The wallet received a DIFFERENT asset than the config/RPC/PG id claims. AFTER=[$(printf '%s' "$after" | tr '\n' ';')]"
    log "#147/link: $label — DERIVED received faucet $derived from the wallet's vault delta (>= $want_amt units); verifying its cold-wallet metadata"
    assert_faucet_symbol "$derived" "$want_sym" "$want_dec" "$label (derived from received asset)"
    pass "#147/link: $label received-asset LINKED — vault delta identifies faucet $derived (== origin), resolves $want_sym/$want_dec on a cold wallet"
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
