#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Bridge RELIABILITY Load Test  (standalone — NOT part of the e2e suite)
#
# Sends N bridge round-trips across 10 tokens (1 native ETH + 9 ERC-20) and
# measures how RELIABLE the bridge is = what fraction of SUBMITTED bridges
# actually get DELIVERED (claimed) by the time the stack settles.
#
#   • L1→L2 deposits (bridgeAsset)  : submitted in PARALLEL batches of PARALLEL
#       (default 5) using EXPLICIT sequential nonces so they pack a block
#       without racing each other.
#   • L2→L1 bridge-outs (bridge-out-tool) : STRICTLY SEQUENTIAL, one at a time —
#       the Miden prover + account state cannot take concurrency.
#
# Reliability is read back from the bridge-service deposit index:
#   GET /bridges/<addr> → {deposits:[{ready_for_claim, claim_tx_hash, amount,
#   orig_addr, network_id, ...}]}.  L1→L2 deposits land at DEST_ADDR (the L2
#   wallet), L2→L1 deposits land at FUNDED_ADDR (the L1 EOA).
#
# Output: a clean RESULTS log (progress lines + final reliability matrix — tail
# this) and a VERBOSE log (all cast / bridge-out-tool output).
#
# Usage:
#   ./scripts/e2e-bridge-loadtest.sh                 # default N=250
#   N=6 PARALLEL=3 ./scripts/e2e-bridge-loadtest.sh  # small validation run
#
# Requires: a fresh stack already up (make e2e-up).  Run this against an
# otherwise-idle stack — it snapshots the bridge-service baseline after funding
# and reports DELTAS, so prior deposits don't pollute the matrix, but a quiet
# stack keeps the numbers easiest to read.
#
# set -uo pipefail (NOT -e): a single failed bridge must NOT abort the run —
# that failure IS the signal we are measuring. Failures are logged and counted.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L1_RPC="${L1_RPC:-http://localhost:8545}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-${COMPOSE_PROJECT_NAME}-miden-agglayer-1}"
AGGKIT_CONTAINER="${AGGKIT_CONTAINER:-${COMPOSE_PROJECT_NAME}-aggkit-1}"

# ── ISOLATED bridge-out client ────────────────────────────────────────────────
# This variant runs bridge-out-tool as a fully INDEPENDENT client in its own
# throwaway container against its OWN sqlite store (a host bind mount, distinct
# from the proxy's store.sqlite3). This mirrors production, where the B2AGG
# wallet is independent and the proxy's store has NO external accessor — so any
# "database is locked" in the proxy logs during this run is genuinely INTERNAL
# (miden-client's own connection pool), not the loadtest tool contending on the
# shared file.
# iso_tool(), provisioning etc. come from the shared helper. ISO_STORE_DIR is
# kept as a back-compat alias for this script's historical env knob; a fresh
# wallet is provisioned each run (B2AGG_FRESH=1) so the experiment always
# measures a clean, fully independent client.
B2AGG_STORE_DIR="${ISO_STORE_DIR:-${B2AGG_STORE_DIR:-$PROJECT_DIR/.b2agg-store/e2e-bridge-loadtest-isolated}}"
B2AGG_FRESH="${B2AGG_FRESH:-1}"
source "$SCRIPT_DIR/lib-isolated-wallet.sh"
ISO_STORE_DIR="$B2AGG_STORE_DIR"

FUNDED_KEY="${FUNDED_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
FUNDED_ADDR=$(cast wallet address --private-key "$FUNDED_KEY")
DEST_NETWORK=1  # Miden network id — local topology patch pins MIDEN_NETWORK_ID=1

# ── Tunables (env-overridable) ────────────────────────────────────────────────
N="${N:-250}"                     # total bridge ops (both directions)
PARALLEL="${PARALLEL:-5}"         # L1→L2 batch size (max concurrent same-block)
NUM_ERC20="${NUM_ERC20:-9}"       # ERC-20 token count (total tokens = 1 + this)
SEED="${SEED:-$$}"                # RNG seed for op selection (reproducible)

# Per-op bridge amounts.
WEI_PER_UNIT=10000000000          # 10^10 wei per Miden unit (18 ETH - 8 Miden dec)
AMT_L1_NATIVE_WEI="${AMT_L1_NATIVE_WEI:-10000000000000}"   # 1e13 wei  -> 1000 units
AMT_L1_ERC20_BASE="${AMT_L1_ERC20_BASE:-1000000000000000}" # 1e15 base -> 1e5  units
AMT_L2_UNITS="${AMT_L2_UNITS:-10}"                          # 10 Miden units per bridge-out

# Funding (large L1→L2 once per token so the L2 wallet has balance for bridge-outs).
FUND_NATIVE_WEI="${FUND_NATIVE_WEI:-10000000000000000}"     # 1e16 wei -> 1e6 units
FUND_ERC20_BASE="${FUND_ERC20_BASE:-1000000000000000000}"   # 1e18 base-> 1e8 units
TOKEN_DECIMALS=18
TOKEN_SUPPLY="1000000000000000000000000000"  # 1e27 — plenty for funding + load
APPROVE_HUGE="115792089237316195423570985008687907853269984665640564039457584007913129639935" # 2^256-1

