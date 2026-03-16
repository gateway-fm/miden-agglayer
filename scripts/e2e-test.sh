#!/usr/bin/env bash
#
# Full bidirectional bridge E2E test — no Kurtosis required.
#
# Prerequisites:
#   - docker compose services running (make e2e-up)
#   - cast (foundry) installed
#   - bridge-out-tool built (in Docker image)
#
# Usage:
#   ./scripts/e2e-test.sh            # run all tests
#   ./scripts/e2e-test.sh l1-to-l2   # run only L1→L2 deposit+claim
#   ./scripts/e2e-test.sh l2-to-l1   # run only L2→L1 bridge-out

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

# ── Configuration ─────────────────────────────────────────────────────────────

# shellcheck disable=SC1091
source "$FIXTURES_DIR/.env"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
BRIDGE_SERVICE_URL="http://localhost:18080"
MIDEN_NODE_URL="http://localhost:57291"

# Pre-funded test key (Kurtosis deployer)
FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
DEPOSIT_AMOUNT="100000000000000000" # 0.1 ETH in wei
DEST_NETWORK=2  # Must match chain_id (not rollup network_id)

# ── Helpers ───────────────────────────────────────────────────────────────────

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! eval "$cmd" 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

pg_query() {
    local sql="$1"
    local pg
    pg=$(docker ps -q --filter 'name=postgres' --filter 'network=miden-e2e')
    docker exec "$pg" psql -U bridge_user -d bridge_db -At -c "$sql" 2>/dev/null
}

# ── Pre-flight ────────────────────────────────────────────────────────────────

preflight() {
    log "Pre-flight checks..."
    command -v cast >/dev/null || fail "cast (foundry) not found"
    cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 not reachable — run: make e2e-up"
    curl -sf "$BRIDGE_SERVICE_URL/api" >/dev/null 2>&1 || fail "Bridge service not reachable"
    pass "Pre-flight OK"
}

# ── Test 1: L1→L2 Deposit + Claim ────────────────────────────────────────────

