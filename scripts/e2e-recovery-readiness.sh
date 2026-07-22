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

# The recovered claim's EXACT global index. The synthesised ClaimEvent encodes globalIndex
# as data word 0 (cf. lib-l2l2.sh::claim_event_rows `data LIKE 0x<gi>%`), so it is the first
# 32-byte word of the event data. We bind the recovery to THIS exact (hash, global index) —
# not merely "a block advanced" (PR #151 blocker).
GI_HEX="$(pgi "SELECT substring(lower(data) from 3 for 64) FROM synthetic_logs WHERE lower(transaction_hash) = lower('$CLAIM_TX') AND lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC') ORDER BY block_number DESC LIMIT 1")"
[[ "$GI_HEX" =~ ^[0-9a-f]{64}$ ]] \
    || fail "#148: could not extract the claim's global index from its ClaimEvent data (got '$GI_HEX')"
GLOBAL_INDEX="$(python3 -c "print(int('$GI_HEX',16))")"
# claimAsset(bytes32[32],bytes32[32],uint256 globalIndex,...): globalIndex is arg 3, at
# byte offset selector(4) + 2*bytes32[32](2*1024) = 2052 → hex-char offset 2*(2052)=4104
# after the 0x. Extract it from the calldata to bind the SERVED calldata to the exact GI
# (schema-free: proves the recovered calldata is THIS claim, not a same-shape placeholder).
gi_from_calldata() {
    python3 -c "
import sys
h=sys.argv[1]; h=h[2:] if h.startswith('0x') else h
off=2*(4+1024+1024)
print(h[off:off+64].lower() if len(h)>=off+64 else '')
" "$1"
}
GI_FROM_CD="$(gi_from_calldata "$ORIG_CALLDATA")"
[[ "$GI_FROM_CD" == "$GI_HEX" ]] \
    || fail "#148: the claim's ORIGINAL calldata globalIndex ('$GI_FROM_CD') != its ClaimEvent global index ('$GI_HEX') — calldata↔event binding broken (bad offset or wrong claim)"

CLAIM_COUNT_PRE="$(pgi "SELECT COUNT(*) FROM synthetic_logs WHERE lower(topics[1]) = lower('$CLAIM_EVENT_TOPIC')")"
[[ "$(proxy_health_code)" == "200" ]] || fail "#148: proxy not READY (200) before the recovery — precondition"
pass "PRE: claim $CLAIM_TX (global index $GLOBAL_INDEX, bound in its calldata) has ${#ORIG_CALLDATA}-char calldata; $CLAIM_COUNT_PRE ClaimEvent(s); /health=200"

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
# Blank only CLAIM_TX's calldata envelope — the LATEST landed claim, which is freshly
# produced and reliably reconstructable. (An earlier attempt blanked ALL claims to widen
# the recovering window, but that swept in older/foreign claims the backfill cannot
# re-consume/reconstruct, stalling /health at backlog>0 forever. It is also unnecessary:
# the withhold is observable via the DEGRADED node-reconnect window now that that /health
# body reports claims_awaiting_calldata — step 1 asserts the PROPERTY, 503+backlog>=1,
# not the transient `recovering` label.)
DELETED="$(pgi "WITH d AS (DELETE FROM transactions WHERE lower(tx_hash) = '$CLAIM_TX' RETURNING 1) SELECT COUNT(*) FROM d")"
[[ "$DELETED" == "1" ]] || fail "#148: expected to blank exactly 1 claim tx envelope, deleted '$DELETED'"
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
# The gating PROPERTY: while any historical ClaimEvent still lacks its calldata, /health is
# 503 with claims_awaiting_calldata >= 1; it flips to 200 only once repair completes. We
# assert THAT, not the specific sub-status label: a correct fast client can transition
# `degraded` (node not yet alive, backlog>0) straight to 200 without ever exposing the
# `recovering` (alive=true, backlog>0) sub-state (the same initial tick that flips alive
# true also drains a small backlog) — so requiring the `recovering` label is unsatisfiable
# for a healthy impl (PR #151 blocker). BOTH 503 sub-states now report the backlog, so
# "503 AND claims_awaiting_calldata>=1" is the reliable, observable withhold signal (the
# node-reconnect window is not sub-second). Single-curl capture (code+body atomically),
# polled tightly, deadline-bounded for the eventual 200.
SAW_WITHHELD=0; READY=0
RECOV_DEADLINE=$(( $(date +%s) + 600 ))
while [[ $(date +%s) -lt $RECOV_DEADLINE ]]; do
    RESP="$(curl -s -m5 -w $'\n%{http_code}' "$L2_RPC/health" 2>/dev/null || printf '{}\n000')"
    CODE="${RESP##*$'\n'}"; BODY="${RESP%$'\n'*}"
    if [[ "$CODE" == "503" ]]; then
        if echo "$BODY" | python3 -c "import json,sys;sys.exit(0 if (json.load(sys.stdin).get('claims_awaiting_calldata',0) or 0) >= 1 else 1)" 2>/dev/null; then
            SAW_WITHHELD=1
        fi
    elif [[ "$CODE" == "200" ]]; then
        READY=1; break
    fi
    sleep 0.3
