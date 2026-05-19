#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-iaic-mempool-conflict.sh — concurrent-load IAIC induction
#
# Reproduces the bali production IAIC by firing concurrent submissions that
# race on the same Miden account in mempool. The mechanism (Loki-verified
# in bali 2026-05-11 → 2026-05-14):
#
#   1. A submission (tx_A) is pending in the Miden node's mempool, atop
#      account-commitment C0. The submitter is waiting for commit.
#   2. A SECOND submission (tx_B) arrives, also built atop C0 (because the
#      submitter sees C0 as the latest committed state — sync_state doesn't
#      surface mempool-pending txs as committed).
#   3. Miden node rejects tx_B with
#        AddTransactionError::IncorrectAccountInitialCommitment
#        Display: "incorrect account initial commitment"
#        gRPC message: "transaction conflicts with current mempool state"
#
# Pre-unify (`feat/v0.3.0-unify-claim-client` NOT applied) — the bug:
#   - publish_claim built a FRESH `miden_client::Client` per call
#     (src/claim.rs:611-704 in main).
#   - The fresh client did NOT funnel through the long-lived MidenClient's
#     `mpsc::channel::<Request>(1)`.
#   - Two concurrent claims for distinct global_indexes → two fresh clients
#     submitting CLAIM notes to the `bridge` account simultaneously →
#     mempool conflict → IAIC.
#
# Post-unify (e3e3e2a applied) — the fix:
#   - publish_claim routes through `MidenClient::with(...)` which uses the
#     long-lived client's `mpsc::channel::<Request>(1)`.
#   - Channel-of-1 PLUS `wait_for_transaction_commit` inside each closure
#     means EVERY submission completes (committed or definitively failed)
#     before the next one starts. Two concurrent submissions for the same
#     account are STRUCTURALLY IMPOSSIBLE.
#   - claim.rs:567 routes through client.with; the prior fresh-client block
#     (lines 611-704 in main) is deleted.
#
# Modes (set MODE env var):
#
#   MODE=expect_iaic     — build is PRE-e3e3e2a. Script PASSES if at least
#                          one `incorrect account initial commitment` log
#                          line appears under N concurrent claimAsset calls
#                          touching the bridge account. FAILS if zero (bug
#                          isn't reproducing or you're not pre-unify).
#
#   MODE=expect_no_iaic  — build is POST-e3e3e2a. Script PASSES if ZERO IAIC
#                          log lines appear under the same load profile.
#                          FAILS on any IAIC (the unify regressed or wasn't
#                          actually applied).
#
# Setup:
#   - Fresh stack via `make e2e-up`.
#   - First, run `scripts/e2e-l1-to-l2.sh` (or equivalent) to seed N
#     distinct, valid-to-claim globalIndexes that aggsender/claimsponsor
#     can replay. THIS SCRIPT consumes those — it does not create them.
#   - Set `CLAIM_REPLAY_FILE` to a file with one `cast send` argument-set
#     per line (the test fixture documented below).
#
# The script does NOT switch branches.
# Evidence captured to /tmp/repro-evidence-iaic-${MODE}-${RUN_SUFFIX}.txt
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

MODE="${MODE:-}"
case "$MODE" in
  expect_iaic|expect_no_iaic) ;;
  *) echo "MODE must be 'expect_iaic' or 'expect_no_iaic' (got: '$MODE')" >&2; exit 2 ;;
esac

# How many concurrent claim submissions to fire. Higher = more likely to
# trigger the race pre-unify, more confidence in absence post-unify.
PARALLEL="${PARALLEL:-10}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yml"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PROXY_CONTAINER="${PROXY_CONTAINER:-miden-agglayer-miden-agglayer-1}"

# The replay fixture: each line is the raw hex of an `eth_sendRawTransaction`
# payload for a previously-signed `claimAsset` against the proxy. The
# generation step is out of scope for this script — see the companion fixture
# generator (e2e-claim-watcher.sh's `--generate-replay` mode or equivalent).
CLAIM_REPLAY_FILE="${CLAIM_REPLAY_FILE:-$PROJECT_DIR/fixtures/.claim-replay.txt}"

RUN_SUFFIX="$(date +%s)"
EVIDENCE="/tmp/repro-evidence-iaic-${MODE}-${RUN_SUFFIX}.txt"

if [[ -t 1 ]]; then
  R=$'\033[0;31m'; G=$'\033[0;32m'; Y=$'\033[0;33m'; C=$'\033[0;36m'; B=$'\033[1m'; N=$'\033[0m'
else R=''; G=''; Y=''; C=''; B=''; N=''; fi

ts()   { date +%H:%M:%S; }
say()  { printf '%s[%s]%s %s\n' "$G" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }
step() { printf '\n%s[%s] %s%s%s\n' "$C" "$(ts)" "$B" "$*" "$N" | tee -a "$EVIDENCE"; }
warn() { printf '%s[%s] WARN:%s %s\n' "$Y" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }
fail() { printf '%s[%s] FAIL:%s %s\n' "$R" "$(ts)" "$N" "$*" >&2; printf 'FAIL %s\n' "$*" >>"$EVIDENCE"; exit 1; }
pass() { printf '%s[%s] PASS:%s %s\n' "$G" "$(ts)" "$N" "$*" | tee -a "$EVIDENCE"; }

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v jq >/dev/null || fail "jq not in PATH"
command -v curl >/dev/null || fail "curl not in PATH"
command -v xargs >/dev/null || fail "xargs not in PATH"
docker inspect "$PROXY_CONTAINER" >/dev/null 2>&1 \
  || fail "proxy container $PROXY_CONTAINER not found — run 'make e2e-up' first"
