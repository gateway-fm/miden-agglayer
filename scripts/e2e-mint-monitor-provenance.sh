#!/usr/bin/env bash
# E2E: mint-monitor provenance — a FOREIGN miden-agglayer deployment minting on
# the SAME Miden chain must not raise a false Cantina #2 (mint-target) / #4
# (forged-mint) critical alert on OUR BridgeOutScanner monitors, AND our
# provenance GATE must be exercised end-to-end (positive skip evidence), AND the
# monitor must still ALERT on a genuinely-ours forged MINT (positive control).
#
# Companion to e2e-claim-provenance.sh (CLAIM path). THIS test covers the MINT
# path. CORRECTED PREMISE (second security re-review, blocker #3): the bridge
# MASM emits its MINT/BURN output notes with the DEFAULT (0) tag
# (`bridge_in_output.masm` / `bridge_out.masm` both `push.DEFAULT_TAG`) — the
# SAME tag-0 family our note-visibility reconciler sweeps. So a FOREIGN
# deployment's MINT, once its (network-account) faucet CONSUMES it, IS swept
# into our store and DOES reach `BridgeOutScanner::scan_consumed_notes_monitors`.
# The earlier claim that MINTs carry a non-zero tag (and therefore never reach
# us) was WRONG — which is exactly why the provenance gate is load-bearing and
# must be exercised, not assumed inert.
#
# The gate (src/bridge_out.rs::note_provenance → FOREIGN) classifies that
# consumed foreign MINT as another deployment's note and SKIPS it in the value
# monitors (bridge_mint_foreign_skipped_total++), instead of raising a false
# #2/#4 page.
#
# Topology: reuses the foreign-deployment machinery (bridge-out-tool
# --create-foreign-bridge + --submit-foreign-claim, isolated store — the proxy's
# store is never touched). Driving a claim through the FOREIGN bridge makes that
# bridge EMIT a foreign MINT (target = foreign faucet); the foreign faucet is a
# NETWORK account, so the stack's network-tx executor CONSUMES that MINT, and our
# reconciler then imports the consumed tag-0 note.
#
# Assertions:
#   (a) POSITIVE SKIP EVIDENCE — bridge_mint_foreign_skipped_total MUST INCREMENT
#       (the consumed foreign MINT reached our scanner and the gate skipped it).
#       Deleting the gate makes this assertion FAIL: the foreign MINT would
#       instead be treated as ours and trip #4.
#   (b) bridge_mint_target_mismatch_total UNCHANGED (Cantina #2 — no false page).
#   (c) bridge_forged_mint_total UNCHANGED across the FOREIGN mint (Cantina #4 —
#       no false page while the faucet is unregistered / foreign).
#   (d) POSITIVE ALERT CONTROL — allowlist the foreign faucet into OUR registry
#       (admin_registerNativeFaucet). The SAME imported MINT is now classified
#       OURS, its serial matches NO aggkit-recorded claim, so #4 MUST FIRE
#       (bridge_forged_mint_total INCREMENTS). Proves the monitor is not merely
#       always-quiet — a real forged case that SHOULD alert does. Deleting the
#       #4 reconcile makes this assertion FAIL.
#   (e) Proxy healthy: synthetic tip advancing, /metrics serving.
#
# Usage:  ./scripts/e2e-mint-monitor-provenance.sh   (stack must be up: make e2e-up)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
# Foreign AggLayer network id — MUST differ from our stack's (NETWORK_ID, default
# 1) so our aggkit never auto-claims the fabricated deposit.
FOREIGN_NETWORK_ID="${FOREIGN_NETWORK_ID:-2}"
DEPOSIT_AMOUNT="10000000000000" # 10^13 wei -> 1000 Miden units at scale 10^10
# Admin bearer for admin_registerNativeFaucet (positive alert control, step 7).
: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY (needed for the positive alert control)}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"
# Max time to wait for (a) the consumed foreign MINT to be imported+skipped and
# (d) the reclassified MINT's forged alert to fire (grace window ~10 sync ticks).
SKIP_WAIT_SECS="${SKIP_WAIT_SECS:-180}"
FORGED_WAIT_SECS="${FORGED_WAIT_SECS:-180}"
# Window over which #2/#4 must stay FLAT while the faucet is still foreign.
OBSERVE_SECS="${OBSERVE_SECS:-60}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
skip() { echo -e "${YELLOW}[$(date +%H:%M:%S)] SKIP:${NC} $*"; exit 0; }

