#!/usr/bin/env bash
# Cantina #6 E2E — non-ETH faucet identity survives a --restore / lost-row event.
#
# THE BUG (finding #6, HIGH): on a `--restore` / fresh-Postgres bootstrap the
# recovery path rebuilds synthetic EVENTS from consumed notes but NEVER rebuilds
# the non-ETH `faucet_registry` rows. A faucet whose local row is gone then makes
# `resolve_faucet_origin` error, so every historical AND future bridge-out tied to
# it is skipped/quarantined (invisible exit), and the next claim/admin-register
# deploys a REPLACEMENT faucet → split-brain the registry can't model.
#
# THE FIX (this test): restore now has a Phase 1.7 that reads each faucet's origin
# identity back from the bridge's authoritative `faucet_metadata_map` and rebuilds
# the missing `faucet_registry` row BEFORE replaying bridge-outs.
#
# FLOW
#   1. Deploy a fresh ERC-20 (TT), bridge L1→L2 (auto-creates the faucet + row).
#   2. Do a first ("historical") L2→L1 bridge-out with an ISOLATED wallet — this
#      leaves a consumed B2AGG note on Miden that references the faucet.
#   3. Snapshot the faucet_registry row, then DELETE it in Postgres (simulate the
#      lost identity that finding #6 is about).
#   4. Run `--restore` (the same one-shot invocation as e2e-restore.sh).
#   5. ASSERT the row is rebuilt with the SAME faucet_id + origin_address + network
#      + scale (rebuilt from bridge state, not re-deployed).
#   6. Do a SECOND bridge-out of the same token and ASSERT it RESOLVES + emits a
#      synthetic BridgeEvent (pre-fix: skipped as UnknownFaucet).
#
# HONESTY / SCOPE
#   - This exercises the real container `--restore` path end-to-end against a live
#     Miden node + Postgres. The pure decode/enumerate logic is unit-tested in
#     `metadata_recovery::tests::finding_6`.
#   - The rebuilt row's ERC-20 `metadata` preimage is recovered from bridge state +
#     the Miden faucet (name==symbol) and, if wired, an L1 RPC; where none matches,
#     the bridge-out emit path gates it (Cantina #13 L2). To keep the post-restore
#     bridge-out deterministic here we deploy TT with name == symbol == "TT" so the
#     all-Miden metadata candidate validates without an L1 RPC.
#   - Requires the full E2E stack up and miden-agglayer on PgStore.
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-cantina6-faucet-identity-restore.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

# Required by docker-compose.e2e.yml's build args (interpolated even for a
# --no-deps one-shot run). Mirrors e2e-restore.sh.
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.15.0}"

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
BRIDGE_SERVICE_URL="http://localhost:18080"
PG_HOST="localhost"; PG_PORT="5434"; PG_USER="agglayer"; PG_PASS="agglayer"; PG_DB="agglayer_store"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY" 2>/dev/null || echo "")
DEST_NETWORK=1  # Miden network id (local topology pins MIDEN_NETWORK_ID=1)

# TT: 18 decimals → scale=10 (18 origin - 8 miden). Bridge 0.001 tokens.
TOKEN_DECIMALS=18
TOKEN_INITIAL_SUPPLY="1000000000000000000000000"
BRIDGE_AMOUNT="1000000000000000"          # 10^15 = 0.001 tokens
WEI_PER_MIDEN_UNIT=10000000000            # 10^10
EXPECTED_L2_BALANCE=$((BRIDGE_AMOUNT / WEI_PER_MIDEN_UNIT))  # 100000

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

TEST_START_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || fail "cast (foundry) not found"
command -v forge >/dev/null || fail "forge (foundry) not found"
command -v psql >/dev/null || fail "psql not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
pgquery "SELECT 1" >/dev/null || fail "PostgreSQL not reachable on $PG_HOST:$PG_PORT"
wait_for "L2 proxy healthy" \
    "curl -sf '$L2_RPC' -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}' >/dev/null" \
    60 3

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY — run scripts/ensure-e2e-secrets.sh}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"

# Infrastructure account ids (from the config file, not the sqlite store).
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# Isolated bridge-out wallet — MUST use lib-isolated-wallet.sh, never the proxy's
# store (never --store-dir /var/lib...). Single-owner store policy.
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-cantina6}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH" \
    || fail "could not provision isolated bridge-out wallet"

log "======================================================================"
log "  Cantina #6 — Faucet Identity Restore E2E"
log "======================================================================"
log "Wallet:  $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"

# ══════════════════════════════════════════════════════════════════════════════
# PART 1: Fresh ERC-20 → L1→L2 → faucet auto-creates
# ══════════════════════════════════════════════════════════════════════════════
step "Part 1: Deploy TT (name==symbol==TT so metadata self-heals w/o L1 RPC)..."
DEPLOY_OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
    --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" --broadcast \
    --constructor-args "TT" "TT" "$TOKEN_DECIMALS" "$TOKEN_INITIAL_SUPPLY" 2>&1)
