#!/usr/bin/env bash
# E2E (#185): an L1→L2 claimAsset whose destination cannot be resolved to a
# Miden AccountId must behave like EVM — the tx is ACCEPTED, its receipt is
# REVERTED (status 0x0, no logs), NO ClaimEvent is emitted, the nonce advances,
# and the unclaimable entry is recorded.
#
# Before #185 the same case emitted a SYNTHETIC ClaimEvent "so aggkit stops
# retrying". That fabricated log was the root cause of two filed defects:
#   #103 — `--restore` rebuilds history from Miden NOTE state; an event with no
#          note has nothing to replay, so it was the one class of log a faithful
#          restore legitimately dropped.
#   #184 — aggsender reads it as a claim-with-unclaim and cuts EVERY AggLayer
#          certificate at its block, permanently ("cutting certificate at block
#          N" → "no bridges or claims found for range" forever).
#
# This script is the live proof for all of it. It asserts, on a running stack:
#
#   (a) REVERT SEMANTICS — receipt status 0x0 with zero logs; no ClaimEvent for
#       the global index in eth_getLogs OR in the proxy's synthetic_logs; an
#       `unclaimable_claims` row exists; `claim_unclaimable_reverted_total`
#       advanced.
#   (b) RETRY SUPPRESSION — `isClaimed(depositCount, networkID)` via eth_call to
#       the bridge address returns TRUE for that global index even though no
#       ClaimEvent exists. This is the signal that replaced the synthetic event.
#   (c) NO RETRY STORM — the claim submitter (zkevm-bridge-service claimtxman)
#       stops re-driving: its `sync.monitored_txs` row reaches a TERMINAL status
#       and the proxy-side revert counter stops advancing. Bounded and terminal,
#       not "forever".
#   (d) CERTIFICATES KEEP SETTLING (#184) — a normal bridge-out landed AFTER the
#       unresolvable claim produces a STRICTLY NEWER settled certificate on L1,
#       and aggkit never logs the "cutting certificate" cut.
#
# Restore fidelity (#103) is proven by scripts/e2e-full-db-loss-recovery.sh run
# on a chain carrying this claim: with no phantom event there is nothing for the
# restore to lose, so the drill's exempt-drop path must report ZERO exemptions.
# Run it after this script; this script prints the gi to check.
#
# Usage:  ./scripts/e2e-185-unresolvable-claim-revert.sh   (stack must be up)
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"
source "$PROJECT_DIR/fixtures/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
METRICS_URL="${METRICS_URL:-http://localhost:8546/metrics}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
PROXY_C="${COMPOSE_PROJECT_NAME}-miden-agglayer-1"
AGGKIT_C="${COMPOSE_PROJECT_NAME}-aggkit-1"
BRIDGE_C="${COMPOSE_PROJECT_NAME}-bridge-service-1"
PG_C="${COMPOSE_PROJECT_NAME}-agglayer-postgres-1"
BRIDGE_PG_C="${COMPOSE_PROJECT_NAME}-postgres-1"
FUNDED_KEY="${FUNDED_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
DEST_NETWORK=1                       # Miden
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"
CLAIM_TOPIC0="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
# The box is shared; a slow run is not a regression. Every budget is generous
# and overridable.
READY_TIMEOUT="${READY_TIMEOUT:-900}"
UNCLAIMABLE_TIMEOUT="${UNCLAIMABLE_TIMEOUT:-900}"
QUIET_WINDOW="${QUIET_WINDOW:-240}"   # how long the retry watch runs
SETTLE_TIMEOUT="${SETTLE_TIMEOUT:-1800}"

RUN_TS="$(date -u +%Y%m%dT%H%M%SZ)"
EV_DIR="${EVIDENCE_DIR:-$PROJECT_DIR/e2e-results/185-live-$RUN_TS}"
mkdir -p "$EV_DIR"

