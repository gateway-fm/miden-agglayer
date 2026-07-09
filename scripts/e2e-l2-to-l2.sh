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

# ── Step 1: deploy OPT0 on L2B (origin_network = 2, not L1) ──────────────────
# PROVEN LIVE 2026-07-09 (see docs/l2-to-l2-notes.md UPDATE 3).
BRIDGE=0xC8cbEBf950B9Df44d987c8619f092beA980fF038
ADMIN=0xE34aaF64b29273B7D567FCFc40544c014EEe9970
ADMIN_KEY=0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625  # TEST-ONLY
L1_RPC="${L1_RPC:-http://localhost:8545}"
GER_L1=0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674
MIDEN_RPC="${MIDEN_RPC:-http://localhost:8546}"
log "Step 1: deploying OPT0 on L2B"
OUT=$(forge create "$REPO/fixtures/TestToken.sol:TestToken" --rpc-url "$L2B_RPC"   --private-key $ADMIN_KEY --broadcast   --constructor-args "L2BToken" "OPT0" 18 1000000000000000000000000 2>&1)
OPT0=$(echo "$OUT" | grep "Deployed to:" | awk '{print $NF}')
[ -n "$OPT0" ] || fail "OPT0 deploy failed: $(echo "$OUT" | tail -2)"
log "  OPT0: $OPT0"

# ── Step 2 (forward half A): bridgeAsset L2B -> Miden + GER propagation ──────
AMOUNT=500000000000000000000
log "Step 2: bridgeAsset(destNet=1/Miden, $AMOUNT OPT0)"
cast send "$OPT0" "approve(address,uint256)" $BRIDGE $AMOUNT   --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null
cast send $BRIDGE "bridgeAsset(uint32,address,uint256,address,bool,bytes)"   1 $ADMIN $AMOUNT "$OPT0" true 0x   --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null || fail "bridgeAsset on L2B"
DC=$(cast call $BRIDGE 'depositCount()(uint256)' --rpc-url "$L2B_RPC")
log "  L2B depositCount: $DC"
# wait: aggsender-l2b cert -> agglayer settle -> L1 GER -> Miden aggoracle
log "  waiting for GER propagation L2B -> L1 -> Miden (cert settle, <=120s)..."
DEADLINE=$(( $(date +%s) + 120 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  L1GER=$(cast call $GER_L1 'getLastGlobalExitRoot()(bytes32)' --rpc-url "$L1_RPC")
  MIDENGER=$(curl -s "$MIDEN_RPC" -H 'Content-Type: application/json'     -d '{"jsonrpc":"2.0","id":1,"method":"zkevm_getLatestGlobalExitRoot","params":[]}'     | python3 -c "import json,sys;print(json.load(sys.stdin).get('result',''))" 2>/dev/null)
  if [ -n "$MIDENGER" ] && [ "$MIDENGER" = "$L1GER" ]; then
    log "  GER propagated to Miden: $MIDENGER"; break
  fi
  sleep 5
done
[ "$MIDENGER" = "$L1GER" ] || fail "GER did not propagate to Miden within 120s (L1=$L1GER miden=$MIDENGER)"

# TODO(2b): CLAIM on Miden via bridge-service proof (/merkle-proof for network 2)
#           -> assert foreign-origin faucet keyed by (OPT0, net-2) [#108 keying],
#           wrapped balance, ClaimEvent at exact consumption block.
log "Step 2b: claim on Miden + foreign-origin faucet assert — TODO (next)"

# TODO(3): FAUCET ISOLATION (#15): deploy same-address ERC-20 on L1, bridge in, assert DISTINCT Miden faucet.
log "Step 3: same-address/different-origin faucet isolation — TODO"

# TODO(4): BACK Miden -> OP-Stack: bridge-out (burn wrapped) -> claim on OP-Stack -> assert round-trip restored.
log "Step 4: back-bridge Miden->OP-Stack + assert round-trip — TODO"

# TODO(5): exact-block completeness asserts (0 missing/extra/locks) + N-run loadtest variant.
log "Step 5: exact-block asserts — TODO"

fail "SKELETON ONLY — OP-Stack L2 + agglayer rollup #2 not yet wired (see docs/l2-to-l2-notes.md)"
