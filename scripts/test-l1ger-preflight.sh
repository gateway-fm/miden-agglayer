#!/usr/bin/env bash
# Unit tests for `_pf_l1ger_consistency` (scripts/lib-l2l2.sh).
#
# This preflight decides whether bridge-service can serve /merkle-proof for
# net-1 deposits. Review found it wrong in both directions, repeatedly:
#   * PASS when its own query failed;
#   * a "newest N still settling" grace that swallowed the whole population;
#   * a "population grew" branch that passed while EVERY row was unmatched;
#   * `timeout <bash function>` (exit 127 in production) — hidden because THIS
#     TEST defined a `timeout` shell function;
#   * an id-based identity rule defeated by reorg delete-and-replay;
#   * bare failing assignments that abort under the real entrypoints' `set -e`
#     before any typed failure line is printed.
#
# Hence two deliberate choices here:
#   1. the mock is a fake `docker` EXECUTABLE on PATH, so the real `timeout` and
#      the real command composition run — only the data is synthetic;
#   2. the suite runs itself a SECOND time under `set -e` (errexit), because the
#      production entrypoints do, and the earlier bugs were invisible without it.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RED=''; GREEN=''; YELLOW=''; CYAN=''; NC=''
_PF_FAILS=0
_pf_pass() { echo "  PASS $*"; }
_pf_fail() { echo "  FAIL $*"; _PF_FAILS=$((_PF_FAILS + 1)); }
COMPOSE_PROJECT_NAME=mock
PF_GER_SETTLE_SECS=10

source <(sed -n '/^_pf_ger_probe_sql() {/,/^}/p;/^_pf_l1ger_consistency() {/,/^}/p' "$HERE/lib-l2l2.sh")

MOCKDIR="$(mktemp -d)"
trap 'rm -rf "$MOCKDIR"' EXIT
cat > "$MOCKDIR/docker" <<'MOCK'
#!/usr/bin/env bash
# Fake `docker`. MOCK_SEQ holds ';'-separated probe results; MOCK_RC forces a
# non-zero exit (docker/timeout failure); the bootstrap count query is the one
# without "servable".
sql="$*"
if [[ -n "${MOCK_RC:-}" && "${MOCK_RC}" != "0" ]]; then
    echo "Error: No such container: mockpg" >&2; exit "$MOCK_RC"
fi
if [[ "$sql" == *servable* ]]; then
    i=$(cat "$MOCK_IDX_FILE" 2>/dev/null || echo 0)
    IFS=';' read -r -a seq <<< "$MOCK_SEQ"
    last=$(( ${#seq[@]} - 1 )); pick=$(( i < last ? i : last ))
    printf '%s' "${seq[$pick]}"; echo $((i + 1)) > "$MOCK_IDX_FILE"
else
    printf '%s' "${MOCK_L1:-5}"
fi
MOCK
chmod +x "$MOCKDIR/docker"
PATH="$MOCKDIR:$PATH"
export MOCK_IDX_FILE="$MOCKDIR/idx"
sleep() { :; }

RESULT=0
expect_case() {
    local want="$1" name="$2"; shift 2
    local joined; printf -v joined '%s;' "$@"; export MOCK_SEQ="${joined%;}"
    echo 0 > "$MOCK_IDX_FILE"; _PF_FAILS=0
    # Run in THIS shell: a command substitution would discard _PF_FAILS.
    local out="$MOCKDIR/out"
    _pf_l1ger_consistency mockpg > "$out" 2>&1
    local got=pass; [[ "$_PF_FAILS" -gt 0 ]] && got=fail
    if [[ "$got" == "$want" ]]; then echo "PASS  $name (expected $want)"
    else echo "FAIL  $name — expected $want, got $got"; sed 's/^/        /' "$out"; RESULT=1; fi
}

expect_case pass "all servable GERs matched"                       "3|0"
expect_case fail "a servable GER has no servable L1 row"           "9|3"
expect_case fail "still unmatched after the settle window"         "2|1" "2|1"
expect_case pass "unmatched resolves during settling"              "3|1" "3|0"
MOCK_L1=7 expect_case pass "bootstrap: nothing servable yet, L1 indexing" "0|0"
MOCK_L1=0 expect_case fail "dead indexer: nothing servable anywhere"      "0|0"
expect_case fail "malformed probe response"                        "not-a-tuple"
MOCK_L1=oops expect_case fail "malformed bootstrap response"       "0|0"
MOCK_RC=1 expect_case fail "docker/psql failure is fail-closed"    "3|0"
MOCK_RC=124 expect_case fail "timeout (rc 124) is fail-closed"     "3|0"
unset MOCK_RC

echo "──────────────────────────────────────────────"
if [[ "$RESULT" == "0" ]]; then echo "L1-GER PREFLIGHT TESTS: ALL PASS"; else echo "L1-GER PREFLIGHT TESTS: FAILURES"; fi

# ERREXIT PASS. Production runs `set -euo pipefail`; a bare failing assignment
# inside the predicate would abort there while passing here. Re-exec the whole
# suite under -e and require the same result.
if [[ "${_PF_ERREXIT_PASS:-0}" != "1" ]]; then
    echo ""
    echo "── re-running the whole suite under errexit (production shell options) ──"
    if _PF_ERREXIT_PASS=1 bash -e "$0"; then
        echo "ERREXIT RE-RUN: ALL PASS"
    else
        echo "ERREXIT RE-RUN: FAILED — the predicate aborts under production shell options"
        RESULT=1
    fi
fi
exit "$RESULT"
