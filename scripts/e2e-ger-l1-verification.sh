#!/usr/bin/env bash
# Audit H6 — L1 GER corroboration E2E (PR #121).
#
# aggoracle-supplied GER bytes (insertGlobalExitRoot / updateExitRoot) were
# trusted verbatim: a compromised signer could inject a FORGED GER (one whose
# (mainnet, rollup) decomposition the L1 InfoTree indexer never observed on
# L1) onto Miden — polluting on-chain state and burning operator gas.
#
# insert_ger (and, in writer mode, the request path before try_enqueue) now
# cross-checks the injected GER against the indexer's observed set: BOTH
# ger_entries roots must be resolved. Under --reject-unverified-ger-injection
# (implied by --require-hardening), an unverified GER is refused BEFORE any
# side effect: no accepted hash, no nonce, no tx row/receipt, no UpdateGerNote.
#
# Phases:
#   A. POSITIVE — prove a FRESH L1-observed GER is accepted and injected by
#      THIS submission (not a pre-injected one that dedup would wave through):
#        1. STOP the aggoracle (aggkit container) so it can't win the race and
#           inject the GER before/instead of us.
#        2. MINT a new L1 GER: one L1 bridgeAsset(forceUpdateGlobalExitRoot=true)
#           advances lastMainnetExitRoot → a brand-new (mainnet, rollup) pair.
#        3. Wait until the proxy's indexer has OBSERVED it on L1
#           (zkevm_getExitRootsByGER non-null) — the exact H6 evidence predicate.
#        4. ASSERT the precondition: this GER is NOT yet injected
#           (ger_entries.is_injected = false / no row, and no prior UpdateGerNote
#           log for it). This assert FAILS if the GER were already injected — so
#           the test cannot silently pass on a pre-injected GER via dedup.
#        5. Submit updateExitRoot(R, M); capture OUR tx hash.
#        6. Tie every success signal to THAT hash: receipt lands with
#           status == 0x1, ger_entries.is_injected flips true (the projector
#           sets it when the UpdateGerNote is CONSUMED), and a proxy log shows
#           the UpdateGerNote for THIS GER (which did not exist before step 5).
#   B. NEGATIVE — submit insertGlobalExitRoot with a FORGED 32-byte root that
#      never appeared in an L1 UpdateL1InfoTree event. Assert the H6 refusal
#      AND that the rejection is side-effect-free: no result hash, nonce NOT
#      consumed, re-broadcast of the identical raw tx is refused again (not
#      dedup-accepted as "known"), no ger_entries row, no UpdateGerNote
#      created/submitted for the forged root (proxy store + ANSI-stripped
#      logs), and ger_injection_unverified_total incremented.
#
# Requires the full E2E stack up (`make e2e-up`) with the service running
# strict H6 (the compose default: REJECT_UNVERIFIED_GER_INJECTION=true). If
# the container is lenient, the script SKIPs (exit 0) with a warning so suite
# runs with an explicit lenient override don't false-fail.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# fixtures/.env is optional here (only used for defaults if present).
[[ -f "$PROJECT_DIR/fixtures/.env" ]] && source "$PROJECT_DIR/fixtures/.env"

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} [e2e-ger-l1-verify] $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} [e2e-ger-l1-verify] $*"; }
fail() { echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} [e2e-ger-l1-verify] $*" >&2; exit 1; }
pass() { echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} [e2e-ger-l1-verify] $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} [e2e-ger-l1-verify] $*"; }

# JSON-RPC endpoint of the miden-agglayer proxy (where eth_sendRawTransaction
# lands and the /metrics scrape lives) — the PROXY on :8546, NOT the
# bridge-service REST on :18080. BRIDGE_SERVICE_URL kept as back-compat alias.
L2_RPC_URL="${L2_RPC_URL:-${BRIDGE_SERVICE_URL:-http://localhost:8546}}"
L1_RPC_URL="${L1_RPC:-${L1_RPC_URL:-http://localhost:8545}}"
L1_GER_ADDRESS="${L1_GER_ADDRESS:-0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674}"
L2_GER_ADDRESS="${L2_GER_ADDRESS:-0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA}"
GAS_PRICE_WEI="${GAS_PRICE_WEI:-1000000000}"
GAS_LIMIT="${GAS_LIMIT:-200000}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
PG_HOST="${PG_HOST:-localhost}"; PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"; PG_PASS="${PG_PASS:-agglayer}"; PG_DB="${PG_DB:-agglayer_store}"

