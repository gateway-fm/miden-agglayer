#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-settlement-bisect.sh — WHY does `make test-e2e` stall at "certificate
# settled" while the same bridge-out passes standalone?
#
#   [12:32:48] PASS: BridgeEvent detected in L2
#   [12:47:45] FAIL: Timed out: certificate settled          (900s)
#
# 2 of 3 hardened battery iterations. The aggkit dump says aggsender never BUILT
# a certificate ("no certificates in local storage and agglayer: initial
# state"), while agglayer itself was healthy.
#
# THE SEARCH SPACE IS TWO SCRIPTS. In `e2e-test.sh all`, e2e-l2-to-l1.sh is the
# FOURTH entry; every proxy-restart / reset-miden-store / restore scenario runs
# AFTER it. So the only difference between the suite's failing tier and the
# standalone `make e2e-l2-to-l1` that passes every time is these two, which run
# before it and do not run in the standalone target:
#
#   e2e-rpc-tip-consistency.sh    tip coherence probing
#   e2e-future-nonce-mempool.sh   #146 — parks and promotes out-of-order txns
#
# Each ARM gets a FRESH stack and runs its prefix, then the bridge-out. After
# every prefix script the aggkit L2 bridgesync is probed, so a break is located
# at the script that caused it rather than inferred from the final verdict.
#
#   RESULTS_DIR=e2e-results/167-<ts> ./scripts/e2e-settlement-bisect.sh [arms]
#   arms default: D B C A   (D = reproduce, A = control)
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_DIR="$PWD/scripts"

R="${RESULTS_DIR:?set RESULTS_DIR}"; mkdir -p "$R/logs"
OUT="$R/settlement-bisect.log"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)
PROXY=miden-agglayer-miden-agglayer-1
AGGKIT=miden-agglayer-aggkit-1
PG=miden-agglayer-agglayer-postgres-1

say() { printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$OUT"; }

# Everything aggkit's L2 bridgesync can tell us about whether it is still
# ingesting the synthetic chain. A halted bridgesync is the shape that would
# leave aggsender with nothing to certify while agglayer stays healthy.
probe() { # $1 = label
    {
        echo "── probe: $1 ($(date -u +%FT%TZ)) ──"
        echo "proxy eth_blockNumber : $(curl -s -m5 -X POST http://localhost:8546 \
            -H 'content-type: application/json' \
            -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' 2>/dev/null \
            | sed -E 's/.*"result":"([^"]*)".*/\1/')"
        echo "proxy cursors         : $(docker exec "$PG" psql -U agglayer -d agglayer_store -tAc \
            "SELECT 'projector='||projector_cursor||' reconcile='||reconcile_cursor||' tip='||latest_block_number FROM service_state WHERE id=1" 2>&1 | tr -d '\n')"
        echo "aggkit last indexed   : $(docker logs --tail 20000 "$AGGKIT" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
            | grep -aoE 'block [0-9]+ processed' | grep -aoE '[0-9]+' | sort -n | tail -1)"
        echo "aggkit restarts       : $(docker inspect -f '{{.RestartCount}}' "$AGGKIT" 2>/dev/null)"
        echo "aggkit halt/reorg/errors (last 15):"
        { docker logs --tail 20000 "$AGGKIT" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
            | grep -aiE 'inconsistent|reorg|halt|ERROR|panic' | tail -15 | sed 's/^/  /'; } || true
        echo
    } >> "$OUT" 2>&1
}

run_arm() { # $1 = arm id, $2.. = prefix scripts (bridge-out is always last)
    local arm="$1"; shift
    say "═══ ARM $arm: prefix = ${*:-<none>} ═══"
    "${BASE_ENV[@]}" make e2e-down >>"$R/logs/bisect-down.log" 2>&1
    if ! "${BASE_ENV[@]}" make e2e-up >>"$R/logs/bisect-$arm-up.log" 2>&1; then
        say "ARM $arm: STACK FAILED TO COME UP — arm void"; return 2
    fi
    probe "$arm/after-stack-up"
    local sc
    for sc in "$@"; do
        say "  running $sc"
        if ! "$SCRIPT_DIR/$sc" > "$R/logs/bisect-$arm-$sc.log" 2>&1; then
            say "  ARM $arm: prefix script $sc FAILED (see $R/logs/bisect-$arm-$sc.log)"
        fi
        probe "$arm/after-$sc"
    done
    say "  running e2e-l1-to-l2.sh"
    "$SCRIPT_DIR/e2e-l1-to-l2.sh" > "$R/logs/bisect-$arm-l1-to-l2.log" 2>&1
    probe "$arm/after-e2e-l1-to-l2.sh"
    say "  running e2e-l2-to-l1.sh (the settlement under test)"
    local rc=0
    "$SCRIPT_DIR/e2e-l2-to-l1.sh" > "$R/logs/bisect-$arm-l2-to-l1.log" 2>&1 || rc=$?
    probe "$arm/after-e2e-l2-to-l1.sh"
    if (( rc == 0 )); then say "ARM $arm: SETTLED (rc=0) — prefix does NOT break certification"
    else say "ARM $arm: FAILED rc=$rc — see $R/logs/bisect-$arm-l2-to-l1.log"; fi
    return $rc
}

declare -A ARM_PREFIX=(
  [A]=""                                              # control: standalone shape
  [B]="e2e-rpc-tip-consistency.sh"
  [C]="e2e-future-nonce-mempool.sh"
  [D]="e2e-rpc-tip-consistency.sh e2e-future-nonce-mempool.sh"   # full suite prefix
)
for arm in "${@:-D B C A}"; do
    # shellcheck disable=SC2086
    run_arm "$arm" ${ARM_PREFIX[$arm]}
done
say "BISECT COMPLETE"