command -v cast    >/dev/null || fail "cast (foundry) not found"
command -v psql    >/dev/null || fail "psql not found (apt-get install postgresql-client)"
command -v curl    >/dev/null || fail "curl not found"
command -v python3 >/dev/null || fail "python3 not found"

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)
# stderr dropped: locale-warning noise corrupts captures (see sibling scripts).
pgq() { "${PSQL[@]}" -c "$1" 2>/dev/null; }

# Sum all series for a Prometheus counter, labelled or unlabelled (0 when absent).
sum_counter_series() {
    local name="$1"
    awk -v n="$name" '
        $1 == n || index($1, n "{") == 1 { total += $2; found=1 }
        END { printf "%.0f\n", found ? total : 0 }
    '
}

counter() {
    local name="$1"
    curl -s "${L2_RPC}/metrics" | sum_counter_series "$name"
}

# Guard the parser against silently dropping reason-labelled counter series.
COUNTER_SUM_GUARD=$(printf '%s\n' \
    'bridge_forged_mint_total 2' \
    'bridge_forged_mint_total{reason="missing_claim"} 3' \
    'bridge_forged_mint_total_other 99' \
    | sum_counter_series bridge_forged_mint_total)
[[ "$COUNTER_SUM_GUARD" == "5" ]] || fail "Prometheus counter parser self-check failed"

l2_tip() {
    curl -sf -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"],16))'
}

# ── Isolated store for the FOREIGN deployment (own trust domain, own keys) ────
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-mint-monitor-provenance}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
mkdir -p "$B2AGG_STORE_DIR/tmp"

log "======================================================================"
log "  Mint-Monitor Provenance E2E (foreign deployment must not alert #2/#4)"
log "======================================================================"

# ── Step 0: foreign-bridge tooling availability guard ───────────────────────
# The foreign-deployment driver lives in bridge-out-tool (--create-foreign-bridge
# / --submit-foreign-claim), landed via the reconciler-restart-hardening merge.
# If this build predates it, SKIP with a clear message rather than false-pass.
step "Checking foreign-bridge tooling is present in the e2e image"
if ! iso_tool --help 2>&1 | grep -q -- '--create-foreign-bridge'; then
    skip "bridge-out-tool lacks --create-foreign-bridge on this image — foreign-MINT \
coverage depends on the foreign-bridge tooling (reconciler-restart-hardening / PR #111). \
The provenance gate itself is proven by the bridge_out.rs unit tests \
(cargo test --lib mint_ / foreign_mint_does_not_raise_cross_faucet_alert)."
fi
pass "foreign-bridge tooling available"

# ── Step 1: positive control precondition — OUR ClaimEvent (=> our MINT) ─────
step "Ensuring a legitimate OWN claim/mint flow exists (positive control)"
OWN_ROWS=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
if [[ "${OWN_ROWS:-0}" -lt 1 ]]; then
    log "no ClaimEvent yet — bootstrapping via e2e-l1-to-l2.sh (drives our own MINT)"
    "$SCRIPT_DIR/e2e-l1-to-l2.sh"
    OWN_ROWS=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
fi
[[ "${OWN_ROWS:-0}" -ge 1 ]] || fail "no own ClaimEvent even after bootstrap"
log "own ClaimEvent rows (our mint flow ran): $OWN_ROWS"

# ── Step 2: baselines ───────────────────────────────────────────────────────
step "Snapshotting mint-monitor counters + tip"
BASE_MISMATCH=$(counter bridge_mint_target_mismatch_total)
BASE_FORGED=$(counter bridge_forged_mint_total)
BASE_MINT_SKIPPED=$(counter bridge_mint_foreign_skipped_total)
BASE_TIP=$(l2_tip) || fail "proxy eth_blockNumber unreachable"
log "  bridge_mint_target_mismatch_total = $BASE_MISMATCH  (Cantina #2 — MUST NOT move)"
log "  bridge_forged_mint_total          = $BASE_FORGED     (Cantina #4 — MUST NOT move)"
log "  bridge_mint_foreign_skipped_total = $BASE_MINT_SKIPPED  (MUST INCREMENT: gate skip evidence)"
log "  synthetic tip                     = $BASE_TIP"
# Positive control (c): our own MINT above did NOT trip the mismatch counter
# (our MINT targets our registered faucet -> belongs -> no alert).
[[ "$BASE_MISMATCH" == "0" ]] \
    || warn "mismatch counter non-zero at baseline ($BASE_MISMATCH) — pre-existing; asserting NO FURTHER increase"

