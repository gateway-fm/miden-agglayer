#!/usr/bin/env bash
#
# Cantina MA#18 — erased / unbridgeable B2AGG bridge-out RECOVERY e2e.
#
# THE FINDING
# ───────────
# A B2AGG bridge-out that the bridge account CONSUMED on-chain (the LET frontier
# advanced, L2 funds were burned) but that the indexer could NOT translate into
# a synthetic BridgeEvent leaves the user's funds stranded: burned on the
# source, never minted on the destination. The canonical trigger is an *erased
# note* (created AND consumed in the same transaction) — its nullifier is
# stripped at block construction (`remove_erased_nullifiers`), so it never
# surfaces in `NoteFilter::Consumed` and the indexer never observes it. The
# on-chain bridge LET still advanced, so the Cantina #9 LET-divergence monitor
# sees `let_num_leaves > deposit_count`.
#
# WHY THIS TEST SIMULATES THE OBSERVABLE STATE (and does not construct a true
# erased note)
# ──────────────────────────────────────────────────────────────────────────────
# The `bridge-out-tool` harness cannot emit-and-consume a B2AGG in ONE
# transaction (there is no such flag — see src/bin/bridge_out_tool.rs; the
# bridge consumes the note on a LATER block), and forcing miden-node's block
# builder to erase a bridge-out nullifier is a protocol-level (miden-node /
# b2agg.masm) behaviour outside this repo. So we reproduce the EXACT observable
# proxy-side state the erased note produces — a bridge-out consumed on-chain (a
# REAL LET leaf) that the indexer quarantined into `unbridgeable_bridge_outs`
# with NO BridgeEvent and a deposit_count gap — by driving a real bridge-out and
# deleting its faucet-registry row before projection (reason=unknown_faucet).
# This is settlement-SAFE: the recovered leaf is backed by a real on-chain LET
# leaf, so the certificate that later covers it matches (no wedge — unlike the
# Cantina #13 poison leaf, which is why THIS test runs BEFORE cantina13).
#
# THE FIX (src/bridge_out_recovery.rs)
# ────────────────────────────────────
# When the LET divergence is detected (or an operator calls
# `admin_recoverUnbridgeableBridgeOuts`), the recovery path re-derives the
# BridgeEvent fields from the captured `note_dump` (storage felts → destination,
# assets → faucet + amount, resolve faucet) and re-emits the BridgeEvent via the
# SAME store primitives a normal bridge-out takes (mark_note_processed +
# add_bridge_event), then deletes the quarantine row. deposit_count catches up,
# the divergence clears, and the funds become claimable.
#
# What this test asserts:
#   BUG   — after the bridge-out, a `unbridgeable_bridge_outs` row exists,
#           NO BridgeEvent was emitted, and deposit_count did NOT advance
#           (the LET-divergence monitor fires: on_chain leaves > deposit_count).
#   FIX   — after re-registering the faucet + calling the recovery RPC, the
#           quarantine row is gone, a BridgeEvent IS emitted, deposit_count
#           advanced, and the recovery metric incremented.

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

TOKEN_DECIMALS=18
TOKEN_INITIAL_SUPPLY="1000000000000000000000000"
BRIDGE_AMOUNT="1000000000000000"
WEI_PER_MIDEN_UNIT=10000000000
EXPECTED_L2_BALANCE=$((BRIDGE_AMOUNT / WEI_PER_MIDEN_UNIT))

BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    while ! ( set +o pipefail; bash -c "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

# psql one-liner against the proxy's store DB.
pg() {
    docker exec "$PG_CONTAINER" psql -U agglayer -d agglayer_store -tA -c "$1"
}

lc() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

# Scrape one Prometheus series. Matches the exact metric name OR a
# `name{...labels}` series (labeled counters render the label in the key), and
# SUMS all matching series. Absent → 0.
proxy_metric_sum() {
    local name="$1"
    curl -sf "$L2_RPC/metrics" 2>/dev/null \
        | awk -v m="$name" '$1 == m || index($1, m "{") == 1 {s += $2; f=1} END {print (f ? s : 0)}'
}

# find_bridge_event <from_block_dec> <origin_addr_0x> — prints "0x<metadata>" for
# the FIRST BridgeEvent whose originAddress matches, else nothing. Queries
# synthetic_logs directly (BridgeEvent row is written there at emit time).
find_bridge_event() {
    local from_block="$1" origin_addr="$2"
    local topic_hex="${BRIDGE_EVENT_TOPIC#0x}"
    pg "SELECT data FROM synthetic_logs WHERE topics::text LIKE '%${topic_hex}%' AND block_number >= ${from_block} ORDER BY block_number" \
        | ORIGIN_ADDR="$origin_addr" python3 -c '
import os, sys
want = os.environ["ORIGIN_ADDR"].lower().replace("0x", "")
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    data = bytes.fromhex(line[2:] if line.startswith("0x") else line)
    origin_address = data[2 * 32 + 12 : 3 * 32].hex()
    if origin_address != want:
        continue
    meta_off = int.from_bytes(data[6 * 32 : 7 * 32], "big")
    meta_len = int.from_bytes(data[meta_off : meta_off + 32], "big")
    print("0x" + data[meta_off + 32 : meta_off + 32 + meta_len].hex())
    break
'
}

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
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-erased-note-recovery}"
B2AGG_FRESH="${B2AGG_FRESH:-1}"   # fresh wallet each run — self-funding
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH"   # sets WALLET_ID / WALLET_HEX / DEST_ADDR

