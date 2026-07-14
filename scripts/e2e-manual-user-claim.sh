#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-manual-user-claim.sh — MANUAL USER CLAIM against the live proxy
#
# There is no sponsor concept in the proxy: an ordinary USER key's claimAsset
# takes the identical eth_sendRawTransaction path as the bridge-service
# ClaimTxManager sponsor's, and the claim dedup lock is keyed by globalIndex
# only (signer-agnostic). This script drives that end-to-end:
#
#   Leg 1 — manual user claim wins:
#     1. Bridge L1→L2 (bridgeAsset on Anvil) to a FRESH isolated Miden wallet
#        (provisioned per run — nothing else ever deposits to it, so balance
#        accounting is exact and causal, not >=).
#     2. A USER key — the anvil dev key, NOT the claimsponsor keystore —
#        pre-signs claimAsset (proof fetched from bridge-service) and submits
#        it via raw eth_sendRawTransaction in a tight retry loop (retrying the
#        C6 "GER not observed yet" rejections), racing the sponsor's 2s
#        monitor. The user's sub-second loop should land first; if the sponsor
#        wins a round anyway, a fresh deposit is tried (MAX_LEG1_ATTEMPTS).
#     3. Assert: user tx receipt (status 1), exactly ONE ClaimEvent for the
#        globalIndex, the event under the USER's tx hash, receipt.from == the
#        USER (hard), and the wallet balance == EXACTLY the deposits sent so
#        far × the per-deposit mint (deposit-linked accounting).
#
#   Sponsor heal + functional verification (between the legs AND as the
#   epilogue — both HARD):
#     Front-running the sponsor wedges aggkit/bridge-service's ClaimTxManager:
#     it has signed its own claim tx for the raced gi at its next nonce; the
#     proxy rejects that tx FOREVER ("claim already submitted" — hard dedup),
#     the R4 nonce gate never advances, and every later sponsor claim queues
#     behind it ("nonce mismatch" spam every ~2s). On a real EVM chain the
#     sponsor's claimAsset would MINE as an AlreadyClaimed revert and consume
#     the nonce; the proxy rejects at RPC submission, so the nonce never burns.
#     THE HEAL (validated live, 2026-07-13 rel-v0158): consume each wedged
#     head nonce with a benign zero-amount claimAsset no-op signed by the
#     sponsor key (the proxy accepts zero-amount claims immediately, RPC-only:
#     no Miden publish, no ClaimEvent, no claim lock — src/service_send_raw_txn.rs
#     "skipping zero-amount claim"), until the sponsor's submissions stop being
#     nonce-frozen. Then HARD-assert the sponsor is functional: a fresh deposit
#     must autoclaim with receipt.from == the sponsor. The test FAILS if the
#     sponsor cannot be healed — never a warning.
#
#   Leg 2 — dedup race on the same globalIndex (sponsor participation PROVEN):
#     Second deposit; wait until it is ready_for_claim, then fire the user's
#     manual claim on the SAME globalIndex against the (verified-live) sponsor.
#     Whoever wins, assert: exactly ONE ClaimEvent for the gi; and POSITIVE
#     sponsor participation —
#       · sponsor won → the winning receipt's from == the SPONSOR address;
#       · user won   → after the user's last (deterministic, dedup-rejected)
#         submission, NEW "claim already submitted" rejections for this gi
#         keep appearing in the proxy log (only the sponsor submits it by
#         then), AND the sponsor signer's eth_sendRawTransaction submissions
#         are visible in the proxy's nonce_snoop log.
#     The test FAILS if the sponsor never raced gi2.
#
#   Optional ALLOWLIST_LEG=1 phase (DISPOSABLE STACKS ONLY — restarts the
#   proxy): recreates the proxy container with --insecure-allow-any-signer
#   REMOVED and ALLOWED_SIGNERS={user,sponsor,aggoracle,sequencer}; proves a
#   manual user claim passes on allow-list membership alone and that a
#   non-listed signer is rejected on the allow-list gate; then restores the
#   original configuration. NEVER run this mid-suite: it recreates the proxy
#   container twice.
#
# USER key: anvil dev key #0 — a TEST-ONLY kurtosis credential already used by
# e2e-security.sh and scripts/claim.sh. Fine in fixtures/scripts; never prod.
#
# Stack-reuse-safe: destination wallet + baselines are per-run; eth_getLogs is
# windowed from the script-start block and chunk-paginated (5000 blocks) under
# the proxy's MAX_GETLOGS_BLOCK_RANGE cap; the sponsor is healed (hard-verified)
# before exit. Never restarts containers (except the opt-in ALLOWLIST_LEG).
# Run with COMPOSE_PROJECT_NAME=<project> to target a named stack.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"

# TEST-ONLY anvil dev key #0 (same fixture credential as e2e-security.sh /
# scripts/claim.sh). Distinct from the stack's claimsponsor keystore signer.
USER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
# The L1-funded deposit key (same as e2e-l1-to-l2.sh).
FUNDED_KEY="0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625"

DEST_NETWORK=1
DEPOSIT_AMOUNT="10000000000000" # 10^13 wei → 1000 Miden units (scale 10^10)
WEI_PER_MIDEN_UNIT=10000000000
EXPECTED_UNITS_PER_DEPOSIT=$((DEPOSIT_AMOUNT / WEI_PER_MIDEN_UNIT))

CLAIM_EVENT_TOPIC="0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
MAX_LEG1_ATTEMPTS="${MAX_LEG1_ATTEMPTS:-5}"
MAX_SPONSOR_NOOPS="${MAX_SPONSOR_NOOPS:-8}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"; }

# Strip ANSI colour escapes before any log assertion (docker logs are
# colourised; raw greps on field patterns silently miss).
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

rpc() { # rpc <method> <params-json>
    curl -s -m 300 -X POST "$L2_RPC" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$1\",\"params\":$2,\"id\":1}"
}

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

# ── Pre-flight ────────────────────────────────────────────────────────────────
command -v cast    >/dev/null || fail "cast (foundry) not found"
command -v curl    >/dev/null || fail "curl not found"
command -v python3 >/dev/null || fail "python3 not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || fail "L1 (Anvil) not reachable"
docker inspect "$AGGLAYER_CONTAINER" >/dev/null 2>&1 \
    || fail "proxy container $AGGLAYER_CONTAINER not found"

BRIDGE_UP=false
for _ in $(seq 1 30); do
    if curl -sf "$BRIDGE_SERVICE_URL/bridges/0x0000000000000000000000000000000000000000" >/dev/null 2>&1; then
        BRIDGE_UP=true; break
    fi
    sleep 2
done
[[ "$BRIDGE_UP" == "true" ]] || fail "bridge-service not reachable at $BRIDGE_SERVICE_URL"

CHAIN_ID_HEX=$(rpc eth_chainId '[]' | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])")
CHAIN_ID=$((CHAIN_ID_HEX))
USER_ADDR=$(cast wallet address --private-key "$USER_KEY")
USER_ADDR_LC="${USER_ADDR,,}"

# Stack-reuse safety: window every eth_getLogs query from the block at which
# THIS run starts, so the queried range never outgrows the proxy's
# MAX_GETLOGS_BLOCK_RANGE (10,000) on long-lived stacks. claim_events_for_gi
# additionally chunk-paginates (5000-block chunks, same fallback as
# scripts/monitoring/watch-completeness.sh) in case the run itself spans the cap.
proxy_block_number() {
    rpc eth_blockNumber '[]' \
        | python3 -c "import json,sys; print(int(json.load(sys.stdin)['result'],16))"
}
START_BLOCK=$(proxy_block_number)
[[ -n "$START_BLOCK" ]] || fail "could not read the proxy block number"

# The sponsor identity — the claimsponsor.keystore key the bridge-service
# ClaimTxManager signs with. scripts/ensure-sponsor-key.sh decrypts it into
# fixtures/.env (SPONSOR_PRIVATE_KEY); run it if this stack never has.
if [[ -z "${SPONSOR_PRIVATE_KEY:-}" ]]; then
    log "SPONSOR_PRIVATE_KEY not in fixtures/.env — running ensure-sponsor-key.sh"
    "$SCRIPT_DIR/ensure-sponsor-key.sh" >/dev/null
    source "$FIXTURES_DIR/.env"
fi
[[ -n "${SPONSOR_PRIVATE_KEY:-}" ]] || fail "SPONSOR_PRIVATE_KEY unavailable — sponsor heal/verification impossible"
SPONSOR_ADDR=$(cast wallet address --private-key "$SPONSOR_PRIVATE_KEY")
SPONSOR_ADDR_LC="${SPONSOR_ADDR,,}"
[[ "$SPONSOR_ADDR_LC" != "$USER_ADDR_LC" ]] || fail "sponsor key equals the user key — fixtures broken"

