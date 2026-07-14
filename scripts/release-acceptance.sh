#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# SOURCE-BUILD release acceptance: prove the checkout IS the claimed immutable
# release, build the proxy image FROM that verified source, then run 1× full e2e
# suite → N=30 mixed loadtest → verdict. The final gate before accepting a cut
# release (agreed 2026-07-13: small acceptance instead of a re-soak — the RC was
# already soaked: 500+ ops, 19 chaos events, 0 completeness/immutability
# violations).
#
# SCOPE / CONTRACT (read this before trusting the verdict):
#   This is a SOURCE-BUILD acceptance. It certifies the SOURCE at the exact
#   RELEASE_REF: it proves the worktree is a clean checkout of that immutable ref
#   and builds the proxy image locally from those sources. It does NOT test a
#   pre-published registry artifact. Equivalence between this local build and any
#   image already pushed to a registry is OUT OF SCOPE — mutable docker base tags
#   and OS/cargo package repositories mean a rebuild is not bit-identical to an
#   earlier push. If you need to accept a *published* digest, pull that digest and
#   run it; that is a different contract and this script does not implement it.
#
# Usage:
#   RELEASE_REF=<full-SHA-or-exact-tag> RELEASE_IMAGE=miden-agglayer-e2e:vX.Y.Z \
#       ./scripts/release-acceptance.sh
#
#   RELEASE_REF must be EITHER a full 40/64-char hex commit SHA, OR an existing
#   annotated/lightweight tag. Mutable refs (HEAD, branch names, revision
#   expressions like origin/main or HEAD~1) are REJECTED — the accepted artifact
#   must be immutable and independently re-derivable. The worktree must be a CLEAN
#   checkout whose HEAD equals that ref, e.g.:
#     git worktree add /tmp/rel vX.Y.Z && cd /tmp/rel
#     RELEASE_REF=vX.Y.Z RELEASE_IMAGE=miden-agglayer-e2e:vX.Y.Z \
#         /path/to/scripts/release-acceptance.sh
#
# The locally-built release image is tagged :latest for the compose bringup and
# re-tagged $RELEASE_IMAGE after we verify the running proxy IS the image this run
# built. Monitors (external completeness watcher + getLogs immutability monitor)
# run for the whole acceptance; any genuine violation fails it.
#
# NOTE ON set -e: this script deliberately does NOT use `set -e` because it relies
# on intended non-zero exits (grep counts, `|| fail` guards, `[ … ]` tests). Every
# fallible command is instead guarded explicitly with `|| fail`.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail

ts() { TZ=${TZ_DISPLAY:-Europe/Berlin} date '+%H:%M:%S'; }
step() { echo "[$(ts)] ════ $* ════"; }
fail() { echo "[$(ts)] ACCEPTANCE FAIL: $*"; exit 1; }

# ── Guard functions (unit-testable; see scripts/release-acceptance-guards.test.sh)
# Each guard resolves its facts DIRECTLY and calls `fail` on any ambiguity,
# missing datum, or mismatch. None of them "fail open".

# Clean worktree: no tracked modifications, staged changes, or untracked files.
# --untracked-files=all so a repo-local status.showUntrackedFiles=no cannot hide
# stray files; --porcelain=v1 pins the machine format regardless of git version.
verify_clean_worktree() {
    local dirty
    dirty="$(git status --porcelain=v1 --untracked-files=all)" \
        || fail "git status failed — cannot prove the worktree is clean"
    [ -z "$dirty" ] || fail "worktree is DIRTY — refusing to accept a non-pristine checkout:
$dirty"
}

# Immutable exact ref: RELEASE_REF must be a full commit SHA OR an existing tag,
# resolved DIRECTLY (never via git describe, which is ambiguous when several tags
# point at one commit, and never via a regex over ref names). HEAD must equal the
# resolved commit. Sets HEAD_SHA and REF_SHA on success.
verify_exact_ref() {
    local ref="${RELEASE_REF}"
    HEAD_SHA="$(git rev-parse --verify HEAD)" \
        || fail "cannot resolve HEAD to a commit"
    if [[ "$ref" =~ ^[0-9a-f]{40}$ ]] || [[ "$ref" =~ ^[0-9a-f]{64}$ ]]; then
        # Full hex SHA: prove the object exists and is (or peels to) a commit.
        git cat-file -e "${ref}^{commit}" 2>/dev/null \
            || fail "RELEASE_REF '$ref' is a full SHA but no such commit exists in this repo"
        REF_SHA="$(git rev-parse --verify "${ref}^{commit}")" \
            || fail "RELEASE_REF '$ref' could not be resolved to a commit"
    elif REF_SHA="$(git rev-parse --verify --quiet "refs/tags/${ref}^{commit}" 2>/dev/null)" \
            && [ -n "$REF_SHA" ]; then
        : # RELEASE_REF is an existing tag; REF_SHA is its peeled commit.
    else
        fail "RELEASE_REF '$ref' is neither a full commit SHA nor an existing tag —
mutable refs (HEAD, branch names, revision expressions) are rejected; pass an
immutable tag or full commit SHA"
    fi
    [ "$HEAD_SHA" = "$REF_SHA" ] \
        || fail "HEAD $HEAD_SHA is not the claimed release $ref ($REF_SHA)"
}