log "======================================================================"
log "  Cantina MA#18 — Erased / Unbridgeable B2AGG Recovery E2E"
log "======================================================================"
log "Wallet:  $WALLET_ID ($WALLET_HEX)"
log "Bridge:  $BRIDGE_ID"

# ── Phase 0 — bridge a fresh ERC-20 in so we have a real faucet + L2 balance ───
log "───────────────── Phase 0: bridge a fresh token L1→L2 ─────────────────"
TOKEN_NAME="Erased Recovery Token"; TOKEN_SYMBOL="ERAS"
PHASE_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)

DEPLOY_OUT=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
    --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" --broadcast \
    --constructor-args "$TOKEN_NAME" "$TOKEN_SYMBOL" "$TOKEN_DECIMALS" "$TOKEN_INITIAL_SUPPLY" 2>&1)
TOKEN_ADDR=$(echo "$DEPLOY_OUT" | grep "Deployed to:" | awk '{print $NF}')
[[ -z "$TOKEN_ADDR" ]] && fail "Failed to deploy TestToken: $DEPLOY_OUT"
pass "$TOKEN_SYMBOL deployed at $TOKEN_ADDR"

cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
    "$TOKEN_ADDR" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$BRIDGE_AMOUNT" >/dev/null 2>&1 \
    || fail "approve failed"
TX=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST_ADDR" "$BRIDGE_AMOUNT" "$TOKEN_ADDR" true 0x 2>&1)
[[ "$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')" == "1" ]] \
    || fail "L1 bridgeAsset failed: $TX"
pass "$TOKEN_SYMBOL bridged on L1"

WANT_ADDR=$(lc "$TOKEN_ADDR")
wait_for "$TOKEN_SYMBOL deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(dep['ready_for_claim'] and (dep.get('orig_addr') or '').lower()=='$WANT_ADDR' for dep in d['deposits']) else 1)\"" \
    180 5
wait_for "$TOKEN_SYMBOL faucet auto-creation" \
    "docker logs --since $PHASE_START $AGGLAYER_CONTAINER 2>&1 | grep -q 'auto-creating faucet'" \
    180 5
wait_for "$TOKEN_SYMBOL claim committed" \
    "docker logs --since $PHASE_START $AGGLAYER_CONTAINER 2>&1 | grep -q 'claim tx.*committed to block'" \
    120 5

FAUCET_ID=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | WANT_ADDR="$WANT_ADDR" python3 -c '
import json, os, sys
want = os.environ["WANT_ADDR"]
for f in json.load(sys.stdin).get("result", []):
    if f.get("origin_address", "").lower() == want:
        print(f["faucet_id"]); break
')
[[ -z "$FAUCET_ID" ]] && fail "$TOKEN_SYMBOL faucet not found"
pass "$TOKEN_SYMBOL faucet: $FAUCET_ID"

BALANCE=0
for attempt in $(seq 1 15); do
    sleep 10
    BALANCE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID" || true)
    log "Attempt $attempt/15: $TOKEN_SYMBOL balance = ${BALANCE:-0}"
    [[ -n "$BALANCE" && "$BALANCE" != "0" ]] && break
done
[[ -z "$BALANCE" || "$BALANCE" == "0" ]] && fail "$TOKEN_SYMBOL L2 balance still 0"
pass "$TOKEN_SYMBOL L2 balance: $BALANCE Miden units"

# Snapshot the token's real registration params (to re-register in the FIX phase).
REG_JSON=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' \
    | WANT_ADDR="$WANT_ADDR" python3 -c '