log "proxy chain id: $CHAIN_ID; start block: $START_BLOCK"
log "user (manual claimant): $USER_ADDR"
log "sponsor (ClaimTxManager signer): $SPONSOR_ADDR"

# ── Infra account ids + FRESH per-run isolated destination wallet ────────────
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || fail "miden-agglayer not initialized yet"
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ID=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')

# CAUSAL BALANCE ACCOUNTING: the destination wallet is provisioned FRESH for
# every run (unique store dir), so no deposit from any earlier test or run can
# ever mint into it — the balance is exactly the sum of THIS run's deposits,
# asserted with equality (a delayed foreign mint or a double mint both FAIL).
B2AGG_STORE_DIR="${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-manual-user-claim-$(date +%s)-$$}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"

SPONSOR_VERIFIED_AT_EXIT=0
cleanup() {
    # rc from the first arg when a re-installed trap passes it explicitly (a bare
    # `(exit "$rc")` before cleanup aborts the trap under `set -e` on failure —
    # skipping cleanup exactly when it matters); otherwise fall back to $?.
    local rc=${1:-$?}
    # Best-effort: never leave the shared stack with a wedged sponsor if the
    # script died before the epilogue heal (subshell so any `fail` inside the
    # heal cannot mask the original exit code).
    if [[ $rc -ne 0 && "$SPONSOR_VERIFIED_AT_EXIT" != "1" ]]; then
        warn "script failed before the sponsor heal epilogue — best-effort heal so the stack is not left wedged"
        ( drain_sponsor_wedge 4 ) || warn "best-effort sponsor heal did not complete"
    fi
    _iso_wipe_store || true
    exit "$rc"
}
trap cleanup EXIT

provision_isolated_wallet "$BRIDGE_ID" "$FAUCET_ID" \
    || fail "could not provision isolated destination wallet"

log "======================================================================"
log "  MANUAL USER CLAIM e2e"
log "======================================================================"
log "Wallet:  $WALLET_ID (fresh per-run isolated store: $B2AGG_STORE_DIR)"
log "Dest:    $DEST_ADDR (zero-padded, network $DEST_NETWORK)"
log "User:    $USER_ADDR (manual claimant, anvil dev key)"
log "Sponsor: $SPONSOR_ADDR (claimsponsor keystore)"

BAL_BEFORE=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
BAL_BEFORE="${BAL_BEFORE:-0}"
[[ "$BAL_BEFORE" -eq 0 ]] \
    || fail "fresh per-run wallet has non-zero balance $BAL_BEFORE — provisioning reused a store; accounting basis broken"
log "L2 wallet balance before: $BAL_BEFORE (fresh wallet)"

# ── Deposit helpers ───────────────────────────────────────────────────────────

BRIDGE_EVENT_TOPIC=$(cast keccak "BridgeEvent(uint8,uint32,address,uint32,address,uint256,bytes,uint32)")
DEPOSITS_SENT=0

# do_l1_deposit → sends bridgeAsset on L1 and DERIVES the deposit identity from
# the transaction's own BridgeEvent (tx hash → event → depositCount), so the
# later bridge-service selection is tied to THIS submission — never to amount
# matching against a (possibly misparsed) baseline. Sets L1_DEP_TX /
# L1_DEP_CNT; increments DEPOSITS_SENT. Fails the script on any parse gap.
do_l1_deposit() {
    local receipt
    receipt=$(cast send --json --rpc-url "$L1_RPC" \
        --private-key "$FUNDED_KEY" \
        "$BRIDGE_ADDRESS" \
        'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
        "$DEST_NETWORK" "$DEST_ADDR" "$DEPOSIT_AMOUNT" \
        0x0000000000000000000000000000000000000000 true 0x \
        --value "$DEPOSIT_AMOUNT" 2>&1) \
        || fail "L1 deposit cast send failed: $receipt"
    read -r L1_DEP_TX L1_DEP_CNT <<<"$(printf '%s' "$receipt" | python3 -c "
import json, sys
try:
    r = json.load(sys.stdin)
except Exception:
    sys.exit(1)                                   # fail closed on parse failure
if r.get('status') not in ('0x1', 1, '1'):
    sys.exit(1)
bridge = '$BRIDGE_ADDRESS'.lower()
topic = '$BRIDGE_EVENT_TOPIC'.lower()
for lg in r.get('logs', []):
    if lg.get('address', '').lower() != bridge:
        continue
    topics = lg.get('topics') or []
    if not topics or topics[0].lower() != topic:
        continue
    data = lg.get('data', '')
    # BridgeEvent head layout: [leafType][origNet][origAddr][destNet][destAddr]
    # [amount][metadata offset][depositCount] — depositCount is head word 7.
    if len(data) < 2 + 64 * 8:
        sys.exit(1)
    print(r['transactionHash'], int(data[2 + 64*7 : 2 + 64*8], 16))
    sys.exit(0)
sys.exit(1)                                       # no BridgeEvent → fail closed
")" || true
    [[ -n "${L1_DEP_TX:-}" && -n "${L1_DEP_CNT:-}" ]] \
        || fail "could not extract BridgeEvent depositCount from the L1 deposit receipt (fail-closed): $receipt"
    DEPOSITS_SENT=$((DEPOSITS_SENT + 1))
    log "L1 deposit #$DEPOSITS_SENT: tx=$L1_DEP_TX deposit_cnt=$L1_DEP_CNT"
}

# deposit_json_for_cnt <deposit_cnt> → the bridge-service deposit JSON for OUR
# deposit (matched by the L1-receipt-derived deposit_cnt + network_id 0).
# FAIL-CLOSED: a JSON parse failure prints nothing (and exits non-zero), so the
# caller keeps waiting / fails — it can never fall back to selecting some old
# deposit the way an empty amount-match baseline could.
deposit_json_for_cnt() {
    curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR?limit=100&offset=0" 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for dep in d.get('deposits', []):
    if str(dep.get('deposit_cnt')) == '$1' and dep.get('network_id') == 0:
        print(json.dumps(dep))
        sys.exit(0)
sys.exit(1)
"
}

# fetch_deposit_json <deposit_cnt> <timeout> → sets DEP_JSON (hard fail on miss)
fetch_deposit_json() {
    local cnt="$1" timeout="$2"
    DEP_JSON=""
    wait_for "deposit cnt=$cnt visible in bridge-service" \
        "DEP_JSON=\$(deposit_json_for_cnt $cnt); [[ -n \"\$DEP_JSON\" ]]" "$timeout" 5
    DEP_JSON=$(deposit_json_for_cnt "$cnt") || true
    [[ -n "$DEP_JSON" ]] || fail "deposit cnt=$cnt not retrievable from bridge-service (fail-closed parse)"
}

dep_field() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

# dep_static_vars <deposit-json> → emits shell assignments for the deposit's
# static claim fields (one python invocation; values are bridge-service hex
# strings / integers). eval'd by the callers.
dep_static_vars() {
    python3 -c "
import json, sys
d = json.load(sys.stdin)
m = d.get('metadata') or '0x'
if m in ('None', 'null', ''): m = '0x'
print(f\"DEP_CNT={d['deposit_cnt']};DEP_GI={d['global_index']};\"
      f\"DEP_ORIG_NET={d['orig_net']};DEP_ORIG_ADDR={d['orig_addr']};\"
      f\"DEP_DEST_NET={d['dest_net']};DEP_DEST_ADDR={d['dest_addr']};\"
      f\"DEP_AMOUNT={d['amount']};DEP_METADATA={m}\")
"
}

# build_user_claim_raw <deposit-json> → prints the pre-signed raw claimAsset tx
# for the USER key (empty on proof-not-ready). One-shot convenience wrapper
# (the race loop in submit_user_claim inlines a faster variant).
build_user_claim_raw() {
    local dep="$1" proof calldata nonce_hex
    local DEP_CNT DEP_GI DEP_ORIG_NET DEP_ORIG_ADDR DEP_DEST_NET DEP_DEST_ADDR DEP_AMOUNT DEP_METADATA
    local MER RER SMT_LOCAL SMT_ROLLUP
    eval "$(echo "$dep" | dep_static_vars)" || return 0
    proof=$(curl -sf "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$DEP_CNT&net_id=0" 2>/dev/null) || return 0
    [[ -z "$proof" ]] && return 0
    eval "$(echo "$proof" | proof_vars)" || return 0
    [[ -z "${MER:-}" ]] && return 0
    calldata=$(claim_calldata_for) || return 0
    nonce_hex=$(rpc eth_getTransactionCount "[\"$USER_ADDR\",\"latest\"]" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])") || return 0
    cast mktx --private-key "$USER_KEY" --chain "$CHAIN_ID" --nonce "$((nonce_hex))" \
        --legacy --gas-price 1000000000 --gas-limit 5000000 \
        "$BRIDGE_ADDRESS" "$calldata" 2>/dev/null || return 0
}

