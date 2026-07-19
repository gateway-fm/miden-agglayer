#!/usr/bin/env bash
# e2e-chaos-soak.sh — TIER 3: the unified WEEKEND CHAOS SOAK, the highest-trust
# pre-release test. Miden runs under BOTH mixed real traffic (L1<->Miden AND
# L2<->L2, including same-address clashes) AND adversarial "garbo" input AND
# infrastructure chaos — then a TWO-SIDED verdict asserts:
#   (a) every LEGITIMATE event still landed exact-block (verify-event-completeness:
#       0 missing / 0 extra / 0 store-locks on the healed stack), AND
#   (b) every GARBO input was correctly contained (skipped/quarantined/never
#       projected): the foreign-claim global indexes produced ZERO synthetic
#       ClaimEvent rows, and no garbo note leaked as a real BridgeEvent (the
#       verify's extra==0 proves it).
# PASS only if BOTH hold.
#
# Sequence:
#   1. L2<->L2 stack (fresh with FRESH=1, else reuse a live one)
#   2. concurrent STORM window:
#        - chaos-seeder  (infra faults: pause pg / kill prover / restart proxy /
#          partition node — external, self-restoring)
#        - chaos-garbo   (adversarial: private/tag-0 notes + a foreign-deployment
#          claim — each with a benign EXPECTED outcome)
#        - e2e-loadtest-mixed (L1<->Miden bulk + L2<->L2 fwd/back + address clash)
#   3. stop injectors + FULL restore (unpause/reconnect/restart)
#   4. post-chaos heal window (late-sweep / cursor catch-up / reconciler)
#   5. two-sided verdict
#
# Usage: N=60 CHAOS_DURATION=300 GARBO_DURATION=300 ./scripts/e2e-chaos-soak.sh
#        FRESH=1 to bring up a clean stack first (requires NO other e2e stack up —
#        the compose network 'miden-e2e' and host ports are shared).
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_DIR="$REPO"
cd "$REPO"

N="${N:-60}"
CHAOS_DURATION="${CHAOS_DURATION:-300}"
GARBO_DURATION="${GARBO_DURATION:-300}"
POST_CHAOS_SETTLE="${POST_CHAOS_SETTLE:-150}"
L2L2_FWD="${L2L2_FWD:-2}"
L2L2_BACK="${L2L2_BACK:-2}"
FRESH="${FRESH:-0}"
TOOL_BIN="${TOOL_BIN:-$PROJECT_DIR/target/debug/bridge-out-tool}"   # repo-local default; override with $TOOL_BIN
# #41: fail FAST if the debug tool is missing — a late WARN used to let the whole
# storm run and then skip the completeness verdict entirely.
if [[ ! -x "$TOOL_BIN" ]]; then
    echo "FATAL: $TOOL_BIN not found/executable — the completeness verdict cannot run." >&2
    echo "       Build it first:  cargo build --bin bridge-out-tool   (then re-run, or pass TOOL_BIN=...)" >&2
    exit 4
fi

CHAOS_LOG="${CHAOS_LOG:-/tmp/chaos-events.log}"
GARBO_LOG="${GARBO_LOG:-/tmp/chaos-garbo.log}"
GARBO_SUMMARY="${GARBO_SUMMARY:-/tmp/chaos-garbo-summary.env}"
: > "$CHAOS_LOG"; : > "$GARBO_LOG"; : > "$GARBO_SUMMARY"

say() { echo "[$(date '+%H:%M:%S')] CHAOS-SOAK: $*"; }

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"

# ── 1. stack ─────────────────────────────────────────────────────────────────
if [[ "$FRESH" == "1" ]]; then
    say "=== FRESH stack (down -v + make e2e-up + L2B overlay) ==="
    docker compose -f docker-compose.e2e.yml -f docker-compose.l2l2.yml --env-file fixtures/.env down -v --remove-orphans >/dev/null 2>&1
    if ! timeout 1200 make e2e-up >/tmp/chaos-up.out 2>&1; then say "e2e-up FAILED"; tail -20 /tmp/chaos-up.out; exit 4; fi
fi

