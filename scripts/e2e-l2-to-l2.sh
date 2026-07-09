#!/usr/bin/env bash
# L2->L2 e2e (Miden <-> OP-Stack) — SKELETON (task #25). See docs/l2-to-l2-notes.md for the full design.
# Requires: a second OP-Stack L2 registered as agglayer rollup #2 + its own aggkit (NOT yet wired — see notes).
set -euo pipefail
GREEN='\033[0;32m'; NC='\033[0m'; log(){ echo -e "${GREEN}[l2l2]${NC} $*"; }
fail(){ echo "FAIL: $*" >&2; exit 1; }

# ── Step 0: bring up the L2B-extended stack + register rollup #2 ─────────────
# (assumes the base stack is ALREADY up healthy via `make e2e-up`; this adds
#  the L2B services on top and runs the one-time L1/L2B setup — all idempotent)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
L2B_RPC="${L2B_RPC:-http://localhost:9545}"
log "Step 0: L2B services + rollup #2 registration"
"$SCRIPT_DIR/gen-l2b-configs.sh"
docker compose -f "$REPO/docker-compose.e2e.yml" -f "$REPO/docker-compose.l2l2.yml" \
  --env-file "$REPO/fixtures/.env" up -d anvil-l2b aggkit-l2b agglayer bridge-service
for i in $(seq 1 30); do cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1 && break; sleep 2; done
cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1 || fail "anvil-l2b not reachable at $L2B_RPC"
L2B_RPC="$L2B_RPC" "$SCRIPT_DIR/setup-l2b.sh"

# TODO(1): deploy ERC-20 OPT0 on L2B (origin_network = 2, not L1)
log "Step 1: deploy OPT0 on L2B — TODO (forge create fixtures/TestToken.sol vs anvil-l2b + approve bridge)"

# TODO(2): FORWARD OP-Stack -> Miden: bridgeAsset(destNet=Miden) -> wait GER -> claim on Miden.
#          Assert Miden provisions a faucet keyed by hash(tokenAddr || OP-Stack-network) [#108 (addr,network)],
#          wrapped balance correct, ClaimEvent at exact consumption block.
log "Step 2: forward-bridge OP-Stack->Miden + assert foreign-origin faucet — TODO"

# TODO(3): FAUCET ISOLATION (#15): deploy same-address ERC-20 on L1, bridge in, assert DISTINCT Miden faucet.
log "Step 3: same-address/different-origin faucet isolation — TODO"

# TODO(4): BACK Miden -> OP-Stack: bridge-out (burn wrapped) -> claim on OP-Stack -> assert round-trip restored.
log "Step 4: back-bridge Miden->OP-Stack + assert round-trip — TODO"

# TODO(5): exact-block completeness asserts (0 missing/extra/locks) + N-run loadtest variant.
log "Step 5: exact-block asserts — TODO"

fail "SKELETON ONLY — OP-Stack L2 + agglayer rollup #2 not yet wired (see docs/l2-to-l2-notes.md)"
