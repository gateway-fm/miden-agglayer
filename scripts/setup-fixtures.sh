#!/usr/bin/env bash
#
# One-time script to extract L1 state, contract addresses, and keystores
# from a Kurtosis CDK deployment, then template config files for docker-compose.
#
# The Kurtosis CDK deploys L1 as Geth+Lighthouse. This script extracts the full
# Geth state via debug_dumpBlock, converts it to an Anvil genesis file, and
# extracts all needed artifacts.
#
# Usage:
#   ./scripts/setup-fixtures.sh
#
# Prerequisites:
#   - kurtosis CLI installed (v1.16+)
#   - Docker running
#   - jq, python3 installed
#   - cast (foundry) installed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
ENCLAVE="snapshot"

# Kurtosis CDK location — adjust if your checkout is elsewhere
KURTOSIS_CDK_DIR="${KURTOSIS_CDK_DIR:-$PROJECT_DIR/../aggkit-proxy/kurtosis/miden-cdk}"

# Use brew kurtosis if available (macOS: /usr/local/bin may have old version)
KURTOSIS="kurtosis"
if [[ -x "/opt/homebrew/opt/kurtosis-cli/bin/kurtosis" ]]; then
    KURTOSIS="/opt/homebrew/opt/kurtosis-cli/bin/kurtosis"
fi

log() { echo "==> $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# ── Pre-flight checks ────────────────────────────────────────────────────────

$KURTOSIS version >/dev/null 2>&1 || die "kurtosis CLI not found or not working"
command -v jq >/dev/null      || die "jq not found"
command -v cast >/dev/null    || die "cast (foundry) not found"
command -v python3 >/dev/null || die "python3 not found"
command -v docker >/dev/null  || die "docker not found"

[[ -d "$KURTOSIS_CDK_DIR" ]] || die "Kurtosis CDK dir not found at $KURTOSIS_CDK_DIR — set KURTOSIS_CDK_DIR"

mkdir -p "$FIXTURES_DIR"

# ── Step 1: Deploy Kurtosis enclave ──────────────────────────────────────────

log "Deploying Kurtosis enclave '$ENCLAVE' (this takes a few minutes)..."
cd "$KURTOSIS_CDK_DIR"

# Clean up any previous snapshot enclave
$KURTOSIS enclave rm "$ENCLAVE" --force 2>/dev/null || true

$KURTOSIS run . --enclave "$ENCLAVE" --args-file params.yaml

# ── Step 2: Extract artifacts from contracts-001 ─────────────────────────────

log "Extracting combined.json (contract addresses)..."
$KURTOSIS service exec "$ENCLAVE" contracts-001 'cat /opt/output/combined.json' 2>/dev/null \
    > "$FIXTURES_DIR/combined.json"

log "Extracting keystores..."
$KURTOSIS service exec "$ENCLAVE" contracts-001 'cat /opt/keystores/claimsponsor.keystore' 2>/dev/null \
    > "$FIXTURES_DIR/claimsponsor.keystore"
$KURTOSIS service exec "$ENCLAVE" contracts-001 'cat /opt/keystores/aggoracle.keystore' 2>/dev/null \
    > "$FIXTURES_DIR/aggoracle.keystore"
$KURTOSIS service exec "$ENCLAVE" contracts-001 'cat /opt/keystores/aggregator.keystore' 2>/dev/null \
    > "$FIXTURES_DIR/aggregator.keystore"

# ── Step 3: Parse contract addresses from combined.json ──────────────────────

COMBINED="$FIXTURES_DIR/combined.json"
[[ -s "$COMBINED" ]] || die "combined.json is empty — extraction failed"
jq -e '.polygonZkEVMBridgeAddress' "$COMBINED" >/dev/null || die "combined.json missing bridge address"

polygonZkEVMBridgeAddress=$(jq -r '.polygonZkEVMBridgeAddress' "$COMBINED")
polygonZkEVMGlobalExitRootAddress=$(jq -r '.polygonZkEVMGlobalExitRootAddress' "$COMBINED")
polygonRollupManagerAddress=$(jq -r '.polygonRollupManagerAddress' "$COMBINED")
rollupAddress=$(jq -r '.rollupAddress' "$COMBINED")
polTokenAddress=$(jq -r '.polTokenAddress' "$COMBINED")
sequencerAddress=$(jq -r '.firstBatchData.sequencer' "$COMBINED")
deploymentRollupManagerBlockNumber=$(jq -r '.deploymentRollupManagerBlockNumber' "$COMBINED")

# L2 contract addresses from combined.json (deployed by sovereign contract setup)
L2_BRIDGE_ADDRESS=$(jq -r '.polygonZkEVML2BridgeAddress // empty' "$COMBINED")
L2_GER_ADDRESS=$(jq -r '.LegacyAgglayerGERL2 // empty' "$COMBINED")

# Fallback to well-known defaults if not in combined.json
L2_BRIDGE_ADDRESS="${L2_BRIDGE_ADDRESS:-0x78908F7A87d589fdB46bdd5EfE7892C5aD6001b6}"
L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40d5f56745a118d0906a34e69aec8c0db1cb8fa}"

