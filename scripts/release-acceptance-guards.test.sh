#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Behavioral tests for the provenance guards in scripts/release-acceptance.sh.
#
# Self-contained, no bats, no real docker/git/build. It SOURCES the acceptance
# script (whose `main` is gated behind a BASH_SOURCE==$0 check, so nothing runs on
# source) and then overrides `git` and `docker` with shell-function MOCKS. Because
# the guards call `git`/`docker` as bare commands, a shell function of the same
# name shadows the real binary at call time — so each guard runs against the mock
# with no PATH juggling.
#
# Each case runs a guard in a SUBSHELL: the guards call `fail` (which `exit 1`s),
# so a rejected case exits the subshell non-zero and an accepted case exits 0.
#
#   Usage:  bash scripts/release-acceptance-guards.test.sh
#   Exit 0 = all cases passed, non-zero = at least one case behaved wrongly.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=release-acceptance.sh
source "$HERE/release-acceptance.sh"

RC=0
pass() { printf '  ok      %s\n' "$1"; }
oops() { printf '  FAILED  %s\n' "$1"; RC=1; }

# Each case runs its body (per-case env overrides + a guard call) via `eval` in a
# SUBSHELL, so the sourced guard functions and the git/docker mocks are in scope
# while per-case variable overrides and any `exit` from `fail` stay contained.
# assert_reject: body must exit non-zero (guard called fail).
assert_reject() {
    local desc="$1" body="$2"
    if ( eval "$body" ) >/dev/null 2>&1; then oops "expected REJECT: $desc"; else pass "reject: $desc"; fi
}
# assert_accept: body must exit zero (guard passed).
assert_accept() {
    local desc="$1" body="$2"
    if ( eval "$body" ) >/dev/null 2>&1; then pass "accept: $desc"; else oops "expected ACCEPT: $desc"; fi
}

# ── Mocks ─────────────────────────────────────────────────────────────────────
# Behavior is driven by MOCK_* environment variables set per-case in a subshell.
#   HEAD_SHA / OTHER_SHA are valid 40-char hex so the SHA regex branch is real.
HEAD_SHA_FIX="0000000000000000000000000000000000000001"
OTHER_SHA_FIX="0000000000000000000000000000000000000002"