[[ -r "$CLAIM_REPLAY_FILE" ]] \
  || fail "CLAIM_REPLAY_FILE not found at $CLAIM_REPLAY_FILE — generate first via the companion fixture generator"

REPLAY_COUNT=$(wc -l <"$CLAIM_REPLAY_FILE" | tr -d ' ')
[[ "$REPLAY_COUNT" -ge "$PARALLEL" ]] \
  || fail "CLAIM_REPLAY_FILE has $REPLAY_COUNT entries, need ≥$PARALLEL"

printf '## evidence run %s, MODE=%s PARALLEL=%s\n' "$RUN_SUFFIX" "$MODE" "$PARALLEL" >"$EVIDENCE"
say "MODE = $MODE  PARALLEL = $PARALLEL"
say "replay fixture: $CLAIM_REPLAY_FILE ($REPLAY_COUNT entries)"

# ── Fire concurrent claim submissions ────────────────────────────────────────
step "Phase 1 — fire $PARALLEL concurrent claimAsset submissions"

# Anchor logs so post-load grep is scoped.
LOGS_BEFORE=$(docker logs "$PROXY_CONTAINER" 2>&1 | wc -l)
say "log line count before load: $LOGS_BEFORE"

# Send raw-tx payloads in parallel using xargs -P. Each line becomes a
# single eth_sendRawTransaction JSON-RPC call against the proxy.
JOBS_OUT="/tmp/iaic-replay-${RUN_SUFFIX}.jsonl"
: >"$JOBS_OUT"
head -n "$PARALLEL" "$CLAIM_REPLAY_FILE" \
  | xargs -P "$PARALLEL" -I {} bash -c '
      ts=$(date +%s%3N)
      resp=$(curl -s -X POST "$1" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendRawTransaction\",\"params\":[\"$2\"],\"id\":1}")
      printf "{\"ts_ms\":%s,\"resp\":%s}\n" "$ts" "$resp"
    ' _ "$L2_RPC" {} >>"$JOBS_OUT"

# How many responses did we get?
TOTAL_SENT=$(wc -l <"$JOBS_OUT" | tr -d ' ')
SUCCESS_COUNT=$(jq -c 'select(.resp.result)' "$JOBS_OUT" | wc -l | tr -d ' ')
ERROR_COUNT=$(jq -c 'select(.resp.error)' "$JOBS_OUT" | wc -l | tr -d ' ')
say "    submissions sent     = $TOTAL_SENT"
say "    JSON-RPC successes   = $SUCCESS_COUNT"
say "    JSON-RPC errors      = $ERROR_COUNT"

# ── Drain a few seconds so any in-flight submissions surface in logs ─────────
step "Phase 2 — wait 30s for any pending submissions to commit or fail"
sleep 30

# ── Scan logs for the IAIC signature ─────────────────────────────────────────
step "Phase 3 — scan proxy logs for the IAIC signature"
IAIC_COUNT=$(docker logs "$PROXY_CONTAINER" 2>&1 \
  | tail -n +$((LOGS_BEFORE + 1)) \
  | grep -c 'incorrect account initial commitment' || true)
MEMPOOL_COUNT=$(docker logs "$PROXY_CONTAINER" 2>&1 \
  | tail -n +$((LOGS_BEFORE + 1)) \
  | grep -c 'transaction conflicts with current mempool state' || true)

say "    IAIC log lines since load start    = $IAIC_COUNT  (pre-e3e3e2a: expect ≥1; post: expect 0)"
say "    mempool-conflict log lines         = $MEMPOOL_COUNT  (correlated with IAIC; expect equal)"

if [[ "$IAIC_COUNT" -gt 0 ]]; then
  FIRST=$(docker logs "$PROXY_CONTAINER" 2>&1 \
    | tail -n +$((LOGS_BEFORE + 1)) \
    | grep -m1 'incorrect account initial commitment' \
    | sed 's/\x1b\[[0-9;]*m//g')
  say "    first IAIC observed:"
  say "      $FIRST"
fi

# ── Assert ───────────────────────────────────────────────────────────────────
case "$MODE" in
  expect_iaic)
    [[ "$IAIC_COUNT" -ge 1 ]] \
      || fail "MODE=expect_iaic but no IAIC observed under $PARALLEL-way load. \
Either this build already has the unify (verify: src/claim.rs:611-704 should be the fresh-client block on main; absent post-unify) \
or the load isn't producing the race (try raising PARALLEL, or check that the replay fixture's claims target the SAME bridge account)."
    pass "BUG REPRODUCED: $IAIC_COUNT IAIC events fired under $PARALLEL-way concurrent claim load (mempool conflicts: $MEMPOOL_COUNT)"
    ;;
  expect_no_iaic)
    [[ "$IAIC_COUNT" -eq 0 ]] \
      || fail "MODE=expect_no_iaic but $IAIC_COUNT IAIC events fired. Either the unify regressed or this build doesn't include e3e3e2a. \
Verify: src/claim.rs:567 routes through client.with(...); the fresh-client block must be deleted."
    pass "STRUCTURAL FIX VERIFIED: 0 IAIC events under $PARALLEL-way concurrent claim load (mempool conflicts: $MEMPOOL_COUNT)"
    ;;
esac

step "Done. Evidence captured to $EVIDENCE"
say "summary:"
say "  MODE              = $MODE"
say "  PARALLEL          = $PARALLEL"
say "  IAIC count        = $IAIC_COUNT (expected ${MODE/expect_iaic/≥1}${MODE/expect_no_iaic/0})"
say "  mempool conflicts = $MEMPOOL_COUNT"
say "  JSON-RPC errors   = $ERROR_COUNT (high count may indicate the proxy was already dead — re-run with fewer submissions)"