# Signer of the test txs. The e2e proxy runs --insecure-allow-any-signer, so
# any key works and per-signer nonces keep it isolated; on an allow-listed
# deployment set SIGNER_KEY to a permitted key (e.g. the aggoracle key).
# Default: anvil dev key #9 (unused by the stack's own submitters). The same key
# funds the L1 bridgeAsset deposit Phase A uses to MINT a fresh L1 GER (all anvil
# dev keys are pre-funded on the L1 chain).
SIGNER_KEY="${SIGNER_KEY:-0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6}"

# L1 bridge — Phase A does one bridgeAsset(forceUpdateGlobalExitRoot=true) here
# to advance lastMainnetExitRoot and thereby MINT a brand-new L1 GER that the
# aggoracle has NOT yet injected (see Phase A header).
L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"

# The aggoracle lives in the aggkit container. Phase A STOPS it for the duration
# of the positive test so the proxy's OWN updateExitRoot submission is the tx
# under test — otherwise the aggoracle races us, injects the fresh GER first,
# and Phase A would false-pass through RD-940 tx-hash dedup on a GER that was
# actually injected by someone else. Restarted unconditionally on exit (trap).
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"
AGGKIT_STOPPED=0
cleanup() {
    if [[ "$AGGKIT_STOPPED" == "1" ]]; then
        step "cleanup — restarting aggoracle container $AGGKIT_CONTAINER"
        docker start "$AGGKIT_CONTAINER" >/dev/null 2>&1 \
            || warn "could not restart $AGGKIT_CONTAINER — restart it manually (docker start $AGGKIT_CONTAINER)"
    fi
}
trap cleanup EXIT

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
}

