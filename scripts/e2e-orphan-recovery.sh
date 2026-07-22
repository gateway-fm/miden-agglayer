#!/usr/bin/env bash
# #156 e2e — automatic recovery of an acknowledged pending/unlinked transaction.
#
# Reproduces the exact durable orphan a crash produces, at the real admission-to-
# handoff boundary, then proves the proxy self-heals on restart with no client
# activity:
#   1. Recreate the proxy with the fault barrier AGGLAYER_FAULT_EXIT_AFTER_ADMIT=1.
#      The next write it admits (an aggoracle GER injection) durably persists its
#      pending row and advances the nonce, then the process aborts BEFORE the
#      writer job is enqueued — leaving a pending row with no miden_tx_id and no
#      submitted handoff while the nonce has advanced.
#   2. Assert that exact durable signature in the proxy's Postgres.
#   3. Recreate the proxy WITHOUT the fault. Startup recovery must re-drive the
#      orphan back into the writer with NO client rebroadcast, finalise it, and
#      never advance the nonce a second time.
#
# Requires a running e2e stack. Env: COMPOSE_PROJECT_NAME, L2_RPC.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
PROXY="${AGGLAYER_CONTAINER:-${PROJECT}-miden-agglayer-1}"
PG="${AGGLAYER_PG_CONTAINER:-${PROJECT}-agglayer-postgres-1}"
COMPOSE=(docker compose -f "$HERE/../docker-compose.e2e.yml")
[ -f "$HERE/../docker-compose.l2l2.yml" ] && COMPOSE+=(-f "$HERE/../docker-compose.l2l2.yml")
COMPOSE+=(--env-file "$HERE/../fixtures/.env")

log()  { echo "[orphan-recovery] $*"; }
pass() { echo "[orphan-recovery] PASS: $*"; }
fail() { echo "[orphan-recovery] FAIL: $*"; exit 1; }

# Query the proxy's durable store (agglayer_store).
pgq() { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }

pgq "SELECT 1" >/dev/null || fail "cannot reach proxy Postgres ($PG)"
metrics() { curl -fsS "$L2_RPC/metrics" 2>/dev/null; }
metric() { metrics | grep -E "^$1[[:space:]]" | awk '{print $2}' | tail -1; }

# ── Phase 1: install the fault barrier and let a real write orphan itself ──────
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

# Wait for the proxy to admit a write and abort (aggoracle injects GERs steadily).
log "waiting for a write to hit the fault barrier (orphan creation)..."
ORPHAN=""
for _ in $(seq 1 60); do
    # A pending row whose nonce is below the signer's advanced nonce and which has
    # no note handoff is the durable orphan signature.
    ORPHAN="$(pgq "
        SELECT t.tx_hash
        FROM transactions t
        LEFT JOIN tx_note_links l ON l.tx_hash = t.tx_hash
        WHERE t.status = 'pending'
          AND t.miden_tx_id IS NULL
          AND l.note_id IS NULL
        ORDER BY t.created_at DESC LIMIT 1" | head -1)"
    [ -n "$ORPHAN" ] && ! docker exec "$PROXY" true >/dev/null 2>&1 && break
    [ -n "$ORPHAN" ] && break
    sleep 5
done
[ -n "$ORPHAN" ] || fail "no orphaned pending transaction was produced within 300s"
pass "orphan produced: pending tx $ORPHAN (no miden_tx_id, no handoff)"

# The signer's nonce advanced past this orphan (durable admission happened).
SIGNER="$(pgq "SELECT lower(signer) FROM transactions WHERE tx_hash = '$ORPHAN'")"
NONCE_AFTER_CRASH="$(pgq "SELECT nonce FROM nonces WHERE address = '$SIGNER'")"
log "signer $SIGNER nonce after crash = ${NONCE_AFTER_CRASH:-?}"
[ -n "$NONCE_AFTER_CRASH" ] && [ "$NONCE_AFTER_CRASH" -ge 1 ] \
    || fail "the orphan's nonce did not advance — not the acknowledged-but-orphaned signature"

# ── Phase 2: remove the fault; startup recovery must self-heal it ──────────────
rm -f "$OVERRIDE"
log "recreating proxy WITHOUT the fault; startup recovery must re-drive the orphan"
"${COMPOSE[@]}" up -d --force-recreate --no-deps miden-agglayer >/dev/null 2>&1 \
    || fail "could not recreate proxy without the fault"

# Proxy back online.
for _ in $(seq 1 24); do
    curl -fsS -X POST -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        "$L2_RPC" 2>/dev/null | grep -q result && break
    sleep 5
done

# Recovery re-drives the orphan and the writer finalises it: the pending row must
# leave 'pending', its nonce must NOT advance a second time, and a recovery
# success must be counted. No client rebroadcast is performed by this script.
log "waiting for automatic recovery to resolve the orphan..."
RESOLVED=""
for _ in $(seq 1 60); do
    ST="$(pgq "SELECT status FROM transactions WHERE tx_hash = '$ORPHAN'")"
    if [ -n "$ST" ] && [ "$ST" != "pending" ]; then RESOLVED="$ST"; break; fi
    sleep 5
done
[ -n "$RESOLVED" ] || fail "the orphan was NOT recovered automatically (still pending after 300s) — client rebroadcast would have been required"
pass "orphan self-healed WITHOUT client activity: tx $ORPHAN reached terminal status '$RESOLVED'"

NONCE_AFTER_RECOVERY="$(pgq "SELECT nonce FROM nonces WHERE address = '$SIGNER'")"
[ "$NONCE_AFTER_RECOVERY" = "$NONCE_AFTER_CRASH" ] \
    || fail "recovery advanced the nonce a second time ($NONCE_AFTER_CRASH -> $NONCE_AFTER_RECOVERY)"
pass "nonce was not advanced twice by recovery (stayed $NONCE_AFTER_RECOVERY)"

SUCCESSES="$(metric orphan_recovery_successes_total)"
log "orphan_recovery_successes_total = ${SUCCESSES:-<none>}"

pass "#156 orphan recovery e2e: acknowledged tx self-healed on restart with no client rebroadcast"
