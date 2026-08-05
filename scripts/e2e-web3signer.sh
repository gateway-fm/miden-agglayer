#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-web3signer.sh — the bridge works end-to-end with account keys held by a
# REMOTE signer, and the proxy holds no secret for them.
#
# This is the test that makes KMS custody credible: not "the HTTP client
# compiles" but "a real deposit completes while every account signature is
# produced by a separate process that owns the key". It asserts, in order:
#
#   1. the signer holds a key and the proxy adopted it (startup fail-closed);
#   2. the proxy's own keystore has NO secret for the account key it uses —
#      i.e. signing genuinely left this host;
#   3. a full L1->L2 deposit completes (every account signature remote-signed);
#   4. the signer actually served signatures during that deposit;
#   5. removing the signer breaks signing — proving step 3 was not silently
#      falling back to a local key.
#
# Step 5 is the one that turns this from a smoke test into evidence.
#
# Usage: ./scripts/e2e-web3signer.sh      (brings up its own stack)
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
PROJECT="${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}"
export COMPOSE_PROJECT_NAME="$PROJECT"
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-$(grep -m1 '^MIDEN_NODE_GIT_URL' Makefile | sed 's/.*= *//')}"
MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-$(grep -m1 '^MIDEN_NODE_GIT_REF' Makefile | sed 's/.*= *//')}"
export MIDEN_NODE_GIT_URL MIDEN_NODE_GIT_REF
COMPOSE=(docker compose -f docker-compose.e2e.yml -f docker-compose.web3signer.yml --env-file fixtures/.env)
PROXY="${PROJECT}-miden-agglayer-1"
SIGNER="${PROJECT}-web3signer-1"
SIGNER_URL="${SIGNER_URL:-http://127.0.0.1:9000}"

GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'; NC='\033[0m'
ts() { date '+%H:%M:%S'; }
log()  { echo -e "${CYAN}[$(ts)]${NC} $*"; }
pass() { echo -e "${GREEN}[$(ts)] PASS:${NC} $*"; }
fail() { echo -e "${RED}[$(ts)] FAIL:${NC} $*"; exit 1; }

# ── 0. provision the signer's key + bring the stack up ───────────────────────
log "provisioning the signer key"
./scripts/gen-web3signer-keys.sh || fail "could not provision the signer key"

log "bringing up the stack with the web3signer overlay"
"${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1
make e2e-clean-data >/dev/null 2>&1 || fail "e2e-clean-data"
"${COMPOSE[@]}" up -d >/tmp/e2e-web3signer-up.log 2>&1 || {
    tail -20 /tmp/e2e-web3signer-up.log; fail "stack bring-up (see /tmp/e2e-web3signer-up.log)"
}

# ── 1. the signer holds a key and the proxy adopted it ───────────────────────
log "waiting for the signer to serve its key list"
KEYS=""
for _ in $(seq 1 60); do
    KEYS="$(curl -sf "$SIGNER_URL/api/v1/eth1/publicKeys" 2>/dev/null || true)"
    [[ "$KEYS" == \[\"0x* ]] && break
    sleep 2
done
[[ "$KEYS" == \[\"0x* ]] || fail "the signer never served a public key (got: ${KEYS:-<nothing>})"
SIGNER_KEY="$(echo "$KEYS" | sed 's/\[\"//;s/\".*//')"
pass "signer holds key $SIGNER_KEY"

log "waiting for the proxy to become healthy with remote signing enabled"
for _ in $(seq 1 90); do
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] && break
    sleep 2
done
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] \
    || fail "the proxy never became healthy with --signer-url set (a fail-closed startup error?)"
docker logs "$PROXY" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g' | grep -q "remote signer attached" \
    || fail "the proxy did not report attaching the remote signer — is AGGLAYER_SIGNER_URL set?"
pass "proxy started with the remote signer attached (fail-closed startup passed)"

# ── 2. the proxy holds NO secret for the remote key ──────────────────────────
# The whole point of vault custody: the account's signing key must not exist on
# this host. The proxy's keystore directory must contain no key file whose
# commitment matches what the accounts use.
log "asserting the proxy stores no secret for the signer's key"
LOCAL_KEYS="$(docker exec "$PROXY" sh -c 'ls -1 /var/lib/miden-agglayer-service/keystore 2>/dev/null | wc -l' 2>/dev/null || echo unknown)"
if [ "$LOCAL_KEYS" = unknown ]; then
    # distroless image without a shell: fall back to the host bind mount
    LOCAL_KEYS="$(ls -1 .miden-agglayer-data/keystore 2>/dev/null | wc -l)"
fi
[ "${LOCAL_KEYS:-1}" -eq 0 ] \
    || fail "the proxy keystore holds $LOCAL_KEYS key file(s); with a remote signer it must hold none"
pass "the proxy holds no local secret — account keys live only in the signer"

# ── 3. a full deposit completes, remote-signed throughout ────────────────────
SIGN_BEFORE="$(docker logs "$SIGNER" 2>&1 | grep -c 'eth1/sign' || true)"
log "running a full L1->L2 deposit with every account signature served remotely"
./scripts/e2e-l1-to-l2.sh > /tmp/e2e-web3signer-l1l2.log 2>&1 \
    || { tail -25 /tmp/e2e-web3signer-l1l2.log; fail "the L1->L2 deposit failed with remote signing"; }
pass "L1->L2 deposit completed with remote signing"

# ── 4. the signer actually served signatures ─────────────────────────────────
SIGN_AFTER="$(docker logs "$SIGNER" 2>&1 | grep -c 'eth1/sign' || true)"
SIGNED=$(( SIGN_AFTER - SIGN_BEFORE ))
[ "$SIGNED" -gt 0 ] \
    || fail "the signer served 0 signing requests during the deposit — signing did not go remote"
pass "the signer served $SIGNED signing request(s) during the deposit"

# ── 5. NEGATIVE CONTROL: without the signer, signing must fail ───────────────
# Without this, a silent local-key fallback would make every assertion above
# pass while custody was never actually remote.
log "negative control: stopping the signer — account signing must now fail"
docker stop "$SIGNER" >/dev/null 2>&1 || fail "could not stop the signer"
NEG_START="$(date -u +%s)"
# Drive a GER injection (a proxy-signed account operation) and require an error.
cast send --async --rpc-url "${L2_RPC:-http://127.0.0.1:8546}" \
    --private-key "${GER_KEY:-0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6}" \
    --legacy --gas-price 1 --gas-limit 1000000 \
    "${BRIDGE_ADDR:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}" \
    'insertGlobalExitRoot(bytes32)' \
    "0x$(printf '%064x' $(( 0xDEAD0000 + RANDOM )))" >/dev/null 2>&1 || true
SAW_FAILURE=""
for _ in $(seq 1 30); do
    if docker logs "$PROXY" --since "$NEG_START" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g' \
        | grep -qiE 'remote signer (failed|refused)|error sending request'; then
        SAW_FAILURE=1; break
    fi
    sleep 2
done
docker start "$SIGNER" >/dev/null 2>&1 || true
[ -n "$SAW_FAILURE" ] \
    || fail "with the signer DOWN the proxy still signed — it is falling back to a local key, so custody is not actually remote"
pass "with the signer down, account signing fails — signatures genuinely come from the signer"

echo ""
pass "WEB3SIGNER E2E COMPLETE — remote custody proven: no local secret, full deposit remote-signed, and signing provably depends on the signer"