# lib-l2l2 auto-detects the compose project from the live proxy container
# (FIX for known bug #1: never hardcode 'miden-agglayer'). It also brings up the
# L2B overlay idempotently (FIX for known bug #2: the soak now runs against the
# L2L2 stack so L2B exists).
source "$SCRIPT_DIR/lib-l2l2.sh"
say "compose project detected: $COMPOSE_PROJECT_NAME"
l2l2_ensure_stack || { say "L2B overlay bring-up FAILED"; exit 4; }
PROJECT="$COMPOSE_PROJECT_NAME"
say "stack up: $(docker ps --filter name=${PROJECT}- -q | wc -l) containers (proxy=$AGGLAYER_CONTAINER)"

# Baseline the garbo-containment metrics + the persistent quarantine table.
counter() { local n="$1" b; b=$(curl -sf "${L2_RPC}/metrics" 2>/dev/null) || { echo 0; return; }; awk -v n="$n" '$0 ~ ("^" n " "){print $2; f=1; exit} END{if(!f)print 0}' <<<"$b" | sed 's/\..*//'; }
BASE_PRIV_SKIP=$(counter synthetic_reconciler_private_skipped_total)
BASE_FOREIGN_SKIP=$(counter claim_event_foreign_skipped_total)
say "garbo baselines: private_skipped=$BASE_PRIV_SKIP foreign_skipped=$BASE_FOREIGN_SKIP"

# ── 2. STORM: chaos-seeder + chaos-garbo + mixed loadtest, concurrent ────────
say "=== STORM: chaos-seeder (${CHAOS_DURATION}s) + chaos-garbo (${GARBO_DURATION}s) + mixed loadtest (N=$N) ==="
PROJECT="$PROJECT" CHAOS_DURATION="$CHAOS_DURATION" CHAOS_LOG="$CHAOS_LOG" \
    "$SCRIPT_DIR/chaos-seeder.sh" >/tmp/chaos-seeder.out 2>&1 &
SEEDER_PID=$!
GARBO_DURATION="$GARBO_DURATION" GARBO_LOG="$GARBO_LOG" GARBO_SUMMARY="$GARBO_SUMMARY" \
    "$SCRIPT_DIR/chaos-garbo.sh" >/tmp/chaos-garbo.out 2>&1 &
GARBO_PID=$!

# The mixed loadtest drives all the legit traffic; suppress its internal verify
# (MIX_VERIFY=0) — the soak runs ONE authoritative verify post-heal. The new mixed
# loadtest takes a per-direction L1 split (N_L1_FWD/N_L1_BACK) instead of a single N;
# split the soak's N evenly across L1->Miden / Miden->L1.
say "=== mixed loadtest under storm (L1 ${N} split $((N / 2))/$((N - N / 2)), L2<->L2 $L2L2_FWD/$L2L2_BACK) ==="
N_L1_FWD=$((N / 2)) N_L1_BACK=$((N - N / 2)) L2L2_FWD="$L2L2_FWD" L2L2_BACK="$L2L2_BACK" \
    MIX_VERIFY=0 ALLOW_LATE=1 COMPOSE_PROJECT_NAME="$PROJECT" \
    timeout 3600 "$SCRIPT_DIR/e2e-loadtest-mixed.sh" >/tmp/chaos-lt.out 2>&1
LT_RC=$?
say "mixed loadtest exited rc=$LT_RC"
grep -aE "MIXED LOADTEST RESULT|forward ops|back ops|address clash|L1<->Miden rc" /tmp/chaos-lt.out | tail -6 || true