log "Contract addresses:"
log "  Bridge (L1):        $polygonZkEVMBridgeAddress"
log "  GER (L1):           $polygonZkEVMGlobalExitRootAddress"
log "  RollupManager:      $polygonRollupManagerAddress"
log "  Rollup:             $rollupAddress"
log "  POL Token:          $polTokenAddress"
log "  Sequencer:          $sequencerAddress"
log "  Deploy Block:       $deploymentRollupManagerBlockNumber"
log "  Bridge (L2 proxy):  $L2_BRIDGE_ADDRESS"
log "  GER (L2 proxy):     $L2_GER_ADDRESS"

# ── Step 4: Extract full L1 state and create Anvil genesis ───────────────────

log "Getting L1 RPC URL from Kurtosis..."
L1_RPC_URL="http://$($KURTOSIS port print "$ENCLAVE" el-1-geth-lighthouse rpc)"
log "L1 RPC: $L1_RPC_URL"

l1GenBlockNumber=$(cast block-number --rpc-url "$L1_RPC_URL" 2>/dev/null || echo "$deploymentRollupManagerBlockNumber")
log "L1 block number: $l1GenBlockNumber"

log "Extracting deployment transactions from Geth L1..."
python3 << PYEOF
import json, urllib.request

L1_RPC = "$L1_RPC_URL"
LATEST = int("$l1GenBlockNumber")

def rpc_call(method, params):
    data = json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}).encode()
    req = urllib.request.Request(L1_RPC, data=data, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req) as resp:
        return json.loads(resp.read()).get("result")

all_txs = []
for block_num in range(0, LATEST + 1):
    block = rpc_call("eth_getBlockByNumber", [hex(block_num), True])
    if not block or not block.get("transactions"):
        continue
    for tx in block["transactions"]:
        raw = rpc_call("eth_getRawTransactionByHash", [tx["hash"]])
        if raw:
            all_txs.append({"block": block_num, "hash": tx["hash"],
                           "from": tx.get("from", ""), "to": tx.get("to", ""), "raw": raw})

# Save full JSON (for reference)
with open("$FIXTURES_DIR/l1-transactions.json", "w") as f:
    json.dump(all_txs, f, indent=2)

# Save raw-only text (for replay script)
with open("$FIXTURES_DIR/l1-raw-txs.txt", "w") as f:
    for tx in all_txs:
        f.write(tx["raw"] + "\n")

print(f"  Extracted {len(all_txs)} transactions from {LATEST} blocks")
PYEOF

TX_COUNT=$(wc -l < "$FIXTURES_DIR/l1-raw-txs.txt" | tr -d ' ')
log "Deployment transactions: $TX_COUNT"

# ── Step 5: Template config files ────────────────────────────────────────────

KEYSTORE_PASSWORD="pSnv6Dh5s9ahuzGzH9RoCDrKAMddaX3m"

# ---- bridge-config.toml ----
log "Writing bridge-config.toml..."
cat > "$FIXTURES_DIR/bridge-config.toml" <<EOF
[Log]
Level = "info"
Environment = "development"
Outputs = ["stderr"]

[SyncDB]
Database = "postgres"
    [SyncDB.PgStorage]
    User = "bridge_user"
    Name = "bridge_db"
    Password = "bridge_password"
    Host = "postgres"
    Port = "5432"
    MaxConns = 20

[Etherman]
l1URL = "http://anvil:8545"
L2URLs = ["http://miden-agglayer:8546"]