done
[[ "$SAW_WITHHELD" == "1" ]] \
    || fail "#148: never observed /health=503 with claims_awaiting_calldata>=1 — the readiness gate did NOT hold while calldata was missing"
pass "1. Readiness WITHHELD: /health=503 (claims_awaiting_calldata>=1) while the claim calldata was missing"
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

# ── Pre-recovery snapshots for the consumer-attribution proofs (PR #151 round 3) ─────
# Captured BEFORE the consumers are force-recreated so the post-recovery deltas below can
# only be attributed to the recreated consumers, never to this script's own reads.
PROXY_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
CLAIM_TX_LC="$(echo "$CLAIM_TX" | tr 'A-F' 'a-f')"
# serve_count: how many times the (un-reset) proxy has served eth_getTransactionByHash for
# THIS exact hash (src/service.rs logs "served stored tx <hash>"). It INCLUDES this script's
# own reads (step 2) — step 4c requires a STRICT increase, so only a genuinely new (aggkit-
# driven) fetch can pass, never the test re-reading.
serve_count() {
    docker logs --tail "${PROXY_LOG_TAIL:-20000}" "$PROXY_CONTAINER" 2>&1 \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -iF 'served stored tx' | grep -icF "$CLAIM_TX_LC" || true
}
# cert_height: the MAX agglayer certificate Height aggkit has logged. Step 4e requires a cert
# STRICTLY newer than this pre-recovery max — never an old cert re-logged after the restart.
cert_height() {
    # `|| true`: no `Height:` line yet (fresh stack, no cert logged) makes grep exit 1, which
    # under `set -o pipefail` would abort the caller's `PRE_CERT_HEIGHT="$(cert_height)"`; a
    # missing height must read as empty (→ 0 via the `:-0` default), not kill the script.
    docker logs --tail "${AGGKIT_LOG_TAIL:-40000}" "$AGGKIT_CONTAINER" 2>&1 \
        | sed -E 's/\x1b\[[0-9;]*m//g' | grep -aoE 'Height: [0-9]+' | grep -aoE '[0-9]+' | sort -n | tail -1 || true
}
SERVES_BEFORE="$(serve_count)"; SERVES_BEFORE="${SERVES_BEFORE:-0}"
PRE_CERT_HEIGHT="$(cert_height)"; PRE_CERT_HEIGHT="${PRE_CERT_HEIGHT:-0}"
log "  pre-recovery snapshots: proxy served $CLAIM_TX_LC ${SERVES_BEFORE}x (incl. this script's reads); max agglayer cert height ${PRE_CERT_HEIGHT}"

