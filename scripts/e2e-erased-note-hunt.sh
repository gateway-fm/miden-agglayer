#!/usr/bin/env bash
# E2E: erased-note HUNT — provoke a GENUINE erased B2AGG note and verify detection.
#
# Unlike e2e-erased-note-recovery.sh (which deterministically simulates the
# OBSERVABLE STATE of an erased note via an unknown faucet, and validates the
# quarantine+recovery machinery), this test goes after the REAL mechanism:
# a B2AGG note created AND consumed within the same block has its nullifier
# stripped at block construction (`remove_erased_nullifiers`), so the
# nullifier-based indexer NEVER sees the consumption — no BridgeEvent, no
# quarantine (there is no preimage to capture), funds burned. The only trace
# is the on-chain LET advancing past `deposit_counter` — which is exactly what
# the Cantina #9/#18 LET-divergence monitor watches.
#
# Strategy: fire bridge-outs back-to-back WITHOUT waiting for consumption
# (B2AGG_WAIT_CONSUMED_SECS=0) in bursts, so creations and NTX consumptions
# interleave within blocks — the same pressure profile that produced the
# missing-BridgeEvent race in the wild (task #27: 1 hit in 6 N=20 runs).
# After each burst, wait for the BridgeEvent count to settle and compare with
# the number of submissions:
#
#   events == submitted        → no erasure this burst; keep hunting.
#   events  < submitted, stable → ERASURE CANDIDATE. Verify detection:
#       (a) bridge_let_divergence_total advanced past its baseline — the
#           divergence monitor FIRED (hard assert; a silent erasure is the
#           actual Cantina #18 nightmare).
#       (b) the gap is persistent (not projector lag): count unchanged after
#           an extra settle window (hard assert).
#       (c) no quarantine row appeared for it — an invisible note has no
#           preimage to quarantine (soft: a row would mean we caught a
#           DIFFERENT skip class, still worth logging).
#     → HUNT SUCCESSFUL: a real erasure occurred and the proxy DETECTED it.
#
# If HUNT_MAX submissions all emit their BridgeEvents: exit 0 with an
# incidence report (absence of a race is not a failure) after a final
# events==submitted completeness check.
#
# Wired as an OPTIONAL suite target (`e2e-test.sh erased-note-hunt`), NOT in
# `all` — runtime is load-shaped and this is a probe, not a regression gate.
#
# Usage:  ./scripts/e2e-erased-note-hunt.sh          (stack up: make e2e-up)
#         HUNT_MAX=100 BURST=10 ./scripts/e2e-erased-note-hunt.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
source "$FIXTURES_DIR/.env"

L2_RPC="${L2_RPC:-http://localhost:8546}"
L1_RPC="${L1_RPC:-http://localhost:8545}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"
PG_PASS="${PG_PASS:-agglayer}"
PG_DB="${PG_DB:-agglayer_store}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-miden-agglayer-miden-agglayer-1}"

HUNT_MAX="${HUNT_MAX:-100}"          # total bridge-outs to try
BURST="${BURST:-10}"                 # submissions per burst (no consumption wait)
BRIDGE_OUT_AMOUNT="${BRIDGE_OUT_AMOUNT:-100}"   # Miden units per bridge-out
SETTLE_POLLS="${SETTLE_POLLS:-8}"    # x15s: events-count settle budget per burst
BRIDGE_EVENT_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

command -v cast >/dev/null || fail "cast (foundry) not found"
command -v psql >/dev/null || fail "psql not found"

export PGPASSWORD="$PG_PASS"
PSQL=(psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -tAX)
pg() {
    # STOPPER on DB error; stderr kept out of the capture (task #26 idioms).
    local out errf rc
    errf="$(mktemp)"
    out=$("${PSQL[@]}" -c "$1" 2>"$errf"); rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "pg FAILED (rc=$rc): $(cat "$errf")" >&2
        rm -f "$errf"
        return 1
    fi
    rm -f "$errf"
    printf '%s\n' "$out"
}
proxy_metric_sum() {
    local name="$1" body
    body=$(curl -sf "$L2_RPC/metrics") || fail "metrics endpoint unreachable: $L2_RPC/metrics"
    awk -v m="$name" '$1 == m || index($1, m "{") == 1 {s += $2; f=1} END {print (f ? s : 0)}' <<<"$body"
}
bridge_events() { pg "SELECT count(*) FROM synthetic_logs WHERE topics[1] = '$BRIDGE_EVENT_TOPIC'"; }
quarantine_rows() { pg "SELECT count(*) FROM unbridgeable_bridge_outs"; }

log "======================================================================"
log "  Erased-Note HUNT (real same-block erasure, max $HUNT_MAX bridge-outs)"
log "======================================================================"

# ── Setup: isolated wallet + ETH funding sized for the whole hunt ────────────
step "Provisioning isolated wallet + funding for $HUNT_MAX bridge-outs"
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-erased-note-hunt}"
B2AGG_FRESH="${B2AGG_FRESH:-1}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ETH"   # sets WALLET_ID / DEST_ADDR

# Need HUNT_MAX * amount Miden units; ETH scale is 10^10 wei per Miden unit.
NEED_UNITS=$(( HUNT_MAX * BRIDGE_OUT_AMOUNT * 2 ))     # 2x headroom
DEPOSIT_WEI=$(python3 -c "print($NEED_UNITS * 10**10)")
FUNDED_KEY="${FUNDED_KEY:-$SPONSOR_PRIVATE_KEY}"
TX=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    1 "$DEST_ADDR" "$DEPOSIT_WEI" \
    0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_WEI" 2>&1)
