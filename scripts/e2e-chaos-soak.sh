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
# PR#145: PASS certifies EXACT-BLOCK fidelity by default — late events fail.
# A consciously-degraded run (e.g. a grown/recovered stack) may set ALLOW_LATE=1;
# the verdict line prints the mode either way.
ALLOW_LATE="${ALLOW_LATE:-0}"
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

# ── cancellation safety net ──────────────────────────────────────────────────
# Injected faults are real: a paused postgres, a disconnected node, a stopped
# prover/proxy. The injectors restore their own faults on a clean exit, but a
# SIGKILL to them — or a Ctrl-C / runner timeout on THIS script — leaves the
# stack faulted and every later test on this machine runs against a crippled
# system, which is how a chaos run contaminates the runs after it.
#
# Idempotent, best-effort, and installed BEFORE the first injector starts so
# there is no window where a fault exists without a way back.
CHAOS_CLEANUP_DONE=0
chaos_cleanup() {
    [ "$CHAOS_CLEANUP_DONE" = "1" ] && return 0
    CHAOS_CLEANUP_DONE=1
    say "cleanup: stopping injectors and reversing any live faults"
    for pid in "${SEEDER_PID:-}" "${GARBO_PID:-}" "${WATCHDOG_PID:-}"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    docker unpause "${PROJECT}-agglayer-postgres-1" >/dev/null 2>&1 || true
    docker unpause "${PROJECT}-postgres-1" >/dev/null 2>&1 || true
    local net
    net="$(docker inspect "$AGGLAYER_CONTAINER" \
        --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' 2>/dev/null \
        | awk '{print $1}')"
    # Reconnect WITH the compose alias — a plain connect drops 'miden-node'
    # name resolution and the stack stays functionally partitioned.
    [ -n "$net" ] && docker network connect --alias miden-node "$net" \
        "${PROJECT}-miden-node-1" >/dev/null 2>&1 || true
    for c in tx-prover-1 miden-agglayer-1 miden-node-1 ntx-builder-1; do
        docker start "${PROJECT}-$c" >/dev/null 2>&1 || true
    done
    say "cleanup: done (unpaused pg, reconnected node, ensured core services up)"
}
trap chaos_cleanup EXIT
trap 'chaos_cleanup; exit 130' INT TERM

# ── 2. STORM: chaos-seeder + chaos-garbo + mixed loadtest, concurrent ────────
say "=== STORM: chaos-seeder (${CHAOS_DURATION}s) + chaos-garbo (${GARBO_DURATION}s) + mixed loadtest (N=$N) ==="
PROJECT="$PROJECT" CHAOS_DURATION="$CHAOS_DURATION" CHAOS_LOG="$CHAOS_LOG" \
    "$SCRIPT_DIR/chaos-seeder.sh" >/tmp/chaos-seeder.out 2>&1 &
SEEDER_PID=$!
GARBO_DURATION="$GARBO_DURATION" GARBO_LOG="$GARBO_LOG" GARBO_SUMMARY="$GARBO_SUMMARY" \
    "$SCRIPT_DIR/chaos-garbo.sh" >/tmp/chaos-garbo.out 2>&1 &
GARBO_PID=$!