G='\033[0;32m'; Y='\033[0;33m'; R='\033[0;31m'; N='\033[0m'
say()  { echo -e "[$(date -u +%H:%M:%SZ)] $*" | tee -a "$EV_DIR/run.log"; }
pass() { echo -e "${G}[$(date -u +%H:%M:%SZ)] PASS:${N} $*" | tee -a "$EV_DIR/run.log"; }
warn() { echo -e "${Y}[$(date -u +%H:%M:%SZ)] WARN:${N} $*" | tee -a "$EV_DIR/run.log"; }
fail() { echo -e "${R}[$(date -u +%H:%M:%SZ)] FAIL:${N} $*" | tee -a "$EV_DIR/run.log"; exit 1; }

pgq()  { docker exec "$PG_C" psql -U agglayer -d agglayer_store -tAc "$1" 2>/dev/null | tr -d '\r'; }
bpgq() { docker exec "$BRIDGE_PG_C" psql -U bridge_user -d bridge_db -tAc "$1" 2>/dev/null | tr -d '\r'; }
delog() { sed -E 's/\x1b\[[0-9;]*m//g'; }

rpc() { # $1 method, $2 params-json  -> prints .result (or empty)
    curl -sf --max-time 20 -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" 2>/dev/null \
        | python3 -c 'import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
r=d.get("result")
print(json.dumps(r) if isinstance(r,(dict,list)) else (r if r is not None else ""))' 2>/dev/null
}

metric() { # $1 = metric name (exact, no labels) -> integer, 0 when absent
    curl -sf --max-time 10 "$METRICS_URL" 2>/dev/null \
        | awk -v m="$1" '$1==m {v=$2} END{printf "%d\n", (v==""?0:v)}'
}

# The max agglayer certificate Height aggkit has logged on a line that also says
# Settled. Falls back to the max Height on any line when the settle lines carry
# no height (log-format drift must degrade the claim, never crash the run).
settled_height() {
    local h
    h=$( set +o pipefail; docker logs --tail "${AGGKIT_LOG_TAIL:-60000}" "$AGGKIT_C" 2>&1 | delog \
        | grep -a 'Settled' | grep -aoE 'Height: [0-9]+' | grep -aoE '[0-9]+' | sort -n | tail -1 )
    if [[ -z "$h" ]]; then
        h=$( set +o pipefail; docker logs --tail "${AGGKIT_LOG_TAIL:-60000}" "$AGGKIT_C" 2>&1 | delog \
            | grep -aoE 'Height: [0-9]+' | grep -aoE '[0-9]+' | sort -n | tail -1 )
        [[ -n "$h" ]] && echo "HEIGHT_SOURCE=any-cert-line" >> "$EV_DIR/notes.txt"
    fi
    echo "${h:-0}"
}

say "======================================================================"
say " #185 — unresolvable-destination claim reverts with NO ClaimEvent"
say " evidence: $EV_DIR"
say "======================================================================"

# ── 0. Preflight + baselines ────────────────────────────────────────────────
docker ps --format '{{.Names}}' | grep -qx "$PROXY_C" || fail "proxy container $PROXY_C is not running — bring the stack up first"
curl -sf --max-time 10 "$METRICS_URL" >/dev/null || fail "proxy /metrics is not serving at $METRICS_URL"

REVERTED_BEFORE=$(metric claim_unclaimable_reverted_total)
UNCLAIMABLE_BEFORE=$(metric claim_unclaimable_total)
ROWS_BEFORE=$(pgq "SELECT count(*) FROM unclaimable_claims"); ROWS_BEFORE=${ROWS_BEFORE:-0}
CLAIMLOGS_BEFORE=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%'"); CLAIMLOGS_BEFORE=${CLAIMLOGS_BEFORE:-0}
PRE_SETTLED_HEIGHT=$(settled_height)
MARK_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "baselines: claim_unclaimable_reverted_total=$REVERTED_BEFORE claim_unclaimable_total=$UNCLAIMABLE_BEFORE"
say "baselines: unclaimable_claims rows=$ROWS_BEFORE synthetic ClaimEvents=$CLAIMLOGS_BEFORE settled cert height=$PRE_SETTLED_HEIGHT"

