#!/usr/bin/env bash
# Register rollup #2 ("l2b-sovereign", chainID 31338, networkID 2) on the L1
# RollupManager and predeploy the REAL Agglayer sovereign contracts on L2B.
# Part of the L2->L2 e2e (task #25).
#
# This follows the kurtosis-cdk sovereign-chain flow (the way an OP-Stack chain
# is attached as a sovereign chain), using the SAME tooling image kurtosis uses
# (agglayer-contracts:v12.2.3) instead of hand-written stubs:
#
#   1. L1 registration = kurtosis contracts.sh create_sovereign_rollup_predeployed:
#      run deployment/v2/4_createRollup.ts (hardhat) inside the contracts image
#      against our L1 anvil. That script deploys a fresh AggchainECDSAMultisig
#      consensus implementation, calls RollupManager.addNewRollupType(...,
#      verifierType=ALGateway, genesis=0, programVKey=0), attaches the chain via
#      attachAggchainToAL(newTypeId, 31338, ...) and initializes the aggchain
#      (admin/trustedSequencer/signers/threshold). addDefaultAggchainVKey is
#      commented out exactly like kurtosis does for l2_network_id != 1 (the
#      route already exists from rollup #1 and would revert).
#      Inputs: fixtures/l2b/create_rollup_parameters.json (mirror of kurtosis
#      static_files/contracts/sovereign-rollup/create_new_rollup.json) and
#      deploy_output.json = fixtures/combined.json.
#   2. L2 predeploy = kurtosis's "predeployed contracts" model: 4_createRollup
#      with isVanillaClient=true also emits genesis_sovereign.json — the base L2
#      genesis (reproduced bit-for-bit by 1_createGenesis from
#      fixtures/l2b/deploy_parameters.json: bridge proxy 0xC8cb..., GER proxy
#      0xa40d..., BridgeLib 0xcC87...) with the REAL AgglayerBridgeL2 +
#      AgglayerGERL2 implementations swapped in and their proxy storage
#      initialized (networkID=2, globalExitRootUpdater=aggoracle, ...). On an
#      OP chain kurtosis bakes this into the chain genesis; L2B is a live anvil,
#      so we inject the same allocs via anvil_setCode/setStorageAt/setBalance/
#      setNonce — the CONTRACT CODE AND STORAGE are the real tool output either
#      way.
#   3. Verify both sides on-chain (rollupIDToRollupData(2) mirroring kurtosis's
#      sovereign-rollup-out.json, plus L2B contract state) and write
#      fixtures/l2b/out/sovereign-rollup-out.json for gen-l2b-configs.sh.
set -euo pipefail
GREEN='\033[0;32m'; NC='\033[0m'; log(){ echo -e "${GREEN}[setup-l2b]${NC} $*"; }
fail(){ echo "FAIL: $*" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
L2B_DIR="$FIXTURES_DIR/l2b"
OUT_DIR="$L2B_DIR/out"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2B_RPC="${L2B_RPC:-http://localhost:9545}"          # anvil-l2b (compose: anvil-l2b:8545)
L2B_CHAIN_ID="${L2B_CHAIN_ID:-31338}"
L2B_NETWORK_ID="${L2B_NETWORK_ID:-2}"                # agglayer network id of rollup #2

# Same image + tag kurtosis-cdk pins (src/package_io/constants.star)
CONTRACTS_IMAGE="${CONTRACTS_IMAGE:-europe-west2-docker.pkg.dev/prj-polygonlabs-devtools-dev/public/agglayer-contracts:v12.2.3}"

ROLLUP_MANAGER=0x6c6c009cC348976dB4A908c92B24433d4F6edA43
L2_BRIDGE=0xC8cbEBf950B9Df44d987c8619f092beA980fF038   # canonical L2 bridge proxy (== L1 addr)
L2_GER=0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA      # canonical L2 sovereign GER proxy

# TEST-ONLY keys (kurtosis-cdk standard; see fixtures/agglayer-config.toml warning)
ADMIN=0xE34aaF64b29273B7D567FCFc40544c014EEe9970
SEQUENCER=0x5b06837A43bdC3dD9F114558DAf4B26ed49842Ed   # committee[0]; sequencer.keystore
KEYSTORE_PW="pSnv6Dh5s9ahuzGzH9RoCDrKAMddaX3m"          # TEST-ONLY (see fixtures warning)

command -v cast    >/dev/null || fail "cast (foundry) required"
command -v docker  >/dev/null || fail "docker required"
command -v python3 >/dev/null || fail "python3 required"
cast chain-id --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 not reachable at $L1_RPC"

# The contracts container joins the L1 anvil's network namespace so the
# hardhat 'localhost' network (127.0.0.1:8545) IS our L1 anvil.
# pipefail-safe: no match -> grep exits 1 (and head can SIGPIPE grep=141); a bare
# assignment would abort the script under set -euo pipefail before the fallback.
_DETECTED_PROJECT=$( ( set +o pipefail; docker ps --format '{{.Names}}' 2>/dev/null | grep -E -- '-anvil-1$' | head -1 | sed 's/-anvil-1$//' ) || true )
L1_CONTAINER="${L1_CONTAINER:-${_DETECTED_PROJECT:-l2l2}-anvil-1}"
docker inspect "$L1_CONTAINER" >/dev/null 2>&1 || fail "L1 anvil container '$L1_CONTAINER' not found (set L1_CONTAINER=)"

mkdir -p "$OUT_DIR"

# ── #77: deploy L2B's OWN gas token on L1 (BEFORE rollup registration) ────────
# A realistic sovereign chain has its OWN gas token (not gasTokenAddress=0/ETH — that
# config makes the sovereign bridge revert native-ETH bridging, finding #77). The gas
# token is an L1-origin ERC-20; `1_createGenesis.ts` reads its name/symbol/decimals off
# L1 to stamp the bridge's `gasTokenMetadata`, so a later native/gas-token bridge-out
# carries the REAL (name,symbol,decimals) to Miden. gasTokenAddress in
# create_rollup_parameters.json pins the deterministic (fixed key @ nonce 0) address
# below; this deploy must produce exactly that address.
GAS_TOKEN_DEPLOYER_KEY="${GAS_TOKEN_DEPLOYER_KEY:-0x7777777777777777777777777777777777777777777777777777777777777777}"
GAS_TOKEN_DEPLOYER=$(cast wallet address --private-key "$GAS_TOKEN_DEPLOYER_KEY")
GAS_TOKEN_ADDR=$(cast compute-address "$GAS_TOKEN_DEPLOYER" --nonce 0 | awk '{print $NF}')
L2B_GAS_SYMBOL="${L2B_GAS_SYMBOL:-L2BGAS}"
L2B_GAS_DECIMALS="${L2B_GAS_DECIMALS:-18}"
if [ "$(cast code "$GAS_TOKEN_ADDR" --rpc-url "$L1_RPC" 2>/dev/null || echo 0x)" = "0x" ]; then
  cast rpc anvil_setBalance "$GAS_TOKEN_DEPLOYER" 0x21e19e0c9bab2400000 --rpc-url "$L1_RPC" >/dev/null 2>&1 || true
  _GT_OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" --rpc-url "$L1_RPC" \
      --private-key "$GAS_TOKEN_DEPLOYER_KEY" --broadcast \
      --constructor-args "L2B Gas" "$L2B_GAS_SYMBOL" "$L2B_GAS_DECIMALS" 1000000000000000000000000 2>&1) || true
  _GT_DEPLOYED=$(echo "$_GT_OUT" | awk '/Deployed to:/{print $NF}')
  [ "$(echo "$_GT_DEPLOYED" | tr 'A-F' 'a-f')" = "$(echo "$GAS_TOKEN_ADDR" | tr 'A-F' 'a-f')" ] \
    || fail "#77: gas token deployed to '${_GT_DEPLOYED:-<none>}', expected deterministic $GAS_TOKEN_ADDR (deployer nonce not 0?): $(echo "$_GT_OUT" | tail -2)"
  log "#77: L2B gas token deployed on L1: $GAS_TOKEN_ADDR ($L2B_GAS_SYMBOL, $L2B_GAS_DECIMALS decimals)"
else
  log "#77: L2B gas token already on L1 at $GAS_TOKEN_ADDR"
fi
# Sanity: create_rollup_parameters.json must pin this exact address.
_CRP_GAS=$(python3 -c "import json;print(json.load(open('$L2B_DIR/create_rollup_parameters.json'))['gasTokenAddress'])")
[ "$(echo "$_CRP_GAS" | tr 'A-F' 'a-f')" = "$(echo "$GAS_TOKEN_ADDR" | tr 'A-F' 'a-f')" ] \
  || fail "#77: create_rollup_parameters.json gasTokenAddress=$_CRP_GAS != deployed $GAS_TOKEN_ADDR"
# Export for the l2l2 lib / e2e legs (the gas-token symbol/decimals the Miden faucet must resolve).
{ echo "L2B_GAS_TOKEN_ADDR=$GAS_TOKEN_ADDR"; echo "L2B_GAS_SYMBOL=$L2B_GAS_SYMBOL"; echo "L2B_GAS_DECIMALS=$L2B_GAS_DECIMALS"; echo "GAS_TOKEN_DEPLOYER_KEY=$GAS_TOKEN_DEPLOYER_KEY"; } > "$L2B_DIR/gas_token.env"

# ── Step 1: L1 registration via the real kurtosis tooling ────────────────────
EXISTING_ID=$(cast call $ROLLUP_MANAGER 'chainIDToRollupID(uint64)(uint32)' "$L2B_CHAIN_ID" --rpc-url "$L1_RPC")
if [ "$EXISTING_ID" != "0" ]; then
  log "chainID $L2B_CHAIN_ID already registered as rollupID $EXISTING_ID — skipping 4_createRollup"
  [ "$EXISTING_ID" = "$L2B_NETWORK_ID" ] || fail "chainID $L2B_CHAIN_ID maps to rollupID $EXISTING_ID, expected $L2B_NETWORK_ID"
else
  log "Step 1: 4_createRollup.ts (agglayer-contracts image) — addNewRollupType + attachAggchainToAL + aggchain init"
  docker run --rm -u 0 --network "container:$L1_CONTAINER" --entrypoint bash \
    -v "$L2B_DIR/deploy_parameters.json:/inputs/deploy_parameters.json:ro" \
    -v "$L2B_DIR/create_rollup_parameters.json:/inputs/create_rollup_parameters.json:ro" \
    -v "$FIXTURES_DIR/combined.json:/inputs/combined.json:ro" \
    -v "$OUT_DIR:/out" \
    "$CONTRACTS_IMAGE" -c '
set -euo pipefail
cd /opt/agglayer-contracts
cp /inputs/deploy_parameters.json        deployment/v2/deploy_parameters.json
cp /inputs/create_rollup_parameters.json deployment/v2/create_rollup_parameters.json
cp /inputs/combined.json                 deployment/v2/deploy_output.json
# Base L2 genesis — offline, exactly like kurtosis contracts.sh _create_genesis.
# (The MNEMONIC is the kurtosis-cdk test mnemonic; only used to derive the
# in-simulation deployer accounts, nothing is sent to a chain.)
MNEMONIC="giant issue aisle success illegal bike spike question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy very lucky have athlete" \
  npx ts-node deployment/v2/1_createGenesis.ts >/out/01_create_genesis.out 2>&1 \
  || { tail -30 /out/01_create_genesis.out; exit 1; }
# Second rollup: the default aggchain vkey route exists from rollup #1 — kurtosis
# comments the call out for l2_network_id != 1 (contracts.sh, same sed).
sed -i "/await aggLayerGateway\.addDefaultAggchainVKey(/,/);/s/^/\/\/ /" deployment/v2/4_createRollup.ts
npx hardhat run deployment/v2/4_createRollup.ts --network localhost >/out/05_create_sovereign_rollup.out 2>&1 \
  || { tail -40 /out/05_create_sovereign_rollup.out; exit 1; }
cp deployment/v2/genesis.json /out/genesis-base.json
cp deployment/v2/genesis_sovereign.json /out/genesis-l2b-sovereign.json
cp deployment/v2/create_rollup_output_*.json /out/create_rollup_output.json
chmod -R a+rw /out
' || fail "containerized 4_createRollup failed (see $OUT_DIR/*.out)"
  log "  create_rollup_output: $(python3 -c "import json;d=json.load(open('$OUT_DIR/create_rollup_output.json'));print('rollupAddress=%s consensus=%s'%(d.get('rollupAddress'),d.get('consensusContract')))" 2>/dev/null || echo '<unparsed>')"
fi

# ── Step 1b: verify registration + write sovereign-rollup-out.json ──────────
# Mirrors kurtosis contracts.sh: cast call rollupIDToRollupData | jq '{...}'.
RD=$(cast call --json --rpc-url "$L1_RPC" $ROLLUP_MANAGER \
  'rollupIDToRollupData(uint32)(address,uint64,address,uint64,bytes32,uint64,uint64,uint64,uint64,uint64,uint64,uint8)' \
  "$L2B_NETWORK_ID")
echo "$RD" | python3 -c '
import json, sys
d = json.load(sys.stdin)
keys = ["sovereignRollupContract","rollupChainID","verifier","forkID","lastLocalExitRoot",
        "lastBatchSequenced","lastVerifiedBatch","_legacyLastPendingState",
        "_legacyLastPendingStateConsolidated","lastVerifiedBatchBeforeUpgrade",
        "rollupTypeID","rollupVerifierType"]
json.dump(dict(zip(keys, d)), open(sys.argv[1], "w"), indent=4)
' "$OUT_DIR/sovereign-rollup-out.json"
ROLLUP2=$(python3 -c "import json;print(json.load(open('$OUT_DIR/sovereign-rollup-out.json'))['sovereignRollupContract'])")
RCHAIN=$(python3 -c "import json;print(json.load(open('$OUT_DIR/sovereign-rollup-out.json'))['rollupChainID'])")
RVTYPE=$(python3 -c "import json;print(json.load(open('$OUT_DIR/sovereign-rollup-out.json'))['rollupVerifierType'])")
log "rollupIDToRollupData($L2B_NETWORK_ID): aggchain=$ROLLUP2 chainID=$RCHAIN verifierType=$RVTYPE"
[ "$RCHAIN" = "$L2B_CHAIN_ID" ] || fail "rollup #2 chainID=$RCHAIN, expected $L2B_CHAIN_ID"
[ "$RVTYPE" = "2" ] || fail "rollup #2 rollupVerifierType=$RVTYPE, expected 2 (ALGateway)"
# The generated configs (gen-l2b-configs.sh) pin this snapshot-deterministic
# address — fail loudly if the chain state diverged from the expectation.
ROLLUP2_EXPECTED="${ROLLUP2_ADDR:-0x5D1A491A416feEbf8C123A558ec28A239960bd0E}"
[ "$ROLLUP2" = "$ROLLUP2_EXPECTED" ] || \
  fail "rollup #2 at $ROLLUP2 but configs expect $ROLLUP2_EXPECTED — re-run scripts/gen-l2b-configs.sh with ROLLUP2_ADDR=$ROLLUP2 and restart aggkit-l2b"
log "  trustedSequencer: $(cast call "$ROLLUP2" 'trustedSequencer()(address)' --rpc-url "$L1_RPC")"
log "  networkName:      $(cast call "$ROLLUP2" 'networkName()(string)' --rpc-url "$L1_RPC")"
# L1 traceability: attachAggchainToAL MUST have emitted the RollupManager's
# CreateNewAggchain(rollupID indexed, ...) event for rollupID 2.
CREATE_AGGCHAIN_TOPIC=0x144e3f9b5c63682a3bb7e9ad31e99c043890d3d540cd79dcebc3b5bdfba94c9b
RID_TOPIC=0x$(printf '%064x' "$L2B_NETWORK_ID")
ATTACH_LOGS=$(cast rpc --raw eth_getLogs "[{\"fromBlock\":\"0x0\",\"toBlock\":\"latest\",\"address\":\"$ROLLUP_MANAGER\",\"topics\":[\"$CREATE_AGGCHAIN_TOPIC\",\"$RID_TOPIC\"]}]" --rpc-url "$L1_RPC")
ATTACH_TX=$(echo "$ATTACH_LOGS" | python3 -c "
import json, sys
logs = json.load(sys.stdin)
if logs: print(logs[-1]['transactionHash'])
")
[ -n "$ATTACH_TX" ] || fail "no CreateNewAggchain(rollupID=$L2B_NETWORK_ID) event on the RollupManager — attach not traceable on L1"
log "  L1 attach tx (CreateNewAggchain rollupID $L2B_NETWORK_ID): $ATTACH_TX"

# ── Step 2: predeploy the sovereign genesis on L2B ───────────────────────────
if ! cast chain-id --rpc-url "$L2B_RPC" >/dev/null 2>&1; then
  log "Step 2 SKIPPED: L2B not reachable at $L2B_RPC (bring up anvil-l2b first)"
  exit 0
fi
GENESIS_FILE="$OUT_DIR/genesis-l2b-sovereign.json"
[ -s "$GENESIS_FILE" ] || fail "$GENESIS_FILE missing — remove the rollup-2 registration (fresh L1) and re-run, or restore the file"
if [ "$(cast call $L2_GER 'globalExitRootUpdater()(address)' --rpc-url "$L2B_RPC" 2>/dev/null || echo '')" != "" ]; then
  log "Step 2: L2B sovereign genesis already injected — skipping"
else
  log "Step 2: injecting sovereign genesis into L2B (anvil_setCode/Storage/Balance/Nonce)"
  python3 - "$GENESIS_FILE" "$L2B_RPC" <<'PY'
import json, sys, urllib.request
gen_path, rpc = sys.argv[1], sys.argv[2]

def call(method, params):
    req = urllib.request.Request(rpc, json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"Content-Type": "application/json"})
    resp = json.load(urllib.request.urlopen(req, timeout=10))
    if "error" in resp:
        raise SystemExit(f"{method}{params[:1]}: {resp['error']}")
    return resp.get("result")

accounts = json.load(open(gen_path))["genesis"]
n_code = n_slot = 0
for acc in accounts:
    name = acc.get("contractName") or acc.get("accountName") or "?"
    addr = acc["address"]
    if acc.get("bytecode"):
        call("anvil_setCode", [addr, acc["bytecode"]])
        n_code += 1
    for slot, value in (acc.get("storage") or {}).items():
        call("anvil_setStorageAt", [addr, slot, "0x" + value[2:].rjust(64, "0")])
        n_slot += 1
    if acc.get("balance") and int(acc["balance"]) > 0:
        call("anvil_setBalance", [addr, hex(int(acc["balance"]))])
    if acc.get("nonce") and int(acc["nonce"]) > 0:
        call("anvil_setNonce", [addr, hex(int(acc["nonce"]))])
    print(f"  {name}: {addr} code={bool(acc.get('bytecode'))} slots={len(acc.get('storage') or {})}")
print(f"injected {n_code} code blobs, {n_slot} storage slots")
PY
fi

# ── Step 2b: verify the real contracts on L2B ────────────────────────────────
GER_UPDATER=$(cast call $L2_GER 'globalExitRootUpdater()(address)' --rpc-url "$L2B_RPC")
AGGORACLE_ADDR=$(cast wallet address --keystore "$FIXTURES_DIR/aggoracle.keystore" \
  --password "$KEYSTORE_PW" 2>/dev/null) || fail "cannot derive aggoracle address"
[ "$GER_UPDATER" = "$AGGORACLE_ADDR" ] || fail "GER globalExitRootUpdater=$GER_UPDATER, expected aggoracle $AGGORACLE_ADDR"
BRIDGE_NETID=$(cast call $L2_BRIDGE 'networkID()(uint32)' --rpc-url "$L2B_RPC")
[ "$BRIDGE_NETID" = "$L2B_NETWORK_ID" ] || fail "L2B bridge networkID=$BRIDGE_NETID, expected $L2B_NETWORK_ID"
BRIDGE_GER=$(cast call $L2_BRIDGE 'globalExitRootManager()(address)' --rpc-url "$L2B_RPC")
[ "$(echo "$BRIDGE_GER" | tr 'A-F' 'a-f')" = "$(echo "$L2_GER" | tr 'A-F' 'a-f')" ] \
  || fail "L2B bridge globalExitRootManager=$BRIDGE_GER, expected $L2_GER"
log "  L2B AgglayerGERL2 proxy $L2_GER: globalExitRootUpdater=$GER_UPDATER"
log "  L2B AgglayerBridgeL2 proxy $L2_BRIDGE: networkID=$BRIDGE_NETID GER=$BRIDGE_GER"

# ── Step 3: fund operational accounts on L2B ─────────────────────────────────
log "Step 3: funding admin/sequencer/aggoracle on L2B"
for A in $ADMIN $SEQUENCER "$AGGORACLE_ADDR"; do
  cast rpc anvil_setBalance "$A" 0x21e19e0c9bab2400000 --rpc-url "$L2B_RPC" >/dev/null
done
log "setup-l2b DONE — rollup #2 registered (real kurtosis flow) + real sovereign contracts live on L2B"