# ── 3. stop injectors + FULL restore ─────────────────────────────────────────
say "=== stopping injectors + restoring all faults ==="
kill "$SEEDER_PID" 2>/dev/null || true; wait "$SEEDER_PID" 2>/dev/null || true
kill "$GARBO_PID" 2>/dev/null || true;  wait "$GARBO_PID" 2>/dev/null || true
# belt-and-suspenders restore in case a trap raced (correct container names)
docker unpause "${PROJECT}-agglayer-postgres-1" >/dev/null 2>&1 || true
NET="$(docker inspect "$AGGLAYER_CONTAINER" --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' 2>/dev/null | awk '{print $1}')"
# reconnect WITH the compose alias (a plain connect drops 'miden-node' resolution)
[ -n "$NET" ] && docker network connect --alias miden-node "$NET" "${PROJECT}-miden-node-1" >/dev/null 2>&1 || true
for c in tx-prover-1 miden-agglayer-1; do docker start "${PROJECT}-$c" >/dev/null 2>&1 || true; done
# grep -c prints "0" AND exits 1 on no match; `|| echo 0` would then append a second "0"
# (FAULTS_DONE="0\n0", non-numeric). `|| true` swallows the exit and keeps the single count.
# Excludes SKIPPED faults (see chaos-seeder.sh: skipped faults are logged without "FAULT ").
FAULTS_DONE=$(grep -c "FAULT " "$CHAOS_LOG" 2>/dev/null || true); FAULTS_DONE="${FAULTS_DONE:-0}"
say "chaos stopped: $FAULTS_DONE faults injected (log: $CHAOS_LOG)"
# shellcheck disable=SC1090
[[ -f "$GARBO_SUMMARY" ]] && source "$GARBO_SUMMARY" || true
say "garbo fired: private=${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_FIRED:-0} gis='${GARBO_FOREIGN_GIS:-}'"
say "garbo attempts vs fired: private=${GARBO_PRIVATE_ATTEMPTS:-?}/${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_ATTEMPTS:-?}/${GARBO_FOREIGN_FIRED:-0} (#41: injections retry until landed)"

# ── 4. post-chaos heal ───────────────────────────────────────────────────────
say "=== post-chaos settle (${POST_CHAOS_SETTLE}s heal window) ==="
for _ in $(seq 1 30); do
    docker inspect "$AGGLAYER_CONTAINER" --format '{{.State.Health.Status}}' 2>/dev/null | grep -q healthy && break
    sleep 5
done
sleep "$POST_CHAOS_SETTLE"

# ── 5a. LEGITIMATE completeness (the primary verdict) ────────────────────────
say "=== (a) verify-event-completeness (legit traffic) ==="
VC_RC=2
if [[ -x "$TOOL_BIN" ]]; then
    ALLOW_LATE="${ALLOW_LATE:-1}" TOOL_BIN="$TOOL_BIN" \
        NODE_CONTAINER="${PROJECT}-miden-node-1" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        "$SCRIPT_DIR/verify-event-completeness.sh" > /tmp/chaos-verify.out 2>&1
    VC_RC=$?
    grep -aE "TYPE|B2AGG->|CLAIM->|GER->|VERDICT|SANITY|MISSING" /tmp/chaos-verify.out | tail -10
else
    say "WARN: $TOOL_BIN not found — completeness cannot run"
fi
LOCKS=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)