STATUS=$(printf '%s\n' "$TX" | awk '$1=="status"{print $2; exit}')
[[ "$STATUS" == "1" ]] || fail "funding bridgeAsset failed: $(printf '%s' "$TX" | head -3)"
pass "L1 funding deposit sent ($NEED_UNITS Miden units)"

wait_funded() {
    local i bal
    for i in $(seq 1 40); do
        bal=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ETH" || true)
        [[ -n "$bal" && "$bal" -ge $(( HUNT_MAX * BRIDGE_OUT_AMOUNT )) ]] && { echo "$bal"; return 0; }
        sleep 10
    done
    return 1
}
BAL=$(wait_funded) || fail "wallet not funded within 400s"
pass "wallet funded: $BAL Miden units"

# ── Baselines ────────────────────────────────────────────────────────────────
DIV_BASE=$(proxy_metric_sum bridge_let_divergence_total)
QROWS_BASE=$(quarantine_rows)
EV_BASE=$(bridge_events)
log "baselines: divergence=$DIV_BASE quarantine=$QROWS_BASE bridge_events=$EV_BASE"

# ── The hunt ─────────────────────────────────────────────────────────────────
submitted=0
erasure_found=0
while (( submitted < HUNT_MAX )); do
    n=$(( HUNT_MAX - submitted < BURST ? HUNT_MAX - submitted : BURST ))
    step "Burst: firing $n bridge-outs back-to-back (no consumption wait; $submitted/$HUNT_MAX so far)"
    for _ in $(seq 1 "$n"); do
        # No consumption wait — creations and NTX consumptions interleave in
        # the same blocks; failures here are hard (funds accounted per-shot).
        B2AGG_WAIT_CONSUMED_SECS=0 iso_tool \
            --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "$FAUCET_ETH" \
            --amount "$BRIDGE_OUT_AMOUNT" \
            --dest-address 0xE34aaF64b29273B7D567FCFc40544c014EEe9970 --dest-network 0 \
            >>"$B2AGG_STORE_DIR/hunt-tool.log" 2>&1 \
            || fail "bridge-out submission $((submitted+1)) failed — see $B2AGG_STORE_DIR/hunt-tool.log"
        submitted=$(( submitted + 1 ))
    done

    # Settle: BridgeEvent count must stop growing (or reach expected).
    expected=$(( EV_BASE + submitted ))
    prev=-1; stable=0
    for _ in $(seq 1 "$SETTLE_POLLS"); do
        cur=$(bridge_events)
        if (( cur >= expected )); then break; fi
        if (( cur == prev )); then stable=$(( stable + 1 )); else stable=0; fi
        prev=$cur
        (( stable >= 2 )) && break     # unchanged across 3 polls = settled short
        sleep 15
    done
    cur=$(bridge_events)
    log "burst settled: events=$((cur - EV_BASE))/$submitted"

    if (( cur < expected )); then
        # ── ERASURE CANDIDATE — verify persistence, then detection ──────────
        step "Gap detected ($((expected - cur)) missing) — verifying persistence (+45s)"
        sleep 45
        cur2=$(bridge_events)
        if (( cur2 >= expected )); then
            log "gap closed late (projector lag, not erasure) — continuing hunt"
            continue
        fi
        missing=$(( expected - cur2 ))
        erasure_found=1
        pass "ERASURE CAUGHT: $missing bridge-out(s) consumed with no BridgeEvent after settle+45s"

        # (a) HARD: the LET-divergence monitor must have fired.
        DIV_NOW=$(proxy_metric_sum bridge_let_divergence_total)
        (( DIV_NOW > DIV_BASE )) \
            || fail "erasure occurred but bridge_let_divergence_total did not advance ($DIV_BASE -> $DIV_NOW) — DETECTION FAILED (the Cantina #18 silent-loss nightmare)"
        pass "detection: bridge_let_divergence_total advanced $DIV_BASE -> $DIV_NOW"

        # (b) soft: quarantine — an invisible note has no preimage to capture.
        QROWS_NOW=$(quarantine_rows)
        if (( QROWS_NOW > QROWS_BASE )); then
            warn "quarantine grew ($QROWS_BASE -> $QROWS_NOW): a VISIBLE skip class was also caught (not the erased note itself) — inspect reasons"
            pg "SELECT note_id, reason FROM unbridgeable_bridge_outs" | sed 's/^/    /'
        else
            pass "no quarantine row (consistent with a truly-invisible erased note — preimage never seen)"
        fi

        # (c) soft: recovery sweep honesty — it should report nothing recoverable.
        if docker logs "$AGGLAYER_CONTAINER" --since 5m 2>&1 | grep -aq "nothing"; then
            pass "recovery sweep ran and (correctly) recovered nothing for the preimage-less gap"
        else
            warn "no recovery-sweep log seen yet (backoff may delay it) — informational"
        fi

        log "hunt statistics: erasure after $submitted bridge-outs (incidence ~$(python3 -c "print(f'{100.0/$submitted:.1f}')")% per bridge-out at this load)"
        break
    fi
done

if (( erasure_found == 0 )); then
    # Final completeness: every submission must have its event.
    cur=$(bridge_events)
    (( cur - EV_BASE == submitted )) \
        || fail "final accounting mismatch: events=$((cur - EV_BASE)) submitted=$submitted"
    log "no erasure in $submitted bridge-outs — incidence < $(python3 -c "print(f'{100.0/$submitted:.1f}')")% at this load; all events accounted for"
fi

log "======================================================================"
log "  ERASED-NOTE HUNT COMPLETE ($( (( erasure_found )) && echo 'erasure caught + detection verified' || echo 'no erasure — clean accounting' ))"
log "======================================================================"
