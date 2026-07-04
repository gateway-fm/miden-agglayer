#!/usr/bin/env bash
# Cantina #13 Layer-2 E2E — ERC-20 bridge-out metadata RECOVERY, VALIDATION and
# fail-safe GATE.
#
# Layer 1 (PR #90) persists the raw ABI metadata preimage
# `abi.encode(name, symbol, decimals)` on every faucet_registry row at faucet
# auto-creation and threads it into the synthetic BridgeEvent. But rows written
# BEFORE Layer 1 — and any registry rebuilt after a DB loss — carry EMPTY
# metadata. Layer 2 (src/metadata_recovery.rs) recovers the preimage from
# authoritative on-chain state (Miden faucet account, then the L1 ERC-20
# contract via --l1-rpc-url) and accepts a candidate ONLY when its keccak256
# equals the bridge account's stored MetadataHash. Unrecoverable → the
# bridge-out is DEFERRED (no BridgeEvent, note not marked processed, metric
# bridge_out_metadata_unrecoverable_total fires) — never emit empty/unvalidated
# ERC-20 metadata.
#
# Phases:
#   A. POSITIVE — deploy an ERC-20 whose name != symbol (so the all-Miden
#      candidate CANNOT match and recovery must come from L1), bridge L1→L2 so
#      the faucet auto-creates WITH metadata, then simulate the legacy/DB-loss
#      row by blanking `faucet_registry.metadata` directly in Postgres. Bridge
#      L2→L1 and assert the synthetic BridgeEvent is emitted with the CORRECT
#      recovered metadata (decoded from eth_getLogs), and that the validated
#      preimage was backfilled into Postgres (one-time self-heal).
#   B. NEGATIVE — second token (name != symbol again); blank the metadata AND
#      repoint origin_address at a non-contract L1 address so BOTH candidates
#      fail keccak validation. Assert: NO BridgeEvent, the defer warn fires,
#      bridge_out_metadata_unrecoverable_total increments, and the note keeps
#      being deferred (still no event a tick later). Then simulate operator
#      remediation (backfill origin_address) and assert the live path still
#      refuses to emit (documented recovery is via --restore; see
#      e2e-restore.sh).
#   (Self-target gate — B2AGG whose dest_network == local network id — is a
#    restore/replay defense that a live bridge-out cannot reach: a self-targeted
#    B2AGG is never consumed on-chain. It is covered by the unit test
#    cantina13_self_target_b2agg_is_gated_in_projection, not by this e2e.)
#
# NOTE on the Postgres surgery: resolve_faucet_origin() reads
# store.get_faucet_by_id() on every consumed B2AGG note — the faucet row is NOT
# cached in the proxy — so a direct UPDATE takes effect on the next sync tick
# without a proxy restart.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

# shellcheck source=/dev/null
source "$FIXTURES_DIR/.env"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
BRIDGE_SERVICE_URL="http://localhost:18080"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
PG_CONTAINER="${PG_CONTAINER:-${COMPOSE_PROJECT_NAME}-agglayer-postgres-1}"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1                       # Miden network id (L1→L2 destination)
LOCAL_NETWORK_ID="${NETWORK_ID:-1}"  # proxy's own network id (self-target probe)

# 18 origin decimals → miden_decimals=8 → scale=10 (same rationale as
# e2e-dynamic-erc20.sh). Bridge 0.001 tokens = 10^15 wei → 100_000 Miden units.
TOKEN_DECIMALS=18
TOKEN_INITIAL_SUPPLY="1000000000000000000000000"
BRIDGE_AMOUNT="1000000000000000"
WEI_PER_MIDEN_UNIT=10000000000
EXPECTED_L2_BALANCE=$((BRIDGE_AMOUNT / WEI_PER_MIDEN_UNIT))

BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
# A funded-on-anvil EOA (definitely NO contract code) used to make the L1
# recovery candidate unfetchable in phase B. eth_call name() on it returns 0x,
# alloy's abi_decode fails, and with name != symbol the Miden candidate already
# fails the keccak gate → Unrecoverable.
NON_CONTRACT_ADDR="000000000000000000000000000000000000dead"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    # pipefail dropped inside the probe: `docker logs | grep -q` otherwise trips
    # on grep's early-exit SIGPIPE (see e2e-dynamic-erc20.sh for the full story).
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

# psql one-liner against the proxy's store DB (docker-compose.e2e.yml
# `agglayer-postgres` service: user/db from its environment block).
pg() {
    docker exec "$PG_CONTAINER" psql -U agglayer -d agglayer_store -tA -c "$1"
}

# Current L2 block number (decimal).
l2_block_number() {
    curl -sf "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c "import json,sys; print(int(json.load(sys.stdin)['result'],16))"
}

# Scrape one counter from the proxy's Prometheus endpoint; absent counter → 0.
proxy_metric() {
    local name="$1"
    curl -sf "$L2_RPC/metrics" 2>/dev/null \
        | awk -v m="$name" '$1 == m {print $2; found=1} END {if (!found) print 0}'
}

# find_bridge_event <from_block_dec> <origin_addr_0x> [<dest_network>]
# eth_getLogs for BridgeEvent since <from_block_dec>, ABI-decodes each log's
# data (all 8 fields are non-indexed — see src/log_synthesis.rs / src/exit.rs)
# and prints "0x<metadata_hex>" for the FIRST event whose originAddress matches
# (and, if given, whose destinationNetwork matches). Prints nothing if no match.
find_bridge_event() {
    local from_block="$1" origin_addr="$2" dest_net="${3:-}"
    local from_hex
    from_hex=$(printf '0x%x' "$from_block")
    curl -sf "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_getLogs","params":[{"fromBlock":"'"$from_hex"'","toBlock":"latest","topics":["'"$BRIDGE_EVENT_TOPIC"'"]}],"id":1}' \
        | ORIGIN_ADDR="$origin_addr" DEST_NET="$dest_net" python3 -c '
import json, os, sys

want_origin = os.environ["ORIGIN_ADDR"].lower().replace("0x", "")
want_dest = os.environ.get("DEST_NET", "")
resp = json.load(sys.stdin)
for entry in resp.get("result", []) or []:
    data = bytes.fromhex(entry["data"][2:])
    # BridgeEvent(uint8 leafType, uint32 originNetwork, address originAddress,
    #             uint32 destinationNetwork, address destinationAddress,
    #             uint256 amount, bytes metadata, uint32 depositCount)
    origin_address = data[2 * 32 + 12 : 3 * 32].hex()
    destination_network = int.from_bytes(data[3 * 32 : 4 * 32], "big")
    if origin_address != want_origin:
        continue
    if want_dest and destination_network != int(want_dest):
        continue
    meta_off = int.from_bytes(data[6 * 32 : 7 * 32], "big")
    meta_len = int.from_bytes(data[meta_off : meta_off + 32], "big")
    metadata = data[meta_off + 32 : meta_off + 32 + meta_len]
    print("0x" + metadata.hex())
    break
'
}

lc() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || fail "cast (foundry) not found"
command -v forge >/dev/null || fail "forge (foundry) not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
wait_for "L2 proxy healthy" \
    "curl -sf '$L2_RPC' -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}' >/dev/null" \
    60 3
docker exec "$PG_CONTAINER" true 2>/dev/null || fail "Postgres container $PG_CONTAINER not running"

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY — run scripts/ensure-e2e-secrets.sh}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
: "${BRIDGE_ADDRESS:?fixtures/.env must define BRIDGE_ADDRESS}"

ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
# The bridge + ETH faucet are the proxy's global accounts (shared on the node);
# the bridge-out WALLET is a fresh INDEPENDENT wallet in its OWN sqlite store —
# the proxy's store has a single owner (see lib-isolated-wallet.sh / policy).
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-cantina13}"
B2AGG_FRESH="${B2AGG_FRESH:-1}"   # fresh wallet each run — this test funds it itself
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH"   # sets WALLET_ID / WALLET_HEX / DEST_ADDR

