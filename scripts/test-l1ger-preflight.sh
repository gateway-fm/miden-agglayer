#!/usr/bin/env bash
# Unit tests for `_pf_l1ger_consistency` (scripts/lib-l2l2.sh).
#
# This preflight decides whether bridge-service can serve /merkle-proof for
# net-1 deposits, and it has been wrong in BOTH directions during review:
#   * it reported PASS when its query failed ("not readable yet — vacuous");
#   * a "newest N are still settling" grace swallowed the whole population when
#     there were <= N rows, so it examined nothing and passed;
#   * a later "population grew, so the standard check applies next time" branch
#     passed while EVERY row was unmatched — and there is no guaranteed next
#     preflight.
# Each of those was a harness false green found only by reading the code, so
# the predicate now has tests that drive it through mocked probe results.
set -uo pipefail
RED=''; GREEN=''; YELLOW=''; CYAN=''; NC=''
_PF_FAILS=0
_pf_pass() { echo "  PASS $*"; }
_pf_fail() { echo "  FAIL $*"; _PF_FAILS=$((_PF_FAILS + 1)); }
COMPOSE_PROJECT_NAME=mock
PF_GER_SETTLE_SECS=10
source <(sed -n '/^_pf_l1ger_consistency() {/,/^}/p' $(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib-l2l2.sh)

sleep() { :; }
timeout() { shift; "$@"; }
# The probe is a nested function that shells out to docker, so mock docker:
# the ranked probe carries "newest_rank"; the bootstrap query does not.
docker() {
  local sql="${*}"
  if [[ "$sql" == *newest_rank* ]]; then
    # File-backed counter: the probe runs inside $( ), so a shell variable
    # would never advance in the parent.
    local i; i=$(cat ${TMPDIR:-/tmp}/pf-l1ger-idx.$$ 2>/dev/null || echo 0)
    local last=$(( ${#MOCK[@]} - 1 ))
    local pick=$(( i < last ? i : last ))
    printf '%s' "${MOCK[$pick]}"
    echo $((i + 1)) > ${TMPDIR:-/tmp}/pf-l1ger-idx.$$
  else
    printf '%s' "${MOCK_L1:-5}"
  fi
}

RESULT=0
# expect_case <expected: pass|fail> <name> <probe tuples...>
expect_case() { local want="$1" name="$2"; shift 2; MOCK=("$@")
  echo 0 > "${TMPDIR:-/tmp}/pf-l1ger-idx.$$"; _PF_FAILS=0
  # Run in THIS shell, not `$( )`. _PF_FAILS is incremented by _pf_fail, and a
  # command substitution runs in a subshell where that increment is discarded —
  # the same trap that made `psql_num`'s `exit 1` unable to stop its caller, and
  # that made an earlier version of this very test report every case as passing.
  local out="${TMPDIR:-/tmp}/pf-l1ger-out.$$"
  _pf_l1ger_consistency mockpg > "$out" 2>&1
  local got=pass; [[ "$_PF_FAILS" -gt 0 ]] && got=fail
  if [[ "$got" == "$want" ]]; then
    echo "PASS  $name (expected $want)"
  else
    echo "FAIL  $name — expected $want, got $got"
    sed 's/^/        /' "$out"
    RESULT=1
  fi
}

expect_case fail "growth with everything unmatched (the round-6 mock: 1|0|1 -> 3|1|3)" "1|0|1" "3|1|3"
expect_case pass "quiet consistent single row" "1|0|0"
expect_case fail "stuck row inside grace, nothing new arriving" "2|0|1" "2|0|1"
expect_case pass "newest row pending while the population grows" "3|0|1" "4|0|1"
expect_case fail "settled row unmatched (real inconsistency)" "9|3|3"
MOCK_L1=7 expect_case pass "bootstrap: no net-1 rows, L1 side indexing" "0|0|0"
MOCK_L1=0 expect_case fail "dead indexer: both networks empty" "0|0|0"

rm -f "${TMPDIR:-/tmp}/pf-l1ger-idx.$$" "${TMPDIR:-/tmp}/pf-l1ger-out.$$"
echo "──────────────────────────────────────────────"
if [[ "$RESULT" == "0" ]]; then
    echo "L1-GER PREFLIGHT TESTS: ALL PASS"
else
    echo "L1-GER PREFLIGHT TESTS: FAILURES"
fi
exit "$RESULT"