# proof_vars: stdin = merkle-proof JSON → shell assignments MER/RER/SMT_LOCAL/
# SMT_ROLLUP (single python invocation — the race loop is latency-sensitive).
proof_vars() {
    python3 -c "
import json, sys
try:
    p = json.load(sys.stdin)['proof']
except Exception:
    sys.exit(0)
def pad(a):
    a = list(a)
    while len(a) < 32: a.append('0x' + '00' * 32)
    return '[' + ','.join(a[:32]) + ']'
print(f\"MER={p['main_exit_root']};RER={p['rollup_exit_root']};\"
      f\"SMT_LOCAL={pad(p['merkle_proof'])};SMT_ROLLUP={pad(p['rollup_merkle_proof'])}\")
"
}

# claim_calldata_for: uses the DEP_*/MER/RER/SMT_* vars in scope.
claim_calldata_for() {
    cast calldata \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$SMT_LOCAL" "$SMT_ROLLUP" "$DEP_GI" "$MER" "$RER" \
        "$DEP_ORIG_NET" "$DEP_ORIG_ADDR" "$DEP_DEST_NET" "$DEP_DEST_ADDR" \
        "$DEP_AMOUNT" "$DEP_METADATA"
}

# submit_user_claim <deposit-json> <timeout-secs>
# Tight retry loop racing the sponsor's 2s ClaimTxManager monitor: re-fetch the
# proof each round (the exit-root pair must coincide with an injected GER —
# C6), but only re-sign when the pair CHANGES, and fetch the nonce once — the
# loop period stays well under the sponsor's poll. Sets:
#   SUBMIT_OUTCOME = user_won | dedup_rejected | timeout
#   USER_TX        = the user's accepted tx hash (user_won only)
#   LAST_ERR       = last JSON-RPC error message
submit_user_claim() {
    local dep="$1" timeout="$2" started proof raw resp result errmsg
    local calldata nonce_hex nonce last_pair=""
    local DEP_CNT DEP_GI DEP_ORIG_NET DEP_ORIG_ADDR DEP_DEST_NET DEP_DEST_ADDR DEP_AMOUNT DEP_METADATA
    local MER RER SMT_LOCAL SMT_ROLLUP
    SUBMIT_OUTCOME="timeout"; USER_TX=""; LAST_ERR=""

    eval "$(echo "$dep" | dep_static_vars)"
    # The user's proxy nonce only advances when a tx is ACCEPTED — which ends
    # this loop — so one fetch up front is safe.
    nonce_hex=$(rpc eth_getTransactionCount "[\"$USER_ADDR\",\"latest\"]" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['result'])") || nonce_hex=0x0
    nonce=$((nonce_hex))

    started=$(date +%s)
    raw=""
    while (( $(date +%s) - started < timeout )); do
        proof=$(curl -sf -m 5 "$BRIDGE_SERVICE_URL/merkle-proof?deposit_cnt=$DEP_CNT&net_id=0" 2>/dev/null) || { sleep 0.5; continue; }
        [[ -z "$proof" ]] && { sleep 0.5; continue; }
        MER=""
        eval "$(echo "$proof" | proof_vars)"
        [[ -z "$MER" ]] && { sleep 0.5; continue; }
        if [[ "$MER|$RER" != "$last_pair" ]]; then
            calldata=$(claim_calldata_for) || { sleep 0.5; continue; }
            raw=$(cast mktx --private-key "$USER_KEY" --chain "$CHAIN_ID" --nonce "$nonce" \
                --legacy --gas-price 1000000000 --gas-limit 5000000 \
                "$BRIDGE_ADDRESS" "$calldata" 2>/dev/null) || { sleep 0.5; continue; }
            last_pair="$MER|$RER"
        fi
        [[ -z "$raw" ]] && { sleep 0.5; continue; }
        resp=$(rpc eth_sendRawTransaction "[\"$raw\"]")
        result=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('result') or '')" 2>/dev/null || true)
        errmsg=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('error',{}).get('message') or '')" 2>/dev/null || true)
        if [[ -n "$result" ]]; then
            USER_TX="$result"
            # #55 — a returned hash is NOT automatically a user win. If the sponsor
            # already LANDED this gi, the proxy ACCEPTS the user's tx and writes an
            # IMMEDIATE status-0x0 (reverted) receipt with EMPTY logs / NO ClaimEvent
            # (geth-faithful AlreadyClaimed) — the user did NOT win. A genuine win's
            # receipt is null (pending) here and finalises to status 0x1 later.
            # accept-and-revert's receipt is written synchronously; one re-check
            # covers RPC propagation.
            local st; st=$(receipt_status "$result")
            [[ -z "$st" ]] && { sleep 1; st=$(receipt_status "$result"); }
            if [[ "$st" == "0x0" ]]; then
                SUBMIT_OUTCOME="accept_reverted"; return 0
            fi
            SUBMIT_OUTCOME="user_won"; return 0
        fi
        LAST_ERR="$errmsg"
        if [[ "$errmsg" == *"already submitted"* ]]; then
            # Sponsor's claim is IN FLIGHT (no ClaimEvent yet): the proxy hard-rejects
            # a second submitter (InFlight). Once it LANDS, a resubmit accept-reverts.
            SUBMIT_OUTCOME="dedup_rejected"; return 0
        fi
        # C6 GER-not-seen and transient rejections: retry quickly.
        sleep 0.3
    done
    return 0
}

# claim_events_for_gi <global_index> → prints "<count> <tx_hash_of_first>"
# from the proxy's eth_getLogs for the ClaimEvent topic. globalIndex is the
# first 32-byte word of the (all-non-indexed) event data.
#
# Range-cap-safe: queries [START_BLOCK, latest] (never from genesis — a reused
# stack past 10,000 blocks would trip MAX_GETLOGS_BLOCK_RANGE) and falls back
# to 5000-block chunk pagination if the window itself outgrows the cap (same
# pattern as scripts/monitoring/watch-completeness.sh). RPC errors are READ
# failures (non-zero exit → caller fails/retries), never "0 events".
claim_events_for_gi() {
    local gi="$1" latest
    latest=$(proxy_block_number) || return 1
    python3 - "$L2_RPC" "$CLAIM_EVENT_TOPIC" "$gi" "$START_BLOCK" "$latest" <<'PY'
import json, sys, urllib.request
rpc, topic, gi, frm, to = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
def get_logs(f, t):
    req = urllib.request.Request(rpc, json.dumps({"jsonrpc": "2.0", "id": 1,
        "method": "eth_getLogs",
        "params": [{"fromBlock": hex(f), "toBlock": hex(t), "topics": [topic]}]}).encode(),
        {"Content-Type": "application/json"})
    resp = json.load(urllib.request.urlopen(req, timeout=30))
    if "error" in resp or "result" not in resp:
        raise RuntimeError(f"getLogs RPC error: {resp.get('error')}")
    return resp["result"]
try:
    logs = get_logs(frm, to)                      # fast path: window under the cap
except (RuntimeError, OSError):
    try:
        logs, s = [], frm                         # range-capped: 5000-block chunks
        while s <= to:
            logs += get_logs(s, min(s + 4999, to))
            s += 5000
    except (RuntimeError, OSError) as e:
        print(f"claim_events_for_gi: {e}", file=sys.stderr)
        sys.exit(3)                               # read failure, NOT zero events
hits = [l for l in logs if len(l.get("data", "")) >= 66 and int(l["data"][2:66], 16) == gi]
print(len(hits), hits[0]["transactionHash"] if hits else "-")
PY
}

receipt_status_ok() { # <tx-hash>
    rpc eth_getTransactionReceipt "[\"$1\"]" \
        | python3 -c "import json,sys; r=json.load(sys.stdin).get('result'); exit(0 if r and r.get('status')=='0x1' else 1)"
}

# receipt_status <tx-hash> → raw status ("0x0" | "0x1") or "" when the receipt is
# still null (pending). #55: accept-and-revert writes an IMMEDIATE status-0x0
# receipt; a genuine claim is null until the projector finalises it, then 0x1.
receipt_status() { # <tx-hash>
    rpc eth_getTransactionReceipt "[\"$1\"]" \
        | python3 -c "import json,sys; r=json.load(sys.stdin).get('result') or {}; print(r.get('status') or '')"
}

