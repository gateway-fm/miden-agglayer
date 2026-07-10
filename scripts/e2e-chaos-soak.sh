#!/usr/bin/env bash
# e2e-chaos-soak.sh — the highest-trust test: drive a large loadtest WHILE a
# randomized fault storm hits the proxy + its deps, then assert ZERO events were
# lost across the chaos. If the proxy's resilience contracts hold (H1/H2 atomic
# commits, #26 sync retry, #27 late-sweep heal, cursor persistence), every
# submitted bridge-out/claim/GER still lands its synthetic event.
#
# Sequence:
#   1. fresh stack
#   2. start chaos-seeder (background, self-restoring) covering the load+settle
#   3. run the loadtest (submits + settles) — NO internal verify (SKIP the noisy
#      mid-chaos verify; we verify once at the end on a healed stack)
#   4. stop chaos, restore all faults, wait for it to fully exit
#   5. POST-CHAOS SETTLE: give the proxy time to heal (late-sweep, cursor catch-up,
#      any restart re-sync) — the whole point of #27's fix
#   6. verify-event-completeness = THE verdict: 0 missing / 0 extra / 0 store-locks
#
# Usage: N=100 CHAOS_DURATION=600 ./scripts/e2e-chaos-soak.sh
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO"

PROJECT="${PROJECT:-miden-agglayer}"
N="${N:-100}"
CHAOS_DURATION="${CHAOS_DURATION:-600}"
POST_CHAOS_SETTLE="${POST_CHAOS_SETTLE:-120}"
CHAOS_LOG="${CHAOS_LOG:-/tmp/chaos-events.log}"
: > "$CHAOS_LOG"

say() { echo "[$(date '+%H:%M:%S')] CHAOS-SOAK: $*"; }

say "=== fresh stack ==="
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env down -v --remove-orphans >/dev/null 2>&1
if ! timeout 900 make e2e-up >/tmp/chaos-up.out 2>&1; then say "e2e-up FAILED"; tail -15 /tmp/chaos-up.out; exit 4; fi
say "stack up: $(docker ps --filter name=${PROJECT}- -q | wc -l) containers"

# 2. chaos in the background, bounded, self-restoring
say "=== launching chaos-seeder (${CHAOS_DURATION}s) ==="
PROJECT="$PROJECT" CHAOS_DURATION="$CHAOS_DURATION" CHAOS_LOG="$CHAOS_LOG" \
    "$SCRIPT_DIR/chaos-seeder.sh" &
CHAOS_PID=$!

# 3. loadtest concurrently — skip its internal verify (we do the authoritative one post-heal)
say "=== loadtest N=$N under chaos ==="
N="$N" ALLOW_LATE=1 VERIFY=0 timeout 2400 "$SCRIPT_DIR/e2e-bridge-loadtest-isolated.sh" >/tmp/chaos-lt.out 2>&1
LT_RC=$?
say "loadtest exited rc=$LT_RC"

# 4. stop chaos + guarantee full restore
say "=== stopping chaos + restoring all faults ==="
kill "$CHAOS_PID" 2>/dev/null || true
wait "$CHAOS_PID" 2>/dev/null || true
# belt-and-suspenders restore in case the trap raced
docker unpause "${PROJECT}-miden-agglayer-postgres-1" >/dev/null 2>&1 || true
NET="$(docker inspect ${PROJECT}-miden-agglayer-1 --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' 2>/dev/null | awk '{print $1}')"
[ -n "$NET" ] && docker network connect "$NET" "${PROJECT}-miden-node-1" >/dev/null 2>&1 || true
for c in tx-prover-1 miden-agglayer-1; do docker start "${PROJECT}-$c" >/dev/null 2>&1 || true; done
FAULTS_DONE=$(grep -c "^\[.*\] FAULT " "$CHAOS_LOG" 2>/dev/null || echo 0)
say "chaos stopped: $FAULTS_DONE faults injected (log: $CHAOS_LOG)"

# 5. POST-CHAOS HEAL — wait for the proxy to fully recover + catch up
say "=== post-chaos settle (${POST_CHAOS_SETTLE}s heal window) ==="
# wait for the proxy to be healthy again first
for _ in $(seq 1 30); do
    docker inspect "${PROJECT}-miden-agglayer-1" --format '{{.State.Health.Status}}' 2>/dev/null | grep -q healthy && break
    sleep 5
done
sleep "$POST_CHAOS_SETTLE"

# 6. THE VERDICT — authoritative completeness on the healed stack
say "=== verify-event-completeness (the verdict) ==="
TOOL_BIN="${TOOL_BIN:-$REPO/target/debug/bridge-out-tool}"
if [[ ! -x "$TOOL_BIN" ]]; then say "WARN: $TOOL_BIN not built — completeness cannot run (build with cargo build --bin bridge-out-tool)"; VC_RC=2
else
    ALLOW_LATE="${ALLOW_LATE:-1}" TOOL_BIN="$TOOL_BIN" \
        NODE_CONTAINER="${PROJECT}-miden-node-1" AGGLAYER_CONTAINER="${PROJECT}-miden-agglayer-1" \
        "$SCRIPT_DIR/verify-event-completeness.sh" > /tmp/chaos-verify.out 2>&1
    VC_RC=$?
    grep -aE "TYPE|B2AGG->|CLAIM->|GER->|VERDICT|SANITY" /tmp/chaos-verify.out | tail -8
fi
LOCKS=$(docker logs "${PROJECT}-miden-agglayer-1" 2>&1 | grep -c "database is locked" || true)

say "======================================================================"
say "  CHAOS SOAK RESULT"
say "    N=$N  faults=$FAULTS_DONE  loadtest_rc=$LT_RC  verify_rc=$VC_RC  store_locks=$LOCKS"
if [ "$VC_RC" = "0" ] && [ "${LOCKS:-1}" = "0" ]; then
    say "  >>> CHAOS SOAK PASS — every event survived the fault storm, 0 store-locks <<<"
    say "======================================================================"
    exit 0
else
    say "  >>> CHAOS SOAK NOT-GREEN (verify_rc=$VC_RC locks=$LOCKS) — inspect /tmp/chaos-verify.out + $CHAOS_LOG <<<"
    say "======================================================================"
    exit 1
fi