log "======================================================================"
log "  Cantina #13 Layer-2 Metadata Recovery E2E"
log "======================================================================"
log "Wallet:  $WALLET_ID ($WALLET_HEX)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"

# deploy_and_bridge_in <name> <symbol>
# Deploys a TestToken, bridges BRIDGE_AMOUNT L1→L2, waits for claim + faucet
# auto-creation + wallet balance. Sets: TOKEN_ADDR, FAUCET_ID, BALANCE.
deploy_and_bridge_in() {
    local token_name="$1" token_symbol="$2"
    local phase_start deploy_out bal_out attempt
    phase_start=$(date -u +%Y-%m-%dT%H:%M:%SZ)

    log "Deploying TestToken '$token_name' ($token_symbol, $TOKEN_DECIMALS decimals) on Anvil..."
    deploy_out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
        --rpc-url "$L1_RPC" \
        --private-key "$FUNDED_KEY" \
        --broadcast \
        --constructor-args "$token_name" "$token_symbol" "$TOKEN_DECIMALS" "$TOKEN_INITIAL_SUPPLY" 2>&1)
    TOKEN_ADDR=$(echo "$deploy_out" | grep "Deployed to:" | awk '{print $NF}')
    [[ -z "$TOKEN_ADDR" ]] && fail "Failed to deploy TestToken: $deploy_out"
    pass "$token_symbol deployed at $TOKEN_ADDR"

    cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
        "$TOKEN_ADDR" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$BRIDGE_AMOUNT" \
        >/dev/null 2>&1 || fail "approve failed"

    local tx status
    tx=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$BRIDGE_AMOUNT" "$TOKEN_ADDR" true 0x 2>&1)
    status=$(printf '%s\n' "$tx" | awk '$1=="status"{print $2; exit}')
    [[ "$status" == "1" ]] || fail "L1 bridgeAsset failed (status=$status): $tx"
    pass "$token_symbol bridged on L1"

    # Filter deposits by THIS token's origin address — earlier tokens from this
    # script (or the wider suite) have deposits on the same wallet address.
    local want_addr
    want_addr=$(lc "$TOKEN_ADDR")
    wait_for "$token_symbol deposit ready_for_claim" \
        "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and (dep.get('orig_addr') or '').lower()=='$want_addr' for dep in d['deposits']) else 1)\"" \
        180 5

    wait_for "$token_symbol faucet auto-creation" \
        "docker logs --since $phase_start $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
        180 5
    wait_for "$token_symbol claim committed" \
        "docker logs --since $phase_start $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
        120 5
    pass "$token_symbol claimed on L2 (faucet auto-created)"

    # Resolve the faucet by origin_address (unique per deployed token; symbol
    # alone could collide across suite runs).
    FAUCET_ID=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
        -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
        | WANT_ADDR="$want_addr" python3 -c '
import json, os, sys
want = os.environ["WANT_ADDR"]
for f in json.load(sys.stdin).get("result", []):
    if f.get("origin_address", "").lower() == want:
        print(f["faucet_id"])
        break
')
    [[ -z "$FAUCET_ID" ]] && fail "$token_symbol faucet not found in admin_listFaucets"
    pass "$token_symbol faucet: $FAUCET_ID"

    BALANCE=0
    for attempt in $(seq 1 15); do
        sleep 10
        BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID" || true)
        log "Attempt $attempt/15: $token_symbol balance = ${BALANCE:-0}"
        [[ -n "$BALANCE" && "$BALANCE" != "0" ]] && break
    done
    [[ -z "$BALANCE" || "$BALANCE" == "0" ]] && fail "$token_symbol L2 balance still 0"
    [[ "$BALANCE" -ne "$EXPECTED_L2_BALANCE" ]] && fail "$token_symbol balance mismatch: got $BALANCE, expected $EXPECTED_L2_BALANCE"
    pass "$token_symbol L2 balance verified: $BALANCE Miden units"
}