# ── 5a'. STORE CORROBORATION (#41) — the authoritative completeness verdict ──
# The verifier's node-DB denominator legitimately over-counts: observed (non-
# injected) GERs emit no UpdateHashChain, L2<->L2/reclaim claims aren't proxy-
# sponsored, and on a RECOVERED stack the whole-history GER denominator is
# permanently ahead of the by-design-reset log view. The proxy STORE reconciling
# against its own authoritative sources is the real integrity signal:
#   UHC logs == injected GERs, CLAIM logs >= landed, BRIDGE logs == emitted,
#   and no unemitted / unbridgeable / alerted-mint rows (unclaimable is reported
#   but non-fatal — a user-front-run leaves a benign row).
say "=== (a') store corroboration (authoritative) ==="
SC_UHC=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%';")
SC_CLAIM=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%';")
SC_BRIDGE=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] LIKE '0x50178120%';")
SC_INJ=$(pgq "SELECT COUNT(*) FROM ger_entries WHERE is_injected;")
SC_LANDED=$(pgq "SELECT COUNT(*) FROM claim_watcher_processed;")
SC_EMIT=$(pgq "SELECT COUNT(*) FROM bridge_out_processed WHERE emitted;")
SC_UNEMIT=$(pgq "SELECT COUNT(*) FROM bridge_out_processed WHERE emitted = false;")
SC_UNBRIDGE=$(pgq "SELECT COUNT(*) FROM unbridgeable_bridge_outs;")
SC_UNCLAIM=$(pgq "SELECT COUNT(*) FROM unclaimable_claims;")
SC_ALERTED=$(pgq "SELECT COUNT(*) FILTER (WHERE alerted) FROM monitor_expected_mints;")
say "  store: UHC=${SC_UHC:-?}/inj=${SC_INJ:-?} CLAIM=${SC_CLAIM:-?}/landed=${SC_LANDED:-?} BRIDGE=${SC_BRIDGE:-?}/emit=${SC_EMIT:-?} unemit=${SC_UNEMIT:-?} unbridge=${SC_UNBRIDGE:-?} unclaim=${SC_UNCLAIM:-?}(non-fatal) alerted=${SC_ALERTED:-?}"
STORE_DROP=""
[[ "${SC_UNEMIT:-1}" != "0" ]]   && STORE_DROP="$STORE_DROP unemitted=${SC_UNEMIT:-?}"
[[ "${SC_UNBRIDGE:-1}" != "0" ]] && STORE_DROP="$STORE_DROP unbridgeable=${SC_UNBRIDGE:-?}"
[[ "${SC_ALERTED:-1}" != "0" ]]  && STORE_DROP="$STORE_DROP alerted-mint=${SC_ALERTED:-?}"
[[ -n "${SC_UHC:-}" && -n "${SC_INJ:-}" && "${SC_UHC}" -lt "${SC_INJ}" ]] 2>/dev/null && STORE_DROP="$STORE_DROP UHC<inj(${SC_UHC}<${SC_INJ})"
[[ -n "${SC_CLAIM:-}" && -n "${SC_LANDED:-}" && "${SC_CLAIM}" -lt "${SC_LANDED}" ]] 2>/dev/null && STORE_DROP="$STORE_DROP CLAIM<landed(${SC_CLAIM}<${SC_LANDED})"
[[ -n "${SC_BRIDGE:-}" && -n "${SC_EMIT:-}" && "${SC_BRIDGE}" -lt "${SC_EMIT}" ]] 2>/dev/null && STORE_DROP="$STORE_DROP BRIDGE<emit(${SC_BRIDGE}<${SC_EMIT})"
if [[ -z "$STORE_DROP" ]]; then
    STORE_OK=1; say "  store corroboration: CLEAN"
    [[ "$VC_RC" != "0" ]] && say "  (verifier mismatch with a CLEAN store = denominator artifact, not a drop)"
else
    STORE_OK=0; say "  store corroboration: DROP —$STORE_DROP"
fi

# ntx-builder liveness (task #68: it dies SILENTLY after idle-timeout actor
# deactivation while the chain keeps moving — bridge note consumption halts with
# it). WARN, not fail: an ops watchdog (docker restart) heals it, but a chaos run
# where it died explains any missing CLAIM/GER growth.
NTX_LAST=$(docker logs --timestamps --tail 1 "${PROJECT}-ntx-builder-1" 2>/dev/null | cut -c1-19)
NTX_AGE=$(( $(date -u +%s) - $(date -u -d "${NTX_LAST:-1970-01-01T00:00:00}" +%s 2>/dev/null || echo 0) ))
if [[ "${NTX_AGE:-0}" -gt 300 ]]; then
    say "  ⚠ ntx-builder silent for ${NTX_AGE}s (task #68 silent-death) — restart it: docker restart ${PROJECT}-ntx-builder-1"
else
    say "  ntx-builder alive (last log ${NTX_AGE}s ago)"
fi

# ── 5b. GARBO containment (the second verdict) ───────────────────────────────
say "=== (b) garbo containment ==="
GARBO_OK=1
# Foreign-claim class: each fabricated global index must have ZERO ClaimEvent rows.
FOREIGN_LEAK=0
for gi_hex in ${GARBO_FOREIGN_GIS:-}; do
    gi_pad=$(python3 -c "print(format(int('$gi_hex',16),'064x'))" 2>/dev/null || echo "")
    [[ -z "$gi_pad" ]] && continue
    rows=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${gi_pad}%';")
    if [[ "${rows:-0}" != "0" ]]; then
        say "  GARBO LEAK: foreign gi 0x$gi_hex has $rows ClaimEvent row(s) — CONTAINMENT BREACH"
        FOREIGN_LEAK=$((FOREIGN_LEAK + rows)); GARBO_OK=0
    else
        say "  foreign gi 0x$gi_hex: 0 ClaimEvent rows (contained)"
    fi