git() {
    case "$1" in
        status)
            [ "${MOCK_GIT_STATUS_FAIL:-0}" = 1 ] && return 1
            printf '%s' "${MOCK_GIT_STATUS:-}"
            return 0 ;;
        rev-parse)
            [ "${MOCK_REVPARSE_FAIL:-0}" = 1 ] && return 1
            local rev="${*: -1}"                     # last positional = the rev
            case "$rev" in
                HEAD)
                    [ "${MOCK_HEAD_FAIL:-0}" = 1 ] && return 1
                    printf '%s\n' "${MOCK_HEAD_SHA}"; return 0 ;;
                refs/tags/*)
                    if [ -n "${MOCK_TAG_SHA:-}" ]; then printf '%s\n' "$MOCK_TAG_SHA"; return 0
                    else return 1; fi ;;                # unknown tag → non-zero, no output
                *)                                     # "<sha>^{commit}"
                    printf '%s\n' "${MOCK_SHA_RESOLVE}"; return 0 ;;
            esac ;;
        cat-file)
            [ "${MOCK_SHA_EXISTS:-1}" = 1 ] && return 0 || return 1 ;;
        *) return 0 ;;
    esac
}

docker() {
    if [ "$1" = "inspect" ]; then
        [ "${MOCK_DOCKER_INSPECT_FAIL:-0}" = 1 ] && return 1
        printf '%s\n' "${MOCK_RUNNING_ID}"; return 0
    fi
    if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
        [ "${MOCK_IMG_INSPECT_FAIL:-0}" = 1 ] && return 1
        printf '%s\n' "${MOCK_BUILT_ID}"; return 0
    fi
    if [ "$1" = "tag" ]; then
        [ "${MOCK_DOCKER_TAG_FAIL:-0}" = 1 ] && return 1
        return 0
    fi
    return 0
}

# Happy-path baseline; each case exports overrides inside its own subshell.
export MOCK_GIT_STATUS=""
export MOCK_HEAD_SHA="$HEAD_SHA_FIX"
export MOCK_SHA_RESOLVE="$HEAD_SHA_FIX"
export MOCK_SHA_EXISTS=1
export MOCK_TAG_SHA=""
export MOCK_RUNNING_ID="sha256:abc"
export MOCK_BUILT_ID="sha256:abc"
export PROJECT="proj"
export RELEASE_IMAGE="miden-agglayer-e2e:vTEST"

echo "── verify_clean_worktree ──────────────────────────────────────────────"
assert_accept "clean tree"                 'MOCK_GIT_STATUS=""; verify_clean_worktree'
assert_reject "modified tracked file"      'MOCK_GIT_STATUS=" M src/main.rs"; verify_clean_worktree'
assert_reject "staged file"                'MOCK_GIT_STATUS="A  new.rs"; verify_clean_worktree'
assert_reject "untracked file"             'MOCK_GIT_STATUS="?? stray.txt"; verify_clean_worktree'
assert_reject "git status failure"         'MOCK_GIT_STATUS_FAIL=1; verify_clean_worktree'

echo "── verify_exact_ref ───────────────────────────────────────────────────"
assert_accept "full SHA == HEAD"           "RELEASE_REF=$HEAD_SHA_FIX; verify_exact_ref"
assert_accept "existing tag peels to HEAD" "RELEASE_REF=v0.15.5 MOCK_TAG_SHA=$HEAD_SHA_FIX; verify_exact_ref"
assert_reject "mutable ref: HEAD"          'RELEASE_REF=HEAD; verify_exact_ref'
assert_reject "mutable ref: branch name"   'RELEASE_REF=main; verify_exact_ref'
assert_reject "mutable ref: origin/main"   'RELEASE_REF=origin/main; verify_exact_ref'
assert_reject "revision expression HEAD~1" 'RELEASE_REF=HEAD~1; verify_exact_ref'
assert_reject "unknown tag"                'RELEASE_REF=v9.9.9 MOCK_TAG_SHA=""; verify_exact_ref'
assert_reject "full SHA, object missing"   "RELEASE_REF=$HEAD_SHA_FIX MOCK_SHA_EXISTS=0; verify_exact_ref"
assert_reject "full SHA resolves != HEAD"  "RELEASE_REF=$OTHER_SHA_FIX MOCK_SHA_RESOLVE=$OTHER_SHA_FIX; verify_exact_ref"
assert_reject "tag peels != HEAD"          "RELEASE_REF=v0.15.5 MOCK_TAG_SHA=$OTHER_SHA_FIX; verify_exact_ref"
assert_reject "git HEAD resolution failure" "RELEASE_REF=$HEAD_SHA_FIX MOCK_HEAD_FAIL=1; verify_exact_ref"

echo "── verify_running_image (verifies + echoes id; does NOT tag) ──────────"
assert_accept "running == built"           'MOCK_RUNNING_ID="sha256:x" MOCK_BUILT_ID="sha256:x"; verify_running_image'
assert_reject "docker inspect fails"       'MOCK_DOCKER_INSPECT_FAIL=1; verify_running_image'
assert_reject "running ID empty"           'MOCK_RUNNING_ID=""; verify_running_image'
assert_reject "docker image inspect fails" 'MOCK_IMG_INSPECT_FAIL=1; verify_running_image'
assert_reject "built ID empty"             'MOCK_BUILT_ID=""; verify_running_image'
assert_reject "running != built mismatch"  'MOCK_RUNNING_ID="sha256:a" MOCK_BUILT_ID="sha256:b"; verify_running_image'
# verify_running_image must echo the verified id on success (caller captures it).
assert_accept "echoes verified id"         '[ "$(MOCK_RUNNING_ID=sha256:zz MOCK_BUILT_ID=sha256:zz verify_running_image)" = "sha256:zz" ]'

echo "── tag_release_image (only after every gate passes) ───────────────────"
assert_accept "tags accepted build"        'RUNNING_ID="sha256:x"; tag_release_image'
assert_reject "docker tag fails"           'RUNNING_ID="sha256:x" MOCK_DOCKER_TAG_FAIL=1; tag_release_image'
assert_reject "no verified id to tag"      'RUNNING_ID=""; tag_release_image'

echo "── verify_monitor_evidence (positive evidence + no fail-open) ─────────"
TDIR="$(mktemp -d)"; trap 'rm -rf "$TDIR"' EXIT
printf '%s\n' '[12:00:00] completeness watcher up: project=proj' 'OK notes=5 logs=5 cut=40' > "$TDIR/watch.ok"
printf '%s\n' '[12:00:00] completeness watcher up: project=proj' > "$TDIR/watch.nocycle"
printf '%s\n' 'OK notes=5 logs=5 cut=40' > "$TDIR/watch.nostart"
printf '%s\n' '[12:00:00] completeness watcher up: project=proj' 'OK notes=5 logs=5 cut=40' \
       '[12:01:00] ████ COMPLETENESS VIOLATION DETECTED ████' > "$TDIR/watch.viol"
printf '%s\n' '════ IMMUTABILITY: polls=7 blocks_tracked=3 resets=0 VIOLATIONS=0 (0=immutable) ════' > "$TDIR/immut.ok"
printf '%s\n' 'immutability monitor: track ... dur=21600s' > "$TDIR/immut.nosummary"
printf '%s\n' '════ IMMUTABILITY: polls=0 blocks_tracked=0 resets=0 VIOLATIONS=0 (0=immutable) ════' > "$TDIR/immut.zeropolls"
printf '%s\n' '════ IMMUTABILITY: polls=7 blocks_tracked=3 resets=0 VIOLATIONS=2 (0=immutable) ════' > "$TDIR/immut.viol"
: > "$TDIR/empty"
assert_accept "healthy monitors"           "verify_monitor_evidence $TDIR/watch.ok $TDIR/immut.ok"
assert_reject "watcher output missing"     "verify_monitor_evidence $TDIR/does-not-exist $TDIR/immut.ok"
assert_reject "watcher output empty"       "verify_monitor_evidence $TDIR/empty $TDIR/immut.ok"
assert_reject "immut output missing"       "verify_monitor_evidence $TDIR/watch.ok $TDIR/does-not-exist"
assert_reject "immut output empty"         "verify_monitor_evidence $TDIR/watch.ok $TDIR/empty"
assert_reject "watcher never started"      "verify_monitor_evidence $TDIR/watch.nostart $TDIR/immut.ok"
assert_reject "watcher no diff cycle"      "verify_monitor_evidence $TDIR/watch.nocycle $TDIR/immut.ok"
assert_reject "immut no summary (crashed)" "verify_monitor_evidence $TDIR/watch.ok $TDIR/immut.nosummary"
assert_reject "immut polls=0"              "verify_monitor_evidence $TDIR/watch.ok $TDIR/immut.zeropolls"
assert_reject "watcher violation"          "verify_monitor_evidence $TDIR/watch.viol $TDIR/immut.ok"
assert_reject "immut violation"            "verify_monitor_evidence $TDIR/watch.ok $TDIR/immut.viol"

echo "───────────────────────────────────────────────────────────────────────"
if [ "$RC" -eq 0 ]; then echo "ALL GUARD TESTS PASSED"; else echo "GUARD TESTS FAILED"; fi
exit "$RC"
