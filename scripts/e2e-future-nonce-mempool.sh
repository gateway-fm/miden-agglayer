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
# The e2e proxy runs with `--insecure-allow-any-signer` (see docker-compose.e2e
# .yml), so this uses a FRESH random key — no allow-list wiring needed.
#
# VEHICLE: `claimAsset`, deliberately. The obvious choice, `insertGlobalExitRoot`,
# CANNOT be used on a stack running `--reject-unverified-ger-injection`: the
# strict-H6 preflight (service_send_raw_txn, scoped to DecodedWriteCall::Ger)
# refuses a root the configured L1 scan has not observed, and it runs BEFORE the
# nonce/park decision — so every tx was rejected and the queue was never
# exercised at all. Nonzero claimAsset calls also validate deposit inclusion
# before parking. First prove that malformed nonzero claims are rejected without
# consuming a nonce or creating a transaction record, at both current and future
# nonces. Then exercise the queue with the existing zero-amount claim no-op:
# it creates no Miden note or ClaimEvent, but still uses normal nonce admission,
# durable parking, conflict detection, and promotion.
#
# Promotion is observed via the pending transaction count advancing across the
# gap. Both no-ops must also reach successful, log-free terminal receipts.
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
# The L2 bridge address claimAsset is decoded against (admitted by SELECTOR, so
# the destination only needs to be a plausible target).
BRIDGE_ADDR="${BRIDGE_ADDR:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
# Mainnet deposit index 2^32-1: outside this disposable fixture's deposits.
GLOBAL_INDEX=18446744078004518911
CLAIM_SIG='claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)'
# 32-entry proof arrays; the marker byte makes each tx's calldata (and therefore
# its hash) distinct so the conflict case is a genuinely different transaction.
proof32() {
    local m="$1" out="[" i
    for i in $(seq 0 31); do
        [[ $i -gt 0 ]] && out+=","
        out+="$(printf '0x%062x%02x' "$i" "$((16#$m))")"
    done
    echo "${out}]"
}
# Chain id MUST come from the node, not a constant: the plain e2e stack and the
# l2l2 stack use DIFFERENT ids (1 vs 2), and R4 rejects a chain-id mismatch
# before the future-nonce path is ever reached — so a hardcoded id silently
# turned this whole test into "the proxy rejects everything" on l2l2.
if [[ -z "${CHAIN_ID:-}" ]]; then
    CHAIN_ID_HEX="$(cast rpc --rpc-url "$L2_RPC" eth_chainId 2>/dev/null | tr -d '"' || true)"
    [[ "$CHAIN_ID_HEX" == 0x* ]] || fail "could not read eth_chainId from $L2_RPC (got: '$CHAIN_ID_HEX')"
    CHAIN_ID="$((CHAIN_ID_HEX))"
fi
GAS_LIMIT="${GAS_LIMIT:-900000}"
GAS_PRICE="${GAS_PRICE:-1000000000}"

command -v cast >/dev/null || fail "cast (foundry) not found"
cast rpc --rpc-url "$L2_RPC" eth_blockNumber >/dev/null 2>&1 || fail "proxy RPC not reachable at $L2_RPC"

# Fresh signer — its next nonce starts at 0.
KEY="$(cast wallet new 2>/dev/null | awk '/Private key:/{print $NF}')"
[[ -n "$KEY" ]] || fail "could not mint a throwaway signing key"
SIGNER="$(cast wallet address --private-key "$KEY")"
SIGNER_LC="$(echo "$SIGNER" | tr 'A-F' 'a-f')"
log "signer=$SIGNER  rpc=$L2_RPC  bridge=$BRIDGE_ADDR  chain_id=$CHAIN_ID"

# mk_raw <nonce> <marker-hex-byte> <amount> → signed claimAsset tx.
# Positive queue cases pass amount 0; negative proof cases pass amount 1.
mk_raw() {
    local pr; pr="$(proof32 "$2")"
    cast mktx --private-key "$KEY" --nonce "$1" --chain-id "$CHAIN_ID" \
        --gas-limit "$GAS_LIMIT" --gas-price "$GAS_PRICE" --value 0 \
        "$BRIDGE_ADDR" "$CLAIM_SIG" \
        "$pr" "$pr" "$GLOBAL_INDEX" \
        0x0000000000000000000000000000000000000000000000000000000000000001 \
        0x0000000000000000000000000000000000000000000000000000000000000002 \
        0 0x0000000000000000000000000000000000000000 1 "$SIGNER" "$3" 0x 2>/dev/null
}
# JSON-RPC helpers (cast rpc prints the raw result; strip surrounding quotes).
# NOTE: returns the node's reply on stdout and NEVER fails the script — the
# caller must inspect the reply. `cast rpc` exits non-zero on an RPC error, and
# under `set -e` a bare `X="$(send_raw ...)"` would abort before the caller's
# `|| fail` could report WHY, which hid a real rejection as a silent exit.
send_raw()   { cast rpc --rpc-url "$L2_RPC" eth_sendRawTransaction "$1" 2>&1 || true; }
get_receipt(){ cast rpc --rpc-url "$L2_RPC" eth_getTransactionReceipt "$1" 2>/dev/null; }
get_tx()     { cast rpc --rpc-url "$L2_RPC" eth_getTransactionByHash "$1" 2>/dev/null; }
pending_cnt(){
    local value
    value="$(cast rpc --rpc-url "$L2_RPC" eth_getTransactionCount "$SIGNER" "pending")" \
        || fail "could not read pending nonce"
    value="${value//\"/}"
    [[ "$value" =~ ^0x[0-9a-fA-F]+$ ]] || fail "invalid pending nonce: $value"
    printf '%d\n' "$((value))"
}

# Distinct calldata markers -> distinct tx hashes.
ROOT1="a1"
ROOT1B="b2"
ROOT0="c3"

# Proof validation must precede both parking and current-nonce admission.
step "0. malformed nonzero claims must be rejected before nonce admission"
for NONCE in 1 0; do
    BAD_RAW="$(mk_raw "$NONCE" d4 1)"
    [[ "$BAD_RAW" == 0x* ]] || fail "could not build the invalid-proof tx"
    BAD_HASH="$(cast keccak "$BAD_RAW")"
    BAD_REPLY="$(send_raw "$BAD_RAW")"
    [[ "$BAD_REPLY" == *InvalidSmtProof* ]] || fail "expected invalid-proof rejection: $BAD_REPLY"
    [[ "$(get_receipt "$BAD_HASH")" == null ]] || fail "rejected proof created a receipt"
    [[ "$(get_tx "$BAD_HASH")" == null ]] || fail "rejected proof was admitted or parked"
    [[ "$(pending_cnt)" -eq 0 ]] || fail "rejected proof consumed a nonce"
done
pass "0. invalid nonzero proofs rejected at current and future nonces, with no admission"

# ── 1. Future nonce (N+1 before N) is PARKED, not rejected ────────────────────
step "1. submit nonce 1 (future) before nonce 0 — must be PARKED (accepted), not rejected"
RAW1="$(mk_raw 1 "$ROOT1" 0)"; [[ "$RAW1" == 0x* ]] || fail "could not build the nonce-1 tx (cast mktx): $RAW1"
HASH1="$(cast keccak "$RAW1")"
SEND1="$(send_raw "$RAW1")"
echo "$SEND1" | grep -qiE 'nonce mismatch|nonce too high' && fail "future-nonce tx was REJECTED instead of parked — #146 not in effect: $SEND1"
RET1="$(echo "$SEND1" | tr -d '"')"
[[ "$RET1" == 0x* && ${#RET1} -ge 66 ]] || fail "eth_sendRawTransaction did not return a tx hash for the parked tx: $SEND1"
[[ -z "$HASH1" || "$HASH1" == "$RET1" ]] || fail "returned hash $RET1 != computed $HASH1"
HASH1="$RET1"
pass "1. future-nonce tx accepted (parked), hash=$HASH1"

step "1a. the parked tx's receipt must be null (not yet executed)"
RCPT1="$(get_receipt "$HASH1")"
[[ "$RCPT1" == "null" ]] || fail "parked tx must have a NULL receipt, got: $RCPT1"
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
step "3. submit a DIFFERENT tx at nonce 1 — must be refused (first wins, no replacement)"
RAW1B="$(mk_raw 1 "$ROOT1B" 0)"; [[ "$RAW1B" == 0x* ]] || fail "could not build the conflicting nonce-1 tx"
SEND3="$(send_raw "$RAW1B")"
echo "$SEND3" | grep -qiE 'already queued|different transaction' \
    || fail "a conflicting same-nonce tx must be refused; got: $SEND3"
pass "3. conflicting same-(signer,nonce) tx is refused"

# ── 4. Filling the gap PROMOTES the parked run in nonce order ─────────────────
step "4. submit nonce 0 (fills the gap) — must promote the parked nonce-1 successor"
RAW0="$(mk_raw 0 "$ROOT0" 0)"; [[ "$RAW0" == 0x* ]] || fail "could not build the nonce-0 tx"
SEND0="$(send_raw "$RAW0")"
echo "$SEND0" | grep -qiE 'nonce mismatch|nonce too (low|high)' && fail "the in-order nonce-0 tx was rejected: $SEND0"
RET0="$(echo "$SEND0" | tr -d '"')"
[[ "$RET0" == 0x* ]] || fail "nonce-0 submission did not return a hash: $SEND0"
pass "4. in-order nonce-0 tx accepted (hash=$RET0)"

step "4a. the pending nonce advances across the WHOLE 0..=1 prefix (promotion + order)"
ADVANCED=0
for _ in $(seq 1 40); do
    PC="$(pending_cnt)"
    [[ "$PC" -ge 2 ]] && { ADVANCED=1; break; }
    sleep 1
done
[[ "$ADVANCED" -eq 1 ]] \
    || fail "pending nonce did not reach 2 after the gap filled — the parked successor was not promoted (last pending=$PC)"
pass "4a. pending nonce advanced to >=2 — nonce 0 then 1 promoted in order (the parked tx was drained)"

step "4b. both promoted no-ops must finish successfully without emitting logs"
for HASH in "$RET0" "$HASH1"; do
    RCPT=null
    for _ in $(seq 1 40); do
        RCPT="$(get_receipt "$HASH")"
        [[ "$RCPT" != null ]] && break
        sleep 1
    done
    printf '%s' "$RCPT" | python3 -c '
import json, sys
receipt = json.load(sys.stdin)
assert isinstance(receipt, dict), "no terminal receipt"
assert receipt.get("transactionHash", "").lower() == sys.argv[1].lower(), "wrong transaction"
assert receipt.get("status") == "0x1", "no-op did not succeed"
assert receipt.get("logs") == [], "no-op emitted logs"
' "$HASH" || fail "no-op receipt did not prove successful, log-free completion: $RCPT"
done
pass "4b. both no-ops completed successfully with no claim logs"

log "══════════════════════════════════════════════════════════════════════════"
pass "#146 future-nonce mempool: park + null-receipt + pending-shape + no-nonce-jump"
pass "#146 idempotent re-broadcast + conflict-refusal + gap-fill promotion in order"
log "══════════════════════════════════════════════════════════════════════════"