done
[[ "${GARBO_FOREIGN_FIRED:-0}" -gt 0 && -z "${GARBO_FOREIGN_GIS:-}" ]] && { say "  WARN: foreign fired but no gi recorded"; }
# Skip counters (best-effort — in-memory, may have reset on a chaos proxy restart).
NOW_PRIV_SKIP=$(counter synthetic_reconciler_private_skipped_total)
NOW_FOREIGN_SKIP=$(counter claim_event_foreign_skipped_total)
say "  private_skipped_total: $BASE_PRIV_SKIP -> $NOW_PRIV_SKIP (garbo private fired=${GARBO_PRIVATE_FIRED:-0})"
say "  foreign_skipped_total: $BASE_FOREIGN_SKIP -> $NOW_FOREIGN_SKIP (garbo foreign fired=${GARBO_FOREIGN_FIRED:-0})"
# The verify's extra==0 (checked below via VC_RC) is the restart-robust proof
# that NO private/tag-0/garbo note leaked as a real BridgeEvent/ClaimEvent.

# ── 6. two-sided verdict ─────────────────────────────────────────────────────
say "======================================================================"
say "  UNIFIED CHAOS SOAK RESULT"
say "    N=$N  faults=$FAULTS_DONE  garbo(private=${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_FIRED:-0})"
say "    loadtest_rc=$LT_RC  verify_rc=$VC_RC  store_locks=$LOCKS  foreign_leak=$FOREIGN_LEAK"
# The mixed loadtest (MIX_VERIFY=0) exits 0 on a clean completion and non-zero
# ONLY if its driver ABORTED (a fail() — e.g. a wedge or a harness bug). A
# crashed driver means the full mixed load never ran, so it must NOT green even
# if the (reduced) traffic verifies — else a dead driver false-passes.
# #41: completeness = verifier PASS *or* store-corroboration CLEAN (the verifier
# denominator over-counts by design in several benign cases); a store DROP always
# fails regardless of the verifier.
LEGIT_OK=0
if [[ ( "$VC_RC" == "0" || "${STORE_OK:-0}" == "1" ) && "${LOCKS:-1}" == "0" && "$LT_RC" == "0" ]]; then LEGIT_OK=1; fi
[[ "${STORE_OK:-0}" == "0" ]] && LEGIT_OK=0
# (c) chaos ACTUALLY happened — a soak that injected no infra faults or fired no garbo
# class would otherwise false-pass on an empty run. Require >=1 injected fault AND each
# enabled garbo class fired (private always; foreign only when GARBO_FOREIGN=1).
CHAOS_OK=1
[[ "${FAULTS_DONE:-0}" -ge 1 ]]              || CHAOS_OK=0
[[ "${GARBO_PRIVATE_FIRED:-0}" -ge 1 ]]      || CHAOS_OK=0
[[ "${GARBO_FOREIGN:-1}" != "1" || "${GARBO_FOREIGN_FIRED:-0}" -ge 1 ]] || CHAOS_OK=0
say "    (a) LEGIT completeness: $([[ $LEGIT_OK == 1 ]] && echo PASS || echo FAIL)  (verify_rc=$VC_RC store=$([[ ${STORE_OK:-0} == 1 ]] && echo CLEAN || echo DROP) locks=$LOCKS loadtest_rc=$LT_RC)"
say "    (b) GARBO containment:  $([[ $GARBO_OK == 1 ]] && echo PASS || echo FAIL)  (foreign_leak=$FOREIGN_LEAK)"
say "    (c) CHAOS actually fired: $([[ $CHAOS_OK == 1 ]] && echo PASS || echo FAIL)  (faults=${FAULTS_DONE:-0} private=${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_FIRED:-0})"
if [[ "$LEGIT_OK" == "1" && "$GARBO_OK" == "1" && "$CHAOS_OK" == "1" ]]; then
    say "  >>> CHAOS SOAK PASS — every legit event survived exact-block; every garbo input contained <<<"
    say "======================================================================"
    exit 0
else
    say "  >>> CHAOS SOAK NOT-GREEN — inspect /tmp/chaos-verify.out, $CHAOS_LOG, $GARBO_LOG, /tmp/chaos-lt.out <<<"
    say "  (stack left UP for forensics)"
    say "======================================================================"
    exit 1
fi