# bridge_out <faucet_id> <amount> <dest_network>
bridge_out() {
    iso_tool \
        --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$1" \
        --amount "$2" --dest-address "$FUNDED_ADDR" --dest-network "$3" 2>&1 \
        || fail "bridge-out-tool failed (faucet=$1 amount=$2 dest_network=$3)"
}

# ══════════════════════════════════════════════════════════════════════════════
# Phase A — POSITIVE: legacy empty-metadata row is recovered from L1, validated
# against the bridge's keccak, emitted, and backfilled (self-heal).
#
# name ("Recovery Test Token") != symbol ("RCVT") is deliberate: the Miden
# faucet stores token_name == sanitised symbol, so the all-Miden candidate's
# keccak CANNOT match and recovery must take the --l1-rpc-url path — the
# fullest Layer-2 chain (bridge-hash read → Miden candidate reject → L1 fetch →
# keccak accept).
# ══════════════════════════════════════════════════════════════════════════════
log "───────────────────── Phase A: positive recovery ─────────────────────"
A_NAME="Recovery Test Token"; A_SYMBOL="RCVT"
deploy_and_bridge_in "$A_NAME" "$A_SYMBOL"
A_TOKEN_ADDR="$TOKEN_ADDR"; A_FAUCET_ID="$FAUCET_ID"; A_BALANCE="$BALANCE"

EXPECTED_METADATA=$(cast abi-encode 'f(string,string,uint8)' "$A_NAME" "$A_SYMBOL" "$TOKEN_DECIMALS")
EXPECTED_METADATA=$(lc "$EXPECTED_METADATA")

# Layer 1 must have persisted the real preimage at auto-creation — otherwise we
# would not be testing recovery of a *legacy* row but masking a Layer-1 break.
STORED_HEX=$(pg "SELECT encode(metadata,'hex') FROM faucet_registry WHERE faucet_id = '$A_FAUCET_ID'")
[[ "0x$(lc "$STORED_HEX")" == "$EXPECTED_METADATA" ]] \
    || fail "Layer-1 precondition: stored metadata ('$STORED_HEX') != abi.encode($A_NAME,$A_SYMBOL,$TOKEN_DECIMALS)"
pass "Layer-1 precondition: faucet row carries the real ABI preimage"

# Simulate the legacy / DB-loss row the fix targets: blank the stored preimage.
# (This is exactly what migrations/008_faucet_metadata.sql leaves behind for
# pre-Layer-1 rows: metadata = ''::bytea. The register_faucet no-clobber upsert
# can't do this — only direct surgery or a legacy row reaches this state.)
UPDATED=$(pg "UPDATE faucet_registry SET metadata = ''::bytea WHERE faucet_id = '$A_FAUCET_ID' RETURNING faucet_id")
[[ -z "$UPDATED" ]] && fail "Postgres UPDATE matched no faucet row for $A_FAUCET_ID"
[[ "$(pg "SELECT octet_length(metadata) FROM faucet_registry WHERE faucet_id = '$A_FAUCET_ID'")" == "0" ]] \
    || fail "metadata not blanked"
pass "Simulated legacy row: faucet_registry.metadata blanked for $A_FAUCET_ID"

A_FROM_BLOCK=$(l2_block_number)
A_PHASE_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
A_OUT_AMOUNT=$((A_BALANCE / 2))
log "Bridging $A_OUT_AMOUNT $A_SYMBOL Miden units L2→L1 (metadata must be recovered)..."
bridge_out "$A_FAUCET_ID" "$A_OUT_AMOUNT" 0
pass "B2AGG note created for $A_SYMBOL"

# The recovery + one-time self-heal must actually run (not the happy path).
wait_for "Layer-2 recovery + backfill log" \
    "docker logs --since $A_PHASE_TS $AGGLAYER_CONTAINER 2>&1 | grep -q 'recovered + backfilled ERC-20 metadata'" \
    120 5
pass "Recovery path executed (recovered + backfilled log present)"

