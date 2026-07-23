#!/usr/bin/env bash
# #156 CHAOS e2e — a real L1->L2 deposit/claim survives BOTH failure modes the
# recovery loop must heal, with AT MOST a proxy restart and no client rebroadcast:
#
#   (a) the miden-node is UNAVAILABLE while the proxy is submitting/proving a tx
#       (GER injection and/or the claim) — the external submit errors; and
#   (b) the PROXY PROCESS CRASHES (SIGKILL) mid-flight, dropping its in-memory
#       writer queue.
#
# Faults are injected with `docker kill` (no in-proxy fault hook). After the chaos
# quiesces, startup + periodic recovery must drive every orphaned acknowledged
# transaction to a durable outcome so the deposit is CLAIMED EXACTLY ONCE — proven
# by the wrapped l1-to-l2 test's before/after balance delta (a double-claim would
# over-credit; a lost claim would never credit) — and no pending/unlinked
# transaction may remain.
#
# Requires a running stack. Env: COMPOSE_PROJECT_NAME, L2_RPC.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
NODE="${MIDEN_NODE_CONTAINER:-${PROJECT}-miden-node-1}"
PROXY="${AGGLAYER_CONTAINER:-${PROJECT}-miden-agglayer-1}"
PG="${AGGLAYER_PG_CONTAINER:-${PROJECT}-agglayer-postgres-1}"

log()  { echo "[chaos] $*"; }
pass() { echo "[chaos] PASS: $*"; }
fail() { echo "[chaos] FAIL: $*"; exit 1; }

pgq() { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }
metric() { curl -fsS "$L2_RPC/metrics" 2>/dev/null | grep -E "^$1[[:space:]]" | awk '{print $2}' | tail -1; }

for c in "$NODE" "$PROXY" "$PG"; do
    docker inspect "$c" >/dev/null 2>&1 || fail "container $c not found — is the stack up?"
done

SUCC_BEFORE="$(metric orphan_recovery_successes_total)"; SUCC_BEFORE="${SUCC_BEFORE%.*}"; SUCC_BEFORE="${SUCC_BEFORE:-0}"

# ── Chaos injector: node-unavailable + proxy-crash rounds, then quiesce ─────────
chaos_injector() {
    for r in 1 2 3; do
        log "chaos round $r: miden-node UNAVAILABLE"
        docker kill "$NODE" >/dev/null 2>&1 || true
        sleep 10                                   # writer submits fail while node is down
        docker start "$NODE" >/dev/null 2>&1 || true
        sleep 12
        log "chaos round $r: PROXY CRASH (SIGKILL)"
        docker kill "$PROXY" >/dev/null 2>&1 || true
        sleep 5                                    # in-memory writer queue is lost
        docker start "$PROXY" >/dev/null 2>&1 || true   # <-- the ONLY remedy: restart the proxy
        sleep 22                                   # startup recovery runs on boot
    done
    log "chaos complete; ensuring node + proxy are up"
    docker start "$NODE" "$PROXY" >/dev/null 2>&1 || true
}

# ── Run the real deposit/claim flow; inject chaos AFTER setup (provision+deposit
#    happen on a stable stack), during the claim-processing window. ──────────────
log "starting the wrapped l1-to-l2 deposit/claim under chaos"
L1L2_LOG="$(mktemp /tmp/chaos-l1l2.XXXXXX.log)"
(
    # Generous polling so the flow outlasts the chaos + recovery windows: the chaos
    # rounds (~130s) can orphan the covering GER as a PREPARED-but-unconfirmed note,
    # which recovery only re-drives once its inclusion window expires past the
    # authoritative reconcile cursor (~submission_note_expiration_delta blocks) — then
    # the fresh GER injects, the deposit becomes claimable, and the claim lands. Allow
    # 15 min end-to-end so "healed by at most a proxy restart" is what we measure, not
    # an arbitrary deadline shorter than the (bounded) self-heal latency.
    RECV_POLL_TRIES=90 RECV_POLL_INTERVAL=10 \
    env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l1-to-l2.sh"
) > "$L1L2_LOG" 2>&1 &
L1L2_PID=$!

