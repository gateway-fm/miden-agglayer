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
# suppressed error; PR-review-rigor #1). Wraps `pgq` (the canonical query helper
# from lib-l2l2 — a bare `pg` was UNDEFINED here, so every DB read was a
# `command not found`); `|| return 1` propagates a psql failure to the caller.
pgi() { local out; out=$(pgq "$1") || return 1; printf '%s' "$out"; }
# `pgi_bridge` — query the SEPARATE bridge-service database (bridge_db in
# $BRIDGE_PG_CONTAINER), error-propagating. `pgq`/`pgi` target the proxy's
# agglayer_store; the consumer sync.status lives in bridge_db.
pgi_bridge() {
    local out
    out=$(docker exec "$BRIDGE_PG_CONTAINER" psql -U bridge_user -d bridge_db -tAX -c "$1") || return 1
    printf '%s' "$out"
}
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
CLAIM_TX="$(pgq "SELECT lower(transaction_hash) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC') ORDER BY block_number DESC LIMIT 1" || true)"
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

# Stop the proxy AND its Miden-side consumers together. #148 protects consumers
# from being released onto an empty-input claim; the readiness probe (503) is
# what an orchestrator (k8s readiness → no Service endpoints) uses to keep
# bridgesync traffic OFF the proxy until repair completes. The prior version left
# aggkit + bridge-service RUNNING throughout, so the gate's actual PURPOSE — that
# consumers are held off during the repair and only resume once ready — was never
# exercised (review blocker 6). Take them down for the whole repair window; they
# are brought back and asserted-settled only after /health flips to 200.
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" "${E2E_COMPOSE[@]}" stop miden-agglayer aggkit bridge-service bridge-autoclaim >/dev/null 2>&1
for c in aggkit bridge-service bridge-autoclaim; do
    [[ "$(docker inspect -f '{{.State.Running}}' "${COMPOSE_PROJECT_NAME}-${c}-1" 2>/dev/null)" == "false" ]] \
        || fail "#148: consumer '$c' is still running — it must be gated OFF during the calldata repair"