# ── aggkit lost-tx watchdog (production-mirroring; counted in the verdict) ────
# When a seeder proxy-SIGKILL races an aggoracle GER send, the tx can die IN
# TRANSIT: aggkit's ephemeral-monitoring DB marks it sent, but the proxy never
# durably admitted it (no row, nothing for #157's recovery to re-drive). aggkit
# then polls the unknown hash forever and never re-sends the deterministic ID —
# GER injection freezes and every downstream leg stalls. That is an aggkit gap
# (no rebroadcast-on-unknown-tx; upstream aggkit 0.8.3-rc1); in production a
# watchdog alert + aggkit bounce is the documented mitigation, so the soak runs
# the same watchdog: wedge signature = the SAME 'already exists in monitoring
# DB' hash repeating for >60s while the proxy has no such transaction row.
# Every bounce is logged and reported in the final verdict line.
WATCHDOG_HEALS_FILE=/tmp/chaos-watchdog-heals; : > "$WATCHDOG_HEALS_FILE"
(
  declare -A seen
  while true; do
    sleep 30
    # ONLY the base aggkit is watched here. aggkit-l2b submits its GER
    # injections to anvil-l2b (fixtures/aggkit-l2b-config.toml), NOT to this
    # proxy — so the "unknown to the proxy" probe below is trivially true for
    # every healthy L2B transaction, and the preserve-healer it would call
    # likewise reads the BASE proxy database. Watching aggkit-l2b through the
    # base proxy's transactions table therefore produced a heal decision from
    # evidence about a different chain entirely, and let unrelated base GER
    # activity satisfy an L2B "recovery" proof.
    #
    # Wiring an L2B-aware watchdog needs an L2B-side admission probe; until
    # then, not watching it is the honest option — a silent wrong answer is
    # worse than a missing one. Tracked as the L2B watchdog follow-up.
    for AK in "${PROJECT}-aggkit-1"; do
      docker inspect "$AK" >/dev/null 2>&1 || continue
      loops=$(docker logs "$AK" --since 60s 2>&1 | grep -c 'already exists in monitoring DB' || true)
      [ "${loops:-0}" -ge 10 ] || continue
      tx=$(docker logs "$AK" --since 60s 2>&1 | grep -oE 'ID: 0x[a-f0-9]{64}' | tail -1 | awk '{print $2}')
      [ -n "$tx" ] || continue
      known=$(docker exec "${PROJECT}-agglayer-postgres-1" psql -U agglayer -d agglayer_store -tAc \
          "SELECT count(*) FROM transactions WHERE tx_hash='$tx'" 2>/dev/null || echo probe-failed)
      [ "$known" = 0 ] || continue        # proxy knows it (or probe failed) -> #157 recovery's job, not ours
      [ -z "${seen[$tx]:-}" ] || continue # one bounce per lost tx
      seen[$tx]=1
      total=$(grep -c 'WATCHDOG:' "$WATCHDOG_HEALS_FILE" 2>/dev/null | head -1)
      total=${total:-0}
      # Heal budget: past this it's a hard failure, not flakiness — and it must
      # reach the final VERDICT (review 0814), not vanish in a skipped iteration.
      if [ "$total" -ge 6 ]; then
        grep -q 'WATCHDOG-BUDGET-EXHAUSTED' "$WATCHDOG_HEALS_FILE" \
          || echo "$(date +%H:%M:%S) WATCHDOG-BUDGET-EXHAUSTED: $total heals consumed and wedges persist" \
               | tee -a "$WATCHDOG_HEALS_FILE"
        continue
      fi
      svc=aggkit; [ "$AK" = "${PROJECT}-aggkit-l2b-1" ] && svc=aggkit-l2b
      # PR#164 #8: PRESERVE-HEAL instead of a blind force-recreate. The old
      # recreate wiped the whole container fs to clear one poisoned file,
      # destroying aggsender cert lineage (#89) and the bridgesync cursors
      # (cold resync into anvil's 256-state wall permanently halts L2<->L2,
      # #87). aggkit-preserve-heal.sh wipes ONLY ethtxmanager-aggoracle.sqlite
      # and restores every other DB — the versioned, self-contained primitive.
      # PR#164 re-review — count a heal only AFTER it actually succeeded. The old
      # form logged the attempt into the heal ledger and swallowed the exit code
      # with `|| true`, so a heal that failed (or refused to run, e.g. the new
      # fail-closed staging guard) still consumed heal budget and still read as a
      # successful intervention. Log the attempt separately from the outcome, and
      # only the OUTCOME feeds the counted ledger.
      echo "$(date +%H:%M:%S) WATCHDOG-ATTEMPT: $AK wedged on lost-in-transit tx $tx (unknown to proxy) — preserve-healing $svc"
      if PROJECT="$PROJECT" FORCE=1 WEDGE_PATTERN='already exists in monitoring DB' \
          WEDGE_TX="$tx" \
          "$SCRIPT_DIR/aggkit-preserve-heal.sh" "$svc" >/dev/null 2>&1; then
        echo "$(date +%H:%M:%S) WATCHDOG: preserve-heal of $svc SUCCEEDED (tx $tx)" \
            | tee -a "$WATCHDOG_HEALS_FILE"
      else
        rc=$?
        # Persisted into the ledger (review 0814): a failed heal must veto the
        # verdict, not just scroll past in the watchdog log.
        echo "$(date +%H:%M:%S) WATCHDOG-FAILED: preserve-heal of $svc returned rc=$rc (tx $tx) — NOT counted as a heal" \
            | tee -a "$WATCHDOG_HEALS_FILE"
      fi
    done
  done
) >>/tmp/chaos-watchdog.out 2>&1 &
WATCHDOG_PID=$!

