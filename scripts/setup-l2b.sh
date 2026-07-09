#!/usr/bin/env bash
# Register + initialize rollup #2 ("l2b-sovereign") on the L1 RollupManager, and
# deploy bridge contracts on the L2B chain. Part of the L2->L2 e2e (task #25).
#
# The REGISTRATION half (steps 1-2) is PROVEN — dry-run live against the anvil
# snapshot on 2026-07-09: attachAggchainToAL created rollupID=2 (aggchain at
# 0x5D1A491A416feEbf8C123A558ec28A239960bd0E on that run) and the hand-built
# initialize set trustedSequencer/networkName/threshold correctly. The BRIDGE
# half (step 3) uses bytecode extracted from the L1 snapshot (impl 13150 bytes,
# verified extractable) — still to be exercised end-to-end.
#
# Provenance of the recipe: fixtures/l1-raw-txs.txt blocks 83-85 decoded:
#   blk83 addNewRollupType(0xabcb5198): consensusImpl=0xFB054898..., verifier=0,
#         forkID=0, verifierType=2, genesis=0, "kurtosis-devnet", vkey=0  -> typeId 1
#   blk84 attachAggchainToAL(0x97d289a3): (typeId=1, chainID=2, abi.encode(aggchainAdmin))
#   blk85 init (selector 0x697427f6, AggchainECDSAMultisig):
#         (admin, trustedSequencer, gasToken, sequencerURL, networkName,
#          bytes32(0), signers=[(addr, url)], threshold)
#         original: sequencer=0x5b06..., URL "http://op-el-1-op-reth-op-node-001:8545",
#         name "op-sovereign" — the snapshot's rollup 1 WAS an OP-reth sovereign chain
#         that the Miden proxy replaced; we mirror the same shape for rollup 2.
set -euo pipefail
GREEN='\033[0;32m'; NC='\033[0m'; log(){ echo -e "${GREEN}[setup-l2b]${NC} $*"; }
fail(){ echo "FAIL: $*" >&2; exit 1; }

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2B_RPC="${L2B_RPC:-http://localhost:9545}"          # anvil-l2b (compose: anvil-l2b:8545)
L2B_CHAIN_ID="${L2B_CHAIN_ID:-31338}"
L2B_NETWORK_ID="${L2B_NETWORK_ID:-2}"                # agglayer network id of rollup #2
L2B_SEQ_URL="${L2B_SEQ_URL:-http://anvil-l2b:8545}"  # stored on-chain; informational
L2B_NAME="${L2B_NAME:-l2b-sovereign}"

ROLLUP_MANAGER=0x6c6c009cC348976dB4A908c92B24433d4F6edA43
L1_BRIDGE=0xC8cbEBf950B9Df44d987c8619f092beA980fF038
L2_GER_ADDR=0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA   # sovereign-GER convention addr
EIP1967_IMPL=0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc

# TEST-ONLY keys (kurtosis-cdk standard; see fixtures/agglayer-config.toml warning)
ADMIN=0xE34aaF64b29273B7D567FCFc40544c014EEe9970
ADMIN_KEY=0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625
SEQUENCER=0x5b06837A43bdC3dD9F114558DAf4B26ed49842Ed   # committee[0]; sequencer.keystore

command -v cast >/dev/null || fail "cast (foundry) required"

# ── Step 1: attach rollup #2 to the agglayer (reuses rollupTypeId 1) ─────────
COUNT=$(cast call $ROLLUP_MANAGER 'rollupCount()(uint32)' --rpc-url "$L1_RPC")
if [ "$COUNT" -ge 2 ]; then
  log "rollup #2 already attached (rollupCount=$COUNT) — skipping attach"
else
  log "Step 1: attachAggchainToAL(typeId=1, chainID=$L2B_CHAIN_ID, admin=$ADMIN)"
  INITBYTES=$(cast abi-encode 'f(address)' $ADMIN)
  cast send $ROLLUP_MANAGER "attachAggchainToAL(uint32,uint64,bytes)" \
    1 "$L2B_CHAIN_ID" "$INITBYTES" \
    --private-key $ADMIN_KEY --rpc-url "$L1_RPC" >/dev/null || fail "attachAggchainToAL"
fi
ROLLUP2=$(cast call $ROLLUP_MANAGER \
  "rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint64,uint64,uint8)" \
  "$L2B_NETWORK_ID" --rpc-url "$L1_RPC" | head -1)
log "rollup #2 aggchain: $ROLLUP2"

# ── Step 2: initialize the aggchain (selector 0x697427f6, hand-built ABI) ───
if [ "$(cast call "$ROLLUP2" 'trustedSequencer()(address)' --rpc-url "$L1_RPC" 2>/dev/null)" = "$SEQUENCER" ]; then
  log "rollup #2 already initialized — skipping init"
