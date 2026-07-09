#!/usr/bin/env bash
# E2E: foreign-bridge claim provenance — a SECOND miden-agglayer deployment on
# the SAME Miden chain must not leak ClaimEvents into our synthetic_logs.
#
# Reproduces the live incident (read-only reindex of the real testnet, which
# hosts a foreign miden-agglayer deployment): the foreign deployment's consumed
# CLAIM notes share our ClaimNote script root, and pre-fix `project_claim_note`
# gated ONLY on that root — 3 foreign ClaimEvents were projected into our
# synthetic_logs. The fix (restore.rs::classify_claim_note) requires a claim to
# be provably OURS: consumed by OUR bridge, or minted by OUR service targeting
# OUR bridge. Foreign claims are skipped fail-closed with
# `claim_event_foreign_skipped_total`.
#
# Topology driven here (all LIVE on-chain, no store surgery):
#   1. Deploy a fully independent FOREIGN deployment on the same Miden chain:
#      foreign service + ger_manager wallets, foreign bridge account
#      (network id 2 — an id our stack does NOT serve), foreign ETH faucet
#      registered in the FOREIGN bridge (bridge-out-tool --create-foreign-bridge,
#      isolated store — the proxy's store is never touched).
#   2. FABRICATE the foreign deposit's world locally — no L1 deposit, no
#      bridge-service. The foreign deployment is fully ours, so its exit
#      roots are whatever its ger_manager injects: build the LxLy leaf hash
#      (keccak256 of the 113-byte packed leaf, exactly what the bridge MASM's
#      leaf_utils::compute_leaf_value hashes), fold it up a depth-32 tree at
#      index=cnt with all-zero siblings (bridge_in.masm::calculate_root fold:
#      bit 0 → keccak(node||sib), bit 1 → keccak(sib||node)) to get a
#      mainnet_exit_root that covers exactly our leaf; rollup_exit_root = 0.
#      Our L1 never saw this leaf and our aggkit can never claim it — the
#      foreign claim is the ONLY claimant of its global index, exactly like
#      the real incident's foreign deposits.
#   3. Build the claimAsset calldata from the fabricated proof (32 zero
#      siblings verify by construction) and drive the claim through the
#      FOREIGN bridge (--submit-foreign-claim: inject keccak(mainnet||rollup)
#      — the fabricated GER — via the foreign ger_manager, mint the CLAIM
#      from the foreign service targeting the foreign bridge, wait for LIVE
#      consumption by the foreign bridge network account; the MASM validates
#      the merkle proof against the injected GER and accepts).
#   4. Our proxy's note-visibility reconciler imports the foreign tag-0 notes
#      and the projector evaluates the consumed CLAIM — the provenance gate
#      must skip it.
#
# Assertions:
#   (a) claim_event_foreign_skipped_total >= baseline+1 on /metrics — the gate
#       fired. (Also exercises the /metrics second-runtime fix on this branch:
#       the counter is emitted from the MidenClient runtime thread.)
#   (b) ZERO synthetic_logs ClaimEvent rows carry the foreign claim's global
#       index (PG, topic 0x1df3f2a9...).
#   (c) Our OWN claim still emits: >= 1 ClaimEvent row exists for a non-foreign
#       global index (bootstrapped via e2e-l1-to-l2.sh when missing).
#   (d) Proxy healthy: synthetic tip still advancing, /metrics serving.
#
# Usage:  ./scripts/e2e-claim-provenance.sh    (stack must be up: make e2e-up)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

# kept for parity with sibling scripts (e2e-l1-to-l2.sh bootstrap re-sources it)
source "$FIXTURES_DIR/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
# The foreign deployment's AggLayer network id. MUST differ from our stack's
# (NETWORK_ID, default 1): our aggkit then never auto-claims the deposit, so
# the foreign claim is the only claimant of its global index.
FOREIGN_NETWORK_ID="${FOREIGN_NETWORK_ID:-2}"
DEPOSIT_AMOUNT="10000000000000" # 10^13 wei → 1000 Miden units at scale 10^10
# How long we give the proxy's reconciler+projector to observe the consumed
# foreign CLAIM and fire the gate (sweep tick is 5s; import must precede the
# projector's consumption discovery).
GATE_TIMEOUT_SECS="${GATE_TIMEOUT_SECS:-600}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

