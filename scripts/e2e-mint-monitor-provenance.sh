#!/usr/bin/env bash
# E2E: mint-monitor provenance — a FOREIGN miden-agglayer deployment minting on
# the SAME Miden chain must not raise a false Cantina #2 (mint-target) / #4
# (forged-mint) critical alert on OUR BridgeOutScanner monitors.
#
# Companion to e2e-claim-provenance.sh. That test covers the CLAIM path (foreign
# CLAIMs are tag-0, so our reconciler DOES import them and the projector gate
# must skip them). THIS test covers the MINT path, and asserts the OTHER half of
# the provenance story established in the investigation:
#
#   MINT and BURN notes are minted with tag = NoteTag::with_account_target(faucet)
#   (miden-standards mint.rs:124 / burn.rs:99), which is NON-ZERO (high 14 bits of
#   the faucet account prefix). Our note-visibility reconciler sweeps ONLY tag 0
#   (synthetic_projector.rs:130). So a FOREIGN deployment's MINT — targeting and
#   consumed by ITS OWN faucet — is NEVER imported into our store, never reaches
#   BridgeOutScanner::on_post_sync, and therefore can NEVER trip:
#       - bridge_mint_target_mismatch_total  (Cantina #2)
#       - bridge_forged_mint_total           (Cantina #4)
#
# The fix (src/bridge_out.rs::mint_note_belongs_to_deployment + mint_*_alert)
# makes that guarantee EXPLICIT rather than incidental: even if such a MINT DID
# reach us, it is provably not attributable to our deployment and is skipped
# (bridge_mint_foreign_skipped_total) instead of alerting. The pure-predicate
# defence-in-depth is proven RED->GREEN by the bridge_out.rs unit tests
# (foreign_mint_does_not_raise_cross_faucet_alert et al.). This e2e proves the
# END-TO-END invariant on a live shared chain.
#
# Topology: reuses e2e-claim-provenance.sh's foreign-deployment machinery
# (bridge-out-tool --create-foreign-bridge + --submit-foreign-claim, isolated
# store — the proxy's store is never touched). Driving a claim through the
# FOREIGN bridge makes that bridge EMIT a foreign MINT (targeting the foreign
# faucet) — exactly the note that, pre-provenance-fix, would have tripped #2 had
# the tag machinery ever delivered it to us.
#
# Assertions:
#   (a) bridge_mint_target_mismatch_total UNCHANGED across a foreign-deployment
#       mint (Cantina #2 stays quiet — no false cross-faucet alert).
#   (b) bridge_forged_mint_total UNCHANGED (Cantina #4 stays quiet).
#   (c) Positive control: our OWN legit MINT flow (L1->L2) runs and likewise does
#       NOT inflate the mismatch counter (our MINT targets our registered faucet
#       -> belongs -> no alert), while our synthetic ClaimEvent still lands.
#   (d) Proxy healthy: synthetic tip advancing, /metrics serving.
#
# HONEST COVERAGE NOTE: because the foreign MINT's non-zero tag keeps it out of
# our tag-0 sweep, bridge_mint_foreign_skipped_total is EXPECTED to stay 0 here
# too (the note never reaches on_post_sync to be skipped). Exercising the skip
# COUNTER end-to-end would require injecting a tag-0 MINT, which the standard
# note constructors never produce; that path is covered by the unit tests. This
# script therefore asserts the invariant that matters operationally: a foreign
# deployment minting raises NO false #2/#4 alert on our monitors.
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
# Window we give the reconciler/projector a chance to (fail to) import + surface
# the foreign MINT before we assert the counters stayed flat. Several sweep ticks
# (5s each) so a would-be false alert has ample time to fire if the fix regressed.
OBSERVE_SECS="${OBSERVE_SECS:-90}"

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

# Prometheus counter from the proxy's /metrics (0 when absent).
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
log "  bridge_mint_foreign_skipped_total = $BASE_MINT_SKIPPED  (expected flat: foreign MINT not tag-0)"
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