wait_for "BridgeEvent emitted for $A_SYMBOL" \
    "[[ -n \"\$(find_bridge_event $A_FROM_BLOCK $A_TOKEN_ADDR)\" ]]" \
    120 5
GOT_METADATA=$(find_bridge_event "$A_FROM_BLOCK" "$A_TOKEN_ADDR")
log "BridgeEvent metadata: got      $GOT_METADATA"
log "                      expected $EXPECTED_METADATA"
[[ "$GOT_METADATA" == "0x" || -z "$GOT_METADATA" ]] && fail \
    "Cantina #13 L2: BridgeEvent has EMPTY metadata — recovery emitted a blank passthrough"
[[ "$(lc "$GOT_METADATA")" == "$EXPECTED_METADATA" ]] || fail \
    "Cantina #13 L2: BridgeEvent metadata mismatch — recovered bytes are wrong"
pass "BridgeEvent carries the RECOVERED, keccak-validated metadata"

# One-time self-heal: the validated preimage must be back in the registry.
HEALED_HEX=$(pg "SELECT encode(metadata,'hex') FROM faucet_registry WHERE faucet_id = '$A_FAUCET_ID'")
[[ "0x$(lc "$HEALED_HEX")" == "$EXPECTED_METADATA" ]] \
    || fail "self-heal backfill missing: registry metadata is '$HEALED_HEX'"
pass "Registry self-healed: recovered preimage backfilled into faucet_registry"

# ══════════════════════════════════════════════════════════════════════════════
# Phase B — NEGATIVE: recovery impossible → fail-safe gate (defer, no event),
# then operator remediation un-wedges the deferred bridge-out.
#
# Both candidates are made invalid: name != symbol kills the Miden candidate
# (keccak mismatch), and origin_address repointed at an EOA kills the L1
# candidate (eth_call name() on a non-contract returns 0x → decode error).
# The bridge's stored MetadataHash (keyed by faucet_id, untouched) still
# exists, so the gate is exercised at the keccak-validation stage — not short-
# circuited by a missing hash.
# ══════════════════════════════════════════════════════════════════════════════
log "───────────────────── Phase B: fail-safe gate ─────────────────────"
B_NAME="Bad Recovery Token"; B_SYMBOL="BADRT"
deploy_and_bridge_in "$B_NAME" "$B_SYMBOL"
B_TOKEN_ADDR="$TOKEN_ADDR"; B_FAUCET_ID="$FAUCET_ID"; B_BALANCE="$BALANCE"

B_EXPECTED_METADATA=$(cast abi-encode 'f(string,string,uint8)' "$B_NAME" "$B_SYMBOL" "$TOKEN_DECIMALS")
B_EXPECTED_METADATA=$(lc "$B_EXPECTED_METADATA")