[Synchronizer]
SyncInterval = "2s"
SyncChunkSize = 100
ForceL2SyncChunk = true

[BridgeController]
Height = 32

[BridgeServer]
GRPCPort = "9090"
HTTPPort = "8080"
DefaultPageLimit = 25
MaxPageLimit = 1000
FinalizedGEREnabled = true
    [BridgeServer.DB]
    Database = "postgres"
        [BridgeServer.DB.PgStorage]
        User = "bridge_user"
        Name = "bridge_db"
        Password = "bridge_password"
        Host = "postgres"
        Port = "5432"
        MaxConns = 20

[NetworkConfig]
L1GenBlockNumber = $l1GenBlockNumber
L2GenBlockNumbers = [0]
PolygonBridgeAddress = "$polygonZkEVMBridgeAddress"
PolygonZkEVMGlobalExitRootAddress = "$polygonZkEVMGlobalExitRootAddress"
PolygonRollupManagerAddress = "$polygonRollupManagerAddress"
PolygonZkEVMAddress = "$rollupAddress"
L2PolygonBridgeAddresses = ["$L2_BRIDGE_ADDRESS"]
RequireSovereignChainSmcs = [true]
L2PolygonZkEVMGlobalExitRootAddresses = ["$L2_GER_ADDRESS"]

[ClaimTxManager]
Enabled = true
FrequencyToMonitorTxs = "2s"
PrivateKey = {Path = "/etc/zkevm/claimsponsor.keystore", Password = "$KEYSTORE_PASSWORD"}
RetryInterval = "1s"
RetryNumber = 10

[Metrics]
Enabled = false
EOF

# ---- agglayer-config.toml ----
log "Writing agglayer-config.toml..."
cat > "$FIXTURES_DIR/agglayer-config.toml" <<EOF
debug-mode = true
mock-verifier = true

[full-node-rpcs]
1 = "http://miden-agglayer:8546"

[proof-signers]
1 = "$sequencerAddress"

[prover.mock-prover]
proving-timeout = "5m"
proving_request_timeout = "300s"

[rpc]
grpc-port = 4443
readrpc-port = 4444
host = "0.0.0.0"
request-timeout = 180
max-request-body-size = 104857600

[grpc]
max-decoding-message-size = 104857600

[outbound.rpc.settle]
max-retries = 3
retry-interval = 7
confirmations = 1
settlement-timeout = 1200
gas-multiplier-factor = 175

[log]
level = "debug"
outputs = ["stderr"]
format = "pretty"

[auth.local]
private-keys = [
    { path = "/etc/agglayer/aggregator.keystore", password = "$KEYSTORE_PASSWORD" },
]

[l1]
chain-id = 271828
node-url = "http://anvil:8545"
ws-node-url = "ws://anvil:8545"
rollup-manager-contract = "$polygonRollupManagerAddress"
polygon-zkevm-global-exit-root-v2-contract = "$polygonZkEVMGlobalExitRootAddress"
rpc-timeout = 45

[l2]
rpc-timeout = 45

[telemetry]
prometheus-addr = "0.0.0.0:9092"

[rate-limiting]
send-tx = "unlimited"
[rate-limiting.network]

[epoch.block-clock]
epoch-duration = 15
genesis-block = $deploymentRollupManagerBlockNumber

[shutdown]
runtime-timeout = 5

[certificate-orchestrator]
input-backpressure-buffer-size = 1000

[certificate-orchestrator.prover.sp1-local]

[storage]
db-path = "/etc/agglayer/storage"

[storage.backup]
path = "/etc/agglayer/backups"
state-max-backup-count = 100
pending-max-backup-count = 100
EOF

# ---- aggkit-config.toml ----
log "Writing aggkit-config.toml..."
cat > "$FIXTURES_DIR/aggkit-config.toml" <<EOF
PathRWData = "/tmp"
L1URL = "http://anvil:8545"
L2URL = "http://miden-agglayer:8546"
AggLayerURL = "agglayer:4443"
AggchainProofURL = ""
SequencerPrivateKeyPath = "/etc/aggkit/aggoracle.keystore"
SequencerPrivateKeyPassword = "$KEYSTORE_PASSWORD"

rollupCreationBlockNumber = $deploymentRollupManagerBlockNumber
rollupManagerCreationBlockNumber = $deploymentRollupManagerBlockNumber
genesisBlockNumber = $deploymentRollupManagerBlockNumber