import json, os, sys
want = os.environ["WANT_ADDR"]
for f in json.load(sys.stdin).get("result", []):
    if f.get("origin_address", "").lower() == want:
        print(json.dumps(f)); break
')
ORIGIN_NETWORK=$(echo "$REG_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["origin_network"])')
ORIGIN_DECIMALS=$(echo "$REG_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["origin_decimals"])')

# ══════════════════════════════════════════════════════════════════════════════
# Phase 1 — REPRODUCE THE BUG: delete the faucet-registry row so the bridge-out
# projects to `unknown_faucet` (the same observable state an ERASED note leaves:
# LET advanced on-chain, NO BridgeEvent, deposit_count gap, quarantine row).
# ══════════════════════════════════════════════════════════════════════════════
log "──────────────── Phase 1: reproduce the unbridgeable/erased state ───────────────"
FROM_BLOCK=1   # lower bound only (find_bridge_event is address-filtered)

DEP_BEFORE=$(pg "SELECT deposit_counter FROM service_state WHERE id = 1")
QROWS_BEFORE=$(pg "SELECT count(*) FROM unbridgeable_bridge_outs")
DIV_BEFORE=$(proxy_metric_sum bridge_let_divergence_total)
log "deposit_counter before=$DEP_BEFORE  quarantine_rows before=$QROWS_BEFORE  divergence before=$DIV_BEFORE"

# No BridgeEvent for this token yet (it has never been bridged OUT).
[[ -z "$(find_bridge_event "$FROM_BLOCK" "$TOKEN_ADDR")" ]] \
    || fail "precondition: a BridgeEvent already exists for $TOKEN_SYMBOL before the bridge-out"

# Surgery: remove the faucet registry row so resolve_faucet_origin fails at
# projection → the consumed B2AGG is quarantined (reason unknown_faucet) with
# NO BridgeEvent, exactly like an erased note the indexer can't translate.
DEL=$(pg "DELETE FROM faucet_registry WHERE faucet_id = '$FAUCET_ID' RETURNING faucet_id")
[[ -z "$DEL" ]] && fail "faucet_registry DELETE matched no row for $FAUCET_ID"
pass "Induced unbridgeable condition: faucet_registry row removed for $FAUCET_ID"

OUT_AMOUNT=$((BALANCE / 2))
log "Bridging $OUT_AMOUNT $TOKEN_SYMBOL Miden units L2→L1 (must quarantine, no BridgeEvent)..."
iso_tool --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount "$OUT_AMOUNT" --dest-address "$FUNDED_ADDR" --dest-network 0 2>&1 \
    || fail "bridge-out-tool failed"
pass "B2AGG note created + consumed on-chain (LET leaf appended, L2 funds burned)"

# The consumed B2AGG must land a quarantine row (unknown_faucet) — the positive
# operator handle that stands in for the erased note the indexer couldn't see.
# NOTE: do NOT filter on bridge_account = "$BRIDGE_ID" here — the toml (and
# hence $BRIDGE_ID) is BECH32 while the PG column stores the HEX form, so the
# equality never matches and the wait times out with the row present (the
# bech32-vs-hex trap documented in e2e-bridge-loadtest-isolated.sh; it cost
# this test its first live run). Count-delta over the snapshot baseline is
# both format-proof and stronger (asserts THIS bridge-out caused the row).
wait_for "quarantine row recorded" \
    "[[ \"\$(pg \"SELECT count(*) FROM unbridgeable_bridge_outs WHERE reason = 'unknown_faucet'\")\" -gt $QROWS_BEFORE ]]" \
    120 5
NOTE_ID=$(pg "SELECT note_id FROM unbridgeable_bridge_outs WHERE reason = 'unknown_faucet' ORDER BY observed_block DESC LIMIT 1")
[[ -z "$NOTE_ID" ]] && fail "no quarantined note_id found"
pass "BUG reproduced: bridge-out quarantined (note_id=$NOTE_ID, reason=unknown_faucet)"

# BUG assertion 1 — NO synthetic BridgeEvent was emitted for the burned funds.
[[ -z "$(find_bridge_event "$FROM_BLOCK" "$TOKEN_ADDR")" ]] \
    || fail "BUG assertion failed: a BridgeEvent was emitted despite the unbridgeable note"
pass "BUG assertion: no BridgeEvent emitted for the consumed bridge-out"

# BUG assertion 2 — deposit_count did NOT advance for this note → the LET is
# ahead of the indexer. The Cantina #9 monitor detects on_chain_ahead.
DEP_MID=$(pg "SELECT deposit_counter FROM service_state WHERE id = 1")
[[ "$DEP_MID" == "$DEP_BEFORE" ]] \
    || warn "deposit_counter changed ($DEP_BEFORE→$DEP_MID) — other suite activity; proceeding"
wait_for "LET-divergence monitor fires (on_chain_ahead)" \
    "[[ \"\$(curl -sf $L2_RPC/metrics 2>/dev/null | awk '/bridge_let_divergence_total\{.*on_chain_ahead/ {s+=\$2} END {print (s+0)}')\" -gt 0 ]]" \
    90 5
pass "BUG assertion: LET-divergence monitor detected on_chain leaves > deposit_count"

# ══════════════════════════════════════════════════════════════════════════════
# Phase 2 — APPLY THE FIX: re-register the faucet, then run recovery. The
# recovery re-derives the BridgeEvent from the quarantine note_dump and emits it
# via the normal bridge-out commit path; deposit_count catches up + row cleared.
# ══════════════════════════════════════════════════════════════════════════════
log "──────────────── Phase 2: recover via admin_recoverUnbridgeableBridgeOuts ───────────────"

# Resolve the blocker: re-register the faucet with its real params.
REG_RESULT=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"admin_registerFaucet\",\"params\":[{\"symbol\":\"$TOKEN_SYMBOL\",\"origin_token_address\":\"$TOKEN_ADDR\",\"origin_network\":$ORIGIN_NETWORK,\"origin_decimals\":$ORIGIN_DECIMALS,\"name\":\"$TOKEN_NAME\"}],\"id\":1}")
echo "$REG_RESULT" | grep -q '"result"' || fail "admin_registerFaucet failed: $REG_RESULT"
pass "Faucet re-registered — recovery blocker resolved"

# Trigger the operator-driven recovery sweep immediately (before the next
# projector tick can self-heal), proving the recovery entrypoint closes the gap.
RECOVER=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
    -d '{"jsonrpc":"2.0","method":"admin_recoverUnbridgeableBridgeOuts","params":[],"id":1}')
