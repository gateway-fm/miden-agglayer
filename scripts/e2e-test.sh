#!/usr/bin/env bash
# Full bidirectional bridge E2E test — L1→L2 deposit+claim then L2→L1 bridge-out.
# Runs both directions sequentially. Use e2e-l1-to-l2.sh / e2e-l2-to-l1.sh for individual tests.
#
# Usage:
#   ./scripts/e2e-test.sh            # run both directions
#   ./scripts/e2e-test.sh l1-to-l2   # L1→L2 only
#   ./scripts/e2e-test.sh l2-to-l1   # L2→L1 only
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

GREEN='\033[0;32m'; RED='\033[0;31m'; NC='\033[0m'
log() { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }

test_filter="${1:-all}"

log "======================================================================"
log "  Miden Bridge E2E Test Suite"
log "======================================================================"
echo ""

case "$test_filter" in
    all)
        # First and cheapest: RPC tip coherence + liveness (postmortem
        # 2026-07-04 frozen-eth_blockNumber regression). Quick-fails the suite.
        "$SCRIPT_DIR/e2e-rpc-tip-consistency.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l1-to-l2.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2-to-l1.sh"
        echo ""
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        echo ""
        "$SCRIPT_DIR/e2e-security.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina12-getlogs-returns-all.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina10-concurrent-faucet.sh"
        echo ""
        "$SCRIPT_DIR/e2e-dynamic-erc20.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina6-faucet-identity-restore.sh"
        echo ""
        # Foreign-bridge claim provenance: deploys a SECOND agglayer deployment
        # on the same chain and drives a claim through it — must not leak a
        # ClaimEvent into our synthetic_logs. Runs before the proxy-restarting
        # tests (it needs the steady-state reconciler/projector, no restarts).
        "$SCRIPT_DIR/e2e-claim-provenance.sh"
        echo ""
        # Manual user claim: a non-sponsor USER key claims its own deposit via
        # raw eth_sendRawTransaction, plus the signer-agnostic dedup race
        # against the sponsor on the same globalIndex. Steady-state (no
        # restarts), so it runs with the other claim-path tests before the
        # proxy-restarting group.
        "$SCRIPT_DIR/e2e-manual-user-claim.sh"
        echo ""
        # Proxy-restarting tests run LAST (they must not race the other
        # scripts' steady-state assumptions), in this order: the cursor test
        # does a plain restart and asserts the sweep RESUMES from the
        # persisted cursor; the private-note test then ends with
        # reset-miden-store + restore, which deliberately RESETS that cursor
        # for a full re-sweep — so it must come after, or the cursor
        # assertion races the restore's intentional genesis walk.
        "$SCRIPT_DIR/e2e-reconciler-cursor-persistence.sh"
        echo ""
        # Audit H2 — GER atomic-commit crash consistency: plain restart, asserts
        # the hash chain is NOT re-rolled and no duplicate UpdateHashChainValue
        # log is emitted. Runs after cursor-persistence (both plain restarts) but
        # BEFORE the private-note test, which ends with a destructive
        # reset-miden-store + restore that would perturb the chain/log counts.
        "$SCRIPT_DIR/e2e-ger-atomic-commit.sh"
        echo ""
        "$SCRIPT_DIR/e2e-reconciler-private-note.sh"
        echo ""
        # ABSOLUTELY LAST on purpose: its final phase leaves a self-targeted
        # poison leaf in the LET (by design of the Cantina #13 circuit-break
        # repro), which wedges any certificate settlement that would happen
        # after it — so no settlement-dependent test may follow.
        "$SCRIPT_DIR/e2e-cantina13-metadata-recovery.sh"
        echo ""
        # L2<->L2 + native Miden-originated tiers — canonical part of the full suite on
        # this branch. They need the docker-compose.l2l2.yml overlay (postgres-l2b /
        # bridge-service-l2b), so GUARD on the overlay being up: on the base stack they're
        # skipped with a warning; on the l2l2 stack they run. `e2e-test.sh l2l2` runs the
        # L2<->L2 group (forward/clash/back); the native round-trips exercise a
        # Miden-originated token to BOTH destinations (->L2B and ->L1).
        if docker ps --format '{{.Names}}' 2>/dev/null | grep -q 'postgres-l2b'; then
            echo "== L2<->L2 group (L2B overlay present) =="
            "$SCRIPT_DIR/e2e-test.sh" l2l2
            echo ""
            echo "== native Miden-originated round-trips (->L2B, ->L1) =="
            DEST=l2b "$SCRIPT_DIR/e2e-miden-origin.sh"
            echo ""
            DEST=l1  "$SCRIPT_DIR/e2e-miden-origin.sh"
        else
            echo "SKIP L2<->L2 + native tiers — L2B overlay not up (base stack). Run 'make e2e-l2l2-up' to include them."
        fi
        ;;
    tip-consistency)
        "$SCRIPT_DIR/e2e-rpc-tip-consistency.sh"
        ;;
    l1-to-l2)
        "$SCRIPT_DIR/e2e-l1-to-l2.sh"
        ;;
    l2-to-l1)
        "$SCRIPT_DIR/e2e-l2-to-l1.sh"
        ;;
    dynamic-erc20)
        "$SCRIPT_DIR/e2e-dynamic-erc20.sh"
        ;;
    cantina13)
        "$SCRIPT_DIR/e2e-cantina13-metadata-recovery.sh"
        ;;
    cantina10)
        "$SCRIPT_DIR/e2e-cantina10-concurrent-faucet.sh"
        ;;
    ger-decomposition)
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        ;;
    security)
        "$SCRIPT_DIR/e2e-security.sh"
        ;;
    cantina12-getlogs-returns-all)
        "$SCRIPT_DIR/e2e-cantina12-getlogs-returns-all.sh"
        ;;
    cantina6-faucet-identity-restore)
        "$SCRIPT_DIR/e2e-cantina6-faucet-identity-restore.sh"
        ;;
    fuzz)
        "$SCRIPT_DIR/e2e-fuzz-bridge.sh"
        ;;
    reconciler-private-note)
        "$SCRIPT_DIR/e2e-reconciler-private-note.sh"
        ;;
    reconciler-cursor)
        "$SCRIPT_DIR/e2e-reconciler-cursor-persistence.sh"
        ;;
    ger-atomic)
        "$SCRIPT_DIR/e2e-ger-atomic-commit.sh"
        ;;
    claim-provenance)
        "$SCRIPT_DIR/e2e-claim-provenance.sh"
        ;;
    manual-user-claim)
        "$SCRIPT_DIR/e2e-manual-user-claim.sh"
        ;;
    l2l2)
        # SIMPLE L2<->L2 pipeline group (NOT in `all` — needs the
        # docker-compose.l2l2.yml overlay: second rollup + aggkit-l2b on top of
        # the base stack). Ordered: forward (deploy+bridge L2B->Miden+claim,
        # foreign-origin faucet), clash (same-address L1-vs-L2B faucet isolation,
        # #108), then back (bridge-out Miden->L2B+claim, net-zero). forward brings
        # the L2B overlay up idempotently (reused if already registered) and hands
        # off a shared wallet + OPT0 state file the clash and back legs consume.
        #
        # Preflight ONCE up front (fail-loud: nothing runs against a
        # half-configured/port-colliding stack), then pin ONE per-run evidence
        # timestamp so forward+back write the SAME NDJSON file for this run (a
        # fresh invocation => a fresh file: 3x cert => 3 evidence artifacts).
        PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
        source "$SCRIPT_DIR/lib-l2l2.sh"
        export EVIDENCE_RUN_TS="$(date +%s)"
        l2l2_ensure_stack
        l2l2_validate_stack
        export L2L2_PREFLIGHT_DONE=1
        "$SCRIPT_DIR/e2e-l2l2-forward.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2l2-clash.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2l2-back.sh"
        ;;
    l2l2-clash)
        "$SCRIPT_DIR/e2e-l2l2-clash.sh"
        ;;
    l2l2-forward)
        "$SCRIPT_DIR/e2e-l2l2-forward.sh"
        ;;
    l2l2-back)
        "$SCRIPT_DIR/e2e-l2l2-back.sh"
        ;;
    *)
        echo -e "${RED}Unknown test: $test_filter${NC}" >&2
        echo "Usage: $0 [all|tip-consistency|l1-to-l2|l2-to-l1|dynamic-erc20|cantina13|cantina10|ger-decomposition|security|cantina12-getlogs-returns-all|cantina6-faucet-identity-restore|fuzz|reconciler-private-note|reconciler-cursor|ger-atomic|claim-provenance|l2l2|l2l2-forward|l2l2-clash|l2l2-back]" >&2
        echo "       (l2l2 is optional and not part of 'all' — it needs the docker-compose.l2l2.yml overlay)" >&2
        exit 1
        ;;
esac

echo ""
log "======================================================================"
log "  ALL TESTS COMPLETE"
log "======================================================================"
