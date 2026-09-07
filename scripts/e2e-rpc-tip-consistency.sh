#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# RPC tip-consistency regression (postmortem 2026-07-04).
#
# Guards against the stale-eth_blockNumber class: the RD-940 BlockMonitor
# mirror froze at its cold-boot seed after the projector redesign orphaned its
# only steady-state writer, so eth_blockNumber served 659 while the synthetic
# tip was 2702 (eth_getBlockByNumber("latest") stayed correct).
#
# Asserts, against a running stack:
#   1. COHERENCE: eth_blockNumber == eth_getBlockByNumber("latest").number
#      (±2 blocks sequential-sampling tolerance), on every sample.
#   2. LIVENESS: eth_blockNumber ADVANCES over the observation window
#      (a frozen tip passes coherence checks taken in isolation — the 2026-07-04
#      bug is only caught by watching it move with the chain).
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
L2_RPC="${L2_RPC:-http://localhost:8546}"
SAMPLES="${SAMPLES:-5}"
INTERVAL="${INTERVAL:-6}"

rpc() { curl -sf "$L2_RPC" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
        | python3 -c "import json,sys;r=json.load(sys.stdin);v=r.get('result');print(int(v if isinstance(v,str) else v['number'],16))"; }

# BRACKET THE SAMPLE, do not merely tolerate skew. The two tips are read by two
# SEQUENTIAL RPC calls, so any block produced between them shows up as a
# divergence that is pure measurement artefact. A flat "±2 blocks" allowance is
# calibrated for steady-state ~5s block time and is wrong exactly when the node
# is bursting — right after a bootstrap, catching up. Observed on a freshly
# wiped stack:
#
#   sample 1: eth_blockNumber=26 latest.number=26 diff=0
#   sample 2: eth_blockNumber=26 latest.number=26 diff=0
#   sample 3: eth_blockNumber=27 latest.number=31 diff=4
#   FAIL: coherence — tip sources diverged by 4 blocks (quick fail)
#
# Four blocks between two back-to-back curls is ~20 seconds of steady-state
# block time, so this was a burst, not incoherence.
#
# Reading eth_blockNumber BEFORE and AFTER the `latest` call brackets it: a
# coherent mirror must land inside [before-1, after+1], however fast blocks
# arrive, because `latest` was served at some instant between the two reads.
# This does NOT weaken the regression it guards — the 2026-07-04 frozen mirror
# served 659 against a synthetic tip of 2702 and would sit far outside the
# bracket, and the liveness check below still catches a tip that never moves.
first=""; last=""; fail=0
for i in $(seq 1 "$SAMPLES"); do
    bn0=$(rpc eth_blockNumber '[]') || { echo "FAIL: eth_blockNumber unreachable"; exit 1; }
    lt=$(rpc eth_getBlockByNumber '["latest", false]') || { echo "FAIL: eth_getBlockByNumber(latest) unreachable"; exit 1; }
    bn1=$(rpc eth_blockNumber '[]') || { echo "FAIL: eth_blockNumber unreachable"; exit 1; }
    lo=$(( bn0 - 1 )); hi=$(( bn1 + 1 ))
    bn=$bn1
    echo "sample $i: eth_blockNumber=$bn0..$bn1 latest.number=$lt (coherent window [$lo,$hi])"
    if [[ $lt -lt $lo || $lt -gt $hi ]]; then
        # QUICK FAIL: divergence is deterministic (a frozen mirror never
        # heals) — no point sampling further, fail the suite immediately.
        echo "FAIL: coherence — latest.number=$lt is OUTSIDE the eth_blockNumber bracket [$lo,$hi]; \
the two tip sources disagree by more than in-flight block production can explain (quick fail)"
        exit 1
    fi
    [[ -z "$first" ]] && first=$bn
    last=$bn
    [[ $i -lt $SAMPLES ]] && sleep "$INTERVAL"
done
if [[ $last -le $first ]]; then
    echo "FAIL: liveness — eth_blockNumber did not advance ($first -> $last over $(( (SAMPLES-1)*INTERVAL ))s); frozen-tip regression"
    fail=1
fi
[[ $fail -eq 0 ]] && echo "PASS: tip coherent and advancing ($first -> $last)"
exit $fail