# receipt_logs_empty <tx-hash> → exit 0 iff the receipt exists and carries ZERO
# logs (the accept-and-revert shape: no ClaimEvent). Non-existent/1+ logs → non-zero.
receipt_logs_empty() { # <tx-hash>
    rpc eth_getTransactionReceipt "[\"$1\"]" \
        | python3 -c "import json,sys; r=json.load(sys.stdin).get('result'); exit(0 if r and len(r.get('logs') or [])==0 else 1)"
}

receipt_from() { # <tx-hash> → lowercase from address (empty if absent)
    rpc eth_getTransactionReceipt "[\"$1\"]" \
        | python3 -c "import json,sys; r=json.load(sys.stdin).get('result') or {}; f=r.get('from') or ''; print('' if f=='null' else f.lower())"
}

# assert_receipt_signer <tx-hash> <expected-addr-lc> <who> — HARD: a receipt
# with no 'from' field is a FAIL, never a skipped assertion.
assert_receipt_signer() {
    local from
    from=$(receipt_from "$1")
    [[ -n "$from" ]] || fail "receipt for $1 carries no 'from' field — cannot prove the $3 signed it (hard requirement)"
    [[ "$from" == "$2" ]] || fail "receipt 'from' for $1 is $from, expected the $3 $2"
    pass "receipt signer for $1 is the $3 ($2)"
}

# wait_balance_exact <expected-units> [<rounds>] — deposit-linked EXACT
# accounting on the fresh per-run wallet: the balance must reach exactly
# expected (sum of this run's deposits); anything above is an immediate FAIL
# (a mint that this run's deposits cannot explain / a double mint).
wait_balance_exact() {
    local expected="$1" rounds="${2:-24}" i bal=0
    log "Waiting for wallet balance == $expected exactly (sync + consume P2ID notes)..."
    for i in $(seq 1 "$rounds"); do
        sleep 10
        bal=$(iso_wallet_balance "$BRIDGE_ID" "$FAUCET_ID")
        bal="${bal:-0}"
        log "balance check $i/$rounds: $bal (want exactly $expected)"
        [[ "$bal" -gt "$expected" ]] \
            && fail "balance $bal EXCEEDS the deposit-linked expectation $expected — an unaccounted or double mint"
        [[ "$bal" -eq "$expected" ]] && { pass "balance is exactly $expected (== $DEPOSITS_SENT deposits × $EXPECTED_UNITS_PER_DEPOSIT units)"; return 0; }
    done
    fail "balance never reached exactly $expected (last: $bal)"
}

# ── Sponsor heal + functional verification ────────────────────────────────────

SPONSOR_NOOPS_SENT=0
ZERO32="0x0000000000000000000000000000000000000000000000000000000000000000"
ZERO_PROOF="[$(python3 -c "print(','.join(['0x' + '00'*32] * 32))")]"

sponsor_nonce() {
    rpc eth_getTransactionCount "[\"$SPONSOR_ADDR\",\"latest\"]" \
        | python3 -c "import json,sys; print(int(json.load(sys.stdin)['result'],16))"
}

# ── #55 accept-and-revert observability ──────────────────────────────────────
# The proxy exposes Prometheus counters on the L2_RPC port's /metrics. The
# `claim_landed_dedup_reverted_total` counter increments each time a claimAsset
# targeting an ALREADY-LANDED globalIndex is ACCEPTED with a reverted (status
# 0x0) receipt instead of hard-rejected — the geth-faithful AlreadyClaimed that
# keeps the sponsor's nonce sequence in lockstep. metric_value reads it (missing
# => 0, it is only emitted after the first increment); an unreachable /metrics is
# a HARD fail — a down proxy must not read as "0" (task #26 sweep lesson).
DEDUP_METRIC="claim_landed_dedup_reverted_total"
metric_value() { # <metric-name>
    local body
    body=$(curl -sf "${L2_RPC}/metrics") || fail "metrics endpoint unreachable: ${L2_RPC}/metrics"
    awk -v n="$1" '$1==n{print $2; f=1; exit} END{if(!f)print 0}' <<<"$body" | sed 's/\..*//'
}

# send_sponsor_noop <nonce> — consume one sponsor nonce with a ZERO-AMOUNT
# claimAsset. The proxy accepts zero-amount claims synchronously and RPC-only
# (worker_handle_claim_asset: "skipping zero-amount claim" — no Miden publish,
# no ClaimEvent, no claim-lock write), so this burns exactly one nonce with no
# bridge side effects. This is the operator remedy validated live (2026-07-13).
send_sponsor_noop() {
    local nonce="$1" gi calldata raw resp result
    gi=$(( $(date +%s) % 1000000000 * 1000 + RANDOM % 1000 ))
    calldata=$(cast calldata \
        'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
        "$ZERO_PROOF" "$ZERO_PROOF" "$gi" "$ZERO32" "$ZERO32" \
        0 0x0000000000000000000000000000000000000000 \
        "$DEST_NETWORK" 0x0000000000000000000000000000000000000000 0 0x)
    raw=$(cast mktx --private-key "$SPONSOR_PRIVATE_KEY" --chain "$CHAIN_ID" --nonce "$nonce" \
        --legacy --gas-price 1000000000 --gas-limit 5000000 \
        "$BRIDGE_ADDRESS" "$calldata")
    resp=$(rpc eth_sendRawTransaction "[\"$raw\"]")
    result=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('result') or '')" 2>/dev/null || true)
    [[ -n "$result" ]] || fail "sponsor no-op at nonce $nonce was rejected — heal impossible: $resp"
    SPONSOR_NOOPS_SENT=$((SPONSOR_NOOPS_SENT + 1))
    log "consumed sponsor nonce $nonce with a zero-amount claimAsset no-op (tx $result)"
}

# sponsor_probe_round — observe the sponsor for 12s (≥ 5 ClaimTxManager 2s
# monitor ticks) and heal ONE wedged head nonce if (and only if) the sponsor is
# actively submitting, its account nonce is frozen, AND the proxy is answering
# with hard rejections (dedup "claim already submitted" / R4 "nonce mismatch").
# Transient C6 GER-not-seen retries are left alone. Sets PROBE_VERDICT (not
# echoed: a $(…) capture would run in a subshell and lose the
# SPONSOR_NOOPS_SENT increment) to: idle | advancing | transient | healed
PROBE_VERDICT=""
sponsor_probe_round() {
    local nonce_before nonce_after win_ts window subs rejections
    PROBE_VERDICT=""
    nonce_before=$(sponsor_nonce)
    win_ts=$(date -u +%Y-%m-%dT%H:%M:%S)
    sleep 12
    nonce_after=$(sponsor_nonce)
    window=$(docker logs --tail 8000 "$AGGLAYER_CONTAINER" 2>&1 | strip_ansi \
        | awk -v ts="$win_ts" '$1 >= ts')
    subs=$(printf '%s\n' "$window" \
        | grep -E "\"event\": ?\"eth_sendRawTransaction_received\"" \
        | grep -cE "\"signer\": ?\"$SPONSOR_ADDR_LC\"" || true)
    if [[ "${subs:-0}" -eq 0 ]]; then
        PROBE_VERDICT="idle"; return 0
    fi
    if (( nonce_after > nonce_before )); then
        PROBE_VERDICT="advancing"; return 0
    fi
    rejections=$(printf '%s\n' "$window" \
        | grep -cE "nonce mismatch for $SPONSOR_ADDR_LC|claim already submitted for global_index" || true)
    if [[ "${rejections:-0}" -eq 0 ]]; then
        PROBE_VERDICT="transient"; return 0
    fi
    warn "sponsor WEDGED at nonce $nonce_after ($subs submissions, $rejections hard rejections in 12s) — consuming the head nonce"
    send_sponsor_noop "$nonce_after"
    PROBE_VERDICT="healed"
}

# drain_sponsor_wedge [<max-rounds>] — probe rounds until the sponsor is idle
# or advancing (i.e. no head-blocked spam). Used between the legs and as the
# best-effort cleanup path; the HARD functional guarantee is
# verify_sponsor_functional below.
drain_sponsor_wedge() {
    local max="${1:-10}" round
    for round in $(seq 1 "$max"); do
        [[ "$SPONSOR_NOOPS_SENT" -lt "$MAX_SPONSOR_NOOPS" ]] \
            || fail "sponsor heal exceeded MAX_SPONSOR_NOOPS=$MAX_SPONSOR_NOOPS no-ops — sponsor state is pathological, refusing to spin"
        sponsor_probe_round
        log "sponsor probe round $round/$max: $PROBE_VERDICT"
        case "$PROBE_VERDICT" in
            idle|advancing) return 0 ;;
            transient|healed) ;; # observe again
        esac
    done
    warn "sponsor still busy after $max probe rounds (last: $PROBE_VERDICT)"
    return 0
}

