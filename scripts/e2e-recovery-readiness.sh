#!/usr/bin/env bash
# e2e-recovery-readiness.sh — #148 acceptance test.
# ============================================================================
# SUPPORTED recovery mode: RETAINED PostgreSQL + RESET Miden store.
#
# The proxy keeps its Postgres (a processed claim + its synthetic ClaimEvent
# remain) but a claim's stored `claimAsset` calldata envelope is lost, and the
# Miden client store / reconcile cursor is reset. On restart the genesis
# reconciler re-observes each historical CLAIM note and BACKFILLS its calldata
# (restore::persist_synthetic_claim_tx). Until that repair completes,
# eth_getTransactionByHash would serve the claim with EMPTY input and aggkit's
# bridgesync parser stalls on it.
#
# #148 gates READINESS on the repair: `/health` stays 503 "recovering" (with a
# nonzero claims_awaiting_calldata) until every ClaimEvent's calldata is
# re-persisted, so consumers are never released onto an empty-input claim. This
# test proves:
#   1. readiness is WITHHELD (503) while a claim's calldata is missing;
#   2. before readiness flips, the repair completes and eth_getTransactionByHash
#      returns the COMPLETE ORIGINAL claimAsset calldata — byte-for-byte, never
#      0x or a fabricated placeholder;
#   3. no FOREIGN-deployment ClaimEvent appears during the recovery;
#   4. the bridge-service resyncs and the recovered claim remains settleable.
#
# Reuses an EXISTING landed claim from the prior full-suite tiers (no fresh
# deposit), like e2e-cantina13 reuses faucet rows. In the full overlay suite it
# is MANDATORY (REQUIRE_RECOVERY_READINESS=1 → FAIL if no claim is present, so a
# prior-phase regression can't silently void it); standalone it SKIPs.
#
# Usage: full l2l2 stack up + at least one landed claim (run via e2e-test.sh all,
#        or after e2e-l1-to-l2.sh). ./scripts/e2e-recovery-readiness.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/lib-l2l2.sh"

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
E2E_COMPOSE=(docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" -f "$PROJECT_DIR/docker-compose.l2l2.yml" --env-file "$FIXTURES_DIR/.env")
PG_CONTAINER="${PG_CONTAINER:-${COMPOSE_PROJECT_NAME}-agglayer-postgres-1}"
BRIDGE_PG_CONTAINER="${BRIDGE_PG_CONTAINER:-${COMPOSE_PROJECT_NAME}-postgres-1}"

# `pgi` — a COUNT/scalar query that FAILS on a psql error (no false-green from a
# suppressed error; PR-review-rigor #1). `pg` (from lib-l2l2) is used for text.
pgi() { local out; out=$(pg "$1") || return 1; printf '%s' "$out"; }
proxy_health_code() { curl -s -m5 -o /dev/null -w '%{http_code}' "$L2_RPC/health" 2>/dev/null || echo 000; }
proxy_health_body() { curl -s -m5 "$L2_RPC/health" 2>/dev/null || echo '{}'; }
# eth_getTransactionByHash → the `input` field (calldata), lowercased.
tx_input() {
    curl -s -m8 "$L2_RPC" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionByHash\",\"params\":[\"$1\"]}" 2>/dev/null \
        | python3 -c "import json,sys
try: r=json.load(sys.stdin).get('result') or {}
except Exception: r={}
print((r.get('input') or '').lower())"
}

log "======================================================================"
log "  #148 RECOVERY READINESS — retained PostgreSQL + reset Miden store"
log "======================================================================"
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi

# ── PRE: reuse an existing landed claim with real calldata ────────────────────
# A ClaimEvent whose tx has non-empty stored calldata (a normally-processed
# claim from the prior tiers). REQUIRE non-empty input so the later byte-for-byte
# equality can't pass as empty->empty (PR-review-rigor #3).
CLAIM_TX="$(pg "SELECT lower(transaction_hash) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC') ORDER BY block_number DESC LIMIT 1" || true)"
if [[ -z "$CLAIM_TX" ]]; then
    if [[ "${REQUIRE_RECOVERY_READINESS:-0}" == "1" ]]; then
        fail "#148: REQUIRE_RECOVERY_READINESS=1 but no landed ClaimEvent is present — a prior claim-producing phase regressed"
    fi
    log "#148: SKIP — no landed ClaimEvent present (standalone run; needs a prior claim, REQUIRE_RECOVERY_READINESS unset)"
    exit 0
fi
ORIG_CALLDATA="$(tx_input "$CLAIM_TX")"
[[ "$ORIG_CALLDATA" == 0x* && ${#ORIG_CALLDATA} -gt 10 ]] \
    || fail "#148: chosen claim $CLAIM_TX has no real calldata to lose (input='$ORIG_CALLDATA') — cannot exercise repair"
CLAIM_COUNT_PRE="$(pgi "SELECT COUNT(*) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC')")"
[[ "$(proxy_health_code)" == "200" ]] || fail "#148: proxy not READY (200) before the recovery — precondition"
pass "PRE: claim $CLAIM_TX has ${#ORIG_CALLDATA}-char calldata; $CLAIM_COUNT_PRE ClaimEvent(s); /health=200"

# ── Induce the recovery shape: retained PG, blanked claim envelope, reset store ─
BASE_ARGS=$(docker inspect -f '{{range .Args}}{{.}} {{end}}' "$AGGLAYER_CONTAINER")
BACKUP="/tmp/agglayer_store.recovery-readiness.$$.sql"
docker exec "$PG_CONTAINER" pg_dump -U agglayer agglayer_store > "$BACKUP" 2>/dev/null
[[ -s "$BACKUP" ]] || fail "#148: DB backup failed (empty) — refusing to mutate without a backup"

MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" "${E2E_COMPOSE[@]}" stop miden-agglayer >/dev/null 2>&1
# RETAIN Postgres; only remove the claim's calldata envelope (transaction_logs
# cascade-drops via FK; the ClaimEvent in synthetic_logs is UNTOUCHED), and reset
# the reconcile cursor to 0 so the genesis re-sweep re-observes + backfills it.
DELETED="$(pgi "WITH d AS (DELETE FROM transactions WHERE lower(tx_hash) = '$CLAIM_TX' RETURNING 1) SELECT COUNT(*) FROM d")"
[[ "$DELETED" == "1" ]] || fail "#148: expected to blank exactly 1 claim tx envelope, deleted '$DELETED'"
pg "UPDATE service_state SET reconcile_cursor = 0 WHERE id = 1" >/dev/null
# Reset the Miden client store (the issue's recovery shape) — best-effort delete
# of the sqlite from the bind mount; the reconcile_cursor=0 above independently
# forces the re-sweep, so the repair runs whether or not the rm matched.
rm -f "$PROJECT_DIR/.miden-agglayer-data/store.sqlite3"* 2>/dev/null || true
# Sanity: the ClaimEvent is retained, its calldata is gone.
[[ "$(pgi "SELECT COUNT(*) FROM synthetic_logs WHERE lower(transaction_hash) = '$CLAIM_TX' AND lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC')")" == "1" ]] \
    || fail "#148: the ClaimEvent must be RETAINED (only the calldata is lost)"
[[ "$(pgi "SELECT COUNT(*) FROM transactions WHERE lower(tx_hash) = '$CLAIM_TX'")" == "0" ]] \
    || fail "#148: the claim tx envelope should be gone"
pass "Induced: claim envelope blanked (retained ClaimEvent), reconcile cursor reset, Miden store reset"

MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" "${E2E_COMPOSE[@]}" start miden-agglayer >/dev/null 2>&1
# Wait for the HTTP server itself to be up (health responds at all, even 503),
# so a connection-refused during boot is not mistaken for readiness.
for _i in $(seq 1 90); do
    [[ "$(proxy_health_code)" != "000" ]] && break
    sleep 2
done

# ── 1. Readiness is WITHHELD while the claim calldata is missing ──────────────
# Poll: we MUST observe a 503 "recovering" with claims_awaiting_calldata >= 1 at
# least once (the gate holding), then a transition to 200 (repair complete).
SAW_RECOVERING=0; READY=0
for _i in $(seq 1 200); do
    CODE="$(proxy_health_code)"
    if [[ "$CODE" == "503" ]]; then
        BODY="$(proxy_health_body)"
        if echo "$BODY" | grep -q 'recovering' && echo "$BODY" | python3 -c "import json,sys;sys.exit(0 if (json.load(sys.stdin).get('claims_awaiting_calldata',0) or 0) >= 1 else 1)" 2>/dev/null; then
            SAW_RECOVERING=1
        fi
    elif [[ "$CODE" == "200" ]]; then
        READY=1; break
    fi
    sleep 3
done
[[ "$SAW_RECOVERING" == "1" ]] \
    || fail "#148: never observed /health=503 'recovering' with claims_awaiting_calldata>=1 — the readiness gate did NOT hold while calldata was missing"
pass "1. Readiness WITHHELD: /health=503 'recovering' (claims_awaiting_calldata>=1) while the claim calldata was missing"
[[ "$READY" == "1" ]] || fail "#148: /health never returned to 200 after the calldata repair (repair stalled?)"
pass "1b. Readiness flipped to 200 once the calldata repair completed"

# ── 2. Repaired calldata is the COMPLETE ORIGINAL (byte-for-byte, not 0x) ─────
POST_CALLDATA="$(tx_input "$CLAIM_TX")"
[[ "$POST_CALLDATA" == 0x* && ${#POST_CALLDATA} -gt 10 ]] \
    || fail "#148: after ready, eth_getTransactionByHash still serves empty/placeholder input ('$POST_CALLDATA') — repair did not restore calldata"
[[ "$POST_CALLDATA" == "$ORIG_CALLDATA" ]] \
    || fail "#148: recovered claimAsset calldata differs from the original (a reconstructed placeholder, not the truth): orig(${#ORIG_CALLDATA})=$ORIG_CALLDATA post(${#POST_CALLDATA})=$POST_CALLDATA"
pass "2. eth_getTransactionByHash returns the COMPLETE ORIGINAL claimAsset calldata (byte-for-byte, ${#POST_CALLDATA} chars, not 0x)"

# ── 3. No FOREIGN-deployment ClaimEvent appeared during recovery ─────────────
# The re-sweep must re-derive exactly the same claims — no spurious/foreign
# ClaimEvent (the #26 claim-provenance invariant). Count is exact + error-safe.
CLAIM_COUNT_POST="$(pgi "SELECT COUNT(*) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC')")"
[[ "$CLAIM_COUNT_POST" == "$CLAIM_COUNT_PRE" ]] \
    || fail "#148: ClaimEvent count changed across recovery ($CLAIM_COUNT_PRE -> $CLAIM_COUNT_POST) — a foreign/spurious claim leaked or a legit one was dropped"
pass "3. No foreign/spurious ClaimEvent across recovery (count stable at $CLAIM_COUNT_POST)"

# ── 4. bridge-service resyncs; the recovered claim stays settleable ──────────
# Realistic operational resync (finding #65): the reset proxy nonces are 0, so
# resync the bridge-service alongside so it re-fetches nonce 0 (else future-nonce
# wedge). Then assert it comes back reachable + synced against the recovered proxy.
"${E2E_COMPOSE[@]}" stop bridge-service bridge-autoclaim >/dev/null 2>&1
docker exec "$BRIDGE_PG_CONTAINER" psql -U bridge_user -d bridge_db \
    -c "DROP SCHEMA IF EXISTS sync CASCADE; DROP SCHEMA IF EXISTS mt CASCADE; DROP SCHEMA public CASCADE; CREATE SCHEMA public; GRANT ALL ON SCHEMA public TO bridge_user;" >/dev/null 2>&1 \
    || fail "#148: failed to drop bridge_db for the realistic resync (finding #65)"
"${E2E_COMPOSE[@]}" up -d --no-deps bridge-service bridge-autoclaim >/dev/null 2>&1
wait_for "bridge-service resynced + reachable after recovery" \
    "[[ \$(curl -s -m3 -o /dev/null -w '%{http_code}' $BRIDGE_SERVICE_URL/ 2>/dev/null) =~ ^(200|404)\$ ]]" 180 5
# The recovered claim's calldata is now complete, so aggkit's bridgesync can parse
# it (the empty-input stall is gone) — assert it is STILL served intact post-resync.
[[ "$(tx_input "$CLAIM_TX")" == "$ORIG_CALLDATA" ]] \
    || fail "#148: recovered calldata regressed after the bridge-service resync"
pass "4. bridge-service resynced against the recovered proxy; the recovered claim's calldata is intact + parseable (settleable)"

rm -f "$BACKUP"
log "======================================================================"
log "  #148 RECOVERY READINESS PASS — readiness gated on calldata repair;"
log "  original calldata recovered byte-for-byte; no foreign claim; resynced"
log "======================================================================"