# ── 4. Release the gated consumers; assert the recovered claim SETTLES ────────
# Only NOW (readiness=200) do we release the consumers the gate held off. This is
# the release the readiness probe authorises. Realistic operational resync
# (finding #65): the reset proxy nonces are 0, so drop bridge_db and let the
# bridge-service re-fetch from nonce 0 (else a future-nonce wedge). aggkit is
# --force-recreate'd (NOT plain up -d): aggkit's BridgeL2Sync cursor lives in the
# container's PathRWData=/tmp, which a plain `up -d` PRESERVES — so aggkit would
# RESUME from its old cursor and never re-fetch/re-parse the historical claim (PR
# #151 blocker). A fresh container re-scans the proxy L2 from block 0, genuinely
# re-exercising the empty-input-stall consumer. Step 4b asserts aggkit's OWN recovery;
# steps 4c/4e attribute the post-recovery serve + a strictly-newer settled cert to the
# recreated consumers via the SERVES_BEFORE / PRE_CERT_HEIGHT snapshots captured above.
docker exec "$BRIDGE_PG_CONTAINER" psql -U bridge_user -d bridge_db \
    -c "DROP SCHEMA IF EXISTS sync CASCADE; DROP SCHEMA IF EXISTS mt CASCADE; DROP SCHEMA public CASCADE; CREATE SCHEMA public; GRANT ALL ON SCHEMA public TO bridge_user;" >/dev/null 2>&1 \
    || fail "#148: failed to drop bridge_db for the realistic resync (finding #65)"
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-x}" MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-x}" \
    "${E2E_COMPOSE[@]}" up -d --no-deps --force-recreate aggkit bridge-service bridge-autoclaim >/dev/null 2>&1
# wait_for runs its predicate as a COMMAND (`"$@"` after `shift 3`), not an eval'd string,
# and its arg order is <desc> <timeout> <interval> <predicate-fn>. The prior call passed a
# `[[ … ]]` STRING in the timeout slot, so it errored every iteration and never terminated.
_bridge_svc_reachable() {
    local c; c="$(curl -s -m3 -o /dev/null -w '%{http_code}' "$BRIDGE_SERVICE_URL/" 2>/dev/null)"
    [[ "$c" =~ ^(200|404)$ ]]
}
wait_for "bridge-service reachable after recovery" 180 5 _bridge_svc_reachable
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
# NOTE: the calldata was verified byte-for-byte in step 2 (pre-release). We do NOT re-read
# eth_getTransactionByHash here — that would be the TEST serving the hash and would pollute
# the step-4c serve-count delta (which must attribute the next serve to aggkit alone).
pass "4. Released consumers SETTLED: bridge-service sync.status network 1 synced=$SYNCED remaining_blocks=$REMAIN (no empty-input stall)"

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

# ── 4c. EXACT-HASH FETCH by aggkit — serve-count DELTA (PR #151 round 3, gap 1) ──────
# Step 2 proved the repaired BYTES. This proves the FORCE-RECREATED aggkit itself re-fetched
# THIS exact hash: the proxy logs every stored tx it serves by exact hash; we snapshotted
# SERVES_BEFORE (incl. this script's own step-2 read) BEFORE the recreate, and now require a
# STRICT increase — a serve that can ONLY be aggkit's post-reset re-fetch, never the test.
# Correlated with 4b (aggkit demonstrably re-processed the claim's block), the new serve is its.
SERVE_DEADLINE=$(( $(date +%s) + 240 )); SERVES_AFTER="$SERVES_BEFORE"
while :; do
    SERVES_AFTER="$(serve_count)"; SERVES_AFTER="${SERVES_AFTER:-0}"
    [[ "$SERVES_AFTER" -gt "$SERVES_BEFORE" ]] && break
    [[ $(date +%s) -ge $SERVE_DEADLINE ]] && break
    sleep 5
done
[[ "$SERVES_AFTER" -gt "$SERVES_BEFORE" ]] \
    || fail "#148: the proxy did NOT serve the exact recovered hash $CLAIM_TX_LC to aggkit after force-recreate (serve count stuck at $SERVES_BEFORE within 240s) — aggkit did not re-fetch THIS claim's calldata (only a block number advanced)"