# verify_sponsor_functional <phase-label> — the HARD sponsor guarantee: a fresh
# deposit (which the user does NOT touch) must be autoclaimed BY THE SPONSOR.
# While waiting, keeps probing/healing (the sponsor may re-wedge the moment its
# next queued claim hits an already-consumed nonce). FAILS the test if the
# sponsor does not claim it — never a warning.
verify_sponsor_functional() {
    local label="$1" vgi deadline now c t
    step "Sponsor functional verification ($label) — fresh deposit MUST autoclaim"
    do_l1_deposit
    fetch_deposit_json "$L1_DEP_CNT" 300
    vgi=$(echo "$DEP_JSON" | dep_field global_index)
    log "verification deposit: cnt=$L1_DEP_CNT gi=$vgi (user will NOT claim it)"

    deadline=$(( $(date +%s) + 600 ))
    while :; do
        read -r c t <<<"$(claim_events_for_gi "$vgi" || echo "0 -")"
        [[ "${c:-0}" -ge 1 ]] && break
        now=$(date +%s)
        (( now >= deadline )) \
            && fail "sponsor never claimed the verification deposit gi=$vgi within 600s — sponsor autoclaim is NOT functional ($label)"
        # Probe + heal while waiting: consumes any (re-)wedged head nonce.
        log "verification claim not landed yet — probing the sponsor ($(( deadline - now ))s left)"
        drain_sponsor_wedge 1 || true
    done

    read -r c t <<<"$(claim_events_for_gi "$vgi")"
    [[ "$c" == "1" ]] || fail "expected exactly 1 ClaimEvent for verification gi=$vgi, got $c"
    wait_for "verification claim receipt (status 0x1)" "receipt_status_ok '$t'" 300 5
    assert_receipt_signer "$t" "$SPONSOR_ADDR_LC" "SPONSOR"
    pass "sponsor is functional ($label): fresh deposit gi=$vgi autoclaimed by the sponsor (tx $t)"
}

# verify_sponsor_recovers_automatically <label> — the HARD #55 regression. After
# the user front-ran a globalIndex the sponsor had already signed + persisted a
# monitored tx for, the sponsor is wedged PRE-FIX: its doomed tx is hard-rejected
# at RPC ("already submitted") without consuming its nonce, so ClaimTxManager
# re-broadcasts forever ("nonce mismatch") and every later claim queues behind
# it. WITH the #55 accept-and-revert fix the sponsor recovers on its own: when it
# re-broadcasts the doomed tx (the gi has since LANDED), the proxy ACCEPTS it and
# writes a reverted (status 0x0) receipt, consuming the nonce, so ethtxmanager
# marks it mined-failed and advances — geth-faithful AlreadyClaimed.
#
# This asserts that recovery WITHOUT ANY MANUAL HEAL (deliberately NO
# drain_sponsor_wedge / zero-amount no-ops — the whole point is the fix heals it
# for us):
#   1. a fresh deposit the user never touches is autoclaimed BY THE SPONSOR
#      within a bound (pre-fix this hangs forever → the regression);
#   2. no PERMANENT nonce-mismatch wedge — after a settle window the sponsor's
#      account nonce has advanced and there is ZERO fresh sponsor nonce-mismatch
#      spam in the most recent proxy-log window (transient mismatches DURING the
#      heal are expected; a permanent wedge is a continuous stream);
#   3. the `claim_landed_dedup_reverted_total` metric incremented over the run —
#      positive proof the recovery came via accept-and-revert, not luck.
verify_sponsor_recovers_automatically() {
    local label="$1" vgi deadline now c t nonce_a nonce_b settle_ts fresh_mismatch dedup_after
    step "Sponsor AUTO-recovery ($label) — #55: fresh deposit MUST autoclaim with NO manual heal"
    do_l1_deposit
    fetch_deposit_json "$L1_DEP_CNT" 300
    vgi=$(echo "$DEP_JSON" | dep_field global_index)
    log "auto-recovery deposit: cnt=$L1_DEP_CNT gi=$vgi (user will NOT claim it; NO nonce drain will be issued)"

    # NO drain_sponsor_wedge here — the fix must heal the sponsor by itself.
    deadline=$(( $(date +%s) + 600 ))
    while :; do
        read -r c t <<<"$(claim_events_for_gi "$vgi" || echo "0 -")"
        [[ "${c:-0}" -ge 1 ]] && break
        now=$(date +%s)
        (( now >= deadline )) \
            && fail "sponsor never autoclaimed gi=$vgi within 600s WITHOUT a manual heal — sponsor did NOT auto-recover from the front-run wedge (#55 regression)"
        log "auto-recovery claim not landed yet — waiting on the sponsor to self-heal ($(( deadline - now ))s left)"
        sleep 10
    done

    read -r c t <<<"$(claim_events_for_gi "$vgi")"
    [[ "$c" == "1" ]] || fail "expected exactly 1 ClaimEvent for auto-recovery gi=$vgi, got $c"
    wait_for "auto-recovery claim receipt (status 0x1)" "receipt_status_ok '$t'" 300 5
    assert_receipt_signer "$t" "$SPONSOR_ADDR_LC" "SPONSOR"
    pass "sponsor AUTO-recovered ($label): fresh deposit gi=$vgi autoclaimed by the sponsor (tx $t), NO manual heal"

    # (2) No PERMANENT nonce-mismatch wedge. Settle, then require ZERO fresh
    # sponsor nonce-mismatch lines in the most-recent window (ANSI-strip;
    # `--tail` seeks from the END — reliable on long-lived stacks, unlike
    # `--since` which can truncate at a corrupt entry).
    nonce_a=$(sponsor_nonce)
    sleep 15
    nonce_b=$(sponsor_nonce)
    settle_ts=$(date -u +%Y-%m-%dT%H:%M:%S)
    sleep 15
    fresh_mismatch=$(docker logs --tail 8000 "$AGGLAYER_CONTAINER" 2>&1 | strip_ansi \
        | awk -v ts="$settle_ts" '$1 >= ts' \
        | grep -cE "nonce mismatch for $SPONSOR_ADDR_LC" || true)
    log "sponsor nonce: before-settle=$nonce_a after-settle=$nonce_b; fresh nonce-mismatch lines since settle=$fresh_mismatch"
    [[ "${fresh_mismatch:-0}" -eq 0 ]] \
        || fail "sponsor STILL emitting nonce-mismatch spam ($fresh_mismatch lines in ~15s after a settle) — permanent wedge NOT healed (#55 regression)"
    pass "no permanent sponsor nonce-mismatch wedge ($label): 0 fresh mismatch lines since settle"

    # (3) The accept-and-revert metric incremented over the run — positive proof
    # the sponsor's doomed tx was accept-and-reverted (nonce consumed), i.e. the
    # recovery came through the #55 fix.
    dedup_after=$(metric_value "$DEDUP_METRIC")
    log "$DEDUP_METRIC: baseline=$DEDUP_BEFORE now=$dedup_after"
    [[ "$dedup_after" -gt "$DEDUP_BEFORE" ]] \
        || fail "$DEDUP_METRIC did not increment ($DEDUP_BEFORE → $dedup_after) — the sponsor's already-landed claim was NOT accept-and-reverted; recovery was not via the #55 fix"
    pass "$DEDUP_METRIC incremented ($DEDUP_BEFORE → $dedup_after): sponsor's landed-gi claim was accepted-and-reverted"
}

# ══════════════════════════════════════════════════════════════════════════════
# Leg 1 — manual user claim (user submits instead of waiting for the sponsor)
# ══════════════════════════════════════════════════════════════════════════════
# #55 — snapshot the accept-and-revert counter BEFORE any front-run so the
# inter-leg auto-recovery assertion can prove it incremented.
DEDUP_BEFORE=$(metric_value "$DEDUP_METRIC")
log "baseline $DEDUP_METRIC = $DEDUP_BEFORE"

step "Leg 1 — deposit L1→L2, then the USER claims it manually"