command -v cast    >/dev/null || fail "cast (foundry) not found"
command -v psql    >/dev/null || fail "psql not found (apt-get install postgresql-client)"
command -v curl    >/dev/null || fail "curl not found"
command -v python3 >/dev/null || fail "python3 not found"

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)
# stderr dropped: locale-warning noise corrupts captures (see sibling scripts).
pgq() { "${PSQL[@]}" -c "$1" 2>/dev/null; }

# Prometheus counter from the proxy's /metrics (0 when absent). State/metric
# assertions over log greps throughout — docker-log field regexes are fragile
# (ANSI escapes / format drift; see e2e log-assertion history).
counter() {
    local name="$1" value
    value=$(curl -s "${L2_RPC}/metrics" | awk -v n="$name" '
        $0 ~ ("^" n " ") { print $2; found=1; exit }
        END { if (!found) print 0 }
    ')
    echo "${value%.*}"
}

l2_tip() {
    curl -sf -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'
}

# ── Isolated store for the FOREIGN deployment ────────────────────────────────
# Own subdir (not the shared e2e-suite wallet store): the foreign deployment
# is a separate trust domain with its own keys, exactly like production.
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-claim-provenance}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
# ALWAYS start from a clean store (task #26). The foreign deployment is
# re-provisioned from scratch every run, so reuse has no value — and a store
# surviving a stack recreation carries the OLD chain's genesis commitment,
# which miden-client presents in its gRPC Accept header; the new node rejects
# the connection outright (AcceptHeaderError/NoSupportedMediaRange, displayed
# as a bare "RPC error"). One such stale store wedged this test on 7
# consecutive cert runs. The sibling tests (private-note, cantina13) already
# defend via B2AGG_FRESH=1; this test uses iso_tool directly and must wipe
# itself. `_iso_wipe_store` falls back to a root container for the
# container-created root-owned files.
_iso_wipe_store
mkdir -p "$B2AGG_STORE_DIR/tmp"

log "======================================================================"
log "  Foreign-Bridge Claim Provenance E2E"
log "======================================================================"

# ── Step 0: Positive-control precondition — OUR ClaimEvent exists ───────────
# Assertion (c) needs at least one legitimate ClaimEvent from our own
# deployment; bootstrap the standard L1→L2 flow when the stack is fresh.
step "Ensuring a legitimate OWN ClaimEvent exists (positive control)"
OWN_ROWS=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
if [[ "${OWN_ROWS:-0}" -lt 1 ]]; then
    log "no ClaimEvent yet — bootstrapping via e2e-l1-to-l2.sh"
    "$SCRIPT_DIR/e2e-l1-to-l2.sh"
    OWN_ROWS=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
fi
[[ "${OWN_ROWS:-0}" -ge 1 ]] || fail "no own ClaimEvent even after bootstrap"
log "own ClaimEvent rows: $OWN_ROWS"

# ── Step 1: Baselines ────────────────────────────────────────────────────────
step "Snapshotting baselines (/metrics + PG + tip)"
BASE_FOREIGN_SKIPPED=$(counter claim_event_foreign_skipped_total)
BASE_TIP=$(l2_tip) || fail "proxy eth_blockNumber unreachable"
log "  claim_event_foreign_skipped_total = $BASE_FOREIGN_SKIPPED"
log "  synthetic tip                     = $BASE_TIP"

# ── Step 2: Deploy the FOREIGN deployment on the same chain ─────────────────
step "Deploying foreign deployment (service, ger_manager, bridge net=$FOREIGN_NETWORK_ID, ETH faucet)"
FB_OUT=$(iso_tool --create-foreign-bridge --foreign-network-id "$FOREIGN_NETWORK_ID" 2>&1) \
    || { echo "$FB_OUT" | tail -30 >&2; fail "--create-foreign-bridge failed"; }
