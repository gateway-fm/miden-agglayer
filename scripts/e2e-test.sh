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
        # Same foreign-deployment machinery, MINT path: a foreign deployment
        # minting must not raise a false Cantina #2/#4 alert on our monitors.
        # Steady-state (no restarts) — keep it with the claim-provenance test,
        # before the restart tests.
        "$SCRIPT_DIR/e2e-mint-monitor-provenance.sh"
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
    mint-monitor-provenance)
        "$SCRIPT_DIR/e2e-mint-monitor-provenance.sh"
        ;;
    *)
        echo -e "${RED}Unknown test: $test_filter${NC}" >&2
        echo "Usage: $0 [all|tip-consistency|l1-to-l2|l2-to-l1|dynamic-erc20|cantina13|cantina10|ger-decomposition|security|cantina12-getlogs-returns-all|cantina6-faucet-identity-restore|fuzz|reconciler-private-note|reconciler-cursor|ger-atomic|claim-provenance|mint-monitor-provenance]" >&2
        exit 1
        ;;
esac

echo ""
log "======================================================================"
log "  ALL TESTS COMPLETE"
log "======================================================================"