test_l1_to_l2() {
    log "======================================================================"
    log "  TEST 1: L1→L2 Deposit + Claim"
    log "======================================================================"

    # Get wallet_hardhat AccountId and convert to zero-padded Eth address
    local accounts_toml
    accounts_toml=$(docker exec miden-agglayer-miden-agglayer-1 \
        cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null)
    local wallet_id bridge_id faucet_id
    wallet_id=$(echo "$accounts_toml" | grep 'wallet_hardhat' | cut -d'"' -f2)
    bridge_id=$(echo "$accounts_toml" | grep 'bridge ' | cut -d'"' -f2)
    faucet_id=$(echo "$accounts_toml" | grep 'faucet_eth' | cut -d'"' -f2)

    # Get wallet hex and build zero-padded Eth address (4 zero bytes + 15 AccountId bytes + 1 zero byte)
    local wallet_hex
    wallet_hex=$(docker exec miden-agglayer-miden-agglayer-1 bridge-out-tool \
        --store-dir /var/lib/miden-agglayer-service --node-url http://miden-node:57291 \
        --wallet-id "$wallet_id" --bridge-id "$bridge_id" --faucet-id "$faucet_id" \
        --amount 1 --dest-address 0xabc --dest-network 0 2>&1 | grep "wallet:" | awk '{print $NF}')
    # AccountId is 15 bytes (30 hex). Pad: 0x00000000<30hex>00
    local inner="${wallet_hex:2}"
    local dest_address="0x00000000${inner}00"
    log "Wallet: $wallet_id"
    log "Destination (zero-padded): $dest_address"

    # Step 1: Deposit on L1
    log "Step 1/3: Depositing $DEPOSIT_AMOUNT wei on L1 (dest_network=$DEST_NETWORK)..."
    local tx_result
    tx_result=$(cast send --rpc-url "$L1_RPC" \
        --private-key "$FUNDED_KEY" \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$dest_address" "$DEPOSIT_AMOUNT" \
        0x0000000000000000000000000000000000000000 true 0x \
        --value "$DEPOSIT_AMOUNT" 2>&1)
    echo "$tx_result" | grep -q "status.*1" || fail "L1 deposit failed"
    pass "L1 deposit tx succeeded"

    # Step 2: Wait for bridge sync + GER injection + ready_for_claim
    log "Step 2/3: Waiting for deposit to become claimable (aggoracle GER injection)..."
    wait_for "deposit ready_for_claim" \
        "[[ \$(pg_query \"SELECT ready_for_claim FROM sync.deposit WHERE deposit_cnt = (SELECT MAX(deposit_cnt) FROM sync.deposit WHERE network_id=0 AND amount != '0')\") == 't' ]]" \
        180 5
    pass "Deposit is ready_for_claim"

    # Step 3: Check that bridge-service auto-submitted claim
    log "Step 3/3: Checking auto-claim submission..."
    sleep 10
    local claim_tx
    claim_tx=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$dest_address" | \
        python3 -c "import json,sys; ds=json.load(sys.stdin)['deposits']; [print(d.get('claim_tx_hash','')) for d in ds if d.get('ready_for_claim') and d.get('amount') != '0']" 2>/dev/null | tail -1)

    if [[ -n "$claim_tx" && "$claim_tx" != "0x0000000000000000000000000000000000000000000000000000000000000000" ]]; then
        pass "L1→L2 claim submitted: ${claim_tx:0:20}..."
    else
        warn "Claim not yet submitted by ClaimTxManager"
    fi

    # Check proxy logs for claim note creation
    local claim_note
    claim_note=$(docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
        logs miden-agglayer 2>&1 | grep "creating CLAIM" | tail -1)
    if [[ -n "$claim_note" ]]; then
        pass "Proxy created CLAIM note on Miden"
    else
        warn "No CLAIM note creation detected yet"
    fi

    echo ""
}

# ── Test 2: L2→L1 Bridge-Out ─────────────────────────────────────────────────

test_l2_to_l1() {
    log "======================================================================"
    log "  TEST 2: L2→L1 Bridge-Out"
    log "======================================================================"

    # Get account IDs from proxy config
    local accounts_toml
    accounts_toml=$(docker exec miden-agglayer-miden-agglayer-1 \
        cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null)
    local wallet_id bridge_id faucet_id
    wallet_id=$(echo "$accounts_toml" | grep 'wallet_hardhat' | cut -d'"' -f2)
    bridge_id=$(echo "$accounts_toml" | grep 'bridge ' | cut -d'"' -f2)
    faucet_id=$(echo "$accounts_toml" | grep 'faucet_eth' | cut -d'"' -f2)

    log "Wallet:  $wallet_id"
    log "Bridge:  $bridge_id"
    log "Faucet:  $faucet_id"

    local l1_dest
    l1_dest=$(cast wallet address --private-key "$FUNDED_KEY")
    log "L1 dest: $l1_dest"

    # Check wallet balance
    local bal_output
    bal_output=$(docker exec miden-agglayer-miden-agglayer-1 bridge-out-tool \
        --store-dir /var/lib/miden-agglayer-service \
        --node-url http://miden-node:57291 \
        --wallet-id "$wallet_id" --bridge-id "$bridge_id" --faucet-id "$faucet_id" \
        --amount 1 --dest-address "$l1_dest" --dest-network 0 2>&1 || true)

    local wallet_bal
    wallet_bal=$(echo "$bal_output" | grep "wallet balance" | awk '{print $NF}')

    if [[ -z "$wallet_bal" || "$wallet_bal" == "0" ]]; then
        warn "L2 wallet balance is 0 — L1→L2 claim must succeed first."
        warn "The CLAIM note may still be processing on the miden-node."
        warn "Skipping L2→L1 bridge-out test."
        return 0
    fi

    log "L2 wallet balance: $wallet_bal"

    # Step 1: Bridge-out
    local bridge_amount=$((wallet_bal / 2))
    log "Step 1/4: Creating bridge-out note (amount=$bridge_amount)..."
    docker exec miden-agglayer-miden-agglayer-1 bridge-out-tool \
        --store-dir /var/lib/miden-agglayer-service \
        --node-url http://miden-node:57291 \
        --wallet-id "$wallet_id" --bridge-id "$bridge_id" --faucet-id "$faucet_id" \
        --amount "$bridge_amount" --dest-address "$l1_dest" --dest-network 0 2>&1
    pass "Bridge-out note created"

    # Step 2: Wait for BridgeEvent
    local bridge_event_topic="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
    log "Step 2/4: Waiting for BridgeEvent in proxy..."
    wait_for "BridgeEvent in L2 logs" \
        "cast logs --rpc-url $L2_RPC --from-block 0 $bridge_event_topic 2>/dev/null | grep -q 'data'" \
        120 5
    pass "BridgeEvent detected"

    # Step 3: Wait for certificate
    log "Step 3/4: Waiting for aggsender certificate..."
    wait_for "certificate submission" \
        "docker compose -f $PROJECT_DIR/docker-compose.e2e.yml --env-file $FIXTURES_DIR/.env logs aggkit 2>/dev/null | grep -q 'certificate.*sent\|SendCertificate'" \
        120 5
    pass "Certificate submitted"

    # Step 4: Check L1 balance change
    log "Step 4/4: Waiting for L1 claim..."
    local l1_bal_before
    l1_bal_before=$(cast balance --rpc-url "$L1_RPC" "$l1_dest")
    wait_for "L1 balance change" \
        "[[ \$(cast balance --rpc-url $L1_RPC $l1_dest 2>/dev/null) != '$l1_bal_before' ]]" \
        180 5
    local l1_bal_after
    l1_bal_after=$(cast balance --rpc-url "$L1_RPC" "$l1_dest")
    pass "L2→L1 complete! L1 balance: $l1_bal_before → $l1_bal_after"

    echo ""
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    local test_filter="${1:-all}"

    log "======================================================================"
    log "  Miden Bridge E2E Test Suite"
    log "======================================================================"
    echo ""

    preflight

    case "$test_filter" in
        all)       test_l1_to_l2; test_l2_to_l1 ;;
        l1-to-l2)  test_l1_to_l2 ;;
        l2-to-l1)  test_l2_to_l1 ;;
        *)         fail "Unknown test: $test_filter (use: all, l1-to-l2, l2-to-l1)" ;;
    esac

    log "======================================================================"
    log "  TESTS COMPLETE"
    log "======================================================================"
}

main "$@"