# ── Step 3: deploy the FOREIGN deployment (service, ger_manager, bridge, faucet)
step "Deploying foreign deployment (bridge net=$FOREIGN_NETWORK_ID, own ETH faucet)"
FB_OUT=$(iso_tool --create-foreign-bridge --foreign-network-id "$FOREIGN_NETWORK_ID" 2>&1) \
    || { echo "$FB_OUT" | tail -30 >&2; fail "--create-foreign-bridge failed"; }
FOREIGN_SERVICE_ID=$(echo "$FB_OUT" | grep "service-id:" | awk '{print $NF}')
FOREIGN_GER_MANAGER_ID=$(echo "$FB_OUT" | grep "ger-manager-id:" | awk '{print $NF}')
FOREIGN_BRIDGE_ID=$(echo "$FB_OUT" | grep -w "bridge-id:" | awk '{print $NF}')
FOREIGN_FAUCET_ID=$(echo "$FB_OUT" | grep "faucet-id:" | awk '{print $NF}')
[[ -n "$FOREIGN_SERVICE_ID" && -n "$FOREIGN_GER_MANAGER_ID" && -n "$FOREIGN_BRIDGE_ID" && -n "$FOREIGN_FAUCET_ID" ]] \
    || { echo "$FB_OUT" | tail -30 >&2; fail "could not parse foreign deployment ids"; }
log "  foreign bridge: $FOREIGN_BRIDGE_ID   foreign faucet: $FOREIGN_FAUCET_ID"

FS_INNER="${FOREIGN_SERVICE_ID#0x}"
FOREIGN_DEST="0x00000000${FS_INNER:0:16}${FS_INNER:16:14}00"

