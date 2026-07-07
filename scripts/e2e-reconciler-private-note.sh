#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-reconciler-private-note.sh — reconciler private-note wedge repro
#                                   (0.15.5 hotfix 9712945, PR #110)
#
# The note-visibility reconciler sweeps blocks via `sync_notes(tags={0})` and
# imports every unknown tag-0 note by id (`import_notes(NoteFile::NoteId)`).
# A PRIVATE note's id lands on-chain in that same tag-0 family (the default
# NoteTag), but its details are never published, so the import fails with
# miden-client's "Incomplete imported note is private" — and a private
# HISTORICAL note never becomes importable. Pre-hotfix the whole batch import
# failed on every tick and the sweep froze on that block window FOREVER: the
# retroactive-heal path (restart → re-sweep from genesis) could never walk
# PAST the private-note block to re-discover later bridge-out notes.
#
# Post-hotfix (9712945): a per-note fallback skips just the private notes
# (metric `synthetic_reconciler_private_skipped_total`, WARN "skipping
# un-importable private network note"); other errors stay fatal.
#
# Phases:
#   0. Fund an ISOLATED wallet via L1→L2 deposit + auto-claim.
#   A. Live skip: inject a PRIVATE tag-0 P2ID note from the isolated wallet
#      (bridge-out-tool --send-private-note). Assert the reconciler skips it
#      (metric increments + WARN with the note id) and does NOT enter the
#      wedge loop ("note reconciler failed ... is private" repeating).
#   B. The prod wedge scenario: AFTER the private note, do a normal bridge-out
#      and record its BridgeEvent. Then stop the proxy, wipe ONLY the miden
#      sqlite store (--reset-miden-store --restore, the supported operator
#      flow) and restart — the reconciler's genesis re-sweep must now re-walk
#      history THROUGH the private-note block. Assert the sweep skips the
#      private note again (fresh metric ≥ 1 + WARN post-restart), never enters
#      the wedge loop, and the bridge-out's BridgeEvent is still served by
#      eth_getLogs — i.e. the private note did not block healing.
#
# Pre-hotfix both phases fail loud: the metric never appears and the proxy
# logs "note reconciler failed (transient — will retry next tick)" with
# "Incomplete imported note is private" every tick, frozen on one window.
#
# Prerequisites: full e2e stack running (make e2e-up).
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

# Required by docker-compose.e2e.yml's miden-node build args (`${VAR:?...}`) —
# even one-shot `docker compose run/stop/start` interpolates the whole file.
export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-https://github.com/0xMiden/node.git}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-v0.15.0}"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yml"
ENV_FILE="$FIXTURES_DIR/.env"

FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"
L1_DEST=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1                   # Miden network id (fixtures pin MIDEN_NETWORK_ID=1)
DEPOSIT_AMOUNT="10000000000000"  # 10^13 wei → 1000 Miden units

# Synthetic BridgeEvent topic (same as e2e-l2-to-l1.sh).
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"

METRIC="synthetic_reconciler_private_skipped_total"
SKIP_LINE="skipping un-importable private network note"
WEDGE_LINE="note reconciler failed"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}"
    local elapsed=0
    log "Waiting: $desc (timeout: ${timeout}s)..."
    # Subshell with pipefail off — see e2e-dynamic-erc20.sh for the SIGPIPE
    # rationale.
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && fail "Timed out: $desc"
        echo -n "."
        sleep "$interval"
    done
    echo ""
}

# Current value of the private-skip counter (0 when the series hasn't been
# rendered yet — metrics-exporter-prometheus only emits a counter after its
# first increment).
metric_value() {
    curl -s "$L2_RPC/metrics" 2>/dev/null \
        | awk -v m="$METRIC" '$1==m{v=$2} END{print v+0}'
}

# Proxy log lines AFTER the given line-count mark.
logs_since() {
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | tail -n +"$(( $1 + 1 ))"
}

log_mark() {
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | wc -l
}

# Wedge-loop lines since a mark: the per-tick reconciler failure caused by the
# un-importable private note. Post-hotfix this must be ZERO.
wedge_count_since() {
    logs_since "$1" | grep "$WEDGE_LINE" | grep -ci "is private" || true
}