rpc_call() {
    local method="$1" params="$2"
    curl -s "$L2_RPC_URL" -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

# ANSI-stripped proxy logs, buffered (set -o pipefail + grep -q closes the
# pipe → docker logs exits 141 — see e2e-rd940-async-submit.sh).
proxy_logs() {
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g'
}

# ── Pre-flight ───────────────────────────────────────────────────────────────
command -v cast  >/dev/null || fail "cast (foundry) not found"
command -v psql  >/dev/null || fail "psql not found"
command -v python3 >/dev/null || fail "python3 not found"
CHAIN_PROBE=$(rpc_call eth_chainId "[]")
grep -q result <<<"$CHAIN_PROBE" || fail "proxy not reachable at $L2_RPC_URL"
cast block-number --rpc-url "$L1_RPC_URL" >/dev/null 2>&1 || fail "L1 (anvil) not reachable at $L1_RPC_URL"
pgquery "SELECT 1" >/dev/null || fail "PostgreSQL not reachable on $PG_HOST:$PG_PORT"
docker inspect "$AGGLAYER_CONTAINER" >/dev/null 2>&1 || fail "container $AGGLAYER_CONTAINER not found (set AGGLAYER_CONTAINER)"

# Strict-mode guard: Phase B only refuses when the container runs strict H6.
# Detect via the container's args/env; SKIP (not fail) when lenient so a suite
# run with an explicit REJECT_UNVERIFIED_GER_INJECTION=false override doesn't
# false-fail.
if ! docker inspect "$AGGLAYER_CONTAINER" \
        --format '{{join .Args " "}} {{join .Config.Env " "}}' 2>/dev/null \
        | grep -Eq -- '--reject-unverified-ger-injection|REJECT_UNVERIFIED_GER_INJECTION=true|REQUIRE_HARDENING=true'; then
    warn "container $AGGLAYER_CONTAINER is not running strict H6"
    warn "(set REJECT_UNVERIFIED_GER_INJECTION=true — the compose default — and restart)"
    warn "SKIPPING e2e-ger-l1-verification"
    exit 0
fi
log "strict H6 active in $AGGLAYER_CONTAINER"

SIGNER=$(cast wallet address --private-key "$SIGNER_KEY")
CHAIN_HEX=$(cast rpc eth_chainId --rpc-url "$L2_RPC_URL" | tr -d '"')
CHAIN_DEC=$((CHAIN_HEX))

signer_nonce() {
    local n
    n=$(cast rpc eth_getTransactionCount "$SIGNER" "latest" --rpc-url "$L2_RPC_URL" | tr -d '"')
    echo $((n))
}

json_field() { # $1 = field (result|error), stdin = JSON body
    python3 -c "import sys,json; r=json.load(sys.stdin).get('$1'); print('' if r is None else (r if isinstance(r,str) else json.dumps(r)))"
}

# ══════════════════════════════════════════════════════════════════════════════
# Phase A — POSITIVE: an L1-observed GER is accepted; UpdateGerNote submitted
#           and consumed.
# ══════════════════════════════════════════════════════════════════════════════
step "Phase A — a FRESH L1-observed GER must be accepted and injected by THIS tx"

# ── A.1 Deconflict the aggoracle ───────────────────────────────────────────
# Stop it BEFORE minting the GER so it can never observe+inject the new pair.
step "A.1 stopping aggoracle container $AGGKIT_CONTAINER to deconflict the race"
if docker inspect -f '{{.State.Running}}' "$AGGKIT_CONTAINER" 2>/dev/null | grep -q true; then
    docker stop "$AGGKIT_CONTAINER" >/dev/null || fail "could not stop $AGGKIT_CONTAINER"
    AGGKIT_STOPPED=1
    pass "aggoracle stopped (will be restarted on exit)"
else
    warn "aggoracle container $AGGKIT_CONTAINER not running — proceeding (no race to deconflict)"
fi

# ── A.2 Mint a brand-new L1 GER ────────────────────────────────────────────
# One L1 bridgeAsset with forceUpdateGlobalExitRoot=true advances
# lastMainnetExitRoot, minting a (mainnet, rollup) pair no one has injected yet.
MAIN_BEFORE=$(cast call "$L1_GER_ADDRESS" "lastMainnetExitRoot()(bytes32)" --rpc-url "$L1_RPC_URL")
step "A.2 minting a fresh L1 GER via bridgeAsset (forceUpdateGlobalExitRoot=true)"
cast send --rpc-url "$L1_RPC_URL" --private-key "$SIGNER_KEY" \
    "$L1_BRIDGE_ADDRESS" \
    'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
    1 "0x000000000000000000000000000000000000dEaD" "$DEPOSIT_WEI" \
    0x0000000000000000000000000000000000000000 true 0x \
    --value "$DEPOSIT_WEI" >/dev/null \
    || fail "L1 bridgeAsset failed — cannot mint a fresh GER (is $L1_BRIDGE_ADDRESS the L1 bridge?)"

MAIN=$(cast call "$L1_GER_ADDRESS" "lastMainnetExitRoot()(bytes32)" --rpc-url "$L1_RPC_URL")
ROLL=$(cast call "$L1_GER_ADDRESS" "lastRollupExitRoot()(bytes32)"  --rpc-url "$L1_RPC_URL")
[[ "$MAIN" != "$MAIN_BEFORE" ]] \
    || fail "lastMainnetExitRoot did not change after bridgeAsset ($MAIN) — no fresh GER was minted (L1 not auto-mining? wrong bridge addr?)"
COMB=$(cast keccak "$(cast concat-hex "$MAIN" "$ROLL")")
COMB_HEX="${COMB#0x}"
pass "fresh L1 GER minted: mainnet=$MAIN rollup=$ROLL combined=$COMB"

# ── A.3 Wait for the selected evidence scan to OBSERVE it ──────────────────
# The indexer scans exactly one configured frontier, so a resolved row already
# satisfies the same evidence predicate used by the strict H6 gate.
EVIDENCE_TAG="${L1_EVIDENCE_TAG:-latest}"
log "waiting for the L1 InfoTree indexer's '$EVIDENCE_TAG' scan to corroborate $COMB..."
ELAPSED=0; TIMEOUT=120
while true; do
    OBSERVED=$(rpc_call zkevm_getExitRootsByGER "[\"$COMB\"]" | json_field result || true)
    [[ -n "$OBSERVED" ]] && break
    ELAPSED=$((ELAPSED + 3))
    [[ $ELAPSED -ge $TIMEOUT ]] && fail "indexer did not corroborate the fresh L1 GER after ${TIMEOUT}s — is the L1InfoTreeIndexer running?"
    sleep 3
done
pass "the '$EVIDENCE_TAG' scan corroborated the fresh L1 GER (zkevm_getExitRootsByGER non-null)"

# ── A.4 ASSERT the not-yet-injected precondition ───────────────────────────
# THE adversarial guard: if this GER were already injected, the whole positive
# test would be meaningless (dedup would wave a foreign injection through). So
# refuse to continue unless it is provably fresh. This assert MUST fail on a
# pre-injected GER.
INJECTED_BEFORE=$(pgquery "SELECT COUNT(*) FROM ger_entries WHERE ger_hash = decode('${COMB_HEX}', 'hex') AND is_injected")
[[ "$INJECTED_BEFORE" == "0" ]] \
    || fail "precondition violated: GER $COMB is ALREADY injected before our submission (is_injected rows=$INJECTED_BEFORE) — the aggoracle race was not deconflicted, so a pass here would be a dedup false-pass. Aborting."
if grep "UpdateGerNote" <<<"$(proxy_logs)" | grep -qi "$COMB_HEX"; then
    fail "precondition violated: an UpdateGerNote for $COMB already exists in the logs before our submission — cannot attribute the injection to THIS tx"
fi
pass "precondition holds: GER $COMB is L1-observed but NOT yet injected"

# ── A.5 Submit — OUR updateExitRoot is the tx under test ───────────────────
# updateExitRoot ships BOTH roots in calldata (rollup FIRST — see
# one-shot-ger-inject.sh for why insertGlobalExitRoot would race L1 here).
NONCE_A=$(signer_nonce)
RAW_A=$(cast mktx "$L2_GER_ADDRESS" "updateExitRoot(bytes32,bytes32)" "$ROLL" "$MAIN" \
    --private-key "$SIGNER_KEY" --chain "$CHAIN_DEC" --nonce "$NONCE_A" \
    --legacy --gas-price "$GAS_PRICE_WEI" --gas-limit "$GAS_LIMIT")
OUT_A=$(rpc_call eth_sendRawTransaction "[\"$RAW_A\"]")
TX_A=$(echo "$OUT_A" | json_field result)
ERR_A=$(echo "$OUT_A" | json_field error)
[[ -n "$ERR_A" ]] && fail "fresh L1-observed GER was refused under strict H6 (false negative!): $ERR_A"
[[ -n "$TX_A" ]] || fail "no tx hash returned for the fresh L1-observed GER: $OUT_A"
pass "fresh L1-observed GER accepted: $TX_A"

# ── A.6 Tie every success signal to THIS tx hash ───────────────────────────
# Receipt must land AND carry status == 0x1 (a reverted/status-0 receipt is a
# failure the old check would have accepted as "non-null").
log "waiting for the receipt of $TX_A..."
ELAPSED=0; TIMEOUT=120
while true; do
    RCPT=$(rpc_call eth_getTransactionReceipt "[\"$TX_A\"]" | json_field result || true)
    [[ -n "$RCPT" ]] && break
    ELAPSED=$((ELAPSED + 3))
    [[ $ELAPSED -ge $TIMEOUT ]] && fail "receipt for the accepted GER tx $TX_A never landed after ${TIMEOUT}s"
    sleep 3
done
RCPT_STATUS=$(echo "$RCPT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))")
[[ "$RCPT_STATUS" == "0x1" ]] \
    || fail "receipt for $TX_A has status=$RCPT_STATUS (expected 0x1) — the accepted GER tx did not succeed"
pass "receipt landed for $TX_A with status 0x1"

# The UpdateGerNote must have reached Miden and been CONSUMED — the projector
# flips is_injected only when it observes the consumption. Because we asserted
# is_injected=false in A.4 and the aggoracle is stopped, this flip is
# attributable to OUR tx.
ELAPSED=0; TIMEOUT=120
while true; do
    INJECTED=$(pgquery "SELECT COUNT(*) FROM ger_entries WHERE ger_hash = decode('${COMB_HEX}', 'hex') AND is_injected" || true)
    [[ "$INJECTED" == "1" ]] && break
    ELAPSED=$((ELAPSED + 3))
    [[ $ELAPSED -ge $TIMEOUT ]] && fail "ger_entries.is_injected never flipped for the accepted GER $COMB — UpdateGerNote not consumed?"
    sleep 3
done
pass "UpdateGerNote consumed (ger_entries.is_injected = true) — attributable to $TX_A"

# And the proxy must have logged the UpdateGerNote for THIS ger — which did not
# exist in the logs before A.5 (asserted in A.4), so it is our submission's.
LOGS=$(proxy_logs)
grep -q "UpdateGerNote created" <<<"$LOGS" || fail "no 'UpdateGerNote created' log line at all"
grep "UpdateGerNote" <<<"$LOGS" | grep -qi "$COMB_HEX" \
    || fail "no UpdateGerNote log line references the accepted GER $COMB"
pass "Phase A complete — FRESH L1-observed GER accepted and injected by $TX_A"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# Phase B — NEGATIVE: a forged GER is refused with NO side effects.
# ══════════════════════════════════════════════════════════════════════════════
step "Phase B — forged GER must be refused, side-effect-free"

# A 32-byte root with no L1 observation: deliberately not a real L1 exit root,
# so the L1 InfoTree indexer never wrote its (mainnet, rollup) decomposition.
FORGED=0x$(printf 'cd%.0s' {1..32})
FORGED_HEX="${FORGED#0x}"

NONCE_B=$(signer_nonce)
log "signer=$SIGNER nonce=$NONCE_B chainId=$CHAIN_DEC forged=$FORGED"

# Build + sign a REAL insertGlobalExitRoot(bytes32) tx so eth_sendRawTransaction
# actually reaches the H6 gate (a placeholder string would only ever produce a
# DECODE error, never the "not observed on L1" refusal → a false-pass). cast
# mktx signs OFFLINE and does NOT broadcast.
RAW_B=$(cast mktx "$L2_GER_ADDRESS" "insertGlobalExitRoot(bytes32)" "$FORGED" \
    --private-key "$SIGNER_KEY" --chain "$CHAIN_DEC" --nonce "$NONCE_B" \
    --legacy --gas-price "$GAS_PRICE_WEI" --gas-limit "$GAS_LIMIT")

# -s (not -f): the H6 refusal comes back as a JSON-RPC error body over HTTP
# 200, which -f would swallow.
OUT_B=$(rpc_call eth_sendRawTransaction "[\"$RAW_B\"]")
RES_B=$(echo "$OUT_B" | json_field result)
ERR_B=$(echo "$OUT_B" | json_field error)

grep -q "not observed on L1" <<<"$OUT_B" \
    || fail "forged GER was not refused (audit H6 regression); response: $OUT_B"
[[ -z "$RES_B" ]] || fail "refusal must NOT return an accepted tx hash (got $RES_B) — the aggoracle would poll a receipt that can never exist"
pass "forged GER refused with the H6 error and no accepted hash"

# No nonce burn: the rejection must be invisible to the signer's sequence, so
# the identical signed tx (same nonce) stays broadcastable.
NONCE_B_AFTER=$(signer_nonce)
[[ "$NONCE_B_AFTER" == "$NONCE_B" ]] \
    || fail "rejection consumed the signer nonce ($NONCE_B → $NONCE_B_AFTER) — ethtxmanager wedge (PR #121 main blocker)"
pass "signer nonce not consumed ($NONCE_B)"

# Re-broadcast of the IDENTICAL raw tx must be refused again with the same H6
# error — NOT dedup-accepted as a "known" hash (pre-fix writer mode admitted
# the hash into the inflight cache before the gate, so the re-broadcast
# short-circuited to Ok and the caller polled a receipt forever).
OUT_B2=$(rpc_call eth_sendRawTransaction "[\"$RAW_B\"]")
grep -q "not observed on L1" <<<"$OUT_B2" \
    || fail "re-broadcast of the rejected tx was not refused again (dedup admitted a rejected hash?): $OUT_B2"
pass "identical re-broadcast refused again (no phantom dedup admission)"

# Exact no-submission proof, store side: the forged GER must not exist in
# ger_entries at all (the indexer never observed it; injection never ran).
FORGED_ROWS=$(pgquery "SELECT COUNT(*) FROM ger_entries WHERE ger_hash = decode('${FORGED_HEX}', 'hex')")
[[ "$FORGED_ROWS" == "0" ]] \
    || fail "forged GER present in ger_entries ($FORGED_ROWS row(s)) — something submitted it"

# Exact no-submission proof, Miden side: no UpdateGerNote was ever created /
# submitted for the forged root (ANSI-stripped logs; the note-creation log
# carries the ger hex).
LOGS=$(proxy_logs)
if grep -Ei "UpdateGerNote|GER injection: submitting" <<<"$LOGS" | grep -qi "$FORGED_HEX"; then
    fail "proxy logs show an UpdateGerNote / submission for the FORGED root — the gate did not stop the Miden submission"
fi
pass "no UpdateGerNote created or submitted for the forged GER (store + logs)"

# The unverified-GER metric must have incremented (>= 1: prior forged attempts
# in the same service lifetime, or indexer lag on legitimate GERs, may already
# have bumped it — an equality check would flake).
METRICS=$(curl -sf "$L2_RPC_URL/metrics")
echo "$METRICS" | awk '/^ger_injection_unverified_total /{ if ($2 + 0 >= 1) ok = 1 } END { exit(ok ? 0 : 1) }' \
    || fail "ger_injection_unverified_total did not increment (>= 1 expected)"
pass "ger_injection_unverified_total >= 1"

echo ""
log "======================================================================"
log "  H6 GER L1-VERIFICATION E2E COMPLETE"
log "  Phase A: FRESH L1-observed GER accepted+injected by our tx (status 0x1) ✓"
log "  Phase B: forged GER refused; no hash/nonce/row/note side effects    ✓"
log "======================================================================"
