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
#   6. RESET aggkit (force-recreate → empty bridgesync DB, no stale cursor) and prove,
#      un-false-passably, that it re-syncs THIS claim:
#        (a) proxy exact-hash serves COUNT-DELTA across the reset (>SERVES_BEFORE) so the
#            script's own step-5 probe cannot satisfy it — a genuinely aggkit-driven fetch;
#        (b) durable persist, HARD + image-independent: the recovered claim's EXACT
#            global_index is delivered (claim_tx_hash set) in aggkit's bridge-service REST
#            index (no docker-exec into the distroless image), plus the atomic per-block
#            cursor-advance floor past the claim block;
#        (c) a certificate reaches Settled AFTER the reset with the recovered claim's exact
#            global_index still bound+delivered — settlement tied to THIS claim, not to an
#            unrelated bridge-out — and ZERO 'input too short' throughout (the wedge cleared).
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
# aggkit bridge-service REST API (image-independent: plain HTTP, NOT docker-exec into the
# distroless aggkit image). `/bridges/<addr>` → {deposits:[{global_index, claim_tx_hash, …}]}.
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
# How much of the (un-reset) proxy log to scan for exact-hash serves. NOT `--since`
# (host-timestamp truncation trap, see docs/e2e log-assertion traps).
PROXY_LOG_TAIL="${PROXY_LOG_TAIL:-20000}"
AGGKIT_LOG_TAIL="${AGGKIT_LOG_TAIL:-40000}"

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

# ── Step 6: aggkit RE-SYNCS the claim from a reset DB, parses it, persists it ──
#
# CRITICAL (review blocker 2): a `docker restart` PRESERVES the container filesystem, and
# aggkit stores its bridgesync DB under PathRWData=/tmp (no named volume) — so a restarted
# aggkit RESUMES past the already-processed claim block and NEVER re-fetches the
# derived-hash tx. That made the old assertions vacuous (it was "already past", never
# re-parsed). We instead RESET aggkit's sync state by RECREATING the container (fresh
# /tmp), forcing a full re-sync that MUST re-fetch and re-parse the derived-hash claim.
#
# Three review-blocker-2 hardenings applied below:
#   (a) the exact-hash serve proof is COUNT-DELTA'd across the force-recreate — the proxy
#       is NOT reset, so the script's OWN step-5 eth_getTransactionByHash serve is already
#       in the log BEFORE the recreate; we snapshot that count and require it to STRICTLY
#       INCREASE afterwards, so only a genuinely NEW (aggkit-driven) serve can pass.
#   (b) the durable-persist proof is IMAGE-INDEPENDENT and keyed to THIS claim's exact
#       global_index via the bridge-service REST API (no docker-exec into the distroless
#       aggkit image), plus the atomic-per-block cursor-advance floor — both HARD.
#   (c) settlement is tied to THIS recovered claim by its global_index (bridge-service
#       delivery of the exact gi + a certificate settling AFTER the reset), NOT to an
#       unrelated bridge-out.

# The recovered claim's destination address (claimAsset arg #9, `address destinationAddress`)
# — needed to locate THIS claim in the bridge-service deposit index. Layout after the two
# inline bytes32[32] arrays: selector(4) + 1024 + 1024 = byte 2052; then six 32-byte words
# (globalIndex, mainnetExitRoot, rollupExitRoot, originNetwork, originTokenAddress,
# destinationNetwork) → destinationAddress word starts at byte 2052+192 = 2244, address in
# its low 20 bytes (word bytes 12..32 → byte 2256). Hex offset = 2 (for "0x") + 2*byte.
CLAIM_DEST_ADDR="0x${INPUT:4514:40}"
[[ "$CLAIM_DEST_ADDR" =~ ^0x[0-9a-fA-F]{40}$ ]] \
    || fail "could not extract destinationAddress from claimAsset calldata (got '$CLAIM_DEST_ADDR')"
GI_DEC=$(cast to-dec "0x${GI_HEX}" 2>/dev/null || echo "")
[[ -n "$GI_DEC" ]] || fail "could not convert global_index 0x${GI_HEX} to decimal"
log "  recovered claim: dest=${CLAIM_DEST_ADDR} global_index(dec)=${GI_DEC}"

DERIVED_HASH_LC=$(echo "$DERIVED_HASH" | tr '[:upper:]' '[:lower:]')

# (a-pre) Snapshot how many times the (un-reset) proxy has ALREADY served this exact derived
# hash — this includes the script's OWN step-5 eth_getTransactionByHash call. Gate (a2) below
# requires the count to STRICTLY exceed this, so the script's own serve can never pass it.
serve_count() {
    docker logs --tail "$PROXY_LOG_TAIL" "$PROXY_CONTAINER" 2>&1 | strip_ansi \
        | grep -iF 'served stored tx' | grep -icF "$DERIVED_HASH_LC" || true
}
SERVES_BEFORE=$(serve_count)
log "  proxy has served the derived hash ${SERVES_BEFORE} time(s) pre-reset (incl. step-5's own probe)"

