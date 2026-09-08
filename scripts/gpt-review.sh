#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# gpt-review.sh — the independent GPT (codex) review pass from WORKFLOW.md step 4.
#
# Runs OpenAI's `codex review` over a branch diff as an ADVERSARIAL second model,
# BEFORE the change is pushed. It is fed the invariant the fix must not break so
# it hunts the failure modes a single author is blind to: correctness holes,
# over-claims, and missed edge cases. This pass is local — it does not appear in
# the PR; it is what makes the PR good.
#
# Usage:
#   ./scripts/gpt-review.sh                    # HEAD vs origin/main (default)
#   ./scripts/gpt-review.sh --base <branch>    # vs a different base
#   ./scripts/gpt-review.sh --commit <sha>     # exactly one commit
#   ./scripts/gpt-review.sh --uncommitted      # staged+unstaged+untracked (pre-commit)
#   INVARIANT="certificates keep settling; claim indices stay stable" \
#     ./scripts/gpt-review.sh                  # override the invariant brief
#
# Requires: `codex` on PATH and logged in (`codex login`). Model override via
#   CODEX_MODEL=... (default: leave codex's configured default).
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

command -v codex >/dev/null 2>&1 || { echo "gpt-review: 'codex' not on PATH — install/login the Codex CLI first" >&2; exit 127; }

BASE="${BASE:-origin/main}"
MODE="base"
declare -a SEL=(--base "$BASE")
while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)        BASE="$2"; SEL=(--base "$BASE"); MODE="base"; shift 2;;
    --commit)      SEL=(--commit "$2"); MODE="commit:$2"; shift 2;;
    --uncommitted) SEL=(--uncommitted); MODE="uncommitted"; shift 1;;
    -h|--help)     sed -n '2,24p' "$0"; exit 0;;
    *)             echo "gpt-review: unknown arg '$1'" >&2; exit 2;;
  esac
done

# The invariant this change must not break — the heart of the review brief.
INVARIANT="${INVARIANT:-Do not break: AggLayer certificate settlement, bridge claim-index stability, byte-identical --restore, or the projector sealing contract (write-before-advance, LET reservation, emitted-frontier).}"

read -r -d '' PROMPT <<EOF || true
You are an adversarial code reviewer for miden-agglayer, a synthetic-EVM bridge
proxy over a Miden L2. Review ONLY the diff in scope. Be concrete and skeptical.

Invariant this change MUST NOT break:
${INVARIANT}

Report, most severe first, each as: file:line — the defect — a concrete failing
input/state → wrong output. Focus on:
  1. Correctness holes and race/ordering/recovery bugs (esp. crash-safety,
     nonce handling, cursor/quiesce, at-least-once vs exactly-once effects).
  2. Over-claims — where a comment, log, or PR statement asserts more than the
     code guarantees.
  3. Missed edge cases the author is likely blind to (empty/expired/erased
     notes, restart mid-effect, unresolvable destinations, upstream quirks).
  4. Comments that restate the code instead of explaining WHY (flag them).
If you find nothing real, say so plainly — do not invent findings.
EOF

MODEL_ARGS=()
[[ -n "${CODEX_MODEL:-}" ]] && MODEL_ARGS=(-c "model=\"${CODEX_MODEL}\"")

echo "── gpt-review: codex review [${MODE}] · base=${BASE} ──" >&2
git --no-pager diff --stat "${BASE}...HEAD" 2>/dev/null | tail -1 >&2 || true

exec codex review "${MODEL_ARGS[@]}" "${SEL[@]}" \
  --title "gpt-review ${MODE}" \
  "$PROMPT"