# Settle polling.
SETTLE_STALL="${SETTLE_STALL:-300}"   # stop polling after this many s with no new claims
SETTLE_CAP="${SETTLE_CAP:-1800}"      # absolute backstop (30 min)
SETTLE_INTERVAL="${SETTLE_INTERVAL:-10}"

# ── Logs ──────────────────────────────────────────────────────────────────────
OUT_DIR="${OUT_DIR:-$PROJECT_DIR/.loadtest-results}"
mkdir -p "$OUT_DIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
RESULTS_LOG="${RESULTS_LOG:-$OUT_DIR/loadtest-$STAMP.results.log}"
VERBOSE_LOG="${VERBOSE_LOG:-$OUT_DIR/loadtest-$STAMP.verbose.log}"
: > "$RESULTS_LOG"
: > "$VERBOSE_LOG"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# r() → results log + stdout ; v() → verbose log only.
r()    { echo -e "[$(date +%H:%M:%S)] $*" | tee -a "$RESULTS_LOG"; }
rraw() { echo -e "$*" | tee -a "$RESULTS_LOG"; }
v()    { echo "[$(date +%H:%M:%S)] $*" >> "$VERBOSE_LOG"; }
die()  { echo -e "[$(date +%H:%M:%S)] FATAL: $*" | tee -a "$RESULTS_LOG" >&2; exit 1; }

# ── Python helpers (written once into $TMP) ───────────────────────────────────
# fetch_deposits.py <bridges-url> <network_id>: fetch ALL pages of the
# bridge-service deposit index and emit "orig_addr_lower<TAB>total<TAB>ready
# <TAB>claimed" per origin group, filtered to the requested network_id
# (0 = L1-origin L1→L2 ; 1 = Miden-origin L2→L1).
#
# WHY pagination + hard failure (display-artifact fix):
#   • /bridges/<addr> serves at most 25 rows by default, NEWEST first. Once an
#     endpoint accumulates >25 deposits, each new (unclaimed) deposit evicts an
#     older CLAIMED row from the default page — a naive count then shows
#     "claimed" DECREASING over time, which is impossible for real claims
#     (claim_tx_hash is permanent). Fetch every page (limit=100 + offset),
#     deduped by (network_id, deposit_cnt) in case a server ignores offset.
#   • On fetch/parse failure exit 3 (instead of silently emitting nothing,
#     which read back as claimed=0): callers retry, then hold the last-good
#     value, so a transient bridge-service hiccup can't fake a drop either.
cat > "$TMP/fetch_deposits.py" <<'PY'
import json, sys, urllib.request

base, want_net = sys.argv[1], int(sys.argv[2])
deps, seen, offset, LIMIT = [], set(), 0, 100
while True:
    url = f"{base}?limit={LIMIT}&offset={offset}"
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            d = json.load(r)
    except Exception:
        sys.exit(3)
    page = d.get("deposits", [])
    fresh = 0
    for dep in page:
        k = (dep.get("network_id"), dep.get("deposit_cnt"))
        if k in seen:
            continue
        seen.add(k)
        deps.append(dep)
        fresh += 1
    total = int(d.get("total_cnt") or 0)
    offset += len(page)
    # Stop on: empty page, no new rows (server ignoring offset), all rows
    # fetched, or a runaway index (hard cap).
    if not page or fresh == 0 or offset >= total or offset > 20000:
        break

groups = {}
for dep in deps:
    if dep.get("network_id") != want_net:
        continue
    oa = (dep.get("orig_addr") or "").lower()
    g = groups.setdefault(oa, [0, 0, 0])
    g[0] += 1
    if dep.get("ready_for_claim"):
        g[1] += 1
    if (dep.get("claim_tx_hash") or "") not in ("", None, "0x"):
        g[2] += 1
for oa, (t, rdy, cl) in groups.items():
    print(f"{oa}\t{t}\t{rdy}\t{cl}")
PY

NATIVE_ADDR="0x0000000000000000000000000000000000000000"

# fetch_groups <endpoint_addr> <want_net> → per-origin TSV (all pages), retried.
# Non-zero only when every attempt failed; callers then hold the last-good
# value. The cache lives in $TMP files (not shell vars) because count_* run
# inside $(...)/< <(...) subshells, where variable writes would be lost.
fetch_groups() {
    local out
    for _ in 1 2 3; do
        if out=$(python3 "$TMP/fetch_deposits.py" "$BRIDGE_SERVICE_URL/bridges/$1" "$2" 2>/dev/null); then
            printf '%s' "$out"
            return 0
        fi
        sleep 1
    done
    return 1
}