LEG1_GI=""; LEG1_TX=""
for attempt in $(seq 1 "$MAX_LEG1_ATTEMPTS"); do
    log "Leg 1 attempt $attempt/$MAX_LEG1_ATTEMPTS"
    do_l1_deposit
    pass "L1 deposit sent (tx $L1_DEP_TX → deposit_cnt $L1_DEP_CNT)"
    fetch_deposit_json "$L1_DEP_CNT" 300
    GI=$(echo "$DEP_JSON" | dep_field global_index)
    log "deposit_cnt=$L1_DEP_CNT globalIndex=$GI"

    # Start submitting IMMEDIATELY (before ready_for_claim): the loop retries
    # the C6 "GER not observed yet" rejections sub-second, so the user grabs
    # the claim lock the moment the GER lands — usually beating the sponsor's
    # 2s monitor.
    submit_user_claim "$DEP_JSON" 420
    case "$SUBMIT_OUTCOME" in
        user_won)
            LEG1_GI="$GI"; LEG1_TX="$USER_TX"
            pass "USER's manual claim accepted: tx=$LEG1_TX (gi=$GI)"
            break
            ;;
        dedup_rejected)
            warn "sponsor's claim is in flight for gi=$GI (user got the dedup rejection: '$LAST_ERR')"
            warn "retrying leg 1 with a fresh deposit"
            ;;
        accept_reverted)
            # #55 — the sponsor already LANDED this gi; the user's tx was
            # accept-and-reverted (status-0x0 receipt). The user did NOT win —
            # retry with a fresh deposit to demonstrate the manual-win path.
            warn "sponsor already landed gi=$GI; user's tx was accept-and-reverted (tx $USER_TX) — retrying with a fresh deposit"
            ;;
        timeout)
            fail "user claim never accepted nor dedup-rejected within 420s (last error: '$LAST_ERR')"
            ;;
    esac
done
[[ -n "$LEG1_TX" ]] || fail "user never won a manual claim in $MAX_LEG1_ATTEMPTS attempts — cannot demonstrate the manual-user-claim path"

# Receipt: pending until the SyntheticProjector observes the CLAIM note
# consumed; then status must be success and the ClaimEvent rides this tx.
wait_for "user claim receipt (projector finalisation)" "receipt_status_ok '$LEG1_TX'" 420 5
pass "user claim receipt landed (status 0x1)"

read -r EV_COUNT EV_TX <<<"$(claim_events_for_gi "$LEG1_GI")"
[[ "$EV_COUNT" == "1" ]] || fail "expected exactly 1 ClaimEvent for gi=$LEG1_GI, got $EV_COUNT"
[[ "${EV_TX,,}" == "${LEG1_TX,,}" ]] || fail "ClaimEvent tx hash $EV_TX != user's tx $LEG1_TX"
pass "exactly ONE ClaimEvent for gi=$LEG1_GI, under the USER's tx hash"

# Receipt 'from' must be the USER — no sponsor substitution anywhere. HARD:
# an absent 'from' is a FAIL, not a skipped assertion.
assert_receipt_signer "$LEG1_TX" "$USER_ADDR_LC" "USER"

# Deposit-linked exact accounting: every deposit sent so far (the user-claimed
# one, plus any sponsor-won attempts) mints to the fresh wallet — and NOTHING
# else can. Equality, not >=.
wait_balance_exact "$((DEPOSITS_SENT * EXPECTED_UNITS_PER_DEPOSIT))"

# ══════════════════════════════════════════════════════════════════════════════
# Between the legs — #55 REGRESSION: the leg-1 front-run wedged the sponsor's
# head nonce (it signed + persisted its own claim tx for the raced gi). PRE-FIX
# this was un-healable without operator intervention (drain_sponsor_wedge
# consuming the wedged nonces by hand). WITH the accept-and-revert fix the
# sponsor RECOVERS AUTOMATICALLY — HARD-assert that, WITHOUT any manual heal, so
# leg 2 races a genuinely self-healed sponsor (and #55 stays fixed forever).
# ══════════════════════════════════════════════════════════════════════════════
verify_sponsor_recovers_automatically "inter-leg"
wait_balance_exact "$((DEPOSITS_SENT * EXPECTED_UNITS_PER_DEPOSIT))"

# ══════════════════════════════════════════════════════════════════════════════
# Leg 2 — dedup race: user's manual claim vs sponsor autoclaim, same gi.
# Sponsor participation is PROVEN positively in both outcomes (never inferred
# from ready_for_claim).
# ══════════════════════════════════════════════════════════════════════════════
step "Leg 2 — race the sponsor on the SAME globalIndex"

do_l1_deposit
pass "race L1 deposit sent (tx $L1_DEP_TX → deposit_cnt $L1_DEP_CNT)"
fetch_deposit_json "$L1_DEP_CNT" 300
GI2=$(echo "$DEP_JSON" | dep_field global_index)
CNT2="$L1_DEP_CNT"
log "race deposit_cnt=$CNT2 globalIndex=$GI2"

# This time WAIT for ready_for_claim first, so the sponsor's ClaimTxManager
# (2s monitor, just verified functional) is actively claiming when the user's
# submission goes in — a genuine race on the same globalIndex.
wait_for "race deposit ready_for_claim" \
    "curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR?limit=100&offset=0' | python3 -c \"import json,sys; d=json.load(sys.stdin); exit(0 if any(str(dep['deposit_cnt'])=='$CNT2' and dep['ready_for_claim'] for dep in d['deposits']) else 1)\"" \
    600 5
pass "race deposit is ready_for_claim"

# Anchor the proxy log BEFORE the race, for the sponsor-participation greps.
# Timestamp anchor + `--tail` window, NOT a line-count over a full read:
# docker's sequential log readers (plain read, --since) can die at a corrupt
# entry mid-file on long-lived stacks (observed live: full read stopped hours
# behind the tip), silently returning nothing past it. `--tail` seeks from the
# END and stays reliable; ISO-8601 timestamps compare lexicographically.
RACE_START_TS=$(date -u +%Y-%m-%dT%H:%M:%S)

# #55 — snapshot the accept-and-revert metric before the race so the
# sponsor-participation proof (user-won branch) can assert it increments as the
# sponsor's own gi2 retries get accept-and-reverted after the gi lands.
DEDUP_BEFORE_RACE=$(metric_value "$DEDUP_METRIC")
submit_user_claim "$DEP_JSON" 420
WINNER=""; WINNER_TX=""
case "$SUBMIT_OUTCOME" in
    user_won)
        WINNER="user"; WINNER_TX="$USER_TX"
        pass "race: USER won (tx=$USER_TX); sponsor is the loser"
        ;;
    dedup_rejected)
        # Sponsor's claim was IN FLIGHT (locked, no ClaimEvent yet) when the user
        # submitted → InFlight hard-reject. The sponsor won the lock.
        WINNER="sponsor"
        [[ "$LAST_ERR" == *"already submitted"* ]] \
            || fail "loser's rejection is not the in-flight dedup path: '$LAST_ERR'"
        pass "race: SPONSOR won (in-flight lock); user (loser) got the dedup rejection: '$LAST_ERR'"
        ;;
    accept_reverted)
        # #55 — the sponsor had already LANDED gi2 when the user submitted; the
        # user's tx was ACCEPT-AND-REVERTED (status-0x0 receipt, empty logs, NO
        # ClaimEvent). The SPONSOR won.
        WINNER="sponsor"
        [[ "$(receipt_status "$USER_TX")" == "0x0" ]] \
            || fail "accept_reverted outcome but user tx $USER_TX receipt is not status 0x0"
        receipt_logs_empty "$USER_TX" \
            || fail "accept-and-revert receipt for $USER_TX must carry EMPTY logs (no ClaimEvent)"
        pass "race: SPONSOR won (already landed); user's tx accept-and-reverted (status-0x0, no ClaimEvent): $USER_TX"
        ;;
    timeout)
        fail "race leg: user claim neither accepted, dedup-rejected, nor accept-and-reverted in 420s (last: '$LAST_ERR')"
        ;;
esac

# Exactly ONE ClaimEvent for gi2, and its tx is the winner's.
wait_for "ClaimEvent for the race gi" \
    "read -r c t <<<\"\$(claim_events_for_gi '$GI2')\"; [[ \"\$c\" -ge 1 ]]" 420 5
read -r EV2_COUNT EV2_TX <<<"$(claim_events_for_gi "$GI2")"
[[ "$EV2_COUNT" == "1" ]] || fail "expected exactly 1 ClaimEvent for gi=$GI2, got $EV2_COUNT"
pass "exactly ONE ClaimEvent for the raced gi=$GI2"

if [[ "$WINNER" == "user" ]]; then
    [[ "${EV2_TX,,}" == "${WINNER_TX,,}" ]] \
        || fail "ClaimEvent tx $EV2_TX != the winning user's tx $WINNER_TX"
else
    WINNER_TX="$EV2_TX"
    log "sponsor's winning tx: $WINNER_TX"