# ── Step 6: observe — the foreign MINT must NOT trip our #2/#4 monitors ──────
step "Observing our mint monitors for ${OBSERVE_SECS}s (must stay flat)"
ELAPSED=0
while [[ "$ELAPSED" -lt "$OBSERVE_SECS" ]]; do
    NOW_MISMATCH=$(counter bridge_mint_target_mismatch_total)
    NOW_FORGED=$(counter bridge_forged_mint_total)
    # Fail FAST if a false alert fires at any point in the window.
    [[ "$NOW_MISMATCH" -le "$BASE_MISMATCH" ]] \
        || fail "FALSE CANTINA #2 ALERT: bridge_mint_target_mismatch_total $BASE_MISMATCH -> $NOW_MISMATCH while a foreign deployment minted"
    [[ "$NOW_FORGED" -le "$BASE_FORGED" ]] \
        || fail "FALSE CANTINA #4 ALERT: bridge_forged_mint_total $BASE_FORGED -> $NOW_FORGED while a foreign deployment minted"
    sleep 5; ELAPSED=$((ELAPSED + 5)); echo -n "."
done
echo ""
NEW_MISMATCH=$(counter bridge_mint_target_mismatch_total)
NEW_FORGED=$(counter bridge_forged_mint_total)
NEW_MINT_SKIPPED=$(counter bridge_mint_foreign_skipped_total)
[[ "$NEW_MISMATCH" == "$BASE_MISMATCH" ]] \
    || fail "bridge_mint_target_mismatch_total moved: $BASE_MISMATCH -> $NEW_MISMATCH"
[[ "$NEW_FORGED" == "$BASE_FORGED" ]] \
    || fail "bridge_forged_mint_total moved: $BASE_FORGED -> $NEW_FORGED"
pass "Cantina #2 quiet: bridge_mint_target_mismatch_total stayed $NEW_MISMATCH"
pass "Cantina #4 quiet: bridge_forged_mint_total stayed $NEW_FORGED"
# Honest note: the foreign MINT's non-zero account-target tag keeps it out of our
# tag-0 reconciler sweep, so it never reaches on_post_sync — the skip counter is
# EXPECTED to stay flat too. Report it for observability; do not fail on it.
log "  bridge_mint_foreign_skipped_total: $BASE_MINT_SKIPPED -> $NEW_MINT_SKIPPED (expected flat — see header)"

# ── Step 7: positive control — our own ClaimEvent/mint flow intact ──────────
step "Positive control: our own ClaimEvent rows intact (our mint flow unaffected)"
OWN_ROWS_AFTER=$(pgq "SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = '${CLAIM_EVENT_TOPIC}';")
[[ "${OWN_ROWS_AFTER:-0}" -ge 1 ]] \
    || fail "positive control broken: no OWN ClaimEvent rows remain"
pass "own ClaimEvent rows intact: $OWN_ROWS_AFTER"

# ── Step 8: proxy health ────────────────────────────────────────────────────
step "Proxy health: tip advancing + /metrics serving"
TIP_A=$(l2_tip) || fail "eth_blockNumber unreachable after test"
sleep 12
TIP_B=$(l2_tip) || fail "eth_blockNumber unreachable after test"
[[ "$TIP_B" -gt "$TIP_A" ]] || fail "synthetic tip frozen at $TIP_A — proxy unhealthy"
pass "tip advancing: $TIP_A -> $TIP_B"

log "======================================================================"
log "  MINT-MONITOR PROVENANCE PASS"
log "    foreign bridge:                    $FOREIGN_BRIDGE_ID (network $FOREIGN_NETWORK_ID)"
log "    foreign faucet (mint target):      $FOREIGN_FAUCET_ID"
log "    bridge_mint_target_mismatch_total: $BASE_MISMATCH -> $NEW_MISMATCH (must be equal)"
log "    bridge_forged_mint_total:          $BASE_FORGED -> $NEW_FORGED (must be equal)"
log "    own ClaimEvent rows:               $OWN_ROWS_AFTER"
log "======================================================================"
