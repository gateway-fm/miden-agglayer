#!/usr/bin/env bash
# E2E — synthesized-claim FULL calldata through the PINNED aggkit parser (PR #136).
#
# SOAK FINDING #2 regression, review-hardened: a PROXY-SYNTHESIZED claim tx (MA#27 —
# ClaimEvent emitted under a DERIVED hash because no real eth-tx link exists) must serve
# WELL-FORMED, AUTHORITATIVE claimAsset calldata via eth_getTransactionByHash:
#   * both SMT proofs + both exit roots + networks/addresses/amount from the consumed
#     CLAIM note's on-chain storage (the values the proxy built and the bridge verified);
#   * the metadata preimage from the faucet registry, hash-verified against the note's
#     metadata_hash;
# and aggkit v0.8.3's L2BridgeSyncer (which fetches EVERY claim tx and parses the full
# calldata — 'DetailedClaimEvent') must sync PAST the claim block and the certificate
# pipeline must keep settling. Zero-filled/fabricated fields are forbidden: aggkit
# persists all of them and derives the claim's GER from the exit roots.
#
# MA#27 derived-hash condition, produced deterministically via the restore flow:
# wiping the proxy's PG store destroys every tx_note_link, so `--restore` re-synthesizes
# EVERY ClaimEvent under its derived hash — exactly the crash-recovery state the live
# soak hit at block 8831 (tx 0x1ac390c7…, empty input, certs halted for 2h).
#
# Flow:
#   1. Ensure a completed L1→L2 claim exists (bootstrap e2e-l1-to-l2.sh if not).
#   2. Record the claim's global_index + note_id; wipe miden-derived PG state
#      (same table set as e2e-restore.sh — includes tx_note_links).
#   3. Run --restore; restart the proxy.
#   4. Assert the re-synthesized ClaimEvent rides the DERIVED hash
#      (recomputed here via cast keccak over the versioned tag ‖ note_id).
#   5. Assert eth_getTransactionByHash(derived) serves full claimAsset calldata:
#      claimAsset selector, globalIndex at its exact ABI offset == the event's gi,
#      length covering both 32-word proof arrays (no stub).
#   6. Restart aggkit (fresh window) and assert its L2BridgeSyncer syncs PAST the
#      claim block with ZERO 'input too short' errors, and a certificate reaches
#      Settled — the exact wedge this fix clears.
#
# Usage:  source fixtures/.env && ./scripts/e2e-synthesized-claim-calldata.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
source "$FIXTURES_DIR/.env"

# Compose-file interpolation needs these even for --no-deps runs (see e2e-restore.sh).
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.15.0}"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
PROXY_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
AGGKIT_SYNC_TIMEOUT="${AGGKIT_SYNC_TIMEOUT:-300}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

# ANSI-strip for docker-log grepping (aggkit logs are colorized; raw regexes break —
# see docs/e2e log-assertion traps).
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

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

pgq() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX -c "$1"
}

rpc() { # method params-json
    curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$1\",\"params\":$2,\"id\":1}"
}

command -v cast >/dev/null || fail "cast (foundry) not found"
command -v psql >/dev/null || fail "psql not found"
command -v jq   >/dev/null || fail "jq not found"
command -v xxd  >/dev/null || fail "xxd not found"
pgq "SELECT 1" >/dev/null || fail "PostgreSQL not reachable"
rpc eth_chainId '[]' >/dev/null || fail "L2 (miden-agglayer) not reachable"

log "======================================================================"
log "  E2E: synthesized-claim FULL calldata → aggkit full-claim parser"
log "======================================================================"

# ── Step 1: ensure a completed L1→L2 claim exists ────────────────────────────
step "1/6: ensure a completed L1→L2 claim exists"
EXISTING=$(pgq "SELECT 1 FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' LIMIT 1;")
if [[ -z "$EXISTING" ]]; then
    log "  no ClaimEvent yet — bootstrapping via e2e-l1-to-l2.sh"
    "$SCRIPT_DIR/e2e-l1-to-l2.sh" >/dev/null
fi