sleep 45                    # let provision + the L1 deposit land on a stable stack
chaos_injector             # inject node-unavailable + proxy-crash during the claim window

# Wait for the deposit/claim flow to finish (it self-heals after chaos quiesces).
if wait "$L1L2_PID"; then
    L1L2_RC=0
else
    L1L2_RC=$?
fi

echo "----- wrapped l1-to-l2 output (tail) -----"
sed -E 's/\x1b\[[0-9;]*m//g' "$L1L2_LOG" | tail -25
echo "------------------------------------------"

[ "$L1L2_RC" -eq 0 ] \
    || fail "the deposit did NOT self-heal to an exact-once claim after node-unavailable + proxy-crash chaos (l1-to-l2 rc=$L1L2_RC). Recovery required more than a proxy restart."
pass "deposit CLAIMED EXACTLY ONCE despite miden-node-unavailable + proxy-crash chaos, healed by restarts alone (no client rebroadcast)"

# ── No acknowledged transaction may be left stranded ──────────────────────────
# Give the periodic recovery sweep a moment to drain any residual orphan.
ORPHANS=""
for _ in $(seq 1 30); do
    ORPHANS="$(pgq "
        SELECT count(*) FROM transactions t
        LEFT JOIN tx_note_links l ON l.tx_hash = t.tx_hash
        WHERE t.status = 'pending' AND l.note_id IS NULL AND t.miden_tx_id IS NULL")"
    [ "${ORPHANS:-1}" = "0" ] && break
    sleep 10
done
[ "${ORPHANS:-1}" = "0" ] \
    || fail "$ORPHANS acknowledged pending/unlinked transaction(s) remain unrecovered after the chaos"
pass "no pending/unlinked transactions remain — every acknowledged tx reached a durable outcome"

SUCC_AFTER="$(metric orphan_recovery_successes_total)"; SUCC_AFTER="${SUCC_AFTER%.*}"; SUCC_AFTER="${SUCC_AFTER:-0}"
log "orphan_recovery_successes_total: $SUCC_BEFORE -> $SUCC_AFTER"
[ "$SUCC_AFTER" -gt "$SUCC_BEFORE" ] \
    || log "note: recovery-success counter did not advance (the writer's own retry may have absorbed the faults); orphan drain above is the authoritative proof"

# ── Post-chaos liveness: the WHOLE pipeline must still work in BOTH directions ──
# Recovering the chaos'd transaction is not enough — prove the bridge is fully
# healthy afterwards by running fresh deposits (L1->L2) AND withdrawals (L2->L1),
# twice each way. A wedged consumer or a stuck nonce would fail these.
log "post-chaos liveness: 2x deposits (L1->L2) + 2x withdrawals (L2->L1)"
for i in 1 2; do
    DEP_LOG="$(mktemp /tmp/postchaos-dep.XXXXXX.log)"
    if env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l1-to-l2.sh" > "$DEP_LOG" 2>&1; then
        pass "post-chaos deposit #$i (L1->L2) succeeded"
    else
        echo "----- deposit #$i output (tail) -----"; sed -E 's/\x1b\[[0-9;]*m//g' "$DEP_LOG" | tail -15
        fail "post-chaos L1->L2 deposit #$i FAILED — the bridge did not fully recover after the chaos"
    fi
done
for i in 1 2; do
    WD_LOG="$(mktemp /tmp/postchaos-wd.XXXXXX.log)"
    if env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l2-to-l1.sh" > "$WD_LOG" 2>&1; then
        pass "post-chaos withdrawal #$i (L2->L1) succeeded"
    else
        echo "----- withdrawal #$i output (tail) -----"; sed -E 's/\x1b\[[0-9;]*m//g' "$WD_LOG" | tail -15
        fail "post-chaos L2->L1 withdrawal #$i FAILED — the bridge did not fully recover after the chaos"
    fi
done
pass "post-chaos liveness confirmed: deposits + withdrawals both work (2x each way) after node-unavailable + proxy-crash chaos"

pass "#156 chaos e2e: node-unavailable + proxy-crash both self-healed with at most a proxy restart, exactly once; bridge fully live both ways afterward"
