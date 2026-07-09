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
        # MA#18 recovery e2e runs BEFORE the proxy-restarting tests: its induced
        # "unknown faucet" state (deleted faucet_registry row) is deliberately
        # repaired by --restore's restore_faucet_identities (Cantina #6 heal),
        # so a restore still finishing in the background — private-note's
        # Phase B ends with reset+restore — can resurrect the row mid-test
        # and flip the outcome (observed live: quarantine in 2s on one run,
        # heal-won no-quarantine timeout on the next).
        "$SCRIPT_DIR/e2e-erased-note-recovery.sh"
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
        # Cantina MA#18 — erased/unbridgeable B2AGG recovery. Settlement-safe
        # (it recovers a REAL, on-chain-backed leaf), so it runs BEFORE the
        # cantina13 poison-leaf test that must stay last.
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
    erased-note-recovery)
        "$SCRIPT_DIR/e2e-erased-note-recovery.sh"
        ;;
    erased-note-hunt)
        # Probe, not a regression gate: fires up to HUNT_MAX real bridge-outs
        # hunting a GENUINE same-block erasure and verifies the divergence
        # monitor detects it. Load-shaped runtime — deliberately NOT in 'all'.
        "$SCRIPT_DIR/e2e-erased-note-hunt.sh"
        ;;
    *)
        echo -e "${RED}Unknown test: $test_filter${NC}" >&2
        echo "Usage: $0 [all|tip-consistency|l1-to-l2|l2-to-l1|dynamic-erc20|cantina13|cantina10|ger-decomposition|security|cantina12-getlogs-returns-all|cantina6-faucet-identity-restore|fuzz|reconciler-private-note|reconciler-cursor|ger-atomic|claim-provenance|erased-note-recovery|erased-note-hunt]" >&2
        exit 1
        ;;
esac

echo ""
log "======================================================================"
log "  ALL TESTS COMPLETE"
log "======================================================================"