step "6/6: RESET aggkit (force-recreate → empty bridgesync DB) and re-sync the claim"
docker compose -f docker-compose.e2e.yml -f docker-compose.l2l2.yml --env-file fixtures/.env \
    up -d --force-recreate --no-deps aggkit >/dev/null 2>&1 \
    || docker compose -f docker-compose.e2e.yml --env-file fixtures/.env \
        up -d --force-recreate --no-deps aggkit >/dev/null 2>&1 \
    || fail "could not force-recreate $AGGKIT_CONTAINER to reset its bridgesync DB"
sleep 5

# (a) RESET proof: force-recreate wiped aggkit's bridgesync DB (/tmp, no volume), so its
# L2BridgeSyncer MUST restart at lastProcessedBlock 0 — a stale cursor is impossible. We scan
# with `--tail` (NOT `--since $host_timestamp`, a truncation trap — see docs/e2e log traps).
wait_for "aggkit L2BridgeSyncer RESET to block 0 (force-recreate wiped its DB — no stale cursor)" \
    "docker logs --tail $AGGKIT_LOG_TAIL $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep 'lastProcessedBlock 0' | grep -q 'L2BridgeSyncer'" \
    60 5

# (a1) RE-PROCESS proof: from the reset, aggkit re-processes the EXACT claim block from
# scratch (it re-parses the persisted derived-hash calldata there; the pre-fix build wedges
# at this block on 'input too short' — gate (c)).
wait_for "aggkit re-PROCESSED the exact claim block ${CLAIM_BLOCK} from the reset" \
    "docker logs --tail $AGGKIT_LOG_TAIL $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep -qE 'block ${CLAIM_BLOCK} processed'" \
    "$AGGKIT_SYNC_TIMEOUT" 5
pass "aggkit reset to block 0 and re-processed the exact claim block ${CLAIM_BLOCK} — no stale cursor"

# (a2) EXACT-HASH FETCH — COUNT-DELTA (review blocker 2a): re-processing block ${CLAIM_BLOCK}
# from the reset, aggkit MUST fetch THIS claim's calldata by its derived hash. The proxy logs
# every stored tx it serves by exact hash. We already snapshotted SERVES_BEFORE (which
# INCLUDES the script's own step-5 probe); require the count to STRICTLY INCREASE — a serve
# that can ONLY have come from aggkit's post-reset re-fetch, never from the script itself.
# Correlated with (a1): aggkit demonstrably re-processed the block, so the new serve is its.
wait_for "proxy served the EXACT derived-hash claim tx to aggkit AFTER the reset (delta > ${SERVES_BEFORE})" \
    "[ \"\$(serve_count)\" -gt \"$SERVES_BEFORE\" ]" \
    "$AGGKIT_SYNC_TIMEOUT" 5
SERVES_AFTER=$(serve_count)
pass "proxy served the exact derived-hash claim ${SERVES_AFTER} time(s) (was ${SERVES_BEFORE}) — a NEW aggkit-driven fetch (${DERIVED_HASH_LC:0:18}…)"

# (b) PERSIST — cursor floor (HARD, image-independent): bridgesync commits each block's rows
# (including this claim) in ONE transaction and only THEN advances lastProcessedBlock, so
# advancing PAST ${CLAIM_BLOCK} is itself proof the claim row was durably committed. The
# pre-fix build wedges AT ${CLAIM_BLOCK} on 'input too short' and never advances.
wait_for "aggkit L2BridgeSyncer re-processed PAST claim block ${CLAIM_BLOCK} (block committed)" \
    "docker logs --tail $AGGKIT_LOG_TAIL $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep -oE 'L2BridgeSyncer.*block[ =:]+[0-9]+' | grep -oE '[0-9]+$' | sort -n | tail -1 | awk '{exit !(\$1 > ${CLAIM_BLOCK})}'" \
    "$AGGKIT_SYNC_TIMEOUT" 10

# (b2) DURABLE CLAIM ROW — HARD, IMAGE-INDEPENDENT (review blocker 2b). The distroless aggkit
# image has no sh/sqlite3, so a `docker exec sqlite3` probe silently skips — not a gate. We
# instead read aggkit's OWN bridge-service REST index (plain HTTP), keyed to THIS claim's
# EXACT global_index, and require the recovered claim to be present AND delivered
# (claim_tx_hash set). The bridge-service reads from the same bridgesync DB aggkit just
# re-populated from the reset, so a present+delivered row here is a durable persist of the
# recovered claim. A reachable service that does NOT list the gi is a HARD FAIL — the
# exact-hash fetch (a2) is already proven, so a missing persist would be a real bug.
GI_IN_INDEX_PY=$(cat <<PYEOF
import json,sys
want=int("${GI_DEC}")
try:
    d=json.load(sys.stdin)
except Exception:
    sys.exit(2)
for dep in d.get("deposits",[]):
    gi=dep.get("global_index")
    if gi is None:
        continue
    try:
        gi_val=int(gi,0) if isinstance(gi,str) else int(gi)
    except Exception:
        continue
    if gi_val==want:
        cth=(dep.get("claim_tx_hash") or "")
        # exit 0 = present+delivered; exit 3 = present but NOT yet delivered (keep waiting)
        sys.exit(0 if cth not in ("","0x",None) else 3)
sys.exit(1)  # gi not present yet
PYEOF
)
# First require the service to be reachable at all (else we cannot make a hard claim).
wait_for "aggkit bridge-service reachable at $BRIDGE_SERVICE_URL" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$CLAIM_DEST_ADDR?limit=100' >/dev/null 2>&1" \
    60 5