FOREIGN_SERVICE_ID=$(echo "$FB_OUT" | grep "service-id:" | awk '{print $NF}')
FOREIGN_GER_MANAGER_ID=$(echo "$FB_OUT" | grep "ger-manager-id:" | awk '{print $NF}')
FOREIGN_BRIDGE_ID=$(echo "$FB_OUT" | grep -w "bridge-id:" | awk '{print $NF}')
FOREIGN_FAUCET_ID=$(echo "$FB_OUT" | grep "faucet-id:" | awk '{print $NF}')
[[ -n "$FOREIGN_SERVICE_ID" && -n "$FOREIGN_GER_MANAGER_ID" && -n "$FOREIGN_BRIDGE_ID" && -n "$FOREIGN_FAUCET_ID" ]] \
    || { echo "$FB_OUT" | tail -30 >&2; fail "could not parse foreign deployment ids"; }
log "  foreign service:     $FOREIGN_SERVICE_ID"
log "  foreign ger_manager: $FOREIGN_GER_MANAGER_ID"
log "  foreign bridge:      $FOREIGN_BRIDGE_ID"
log "  foreign faucet:      $FOREIGN_FAUCET_ID"

# Deposit destination: zero-padded eth-address form of the foreign service id
# (same decodable mapping the suite's DEST_ADDR uses — the MASM claim path
# must be able to decode it into a Miden account id).
FS_INNER="${FOREIGN_SERVICE_ID#0x}"
FOREIGN_DEST="0x00000000${FS_INNER:0:16}${FS_INNER:16:14}00"
log "  foreign deposit dest: $FOREIGN_DEST"

# ── Step 3: FABRICATE the foreign deposit's leaf, proof and exit roots ──────
# No L1 deposit, no bridge-service (its sync model only covers network-1
# destinations — a net-2 leaf never becomes "synchronized", observed live).
# The foreign deployment trusts whatever GER its own ger_manager injects, so
# we fabricate a self-consistent world:
#
#   leaf  = keccak256(abi.encodePacked(leafType u8, origNet u32, origAddr,
#           destNet u32, destAddr, amount u256, keccak256(metadata)))
#           — 113 bytes, the exact preimage bridge MASM leaf_utils.masm
#           (compute_leaf_value, LEAF_DATA_BYTES=113) hashes on consumption.
#   root  = fold(leaf, index=cnt, 32 all-zero siblings) — the calculate_root
#           fold in bridge_in.masm: index bit 0 → keccak(node||sibling),
#           bit 1 → keccak(sibling||node), index >>= 1 per level.
#   GER   = keccak256(mainnet_exit_root || rollup_exit_root), rollup = 0 —
#           computed by bridge-out-tool from the calldata roots and injected
#           into the FOREIGN bridge (src/ger.rs::combined_ger).
step "Fabricating foreign leaf + depth-32 proof (index unique per run)"
# Leaf index / deposit count: unique per run (u32; also keeps the PG global-
# index assertion collision-free across runs). Global index = 2^64 + cnt:
# mainnet flag 1, rollup index 0, leaf index cnt — the exact shape
# process_global_index_mainnet accepts.
DEPOSIT_CNT=$(date +%s)
GLOBAL_INDEX=$(python3 -c "print(2**64 + $DEPOSIT_CNT)")