fi
wait_for "winner's receipt (status 0x1)" "receipt_status_ok '$WINNER_TX'" 300 5
pass "winner's tx hash $WINNER_TX carries the receipt + ClaimEvent"

# ── POSITIVE sponsor-participation proof (the race must have TWO racers) ─────
if [[ "$WINNER" == "sponsor" ]]; then
    # Sponsor won → the winning receipt itself is the proof: signed by the
    # sponsor key (hard; absent 'from' fails).
    assert_receipt_signer "$WINNER_TX" "$SPONSOR_ADDR_LC" "SPONSOR"
    pass "sponsor participation proven: the sponsor's own tx won gi=$GI2"
else
    # User won → force the DETERMINISTIC loser. #55: the user's claim has LANDED
    # (its receipt is status 0x1, ClaimEvent exists — asserted above), so ONE more
    # user submission for the SAME gi is now ACCEPT-AND-REVERTED (geth-faithful
    # AlreadyClaimed): it returns a hash with a status-0x0 receipt, EMPTY logs, NO
    # new ClaimEvent, and increments the dedup-reverted metric — it is NOT a hard
    # "already submitted" error anymore.
    DEDUP_BEFORE_RESUB=$(metric_value "$DEDUP_METRIC")
    RAW=$(build_user_claim_raw "$DEP_JSON")
    [[ -n "$RAW" ]] || fail "could not rebuild the user claim for the deterministic accept-and-revert check"
    RESP=$(rpc eth_sendRawTransaction "[\"$RAW\"]")
    RESUB_TX=$(echo "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin).get('result') or '')" 2>/dev/null || true)
    RESUB_ERR=$(echo "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin).get('error',{}).get('message') or '')" 2>/dev/null || true)
    [[ -n "$RESUB_TX" ]] \
        || fail "post-race user resubmission for gi=$GI2 was NOT accepted — expected #55 accept-and-revert, got error '$RESUB_ERR' (resp: $RESP)"
    # Its receipt must be an immediate status-0x0 revert with empty logs.
    wait_for "resubmission accept-and-revert receipt (status 0x0)" \
        "[[ \"\$(receipt_status '$RESUB_TX')\" == '0x0' ]]" 60 2
    receipt_logs_empty "$RESUB_TX" \
        || fail "accept-and-revert receipt for the resubmission $RESUB_TX must carry EMPTY logs (no ClaimEvent)"
    DEDUP_AFTER_RESUB=$(metric_value "$DEDUP_METRIC")
    [[ "$DEDUP_AFTER_RESUB" -gt "$DEDUP_BEFORE_RESUB" ]] \
        || fail "$DEDUP_METRIC did not increment on the post-win user resubmission ($DEDUP_BEFORE_RESUB → $DEDUP_AFTER_RESUB) — accept-and-revert did not fire"
    # Still exactly ONE ClaimEvent for gi2 (accept-and-revert emits none).
    read -r EV2B_COUNT _ <<<"$(claim_events_for_gi "$GI2")"
    [[ "$EV2B_COUNT" == "1" ]] || fail "post-resubmission gi2 has $EV2B_COUNT ClaimEvents — accept-and-revert must not emit a second"
    pass "post-race user resubmission ACCEPT-AND-REVERTED (status-0x0 $RESUB_TX, no ClaimEvent, metric $DEDUP_BEFORE_RESUB→$DEDUP_AFTER_RESUB)"

    # ...then prove the SPONSOR raced gi2, positively: after the user's LAST
    # submission the sponsor's ClaimTxManager keeps re-broadcasting its own signed
    # gi2 tx, which — the gi having landed — is ACCEPT-AND-REVERTED each time. So
    # the sponsor's participation shows as its own eth_sendRawTransaction
    # submissions in the proxy log AND further increments of the dedup-reverted
    # metric, anchored strictly AFTER the user's resubmission (so the user's own
    # accept-and-revert above cannot satisfy it).
    sleep 2
    SPONSOR_PROOF_TS=$(date -u +%Y-%m-%dT%H:%M:%S)
    DEDUP_AT_ANCHOR=$(metric_value "$DEDUP_METRIC")
    log "observing 20s for the sponsor's own gi=$GI2 submissions (anchor $SPONSOR_PROOF_TS)..."
    sleep 20
    PROOF_WINDOW=$(docker logs --tail 8000 "$AGGLAYER_CONTAINER" 2>&1 | strip_ansi \
        | awk -v ts="$SPONSOR_PROOF_TS" '$1 >= ts')
    SPONSOR_SUBS=$(printf '%s\n' "$PROOF_WINDOW" \
        | grep -E "\"event\": ?\"eth_sendRawTransaction_received\"" \
        | grep -cE "\"signer\": ?\"$SPONSOR_ADDR_LC\"" || true)
    DEDUP_AFTER_WINDOW=$(metric_value "$DEDUP_METRIC")
    log "post-anchor sponsor submissions: ${SPONSOR_SUBS:-0}; $DEDUP_METRIC $DEDUP_AT_ANCHOR→$DEDUP_AFTER_WINDOW (since race: ${DEDUP_BEFORE_RACE})"
    [[ "${SPONSOR_SUBS:-0}" -ge 1 ]] \
        || fail "no eth_sendRawTransaction from the sponsor signer $SPONSOR_ADDR_LC after the user's last submission — the sponsor never raced gi=$GI2 (no second racer)"
    [[ "$DEDUP_AFTER_WINDOW" -gt "$DEDUP_AT_ANCHOR" ]] \
        || fail "$DEDUP_METRIC did not increment after the user stopped ($DEDUP_AT_ANCHOR → $DEDUP_AFTER_WINDOW) — the sponsor's own gi=$GI2 retries were not accept-and-reverted (cannot attribute continued claiming to the sponsor)"
    pass "sponsor participation proven: sponsor kept submitting gi=$GI2 after the user stopped ($SPONSOR_SUBS submissions; dedup-reverted $DEDUP_AT_ANCHOR→$DEDUP_AFTER_WINDOW)"
fi

# ══════════════════════════════════════════════════════════════════════════════
# Epilogue — HEAL the sponsor wedge this run created and HARD-verify the stack
# is left with a functional autoclaimer (suite-safety: this script must not
# poison whatever runs after it).
# ══════════════════════════════════════════════════════════════════════════════
step "Epilogue — sponsor heal + hard functional assertion"
drain_sponsor_wedge 10
verify_sponsor_functional "epilogue"
SPONSOR_VERIFIED_AT_EXIT=1

# Final deposit-linked accounting: EVERY deposit this run sent (leg 1 attempts,
# two verification deposits, the raced deposit) minted exactly once — the fresh
# wallet's balance equals the exact sum, nothing more.
wait_balance_exact "$((DEPOSITS_SENT * EXPECTED_UNITS_PER_DEPOSIT))"

echo ""
log "======================================================================"
log "  MANUAL USER CLAIM e2e DONE"
log "    leg 1: user tx $LEG1_TX claimed gi=$LEG1_GI"
log "    leg 2: winner=$WINNER tx=$WINNER_TX gi=$GI2 (single ClaimEvent, loser dedup-rejected, sponsor participation proven)"
log "    sponsor: healed + verified functional ($SPONSOR_NOOPS_SENT wedged nonce(s) consumed)"
log "    deposits: $DEPOSITS_SENT sent, balance exactly $((DEPOSITS_SENT * EXPECTED_UNITS_PER_DEPOSIT)) units"
log "======================================================================"

# ══════════════════════════════════════════════════════════════════════════════
# OPTIONAL: ALLOW-LIST LEG (ALLOWLIST_LEG=1) — DISPOSABLE STACKS ONLY.
#
# Recreates the proxy container WITHOUT --insecure-allow-any-signer and with an
# explicit ALLOWED_SIGNERS list (user + sponsor + the stack's own aggoracle /
# sequencer signers, which must stay allowed or GER injection dies and no claim
# can ever pass C6), proves the manual user claim works on allow-list
# membership alone plus that a non-listed signer is rejected on the allow-list
# gate, then restores the original configuration. THE PROXY RESTARTS TWICE:
# never run this mid-suite or on a stack anyone else is using.
#
# Assumes the stack was created from docker-compose.e2e.yml (override the file
# list via ALLOWLIST_COMPOSE_FILES for overlay stacks).
# ══════════════════════════════════════════════════════════════════════════════

keystore_address() { # <keystore-path> <password> → 0x address (key never printed)
    local key
    key=$(cast wallet decrypt-keystore "$(basename "$1")" \
            --keystore-dir "$(dirname "$1")" \
            --unsafe-password "$2" 2>/dev/null \
          | grep -oiE -m1 '(0x)?[0-9a-f]{64}' || true)
    [[ -n "$key" ]] || return 1
    cast wallet address --private-key "0x${key#0x}"
}