wait_for "bridge-service durably lists the recovered claim (gi=${GI_DEC}) as DELIVERED (claim_tx_hash set)" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$CLAIM_DEST_ADDR?limit=100' 2>/dev/null | python3 -c '$GI_IN_INDEX_PY'" \
    "$AGGKIT_SYNC_TIMEOUT" 10
pass "bridge-service durably persisted the recovered claim gi=${GI_DEC} with a claim_tx_hash — image-independent, exact-gi tie"

# (c) ZERO calldata-parse failures — MEANINGFUL because aggkit genuinely re-parsed the block.
if docker logs --tail "$AGGKIT_LOG_TAIL" "$AGGKIT_CONTAINER" 2>&1 | strip_ansi \
    | grep -q "input too short"; then
    fail "aggkit logged 'input too short' — a claim tx still serves unparsable calldata"
fi
pass "aggkit re-synced past block ${CLAIM_BLOCK} (claim persisted) with zero parse errors"

# ── Step 6b: certificate settlement TIED to the recovered claim (review blocker 2c) ─────────
# The prior version drove a NEW, unrelated bridge-out and asserted "a cert settled" — which
# proves pipeline liveness but is NOT tied to the recovered claim. We now require a
# certificate to reach Settled AFTER the reset AND, in the SAME post-reset window, the
# recovered claim (by its EXACT global_index, gate b2) to be delivered — binding settlement
# to THIS claim's imported exit rather than to an unrelated exit. Driving one L2→L1 bridge-out
# supplies the fresh Local Exit Tree leaf a certificate needs to build (a ClaimEvent is an
# IMPORTED exit, not itself a LET leaf), but the tie asserted is the recovered gi, not that
# leaf.
step "6b: certificate settles AND the recovered claim (gi=${GI_DEC}) is bound to that window"
CERT_WINDOW_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
"$SCRIPT_DIR/e2e-l2-to-l1.sh" 2>&1 | strip_ansi | tail -5 \
    | while IFS= read -r line; do echo "  [l2-to-l1] $line"; done
[[ "${PIPESTATUS[0]}" -eq 0 ]] || fail "post-restore bridge-out (e2e-l2-to-l1.sh) failed"

# NB: a settled cert line carries BOTH roots — a fresh chain's PreviousLocalExitRoot is
# the empty-tree root, so a line-level `grep -v $EMPTY_LER` deletes the very line that
# proves settlement. Extract the NEW root and test it alone (the 9ac5c0e lesson).
EMPTY_LER="0x27ae5ba08d7291c96c8cbddcc148bf48a6d68c7974b94356f53754ef6171d757"
wait_for "certificate settled with non-empty exit root (post-reset)" \
    "docker logs --tail $AGGKIT_LOG_TAIL $AGGKIT_CONTAINER 2>&1 | strip_ansi | grep 'changed status.*Settled' | grep -oE 'NewLocalExitRoot: 0x[0-9a-fA-F]{64}' | grep -qv '$EMPTY_LER'" \
    "$AGGKIT_SYNC_TIMEOUT" 10

# The recovered claim must STILL be durably delivered (gi-keyed) after the full cert build —
# i.e. the settlement window did not drop or wedge THIS claim. This is the tie: a settled
# certificate coexists with the recovered claim's durable, delivered presence by exact gi.
curl -sf "$BRIDGE_SERVICE_URL/bridges/$CLAIM_DEST_ADDR?limit=100" 2>/dev/null \
    | python3 -c "$GI_IN_INDEX_PY" \
    || fail "recovered claim gi=${GI_DEC} not delivered in bridge-service after cert settlement — settlement not tied to the recovered claim"

# The wedge signature must STILL be absent after the full cert build consumed the claim window.
if docker logs --tail "$AGGKIT_LOG_TAIL" "$AGGKIT_CONTAINER" 2>&1 | strip_ansi \
    | grep -q "input too short"; then
    fail "aggkit logged 'input too short' during certificate build"
fi
pass "certificate settled (non-empty NewLocalExitRoot) with the recovered claim gi=${GI_DEC} bound and delivered — pipeline unwedged through THIS claim's window"

log "======================================================================"
log "  PASS: synthesized claim serves authoritative full calldata;"
log "        aggkit resets, re-fetches THIS derived hash, persists gi=${GI_DEC},"
log "        syncs past block ${CLAIM_BLOCK}, and a cert settles bound to it."
log "======================================================================"