# count_for <endpoint_addr> <want_net> <orig_addr_lower>  → "total ready claimed"
count_for() {
    local endpoint="$1" net="$2" oa="$3" out
    local cache="$TMP/lastgood_for_${endpoint}_${net}_${oa}"
    if out=$(fetch_groups "$endpoint" "$net"); then
        out=$(printf '%s\n' "$out" | awk -v k="$oa" -F'\t' '$1==k{print $2, $3, $4; found=1} END{if(!found) print "0 0 0"}')
        printf '%s' "$out" > "$cache"
    else
        out=$(cat "$cache" 2>/dev/null || echo "0 0 0")
        v "count_for: bridge-service unreachable; holding last-good for ${endpoint}/${net}/${oa}: $out"
    fi
    echo "$out"
}

# count_all <endpoint_addr> <want_net>  → "total ready claimed" summed over all tokens
count_all() {
    local out
    local cache="$TMP/lastgood_all_${1}_${2}"
    if out=$(fetch_groups "$1" "$2"); then
        out=$(printf '%s\n' "$out" | awk -F'\t' '{t+=$2; r+=$3; c+=$4} END{print (t+0), (r+0), (c+0)}')
        printf '%s' "$out" > "$cache"
    else
        out=$(cat "$cache" 2>/dev/null || echo "0 0 0")
        v "count_all: bridge-service unreachable; holding last-good for ${1}/${2}: $out"
    fi
    echo "$out"
}

wait_for() {
    local desc="$1" cmd="$2" timeout="$3" interval="${4:-5}" elapsed=0
    v "WAIT: $desc (timeout ${timeout}s)"
    while ! ( set +o pipefail; eval "$cmd" ) 2>/dev/null; do
        elapsed=$((elapsed + interval))
        [[ $elapsed -ge $timeout ]] && { v "WAIT TIMEOUT: $desc"; return 1; }
        sleep "$interval"
    done
    return 0
}

# ══════════════════════════════════════════════════════════════════════════════
# Pre-flight
# ══════════════════════════════════════════════════════════════════════════════
command -v cast  >/dev/null || die "cast (foundry) not found"
command -v forge >/dev/null || die "forge (foundry) not found"
command -v python3 >/dev/null || die "python3 not found"
cast block-number --rpc-url "$L1_RPC" >/dev/null 2>&1 || die "L1 (Anvil) not reachable at $L1_RPC"
: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"

# bridge-service binds its HTTP API a little after the container is healthy.
BRIDGE_UP=false
for _ in $(seq 1 30); do
    if curl -sf "$BRIDGE_SERVICE_URL/bridges/$NATIVE_ADDR" >/dev/null 2>&1; then BRIDGE_UP=true; break; fi
    sleep 2
done
[[ "$BRIDGE_UP" == "true" ]] || die "Bridge service not reachable at $BRIDGE_SERVICE_URL"

# ── Account IDs + DEST_ADDR (zero-padded L2 wallet) ───────────────────────────
ACCOUNTS=$(docker exec "$AGGLAYER_CONTAINER" \
    cat /var/lib/miden-agglayer-service/bridge_accounts.toml 2>/dev/null) \
    || die "miden-agglayer not initialized"
# The bridge + ETH faucet are the proxy's global accounts (shared on the node);
# the bridge-out WALLET, however, is a fresh INDEPENDENT wallet we provision in
# the isolated store below (NOT wallet_hardhat).
BRIDGE_ID=$(echo "$ACCOUNTS" | grep 'bridge = ' | sed 's/.*= "//;s/"//')
FAUCET_ETH=$(echo "$ACCOUNTS" | grep faucet_eth | sed 's/.*= "//;s/"//')
[[ -n "$BRIDGE_ID" && -n "$FAUCET_ETH" ]] || die "could not parse bridge_accounts.toml"

# Provision a fully independent bridge-out wallet in its own isolated store
# (B2AGG_FRESH=1 above wipes any previous run's store first).
r "Provisioning INDEPENDENT bridge-out wallet (isolated store: $ISO_STORE_DIR)..."
provision_isolated_wallet 2>>"$VERBOSE_LOG" || die "wallet provisioning failed"
r "  independent wallet: $WALLET_ID"

# ── Token tables (index 0 = native ETH) ───────────────────────────────────────
declare -a T_ADDR T_LABEL T_FAUCET T_NATIVE
T_ADDR[0]="$NATIVE_ADDR"; T_LABEL[0]="ETH"; T_FAUCET[0]="$FAUCET_ETH"; T_NATIVE[0]=1
NUM_TOKENS=$((NUM_ERC20 + 1))

r "======================================================================"
r "  Bridge RELIABILITY Load Test (ISOLATED bridge-out client)"
r "======================================================================"
r "  N=$N  PARALLEL=$PARALLEL  tokens=$NUM_TOKENS (1 ETH + $NUM_ERC20 ERC20)  seed=$SEED"
r "  Wallet:  $WALLET_HEX"
r "  Dest:    $DEST_ADDR"
r "  Results: $RESULTS_LOG"
r "  Verbose: $VERBOSE_LOG"
r "======================================================================"