bridge_event_count() {
    curl -s "$L2_RPC" -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getLogs\",\"params\":[{\"fromBlock\":\"0x0\",\"toBlock\":\"latest\",\"topics\":[\"$BRIDGE_EVENT_TOPIC\"]}]}" \
        | python3 -c 'import sys,json; r=json.load(sys.stdin).get("result"); print(len(r) if isinstance(r,list) else 0)' 2>/dev/null \
        || echo 0
}

# Wait for the reconciler to SKIP the given note (the per-note WARN naming its
# id), failing FAST (with wedge evidence) if the pre-hotfix wedge loop shows up
# instead: >=3 per-tick reconciler failures on the private note is the
# frozen-sweep signature — waiting longer cannot help, the window is stuck.
#
# The wait is log-driven (note-id-specific, so reruns against a chain that
# already carries older private notes stay deterministic); the metric is
# asserted separately as >= 1 — note the proxy's prometheus exporter renders
# counters saturated at 1 (pre-existing quirk on main, every *_total series
# reads 1 no matter how often it increments), so a delta assertion on the
# counter VALUE would be meaningless.
wait_for_skip_or_wedge() {
    local note_id="$1" mark="$2" timeout="$3" what="$4"
    local elapsed=0 w
    log "Waiting: $what (skip WARN for $note_id, timeout ${timeout}s)..."
    while :; do
        if logs_since "$mark" | grep "$SKIP_LINE" | grep -q "$note_id"; then
            echo ""
            return 0
        fi
        w=$(wedge_count_since "$mark")
        if [[ "$w" -ge 3 ]]; then
            echo ""
            warn "reconciler WEDGE detected: $w per-tick import failures on the private note"
            warn "wedge evidence (proxy log):"
            logs_since "$mark" | grep "$WEDGE_LINE" | grep -i "is private" | head -5 >&2
            fail "$what: reconciler wedged on the private note instead of skipping it (pre-hotfix behaviour — sweep frozen, retroactive healing dead)"
        fi
        elapsed=$((elapsed + 5))
        [[ $elapsed -ge $timeout ]] && {
            echo ""
            warn "last proxy reconciler lines:"
            logs_since "$mark" | grep -i "reconciler" | tail -10 >&2
            fail "Timed out: $what (no skip WARN for $note_id)"
        }
        echo -n "."
        sleep 5
    done
}

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast >/dev/null || fail "cast (foundry) not found"
command -v python3 >/dev/null || fail "python3 not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || fail "L2 (miden-agglayer) not reachable"

ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# ── Isolated wallet (single-owner store policy — NEVER the proxy's store) ────
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-private-note}"
B2AGG_FRESH="${B2AGG_FRESH:-1}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ID" \
    || fail "could not provision isolated wallet"

log "======================================================================"
log "  Reconciler Private-Note Wedge E2E (0.15.5 hotfix, PR #110)"
log "======================================================================"
log "Wallet:  $WALLET_ID (isolated store: $B2AGG_STORE_DIR)"
log "Bridge:  $BRIDGE_ID"
log "Faucet:  $FAUCET_ID"

# ══════════════════════════════════════════════════════════════════════════════
# Phase 0 — fund the isolated wallet (L1→L2 deposit + auto-claim)
# ══════════════════════════════════════════════════════════════════════════════
step "Phase 0: funding the isolated wallet via L1→L2 deposit..."
BAL=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
BAL="${BAL:-0}"
if [[ "$BAL" -gt 0 ]]; then
    log "wallet already funded (balance $BAL) — skipping deposit"
else
    TX=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$DEPOSIT_AMOUNT" \
        0x0000000000000000000000000000000000000000 true 0x \
        --value "$DEPOSIT_AMOUNT" 2>&1)
    STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
    [[ "$STATUS" == "1" ]] || fail "L1 deposit tx failed (status=$STATUS)"
    log "L1 deposit sent; waiting for auto-claim + P2ID delivery..."
    for attempt in $(seq 1 24); do
        sleep 10
        BAL=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
        BAL="${BAL:-0}"
        log "  attempt $attempt/24: balance = $BAL"
        [[ "$BAL" -gt 0 ]] && break
    done
    [[ "$BAL" -gt 0 ]] || fail "wallet not funded after 240s"
fi
pass "isolated wallet funded (balance $BAL)"

