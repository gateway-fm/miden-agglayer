#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# lib-stack-health.sh — classify the e2e stack so the harness never reports a
# broken box as a wall of test failures.
#
# Two incidents this session motivated it, both of which reported as innocent
# per-target failures:
#   1. INFRA  — a stale/half-reset box left the proxy container `unhealthy` and
#      L2B crash-looping; every target failed on `up --wait`, matrix showed 0/21.
#   2. WEDGED — the projector fail-closed on a leaf and the synthetic tip froze;
#      40+ later targets each burned their full timeout blaming themselves.
#
# `stack_health` prints ONE verdict and returns a matching code:
#   HEALTHY           rc 0   — safe to run a target
#   INFRA:<reason>    rc 1   — the stack cannot serve (bring-up/container/RPC);
#                             a nuclear reset + rebuild is the fix, not a re-test
#   WEDGED:<reason>   rc 2   — up but the synthetic pipeline is stuck; no target
#                             will pass until it's recovered (projector halt / frozen tip)
#
# Pure read-only + best-effort: it must NEVER error the caller. Env:
#   L2_RPC (default http://localhost:8546), WITH_WEB3SIGNER (checks that overlay too).
# ══════════════════════════════════════════════════════════════════════════════

# The proxy container name ends `-miden-agglayer-1`; everything else shares its
# compose-project prefix. Returns "" when nothing is up (itself an INFRA signal).
_sh_proxy() { docker ps -a --format '{{.Names}}' 2>/dev/null | grep -E -- '-miden-agglayer-1$' | head -1; }

# Core services that MUST be Up for the base stack to serve. (Bootstrap
# `node-bootstrap-*` containers exit 0 by design and are excluded.)
_SH_CORE_SUFFIXES=(anvil postgres agglayer-postgres miden-node ntx-builder tx-prover
                   miden-agglayer agglayer aggkit bridge-service)

stack_health() {
    local rpc="${L2_RPC:-http://localhost:8546}"
    local proxy prefix
    proxy="$(_sh_proxy)"
    if [[ -z "$proxy" ]]; then echo "INFRA:no-proxy-container-running"; return 1; fi
    prefix="${proxy%-miden-agglayer-1}"

    # 1. Any core container Exited/Dead/Restarting is a hard infra failure — the
    #    crash-loop (e.g. a stateless L2B anvil) that made `up --wait` time out.
    local s name
    for suf in "${_SH_CORE_SUFFIXES[@]}"; do
        name="${prefix}-${suf}-1"
        s="$(docker inspect -f '{{.State.Status}}' "$name" 2>/dev/null)" || continue
        case "$s" in
            running) : ;;
            restarting) echo "INFRA:${suf}-restarting"; return 1 ;;
            exited|dead) echo "INFRA:${suf}-${s}"; return 1 ;;
            *) : ;;
        esac
    done
    if [[ -n "${WITH_WEB3SIGNER:-}" ]]; then
        name="${prefix}-web3signer-1"
        s="$(docker inspect -f '{{.State.Status}}' "$name" 2>/dev/null)"
        [[ -n "$s" && "$s" != running ]] && { echo "INFRA:web3signer-${s}"; return 1; }
    fi

    # 2. The proxy's own health gate — the exact "(health: starting)"/unhealthy
    #    that failed the box this session.
    local h
    h="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$proxy" 2>/dev/null)"
    case "$h" in
        unhealthy|starting) echo "INFRA:proxy-health-${h}"; return 1 ;;
    esac

    # 3. RPC must actually answer.
    local tip1
    tip1="$(_sh_tip "$rpc")"
    [[ -z "$tip1" ]] && { echo "INFRA:proxy-rpc-unresponsive"; return 1; }

    # 4. WEDGED: the projector fail-closed (emitted every sync tick while halted).
    local halt
    halt="$(docker logs --tail 200 "$proxy" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
        | grep -aoiE 'projector halted \(fail-closed\):[^"]*' | tail -1 | cut -c1-180)"
    [[ -n "$halt" ]] && { echo "WEDGED:${halt}"; return 2; }

    echo "HEALTHY"
    return 0
}

_sh_tip() { # $1 = rpc url
    curl -sf --max-time 6 -X POST -H 'content-type:application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        "$1" 2>/dev/null | grep -oE '0x[0-9a-fA-F]+' | head -1
}

# Full teardown so a run never inherits a corrupted box (stale node_data, a
# stateless L2B anvil, a wedged chain). `make e2e-clean-data` does NOT recreate
# already-running node containers, which is how a broken box failed 21 targets.
stack_nuclear_reset() {
    local ids
    ids="$(docker ps -aq --filter name=miden-agglayer 2>/dev/null)"
    [[ -n "$ids" ]] && docker rm -f $ids >/dev/null 2>&1
    docker volume ls -q 2>/dev/null | grep -i miden-agglayer | xargs -r docker volume rm >/dev/null 2>&1
    return 0
}
