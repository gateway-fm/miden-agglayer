#!/usr/bin/env bash
# Unit test for lib-stack-health.sh's stack_health classifier — mocks docker/curl
# so it runs with no stack. Locks in the two classifier bugs the box gate exposed:
#   • "no proxy yet" (fresh run, pre-bring-up) must be HEALTHY, not INFRA.
#   • an L2B overlay container `unhealthy` (anvil-l2b after hours) must be INFRA.
# Run: ./scripts/test-lib-stack-health.sh   (exits non-zero on any failure)
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
source scripts/lib-stack-health.sh

FAILS=0
check() { # $1 desc  $2 expected-verdict  $3 expected-rc  $4 actual-verdict  $5 actual-rc
  if [[ "$4" == "$2" && "$5" == "$3" ]]; then echo "PASS: $1"; else
    echo "FAIL: $1 — expected [$2] rc=$3, got [$4] rc=$5"; FAILS=$((FAILS+1)); fi
}
_sh_tip() { echo "0x100"; }  # proxy RPC responds in every scenario below

# PROXY_PRESENT=0 -> no proxy container at all (pre-bring-up).
# ANVIL_L2B: absent|running-healthy|running-unhealthy ; CORE_STATE for base cores.
docker() {
  if [[ "$1" == ps ]]; then
    [[ "${PROXY_PRESENT:-1}" == 1 ]] && echo "miden-agglayer-miden-agglayer-1"; return 0; fi
  if [[ "$1" == inspect ]]; then
    local fmt="$3" name="$4"
    case "$fmt" in
      *State.Status*)
        case "$name" in
          *-anvil-l2b-1) [[ "${ANVIL_L2B:-absent}" == absent ]] && return 1 || echo running ;;
          *-postgres-l2b-1|*-bridge-service-l2b-1|*-aggkit-l2b-1) return 1 ;;
          *) echo "${CORE_STATE:-running}" ;;
        esac ;;
      *State.Health*)
        case "$name" in
          *-anvil-l2b-1) [[ "${ANVIL_L2B:-absent}" == running-unhealthy ]] && echo unhealthy || echo healthy ;;
          *-miden-agglayer-1) echo healthy ;;
          *) echo none ;;
        esac ;;
    esac
    return 0
  fi
  [[ "$1" == logs ]] && { echo ""; return 0; }
  return 0
}

PROXY_PRESENT=0;                         v=$(stack_health); r=$?; check "no proxy yet => HEALTHY:no-stack-yet" "HEALTHY:no-stack-yet" 0 "$v" "$r"
PROXY_PRESENT=1; ANVIL_L2B=absent;        v=$(stack_health); r=$?; check "base stack, no L2B => HEALTHY" "HEALTHY" 0 "$v" "$r"
PROXY_PRESENT=1; ANVIL_L2B=running-healthy;   v=$(stack_health); r=$?; check "L2B up & healthy => HEALTHY" "HEALTHY" 0 "$v" "$r"
PROXY_PRESENT=1; ANVIL_L2B=running-unhealthy; v=$(stack_health); r=$?; check "anvil-l2b unhealthy => INFRA" "INFRA:anvil-l2b-unhealthy" 1 "$v" "$r"
unset ANVIL_L2B; PROXY_PRESENT=1; CORE_STATE=exited; v=$(stack_health); r=$?; check "a base core exited => INFRA" "INFRA:anvil-exited" 1 "$v" "$r"

echo "----"; [[ $FAILS -eq 0 ]] && { echo "ALL PASS"; exit 0; } || { echo "$FAILS FAILED"; exit 1; }