# ══════════════════════════════════════════════════════════════════════════════
# Phase A — live skip: inject the private note, reconciler must skip (not wedge)
# ══════════════════════════════════════════════════════════════════════════════
step "Phase A: injecting a PRIVATE tag-0 note (bridge-out-tool --send-private-note)..."
MARK_A=$(log_mark)

PRIV_OUT=$(iso_tool --send-private-note --wallet-id "$WALLET_ID" 2>&1) \
    || { echo "$PRIV_OUT" | tail -20 >&2; fail "--send-private-note failed"; }
NOTE_ID=$(echo "$PRIV_OUT" | grep '\[private-note\] note-id:' | awk '{print $NF}')
COMMIT_BLOCK=$(echo "$PRIV_OUT" | grep '\[private-note\] commit-block:' | awk '{print $NF}')
[[ -n "$NOTE_ID" && -n "$COMMIT_BLOCK" ]] \
    || { echo "$PRIV_OUT" | tail -20 >&2; fail "could not parse private note id / commit block"; }
log "  private note $NOTE_ID committed at Miden block $COMMIT_BLOCK"

wait_for_skip_or_wedge "$NOTE_ID" "$MARK_A" 300 "live reconciler skip of the private note"
pass "reconciler skipped the private note (WARN names our note id)"

SKIP_METRIC=$(metric_value)
[[ "$SKIP_METRIC" -ge 1 ]] \
    || fail "$METRIC not >= 1 on /metrics after the skip (got $SKIP_METRIC)"
pass "$METRIC >= 1 on /metrics ($SKIP_METRIC)"

# Post-skip quiescence: several ticks with ZERO wedge-loop lines. Pre-hotfix
# the same window fails every tick forever.
log "verifying no wedge loop after the skip (15s / ~3 ticks)..."
sleep 15
WEDGES=$(wedge_count_since "$MARK_A")
[[ "$WEDGES" -eq 0 ]] \
    || { logs_since "$MARK_A" | grep "$WEDGE_LINE" | grep -i "is private" | head -5 >&2; \
         fail "reconciler entered the wedge loop ($WEDGES per-tick failures) despite skipping"; }
HEALTH=$(docker inspect -f '{{.State.Health.Status}}' "$AGGLAYER_CONTAINER" 2>/dev/null || echo none)
[[ "$HEALTH" == "healthy" ]] || fail "proxy not healthy after private-note skip (status: $HEALTH)"
pass "Phase A: live skip clean — no repeated import_notes failures, proxy healthy"

# ══════════════════════════════════════════════════════════════════════════════
# Phase B — the prod wedge scenario: genesis re-sweep through the private block
# ══════════════════════════════════════════════════════════════════════════════
step "Phase B: bridge-out AFTER the private note, then reset-miden-store + restore..."

EVENTS_BEFORE=$(bridge_event_count)
BRIDGE_AMOUNT=$((BAL / 2))
[[ "$BRIDGE_AMOUNT" -gt 0 ]] || fail "wallet balance too small to bridge out ($BAL)"
log "  bridge-out $BRIDGE_AMOUNT units → $L1_DEST (BridgeEvents before: $EVENTS_BEFORE)"
iso_tool \
    --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ID" \
    --amount "$BRIDGE_AMOUNT" --dest-address "$L1_DEST" --dest-network 0 2>&1 \
    | tail -5 || fail "bridge-out failed"

wait_for "post-private-note BridgeEvent in eth_getLogs" \
    "[[ \$(bridge_event_count) -gt $EVENTS_BEFORE ]]" 240 5
EVENTS_WITH_EXIT=$(bridge_event_count)
pass "bridge-out BridgeEvent recorded (count $EVENTS_BEFORE → $EVENTS_WITH_EXIT)"

# ── Operator flow: wipe ONLY the miden sqlite store, restore, restart ────────
# (--reset-miden-store deletes store.sqlite3; postgres is untouched. The
# reconcile cursor is in-memory, so the restarted proxy re-sweeps from genesis
# and must re-walk THROUGH the private-note block. Pre-hotfix it froze there.)
log "stopping proxy..."
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" stop miden-agglayer >/dev/null 2>&1