ALLOWLIST_OVERRIDE=""
ALLOWLIST_ACTIVE=0

restore_proxy_config() {
    warn "restoring the original proxy configuration (container recreate)..."
    local -a compose
    read -r -a compose <<<"$ALLOWLIST_COMPOSE_CMD"
    "${compose[@]}" up -d --no-deps --force-recreate miden-agglayer >/dev/null 2>&1 \
        || fail "could not restore the original proxy config — STACK LEFT MODIFIED, restore manually: $ALLOWLIST_COMPOSE_CMD up -d --no-deps --force-recreate miden-agglayer"
    wait_for "proxy healthy after restore" "rpc eth_chainId '[]' | grep -q '\"result\"'" 300 5
    ALLOWLIST_ACTIVE=0
    [[ -n "$ALLOWLIST_OVERRIDE" ]] && rm -f "$ALLOWLIST_OVERRIDE"
    pass "original proxy configuration restored"
}

run_allowlist_leg() {
    step "ALLOW-LIST LEG — proxy restart with explicit --allowed-signers (DISPOSABLE STACK ONLY)"
    warn "this phase RECREATES $AGGLAYER_CONTAINER twice; it must never run mid-suite"

    local ks_password="${KEYSTORE_PASSWORD:-pSnv6Dh5s9ahuzGzH9RoCDrKAMddaX3m}"
    local aggoracle_addr sequencer_addr allowlist
    aggoracle_addr=$(keystore_address "$FIXTURES_DIR/aggoracle.keystore" "$ks_password") \
        || fail "could not derive the aggoracle signer address (fixtures/aggoracle.keystore)"
    sequencer_addr=$(keystore_address "$FIXTURES_DIR/sequencer.keystore" "$ks_password") \
        || fail "could not derive the sequencer signer address (fixtures/sequencer.keystore)"
    allowlist="$USER_ADDR,$SPONSOR_ADDR,$aggoracle_addr,$sequencer_addr"
    log "ALLOWED_SIGNERS = $allowlist"

    # Reproduce the CURRENT container command minus --insecure-allow-any-signer
    # (from docker inspect, so the override never drifts from the running
    # config), plus the ALLOWED_SIGNERS env.
    ALLOWLIST_OVERRIDE=$(mktemp /tmp/manual-user-claim-allowlist-XXXXXX.yml)
    docker inspect "$AGGLAYER_CONTAINER" | python3 -c "
import json, sys
allow = sys.argv[1]
cmd = json.load(sys.stdin)[0]['Config']['Cmd'] or []
if '--insecure-allow-any-signer' not in cmd:
    print('current proxy command has no --insecure-allow-any-signer; nothing to strip', file=sys.stderr)
cmd = [c for c in cmd if c != '--insecure-allow-any-signer']
out = ['services:', '  miden-agglayer:', '    environment:',
       '      ALLOWED_SIGNERS: %s' % json.dumps(allow), '    command:']
out += ['      - %s' % json.dumps(c) for c in cmd]
print('\n'.join(out))
" "$allowlist" > "$ALLOWLIST_OVERRIDE"

    ALLOWLIST_COMPOSE_CMD="${ALLOWLIST_COMPOSE_CMD:-docker compose -p $COMPOSE_PROJECT_NAME ${ALLOWLIST_COMPOSE_FILES:--f $PROJECT_DIR/docker-compose.e2e.yml} --env-file $FIXTURES_DIR/.env}"
    local -a compose
    read -r -a compose <<<"$ALLOWLIST_COMPOSE_CMD"

    ALLOWLIST_ACTIVE=1
    # This optional phase can itself re-wedge the sponsor; clear the "verified at
    # exit" flag so cleanup's best-effort heal runs again if this phase fails.
    SPONSOR_VERIFIED_AT_EXIT=0
    # Preserve the failing command's exit code and pass it to cleanup EXPLICITLY.
    # A bare `(exit "$rc")` before cleanup aborts the trap under `set -e` on a
    # nonzero status, so cleanup (wallet wipe + sponsor heal) would be skipped
    # exactly on failure. Passing $rc as an arg avoids that.
    trap 'rc=$?; if [[ "$ALLOWLIST_ACTIVE" == "1" ]]; then ( restore_proxy_config ) || true; fi; cleanup "$rc"' EXIT
    "${compose[@]}" -f "$ALLOWLIST_OVERRIDE" up -d --no-deps --force-recreate miden-agglayer \
        || fail "could not recreate the proxy with the allow-list override"
    wait_for "proxy healthy with allow-list config" "rpc eth_chainId '[]' | grep -q '\"result\"'" 300 5
    docker inspect "$AGGLAYER_CONTAINER" --format '{{json .Config.Cmd}}' | grep -q 'insecure-allow-any-signer' \
        && fail "override did not remove --insecure-allow-any-signer — open mode still active"
    pass "proxy is running fail-closed with an explicit allow-list"

    # Positive: the allow-listed USER's manual claim completes end-to-end.
    do_l1_deposit
    fetch_deposit_json "$L1_DEP_CNT" 300
    local gi_al
    gi_al=$(echo "$DEP_JSON" | dep_field global_index)
    log "allow-list leg deposit: cnt=$L1_DEP_CNT gi=$gi_al"
    submit_user_claim "$DEP_JSON" 420
    case "$SUBMIT_OUTCOME" in
        user_won)
            pass "allow-listed USER's manual claim accepted under allow-list mode (tx $USER_TX)" ;;
        dedup_rejected|accept_reverted)
            # The sponsor (also allow-listed) beat the user — via in-flight dedup
            # (dedup_rejected) or, if it already landed, #55 accept-and-revert. The
            # user's submission still traversed the allow-list gate (the claim lock /
            # landed classification sits BEHIND it), so membership is proven either way.
            pass "sponsor (also allow-listed) won the claim ($SUBMIT_OUTCOME); user's submission passed the allow-list gate" ;;
        timeout)
            fail "no claim landed for gi=$gi_al under allow-list mode — allow-list config broke the claim path" ;;
    esac
    wait_for "allow-list leg ClaimEvent" \
        "read -r c t <<<\"\$(claim_events_for_gi '$gi_al')\"; [[ \"\$c\" -ge 1 ]]" 420 5
    pass "claim completed under allow-list mode (gi=$gi_al)"

    # Negative: a signer NOT on the list is rejected on the allow-list gate.
    # anvil dev key #1 — TEST-ONLY, deliberately not in ALLOWED_SIGNERS.
    local outsider_key="0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
    local outsider_addr outsider_nonce raw resp errmsg
    outsider_addr=$(cast wallet address --private-key "$outsider_key")
    outsider_nonce=$(rpc eth_getTransactionCount "[\"$outsider_addr\",\"latest\"]" \
        | python3 -c "import json,sys; print(int(json.load(sys.stdin)['result'],16))")
    raw=$(cast mktx --private-key "$outsider_key" --chain "$CHAIN_ID" --nonce "$outsider_nonce" \
        --legacy --gas-price 1000000000 --gas-limit 5000000 \
        "$BRIDGE_ADDRESS" \
        "$(cast calldata \
            'claimAsset(bytes32[32],bytes32[32],uint256,bytes32,bytes32,uint32,address,uint32,address,uint256,bytes)' \
            "$ZERO_PROOF" "$ZERO_PROOF" 424242 "$ZERO32" "$ZERO32" \
            0 0x0000000000000000000000000000000000000000 \
            "$DEST_NETWORK" 0x0000000000000000000000000000000000000000 1 0x)")
    resp=$(rpc eth_sendRawTransaction "[\"$raw\"]")
    errmsg=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('error',{}).get('message') or '')" 2>/dev/null || true)
    [[ "$errmsg" == *"not on the allow-list"* ]] \
        || fail "non-listed signer $outsider_addr was NOT rejected on the allow-list gate (got: '$errmsg', resp: $resp)"
    pass "non-listed signer rejected on the allow-list gate: '$errmsg'"

    restore_proxy_config
    # The restart + allow-list window may have wedged the sponsor again (its
    # monitor kept submitting throughout) — leave the stack verified-healthy.
    drain_sponsor_wedge 10
    verify_sponsor_functional "post-allow-list-leg"
    pass "ALLOW-LIST LEG complete (original config restored, sponsor verified)"
}

if [[ "${ALLOWLIST_LEG:-0}" == "1" ]]; then
    run_allowlist_leg
else
    log "ALLOWLIST_LEG not set — skipping the proxy-restarting allow-list leg (disposable stacks only)"
fi
