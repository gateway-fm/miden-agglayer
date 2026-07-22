#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-future-nonce-mempool.sh — #146 Geth-style future-nonce queue
#
# Proves the RPC contract end-to-end against the live proxy: a valid transaction
# whose nonce is AHEAD of the signer's next expected nonce is PARKED (its hash
# returned immediately, receipt null, surfaced as a pending tx, pending-nonce NOT
# bumped) instead of blocked-then-rejected; a same-hash re-broadcast is
# idempotent; a conflicting same-(signer,nonce) tx is refused; and filling the
# gap PROMOTES the contiguous parked run in nonce order (the pending nonce
# advances across the whole 0..=K prefix).
#
# Vehicle: a ZERO-AMOUNT `claimAsset` with a fresh (unclaimed) globalIndex and
# destinationNetwork == the proxy's network. This is an admitted selector that,
# at amount 0, passes `validate_before_nonce_reservation` WITHOUT requiring an
# observed GER (`claim_state_gate` only demands an applied GER for a nonzero
# amount) — so the park/promote path is exercised on a FRESH stack with no prior
# bridge/GER activity. (insertGlobalExitRoot would need a real L1-observed GER;
# claimAsset with amount 0 does not.) The e2e proxy runs `--insecure-allow-any-
# signer`, so a throwaway key is used. Promotion is observed via the `pending`
# transaction count advancing across the gap — admission-level (queue drain +
# nonce CAS), independent of whether the claim itself finalises on Miden.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
[[ -f "$PROJECT_DIR/fixtures/.env" ]] && source "$PROJECT_DIR/fixtures/.env"

GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE="${BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
DEST_NET="${NETWORK_ID:-1}"
GAS_LIMIT="${GAS_LIMIT:-600000}"

command -v cast >/dev/null || fail "cast (foundry) not found"
command -v python3 >/dev/null || fail "python3 not found"
# Derive the chain id from the proxy — do NOT assume 1 (the e2e proxy is 2).
CHAIN_ID="${CHAIN_ID:-$(cast rpc --rpc-url "$L2_RPC" eth_chainId 2>/dev/null | tr -d '"' \
    | python3 -c 'import sys; print(int(sys.stdin.read().strip() or "0x0", 16))')}"
[[ "$CHAIN_ID" =~ ^[0-9]+$ && "$CHAIN_ID" -gt 0 ]] || fail "could not derive chain id from $L2_RPC (got '$CHAIN_ID')"

KEY="$(cast wallet new 2>/dev/null | awk '/Private key:/{print $NF}')"
[[ -n "$KEY" ]] || fail "could not mint a throwaway signing key"
SIGNER="$(cast wallet address --private-key "$KEY")"
log "signer=$SIGNER  rpc=$L2_RPC  chain_id=$CHAIN_ID  dest_net=$DEST_NET"

Z32="0x0000000000000000000000000000000000000000000000000000000000000000"
ZADDR="0x0000000000000000000000000000000000000000"
PROOF="[$Z32"; for _ in $(seq 2 32); do PROOF="$PROOF,$Z32"; done; PROOF="$PROOF]"
CLAIM_SIG="claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)"

# mk_raw <nonce> <gas_price> <globalIndex> → raw signed zero-amount claimAsset.
mk_raw() {
    cast mktx --private-key "$KEY" --nonce "$1" --chain-id "$CHAIN_ID" \
        --gas-limit "$GAS_LIMIT" --gas-price "$2" --value 0 \
        "$BRIDGE" "$CLAIM_SIG" \
        "$PROOF" "$PROOF" "$3" "$Z32" "$Z32" 0 "$ZADDR" "$DEST_NET" "$ZADDR" 0 0x 2>/dev/null
}
send_raw()    { cast rpc --rpc-url "$L2_RPC" eth_sendRawTransaction "$1" 2>&1 || true; }
get_receipt() { cast rpc --rpc-url "$L2_RPC" eth_getTransactionReceipt "$1" 2>/dev/null | tr -d '"'; }
get_tx()      { cast rpc --rpc-url "$L2_RPC" eth_getTransactionByHash "$1" 2>/dev/null; }
pending_cnt() { cast rpc --rpc-url "$L2_RPC" eth_getTransactionCount "$SIGNER" "pending" 2>/dev/null \
    | tr -d '"' | python3 -c 'import sys; print(int(sys.stdin.read().strip() or "0x0", 16))'; }

GI0="$(( (RANDOM * RANDOM) + 100001 ))"
GI1="$(( (RANDOM * RANDOM) + 200002 ))"