done
pass "Consumers gated OFF (aggkit, bridge-service, bridge-autoclaim stopped) for the repair window"
# RETAIN Postgres; only remove the claim's calldata envelope (transaction_logs
# cascade-drops via FK; the ClaimEvent in synthetic_logs is UNTOUCHED), and reset
# the reconcile cursor to 0 so the genesis re-sweep re-observes + backfills it.
# Blank the envelopes for ALL landed claims (not just CLAIM_TX). PR #151 blocker: with
# a SINGLE missing claim, the projector's initial sync tick — the same one that flips
# is_alive true — can repair it, so /health jumps degraded(alive=false)→200 and NEVER
# passes through the `recovering` state (alive=true, backlog>0) that step 1 asserts. A
# larger backlog is not fully drained in the alive-transition tick, so `recovering` is
# genuinely observable. CLAIM_TX's calldata is still checked byte-for-byte in step 2
# (its ORIG_CALLDATA is saved); the rest only need to drain the backlog.
DELETED="$(pgi "WITH d AS (DELETE FROM transactions WHERE lower(tx_hash) IN (SELECT DISTINCT lower(transaction_hash) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC')) RETURNING 1) SELECT COUNT(*) FROM d")"
[[ "$DELETED" =~ ^[0-9]+$ && "$DELETED" -ge 1 ]] || fail "#148: expected to blank >=1 claim tx envelope, deleted '$DELETED'"
[[ "$(pgi "SELECT COUNT(*) FROM transactions WHERE lower(tx_hash) = '$CLAIM_TX'")" == "0" ]] || fail "#148: CLAIM_TX envelope not among the blanked set"
log "  #148: blanked $DELETED claim envelope(s) to build an observable repair backlog"
pgq "UPDATE service_state SET reconcile_cursor = 0 WHERE id = 1" >/dev/null \
    || fail "#148: failed to reset reconcile_cursor (the genesis re-sweep would not run)"
# Reset the Miden client store — the issue's recovery shape MUST be genuinely
# reproduced, not best-effort. A host `rm` fails EPERM here: the proxy runs as
# root (no `user:`), so the bind-mounted sqlite is root-owned. The previous
# `rm -f … || true` therefore silently NO-OP'd and the test only passed because
# reconcile_cursor=0 independently forced the re-sweep — the reset-Miden-store
# path was never actually exercised (review blocker 5). Remove the sqlite set
# (exactly `recovery::reset_miden_store`'s SQLITE_FILES) as ROOT via a one-shot
# container sharing the same bind mount, and ASSERT it happened: a file existed
# and none remains.
MIDEN_RESET_OUT="$(MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" \
    "${E2E_COMPOSE[@]}" run --rm --no-deps --entrypoint sh miden-agglayer -c \
    'cd /var/lib/miden-agglayer-service 2>/dev/null || exit 3
     n=$(ls store.sqlite3 store.sqlite3-wal store.sqlite3-shm 2>/dev/null | wc -l)
     rm -f store.sqlite3 store.sqlite3-wal store.sqlite3-shm
     if ls store.sqlite3 store.sqlite3-wal store.sqlite3-shm >/dev/null 2>&1; then echo "REMAIN removed=$n"; else echo "GONE removed=$n"; fi' \
    2>/dev/null | tr -d '\r')"
[[ "$MIDEN_RESET_OUT" == GONE\ * ]] \
    || fail "#148: Miden store reset did not complete (out='$MIDEN_RESET_OUT') — the reset-Miden-store recovery shape was not reproduced"
[[ "$MIDEN_RESET_OUT" =~ removed=([0-9]+) && "${BASH_REMATCH[1]}" -ge 1 ]] \
    || fail "#148: Miden store had NO sqlite to reset (out='$MIDEN_RESET_OUT') — the proxy never wrote a store, so this run cannot exercise the recovery"
pass "Miden store reset (as root, via shared mount): removed ${BASH_REMATCH[1]} sqlite file(s), none remain"
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
# Single-curl capture (code+body together, no race between two requests) polled TIGHTLY
# so a short-lived `recovering` window is not stepped over. Deadline-bounded so the wait
# for the eventual 200 stays generous even at a fast interval.
SAW_RECOVERING=0; READY=0
RECOV_DEADLINE=$(( $(date +%s) + 600 ))
while [[ $(date +%s) -lt $RECOV_DEADLINE ]]; do
    RESP="$(curl -s -m5 -w $'\n%{http_code}' "$L2_RPC/health" 2>/dev/null || printf '{}\n000')"
    CODE="${RESP##*$'\n'}"; BODY="${RESP%$'\n'*}"
    if [[ "$CODE" == "503" ]]; then
        if echo "$BODY" | grep -q 'recovering' && echo "$BODY" | python3 -c "import json,sys;sys.exit(0 if (json.load(sys.stdin).get('claims_awaiting_calldata',0) or 0) >= 1 else 1)" 2>/dev/null; then
            SAW_RECOVERING=1
        fi
    elif [[ "$CODE" == "200" ]]; then
        READY=1; break
    fi
    sleep 0.3
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

# ── 4. Release the gated consumers; assert the recovered claim SETTLES ────────
# Only NOW (readiness=200) do we release the consumers the gate held off. This is
# the release the readiness probe authorises. Realistic operational resync
# (finding #65): the reset proxy nonces are 0, so drop bridge_db and let the
# bridge-service re-fetch from nonce 0 (else a future-nonce wedge). aggkit is
# --force-recreate'd (NOT plain up -d): aggkit's BridgeL2Sync cursor lives in the
# container's PathRWData=/tmp, which a plain `up -d` PRESERVES — so aggkit would
# RESUME from its old cursor and never re-fetch/re-parse the historical claim (PR
# #151 blocker). A fresh container re-scans the proxy L2 from block 0, genuinely
# re-exercising the empty-input-stall consumer. Step 4b asserts aggkit's OWN recovery.
docker exec "$BRIDGE_PG_CONTAINER" psql -U bridge_user -d bridge_db \
    -c "DROP SCHEMA IF EXISTS sync CASCADE; DROP SCHEMA IF EXISTS mt CASCADE; DROP SCHEMA public CASCADE; CREATE SCHEMA public; GRANT ALL ON SCHEMA public TO bridge_user;" >/dev/null 2>&1 \
    || fail "#148: failed to drop bridge_db for the realistic resync (finding #65)"
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" \
    "${E2E_COMPOSE[@]}" up -d --no-deps --force-recreate aggkit bridge-service bridge-autoclaim >/dev/null 2>&1
wait_for "bridge-service reachable after recovery" \
    "[[ \$(curl -s -m3 -o /dev/null -w '%{http_code}' $BRIDGE_SERVICE_URL/ 2>/dev/null) =~ ^(200|404)\$ ]]" 180 5
# SETTLEMENT (not merely "reachable"): the Miden bridge-service (network 1) must
# reach synced=true with a small remaining_blocks lag. If the recovered claim
# still served empty input, aggkit's bridgesync parser would STALL and this row
# would sit synced=false / remaining climbing — so a genuine synced row is the
# end-to-end proof the empty-input stall is gone and the claim is settleable.
# Error-propagating (PR-review-rigor #1): a missing/empty row is a FAIL, not a
# silent pass.
SETTLE_DEADLINE=$(( $(date +%s) + 240 ))
SYNCED=""; REMAIN=""
while :; do
    ROW="$(pgi_bridge "SELECT synced||'|'||remaining_blocks FROM sync.status WHERE network_id=1" 2>/dev/null | tr -d '[:space:]')" || ROW=""
    SYNCED="${ROW%%|*}"; REMAIN="${ROW##*|}"
    [[ "$SYNCED" == "t" || "$SYNCED" == "true" ]] && [[ "${REMAIN:-999999}" -le 50 ]] && break
    [[ $(date +%s) -ge $SETTLE_DEADLINE ]] && break
    sleep 5
done
[[ -n "$ROW" ]] \
    || fail "#148: no sync.status row for network 1 in bridge_db after release — the bridgesync consumer never started (settlement not observed)"
{ [[ "$SYNCED" == "t" || "$SYNCED" == "true" ]] && [[ "${REMAIN:-999999}" -le 50 ]]; } \
    || fail "#148: released consumer did NOT settle — sync.status network 1 synced=$SYNCED remaining_blocks=$REMAIN after 240s; the recovered claim likely still stalls bridgesync"
# And the calldata the consumer parsed is still the complete original.
[[ "$(tx_input "$CLAIM_TX")" == "$ORIG_CALLDATA" ]] \
    || fail "#148: recovered calldata regressed after the consumer release"
pass "4. Released consumers SETTLED: bridge-service sync.status network 1 synced=$SYNCED remaining_blocks=$REMAIN; recovered claim's calldata intact + parsed (no empty-input stall)"

# ── 4b. AGGKIT's OWN BridgeL2Sync re-processed the recovered claim ────────────
# The step-4 sync.status belongs to BRIDGE-SERVICE. The consumer #148 is really about
# is aggkit's L2BridgeSyncer (it fetches each ClaimEvent tx's claimAsset calldata to
# parse it; empty input STALLS it). Because we --force-recreate'd aggkit, its cursor is
# fresh and it re-scans the proxy L2 from block 0. If the claim STILL served empty input
# its parser would stall AT the claim's block and never log it processed. aggkit's
# L2BridgeSyncer logs `block N processed with M events` (bridgesync/processor.go), and
# it only reaches an event-bearing block once it has fetched+parsed that block's claim.
# Assert it processes to >= the recovered claim's block. (Fresh container ⇒ docker logs
# holds only the post-recreate re-scan, so no timestamp correlation is needed.)
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
CLAIM_BLOCK="$(pgq "SELECT block_number FROM synthetic_logs WHERE lower(transaction_hash) = lower('$CLAIM_TX') ORDER BY block_number DESC LIMIT 1" || true)"
[[ "$CLAIM_BLOCK" =~ ^[0-9]+$ ]] \
    || fail "#148: could not resolve the recovered claim's L2 block (CLAIM_TX=$CLAIM_TX) for the aggkit re-process assertion"
AGGKIT_DEADLINE=$(( $(date +%s) + 240 ))
AGGKIT_MAXBLK=0
while :; do
    AGGKIT_MAXBLK="$(docker logs "$AGGKIT_CONTAINER" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
        | grep -aE 'bridgesync/processor\.go.*block [0-9]+ processed' \
        | grep -aoE 'block [0-9]+ processed' | grep -aoE '[0-9]+' | sort -n | tail -1)"
    AGGKIT_MAXBLK="${AGGKIT_MAXBLK:-0}"
    [[ "$AGGKIT_MAXBLK" -ge "$CLAIM_BLOCK" ]] && break
    [[ $(date +%s) -ge $AGGKIT_DEADLINE ]] && break
    sleep 5
done
[[ "$AGGKIT_MAXBLK" -ge "$CLAIM_BLOCK" ]] \
    || fail "#148: aggkit's OWN L2BridgeSyncer did NOT re-process past the recovered claim after force-recreate (reached block $AGGKIT_MAXBLK < claim block $CLAIM_BLOCK within 240s) — its bridgesync stalled on the claim, so the empty-input recovery is NOT proven for the consumer that actually stalls"
pass "4b. aggkit's OWN L2BridgeSyncer re-processed the recovered claim from a FRESH cursor (reached block $AGGKIT_MAXBLK >= claim block $CLAIM_BLOCK) — the stalling consumer genuinely recovered, not just bridge-service"

rm -f "$BACKUP"
log "======================================================================"
log "  #148 RECOVERY READINESS PASS — consumers gated OFF during repair,"
log "  readiness gated on calldata repair; original calldata recovered"
log "  byte-for-byte; no foreign claim; released consumers settled"
log "======================================================================"