ZERO_WORD="$(printf '0%.0s' {1..64})" # 32 zero bytes (one merkle sibling)
EMPTY_METADATA_HASH=$(cast keccak 0x) # keccak256("") — native ETH, no metadata
AMOUNT_HEX=$(printf '%064x' "$DEPOSIT_AMOUNT")
LEAF_PACKED="0x00$(printf '%08x' 0)$(printf '0%.0s' {1..40})$(printf '%08x' "$FOREIGN_NETWORK_ID")${FOREIGN_DEST#0x}${AMOUNT_HEX}${EMPTY_METADATA_HASH#0x}"
[[ ${#LEAF_PACKED} -eq 228 ]] || fail "packed leaf is $(( (${#LEAF_PACKED}-2)/2 )) bytes, want 113"
LEAF=$(cast keccak "$LEAF_PACKED")
log "  leaf index (deposit_cnt): $DEPOSIT_CNT"
log "  leaf value:               $LEAF"

# Mainnet exit root: fold the leaf up 32 levels against all-zero siblings.
NODE="${LEAF#0x}"
IDX="$DEPOSIT_CNT"
for _ in $(seq 1 32); do
    if (( IDX & 1 )); then
        NODE=$(cast keccak "0x${ZERO_WORD}${NODE}")
    else
        NODE=$(cast keccak "0x${NODE}${ZERO_WORD}")
    fi
    NODE="${NODE#0x}"
    IDX=$(( IDX >> 1 ))
done
MAINNET_EXIT_ROOT="0x${NODE}"
ROLLUP_EXIT_ROOT="0x${ZERO_WORD}"
SMT_LOCAL=$(python3 -c "print('[' + ','.join(['0x' + '00'*32]*32) + ']')")
SMT_ROLLUP="$SMT_LOCAL"
log "  mainnet_exit_root = $MAINNET_EXIT_ROOT"
log "  rollup_exit_root  = $ROLLUP_EXIT_ROOT"
pass "fabricated world ready: globalIndex=$GLOBAL_INDEX (mainnet flag + leaf $DEPOSIT_CNT)"

# ── Step 4: Build claimAsset calldata (identical shape to the L1 siblings) ──
step "Building claimAsset calldata for the FOREIGN claim"
CALLDATA=$(cast calldata \
    'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
    "$SMT_LOCAL" "$SMT_ROLLUP" "$GLOBAL_INDEX" \
    "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
    0 0x0000000000000000000000000000000000000000 \
    "$FOREIGN_NETWORK_ID" "$FOREIGN_DEST" \
    "$DEPOSIT_AMOUNT" 0x)
echo "$CALLDATA" > "$B2AGG_STORE_DIR/foreign-claim-calldata.hex"
log "  calldata: ${#CALLDATA} hex chars → $B2AGG_STORE_DIR/foreign-claim-calldata.hex"

# ── Step 5: Drive the claim through the FOREIGN bridge (live consumption) ───
step "Submitting foreign GER inject + CLAIM (waiting for the foreign bridge to consume)"
FC_OUT=$(iso_tool --submit-foreign-claim \
    --claim-calldata-file /store/foreign-claim-calldata.hex \
    --foreign-bridge-id "$FOREIGN_BRIDGE_ID" \
    --foreign-service-id "$FOREIGN_SERVICE_ID" \
    --foreign-ger-manager-id "$FOREIGN_GER_MANAGER_ID" \
    --scale-exp 10 2>&1) \
    || { echo "$FC_OUT" | tail -40 >&2; fail "--submit-foreign-claim failed (foreign bridge did not consume)"; }
echo "$FC_OUT" | tail -8
FOREIGN_GI=$(echo "$FC_OUT" | grep "global-index:" | awk '{print $NF}')
FOREIGN_NOTE_COMMITMENT=$(echo "$FC_OUT" | grep "note-commitment:" | awk '{print $NF}')
[[ -n "$FOREIGN_GI" && -n "$FOREIGN_NOTE_COMMITMENT" ]] \
    || fail "could not parse foreign claim identifiers from tool output"
FOREIGN_GI_HEX="${FOREIGN_GI#0x}"
pass "foreign bridge CONSUMED the claim (note commitment $FOREIGN_NOTE_COMMITMENT, gi $FOREIGN_GI)"

# ── Step 6: Wait for our proxy's provenance gate to evaluate it ──────────────
# The note-visibility reconciler imports the foreign tag-0 notes; once the
# consumption is discovered the projector runs the CLAIM derivation and the
# gate must skip it, incrementing the counter. Metric-first (fixed on this
# branch — emitted from the MidenClient runtime and now rendered reliably).
step "Waiting for claim_event_foreign_skipped_total to advance (≤${GATE_TIMEOUT_SECS}s)"
ELAPSED=0
NEW_FOREIGN_SKIPPED="$BASE_FOREIGN_SKIPPED"
while [[ "$ELAPSED" -lt "$GATE_TIMEOUT_SECS" ]]; do
    NEW_FOREIGN_SKIPPED=$(counter claim_event_foreign_skipped_total)
    [[ "$NEW_FOREIGN_SKIPPED" -gt "$BASE_FOREIGN_SKIPPED" ]] && break
    sleep 5; ELAPSED=$((ELAPSED + 5)); echo -n "."
done
echo ""
if [[ "$NEW_FOREIGN_SKIPPED" -le "$BASE_FOREIGN_SKIPPED" ]]; then
    warn "diagnostics: tip=$(l2_tip 2>/dev/null || echo '?') foreign_skipped=$NEW_FOREIGN_SKIPPED (base $BASE_FOREIGN_SKIPPED)"
    warn "if the foreign CLAIM was consumed before the reconciler imported it, miden-client"
    warn "drops the import (spent-before-import applies to B2AGG recovery only) — the gate"
    warn "then never evaluates the note. Re-run against a quieter stack before treating as regression."
    fail "provenance gate did not fire within ${GATE_TIMEOUT_SECS}s (claim_event_foreign_skipped_total stuck at $NEW_FOREIGN_SKIPPED)"
fi
pass "gate fired: claim_event_foreign_skipped_total $BASE_FOREIGN_SKIPPED → $NEW_FOREIGN_SKIPPED"

# ── Step 7: PG assertions — the load-bearing state checks ────────────────────
step "Asserting synthetic_logs state"
# (b) ZERO ClaimEvent rows for the foreign global index. ABI data layout of
# ClaimEvent: word 0 = globalIndex — prefix-match on the data column.
FOREIGN_ROWS=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) LIKE '0x${FOREIGN_GI_HEX}%';")
[[ "$FOREIGN_ROWS" == "0" ]] \
    || fail "FOREIGN CLAIM LEAKED: $FOREIGN_ROWS ClaimEvent row(s) carry the foreign global index 0x${FOREIGN_GI_HEX}"
pass "zero ClaimEvent rows for the foreign global index"

# (c) our own claim's row still present (and not somehow gated away).
OWN_ROWS_AFTER=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}' AND lower(data) NOT LIKE '0x${FOREIGN_GI_HEX}%';")
[[ "${OWN_ROWS_AFTER:-0}" -ge 1 ]] \
    || fail "positive control broken: no OWN ClaimEvent rows remain (gate over-eager?)"
pass "own ClaimEvent rows intact: $OWN_ROWS_AFTER"

# ── Step 8: Proxy health ─────────────────────────────────────────────────────
step "Proxy health: tip advancing + /metrics serving"
TIP_A=$(l2_tip) || fail "eth_blockNumber unreachable after test"
sleep 12
TIP_B=$(l2_tip) || fail "eth_blockNumber unreachable after test"
[[ "$TIP_B" -gt "$TIP_A" ]] || fail "synthetic tip frozen at $TIP_A — proxy unhealthy after foreign-claim exposure"
pass "tip advancing: $TIP_A → $TIP_B"

log "======================================================================"
log "  FOREIGN-BRIDGE CLAIM PROVENANCE PASS"
log "    foreign bridge:                    $FOREIGN_BRIDGE_ID (network $FOREIGN_NETWORK_ID)"
log "    foreign claim gi:                  $FOREIGN_GI"
log "    claim_event_foreign_skipped_total: $BASE_FOREIGN_SKIPPED → $NEW_FOREIGN_SKIPPED"
log "    foreign ClaimEvent rows:           $FOREIGN_ROWS (must be 0)"
log "    own ClaimEvent rows:               $OWN_ROWS_AFTER"
log "======================================================================"
