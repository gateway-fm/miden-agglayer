#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Release acceptance test: fresh bringup on the RELEASE artifact → 1× full e2e
# suite → N=30 mixed loadtest → verdict. The final gate before accepting a cut
# release (agreed 2026-07-13: small acceptance instead of a re-soak — the RC was
# already soaked: 500+ ops, 19 chaos events, 0 completeness/immutability
# violations).
#
# Usage:
#   RELEASE_REF=vX.Y.Z RELEASE_IMAGE=miden-agglayer-e2e:vX.Y.Z ./scripts/release-acceptance.sh
#   # RELEASE_REF (tag or SHA) is enforced: the worktree must be a CLEAN checkout of it.
#   # or build from a tag first:
#   #   git worktree add /tmp/rel vX.Y.Z && docker build -t miden-agglayer-e2e:vX.Y.Z /tmp/rel
#
# The release image is tagged :latest for the compose bringup. Monitors
# (external completeness watcher + getLogs immutability monitor) run for the
# whole acceptance; any genuine violation fails it.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
: "${RELEASE_IMAGE:?set RELEASE_IMAGE to the release image tag}"
PROJECT="${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}"
export COMPOSE_PROJECT_NAME="$PROJECT"
OUT="${ACCEPT_DIR:-/tmp/release-acceptance}"; mkdir -p "$OUT"
ts() { TZ=${TZ_DISPLAY:-Europe/Berlin} date '+%H:%M:%S'; }
step() { echo "[$(ts)] ════ $* ════"; }
fail() { echo "[$(ts)] ACCEPTANCE FAIL: $*"; exit 1; }

# ── Provenance ENFORCEMENT (release-acceptance-provenance) ────────────────────
# The image built below is only as trustworthy as the checkout it is built from.
# BEFORE building, prove the checkout IS the claimed immutable release and is not
# locally modified — otherwise the acceptance would certify drifted/dirty source.
#
#   RELEASE_REF  (required) — the exact release tag or commit SHA being accepted.
#   The HEAD must resolve to it, and — for a tag — HEAD must be EXACTLY that tag.
: "${RELEASE_REF:?set RELEASE_REF to the exact release tag or SHA being accepted}"

# 1. Clean worktree: no tracked modifications, staged changes, or untracked files.
#    A dirty tree means the built image would not match the claimed ref's source.
DIRTY="$(git status --porcelain)"
[ -z "$DIRTY" ] || fail "worktree is DIRTY — refusing to accept a non-pristine checkout:
$DIRTY"

# 2. Exact ref: HEAD must resolve to RELEASE_REF's commit …
HEAD_SHA="$(git rev-parse HEAD)"
if ! REF_SHA="$(git rev-parse --verify "${RELEASE_REF}^{commit}" 2>/dev/null)"; then
    fail "RELEASE_REF '$RELEASE_REF' does not resolve to a commit in this repo"
fi
[ "$HEAD_SHA" = "$REF_SHA" ] \
    || fail "HEAD $HEAD_SHA is not the claimed release $RELEASE_REF ($REF_SHA)"

# 3. … and if RELEASE_REF names a tag, HEAD must be EXACTLY that tag (not merely a
#    commit that happens to share the sha with an unrelated ref).
if git rev-parse --verify --quiet "refs/tags/${RELEASE_REF}" >/dev/null; then
    git describe --tags --exact-match HEAD 2>/dev/null | grep -qx "$RELEASE_REF" \
        || fail "HEAD is not exactly tag $RELEASE_REF (git describe --exact-match mismatch)"
fi
echo "[$(ts)] provenance verified: clean worktree at $RELEASE_REF ($HEAD_SHA)"

# Provenance model: the checkout was PROVEN above to be a clean tree at the exact
# RELEASE_REF, so the bringup's `up -d --build` builds the proxy image from that release's
# sources by construction. Image-ID equality vs a pre-built tag is NOT checkable (a
# cache-mounted cargo RUN step makes IDs non-deterministic across builds); we assert the
# running proxy IS the image this run built and re-point $RELEASE_IMAGE at it.
RUN_START=$(date -u +%s)
step "fresh bringup from the release checkout ($(git -C . describe --tags --always 2>/dev/null))"
make e2e-l2l2-up > "$OUT/up.log" 2>&1 || fail "bringup (see $OUT/up.log)"
RUNNING_ID=$(docker inspect "${PROJECT}-miden-agglayer-1" --format '{{.Image}}')
[ "$RUNNING_ID" = "$(docker image inspect miden-agglayer-e2e:latest --format '{{.Id}}')" ] \
    || fail "running proxy is not the image this run built"
# NOTE: no freshness check — a full cache hit to a previous build of this same
# immutable tag checkout is the SAME code (that is what caches are for). The
# stale-image trap this script guards against is worktree-source drift, which an
# immutable tag context rules out by construction.
docker tag "$RUNNING_ID" "$RELEASE_IMAGE"
echo "[$(ts)] image verified: built this run from the release checkout; tagged $RELEASE_IMAGE"

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

step "ACCEPTANCE PASSED — $RELEASE_IMAGE: 1× e2e + N=30 green, 0 violations"