# The claim's global_index (first 32 data bytes) + block, from the latest ClaimEvent.
ROW=$(pgq "SELECT data || '|' || block_number FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' ORDER BY block_number DESC LIMIT 1;")
GI_HEX=$(echo "${ROW%%|*}" | sed -E 's/^0x([0-9a-f]{64}).*/\1/')
CLAIM_BLOCK="${ROW##*|}"
[[ ${#GI_HEX} -eq 64 ]] || fail "could not extract global_index from synthetic_logs"
NOTE_ID=$(pgq "SELECT note_id FROM claim_watcher_processed WHERE global_index = decode('${GI_HEX}', 'hex') LIMIT 1;")
[[ -n "$NOTE_ID" ]] || fail "no claim_watcher_processed row for gi 0x${GI_HEX} — cannot derive the synthetic hash"
log "  claim: gi=0x${GI_HEX:0:16}… block=${CLAIM_BLOCK} note_id=${NOTE_ID:0:16}…"

# The derived hash this claim will ride after restore: keccak(TAG ‖ note_id_str), with
# TAG = "miden-agglayer/manual-claim/v1\0" (claim_watcher::MANUAL_CLAIM_TX_HASH_TAG) and
# note_id_str hashed as its ASCII bytes (hasher.update(note_id_str.as_bytes())).
TAG_HEX=$(printf 'miden-agglayer/manual-claim/v1\0' | xxd -p | tr -d '\n')
NOTE_ASCII_HEX=$(printf '%s' "$NOTE_ID" | xxd -p | tr -d '\n')
DERIVED_HASH=$(cast keccak "0x${TAG_HEX}${NOTE_ASCII_HEX}")
log "  expected derived tx hash: ${DERIVED_HASH}"

# ── Step 2: wipe PG state INCLUDING the durable eth-side tables ──────────────
step "2/6: wiping PG state incl. transactions/tx_note_links (the MA#27 crash-loss)"
# e2e-restore.sh wipes only the miden-derived set and deliberately PRESERVES
# `transactions` + `tx_note_links` (pre-fix, claim calldata was unrecoverable from
# Miden). This test wipes them TOO — that is the MA#27 condition (real claim tx +
# link lost), and the point of the fix: restore now recovers the FULL calldata from
# the CLAIM note storage + faucet registry (rebuilt in restore Phase 1.7, before the
# Phase 2.5 claim replay) and persists it under the derived hash.
pgq "TRUNCATE service_state, synthetic_logs, ger_entries, nonces, claimed_indices, \
     address_mappings, bridge_out_processed, faucet_registry, transactions, \
     tx_note_links, claim_watcher_processed CASCADE" >/dev/null
pgq "INSERT INTO service_state (id) VALUES (1)" >/dev/null
[[ "$(pgq 'SELECT COUNT(*) FROM synthetic_logs')" -eq 0 ]] || fail "tables not wiped"
log "  wiped (incl. transactions + tx_note_links)"

# ── Step 3: run --restore, restart the proxy ─────────────────────────────────
step "3/6: running --restore (re-synthesizes ClaimEvents under derived hashes)"
docker stop "$PROXY_CONTAINER" >/dev/null
# One-shot restore container: compose gives it volumes/network, but NOT the service's
# command-line args — the node URL must be passed explicitly or the binary dials its
# 127.0.0.1 default and retries forever (same wiring as e2e-restore.sh).
docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" --env-file "$FIXTURES_DIR/.env" \
    run --rm --no-deps miden-agglayer \
    --miden-node=http://miden-node:57291 \
    --miden-store-dir=/var/lib/miden-agglayer-service \
    --restore 2>&1 | strip_ansi \
    | while IFS= read -r line; do echo "  [restore] $line"; done
RESTORE_EXIT=${PIPESTATUS[0]}
[[ "$RESTORE_EXIT" -eq 0 ]] || fail "--restore exited with code $RESTORE_EXIT"
docker start "$PROXY_CONTAINER" >/dev/null
wait_for "proxy back up" \
    "curl -sf $L2_RPC -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_chainId\",\"params\":[],\"id\":1}' >/dev/null" \
    60 2

# ── Step 4: the re-synthesized ClaimEvent rides the DERIVED hash ─────────────
step "4/6: verifying the re-synthesized ClaimEvent rides the derived hash"
NEW_TX=$(pgq "SELECT transaction_hash FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${GI_HEX}%' LIMIT 1;")
[[ -n "$NEW_TX" ]] || fail "ClaimEvent for gi 0x${GI_HEX:0:16}… not re-synthesized by --restore"
[[ "${NEW_TX,,}" == "${DERIVED_HASH,,}" ]] \
    || fail "re-synthesized ClaimEvent rides ${NEW_TX}, expected the derived hash ${DERIVED_HASH} — MA#27 condition not reproduced"
pass "derived-hash ClaimEvent reproduced (${NEW_TX:0:18}…)"

# ── Step 5: eth_getTransactionByHash serves FULL authoritative calldata ──────
step "5/6: verifying eth_getTransactionByHash serves full claimAsset calldata"
INPUT=$(rpc eth_getTransactionByHash "[\"${DERIVED_HASH}\"]" | jq -r '.result.input')
[[ -n "$INPUT" && "$INPUT" != "null" ]] || fail "no tx served for the derived hash"
[[ "$INPUT" != "0x" ]] || fail "derived-hash claim tx serves EMPTY calldata — the persisted-calldata path did not engage (check synthetic_claim_calldata_persisted_total / _unrecoverable_total)"

CLAIM_ASSET_SELECTOR=$(cast sig "claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)")
[[ "${INPUT:0:10}" == "$CLAIM_ASSET_SELECTOR" ]] \
    || fail "input does not start with the claimAsset selector (got ${INPUT:0:10}, want ${CLAIM_ASSET_SELECTOR})"

# globalIndex is arg 3: after two inline bytes32[32] arrays → byte offset 4+1024+1024,
# i.e. hex-char offset 10 + 2048*2 = 4106, 64 chars. Must equal the EVENT's gi (truthful).
GI_IN_CALLDATA="${INPUT:4106:64}"
[[ "${GI_IN_CALLDATA,,}" == "${GI_HEX,,}" ]] \
    || fail "calldata globalIndex (${GI_IN_CALLDATA}) != event globalIndex (${GI_HEX}) — NOT the authoritative claim"

# Full-length sanity: selector + 11 args with two 32-word proof arrays ≥ 4+69*32 bytes.
MIN_HEX_LEN=$((2 + 2 * (4 + 69 * 32)))
[[ ${#INPUT} -ge $MIN_HEX_LEN ]] || fail "calldata too short (${#INPUT} hex chars) — proofs missing?"

# Proof material must be present and non-zero (the local SMT proof of a real deposit is
# never all-zero) — pins that proofs come from the note storage, not zero-fill.
LOCAL_PROOF_HEX="${INPUT:10:2048}"
[[ "$LOCAL_PROOF_HEX" =~ [1-9a-f] ]] \
    || fail "local SMT proof in calldata is all-zero — fabrication, not the authoritative proof"
pass "full authoritative claimAsset calldata served ($(( (${#INPUT} - 2) / 2 )) bytes, truthful gi, non-zero proofs)"

# ── Step 6: aggkit parses it, syncs past the block, cert settles ─────────────
step "6/6: aggkit full-claim parser + certificate pipeline"
AGGKIT_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
docker restart "$AGGKIT_CONTAINER" >/dev/null
sleep 5

# (a) L2BridgeSyncer must get PAST the claim block: its processed block exceeds it.
wait_for "aggkit L2BridgeSyncer past claim block ${CLAIM_BLOCK}" \
    "docker logs --since $AGGKIT_START $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep -oE 'L2BridgeSyncer.*block[ =:]+[0-9]+' | grep -oE '[0-9]+$' | sort -n | tail -1 | awk '{exit !(\$1 > ${CLAIM_BLOCK})}'" \
    "$AGGKIT_SYNC_TIMEOUT" 10

# (b) ZERO calldata-parse failures — the exact soak wedge signature.
if docker logs --since "$AGGKIT_START" "$AGGKIT_CONTAINER" 2>&1 | strip_ansi \
    | grep -q "input too short"; then
    fail "aggkit logged 'input too short' — a claim tx still serves unparsable calldata"
fi
pass "aggkit synced past block ${CLAIM_BLOCK} with zero claim-calldata parse errors"

# (c) certificate pipeline alive: a cert reaches Settled after the restart window.
wait_for "certificate settled" \
    "docker logs --since $AGGKIT_START $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep 'changed status.*Settled' | grep -q 'NewLocalExitRoot'" \
    "$AGGKIT_SYNC_TIMEOUT" 10
pass "certificate settled — pipeline unwedged"

log "======================================================================"
log "  PASS: synthesized claim serves authoritative full calldata;"
log "        aggkit parses it, syncs past block ${CLAIM_BLOCK}, certs settle."
log "======================================================================"