# Provenance-by-construction: the running proxy must be exactly the image this run
# built from the verified checkout. Capture the running image ID and the built
# image ID INDEPENDENTLY, require BOTH non-empty, compare, then re-tag — every
# docker call guarded so nothing fails open. Sets RUNNING_ID on success.
verify_running_image() {
    local running built
    running="$(docker inspect "${PROJECT}-miden-agglayer-1" --format '{{.Image}}')" \
        || fail "docker inspect of the running proxy container failed"
    [ -n "$running" ] || fail "running proxy image ID is empty — cannot verify provenance"
    built="$(docker image inspect miden-agglayer-e2e:latest --format '{{.Id}}')" \
        || fail "docker image inspect of the built image (miden-agglayer-e2e:latest) failed"
    [ -n "$built" ] || fail "built image ID is empty — cannot verify provenance"
    [ "$running" = "$built" ] \
        || fail "running proxy is not the image built from the verified checkout (running=$running built=$built)"
    docker tag "$running" "$RELEASE_IMAGE" \
        || fail "docker tag $running -> $RELEASE_IMAGE failed"
    RUNNING_ID="$running"
}

main() {
    cd "$(dirname "${BASH_SOURCE[0]}")/.."
    : "${RELEASE_IMAGE:?set RELEASE_IMAGE to the release image tag}"
    : "${RELEASE_REF:?set RELEASE_REF to the exact release tag or full commit SHA being accepted}"
    PROJECT="${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}"
    export COMPOSE_PROJECT_NAME="$PROJECT"
    OUT="${ACCEPT_DIR:-/tmp/release-acceptance}"; mkdir -p "$OUT"

    # ── Provenance ENFORCEMENT (release-acceptance-provenance) ────────────────
    # The image built below is only as trustworthy as the checkout it is built
    # from. BEFORE building, prove the checkout IS the claimed immutable release
    # and is not locally modified — otherwise the acceptance would certify
    # drifted/dirty source.
    verify_clean_worktree
    verify_exact_ref
    echo "[$(ts)] provenance verified: clean worktree at $RELEASE_REF ($HEAD_SHA)"

    # Provenance model: the checkout was PROVEN above to be a clean tree at the
    # exact RELEASE_REF, so the bringup's `up -d --build` builds the proxy image
    # from that release's sources BY CONSTRUCTION. Image-ID equality vs a pre-built
    # tag is NOT checkable (a cache-mounted cargo RUN step makes IDs
    # non-deterministic across builds); we instead assert the running proxy IS the
    # image this run built and re-point $RELEASE_IMAGE at it.
    step "fresh bringup — build proxy from the verified checkout at $RELEASE_REF ($HEAD_SHA)"
    make e2e-l2l2-up > "$OUT/up.log" 2>&1 || fail "bringup (see $OUT/up.log)"
    verify_running_image
    # NOTE: no freshness check — a full cache hit to a previous build of this same
    # immutable checkout is the SAME code (that is what caches are for). The
    # stale-image trap this script guards against is worktree-source drift, which
    # the proven-clean immutable-ref checkout rules out by construction.
    echo "[$(ts)] image verified: running proxy built from the verified checkout; tagged $RELEASE_IMAGE"

    step "monitors up (completeness watcher + immutability)"
    INTERVAL=10 MARGIN=2 ./scripts/monitoring/watch-completeness.sh > "$OUT/watch.output" 2>&1 &
    WPID=$!
    python3 scripts/monitoring/immutability-monitor.py 21600 > "$OUT/immut.output" 2>&1 &
    IPID=$!
    trap 'kill $WPID $IPID 2>/dev/null' EXIT

    step "1/2: full 'all' e2e suite"
    ./scripts/e2e-test.sh > "$OUT/suite.log" 2>&1 || fail "e2e suite (see $OUT/suite.log)"
    echo "[$(ts)] suite: ALL TESTS COMPLETE"

    step "2/2: N=30 mixed loadtest"
    ./scripts/e2e-loadtest-mixed.sh > "$OUT/n30.log" 2>&1 || fail "N=30 (see $OUT/n30.log + /tmp/mixed-verify.out)"
    echo "[$(ts)] N=30: GREEN"

    step "invariant sweep"
    V=$(grep -ac "COMPLETENESS VIOLATION" "$OUT/watch.output" 2>/dev/null | head -1)
    I=$(grep -acE "VIOLATION|CHANGED|mismatch" "$OUT/immut.output" 2>/dev/null | head -1)
    [ "${V:-0}" -eq 0 ] || fail "completeness watcher flagged $V violation(s)"
    [ "${I:-0}" -eq 0 ] || fail "immutability monitor flagged $I violation(s)"
    sed 's/\x1b\[[0-9;]*m//g' /tmp/mixed-verify.out | grep -aE "B2AGG->|CLAIM->|GER->|VERDICT"

    step "SOURCE-BUILD ACCEPTANCE PASSED — $RELEASE_IMAGE from $RELEASE_REF: 1× e2e + N=30 green, 0 violations"
}

# Only run the acceptance when executed directly; when sourced (by the guard test
# harness) just expose the functions above.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
