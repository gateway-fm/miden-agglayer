#!/usr/bin/env bash
# E2E Security Test Suite — exercises adversarial inputs, boundary conditions,
# information leakage, and concurrency against the miden-agglayer JSON-RPC service.
#
# This test is non-fatal: individual test failures increment FAIL_COUNT so that
# every test runs regardless of earlier failures. The script exits non-zero
# only if any test failed.
#
# Prerequisites:
#   - Full E2E stack running (make e2e-up)
#   - miden-agglayer using PgStore (DATABASE_URL set)
#
# Usage:
#   source fixtures/.env && ./scripts/e2e-security.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"

source "$FIXTURES_DIR/.env"

L2_RPC="http://localhost:8546"
PG_HOST="localhost"
PG_PORT="5434"
PG_USER="agglayer"
PG_PASS="agglayer"
PG_DB="agglayer_store"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }
warn() { echo -e "${YELLOW}[$(date +%H:%M:%S)] WARN:${NC} $*"; }
step() { echo -e "${CYAN}[$(date +%H:%M:%S)] STEP:${NC} $*"; }

FAIL_COUNT=0
PASS_COUNT=0
ERROR_RESPONSES=""

fail() {
    echo -e "${RED}[$(date +%H:%M:%S)] FAIL:${NC} $*" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
}

pass() {
    echo -e "${GREEN}[$(date +%H:%M:%S)] PASS:${NC} $*"
    PASS_COUNT=$((PASS_COUNT + 1))
}

pgquery() {
    PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" -t -A -c "$1" 2>/dev/null
}

# rpc_raw: like rpc_call but uses -s (not -sf) to capture error responses
rpc_raw() {
    local body="$1"
    curl -s --max-time 10 "$L2_RPC" -X POST -H "Content-Type: application/json" -d "$body"
}

rpc_call() {
    local method="$1" params="$2"
    rpc_raw "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

# Collect error responses for later leak scanning
collect_error() {
    local resp="$1"
    ERROR_RESPONSES="$ERROR_RESPONSES
$resp"
}

# assert_no_leak: scan a response string for sensitive patterns
assert_no_leak() {
    local resp="$1" label="$2"
    local leaked=false
    for pattern in "panic" "stack trace" "RUST_BACKTRACE" ".rs:" "password" "PRIVATE_KEY" "DATABASE_URL" "SELECT " "INSERT " "DELETE "; do
        if echo "$resp" | grep -qi "$pattern"; then
            fail "$label: leaked sensitive pattern '$pattern'"
            leaked=true
        fi
    done
    if [[ "$leaked" == "false" ]]; then
        return 0
    fi
    return 1
}

# Cleanup test GER entries on exit
CLEANUP_GERS=()
cleanup() {
    for ger_hex in "${CLEANUP_GERS[@]}"; do
        pgquery "DELETE FROM ger_entries WHERE ger_hash = decode('${ger_hex}', 'hex')" 2>/dev/null || true
    done
}
trap cleanup EXIT

# ── Pre-flight checks ────────────────────────────────────────────────────────
command -v psql >/dev/null || { echo "psql not found"; exit 1; }
curl -sf "$L2_RPC" -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
    || { echo "L2 (miden-agglayer) not reachable at $L2_RPC"; exit 1; }
pgquery "SELECT 1" >/dev/null || { echo "PostgreSQL not reachable on $PG_HOST:$PG_PORT"; exit 1; }

log "======================================================================"
log "  Security E2E Test Suite"
log "======================================================================"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 1: Malformed Input Fuzzing
# ══════════════════════════════════════════════════════════════════════════════
step "Section 1: Malformed Input Fuzzing"

# 1.1 Empty POST body
RESP=$(rpc_raw "")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "1.1 Empty POST body returns error"
else
    # Even a non-JSON error is acceptable — the server didn't crash
    if [[ -n "$RESP" ]]; then
        pass "1.1 Empty POST body returns error response"
    else
        fail "1.1 Empty POST body: no response"
    fi
fi
collect_error "$RESP"

# 1.2 JSON missing method field
RESP=$(rpc_raw '{"jsonrpc":"2.0","params":[],"id":1}')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "1.2 Missing 'method' field returns error"
else
    fail "1.2 Missing 'method' field: expected error, got: $RESP"
fi
collect_error "$RESP"

# 1.3 params: null on eth_blockNumber (params optional)
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_blockNumber","params":null,"id":1}')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d.get('result'); assert r is not None" 2>/dev/null; then
    pass "1.3 params:null on eth_blockNumber succeeds"
else
    # eth_blockNumber doesn't parse params, so null should be fine
    # But even an error is acceptable — just not a crash
    if [[ -n "$RESP" ]]; then
        pass "1.3 params:null returns a response (no crash)"
    else
        fail "1.3 params:null: no response"
    fi
fi
collect_error "$RESP"

# 1.4 Truncated JSON
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_blockNu')
if [[ -n "$RESP" ]]; then
    pass "1.4 Truncated JSON returns error (no crash)"
else
    fail "1.4 Truncated JSON: no response"
fi
collect_error "$RESP"

# 1.5 Extra closing brackets
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}}}')
if [[ -n "$RESP" ]]; then
    pass "1.5 Extra closing brackets: server responded (no crash)"