# The mixed loadtest drives all the legit traffic; suppress its internal verify
# (MIX_VERIFY=0) — the soak runs ONE authoritative verify post-heal. The new mixed
# loadtest takes a per-direction L1 split (N_L1_FWD/N_L1_BACK) instead of a single N;
# split the soak's N evenly across L1->Miden / Miden->L1.
say "=== mixed loadtest under storm (L1 ${N} split $((N / 2))/$((N - N / 2)), L2<->L2 $L2L2_FWD/$L2L2_BACK) ==="
N_L1_FWD=$((N / 2)) N_L1_BACK=$((N - N / 2)) L2L2_FWD="$L2L2_FWD" L2L2_BACK="$L2L2_BACK" \
    MIX_VERIFY=0 ALLOW_LATE="$ALLOW_LATE" COMPOSE_PROJECT_NAME="$PROJECT" \
    timeout "${CHAOS_LT_TIMEOUT:-9000}" "$SCRIPT_DIR/e2e-loadtest-mixed.sh" >/tmp/chaos-lt.out 2>&1
LT_RC=$?
say "mixed loadtest exited rc=$LT_RC"
grep -aE "MIXED LOADTEST RESULT|forward ops|back ops|address clash|L1<->Miden rc" /tmp/chaos-lt.out | tail -6 || true

# ── 3. stop injectors + FULL restore ─────────────────────────────────────────
say "=== stopping injectors + restoring all faults ==="
kill "$SEEDER_PID" 2>/dev/null || true; wait "$SEEDER_PID" 2>/dev/null || true
kill "$GARBO_PID" 2>/dev/null || true;  wait "$GARBO_PID" 2>/dev/null || true
kill "$WATCHDOG_PID" 2>/dev/null || true; wait "$WATCHDOG_PID" 2>/dev/null || true
# CLEAR the PID variables now that these children are reaped. The EXIT trap
# runs on the SUCCESS path too, and a reaped PID can already have been reused
# by an unrelated process on this shared host — signalling it would be someone
# else's outage caused by our cleanup.
SEEDER_PID=""; GARBO_PID=""; WATCHDOG_PID=""
WATCHDOG_HEALS=$(grep -c 'WATCHDOG:' "$WATCHDOG_HEALS_FILE" 2>/dev/null | head -1); WATCHDOG_HEALS=${WATCHDOG_HEALS:-0}
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
# Review 0814: failure to regain proxy health must feed the VERDICT — the old
# loop broke on healthy but fell through silently on timeout.
PROXY_HEALTHY=0
for _ in $(seq 1 30); do
    if docker inspect "$AGGLAYER_CONTAINER" --format '{{.State.Health.Status}}' 2>/dev/null | grep -q healthy; then
        PROXY_HEALTHY=1; break
    fi
    sleep 5
done
[[ "$PROXY_HEALTHY" == "1" ]] || say "WARN: proxy did NOT report healthy within 150s post-chaos (feeds verdict)"
sleep "$POST_CHAOS_SETTLE"