UPDATED=$(pg "UPDATE faucet_registry
              SET metadata = ''::bytea,
                  origin_address = decode('$NON_CONTRACT_ADDR','hex')
              WHERE faucet_id = '$B_FAUCET_ID' RETURNING faucet_id")
[[ -z "$UPDATED" ]] && fail "Postgres UPDATE matched no faucet row for $B_FAUCET_ID"
pass "Simulated unrecoverable row: metadata blanked + origin_address → EOA 0x$NON_CONTRACT_ADDR"

UNRECOVERABLE_BEFORE=$(proxy_metric bridge_out_metadata_unrecoverable_total)
B_FROM_BLOCK=$(l2_block_number)
B_PHASE_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
B_OUT_AMOUNT=$((B_BALANCE / 2))
log "Bridging $B_OUT_AMOUNT $B_SYMBOL Miden units L2→L1 (must be GATED)..."
bridge_out "$B_FAUCET_ID" "$B_OUT_AMOUNT" 0

wait_for "fail-safe defer warn" \
    "docker logs --since $B_PHASE_TS $AGGLAYER_CONTAINER 2>&1 | grep -q 'could not be recovered + validated'" \
    120 5
pass "Defer warn fired (empty metadata refused, bridge-out gated)"

UNRECOVERABLE_AFTER=$(proxy_metric bridge_out_metadata_unrecoverable_total)
[[ "$UNRECOVERABLE_AFTER" -gt "$UNRECOVERABLE_BEFORE" ]] \
    || fail "bridge_out_metadata_unrecoverable_total did not increment ($UNRECOVERABLE_BEFORE → $UNRECOVERABLE_AFTER)"
pass "bridge_out_metadata_unrecoverable_total: $UNRECOVERABLE_BEFORE → $UNRECOVERABLE_AFTER"

# NO BridgeEvent may exist for this bridge-out — neither under the corrupted
# origin (what an emit would carry) nor under the real token address.
[[ -z "$(find_bridge_event "$B_FROM_BLOCK" "0x$NON_CONTRACT_ADDR")" ]] \
    || fail "GATE BREACH: BridgeEvent emitted with the corrupted origin address"
[[ -z "$(find_bridge_event "$B_FROM_BLOCK" "$B_TOKEN_ADDR")" ]] \
    || fail "GATE BREACH: BridgeEvent emitted for $B_SYMBOL despite unrecoverable metadata"
# The deferred note re-surfaces every ~5s sync tick; give it two more ticks and
# re-assert the gate held (a defer, not a delay-then-emit).
sleep 12
[[ -z "$(find_bridge_event "$B_FROM_BLOCK" "0x$NON_CONTRACT_ADDR")" ]] \
    || fail "GATE BREACH: BridgeEvent appeared on a later sync tick"
pass "No BridgeEvent emitted while unrecoverable (gate holds across sync ticks)"

# Remediation is operator-driven and DOCUMENTED, not live-automatic: the defer
# WARN instructs "Backfill the faucet registry ... then re-run restore". By
# design the live projector does NOT re-attempt a deferred note every tick
# (the reconciler's swept-cache bounds RPC retries on genuinely-dead tokens),
# so recovery-after-fix happens on the next `--restore`, which is covered
# end-to-end by scripts/e2e-restore.sh. Here we backfill the registry so the
# environment is left consistent, and re-assert the security invariant: fixing
# the row does NOT retroactively conjure an event on the live path (the
# finding is about NEVER emitting empty/unvalidated metadata).
pg "UPDATE faucet_registry
    SET origin_address = decode('$(lc "${B_TOKEN_ADDR#0x}")','hex')
    WHERE faucet_id = '$B_FAUCET_ID'" >/dev/null
sleep 15
[[ -z "$(find_bridge_event "$B_FROM_BLOCK" "$B_TOKEN_ADDR")" ]] \
    || fail "GATE BREACH: a BridgeEvent appeared on the live path after a mere registry fix (recovery must go through --restore)"
pass "Registry backfilled; live path still emits no event for the deferred note (recovery is via --restore, see e2e-restore.sh)"

# ── Self-target gate (Cantina #13): covered by a UNIT test, not e2e ──────────
# The projector's self-target poison-leaf gate (src/restore.rs — emits no
# BridgeEvent when a consumed B2AGG's dest_network == the local network id) is
# a RESTORE/REPLAY defense-in-depth layer. It cannot be exercised via a live
# bridge-out: the network never lets a self-targeted B2AGG be consumed (the
# note never commits), so the gate's trigger is unreachable end-to-end. It is
# instead covered by the unit test that fabricates the exact consumed
# self-target note:
#   cargo test --lib cantina13_self_target_b2agg_is_gated_in_projection
log "Self-target gate: covered by unit test cantina13_self_target_b2agg_is_gated_in_projection (unreachable via live bridge-out — note never commits)."

echo ""
log "======================================================================"
log "  CANTINA #13 LAYER-2 METADATA RECOVERY E2E DONE"
log "  A: legacy row recovered from L1, keccak-validated, emitted, self-healed"
log "  B: unrecoverable row gated (defer + metric); live path refuses post-fix"
log "======================================================================"