else
  log "Step 2: initialize aggchain (sequencer=$SEQUENCER, name=$L2B_NAME)"
  CALLDATA=$(python3 - "$ADMIN" "$SEQUENCER" "$L2B_SEQ_URL" "$L2B_NAME" <<'PY'
import sys
admin, seq, url, name = sys.argv[1:5]
def w(x): return format(x,'064x')
def addr(a): return a[2:].lower().rjust(64,'0')
def s2w(s):
    b=s.encode(); assert len(b)<=32, "string >32 bytes: extend offsets"
    return [format(len(b),'064x'), b.hex().ljust(64,'0')]
head=[addr(admin), addr(seq), w(0), w(8*32), w(10*32), w(0), w(12*32), w(1)]
tail = s2w(url) + s2w(name)
tail += [w(1), w(0x20), addr(seq), w(0x40), w(1), '20'.ljust(64,'0')]  # [(seq," ")]
print('0x697427f6' + ''.join(head+tail))
PY
)
  cast send "$ROLLUP2" "$CALLDATA" --private-key $ADMIN_KEY --rpc-url "$L1_RPC" >/dev/null || fail "aggchain init"
fi
log "  trustedSequencer: $(cast call "$ROLLUP2" 'trustedSequencer()(address)' --rpc-url "$L1_RPC")"
log "  networkName:      $(cast call "$ROLLUP2" 'networkName()(string)' --rpc-url "$L1_RPC")"

# ── Step 3: bridge contracts on L2B (extract impl from L1 snapshot) ─────────
# The L1 bridge proxy's implementation (PolygonZkEVMBridgeV2, ~13KB) is copied
# onto L2B via anvil_setCode, fronted by a fresh proxy, then initialized with
# networkID=$L2B_NETWORK_ID and the sovereign-GER address. TODO(next): the
# sovereign L2 GER contract (insertGlobalExitRoot / updateExitRoot ABI) is NOT
# on the L1 snapshot — provide it as a small vendored contract (see notes).
if ! cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1; then
  log "Step 3 SKIPPED: L2B not reachable at $L2B_RPC (bring up anvil-l2b first)"
  exit 0
fi
log "Step 3: deploying bridge on L2B ($L2B_RPC)"
IMPL_ADDR_WORD=$(cast storage $L1_BRIDGE $EIP1967_IMPL --rpc-url "$L1_RPC")
IMPL_CODE=$(cast code "0x${IMPL_ADDR_WORD:26}" --rpc-url "$L1_RPC")
[ ${#IMPL_CODE} -gt 100 ] || fail "could not extract bridge impl bytecode from L1"
BRIDGE_IMPL_L2B=0x00000000000000000000000000000000000B41d6   # arbitrary impl address
cast rpc anvil_setCode "$BRIDGE_IMPL_L2B" "$IMPL_CODE" --rpc-url "$L2B_RPC" >/dev/null
log "  impl code set at $BRIDGE_IMPL_L2B ($(( (${#IMPL_CODE}-2)/2 )) bytes)"
# Fresh ERC1967-ish proxy: set the SAME proxy bytecode as L1's bridge proxy,
# at the SAME address (0xC8cb...) for config symmetry, then point its impl slot
# at BRIDGE_IMPL_L2B and initialize with networkID=$L2B_NETWORK_ID.
PROXY_CODE=$(cast code $L1_BRIDGE --rpc-url "$L1_RPC")
cast rpc anvil_setCode "$L1_BRIDGE" "$PROXY_CODE" --rpc-url "$L2B_RPC" >/dev/null
cast rpc anvil_setStorageAt "$L1_BRIDGE" "$EIP1967_IMPL" \
  "0x000000000000000000000000${BRIDGE_IMPL_L2B:2}" --rpc-url "$L2B_RPC" >/dev/null
log "  proxy staged at $L1_BRIDGE -> $BRIDGE_IMPL_L2B"
# initialize(networkID, gasToken, gasTokenNetwork, GER, rollupManager, gasTokenMetadata)
cast send $L1_BRIDGE \
  "initialize(uint32,address,uint32,address,address,bytes)" \
  "$L2B_NETWORK_ID" 0x0000000000000000000000000000000000000000 0 \
  "$L2_GER_ADDR" 0x0000000000000000000000000000000000000000 0x \
  --private-key $ADMIN_KEY --rpc-url "$L2B_RPC" >/dev/null || fail "bridge initialize on L2B"
log "  L2B bridge networkID: $(cast call $L1_BRIDGE 'networkID()(uint32)' --rpc-url "$L2B_RPC")"
log "setup-l2b DONE (GER stub on L2B still TODO — see docs/l2-to-l2-notes.md)"