# ── 1. Drive a REAL deposit to an UNRESOLVABLE destination ──────────────────
# Unresolvable = the address has no store mapping AND is not a zero-padded
# Miden AccountId (address_mapper::resolve_address fails). A random address
# whose FIRST byte is non-zero can never satisfy the zero-padding fallback,
# which requires bytes 0..4 to be zero. Fresh per run, so it cannot collide
# with a mapping an earlier target installed.
DEST="0x$(printf 'd1'; openssl rand -hex 19)"
[[ "${DEST:0:10}" != "0x00000000" ]] || fail "generated destination is zero-padded — it would RESOLVE"
MAPPED=$(pgq "SELECT count(*) FROM address_mappings WHERE lower(eth_address) = lower('$DEST')" 2>/dev/null)
[[ "${MAPPED:-0}" == "0" ]] || fail "destination $DEST already has a store mapping — it would resolve"
say "unresolvable destination: $DEST"

L1_TX=$(cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" "$BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    "$DEST_NETWORK" "$DEST" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_WEI" --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["transactionHash"])' 2>/dev/null)
[[ "$L1_TX" =~ ^0x[0-9a-fA-F]{64}$ ]] || fail "L1 bridgeAsset did not return a tx hash (got '${L1_TX:-<none>}')"
say "L1 bridgeAsset tx: $L1_TX"

# Read OUR deposit's coordinates by tx hash — never by index. On a shared,
# growing chain another target's deposit can take the index we read.
deposit_field() { # $1 = json field
    curl -sf --max-time 20 "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=100&offset=0" 2>/dev/null \
      | L1_TX="$L1_TX" F="$1" python3 -c '
import json, os, sys
want = os.environ["L1_TX"].lower(); f = os.environ["F"]
try: ds = json.load(sys.stdin).get("deposits", [])
except Exception: ds = []
for d in ds:
    if str(d.get("tx_hash","")).lower() == want:
        print(d.get(f, "")); break
' 2>/dev/null
}

say "waiting for bridge-service to index the deposit as ready_for_claim (<= ${READY_TIMEOUT}s)..."
t0=$SECONDS
while :; do
    RFC=$(deposit_field ready_for_claim)
    [[ "$RFC" == "True" || "$RFC" == "true" ]] && break
    (( SECONDS - t0 >= READY_TIMEOUT )) && fail "deposit from $L1_TX never became ready_for_claim in ${READY_TIMEOUT}s"
    sleep 10
done
DEPOSIT_CNT=$(deposit_field deposit_cnt)
NET_ID=$(deposit_field network_id)
GI_DEC=$(deposit_field global_index)
[[ "$GI_DEC" =~ ^[0-9]+$ ]] || fail "bridge-service gave no decimal global_index for $L1_TX (got '${GI_DEC:-<none>}')"
GI_HEX=$(python3 -c 'import sys; print(hex(int(sys.argv[1])))' "$GI_DEC")
GI_PADDED=$(python3 -c 'import sys; print("%064x" % int(sys.argv[1]))' "$GI_DEC")
pass "deposit ready: deposit_cnt=$DEPOSIT_CNT network_id=$NET_ID global_index=$GI_DEC ($GI_HEX)"
{ echo "l1_tx=$L1_TX"; echo "dest=$DEST"; echo "deposit_cnt=$DEPOSIT_CNT"; echo "network_id=$NET_ID"
  echo "global_index_dec=$GI_DEC"; echo "global_index_hex=$GI_HEX"; } > "$EV_DIR/deposit.txt"

# ── 2. Wait for the proxy to record it unclaimable ──────────────────────────
say "waiting for the claim submitter to drive claimAsset and the proxy to record it unclaimable (<= ${UNCLAIMABLE_TIMEOUT}s)..."
t0=$SECONDS
while :; do
    ETH_TX=$(pgq "SELECT eth_tx_hash FROM unclaimable_claims WHERE global_index = '$GI_HEX'")
    [[ -n "$ETH_TX" ]] && break
    (( SECONDS - t0 >= UNCLAIMABLE_TIMEOUT )) && {
        docker logs --since "$MARK_TS" "$PROXY_C" 2>&1 | delog | tail -200 > "$EV_DIR/proxy-tail-on-timeout.log"
        docker logs --since "$MARK_TS" "$BRIDGE_C" 2>&1 | delog | tail -200 > "$EV_DIR/bridge-tail-on-timeout.log"
        fail "no unclaimable_claims row for gi=$GI_HEX after ${UNCLAIMABLE_TIMEOUT}s — the claim never reached the proxy (see $EV_DIR)"
    }
    sleep 10
done
pass "unclaimable_claims row recorded: gi=$GI_HEX eth_tx=$ETH_TX"

# ═══ (a) REVERT SEMANTICS ═══════════════════════════════════════════════════
say "── (a) revert semantics ──────────────────────────────────────────────"
RCPT=$(rpc eth_getTransactionReceipt "[\"$ETH_TX\"]")
echo "$RCPT" > "$EV_DIR/receipt.json"
[[ -n "$RCPT" && "$RCPT" != "null" ]] || fail "(a) no receipt served for the claim tx $ETH_TX — the tx must be ACCEPTED, not dropped"
RCPT_STATUS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$EV_DIR/receipt.json")
RCPT_NLOGS=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("logs") or []))' "$EV_DIR/receipt.json")
RCPT_BLOCK=$(python3 -c 'import json,sys; print(int(json.load(open(sys.argv[1])).get("blockNumber","0x0"),16))' "$EV_DIR/receipt.json")
[[ "$RCPT_STATUS" == "0x0" ]] || fail "(a) claim receipt status is '$RCPT_STATUS', expected 0x0 (REVERTED) — #185 regression"
[[ "$RCPT_NLOGS" == "0" ]]    || fail "(a) claim receipt carries $RCPT_NLOGS log(s), expected 0 — a reverted claim must emit nothing"
pass "(a) receipt REVERTED: status=$RCPT_STATUS logs=$RCPT_NLOGS block=$RCPT_BLOCK"