TOKEN_ADDR=$(echo "$DEPLOY_OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -z "$TOKEN_ADDR" ]] && fail "Failed to deploy TestToken: $DEPLOY_OUT"
# 20-byte hex (no 0x) for BYTEA comparisons against origin_address.
TOKEN_HEX40=$(printf '%s' "$TOKEN_ADDR" | sed 's/^0x//' | tr 'A-F' 'a-f')
pass "TT deployed at $TOKEN_ADDR"

log "Approving + bridging TT L1→L2..."
cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
    "$TOKEN_ADDR" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$BRIDGE_AMOUNT" \
    >/dev/null 2>&1 || fail "approve failed"
TX=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
    "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST_ADDR" "$BRIDGE_AMOUNT" "$TOKEN_ADDR" true 0x 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "L1 bridge tx failed (status=$STATUS): $TX"

wait_for "deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and dep['amount']!='0' for dep in d['deposits']) else 1)\"" \
    180 5
wait_for "faucet auto-creating" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    180 5
wait_for "claim committed" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    120 5
pass "TT faucet auto-created + claim committed"

# The faucet_registry row now exists. Capture it.
step "Snapshotting faucet_registry row for TT..."
FAUCET_ROW=$(pgquery "SELECT faucet_id, encode(origin_address,'hex'), origin_network, scale FROM faucet_registry WHERE origin_address = decode('$TOKEN_HEX40','hex') LIMIT 1")
[[ -z "$FAUCET_ROW" ]] && fail "TT faucet_registry row not found after auto-create"
PRE_FAUCET_ID=$(echo "$FAUCET_ROW" | awk -F'|' '{print $1}')
PRE_ORIGIN_HEX=$(echo "$FAUCET_ROW" | awk -F'|' '{print $2}')
PRE_ORIGIN_NET=$(echo "$FAUCET_ROW" | awk -F'|' '{print $3}')
PRE_SCALE=$(echo "$FAUCET_ROW" | awk -F'|' '{print $4}')
log "  faucet_id=$PRE_FAUCET_ID origin=0x$PRE_ORIGIN_HEX network=$PRE_ORIGIN_NET scale=$PRE_SCALE"
[[ "$PRE_ORIGIN_HEX" == "$TOKEN_HEX40" ]] || fail "origin_address mismatch pre-restore"
pass "Row snapshotted"

# Verify L2 balance so the bridge-out below has funds.
BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$PRE_FAUCET_ID")
    log "L2 balance attempt $attempt/15: ${BALANCE:-0}"
    [[ -n "$BALANCE" && "$BALANCE" != "0" ]] && break
done
[[ -z "$BALANCE" || "$BALANCE" == "0" ]] && fail "L2 TT balance still 0"
[[ "$BALANCE" -eq "$EXPECTED_L2_BALANCE" ]] || fail "Balance mismatch: got $BALANCE, expected $EXPECTED_L2_BALANCE"
pass "L1→L2 TT balance verified: $BALANCE Miden units"

# ══════════════════════════════════════════════════════════════════════════════
# PART 2: First ("historical") bridge-out — leaves a consumed B2AGG on Miden
# ══════════════════════════════════════════════════════════════════════════════
step "Part 2: First L2→L1 bridge-out (the 'historical' exit)..."
FIRST_OUT=$((BALANCE / 4))
iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$PRE_FAUCET_ID" \
    --amount "$FIRST_OUT" --dest-address "$FUNDED_ADDR" --dest-network 0 2>&1 \
    || fail "first bridge-out failed"
wait_for "BridgeEvent (historical)" \
    "docker logs --since $TEST_START_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'emitted BridgeEvent'" \
    120 5
pass "Historical bridge-out emitted a BridgeEvent"

# ══════════════════════════════════════════════════════════════════════════════
# PART 3: DELETE the faucet_registry row (simulate lost identity)
# ══════════════════════════════════════════════════════════════════════════════
step "Part 3: Deleting TT's faucet_registry row (simulating finding #6 lost row)..."
pgquery "DELETE FROM faucet_registry WHERE faucet_id = '$PRE_FAUCET_ID'" >/dev/null
GONE=$(pgquery "SELECT COUNT(*) FROM faucet_registry WHERE faucet_id = '$PRE_FAUCET_ID'")
[[ "$GONE" -eq 0 ]] || fail "faucet_registry row not deleted"
pass "TT faucet_registry row deleted (identity lost)"

# ══════════════════════════════════════════════════════════════════════════════
# PART 4: --restore (one-shot container), then assert the row is rebuilt
# ══════════════════════════════════════════════════════════════════════════════
step "Part 4: Running --restore..."
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    stop miden-agglayer >/dev/null 2>&1
sleep 2
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    run --rm --no-deps miden-agglayer \
    --miden-node=http://miden-node:57291 \
    --miden-store-dir=/var/lib/miden-agglayer-service \
    --restore 2>&1 | while IFS= read -r line; do echo "  [restore] $line"; done
RESTORE_EXIT=${PIPESTATUS[0]}
[[ "$RESTORE_EXIT" -eq 0 ]] || fail "Restore exited with code $RESTORE_EXIT"
pass "Restore completed"

step "Asserting the faucet_registry row was rebuilt from bridge state..."
REBUILT=$(pgquery "SELECT faucet_id, encode(origin_address,'hex'), origin_network, scale FROM faucet_registry WHERE faucet_id = '$PRE_FAUCET_ID' LIMIT 1")
[[ -z "$REBUILT" ]] && fail "Cantina #6: faucet_registry row NOT rebuilt by --restore (the bug)"
RB_ID=$(echo "$REBUILT" | awk -F'|' '{print $1}')
RB_ORIGIN_HEX=$(echo "$REBUILT" | awk -F'|' '{print $2}')
RB_ORIGIN_NET=$(echo "$REBUILT" | awk -F'|' '{print $3}')
RB_SCALE=$(echo "$REBUILT" | awk -F'|' '{print $4}')
log "  rebuilt: faucet_id=$RB_ID origin=0x$RB_ORIGIN_HEX network=$RB_ORIGIN_NET scale=$RB_SCALE"
[[ "$RB_ID" == "$PRE_FAUCET_ID" ]]       || fail "rebuilt faucet_id differs (a REPLACEMENT was deployed — split-brain)"
[[ "$RB_ORIGIN_HEX" == "$PRE_ORIGIN_HEX" ]] || fail "rebuilt origin_address mismatch"
[[ "$RB_ORIGIN_NET" == "$PRE_ORIGIN_NET" ]] || fail "rebuilt origin_network mismatch"
[[ "$RB_SCALE" == "$PRE_SCALE" ]]         || fail "rebuilt scale mismatch"
# Exactly ONE row for this origin — no second generation.
ROWS_FOR_ORIGIN=$(pgquery "SELECT COUNT(*) FROM faucet_registry WHERE origin_address = decode('$TOKEN_HEX40','hex')")
[[ "$ROWS_FOR_ORIGIN" -eq 1 ]] || fail "expected exactly 1 faucet row for origin, got $ROWS_FOR_ORIGIN"
pass "Cantina #6: row rebuilt with SAME identity (faucet_id/origin/scale), no split-brain"

# ══════════════════════════════════════════════════════════════════════════════
# PART 5: Restart, then a SECOND bridge-out must RESOLVE + emit (not skip)
# ══════════════════════════════════════════════════════════════════════════════
step "Part 5: Restart miden-agglayer + a fresh bridge-out of TT..."
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    start miden-agglayer >/dev/null 2>&1
wait_for "miden-agglayer healthy" \
    "curl -sf $L2_RPC -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}'" \
    60 3

POST_RESTORE_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SECOND_OUT=$((BALANCE / 4))
iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$PRE_FAUCET_ID" \
    --amount "$SECOND_OUT" --dest-address "$FUNDED_ADDR" --dest-network 0 2>&1 \
    || fail "second bridge-out failed"

# The load-bearing assertion: post-restore the faucet resolves, so the projector
# EMITS a synthetic BridgeEvent for this exit. Pre-fix (no rebuilt row) it would
# instead log an UnknownFaucet quarantine and skip. Assert emit AND absence of an
# unknown-faucet skip for this window.
wait_for "post-restore BridgeEvent emitted" \
    "docker logs --since $POST_RESTORE_TIME $AGGLAYER_CONTAINER 2>&1 | grep -q 'emitted BridgeEvent'" \
    150 5
if docker logs --since "$POST_RESTORE_TIME" "$AGGLAYER_CONTAINER" 2>&1 | grep -qi 'unknown faucet'; then
    fail "Cantina #6: post-restore bridge-out hit 'unknown faucet' — the row was not usable"
fi
pass "Cantina #6: post-restore bridge-out RESOLVED + emitted a BridgeEvent (not skipped)"

echo ""
log "======================================================================"
log "  CANTINA #6 FAUCET-IDENTITY RESTORE TEST COMPLETE"
log "  faucet_id $PRE_FAUCET_ID: row lost → --restore rebuilt it from bridge"
log "  faucet_metadata_map (same id/origin/scale); bridge-out replays, no"
log "  replacement faucet, no split-brain."
log "======================================================================"
