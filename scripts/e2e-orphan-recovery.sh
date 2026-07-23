#!/usr/bin/env bash
# #156 e2e — automatic recovery of an acknowledged pending/unlinked transaction.
#
# Reproduces the exact durable orphan a crash produces, at the real admission-to-
# handoff boundary, then proves the proxy self-heals on restart with no client
# activity:
#   1. Recreate the proxy with the fault barrier AGGLAYER_FAULT_EXIT_AFTER_ADMIT=1.
#   2. Submit ONE zero-amount claimAsset (the admittable vehicle — a fresh
#      globalIndex + amount 0 skips the GER-observed preflight). The proxy durably
#      persists the pending row and advances the nonce, then aborts BEFORE the
#      writer job is enqueued — a pending row with no miden_tx_id and no submitted
#      handoff while the nonce has advanced. The RPC call gets no response.
#   3. Assert that exact durable signature directly in the proxy's Postgres.
#   4. Recreate the proxy WITHOUT the fault. Startup recovery must re-drive the
#      orphan to a terminal outcome with NO client rebroadcast and never advance
#      the nonce a second time.
#
# Requires a running e2e stack. Env: COMPOSE_PROJECT_NAME, L2_RPC.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
PROXY="${AGGLAYER_CONTAINER:-${PROJECT}-miden-agglayer-1}"
PG="${AGGLAYER_PG_CONTAINER:-${PROJECT}-agglayer-postgres-1}"
BRIDGE="${BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
DEST_NET="${NETWORK_ID:-1}"
GAS_LIMIT="${GAS_LIMIT:-600000}"
COMPOSE=(docker compose -f "$HERE/../docker-compose.e2e.yml")
[ -f "$HERE/../docker-compose.l2l2.yml" ] && COMPOSE+=(-f "$HERE/../docker-compose.l2l2.yml")
COMPOSE+=(--env-file "$HERE/../fixtures/.env")

log()  { echo "[orphan-recovery] $*"; }
pass() { echo "[orphan-recovery] PASS: $*"; }
fail() { echo "[orphan-recovery] FAIL: $*"; exit 1; }

command -v cast >/dev/null || fail "cast (foundry) not found"
command -v python3 >/dev/null || fail "python3 not found"

pgq() { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }
pgq "SELECT 1" >/dev/null || fail "cannot reach proxy Postgres ($PG)"
metric() { curl -fsS "$L2_RPC/metrics" 2>/dev/null | grep -E "^$1[[:space:]]" | awk '{print $2}' | tail -1; }
proxy_up() {
    cast rpc --rpc-url "$L2_RPC" eth_blockNumber >/dev/null 2>&1
}
wait_proxy_up() { for _ in $(seq 1 "${1:-24}"); do proxy_up && return 0; sleep 5; done; return 1; }

CHAIN_ID="$(cast rpc --rpc-url "$L2_RPC" eth_chainId 2>/dev/null | tr -d '"' \
    | python3 -c 'import sys; print(int(sys.stdin.read().strip() or "0x0", 16))')"
[[ "$CHAIN_ID" =~ ^[0-9]+$ && "$CHAIN_ID" -gt 0 ]] || fail "could not derive chain id from $L2_RPC"
KEY="$(cast wallet new 2>/dev/null | awk '/Private key:/{print $NF}')"
[[ -n "$KEY" ]] || fail "could not mint a throwaway signing key"
SIGNER="$(cast wallet address --private-key "$KEY")"
SIGNER_LC="$(echo "$SIGNER" | tr 'A-F' 'a-f')"
log "signer=$SIGNER  rpc=$L2_RPC  chain_id=$CHAIN_ID"

Z32="0x0000000000000000000000000000000000000000000000000000000000000000"
ZADDR="0x0000000000000000000000000000000000000000"
PROOF="[$Z32"; for _ in $(seq 2 32); do PROOF="$PROOF,$Z32"; done; PROOF="$PROOF]"
CLAIM_SIG="claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)"
GI="$(( (RANDOM * RANDOM) + 300003 ))"   # fresh, unclaimed global index
RAW="$(cast mktx --private-key "$KEY" --nonce 0 --chain-id "$CHAIN_ID" \
    --gas-limit "$GAS_LIMIT" --gas-price 1000000000 --value 0 \
    "$BRIDGE" "$CLAIM_SIG" \
    "$PROOF" "$PROOF" "$GI" "$Z32" "$Z32" 0 "$ZADDR" "$DEST_NET" "$ZADDR" 0 0x 2>/dev/null)"