[Log]
Level = "info"

[L1Config]
URL = "http://anvil:8545"
chainId = "271828"
polygonZkEVMGlobalExitRootAddress = "$polygonZkEVMGlobalExitRootAddress"
polygonRollupManagerAddress = "$polygonRollupManagerAddress"
polTokenAddress = "$polTokenAddress"
polygonZkEVMAddress = "$rollupAddress"
BridgeAddr = "$polygonZkEVMBridgeAddress"

[L2Config]
GlobalExitRootAddr = "$L2_GER_ADDRESS"
BridgeAddr = "$L2_BRIDGE_ADDRESS"

[AggOracle]
WaitPeriodNextGER = "5s"
EnableAggOracleCommittee = false

[AggOracle.EVMSender]
GlobalExitRootL2 = "$L2_GER_ADDRESS"
WaitPeriodMonitorTx = "5s"

[AggOracle.EVMSender.EthTxManager]
FrequencyToMonitorTxs = "1s"
WaitTxToBeMined = "2m"
GasPriceMarginFactor = 1
MaxGasPriceLimit = 0
ForcedGas = 0

[[AggOracle.EVMSender.EthTxManager.PrivateKeys]]
Path = "/etc/aggkit/aggoracle.keystore"
Password = "$KEYSTORE_PASSWORD"

[AggOracle.EVMSender.EthTxManager.Etherman]
URL = "http://miden-agglayer:8546"
L1ChainID = 2

[AggSender]
AggSenderPrivateKey = {Path = "/etc/aggkit/aggoracle.keystore", Password = "$KEYSTORE_PASSWORD"}
Mode = "PessimisticProof"
CheckStatusCertificateInterval = "1s"
TriggerCertMode = "ASAP"

[AggSender.StorageRetainCertificatesPolicy]
RetryCertAfterInError = true

[AggSender.AggkitProverClient]
UseTLS = false

[AggSender.AgglayerClient]

[[AggSender.AgglayerClient.APIRateLimits]]
MethodName = "SendCertificate"
[AggSender.AgglayerClient.APIRateLimits.RateLimit]
NumRequests = 0

[AggSender.AgglayerClient.GRPC]
URL = "agglayer:4443"
MinConnectTimeout = "5s"
RequestTimeout = "300s"
UseTLS = false

[AggSender.AgglayerClient.GRPC.Retry]
InitialBackoff = "1s"
MaxBackoff = "10s"
BackoffMultiplier = 2.0
MaxAttempts = 20

[BridgeL2Sync]
BridgeAddr = "$L2_BRIDGE_ADDRESS"
BlockFinality = "LatestBlock"
SyncFromInBridges = "false"

[ReorgDetectorL2]
FinalizedBlock = "LatestBlock"

[L1InfoTreeSync]
InitialBlock = $deploymentRollupManagerBlockNumber

[L2GERSync]
BlockFinality = "LatestBlock"
EOF

# ── Step 6: Write .env for docker-compose ────────────────────────────────────

log "Writing .env..."
cat > "$FIXTURES_DIR/.env" <<EOF
# Generated by setup-fixtures.sh — do not edit manually
BRIDGE_ADDRESS=$polygonZkEVMBridgeAddress
L2_BRIDGE_ADDRESS=$L2_BRIDGE_ADDRESS
L2_GER_ADDRESS=$L2_GER_ADDRESS
ROLLUP_ADDRESS=$rollupAddress
POL_TOKEN_ADDRESS=$polTokenAddress
SEQUENCER_ADDRESS=$sequencerAddress
DEPLOYMENT_BLOCK=$deploymentRollupManagerBlockNumber
L1_GEN_BLOCK=$l1GenBlockNumber
EOF

# ── Step 7: Teardown Kurtosis enclave ────────────────────────────────────────

log "Tearing down Kurtosis enclave '$ENCLAVE'..."
$KURTOSIS enclave rm "$ENCLAVE" --force

# ── Done ─────────────────────────────────────────────────────────────────────

log ""
log "Fixtures written to $FIXTURES_DIR/"
log "Files:"
ls -lh "$FIXTURES_DIR/"
log ""
log "Next steps:"
log "  1. Review generated configs in fixtures/"
log "  2. Run: make e2e-up"
log "  3. Run: make e2e-test"