# ── 4b. post-chaos FRESH two-way liveness (review 0814) ──────────────────────
# The storm-phase load may have completed BEFORE the last fault, and the log
# verifier reads state that can predate the recovery — neither proves the stack
# works AFTER the faults. Require one fresh deposit + one fresh withdrawal to
# complete post-chaos before the soak may PASS.
# Snapshot service state BEFORE the post-op: the loadtest calls
# l2l2_ensure_stack, which brings missing services back UP. Checking
# services-running only after that would credit the harness's own repair to the
# system under test.
POST_STORM_DOWN=""
_L2B_OVERLAY=0
[[ -f "$REPO/docker-compose.l2l2.yml" ]] && _L2B_OVERLAY=1
# One snapshot rule for every service. The core loop used to be status-only
# with a second inspect call (so a race rendered `svc=` instead of typed
# evidence) and ignored the proxy's own healthcheck.
_snapshot_service() {   # <service>  -> appends to POST_STORM_DOWN when not OK
    local svc="$1" insp err st health
    # ONE inspect. Two calls meant the first failure was labelled MISSING
    # whatever the cause — absence, a daemon hiccup, permissions — so a
    # transient docker error read as "the storm destroyed this container".
    # Distinguish by the error text docker itself uses for absence.
    insp=$(docker inspect \
        -f '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
        "${PROJECT}-${svc}-1" 2>&1)
    if [[ $? -ne 0 ]]; then
        err="$insp"
        if [[ "$err" == *"No such object"* || "$err" == *"no such container"* ]]; then
            [[ "$2" == "required" ]] && POST_STORM_DOWN="$POST_STORM_DOWN ${svc}=MISSING"
        else
            POST_STORM_DOWN="$POST_STORM_DOWN ${svc}=INSPECT-FAILED"
        fi
        return
    fi
    read -r st health <<<"$insp"
    case "$st:$health" in
        running:healthy|running:none) ;;
        running:starting) POST_STORM_DOWN="$POST_STORM_DOWN ${svc}=health-starting" ;;
        running:*)        POST_STORM_DOWN="$POST_STORM_DOWN ${svc}=running-but-${health}" ;;
        *)                POST_STORM_DOWN="$POST_STORM_DOWN ${svc}=${st}" ;;
    esac
}
for svc in miden-agglayer aggkit bridge-service miden-node ntx-builder; do
    _snapshot_service "$svc" required
done
# EVERY service l2l2_ensure_stack can bring back must be in this snapshot,
# not just the ones checked in the final verdict: anything it repairs while
# unobserved is a fault the storm caused and the harness silently undid.
# (lib-l2l2.sh brings up anvil-l2b, aggkit-l2b, agglayer, bridge-service,
# postgres-l2b and bridge-service-l2b.)
# `l2l2_ensure_stack` runs `docker compose up` WITHOUT --no-deps, so it can
# also restart anything those services depend on (anvil, postgres, tx-prover,
# agglayer-postgres). Any of those repaired while unobserved is a storm-caused
# fault the harness silently undid, so they belong in the snapshot too.
for svc in aggkit-l2b bridge-service-l2b anvil-l2b postgres-l2b agglayer \
           anvil postgres tx-prover agglayer-postgres validator; do
    # Required only when the l2l2 overlay is in play; otherwise absence is
    # simply "this stack does not run it".
    if [[ "$_L2B_OVERLAY" == "1" ]]; then _snapshot_service "$svc" required
    else _snapshot_service "$svc" optional; fi
done
say "=== (pre-verdict) fresh post-chaos operation (L1<->Miden + BOTH L2<->L2 directions) ==="
say "    post-storm service state before any harness repair: ${POST_STORM_DOWN:-all running}"
# BOTH L2<->L2 directions run after the last fault. Naming here is a trap worth
# spelling out, because getting it wrong silently tests the wrong thing:
# e2e-loadtest-mixed defines L2L2_FWD as L2B->Miden and L2L2_BACK as
# Miden->L2B. The probe used to disable both, so an aggkit-l2b aggoracle left
# frozen by the storm passed every gate; a first attempt then set FWD=1 while
# describing it as Miden->L2B, which exercised the direction that was already
# covered. Running both removes the ambiguity entirely — the L2B path is what
# chaos breaks most often (#41/#87), and neither direction proves the other.
POSTOP_RC=1
if N_L1_FWD=1 N_L1_BACK=1 L2L2_FWD=1 L2L2_BACK=1 MIX_VERIFY=0 ALLOW_LATE="$ALLOW_LATE" \
    COMPOSE_PROJECT_NAME="$PROJECT" timeout "${CHAOS_POSTOP_TIMEOUT:-3600}" "$SCRIPT_DIR/e2e-loadtest-mixed.sh" \
    >/tmp/chaos-postop.out 2>&1; then
    POSTOP_RC=0
fi
say "post-chaos fresh op rc=$POSTOP_RC $(grep -aE 'OVERALL RELIABILITY' /tmp/chaos-postop.out | tail -1 | cut -c1-80)"