# ── 1. Future nonce (N+1 before N) is PARKED, not rejected ────────────────────
step "1. submit nonce 1 (future) before nonce 0 — must be PARKED (accepted), not rejected"
RAW1="$(mk_raw 1 1000000000 "$GI1")"; [[ "$RAW1" == 0x* ]] || fail "could not build the nonce-1 tx (cast mktx)"
SEND1="$(send_raw "$RAW1")"
echo "$SEND1" | grep -qi 'nonce mismatch' && fail "future-nonce tx was REJECTED (nonce mismatch) — #146 not in effect: $SEND1"
HASH1="$(echo "$SEND1" | tr -d '"')"
[[ "$HASH1" == 0x* && ${#HASH1} -ge 66 ]] || fail "eth_sendRawTransaction did not return a tx hash for the parked tx: $SEND1"
pass "1. future-nonce tx accepted (parked), hash=$HASH1"

step "1a. the parked tx's receipt must be null (not yet executed)"
RCPT1="$(get_receipt "$HASH1")"
[[ -z "$RCPT1" || "$RCPT1" == "null" ]] || fail "parked tx must have a NULL receipt, got: $RCPT1"
pass "1a. eth_getTransactionReceipt(parked) is null"

step "1b. the parked tx is surfaced by eth_getTransactionByHash as a pending shape"
TX1="$(get_tx "$HASH1")"
echo "$TX1" | grep -q '"nonce":"0x1"' || fail "parked tx not surfaced with nonce 0x1: $TX1"
echo "$TX1" | grep -q '"blockNumber":null' || fail "parked tx must show blockNumber:null (pending shape): $TX1"
pass "1b. eth_getTransactionByHash(parked) returns the geth pending shape (nonce 0x1, blockNumber null)"

step "1c. the gapped queued tx must NOT bump the pending nonce"
PC="$(pending_cnt)"
[[ "$PC" -eq 0 ]] || fail "pending transaction count must stay 0 (gapped queued tx must not advance it), got $PC"
pass "1c. eth_getTransactionCount(pending) is still 0 — the gap does not advance pending"

# ── 2. Same-hash re-broadcast is idempotent ───────────────────────────────────
step "2. re-broadcast the SAME parked tx — idempotent accept (same hash)"
RET1B="$(send_raw "$RAW1" | tr -d '"')"
[[ "$RET1B" == "$HASH1" ]] || fail "same-hash re-broadcast must return the same hash; got $RET1B"
pass "2. same-hash re-broadcast is idempotent"

# ── 3. Conflicting same-(signer,nonce) different tx is refused ────────────────
step "3. submit a DIFFERENT tx at nonce 1 (different gas price) — must be refused"
RAW1B="$(mk_raw 1 2000000000 "$GI1")"; [[ "$RAW1B" == 0x* ]] || fail "could not build the conflicting nonce-1 tx"
SEND3="$(send_raw "$RAW1B")"
echo "$SEND3" | grep -qiE 'already queued|different transaction' \
    || fail "a conflicting same-nonce tx must be refused; got: $SEND3"
pass "3. conflicting same-(signer,nonce) tx is refused (first wins, no replacement)"

# ── 4. Filling the gap PROMOTES the parked run in nonce order ─────────────────
step "4. submit nonce 0 (fills the gap) — must promote the parked nonce-1 successor"
RAW0="$(mk_raw 0 1000000000 "$GI0")"; [[ "$RAW0" == 0x* ]] || fail "could not build the nonce-0 tx"
SEND0="$(send_raw "$RAW0")"
echo "$SEND0" | grep -qi 'nonce mismatch' && fail "the in-order nonce-0 tx was rejected: $SEND0"
HASH0="$(echo "$SEND0" | tr -d '"')"
[[ "$HASH0" == 0x* ]] || fail "nonce-0 submission did not return a hash: $SEND0"
pass "4. in-order nonce-0 tx accepted (hash=$HASH0)"

step "4a. the pending nonce advances across the WHOLE 0..=1 prefix (promotion + order)"
ADVANCED=0
for _ in $(seq 1 40); do
    PC="$(pending_cnt)"
    [[ "$PC" -ge 2 ]] && { ADVANCED=1; break; }
    sleep 1
done
[[ "$ADVANCED" -eq 1 ]] \
    || fail "pending nonce did not reach 2 after the gap filled — the parked successor was not promoted (last pending=$PC)"
pass "4a. pending nonce advanced to $PC (>=2) — nonce 0 then 1 promoted in order (parked tx drained)"

log "══════════════════════════════════════════════════════════════════════════"
pass "#146 future-nonce mempool: park + null-receipt + pending-shape + no-nonce-jump"
pass "#146 idempotent re-broadcast + conflict-refusal + gap-fill promotion in order"
log "══════════════════════════════════════════════════════════════════════════"