[[ "$RAW" == 0x* ]] || fail "could not build the zero-amount claimAsset (cast mktx)"

# ── Phase 1: install the fault barrier, submit ONE write → deterministic orphan ─
OVERRIDE="$(mktemp /tmp/orphan-fault.XXXXXX.yml)"
cat > "$OVERRIDE" <<YML
services:
  miden-agglayer:
    restart: "no"
    environment:
      AGGLAYER_FAULT_EXIT_AFTER_ADMIT: "1"
YML
log "recreating proxy with the post-admit fault barrier"
"${COMPOSE[@]}" -f "$OVERRIDE" up -d --force-recreate --no-deps miden-agglayer >/dev/null 2>&1 \
    || fail "could not recreate proxy with the fault override"
wait_proxy_up 24 || fail "faulted proxy did not come up"

log "submitting one zero-amount claimAsset (nonce 0) to trigger the fault at admission"
cast rpc --rpc-url "$L2_RPC" eth_sendRawTransaction "$RAW" >/dev/null 2>&1 || true

# The proxy admits (durable pending row + nonce CAS) then aborts before enqueue.
log "waiting for the orphan signature in the proxy store..."
ORPHAN=""
for _ in $(seq 1 40); do
    ORPHAN="$(pgq "
        SELECT t.tx_hash
        FROM transactions t
        LEFT JOIN tx_note_links l ON l.tx_hash = t.tx_hash
        WHERE t.status = 'pending'
          AND lower(t.signer) = '$SIGNER_LC'
          AND t.miden_tx_id IS NULL
          AND l.note_id IS NULL
        LIMIT 1" | head -1)"
    [ -n "$ORPHAN" ] && break
    sleep 3
done
[ -n "$ORPHAN" ] || fail "no orphaned pending transaction was produced within 120s (fault did not fire?)"
NONCE_AFTER_CRASH="$(pgq "SELECT nonce FROM nonces WHERE address = '$SIGNER_LC'")"
[ -n "$NONCE_AFTER_CRASH" ] && [ "$NONCE_AFTER_CRASH" -ge 1 ] \
    || fail "the orphan's nonce did not advance (got '${NONCE_AFTER_CRASH:-}') — not the acknowledged-but-orphaned signature"
pass "orphan produced: pending claim $ORPHAN (no miden_tx_id, no handoff), nonce advanced to $NONCE_AFTER_CRASH"

# ── Phase 2: remove the fault; startup recovery must self-heal it ───────────────
rm -f "$OVERRIDE"
log "recreating proxy WITHOUT the fault; startup recovery must re-drive the orphan"
"${COMPOSE[@]}" up -d --force-recreate --no-deps miden-agglayer >/dev/null 2>&1 \
    || fail "could not recreate proxy without the fault"
wait_proxy_up 24 || fail "recovered proxy did not come back online"

log "waiting for automatic recovery to resolve the orphan (no client rebroadcast)..."
RESOLVED=""
for _ in $(seq 1 60); do
    ST="$(pgq "SELECT status FROM transactions WHERE tx_hash = '$ORPHAN'")"
    if [ -n "$ST" ] && [ "$ST" != "pending" ]; then RESOLVED="$ST"; break; fi
    sleep 5
done
[ -n "$RESOLVED" ] || fail "the orphan was NOT recovered automatically (still pending after 300s) — a client rebroadcast would have been required"
pass "orphan self-healed WITHOUT client activity: claim $ORPHAN reached terminal status '$RESOLVED'"

NONCE_AFTER_RECOVERY="$(pgq "SELECT nonce FROM nonces WHERE address = '$SIGNER_LC'")"
[ "$NONCE_AFTER_RECOVERY" = "$NONCE_AFTER_CRASH" ] \
    || fail "recovery advanced the nonce a second time ($NONCE_AFTER_CRASH -> $NONCE_AFTER_RECOVERY)"
pass "nonce not advanced twice by recovery (stayed $NONCE_AFTER_RECOVERY)"
log "orphan_recovery_successes_total = $(metric orphan_recovery_successes_total || echo '<none>')"

pass "#156 orphan recovery e2e: acknowledged tx self-healed on restart with no client rebroadcast"
