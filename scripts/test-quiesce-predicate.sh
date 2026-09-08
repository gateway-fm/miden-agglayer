#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# test-quiesce-predicate.sh — unit test for scripts/lib-quiesce.sh.
#
# `quiesce_projection` is the gate in front of every state fingerprint the
# recovery drills make. When it is wrong in the LAX direction the drill
# fingerprints a moving pipeline and reports a spurious data-loss failure
# (#88, 2026-09-04); when it is wrong in the STRICT direction the drill never
# runs at all ("NOT quiesced after 180s ... queue=1", 2026-09-05, a stale
# metric on a wholly idle stack). Both cost a day. So the predicate gets
# tested on stubs, with no docker and no stack.
#
# Every primitive it touches — postgres, /metrics, cast — is replaced here, so
# each of the four conditions can be failed IN ISOLATION and the message it
# produces asserted.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Fast samples: the predicate's logic is under test, not its patience.
export QUIESCE_SAMPLE_SECS=1
# shellcheck source=/dev/null
. "$SCRIPT_DIR/lib-quiesce.sh"

# ── Stub state. Defaults describe a fully quiesced stack. ────────────────────
S_QUEUE=0 S_NONTERM=0 S_PENDING=0 S_PREPARED=0 S_PREPARED_EXPIRED=0 S_PARKED=0
S_CURSOR=155 S_TIP=155 S_LOGS=42 S_GER_MATCH=1
S_L1GER="0x0c1926fbbdfff4759cfc9d479566e6baf1dbb61cf3d0be3cb6e875495544a9d8"
S_LOGS_GROW=0   # when 1, the log count increments on every sample

S_LOGS_FILE="$(mktemp)"
trap 'rm -f "$S_LOGS_FILE"' EXIT

reset_stubs() {
    S_QUEUE=0 S_NONTERM=0 S_PENDING=0 S_PREPARED=0 S_PREPARED_EXPIRED=0 S_PARKED=0
    S_CURSOR=155 S_TIP=155 S_LOGS=42 S_GER_MATCH=1 S_LOGS_GROW=0
    echo 42 > "$S_LOGS_FILE"
}

_q_metric() {
    case "$1" in
        agglayer_writer_queue_depth)      echo "$S_QUEUE" ;;
        agglayer_writer_nonterminal_jobs) echo "$S_NONTERM" ;;
        *) echo "" ;;
    esac
}
_q_pgq() {
    case "$1" in
        *projector_cursor*)          echo "$S_CURSOR" ;;
        *latest_block_number*)       echo "$S_TIP" ;;
        *"status='pending'"*)        echo "$S_PENDING" ;;
        # Two prepared queries now: reclaim-not-yet-due (live) and past-expiry.
        # They must be stubbed SEPARATELY or one value answers both and the
        # reported total is double what the test set.
        *"prepared_expiration_block IS NULL"*)     echo "$S_PREPARED" ;;
        *"prepared_expiration_block IS NOT NULL"*) echo "$S_PREPARED_EXPIRED" ;;
        *"handoff_state='prepared'"*)              echo "$S_PREPARED" ;;
        *queued_txns*)               echo "$S_PARKED" ;;
        *ger_entries*)               echo "$S_GER_MATCH" ;;
        # File-backed, not a shell variable: `_q_int` reads this through a
        # command substitution, so an increment to a variable would be lost
        # with the subshell and the "still growing" case would silently
        # degenerate into "stable" — the stub would then certify the exact
        # blind spot the case exists to prove.
        *synthetic_logs*)            if (( S_LOGS_GROW )); then
                                         local n; n=$(( $(cat "$S_LOGS_FILE") + 1 ))
                                         echo "$n" > "$S_LOGS_FILE"; echo "$n"
                                     else echo "$S_LOGS"; fi ;;
        *) echo "" ;;
    esac
}
_q_l1_ger() { echo "$S_L1GER"; }

# ── Harness ─────────────────────────────────────────────────────────────────
FAILURES=0
ok()   { printf '  PASS  %s\n' "$1"; }
bad()  { printf '  FAIL  %s\n' "$1"; FAILURES=$((FAILURES+1)); }

# expect_quiesced <name>
expect_quiesced() {
    local out rc
    out=$(quiesce_projection 5 2 2>&1); rc=$?
    if (( rc == 0 )); then ok "$1"
    else bad "$1 — expected quiesced, got rc=$rc: $out"; fi
}
# expect_blocked <name> <substring the diagnosis MUST contain>
expect_blocked() {
    local out rc
    out=$(quiesce_projection 3 2 2>&1); rc=$?
    if (( rc == 0 )); then
        bad "$1 — quiesce returned SUCCESS on a pipeline with pending work"
    elif [[ "$out" != *"$2"* ]]; then
        bad "$1 — blocked (good) but the diagnosis never named the condition; wanted '$2', got: $out"
    else
        ok "$1"
    fi
}

echo "quiesce predicate:"

reset_stubs
expect_quiesced "a fully drained pipeline quiesces"

reset_stubs; S_QUEUE=1
expect_blocked "a non-empty writer queue blocks" "(a) writer NOT drained"

reset_stubs; S_NONTERM=2
expect_blocked "an in-flight (non-terminal) writer job blocks" "nonterminal_jobs=2"

reset_stubs; S_NONTERM=""
expect_blocked "a /metrics that omits the gauges blocks (never treated as drained)" \
    "did not serve"

reset_stubs; S_PENDING=1
expect_blocked "a pending receipt blocks" "(b) store NOT drained"

reset_stubs; S_PREPARED=3
expect_blocked "a PREPARED note handoff whose reclaim is not yet due blocks" "live=3"

reset_stubs; S_PREPARED_EXPIRED=2
expect_blocked "a PREPARED handoff PAST its expiry block blocks, and is named as such" "past-expiry=2"

reset_stubs; S_PARKED=1
expect_blocked "a parked future-nonce txn blocks" "parked txns=1"

reset_stubs; S_GER_MATCH=0
expect_blocked "an L1 GER the proxy has not injected blocks" "is NOT yet injected"

reset_stubs; S_CURSOR=150
expect_blocked "a projector behind the synthetic tip blocks" "(d) projector behind"

# The one condition that is only observable ACROSS samples: everything reads
# settled every time, but new logs keep landing. This is the case a
# single-shot check cannot see.
reset_stubs; S_LOGS_GROW=1
expect_blocked "a still-growing synthetic log count blocks" "log count kept moving"

# Guard the escape hatch: it must disable ONLY condition (c).
reset_stubs; S_GER_MATCH=0
QUIESCE_SKIP_L1_GER=1 expect_quiesced "QUIESCE_SKIP_L1_GER=1 waives (c) and only (c)"
reset_stubs; S_GER_MATCH=0; S_PENDING=1
QUIESCE_SKIP_L1_GER=1 expect_blocked "QUIESCE_SKIP_L1_GER=1 still enforces (b)" \
    "(b) store NOT drained"

echo
if (( FAILURES )); then
    echo "quiesce predicate: $FAILURES FAILED"
    exit 1
fi
echo "quiesce predicate: all checks passed"
