#!/usr/bin/env bash
# Unit tests for `_pf_l1ger_consistency` (scripts/lib-l2l2.sh).
#
# This preflight decides whether bridge-service can serve /merkle-proof for
# net-1 deposits, and review found it wrong in BOTH directions, repeatedly:
#   * PASS when its own query failed ("not readable yet — vacuous");
#   * a "newest N are still settling" grace that swallowed the whole population
#     when there were <= N rows, so it examined nothing and passed;
#   * a "population grew, the standard check applies next time" branch that
#     passed while EVERY row was unmatched — and there is no guaranteed next
#     preflight;
#   * `timeout <bash function>`, which exits 127 in production because timeout
#     needs an EXECUTABLE. An earlier version of THIS TEST hid that by defining
#     a `timeout` shell function, so the suite was green while every real
#     preflight failed.
#
# Because of that last one, the mock is a fake `docker` EXECUTABLE placed on
# PATH — the real `timeout` runs, the real command composition runs, and only
# the data is synthetic.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RED=''; GREEN=''; YELLOW=''; CYAN=''; NC=''
_PF_FAILS=0
_pf_pass() { echo "  PASS $*"; }
_pf_fail() { echo "  FAIL $*"; _PF_FAILS=$((_PF_FAILS + 1)); }
COMPOSE_PROJECT_NAME=mock
PF_GER_SETTLE_SECS=10

# Pull in ONLY the two functions under test, from the real file.
source <(sed -n '/^_pf_ger_probe_sql() {/,/^}/p;/^_pf_l1ger_consistency() {/,/^}/p' "$HERE/lib-l2l2.sh")

MOCKDIR="$(mktemp -d)"
trap 'rm -rf "$MOCKDIR"' EXIT
cat > "$MOCKDIR/docker" <<'MOCK'
#!/usr/bin/env bash
# Fake `docker`: serves the scripted probe tuples. The ranked probe carries
# "newest_rank"; the bootstrap count query does not.
sql="$*"
idx_file="$MOCK_IDX_FILE"
if [[ "$sql" == *newest_rank* ]]; then
    i=$(cat "$idx_file" 2>/dev/null || echo 0)
    IFS=';' read -r -a seq <<< "$MOCK_SEQ"
    last=$(( ${#seq[@]} - 1 ))
    pick=$(( i < last ? i : last ))
    printf '%s' "${seq[$pick]}"
    echo $((i + 1)) > "$idx_file"
else
    printf '%s' "${MOCK_L1:-5}"
fi
MOCK
chmod +x "$MOCKDIR/docker"
PATH="$MOCKDIR:$PATH"
export MOCK_IDX_FILE="$MOCKDIR/idx"

# Speed: the settle loop sleeps between probes.
sleep() { :; }

RESULT=0
# expect_case <pass|fail> <name> <probe tuples: pop|settled|total|maxid|minunmatched ...>
expect_case() {
    local want="$1" name="$2"; shift 2
    local joined; printf -v joined '%s;' "$@"; export MOCK_SEQ="${joined%;}"
    echo 0 > "$MOCK_IDX_FILE"
    _PF_FAILS=0
    # Run in THIS shell: _PF_FAILS is incremented by _pf_fail, and a command
    # substitution would discard it in a subshell — the same trap that made
    # psql_num's `exit 1` unable to stop its caller, and that made an earlier
    # version of this test report every case as passing.
    local out="$MOCKDIR/out"
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

# ── the probe must actually RUN (timeout + docker composition) ──────────────
expect_case pass "healthy: every GER matched"                     "3|0|0|30|0"
expect_case fail "settled row unmatched (real inconsistency)"     "9|3|3|90|10"
expect_case fail "stuck row inside grace, nothing new arriving"   "2|0|1|20|20" "2|0|1|20|20"
# The round-6 mock: population grows while everything stays unmatched.
expect_case fail "growth with everything unmatched"               "1|0|1|10|10" "3|1|3|30|10"
# The round-7 ambiguity: an OLD unmatched row drifting into the grace window
# behind newer matched rows must NOT pass.
expect_case fail "old unmatched row hides behind newer matched rows" "3|0|1|30|10" "4|0|1|40|10"
# Genuinely new-and-pending: the unmatched row arrived AFTER we started.
expect_case pass "only rows that arrived during settling are pending" "3|0|0|30|0" "4|0|1|40|40"
MOCK_L1=7 expect_case pass "bootstrap: no net-1 rows, L1 side indexing" "0|0|0|0|0"
MOCK_L1=0 expect_case fail "dead indexer: both networks empty"          "0|0|0|0|0"
expect_case fail "malformed probe response"                       "not-a-tuple"

echo "──────────────────────────────────────────────"
if [[ "$RESULT" == "0" ]]; then echo "L1-GER PREFLIGHT TESTS: ALL PASS"; else echo "L1-GER PREFLIGHT TESTS: FAILURES"; fi
exit "$RESULT"