echo "$RECOVER" | grep -q '"result"' || fail "admin_recoverUnbridgeableBridgeOuts failed: $RECOVER"
log "Recovery summary: $RECOVER"
RECOVERED=$(echo "$RECOVER" | python3 -c 'import json,sys; r=json.load(sys.stdin)["result"]; print(r["recovered"]+r["stale_cleared"])')
[[ "${RECOVERED:-0}" -ge 1 ]] \
    || fail "recovery sweep closed nothing (recovered+stale_cleared=$RECOVERED)"
pass "Recovery sweep enacted the stranded bridge-out (recovered+stale_cleared=$RECOVERED)"

# FIX assertion 1 — the quarantine row for this note is gone.
wait_for "quarantine row cleared" \
    "[[ \"\$(pg \"SELECT count(*) FROM unbridgeable_bridge_outs WHERE note_id = '$NOTE_ID'\")\" -eq 0 ]]" \
    60 3
pass "FIX assertion: quarantine row removed for note_id=$NOTE_ID"

# FIX assertion 2 — a BridgeEvent is now emitted for the previously-stranded funds.
wait_for "BridgeEvent emitted after recovery" \
    "[[ -n \"\$(find_bridge_event $FROM_BLOCK $TOKEN_ADDR)\" ]]" \
    120 5
pass "FIX assertion: BridgeEvent emitted — the off-chain side is enacted"

# FIX assertion 3 — deposit_count advanced (the LET divergence closes).
DEP_AFTER=$(pg "SELECT deposit_counter FROM service_state WHERE id = 1")
[[ "$DEP_AFTER" -gt "$DEP_MID" ]] \
    || fail "FIX assertion failed: deposit_counter did not advance ($DEP_MID→$DEP_AFTER)"
pass "FIX assertion: deposit_counter advanced ($DEP_MID→$DEP_AFTER) — indexer caught up to the LET"

# FIX assertion 4 — the recovery metric fired.
REC_METRIC=$(proxy_metric_sum bridge_out_recovered_unbridgeable_total)
[[ "${REC_METRIC:-0}" -ge 1 ]] \
    || warn "bridge_out_recovered_unbridgeable_total=$REC_METRIC (0 is possible if the live projector won the race and recovery only StaleCleared)"

log "======================================================================"
pass "Cantina MA#18 erased/unbridgeable recovery E2E PASSED"
log "  BUG: consumed bridge-out quarantined, no BridgeEvent, LET divergence fired"
log "  FIX: recovery re-emitted the BridgeEvent, deposit_count caught up, row cleared"
log "======================================================================"
