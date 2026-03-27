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
        "$SCRIPT_DIR/e2e-l1-to-l2.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2-to-l1.sh"
        echo ""
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        echo ""
        "$SCRIPT_DIR/e2e-security.sh"
        echo ""
        "$SCRIPT_DIR/e2e-dynamic-erc20.sh"
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
    ger-decomposition)
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        ;;
    security)
        "$SCRIPT_DIR/e2e-security.sh"
        ;;
    *)
        echo -e "${RED}Unknown test: $test_filter${NC}" >&2
        echo "Usage: $0 [all|l1-to-l2|l2-to-l1|dynamic-erc20|ger-decomposition|security]" >&2
        exit 1
        ;;
esac

echo ""
log "======================================================================"
log "  ALL TESTS COMPLETE"
log "======================================================================"