RESTORE_LOG=$(mktemp /tmp/e2e-private-note-restore.XXXXXX.log)
log "running one-shot: --reset-miden-store --restore (log: $RESTORE_LOG)"
set +e
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" \
    run --rm --no-deps miden-agglayer \
    --port=8546 \
    --miden-node=http://miden-node:57291 \
    --miden-store-dir=/var/lib/miden-agglayer-service \
    --l1-rpc-url=http://anvil:8545 \
    --ger-l1-address=0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674 \
    --reset-miden-store \
    --restore \
    >"$RESTORE_LOG" 2>&1
RESTORE_EXIT=$?
set -e
[[ "$RESTORE_EXIT" -eq 0 ]] \
    || { tail -20 "$RESTORE_LOG" >&2; fail "reset+restore one-shot exited $RESTORE_EXIT"; }
grep -q 'reset_miden_store: deleted' "$RESTORE_LOG" \
    || fail "reset marker missing — --reset-miden-store did not wipe the sqlite"
grep -q 'RESTORE: complete' "$RESTORE_LOG" || fail "restore did not complete"
pass "miden sqlite wiped + restore completed (postgres untouched)"

log "restarting proxy..."
# Mark BEFORE the restart: the reconciler can reach (and skip) the private
# block before the container's healthcheck flips to healthy, and `docker logs`
# accumulates across restarts of the same container.
MARK_B=$(log_mark)
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" start miden-agglayer >/dev/null 2>&1
wait_for "proxy healthy after restart" \
    "[[ \$(docker inspect -f '{{.State.Health.Status}}' $AGGLAYER_CONTAINER 2>/dev/null) == healthy ]]" \
    180 3

# The genesis re-sweep (the reconcile cursor is in-memory, so a restart always
# re-walks from block 1) must re-encounter — and re-skip — the private note at
# block $COMMIT_BLOCK to reach anything after it. Pre-hotfix: it wedges there
# instead and the sweep never passes the block — retroactive healing dead.
wait_for_skip_or_wedge "$NOTE_ID" "$MARK_B" 420 "genesis re-sweep skipping the private note again"
pass "re-sweep advanced past the private-note block (post-restart WARN names our note id)"

# Restarted process → fresh metrics registry → the counter re-appears only
# because the re-sweep re-skipped.
SKIP_METRIC_B=$(metric_value)
[[ "$SKIP_METRIC_B" -ge 1 ]] \
    || fail "$METRIC not >= 1 on /metrics after the re-sweep skip (got $SKIP_METRIC_B)"
pass "$METRIC >= 1 on /metrics post-restart ($SKIP_METRIC_B)"

log "verifying no wedge loop after the re-sweep skip (15s / ~3 ticks)..."
sleep 15
WEDGES_B=$(wedge_count_since "$MARK_B")
[[ "$WEDGES_B" -eq 0 ]] \
    || { logs_since "$MARK_B" | grep "$WEDGE_LINE" | grep -i "is private" | head -5 >&2; \
         fail "reconciler wedge loop after restart ($WEDGES_B per-tick failures)"; }
pass "no wedge loop post-restart"

# The post-private-note exit survived the heal: eth_getLogs still serves its
# BridgeEvent (and the count did not regress).
EVENTS_AFTER=$(bridge_event_count)
log "BridgeEvents after heal: $EVENTS_AFTER (had $EVENTS_WITH_EXIT before the reset)"
[[ "$EVENTS_AFTER" -ge "$EVENTS_WITH_EXIT" ]] \
    || fail "BridgeEvent lost after reset+restore heal ($EVENTS_WITH_EXIT → $EVENTS_AFTER)"
pass "post-private-note bridge-out's BridgeEvent present after the heal"

# Leave the stack coherent for any subsequent suite scripts (same as
# e2e-restore.sh): dependents reconnect to the restarted proxy.
docker compose -f "$COMPOSE_FILE" --env-file "$ENV_FILE" \
    restart bridge-service aggkit >/dev/null 2>&1 || true

log "======================================================================"
log "  RECONCILER PRIVATE-NOTE TEST COMPLETE"
log "  private note: $NOTE_ID @ block $COMMIT_BLOCK (tag 0, type Private)"
log "  Phase A: live skip (metric + WARN), zero wedge-loop lines"
log "  Phase B: reset-miden-store + restore + restart → re-sweep skipped the"
log "           private note again and the post-private-note BridgeEvent"
log "           survived ($EVENTS_AFTER events served)"
log "======================================================================"