# ── Step 4: fabricate the foreign deposit leaf + depth-32 proof + exit roots ─
# Identical construction to e2e-claim-provenance.sh (the bridge MASM leaf/proof
# preimage). Driving this claim makes the FOREIGN bridge emit a foreign MINT.
step "Fabricating foreign leaf + depth-32 proof"
DEPOSIT_CNT=$(date +%s)
GLOBAL_INDEX=$(python3 -c "print(2**64 + $DEPOSIT_CNT)")
ZERO_WORD="$(printf '0%.0s' {1..64})"
EMPTY_METADATA_HASH=$(cast keccak 0x)
AMOUNT_HEX=$(printf '%064x' "$DEPOSIT_AMOUNT")
LEAF_PACKED="0x00$(printf '%08x' 0)$(printf '0%.0s' {1..40})$(printf '%08x' "$FOREIGN_NETWORK_ID")${FOREIGN_DEST#0x}${AMOUNT_HEX}${EMPTY_METADATA_HASH#0x}"
[[ ${#LEAF_PACKED} -eq 228 ]] || fail "packed leaf is $(( (${#LEAF_PACKED}-2)/2 )) bytes, want 113"
LEAF=$(cast keccak "$LEAF_PACKED")
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
log "  globalIndex=$GLOBAL_INDEX  mainnet_exit_root=$MAINNET_EXIT_ROOT"

step "Building claimAsset calldata for the FOREIGN claim"
CALLDATA=$(cast calldata \
    'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
    "$SMT_LOCAL" "$SMT_ROLLUP" "$GLOBAL_INDEX" \
    "$MAINNET_EXIT_ROOT" "$ROLLUP_EXIT_ROOT" \
    0 0x0000000000000000000000000000000000000000 \
    "$FOREIGN_NETWORK_ID" "$FOREIGN_DEST" \
    "$DEPOSIT_AMOUNT" 0x)
echo "$CALLDATA" > "$B2AGG_STORE_DIR/foreign-claim-calldata.hex"

# ── Step 5: drive the claim -> foreign bridge consumes CLAIM -> emits MINT ───
step "Submitting foreign GER inject + CLAIM (foreign bridge then emits a foreign MINT)"
FC_OUT=$(iso_tool --submit-foreign-claim \
    --claim-calldata-file /store/foreign-claim-calldata.hex \
    --foreign-bridge-id "$FOREIGN_BRIDGE_ID" \
    --foreign-service-id "$FOREIGN_SERVICE_ID" \
    --foreign-ger-manager-id "$FOREIGN_GER_MANAGER_ID" \
    --scale-exp 10 2>&1) \
    || { echo "$FC_OUT" | tail -40 >&2; fail "--submit-foreign-claim failed (foreign bridge did not consume)"; }
echo "$FC_OUT" | tail -6
pass "foreign bridge consumed the CLAIM — a foreign MINT (target=foreign faucet) is now on-chain"

# ── Step 6: the foreign faucet consumes the MINT → our scanner MUST skip it ──
# The foreign faucet is a network account; the stack's network-tx executor
# consumes the tag-0 MINT it targets. Our reconciler then imports that consumed
# note and the provenance gate skips it. Assertion (a): the skip counter MUST
# INCREMENT (exact skip evidence) — AND, while the faucet is still foreign, #2/#4
# MUST stay flat (assertions b/c). This is the deletion-detecting core: with the
# gate removed, the imported foreign MINT is treated as ours and trips #4 (so the
# forged counter moves — failing (c) — and the skip counter never moves —
# failing (a)).
step "Waiting ≤${SKIP_WAIT_SECS}s for the consumed foreign MINT to be imported + SKIPPED"
NEW_MINT_SKIPPED="$BASE_MINT_SKIPPED"
ELAPSED=0
while [[ "$ELAPSED" -lt "$SKIP_WAIT_SECS" ]]; do
    NOW_MISMATCH=$(counter bridge_mint_target_mismatch_total)
    NOW_FORGED=$(counter bridge_forged_mint_total)
    # Fail FAST if a false alert fires while the faucet is still FOREIGN.
    [[ "$NOW_MISMATCH" -le "$BASE_MISMATCH" ]] \
        || fail "FALSE CANTINA #2 ALERT: bridge_mint_target_mismatch_total $BASE_MISMATCH -> $NOW_MISMATCH on a FOREIGN mint"
    [[ "$NOW_FORGED" -le "$BASE_FORGED" ]] \
        || fail "FALSE CANTINA #4 ALERT: bridge_forged_mint_total $BASE_FORGED -> $NOW_FORGED on a FOREIGN mint (gate deleted?)"
    NEW_MINT_SKIPPED=$(counter bridge_mint_foreign_skipped_total)
    [[ "$NEW_MINT_SKIPPED" -gt "$BASE_MINT_SKIPPED" ]] && break
    sleep 5; ELAPSED=$((ELAPSED + 5)); echo -n "."
done
echo ""
[[ "$NEW_MINT_SKIPPED" -gt "$BASE_MINT_SKIPPED" ]] \
    || fail "NO SKIP EVIDENCE: bridge_mint_foreign_skipped_total stayed $BASE_MINT_SKIPPED after ${SKIP_WAIT_SECS}s — the consumed foreign MINT never reached the gate (or the gate was deleted)"
pass "gate exercised: bridge_mint_foreign_skipped_total $BASE_MINT_SKIPPED -> $NEW_MINT_SKIPPED (foreign MINT skipped)"

# ── Step 6b: hold — #2/#4 stay flat for OBSERVE_SECS while still foreign ──────
step "Confirming #2/#4 stay flat for ${OBSERVE_SECS}s (no false page on the foreign mint)"
ELAPSED=0
while [[ "$ELAPSED" -lt "$OBSERVE_SECS" ]]; do
    NOW_MISMATCH=$(counter bridge_mint_target_mismatch_total)
    NOW_FORGED=$(counter bridge_forged_mint_total)
    [[ "$NOW_MISMATCH" == "$BASE_MISMATCH" ]] \
        || fail "bridge_mint_target_mismatch_total moved: $BASE_MISMATCH -> $NOW_MISMATCH"
    [[ "$NOW_FORGED" == "$BASE_FORGED" ]] \
        || fail "bridge_forged_mint_total moved on a FOREIGN mint: $BASE_FORGED -> $NOW_FORGED"
    sleep 5; ELAPSED=$((ELAPSED + 5)); echo -n "."
done
echo ""
NEW_MISMATCH=$(counter bridge_mint_target_mismatch_total)
pass "Cantina #2 quiet: bridge_mint_target_mismatch_total stayed $NEW_MISMATCH"
pass "Cantina #4 quiet on the FOREIGN mint: bridge_forged_mint_total stayed $BASE_FORGED"

# ── Step 7: POSITIVE ALERT CONTROL — allowlist the foreign faucet → #4 MUST FIRE
# Register the foreign faucet id into OUR registry. The SAME imported MINT is now
# classified OURS (target ∈ our registered set); its serial matches NO
# aggkit-recorded claim, so the forged-MINT reconciler MUST fire after its grace
# window. This proves the monitor is not merely always-quiet — a real forged case
# that SHOULD alert does. Deleting the #4 reconcile makes this assertion FAIL.
step "Positive control: allowlisting the foreign faucet, then requiring #4 to FIRE"
NATIVE_ORIGIN_ADDR="0x0f0f0f$(python3 -c 'import secrets;print(secrets.token_hex(17))')"
REG=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$FOREIGN_FAUCET_ID\",\"origin_token_address\":\"$NATIVE_ORIGIN_ADDR\",
    \"symbol\":\"FGN\",\"decimals\":8}]}" 2>/dev/null) \
    || fail "admin_registerNativeFaucet unreachable (positive control needs it)"
echo "$REG" | python3 -c "import json,sys;sys.exit(0 if 'result' in json.load(sys.stdin) else 1)" \
    || fail "admin_registerNativeFaucet failed: $REG"
log "  foreign faucet $FOREIGN_FAUCET_ID allowlisted — it now reads as OURS"

step "Waiting ≤${FORGED_WAIT_SECS}s for the reclassified MINT to trip Cantina #4"
NEW_FORGED="$BASE_FORGED"
ELAPSED=0
while [[ "$ELAPSED" -lt "$FORGED_WAIT_SECS" ]]; do
    NEW_FORGED=$(counter bridge_forged_mint_total)
    [[ "$NEW_FORGED" -gt "$BASE_FORGED" ]] && break
    sleep 5; ELAPSED=$((ELAPSED + 5)); echo -n "."
done
echo ""
[[ "$NEW_FORGED" -gt "$BASE_FORGED" ]] \
    || fail "POSITIVE CONTROL FAILED: bridge_forged_mint_total stayed $BASE_FORGED — an ours-classified MINT with NO matching claim did NOT trip #4 (the forged reconcile is broken/deleted)"
pass "Cantina #4 FIRES on a genuinely-forged (ours, no-claim) MINT: $BASE_FORGED -> $NEW_FORGED"

# ── Step 8: positive control (own flow intact) + proxy health ────────────────
step "Own ClaimEvent rows intact + proxy health (tip advancing, /metrics serving)"
OWN_ROWS_AFTER=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
[[ "${OWN_ROWS_AFTER:-0}" -ge 1 ]] \
    || fail "positive control broken: no OWN ClaimEvent rows remain"
TIP_A=$(l2_tip) || fail "eth_blockNumber unreachable after test"
sleep 12
TIP_B=$(l2_tip) || fail "eth_blockNumber unreachable after test"
[[ "$TIP_B" -gt "$TIP_A" ]] || fail "synthetic tip frozen at $TIP_A — proxy unhealthy"
pass "own ClaimEvent rows intact: $OWN_ROWS_AFTER; tip advancing: $TIP_A -> $TIP_B"

log "======================================================================"
log "  MINT-MONITOR PROVENANCE PASS"
log "    foreign bridge:                    $FOREIGN_BRIDGE_ID (network $FOREIGN_NETWORK_ID)"
log "    foreign faucet (mint target):      $FOREIGN_FAUCET_ID"
log "    bridge_mint_foreign_skipped_total: $BASE_MINT_SKIPPED -> $NEW_MINT_SKIPPED (gate skip evidence — MUST increase)"
log "    bridge_mint_target_mismatch_total: $BASE_MISMATCH -> $NEW_MISMATCH (foreign mint: must be equal)"
log "    bridge_forged_mint_total:          $BASE_FORGED -> $NEW_FORGED (positive control: MUST increase after allowlist)"
log "    own ClaimEvent rows:               $OWN_ROWS_AFTER"
log "======================================================================"