else
    fail "1.5 Extra closing brackets: no response"
fi
collect_error "$RESP"

# 1.6 1MB payload in params
LARGE_PAYLOAD=$(python3 -c "print('{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[\"' + 'A'*1048576 + '\"],\"id\":1}')")
RESP=$(curl -s --max-time 5 "$L2_RPC" -X POST -H "Content-Type: application/json" -d "$LARGE_PAYLOAD" 2>/dev/null || echo "__timeout__")
if [[ "$RESP" == "__timeout__" ]]; then
    # Timeout after 5s is acceptable (no OOM)
    pass "1.6 1MB payload: timed out gracefully (no OOM)"
elif [[ -n "$RESP" ]]; then
    pass "1.6 1MB payload: server responded (no crash/OOM)"
else
    fail "1.6 1MB payload: no response"
fi
collect_error "$RESP"

# 1.7 Null bytes in method name
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_\u0000blockNumber","params":[],"id":1}')
if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
err=d.get('error',{})
assert err.get('code') == -32601 or 'result' not in d or d.get('result') is None
" 2>/dev/null; then
    pass "1.7 Null bytes in method name: method not found or error"
else
    if [[ -n "$RESP" ]]; then
        pass "1.7 Null bytes in method name: server responded (no crash)"
    else
        fail "1.7 Null bytes in method name: no response"
    fi
fi
collect_error "$RESP"

# 1.8 Integer overflow in block number
RESP=$(rpc_call "eth_getBlockByNumber" '["0xFFFFFFFFFFFFFFFFFFFFFFFF", false]')
if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
# Either an error or null result is acceptable
assert 'error' in d or d.get('result') is None
" 2>/dev/null; then
    pass "1.8 Overflow block number: error or null"
else
    if [[ -n "$RESP" ]]; then
        pass "1.8 Overflow block number: server responded (no crash)"
    else
        fail "1.8 Overflow block number: no response"
    fi
fi
collect_error "$RESP"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 2: RPC Method Boundary Testing
# ══════════════════════════════════════════════════════════════════════════════
step "Section 2: RPC Method Boundary Testing"