# ══════════════════════════════════════════════════════════════════════════════
# Setup: deploy + approve ERC-20s
# ══════════════════════════════════════════════════════════════════════════════
r "Setup 1/4: deploying $NUM_ERC20 ERC-20 tokens (distinct symbols TT0..TT$((NUM_ERC20-1)))..."
for k in $(seq 0 $((NUM_ERC20 - 1))); do
    idx=$((k + 1))
    sym="TT$k"
    out=$(forge create "$FIXTURES_DIR/TestToken.sol:TestToken" \
        --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" --broadcast \
        --constructor-args "LoadToken$k" "$sym" "$TOKEN_DECIMALS" "$TOKEN_SUPPLY" 2>&1)
    echo "$out" >> "$VERBOSE_LOG"
    addr=$(echo "$out" | grep "Deployed to:" | awk '{print $NF}')
    [[ -n "$addr" ]] || die "deploy failed for $sym: $(echo "$out" | tail -3)"
    T_ADDR[$idx]="$addr"; T_LABEL[$idx]="$sym"; T_NATIVE[$idx]=0; T_FAUCET[$idx]=""
    # Pre-approve the bridge once, huge, so per-bridge sends skip approve.
    cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" \
        "$addr" "approve(address,uint256)" "$BRIDGE_ADDRESS" "$APPROVE_HUGE" \
        >> "$VERBOSE_LOG" 2>&1 || die "approve failed for $sym ($addr)"
    r "  $sym -> $addr (approved)"
done

# ══════════════════════════════════════════════════════════════════════════════
# Setup 2/4: fund the L2 wallet — one LARGE L1→L2 per token (parallel batches).
# ══════════════════════════════════════════════════════════════════════════════
# submit_l1 <nonce> <token_idx> <amount> <result_file>  (backgroundable)
submit_l1() {
    local nonce="$1" ti="$2" amt="$3" rf="$4"
    # --gas-limit: parallel same-block deposits each grow the bridge exit-tree, so a
    # deposit mined later in the block costs more gas than cast's pre-block estimate
    # -> out-of-gas revert (status 0). A generous fixed limit sidesteps the
    # under-estimate (Anvil gas is effectively free, so over-provisioning is harmless).
    if [[ "${T_NATIVE[$ti]}" == "1" ]]; then
        cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" --nonce "$nonce" --gas-limit 2000000 \
            "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
            "$DEST_NETWORK" "$DEST_ADDR" "$amt" \
            "$NATIVE_ADDR" true 0x --value "$amt" > "$rf" 2>&1
    else
        cast send --rpc-url "$L1_RPC" --private-key "$FUNDED_KEY" --nonce "$nonce" --gas-limit 2000000 \
            "$BRIDGE_ADDRESS" 'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
            "$DEST_NETWORK" "$DEST_ADDR" "$amt" \
            "${T_ADDR[$ti]}" true 0x > "$rf" 2>&1
    fi
}
l1_status() { awk '$1=="status"{print $2; exit}' "$1"; }

r "Setup 2/4: funding L2 wallet (1 large L1→L2 per token, batches of $PARALLEL)..."
fund_idx=0
while [[ $fund_idx -lt $NUM_TOKENS ]]; do
    base_nonce=$(cast nonce --rpc-url "$L1_RPC" "$FUNDED_ADDR")
    pids=(); slots=()
    for j in $(seq 0 $((PARALLEL - 1))); do
        ti=$((fund_idx + j))
        [[ $ti -ge $NUM_TOKENS ]] && break
        amt="$FUND_ERC20_BASE"; [[ "${T_NATIVE[$ti]}" == "1" ]] && amt="$FUND_NATIVE_WEI"
        submit_l1 $((base_nonce + j)) "$ti" "$amt" "$TMP/fund_$ti" &
        pids+=($!); slots+=("$ti")
    done
    for p in "${pids[@]}"; do wait "$p"; done
    for ti in "${slots[@]}"; do
        st=$(l1_status "$TMP/fund_$ti")
        [[ "$st" == "1" ]] || die "funding L1 tx failed for ${T_LABEL[$ti]} (status=$st)"
    done
    fund_idx=$((fund_idx + PARALLEL))
done
r "  all $NUM_TOKENS funding deposits submitted on L1"

# Wait until all funding deposits are ready_for_claim on L2 (count >= NUM_TOKENS).
r "Setup 3/4: waiting for funding deposits to be ready_for_claim + auto-claimed..."
wait_for "all funding deposits ready_for_claim" \
    "[ \$(curl -sf '$BRIDGE_SERVICE_URL/bridges/$DEST_ADDR?limit=100' 2>/dev/null | python3 -c \"import json,sys; d=json.load(sys.stdin); print(len([x for x in d.get('deposits',[]) if x.get('ready_for_claim') and x.get('amount')!='0']))\" 2>/dev/null || echo 0) -ge $NUM_TOKENS ]" \
    600 5 || r "  WARN: not all funding deposits ready within 600s (continuing)"

# Build token -> faucet_id map from admin_listFaucets (key on origin token addr).
r "Setup 4/4: mapping tokens -> faucet ids + confirming L2 balances..."
map_faucets() {
    local fj
    fj=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" \
        -d '{"jsonrpc":"2.0","method":"admin_listFaucets","params":[],"id":1}' 2>/dev/null)
    echo "$fj" >> "$VERBOSE_LOG"
    for k in $(seq 1 $NUM_ERC20); do
        [[ -n "${T_FAUCET[$k]}" ]] && continue
        local fid
        fid=$(echo "$fj" | python3 -c "
import json,sys
want='${T_ADDR[$k]}'.lower(); sym='${T_LABEL[$k]}'
try: r=json.load(sys.stdin)
except Exception: r={}
for f in r.get('result',[]):
    oa=(f.get('origin_address') or f.get('orig_addr') or '').lower()
    if oa==want or f.get('symbol')==sym:
        print(f.get('faucet_id') or ''); break
" 2>/dev/null)
        [[ -n "$fid" ]] && T_FAUCET[$k]="$fid"
    done
}
# Faucets auto-create on the first claim; retry the mapping while claims land.
# The Miden prover processes claims ~1-2/min, so 10 funding claims can take
# ~15min to all land — wait generously (FAUCET_WAIT_ATTEMPTS x 10s).
for attempt in $(seq 1 "${FAUCET_WAIT_ATTEMPTS:-180}"); do
    map_faucets
    missing=0
    for k in $(seq 1 $NUM_ERC20); do [[ -z "${T_FAUCET[$k]}" ]] && missing=$((missing+1)); done
    [[ $missing -eq 0 ]] && break
    v "faucet map: $missing tokens still unmapped (attempt $attempt)"
    sleep 10
done
for k in $(seq 0 $((NUM_TOKENS - 1))); do
    [[ -z "${T_FAUCET[$k]}" ]] && die "no faucet id for ${T_LABEL[$k]} (${T_ADDR[$k]}) — funding claim may not have landed"
    r "  ${T_LABEL[$k]}: faucet=${T_FAUCET[$k]}"
done

# Confirm each faucet shows a positive L2 wallet balance (funding delivered).
for k in $(seq 0 $((NUM_TOKENS - 1))); do
    bal=0
    for attempt in $(seq 1 18); do
        out=$(iso_tool \
            --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "${T_FAUCET[$k]}" \
            --amount 999999999 --dest-address 0xdead --dest-network 0 2>&1 || true)
        echo "$out" >> "$VERBOSE_LOG"
        bal=$(echo "$out" | grep "wallet balance:" | head -1 | awk '{print $NF}')
        [[ -n "$bal" && "$bal" != "0" ]] && break
        sleep 10
    done
    [[ -n "$bal" && "$bal" != "0" ]] || die "${T_LABEL[$k]} L2 balance still 0 — cannot fund bridge-outs"
    r "  ${T_LABEL[$k]}: L2 balance=$bal"
done

# ── Baseline snapshot (so the matrix reports only load-loop deltas) ───────────
declare -a BASE_L1_T BASE_L1_R BASE_L1_C BASE_L2_T BASE_L2_R BASE_L2_C
for k in $(seq 0 $((NUM_TOKENS - 1))); do
    read -r t rdy cl < <(count_for "$DEST_ADDR" 0 "$(echo "${T_ADDR[$k]}" | tr 'A-F' 'a-f')")
    BASE_L1_T[$k]=$t; BASE_L1_R[$k]=$rdy; BASE_L1_C[$k]=$cl
    read -r t rdy cl < <(count_for "$FUNDED_ADDR" 1 "$(echo "${T_ADDR[$k]}" | tr 'A-F' 'a-f')")
    BASE_L2_T[$k]=$t; BASE_L2_R[$k]=$rdy; BASE_L2_C[$k]=$cl
done
# aggregate baseline totals (subtracted in the live status line so it shows load-only)
BASE_L1_R_TOT=0; BASE_L1_C_TOT=0; BASE_L2_R_TOT=0; BASE_L2_C_TOT=0
for k in $(seq 0 $((NUM_TOKENS - 1))); do
    BASE_L1_R_TOT=$((BASE_L1_R_TOT + BASE_L1_R[$k])); BASE_L1_C_TOT=$((BASE_L1_C_TOT + BASE_L1_C[$k]))
    BASE_L2_R_TOT=$((BASE_L2_R_TOT + BASE_L2_R[$k])); BASE_L2_C_TOT=$((BASE_L2_C_TOT + BASE_L2_C[$k]))
done
v "baseline captured"

# ══════════════════════════════════════════════════════════════════════════════
# Load loop
# ══════════════════════════════════════════════════════════════════════════════
# Build the randomized op plan: N ops, each (token_idx, dir). dir: 0=L1→L2, 1=L2→L1.
RANDOM=$SEED
declare -a OP_TOKEN OP_DIR
declare -a SUB_L1 SUB_L2 FAIL_L1 FAIL_L2
for k in $(seq 0 $((NUM_TOKENS - 1))); do SUB_L1[$k]=0; SUB_L2[$k]=0; FAIL_L1[$k]=0; FAIL_L2[$k]=0; done
PLAN_L1=0; PLAN_L2=0
for i in $(seq 1 "$N"); do
    OP_TOKEN[$i]=$((RANDOM % NUM_TOKENS))
    d=$((RANDOM % 2)); OP_DIR[$i]=$d
    [[ $d -eq 0 ]] && PLAN_L1=$((PLAN_L1+1)) || PLAN_L2=$((PLAN_L2+1))
done
r "----------------------------------------------------------------------"
r "Load: $N ops planned — L1→L2=$PLAN_L1  L2→L1=$PLAN_L2"
r "----------------------------------------------------------------------"

DONE_L1=0; DONE_L2=0
declare -a L1BUF_OP L1BUF_TI
L1BUF_OP=(); L1BUF_TI=()

progress() {  # <op#> <label> <dir-str> <verb>
    r "#$1 ${2} ${3} ${4}"
}

# status_line: the live picture both ways — submitted (our counters) / ready / claimed
# (bridge-service, load-only via baseline subtraction) / failed. Printed each step.
status_line() {
    local s1=0 f1=0 s2=0 f2=0 k
    for k in $(seq 0 $((NUM_TOKENS - 1))); do
        s1=$((s1 + SUB_L1[$k])); f1=$((f1 + FAIL_L1[$k]))
        s2=$((s2 + SUB_L2[$k])); f2=$((f2 + FAIL_L2[$k]))
    done
    local t1 r1 c1 t2 r2 c2
    read -r t1 r1 c1 < <(count_all "$DEST_ADDR" 0)
    read -r t2 r2 c2 < <(count_all "$FUNDED_ADDR" 1)
    r "   ┊ L1→L2  submitted=$s1/$PLAN_L1  ready=$((r1 - BASE_L1_R_TOT))  claimed=$((c1 - BASE_L1_C_TOT))  failed=$f1"
    r "   ┊ L2→L1  submitted=$s2/$PLAN_L2  ready=$((r2 - BASE_L2_R_TOT))  claimed=$((c2 - BASE_L2_C_TOT))  failed=$f2"
}

flush_l1_buf() {
    [[ ${#L1BUF_OP[@]} -eq 0 ]] && return
    local base_nonce; base_nonce=$(cast nonce --rpc-url "$L1_RPC" "$FUNDED_ADDR")
    local pids=() j=0
    for n in "${!L1BUF_OP[@]}"; do
        local ti="${L1BUF_TI[$n]}"
        local amt="$AMT_L1_ERC20_BASE"; [[ "${T_NATIVE[$ti]}" == "1" ]] && amt="$AMT_L1_NATIVE_WEI"
        submit_l1 $((base_nonce + j)) "$ti" "$amt" "$TMP/op_${L1BUF_OP[$n]}" &
        pids+=($!); j=$((j+1))
    done
    for p in "${pids[@]}"; do wait "$p"; done
    for n in "${!L1BUF_OP[@]}"; do
        local op="${L1BUF_OP[$n]}" ti="${L1BUF_TI[$n]}"
        local st; st=$(l1_status "$TMP/op_$op")
        DONE_L1=$((DONE_L1+1))
        if [[ "$st" == "1" ]]; then
            SUB_L1[$ti]=$(( ${SUB_L1[$ti]} + 1 ))
            progress "$op" "${T_LABEL[$ti]}" "L1→L2" "submitted"
        else
            FAIL_L1[$ti]=$(( ${FAIL_L1[$ti]} + 1 ))
            v "OP $op L1→L2 ${T_LABEL[$ti]} SUBMIT FAILED (status=$st): $(tail -2 "$TMP/op_$op" | tr '\n' ' ')"
            progress "$op" "${T_LABEL[$ti]}" "L1→L2" "SUBMIT-FAILED"
        fi
    done
    L1BUF_OP=(); L1BUF_TI=()
    status_line
}

run_l2_out() {  # <op#> <token_idx>
    local op="$1" ti="$2"
    DONE_L2=$((DONE_L2+1))
    local out rc
    out=$(iso_tool \
        --wallet-id "$WALLET_ID" --bridge-id "$BRIDGE_ID" --faucet-id "${T_FAUCET[$ti]}" \
        --amount "$AMT_L2_UNITS" --dest-address "$FUNDED_ADDR" --dest-network 0 2>&1)
    rc=$?
    echo "$out" >> "$VERBOSE_LOG"
    if [[ $rc -eq 0 ]]; then
        SUB_L2[$ti]=$(( ${SUB_L2[$ti]} + 1 ))
        progress "$op" "${T_LABEL[$ti]}" "L2→L1" "submitted"
    else
        FAIL_L2[$ti]=$(( ${FAIL_L2[$ti]} + 1 ))
        v "OP $op L2→L1 ${T_LABEL[$ti]} BRIDGE-OUT FAILED (rc=$rc): $(echo "$out" | tail -2 | tr '\n' ' ')"
        progress "$op" "${T_LABEL[$ti]}" "L2→L1" "BRIDGE-OUT-FAILED"
    fi
    status_line
}

LOAD_START=$(date +%s)
for i in $(seq 1 "$N"); do
    ti="${OP_TOKEN[$i]}"; d="${OP_DIR[$i]}"
    if [[ "$d" -eq 0 ]]; then
        L1BUF_OP+=("$i"); L1BUF_TI+=("$ti")
        [[ ${#L1BUF_OP[@]} -ge $PARALLEL ]] && flush_l1_buf
    else
        flush_l1_buf          # keep ordering: drain pending L1 batch before a sequential L2 op
        run_l2_out "$i" "$ti"
    fi
done
flush_l1_buf
LOAD_SECS=$(( $(date +%s) - LOAD_START ))

TOT_SUB_L1=0; TOT_SUB_L2=0; TOT_FAIL_L1=0; TOT_FAIL_L2=0
for k in $(seq 0 $((NUM_TOKENS - 1))); do
    TOT_SUB_L1=$((TOT_SUB_L1 + SUB_L1[$k])); TOT_SUB_L2=$((TOT_SUB_L2 + SUB_L2[$k]))
    TOT_FAIL_L1=$((TOT_FAIL_L1 + FAIL_L1[$k])); TOT_FAIL_L2=$((TOT_FAIL_L2 + FAIL_L2[$k]))
done
r "----------------------------------------------------------------------"
r "Load complete in ${LOAD_SECS}s — submitted L1→L2=$TOT_SUB_L1 (fail $TOT_FAIL_L1), L2→L1=$TOT_SUB_L2 (fail $TOT_FAIL_L2)"
r "----------------------------------------------------------------------"

# ══════════════════════════════════════════════════════════════════════════════
# Settle: poll until claimed counts stop rising
# ══════════════════════════════════════════════════════════════════════════════
# total claimed (delta over baseline) across all tokens, both directions
total_claimed_delta() {
    local sum=0 k oa t rdy cl
    for k in $(seq 0 $((NUM_TOKENS - 1))); do
        oa=$(echo "${T_ADDR[$k]}" | tr 'A-F' 'a-f')
        read -r t rdy cl < <(count_for "$DEST_ADDR" 0 "$oa")
        sum=$((sum + cl - BASE_L1_C[$k]))
        read -r t rdy cl < <(count_for "$FUNDED_ADDR" 1 "$oa")
        sum=$((sum + cl - BASE_L2_C[$k]))
    done
    echo "$sum"
}

r "Settle: polling bridge-service until claims stall (${SETTLE_STALL}s) or cap ${SETTLE_CAP}s..."
last=-1; stalled=0; elapsed=0
while :; do
    cur=$(total_claimed_delta)
    if [[ "$cur" != "$last" ]]; then
        r "  settle: claimed(delta)=$cur  (elapsed ${elapsed}s)"
        last="$cur"; stalled=0
    else
        stalled=$((stalled + SETTLE_INTERVAL))
    fi
    [[ $stalled -ge $SETTLE_STALL ]] && { r "  settle: no new claims for ${SETTLE_STALL}s — done"; break; }
    [[ $elapsed -ge $SETTLE_CAP ]]   && { r "  settle: hit ${SETTLE_CAP}s cap — done"; break; }
    sleep "$SETTLE_INTERVAL"; elapsed=$((elapsed + SETTLE_INTERVAL))
done

# ══════════════════════════════════════════════════════════════════════════════
# Reliability matrix
# ══════════════════════════════════════════════════════════════════════════════
pct() { [[ "$2" -eq 0 ]] && { echo "  n/a"; return; }; LC_NUMERIC=C awk -v a="$1" -v b="$2" 'BEGIN{printf "%5.1f%%", 100*a/b}'; }

rraw ""
r "======================================================================"
r "  RELIABILITY MATRIX  (delivered = claim_tx_hash present)"
r "======================================================================"
printf "%-7s %-7s %9s %9s %9s %9s %8s\n" "TOKEN" "DIR" "submit" "ready" "claimed" "fail" "deliv%" | tee -a "$RESULTS_LOG"
r "----------------------------------------------------------------------"

declare -i G_SUB=0 G_RDY=0 G_CLM=0 G_FAIL=0
FAILURES_DETAIL=""
emit_row() {  # <k> <dir 0|1>
    local k="$1" dir="$2" endpoint net sub fail t rdy cl oa
    oa=$(echo "${T_ADDR[$k]}" | tr 'A-F' 'a-f')
    if [[ "$dir" -eq 0 ]]; then
        endpoint="$DEST_ADDR"; net=0; sub=${SUB_L1[$k]}; fail=${FAIL_L1[$k]}
        read -r t rdy cl < <(count_for "$endpoint" "$net" "$oa")
        rdy=$((rdy - BASE_L1_R[$k])); cl=$((cl - BASE_L1_C[$k]))
        dirs="L1→L2"
    else
        endpoint="$FUNDED_ADDR"; net=1; sub=${SUB_L2[$k]}; fail=${FAIL_L2[$k]}
        read -r t rdy cl < <(count_for "$endpoint" "$net" "$oa")
        rdy=$((rdy - BASE_L2_R[$k])); cl=$((cl - BASE_L2_C[$k]))
        dirs="L2→L1"
    fi
    [[ $rdy -lt 0 ]] && rdy=0; [[ $cl -lt 0 ]] && cl=0
    printf "%-7s %-7s %9s %9s %9s %9s %8s\n" \
        "${T_LABEL[$k]}" "$dirs" "$sub" "$rdy" "$cl" "$fail" "$(pct "$cl" "$sub")" | tee -a "$RESULTS_LOG"
    G_SUB+=$sub; G_RDY+=$rdy; G_CLM+=$cl; G_FAIL+=$fail
    local undeliv=$((sub - cl))
    [[ $undeliv -gt 0 ]] && FAILURES_DETAIL+="  ${T_LABEL[$k]} ${dirs}: ${undeliv} submitted-but-unclaimed (amount ${3})\n"
}

for k in $(seq 0 $((NUM_TOKENS - 1))); do
    amt="$AMT_L1_ERC20_BASE"; [[ "${T_NATIVE[$k]}" == "1" ]] && amt="$AMT_L1_NATIVE_WEI"
    emit_row "$k" 0 "$amt"
done
for k in $(seq 0 $((NUM_TOKENS - 1))); do emit_row "$k" 1 "$AMT_L2_UNITS units"; done

r "----------------------------------------------------------------------"
printf "%-7s %-7s %9s %9s %9s %9s %8s\n" "TOTAL" "both" "$G_SUB" "$G_RDY" "$G_CLM" "$G_FAIL" "$(pct "$G_CLM" "$G_SUB")" | tee -a "$RESULTS_LOG"
r "======================================================================"
r "  OVERALL RELIABILITY: $(pct "$G_CLM" "$G_SUB")  ($G_CLM claimed / $G_SUB submitted)"
r "  ready_for_claim but not yet claimed: $((G_RDY > G_CLM ? G_RDY - G_CLM : 0))"
r "======================================================================"
if [[ -n "$FAILURES_DETAIL" ]]; then
    r "Submitted-but-undelivered (the failures):"
    rraw "$FAILURES_DETAIL"
else
    r "No undelivered bridges — 100% of submitted bridges were claimed."
fi
r "Results log: $RESULTS_LOG"
r "Verbose log: $VERBOSE_LOG"

# ══════════════════════════════════════════════════════════════════════════════
# THE experiment signal: with the bridge-out tool fully isolated (its own store),
# the proxy's store.sqlite3 has NO external accessor. Count "database is locked"
# in the proxy logs — any hit is a genuine INTERNAL lock (miden-client's own
# pool), which a singleton MidenClient cannot fix (only WAL / pool-size-1 can).
r "======================================================================"
LOCK_COUNT=$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -c "database is locked" || true)
r "  PROXY 'database is locked' count: ${LOCK_COUNT}"
if [[ "${LOCK_COUNT:-0}" -gt 0 ]]; then
    r "  => INTERNAL lock reproduced with the tool isolated + singleton enforced."
    r "     Sample lines:"
    docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep "database is locked" | head -5 | tee -a "$RESULTS_LOG"
else
    r "  => No proxy locks. With the external writer removed, the loadtest is lock-free"
    r "     (i.e. the shared-store bridge-out tool was the loadtest's lock source)."
fi
r "======================================================================"

# ── Independent event-completeness verification ──────────────────────────────
# Cross-checks the miden-node DB (consumed B2AGG/CLAIM/GER notes, by canonical
# script root) against eth_getLogs on the synthetic L2 — each event must exist
# at EXACTLY the note's consumption block. Failure fails the whole loadtest.
VERIFY_RC=0
if [[ "${VERIFY:-1}" == "1" ]]; then
    r "Event-completeness verification (node DB ⇄ eth_getLogs):"
    ALLOW_LATE="${ALLOW_LATE:-1}" "$SCRIPT_DIR/verify-event-completeness.sh" 2>&1                                               | tee -a "$RESULTS_LOG"
    VERIFY_RC=${PIPESTATUS[0]}
    r "verification exit: $VERIFY_RC"
fi
exit $(( LOCK_COUNT > 0 ? 1 : VERIFY_RC ))
# Forensics: archive proxy logs (reconciler warnings, import errors, sweep
# lines) so rung teardown can't destroy the evidence for a failed run.
docker logs "$AGGLAYER_CONTAINER" > "$OUT_DIR/proxy-$STAMP.log" 2>&1 || true
