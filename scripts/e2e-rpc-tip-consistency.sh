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

first=""; last=""; fail=0
for i in $(seq 1 "$SAMPLES"); do
    bn=$(rpc eth_blockNumber '[]') || { echo "FAIL: eth_blockNumber unreachable"; exit 1; }
    lt=$(rpc eth_getBlockByNumber '["latest", false]') || { echo "FAIL: eth_getBlockByNumber(latest) unreachable"; exit 1; }
    diff=$(( bn > lt ? bn - lt : lt - bn ))
    echo "sample $i: eth_blockNumber=$bn latest.number=$lt diff=$diff"
    if [[ $diff -gt 2 ]]; then
        # QUICK FAIL: divergence is deterministic (a frozen mirror never
        # heals) — no point sampling further, fail the suite immediately.
        echo "FAIL: coherence — tip sources diverged by $diff blocks (quick fail)"
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