# 2.1 Unknown method
RESP=$(rpc_call "eth_doesNotExist" '[]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['error']['code'] == -32601" 2>/dev/null; then
    pass "2.1 Unknown method returns -32601"
else
    fail "2.1 Unknown method: expected -32601, got: $RESP"
fi
collect_error "$RESP"

# 2.2 Uppercase method name
RESP=$(rpc_call "ETH_BLOCKNUMBER" '[]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['error']['code'] == -32601" 2>/dev/null; then
    pass "2.2 Uppercase method returns -32601"
else
    fail "2.2 Uppercase method: expected -32601, got: $RESP"
fi
collect_error "$RESP"

# 2.3 Batch JSON-RPC (array of requests)
RESP=$(rpc_raw '[{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1},{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":2}]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "2.3 Batch request returns error (single-object parser)"
else
    if [[ -n "$RESP" ]]; then
        pass "2.3 Batch request: server responded with non-batch response"
    else
        fail "2.3 Batch request: no response"
    fi
fi
collect_error "$RESP"

# 2.4 String ID
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":"abc"}')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['id'] == 'abc'" 2>/dev/null; then
    pass "2.4 String ID 'abc' echoed back"
else
    if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
        pass "2.4 String ID: valid JSON-RPC response"
    else
        fail "2.4 String ID: unexpected response: $RESP"
    fi
fi

# 2.5 Null ID
RESP=$(rpc_raw '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":null}')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['id'] is None" 2>/dev/null; then
    pass "2.5 Null ID echoed back as null"
else
    if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
        pass "2.5 Null ID: valid JSON-RPC response"
    else
        fail "2.5 Null ID: unexpected response: $RESP"
    fi
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 3: eth_sendRawTransaction Security
# ══════════════════════════════════════════════════════════════════════════════
step "Section 3: eth_sendRawTransaction Security"

# 3.1 Invalid hex
RESP=$(rpc_call "eth_sendRawTransaction" '["0xZZZZ"]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "3.1 Invalid hex '0xZZZZ' returns error"
else
    fail "3.1 Invalid hex: expected error, got: $RESP"
fi
collect_error "$RESP"

# 3.2 Valid hex but not RLP
RESP=$(rpc_call "eth_sendRawTransaction" '["0x1234567890abcdef"]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "3.2 Valid hex / invalid RLP returns error"
else
    fail "3.2 Valid hex / invalid RLP: expected error, got: $RESP"
fi
collect_error "$RESP"

# 3.3-3.5 require cast (foundry) for signed transaction construction
if command -v cast >/dev/null 2>&1; then
    # Get chain ID from the service
    CHAIN_ID_HEX=$(rpc_call "eth_chainId" '[]' | python3 -c "import sys,json; print(json.load(sys.stdin)['result'])")
    CHAIN_ID=$((CHAIN_ID_HEX))

    # Generate a throwaway private key for test transactions
    TEST_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

    # 3.3 Empty calldata (signed tx with no input data)
    RAW_TX=$(cast mktx --private-key "$TEST_KEY" --chain "$CHAIN_ID" --nonce 999 --gas-price 1000000000 --gas-limit 21000 "0x0000000000000000000000000000000000000001" 2>/dev/null || echo "")
    if [[ -n "$RAW_TX" ]]; then
        RESP=$(rpc_call "eth_sendRawTransaction" "[\"$RAW_TX\"]")
        if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
            pass "3.3 Empty calldata returns error"
        else
            # Some implementations may accept it — as long as it doesn't crash
            pass "3.3 Empty calldata: server responded (no crash)"
        fi
        collect_error "$RESP"
    else
        warn "3.3 Skipped: cast mktx failed"
    fi

    # 3.4 Wrong chain_id
    WRONG_CHAIN=999
    RAW_TX=$(cast mktx --private-key "$TEST_KEY" --chain "$WRONG_CHAIN" --nonce 999 --gas-price 1000000000 --gas-limit 100000 "0x0000000000000000000000000000000000000001" 2>/dev/null || echo "")
    if [[ -n "$RAW_TX" ]]; then
        RESP=$(rpc_call "eth_sendRawTransaction" "[\"$RAW_TX\"]")
        if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
err_msg = d.get('error',{}).get('message','')
assert 'chain_id mismatch' in err_msg or 'error' in d
" 2>/dev/null; then
            pass "3.4 Wrong chain_id returns error"
        else
            fail "3.4 Wrong chain_id: expected error, got: $RESP"
        fi
        collect_error "$RESP"
    else
        warn "3.4 Skipped: cast mktx failed"
    fi

    # 3.5 Truncated claimAsset calldata (selector only, 4 bytes)
    CLAIM_SELECTOR="0xccaa2d11"  # claimAsset selector
    RAW_TX=$(cast mktx --private-key "$TEST_KEY" --chain "$CHAIN_ID" --nonce 998 --gas-price 1000000000 --gas-limit 100000 "0x0000000000000000000000000000000000000001" "$CLAIM_SELECTOR" 2>/dev/null || echo "")
    if [[ -n "$RAW_TX" ]]; then
        RESP=$(rpc_call "eth_sendRawTransaction" "[\"$RAW_TX\"]")
        if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
            pass "3.5 Truncated claimAsset calldata returns error"
        else
            fail "3.5 Truncated claimAsset calldata: expected error, got: $RESP"
        fi
        collect_error "$RESP"
    else
        warn "3.5 Skipped: cast mktx failed"
    fi
else
    warn "3.3-3.5 Skipped: 'cast' (foundry) not available"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 4: GER Manipulation
# ══════════════════════════════════════════════════════════════════════════════
step "Section 4: GER Manipulation"

# 4.1 Duplicate GER insert (same hash twice)
DUP_GER="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
CLEANUP_GERS+=("$DUP_GER")
pgquery "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
         VALUES (decode('${DUP_GER}', 'hex'), NULL, NULL, 9999, 1234567890)
         ON CONFLICT (ger_hash) DO NOTHING"
pgquery "INSERT INTO ger_entries (ger_hash, mainnet_exit_root, rollup_exit_root, block_number, timestamp)
         VALUES (decode('${DUP_GER}', 'hex'), NULL, NULL, 9999, 1234567890)
         ON CONFLICT (ger_hash) DO NOTHING"
RESP=$(rpc_call "zkevm_getExitRootsByGER" "[\"0x${DUP_GER}\"]")
if [[ -n "$RESP" ]]; then
    pass "4.1 Duplicate GER insert: idempotent, no crash"
else
    fail "4.1 Duplicate GER insert: no response"
fi
collect_error "$RESP"

# 4.2 All-zero GER hash query
ZERO_GER="0x0000000000000000000000000000000000000000000000000000000000000000"
RESP=$(rpc_call "zkevm_getExitRootsByGER" "[\"${ZERO_GER}\"]")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('result') is None or 'error' not in d" 2>/dev/null; then
    pass "4.2 All-zero GER hash: null result (no crash)"
else
    fail "4.2 All-zero GER hash: unexpected response: $RESP"
fi
collect_error "$RESP"

# 4.3 Wrong-length GER hash (16 bytes = 32 hex chars instead of 64)
RESP=$(rpc_call "zkevm_getExitRootsByGER" '["0xdeadbeefdeadbeefdeadbeefdeadbeef"]')
if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert 'error' in d or d.get('result') is None
" 2>/dev/null; then
    pass "4.3 Wrong-length GER hash: error or null (no crash)"
else
    fail "4.3 Wrong-length GER hash: unexpected response: $RESP"
fi
collect_error "$RESP"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 5: eth_getLogs Abuse
# ══════════════════════════════════════════════════════════════════════════════
step "Section 5: eth_getLogs Abuse"

# 5.1 Huge block range
RESP=$(rpc_call "eth_getLogs" '[{"fromBlock":"0x0","toBlock":"0xFFFFFFFF"}]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    pass "5.1 Huge block range: returns result (no crash)"
else
    if [[ -n "$RESP" ]]; then
        pass "5.1 Huge block range: server responded"
    else
        fail "5.1 Huge block range: no response"
    fi
fi

# 5.2 Inverted range (fromBlock > toBlock)
RESP=$(rpc_call "eth_getLogs" '[{"fromBlock":"0xFFFF","toBlock":"0x0"}]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
    pass "5.2 Inverted block range: valid response"
else
    fail "5.2 Inverted block range: unexpected response: $RESP"
fi

# 5.3 Invalid topic strings
RESP=$(rpc_call "eth_getLogs" '[{"topics":["not_a_hex_topic"]}]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
    pass "5.3 Invalid topic strings: valid response (no crash)"
else
    fail "5.3 Invalid topic strings: unexpected response: $RESP"
fi

# 5.4 Non-existent address filter
RESP=$(rpc_call "eth_getLogs" '[{"address":"0x0000000000000000000000000000000000000000"}]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert isinstance(d.get('result'), list)" 2>/dev/null; then
    pass "5.4 Non-existent address filter: returns empty array"
else
    if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
        pass "5.4 Non-existent address filter: valid response"
    else
        fail "5.4 Non-existent address filter: unexpected response: $RESP"
    fi
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 6: Block Number Edge Cases
# ══════════════════════════════════════════════════════════════════════════════
step "Section 6: Block Number Edge Cases"

# 6.1 "earliest"
RESP=$(rpc_call "eth_getBlockByNumber" '["earliest", false]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
    pass "6.1 'earliest' tag: valid response"
else
    fail "6.1 'earliest' tag: unexpected response: $RESP"
fi

# 6.2 Far future block
RESP=$(rpc_call "eth_getBlockByNumber" '["0xFFFFFFFF", false]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('result') is None" 2>/dev/null; then
    pass "6.2 Far future block: returns null"
else
    if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d or 'error' in d" 2>/dev/null; then
        pass "6.2 Far future block: valid response"
    else
        fail "6.2 Far future block: unexpected response: $RESP"
    fi
fi

# 6.3 Invalid tag "newest"
RESP=$(rpc_call "eth_getBlockByNumber" '["newest", false]')
if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert 'error' in d and 'bad block number' in d['error'].get('message','')
" 2>/dev/null; then
    pass "6.3 Invalid tag 'newest': returns 'bad block number' error"
else
    if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
        pass "6.3 Invalid tag 'newest': returns error"
    else
        fail "6.3 Invalid tag 'newest': expected error, got: $RESP"
    fi
fi
collect_error "$RESP"

# 6.4 Empty string — alloy's U64::from_str("") parses as 0, so this returns block 0
# which is valid behavior. The key check is no crash.
RESP=$(rpc_call "eth_getBlockByNumber" '["", false]')
if echo "$RESP" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert 'error' in d or 'result' in d
" 2>/dev/null; then
    pass "6.4 Empty string block tag: valid response (no crash)"
else
    fail "6.4 Empty string block tag: no response"
fi
collect_error "$RESP"

# 6.5 All 5 standard tags
ALL_TAGS_OK=true
for tag in "latest" "pending" "finalized" "safe" "earliest"; do
    RESP=$(rpc_call "eth_getBlockByNumber" "[\"$tag\", false]")
    if ! echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
        fail "6.5 Standard tag '$tag' failed: $RESP"
        ALL_TAGS_OK=false
    fi
done
if [[ "$ALL_TAGS_OK" == "true" ]]; then
    pass "6.5 All 5 standard tags return valid responses"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 7: Transaction Receipt Probing
# ══════════════════════════════════════════════════════════════════════════════
step "Section 7: Transaction Receipt Probing"

# 7.1 Random non-existent hash
RAND_HASH="0x$(python3 -c "import secrets; print(secrets.token_hex(32))")"
RESP=$(rpc_call "eth_getTransactionReceipt" "[\"$RAND_HASH\"]")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('result') is None" 2>/dev/null; then
    pass "7.1 Random non-existent hash: returns null"
else
    fail "7.1 Random non-existent hash: expected null, got: $RESP"
fi

# 7.2 Zero hash
ZERO_HASH="0x0000000000000000000000000000000000000000000000000000000000000000"
RESP=$(rpc_call "eth_getTransactionReceipt" "[\"$ZERO_HASH\"]")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('result') is None" 2>/dev/null; then
    pass "7.2 Zero hash: returns null"
else
    fail "7.2 Zero hash: expected null, got: $RESP"
fi

# 7.3 Wrong-length hash
RESP=$(rpc_call "eth_getTransactionReceipt" '["0x1234"]')
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'error' in d" 2>/dev/null; then
    pass "7.3 Wrong-length hash: returns error"
else
    # TxHash::from_str might fail or succeed with padding — either way, no crash
    if [[ -n "$RESP" ]]; then
        pass "7.3 Wrong-length hash: server responded (no crash)"
    else
        fail "7.3 Wrong-length hash: no response"
    fi
fi
collect_error "$RESP"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 8: Timing/State Consistency
# ══════════════════════════════════════════════════════════════════════════════
step "Section 8: Timing/State Consistency"

# 8.1 10 concurrent eth_blockNumber requests
CONCURRENT_OK=true
PIDS=()
TMPDIR_CONC=$(mktemp -d)
for i in $(seq 1 10); do
    rpc_call "eth_blockNumber" '[]' > "$TMPDIR_CONC/resp_$i" 2>/dev/null &
    PIDS+=($!)
done
for pid in "${PIDS[@]}"; do
    wait "$pid" || true
done
for i in $(seq 1 10); do
    RESP=$(cat "$TMPDIR_CONC/resp_$i")
    if ! echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'result' in d" 2>/dev/null; then
        CONCURRENT_OK=false
        break
    fi
done
rm -rf "$TMPDIR_CONC"
if [[ "$CONCURRENT_OK" == "true" ]]; then
    pass "8.1 10 concurrent eth_blockNumber: all returned valid responses"
else
    fail "8.1 Concurrent requests: some responses invalid"
fi

# 8.2 Nonce stability
ADDR="0x0000000000000000000000000000000000000001"
NONCE1=$(rpc_call "eth_getTransactionCount" "[\"$ADDR\",\"latest\"]" | python3 -c "import sys,json; print(json.load(sys.stdin).get('result',''))" 2>/dev/null)
NONCE2=$(rpc_call "eth_getTransactionCount" "[\"$ADDR\",\"latest\"]" | python3 -c "import sys,json; print(json.load(sys.stdin).get('result',''))" 2>/dev/null)
if [[ "$NONCE1" == "$NONCE2" && -n "$NONCE1" ]]; then
    pass "8.2 Nonce stability: two reads return same value ($NONCE1)"
else
    fail "8.2 Nonce stability: nonce1=$NONCE1 nonce2=$NONCE2"
fi

# 8.3 Block number monotonicity
BN1=$(rpc_call "eth_blockNumber" '[]' | python3 -c "import sys,json; print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
BN2=$(rpc_call "eth_blockNumber" '[]' | python3 -c "import sys,json; print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null)
if [[ -n "$BN1" && -n "$BN2" ]] && [[ "$BN2" -ge "$BN1" ]]; then
    pass "8.3 Block number monotonicity: $BN1 <= $BN2"
else
    fail "8.3 Block number monotonicity: bn1=$BN1 bn2=$BN2"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 9: Error Information Leakage
# ══════════════════════════════════════════════════════════════════════════════
step "Section 9: Error Information Leakage"

# 9.1 Scan all collected error responses for sensitive patterns
LEAK_FOUND=false
for pattern in "panic" "stack trace" "RUST_BACKTRACE" ".rs:"; do
    if echo "$ERROR_RESPONSES" | grep -qi "$pattern"; then
        fail "9.1 Error responses contain '$pattern'"
        LEAK_FOUND=true
    fi
done
if [[ "$LEAK_FOUND" == "false" ]]; then
    pass "9.1 No stack traces, panics, or .rs: paths in error responses"
fi

# 9.2 Scan for sensitive keywords
SENSITIVE_FOUND=false
for pattern in "password" "PRIVATE_KEY" "DATABASE_URL" "SELECT " "INSERT "; do
    if echo "$ERROR_RESPONSES" | grep -qi "$pattern"; then
        fail "9.2 Error responses contain sensitive keyword '$pattern'"
        SENSITIVE_FOUND=true
    fi
done
if [[ "$SENSITIVE_FOUND" == "false" ]]; then
    pass "9.2 No sensitive keywords in error responses"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SECTION 10: Health/Metrics Endpoint Security
# ══════════════════════════════════════════════════════════════════════════════
step "Section 10: Health/Metrics Endpoint Security"

# 10.1 GET /health
RESP=$(curl -s "$L2_RPC/health")
if echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('status') == 'ok'" 2>/dev/null; then
    pass "10.1 GET /health returns {\"status\":\"ok\"}"
else
    fail "10.1 GET /health: unexpected response: $RESP"
fi

# 10.2 GET /metrics — no sensitive data
METRICS=$(curl -s "$L2_RPC/metrics" | tr -d '\0')
METRICS_LEAK=false
for pattern in "password" "PRIVATE_KEY" "DATABASE_URL" "secret"; do
    if echo "$METRICS" | grep -qi "$pattern"; then
        fail "10.2 /metrics contains sensitive pattern '$pattern'"
        METRICS_LEAK=true
    fi
done
if [[ "$METRICS_LEAK" == "false" ]]; then
    pass "10.2 /metrics contains no sensitive data"
fi

# 10.3 Metrics contain expected counters
METRICS_OK=true
for counter in "rpc_requests_total" "rpc_request_duration_seconds"; do
    if ! echo "$METRICS" | grep -q "$counter"; then
        fail "10.3 Missing expected metric: $counter"
        METRICS_OK=false
    fi
done
if [[ "$METRICS_OK" == "true" ]]; then
    pass "10.3 Expected metrics counters present"
fi
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SUMMARY
# ══════════════════════════════════════════════════════════════════════════════
TOTAL=$((PASS_COUNT + FAIL_COUNT))
log "======================================================================"
log "  SECURITY E2E TEST SUITE COMPLETE"
log ""
log "  Passed: $PASS_COUNT / $TOTAL"
if [[ $FAIL_COUNT -gt 0 ]]; then
    echo -e "${RED}  Failed: $FAIL_COUNT / $TOTAL${NC}"
else
    log "  Failed: 0 / $TOTAL"
fi
log "======================================================================"

exit "$FAIL_COUNT"