# No ClaimEvent for this gi anywhere a consumer can see it.
LOGS_JSON=$(rpc eth_getLogs "[{\"fromBlock\":\"0x0\",\"toBlock\":\"latest\",\"topics\":[\"$CLAIM_TOPIC0\"]}]")
echo "$LOGS_JSON" > "$EV_DIR/claim-logs.json"
GI_LOGS=$(python3 -c '
import json,sys
gi = int(sys.argv[2])
try: logs = json.load(open(sys.argv[1]))
except Exception: logs = []
n = 0
for l in (logs or []):
    d = (l.get("data") or "")[2:]
    if d and int(d[:64] or "0", 16) == gi: n += 1
print(n)' "$EV_DIR/claim-logs.json" "$GI_DEC")
[[ "$GI_LOGS" == "0" ]] || fail "(a) eth_getLogs serves $GI_LOGS ClaimEvent(s) for gi=$GI_DEC — #185 requires NONE (this phantom event is what broke #103 and #184)"
STORE_LOGS=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%' AND lower(data) LIKE '0x${GI_PADDED}%'")
[[ "${STORE_LOGS:-0}" == "0" ]] || fail "(a) proxy synthetic_logs holds ${STORE_LOGS} ClaimEvent row(s) for gi=$GI_DEC — #185 requires NONE"
pass "(a) NO ClaimEvent for gi=$GI_DEC — eth_getLogs=0, synthetic_logs=0"

# Nonce advanced (EVM semantics: a reverted tx still consumes its nonce).
TX_FROM=$(python3 -c 'import json,sys; print((json.load(open(sys.argv[1])).get("from") or "").lower())' "$EV_DIR/receipt.json")
if [[ "$TX_FROM" =~ ^0x[0-9a-f]{40}$ ]]; then
    NONCE_NOW=$(rpc eth_getTransactionCount "[\"$TX_FROM\",\"latest\"]")
    TX_NONCE_HEX=$(rpc eth_getTransactionByHash "[\"$ETH_TX\"]" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("nonce",""))
except Exception: print("")' 2>/dev/null)
    say "(a) claim signer $TX_FROM: tx nonce=$TX_NONCE_HEX, account nonce now=$NONCE_NOW"
    if [[ -n "$TX_NONCE_HEX" && -n "$NONCE_NOW" ]]; then
        python3 -c 'import sys; sys.exit(0 if int(sys.argv[1],16) > int(sys.argv[2],16) else 1)' "$NONCE_NOW" "$TX_NONCE_HEX" \
            || fail "(a) the reverted claim did NOT advance the signer nonce (account=$NONCE_NOW, tx=$TX_NONCE_HEX)"
        pass "(a) nonce advanced past the reverted claim (EVM semantics)"
    fi
fi

REVERTED_AFTER=$(metric claim_unclaimable_reverted_total)
(( REVERTED_AFTER > REVERTED_BEFORE )) \
    || fail "(a) claim_unclaimable_reverted_total did not advance ($REVERTED_BEFORE -> $REVERTED_AFTER)"
pass "(a) claim_unclaimable_reverted_total advanced: $REVERTED_BEFORE -> $REVERTED_AFTER"

ROWS_AFTER=$(pgq "SELECT count(*) FROM unclaimable_claims")
say "(a) unclaimable_claims rows: $ROWS_BEFORE -> ${ROWS_AFTER:-?}"
pgq "SELECT global_index, destination_address, amount, reason, eth_tx_hash FROM unclaimable_claims WHERE global_index='$GI_HEX'" \
    > "$EV_DIR/unclaimable-row.txt"

# ═══ (b) isClaimed(globalIndex) reads TRUE with no event ════════════════════
say "── (b) isClaimed retry-suppression signal ────────────────────────────"
# isClaimed(uint32 leafIndex, uint32 sourceNetwork) — selector 0xcc461632. A
# deposit made ON L1 is mainnet-origin, so sourceNetwork is the deposit's
# network_id (0) and leafIndex is its deposit_cnt.
IS_CLAIMED_DATA=$(python3 -c '
import sys
leaf, src = int(sys.argv[1]), int(sys.argv[2])
print("0xcc461632" + "%064x" % leaf + "%064x" % src)' "$DEPOSIT_CNT" "${NET_ID:-0}")
IS_CLAIMED=$(rpc eth_call "[{\"to\":\"$BRIDGE_ADDRESS\",\"data\":\"$IS_CLAIMED_DATA\"},\"latest\"]")
echo "isClaimed($DEPOSIT_CNT,${NET_ID:-0}) -> $IS_CLAIMED" > "$EV_DIR/isclaimed.txt"
[[ "$IS_CLAIMED" == "0x0000000000000000000000000000000000000000000000000000000000000001" ]] \
    || fail "(b) isClaimed($DEPOSIT_CNT,${NET_ID:-0}) returned '$IS_CLAIMED', expected ABI true — without it the submitter re-drives forever"
pass "(b) isClaimed($DEPOSIT_CNT,${NET_ID:-0}) = TRUE from the unclaimable record, with NO ClaimEvent behind it"

# ═══ (c) the claim submitter STOPS re-driving ══════════════════════════════
say "── (c) no retry storm (watching ${QUIET_WINDOW}s) ────────────────────"
# Every re-drive of this gi hits the same unresolvable arm, so the proxy-side
# revert counter is an exact retry meter. A terminal monitored_txs row is the
# submitter-side proof.
R1=$(metric claim_unclaimable_reverted_total)
# sync.deposit.tx_hash is BYTEA — comparing it to a '0x…' string matches nothing
# and silently yields an empty deposit id, which made the terminal-status probe
# below assert on a row it never found. Compare hex to hex.
DEPOSIT_ID=$(bpgq "SELECT id FROM sync.deposit WHERE encode(tx_hash,'hex') = lower('${L1_TX#0x}')")
say "(c) bridge-service deposit id: ${DEPOSIT_ID:-<not found>}"
t0=$SECONDS; MTX_STATUS=""; TERMINAL=0
while (( SECONDS - t0 < QUIET_WINDOW )); do
    if [[ -n "$DEPOSIT_ID" ]]; then
        MTX_STATUS=$(bpgq "SELECT status FROM sync.monitored_txs WHERE deposit_id = $DEPOSIT_ID")
        case "$MTX_STATUS" in
            confirmed|failed) TERMINAL=1; break ;;
        esac
    fi
    sleep 15
done
R2=$(metric claim_unclaimable_reverted_total)
RETRIES=$(( R2 - REVERTED_BEFORE ))
say "(c) monitored_txs status=${MTX_STATUS:-<none>} terminal=$TERMINAL; reverted counter ${REVERTED_BEFORE} -> ${R1} -> ${R2} (total attempts for this gi: $RETRIES)"
docker logs --since "$MARK_TS" "$BRIDGE_C" 2>&1 | delog | grep -aiE "monitoredTx|claim|revert|nonce" | tail -400 > "$EV_DIR/bridge-service-claimtxman.log"
docker logs --since "$MARK_TS" "$PROXY_C"  2>&1 | delog | grep -aiE "unresolvable|unclaimable|reverted" | tail -400 > "$EV_DIR/proxy-unclaimable.log"
bpgq "SELECT m.deposit_id, m.status, m.nonce, array_length(m.history,1) AS history_len, \
             d.deposit_cnt, encode(d.tx_hash,'hex') AS l1_tx \
        FROM sync.monitored_txs m JOIN sync.deposit d ON d.id = m.deposit_id \
       ORDER BY m.deposit_id DESC LIMIT 20" > "$EV_DIR/monitored-txs.txt"

# The cap is 3, NOT claimtxman's own maxHistorySize backstop of 10.
#
# Stopping at 10 is what this test measured on 2026-09-07 BEFORE the estimate-gas
# fix: ten reverted receipts and ten consumed nonces in 19s, ended only by
# "marked as failed because reached the history size limit (10)" — bounded, but
# the submitter never once ran checkIfClaimed, so #185's documented suppression
# path was dead. Asserting <= 10 would have called that green.
#
# The intended shape is: attempt 1 creates the unclaimable record (its estimate
# ran before the record existed), attempt 2's estimate reverts AlreadyClaimed(),
# checkIfClaimed reads isClaimed=true and the monitored tx goes CONFIRMED. 3
# leaves one attempt of slack for a retried estimate; 10 would hide the bug.
RETRY_CAP="${RETRY_CAP:-3}"
(( RETRIES <= RETRY_CAP )) \
    || fail "(c) RETRY STORM: $RETRIES claimAsset attempts for gi=$GI_DEC in ${QUIET_WINDOW}s (cap $RETRY_CAP) — the submitter never stopped"
if (( TERMINAL == 1 )); then
    pass "(c) submitter STOPPED: monitored_txs status=$MTX_STATUS (terminal) after $RETRIES attempt(s)"
else
    # Not yet terminal is only acceptable if it is also not re-driving.
    (( R2 == R1 )) \
        || fail "(c) monitored_txs is still non-terminal (${MTX_STATUS:-<none>}) AND the revert counter is still advancing ($R1 -> $R2) — the submitter is re-driving"
    pass "(c) submitter QUIET: no new claimAsset for gi=$GI_DEC over ${QUIET_WINDOW}s (status=${MTX_STATUS:-<none>}, $RETRIES attempt(s) total)"
fi

# ═══ (d) certificates keep settling (#184) ═════════════════════════════════
say "── (d) certificate settlement after an unresolvable claim (#184) ─────"
CUTS=$( set +o pipefail; docker logs --since "$MARK_TS" "$AGGKIT_C" 2>&1 | delog \
        | grep -acE 'cutting certificate at block' )
say "(d) aggkit 'cutting certificate at block' lines since the claim: ${CUTS:-0}"
docker logs --since "$MARK_TS" "$AGGKIT_C" 2>&1 | delog \
    | grep -aiE 'cutting certificate|claim with unclaim|no bridges or claims|changed status|Height:' \
    | tail -200 > "$EV_DIR/aggkit-certificates.log"

if [[ "${RUN_BRIDGE_OUT:-1}" == "1" ]]; then
    say "(d) landing a NORMAL bridge-out AFTER the unresolvable claim..."
    if ! "$SCRIPT_DIR/e2e-l2-to-l1.sh" > "$EV_DIR/bridge-out.log" 2>&1; then
        if grep -q "no balance" "$EV_DIR/bridge-out.log"; then
            say "(d) bridge-out wallet is empty — funding it with an L1->L2 deposit first"
            "$SCRIPT_DIR/e2e-l1-to-l2.sh" > "$EV_DIR/bridge-out-fund.log" 2>&1 \
                || fail "(d) could not fund the bridge-out wallet (see $EV_DIR/bridge-out-fund.log)"
            "$SCRIPT_DIR/e2e-l2-to-l1.sh" > "$EV_DIR/bridge-out.log" 2>&1 \
                || fail "(d) the post-claim bridge-out FAILED (see $EV_DIR/bridge-out.log) — an unresolvable claim must not wedge the bridge"
        else
            fail "(d) the post-claim bridge-out FAILED (see $EV_DIR/bridge-out.log) — an unresolvable claim must not wedge the bridge"
        fi
    fi
    pass "(d) post-claim bridge-out completed"
fi

# Nothing new to certify without the bridge-out above, so the strictly-newer
# assertion has nothing to wait FOR: it would burn the whole SETTLE_TIMEOUT and
# then fail for a reason that is not #184. RUN_BRIDGE_OUT=0 is for planting an
# unclaimable claim on an existing chain; the #184 proof needs the bridge-out.
POST_SETTLED_HEIGHT="$PRE_SETTLED_HEIGHT"
if [[ "${RUN_BRIDGE_OUT:-1}" != "1" ]]; then
    warn "(d) RUN_BRIDGE_OUT=0 — no new bridge-out, so the strictly-newer-certificate assertion is SKIPPED (not proven this run)"
else
say "(d) waiting for a settled certificate STRICTLY newer than height $PRE_SETTLED_HEIGHT (<= ${SETTLE_TIMEOUT}s)..."
t0=$SECONDS; POST_SETTLED_HEIGHT=0
while :; do
    POST_SETTLED_HEIGHT=$(settled_height)
    (( POST_SETTLED_HEIGHT > PRE_SETTLED_HEIGHT )) && break
    (( SECONDS - t0 >= SETTLE_TIMEOUT )) && {
        docker logs --since "$MARK_TS" "$AGGKIT_C" 2>&1 | delog | tail -300 > "$EV_DIR/aggkit-tail-on-settle-timeout.log"
        fail "(d) #184 REGRESSION SHAPE: no certificate settled above height $PRE_SETTLED_HEIGHT in ${SETTLE_TIMEOUT}s after the unresolvable claim — settlement is wedged"
    }
    sleep 20
done
pass "(d) certificate settlement ADVANCED across the unresolvable claim: height $PRE_SETTLED_HEIGHT -> $POST_SETTLED_HEIGHT"
fi

CUTS=$( set +o pipefail; docker logs --since "$MARK_TS" "$AGGKIT_C" 2>&1 | delog \
        | grep -acE 'cutting certificate at block' )
(( ${CUTS:-0} == 0 )) \
    || fail "(d) aggkit logged ${CUTS} 'cutting certificate at block' line(s) after the unresolvable claim — that is the #184 cut #185 removes"
pass "(d) aggkit logged ZERO 'cutting certificate' lines — #184 is gone"

docker logs --since "$MARK_TS" "$AGGKIT_C" 2>&1 | delog \
    | grep -aiE 'cutting certificate|claim with unclaim|no bridges or claims|changed status|Height:' \
    | tail -300 > "$EV_DIR/aggkit-certificates.log"

# ── verdict ─────────────────────────────────────────────────────────────────
{ echo "global_index_dec=$GI_DEC"; echo "global_index_hex=$GI_HEX"; echo "global_index_padded=$GI_PADDED"
  echo "deposit_cnt=$DEPOSIT_CNT"; echo "network_id=$NET_ID"; echo "dest=$DEST"
  echo "l1_tx=$L1_TX"; echo "claim_eth_tx=$ETH_TX"; echo "claim_receipt_status=$RCPT_STATUS"
  echo "claim_receipt_block=$RCPT_BLOCK"; echo "claim_attempts=$RETRIES"
  echo "monitored_tx_status=${MTX_STATUS:-<none>}"
  echo "settled_height_before=$PRE_SETTLED_HEIGHT"; echo "settled_height_after=$POST_SETTLED_HEIGHT"
} > "$EV_DIR/VERDICT.txt"

say "======================================================================"
pass "#185 LIVE PROOF COMPLETE — (a) revert+no-event  (b) isClaimed=true  (c) no retry storm  (d) certificates settle"
say "gi for the #103 restore check: $GI_HEX ($GI_DEC)"
say "evidence: $EV_DIR"
say "======================================================================"