pass "4c. Proxy served the EXACT recovered hash to aggkit AFTER force-recreate (serve count $SERVES_BEFORE -> $SERVES_AFTER) — a genuinely NEW aggkit-driven fetch of THIS claim, not the test re-reading"

# ── 4d. CONSUMER-SIDE exact global index (PR #151 round 3, gap 2) ────────────────────
# The exact GI was bound INSIDE the proxy (ClaimEvent<->calldata, at PRE). Now require the
# SAME exact GI to be durably DELIVERED in the CONSUMER index: bridge_db (re-populated from
# the drop+resync) must hold a sync.claim row for this exact global_index — proving the
# consumer actually INGESTED THIS claim, not merely advanced a block. sync.claim.global_index
# is a `character varying` (a decimal STRING, e.g. '18446744073709551617'), so it MUST be
# compared as a QUOTED literal — an unquoted numeric fails with `text = bigint`. GLOBAL_INDEX
# is that same decimal value. Error-propagating (pgi_bridge).
GI_IN_CONSUMER="$(pgi_bridge "SELECT COUNT(*) FROM sync.claim WHERE global_index = '$GLOBAL_INDEX'")"
[[ "$GI_IN_CONSUMER" =~ ^[0-9]+$ && "$GI_IN_CONSUMER" -ge 1 ]] \
    || fail "#148: the recovered claim's EXACT global index $GLOBAL_INDEX is NOT delivered in the consumer index (bridge_db sync.claim count='$GI_IN_CONSUMER') — the consumer did not ingest THIS claim"
pass "4d. Consumer (bridge_db sync.claim) durably delivered the recovered claim's EXACT global index $GLOBAL_INDEX — exact-gi tie on the consumer side, not just a block advance"

# ── 4e. Post-recovery OUTBOUND + a STRICTLY-NEWER cert settled ON-CHAIN (gap 3) ──────
# An imported ClaimEvent is NOT a Local-Exit-Tree leaf, so it cannot by itself drive a fresh
# certificate. Submit ONE Miden->L1 bridge-out (a real LET leaf), then require a certificate
# with Height STRICTLY GREATER than the pre-recovery max (never an old cert re-logged after
# the restart), and receipt-check its SettlementTxnHash on L1 (status 0x1, to == RollupManager)
# — the on-chain proof that the aggsender -> agglayer -> L1 settlement pipeline resumed.
step "4e: post-recovery Miden->L1 bridge-out (fresh LET leaf), then require a strictly-newer settled cert proven on-chain"
if ! "$SCRIPT_DIR/e2e-l2-to-l1.sh" > "${TMPDIR:-/tmp}/rr-l2l1.$$.log" 2>&1; then
    sed -E 's/\x1b\[[0-9;]*m//g' "${TMPDIR:-/tmp}/rr-l2l1.$$.log" 2>/dev/null | tail -6 | sed 's/^/  [l2-to-l1] /'
    rm -f "${TMPDIR:-/tmp}/rr-l2l1.$$.log" 2>/dev/null || true
    fail "#148: post-recovery Miden->L1 bridge-out (e2e-l2-to-l1.sh) failed — the recovered stack cannot produce a fresh outbound exit for a new certificate"