# ── 5a. LEGITIMATE completeness (the primary verdict) ────────────────────────
say "=== (a) verify-event-completeness (legit traffic) ==="
VC_RC=2
if [[ -x "$TOOL_BIN" ]]; then
    ALLOW_LATE="$ALLOW_LATE" TOOL_BIN="$TOOL_BIN" \
        NODE_CONTAINER="${PROJECT}-miden-node-1" AGGLAYER_CONTAINER="$AGGLAYER_CONTAINER" \
        "$SCRIPT_DIR/verify-event-completeness.sh" > /tmp/chaos-verify.out 2>&1
    VC_RC=$?
    grep -aE "TYPE|B2AGG->|CLAIM->|GER->|VERDICT|SANITY|MISSING" /tmp/chaos-verify.out | tail -10
else
    say "WARN: $TOOL_BIN not found — completeness cannot run"
fi
LOCKS=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)

# ── 5a'. STORE CORROBORATION (#41) — ADDITIONAL failure signal, never a PASS ─
# PR#145 review: both sides of this comparison are proxy-owned PostgreSQL
# (synthetic_logs vs ger_entries / claim_watcher_processed /
# bridge_out_processed) compared as aggregate counts — it cannot prove events
# the proxy never observed, nor identity / exact-block fidelity. The
# INDEPENDENT verifier above is the completeness authority; this section can
# only VETO a verifier-pass (a store DROP always fails) and serves as a
# diagnostic when the verifier's node denominator over-counts (observed GERs
# emit no UpdateHashChain; non-sponsored claims; recovered-stack history).
say "=== (a') store corroboration (additional failure signal) ==="
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
    # Diagnostic label ONLY — a verifier fail still fails the verdict (PR#145).
    [[ "$VC_RC" != "0" ]] && say "  (diagnostic: verifier mismatch with a CLEAN store is usually a denominator artifact — but the independent verifier remains authoritative)"
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
# PR#145: DIRECT private/tag-0 containment — each fired private note's id must
# be ABSENT from every proxy table a projected note would persist into
# (bridge_out_processed, tx_note_links, unbridgeable_bridge_outs). A leak by
# definition persists rows in the proxy's own store before getLogs can serve
# it, so ID absence here is a positive containment proof independent of the
# in-memory skip counters (which reset on a chaos proxy restart).
PRIVATE_LEAK=0
PRIV_IDS_CHECKED=0
for pid_hex in ${GARBO_PRIVATE_NOTE_IDS:-}; do
    pid_lc=$(echo "$pid_hex" | tr 'A-F' 'a-f'); pid_lc="${pid_lc#0x}"
    [[ -z "$pid_lc" ]] && continue
    PRIV_IDS_CHECKED=$((PRIV_IDS_CHECKED + 1))
    rows=$(pgq "SELECT (SELECT COUNT(*) FROM bridge_out_processed WHERE lower(note_id) LIKE '%${pid_lc}%')
              + (SELECT COUNT(*) FROM tx_note_links WHERE lower(coalesce(note_id,'')) LIKE '%${pid_lc}%' OR lower(coalesce(note_commitment,'')) LIKE '%${pid_lc}%')
              + (SELECT COUNT(*) FROM unbridgeable_bridge_outs WHERE lower(note_id) LIKE '%${pid_lc}%');")
    if [[ "${rows:-0}" != "0" ]]; then
        say "  GARBO LEAK: private note 0x$pid_lc persisted $rows proxy row(s) — CONTAINMENT BREACH"
        PRIVATE_LEAK=$((PRIVATE_LEAK + rows)); GARBO_OK=0
    fi
done
if [[ "$PRIV_IDS_CHECKED" -gt 0 && "$PRIVATE_LEAK" == "0" ]]; then
    say "  private notes: $PRIV_IDS_CHECKED id(s) checked, 0 persisted proxy rows (contained)"
elif [[ "${GARBO_PRIVATE_FIRED:-0}" -gt 0 && "$PRIV_IDS_CHECKED" == "0" ]]; then
    # Fired but ids not recorded (old summary / parse miss): fall back to the
    # independent verifier (its extra==0), which the overall verdict already
    # requires via VC_RC==0.
    say "  WARN: private fired but no note ids recorded — relying on the independent verifier's extra==0"
fi
# Skip counters (best-effort — in-memory, may have reset on a chaos proxy restart).
NOW_PRIV_SKIP=$(counter synthetic_reconciler_private_skipped_total)
NOW_FOREIGN_SKIP=$(counter claim_event_foreign_skipped_total)
say "  private_skipped_total: $BASE_PRIV_SKIP -> $NOW_PRIV_SKIP (garbo private fired=${GARBO_PRIVATE_FIRED:-0})"
say "  foreign_skipped_total: $BASE_FOREIGN_SKIP -> $NOW_FOREIGN_SKIP (garbo foreign fired=${GARBO_FOREIGN_FIRED:-0})"
# The verify's extra==0 (required below via VC_RC==0) is the restart-robust
# proof that NO private/tag-0/garbo note leaked as a real BridgeEvent/ClaimEvent.

# ── 6. two-sided verdict ─────────────────────────────────────────────────────
# PR#145: predicates live in lib-chaos-verdict.sh (unit-tested by
# scripts/test-chaos-verdict.sh). The INDEPENDENT verifier is required
# (VC_RC==0); store corroboration is an additional veto, never an override.
# shellcheck source=lib-chaos-verdict.sh
source "$SCRIPT_DIR/lib-chaos-verdict.sh"
say "======================================================================"
say "  UNIFIED CHAOS SOAK RESULT"
say "    N=$N  faults=$FAULTS_DONE  garbo(private=${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_FIRED:-0})  allow_late=$ALLOW_LATE  aggkit_watchdog_heals=${WATCHDOG_HEALS:-0}"
say "    loadtest_rc=$LT_RC  verify_rc=$VC_RC  store_locks=$LOCKS  foreign_leak=$FOREIGN_LEAK  private_leak=$PRIVATE_LEAK"
# The mixed loadtest (MIX_VERIFY=0) now enforces its FULL operational verdict
# (all fwd/back landed, clash distinct, L1 rc) and skips only the duplicate
# verifier run — a nonzero LT_RC means an operation never landed OR the driver
# aborted; either must fail here.
LEGIT_OK=0
chaos_legit_ok "$VC_RC" "${STORE_OK:-0}" "${LOCKS:-1}" "$LT_RC" && LEGIT_OK=1
GARBO_VERDICT_OK=0
[[ "$GARBO_OK" == "1" ]] && chaos_garbo_ok "$FOREIGN_LEAK" "$PRIVATE_LEAK" && GARBO_VERDICT_OK=1
# (c) chaos ACTUALLY happened — a soak that injected no infra faults or fired no garbo
# class would otherwise false-pass on an empty run. Require >=1 injected fault AND each
# enabled garbo class fired (private always; foreign only when GARBO_FOREIGN=1).
CHAOS_OK=0
chaos_fired_ok "${FAULTS_DONE:-0}" "${GARBO_PRIVATE_FIRED:-0}" "${GARBO_FOREIGN:-1}" "${GARBO_FOREIGN_FIRED:-0}" && CHAOS_OK=1
# (d) POST-CHAOS liveness (redesigned 2026-08-18, chaos-green): mid-storm heal
# churn is EXPECTED — faults re-form the aggoracle wedge while injection runs,
# and post-cf78a0e a "failed" heal is a soft-fail that leaves the service
# RUNNING (2026-08-18 12:17 run: heal_fails=11 + budget exhausted, yet every
# service up and the fresh op operationally green). Counting storm-phase heal
# attempts as liveness failures made (d) structurally red. The truthful gate:
#   - every stack service is RUNNING after the storm (a heal that failed HARD
#     leaves one down — still caught),
#   - the proxy is healthy,
#   - the fresh two-way op lands (the positive end-to-end proof).
# WATCHDOG_FAILED / BUDGET_EXHAUSTED stay in the summary as telemetry.
WATCHDOG_FAILED=$(grep -c 'WATCHDOG-FAILED' "$WATCHDOG_HEALS_FILE" 2>/dev/null | head -1); WATCHDOG_FAILED=${WATCHDOG_FAILED:-0}
BUDGET_EXHAUSTED=$(grep -c 'WATCHDOG-BUDGET-EXHAUSTED' "$WATCHDOG_HEALS_FILE" 2>/dev/null | head -1); BUDGET_EXHAUSTED=${BUDGET_EXHAUSTED:-0}
SERVICES_DOWN=""
for svc in miden-agglayer aggkit bridge-service miden-node ntx-builder; do
    st=$(docker inspect -f '{{.State.Status}}' "${PROJECT}-${svc}-1" 2>/dev/null || echo missing)
    [[ "$st" == "running" ]] || SERVICES_DOWN="$SERVICES_DOWN ${svc}=${st}"
done
# The L2B services are REQUIRED, not optional, whenever this stack runs the
# l2l2 overlay — `docker inspect` failing means the container is GONE, which
# the old `if` treated as "not applicable" and skipped. A destroyed aggkit-l2b
# is the loudest possible failure, not an absent one.
L2B_STACK=0
[[ -f "$REPO/docker-compose.l2l2.yml" ]] && L2B_STACK=1
for svc in aggkit-l2b bridge-service-l2b; do
    if docker inspect "${PROJECT}-${svc}-1" >/dev/null 2>&1; then
        st=$(docker inspect -f '{{.State.Status}}' "${PROJECT}-${svc}-1" 2>/dev/null)
        [[ "$st" == "running" ]] || SERVICES_DOWN="$SERVICES_DOWN ${svc}=${st}"
    elif [[ "$L2B_STACK" == "1" ]]; then
        SERVICES_DOWN="$SERVICES_DOWN ${svc}=MISSING"
    fi
done
# The post-storm snapshot is a VERDICT CONDITION, not commentary. The post-op
# runs l2l2_ensure_stack, which brings missing services back up — so checking
# only the post-op state credits the harness's own repair to the system under
# test. A service still down after the storm's own heal window means chaos left
# it down; that is a chaos failure regardless of what the repair achieved.
# CHAOS_ALLOW_HARNESS_REPAIR=1 exists for deliberate teardown experiments and
# must never be set in a release gate.
POSTLIVE_OK=0
SELF_RECOVERED=1
if [[ -n "$POST_STORM_DOWN" && "${CHAOS_ALLOW_HARNESS_REPAIR:-0}" != "1" ]]; then
    SELF_RECOVERED=0
fi
[[ -z "$SERVICES_DOWN" && "$SELF_RECOVERED" == "1" && "${PROXY_HEALTHY:-0}" == "1" && "${POSTOP_RC:-1}" == "0" ]] && POSTLIVE_OK=1
say "    (a) LEGIT completeness: $([[ $LEGIT_OK == 1 ]] && echo PASS || echo FAIL)  (verify_rc=$VC_RC store=$([[ ${STORE_OK:-0} == 1 ]] && echo CLEAN || echo DROP) locks=$LOCKS loadtest_rc=$LT_RC allow_late=$ALLOW_LATE)"
say "    (b) GARBO containment:  $([[ $GARBO_VERDICT_OK == 1 ]] && echo PASS || echo FAIL)  (foreign_leak=$FOREIGN_LEAK private_leak=$PRIVATE_LEAK)"
say "    (c) CHAOS actually fired: $([[ $CHAOS_OK == 1 ]] && echo PASS || echo FAIL)  (faults=${FAULTS_DONE:-0} private=${GARBO_PRIVATE_FIRED:-0} foreign=${GARBO_FOREIGN_FIRED:-0})"
say "    (d) POST-CHAOS liveness: $([[ $POSTLIVE_OK == 1 ]] && echo PASS || echo FAIL)  (services_down='${SERVICES_DOWN:-none}' proxy_healthy=${PROXY_HEALTHY:-0} fresh_op_rc=${POSTOP_RC:-1} [L1 both ways + L2B both ways]; self_recovered=$SELF_RECOVERED post-storm_before_repair='${POST_STORM_DOWN:-all running}'; telemetry: heal_softfails=$WATCHDOG_FAILED budget_exhausted=$BUDGET_EXHAUSTED)"
if [[ "$LEGIT_OK" == "1" && "$GARBO_VERDICT_OK" == "1" && "$CHAOS_OK" == "1" && "$POSTLIVE_OK" == "1" ]]; then
    if [[ "$ALLOW_LATE" == "0" ]]; then
        say "  >>> CHAOS SOAK PASS — every legit event survived exact-block; every garbo input contained <<<"
    else
        say "  >>> CHAOS SOAK PASS — every legit event survived (ALLOW_LATE=1: late events permitted, NOT exact-block certified); every garbo input contained <<<"
    fi
    say "======================================================================"
    exit 0
else
    say "  >>> CHAOS SOAK NOT-GREEN — inspect /tmp/chaos-verify.out, $CHAOS_LOG, $GARBO_LOG, /tmp/chaos-lt.out <<<"
    say "  (stack left UP for forensics)"
    say "======================================================================"
    exit 1
fi