fi
rm -f "${TMPDIR:-/tmp}/rr-l2l1.$$.log" 2>/dev/null || true
EMPTY_LER="0x27ae5ba08d7291c96c8cbddcc148bf48a6d68c7974b94356f53754ef6171d757"
CERT_DEADLINE=$(( $(date +%s) + 300 )); NEW_CERT_HEIGHT=""; NEW_SETTLEMENT_TX=""
while :; do
    # Parse each settled-cert line for Height / SettlementTxnHash / NewLocalExitRoot; keep the
    # highest with Height > pre-recovery max, a non-empty exit root, and a well-formed tx hash.
    CERT_LINE="$(docker logs --tail "${AGGKIT_LOG_TAIL:-40000}" "$AGGKIT_CONTAINER" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
        | grep -aE 'changed status.*to \[Settled\]' \
        | awk -v pre="$PRE_CERT_HEIGHT" -v empty="$EMPTY_LER" \
              -v zero="0x0000000000000000000000000000000000000000000000000000000000000000" '
            { h=""; st=""; nler="";
              for (i=1;i<=NF;i++) {
                  if ($i=="Height:")            { h=$(i+1);    gsub(/[,.]/,"",h) }
                  if ($i=="SettlementTxnHash:") { st=$(i+1);   gsub(/[,.]/,"",st) }
                  if ($i=="NewLocalExitRoot:")  { nler=$(i+1); gsub(/[,.]/,"",nler) }
              }
              # NewLocalExitRoot must be a well-formed 32-byte root that is NOT the empty-tree
              # root, NOT all-zero, and NOT missing (a missing field leaves nler="" which the
              # regex rejects) — otherwise the settlement proof could false-pass on a cert that
              # carries no real bridge-out leaf.
              if ((h+0)>(pre+0) && nler ~ /^0x[0-9a-fA-F]{64}$/ && nler!=empty && nler!=zero \
                 && st ~ /^0x[0-9a-fA-F]{64}$/) print h, st
            }' | sort -n | tail -1 || true)"
    if [[ -n "$CERT_LINE" ]]; then NEW_CERT_HEIGHT="${CERT_LINE%% *}"; NEW_SETTLEMENT_TX="${CERT_LINE##* }"; break; fi
    [[ $(date +%s) -ge $CERT_DEADLINE ]] && break
    sleep 10
done
[[ -n "$NEW_SETTLEMENT_TX" ]] \
    || fail "#148: NO certificate with Height > $PRE_CERT_HEIGHT (a genuinely NEW, non-empty-root cert) settled within 300s after the post-recovery bridge-out — the aggsender->agglayer settlement pipeline did not resume. Recent settled certs: $(docker logs --tail "${AGGKIT_LOG_TAIL:-40000}" "$AGGKIT_CONTAINER" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -aoE 'Height: [0-9]+, CertificateID' | tail -3 | tr '\n' '|')"
# On-chain settlement proof: the cert's SettlementTxnHash must be a confirmed L1 tx to the RollupManager.
RCPT_JSON="$(cast receipt "$NEW_SETTLEMENT_TX" --rpc-url "$L1_RPC" --json 2>/dev/null || true)"
RCPT_STATUS="$(echo "$RCPT_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin).get('status',''))" 2>/dev/null || true)"
RCPT_TO="$(echo "$RCPT_JSON" | python3 -c "import json,sys; print((json.load(sys.stdin).get('to') or '').lower())" 2>/dev/null || true)"
[[ "$RCPT_STATUS" == "0x1" ]] \
    || fail "#148: the fresh cert's SettlementTxnHash $NEW_SETTLEMENT_TX is NOT a confirmed L1 tx (receipt status='$RCPT_STATUS') — settlement not proven on-chain"
[[ "$RCPT_TO" == "$(echo "$ROLLUP_MANAGER" | tr 'A-F' 'a-f')" ]] \
    || fail "#148: the settlement tx $NEW_SETTLEMENT_TX target ('$RCPT_TO') is not the RollupManager ($ROLLUP_MANAGER) — not a genuine rollup settlement"
pass "4e. Post-recovery STRICTLY-NEWER certificate settled (Height $PRE_CERT_HEIGHT -> $NEW_CERT_HEIGHT) and its SettlementTxnHash $NEW_SETTLEMENT_TX is confirmed ON L1 (status 0x1, to=RollupManager) — aggsender -> agglayer -> L1 settlement resumed end-to-end"

rm -f "$BACKUP"
log "======================================================================"
log "  #148 RECOVERY READINESS PASS — consumers gated OFF during repair,"
log "  readiness gated on calldata repair; original calldata recovered"
log "  byte-for-byte; no foreign claim; released consumers settled"
log "======================================================================"
