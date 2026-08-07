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
# The signer container must run as the owner of fixtures/web3signer-keys (0700),
# or it silently starts with zero keys loaded. See docker-compose.web3signer.yml.
WEB3SIGNER_UID="$(id -u)"; WEB3SIGNER_GID="$(id -g)"
export WEB3SIGNER_UID WEB3SIGNER_GID
COMPOSE=(docker compose -f docker-compose.e2e.yml -f docker-compose.web3signer.yml --env-file fixtures/.env)
PROXY="${PROJECT}-miden-agglayer-1"
SIGNER="${PROJECT}-web3signer-1"
SIGNER_URL="${SIGNER_URL:-http://127.0.0.1:9000}"
SIGNER_METRICS="${SIGNER_METRICS:-http://127.0.0.1:9001}"

GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'; NC='\033[0m'
ts() { date '+%H:%M:%S'; }
log()  { echo -e "${CYAN}[$(ts)]${NC} $*"; }
pass() { echo -e "${GREEN}[$(ts)] PASS:${NC} $*"; }
fail() { echo -e "${RED}[$(ts)] FAIL:${NC} $*"; exit 1; }

# ── 0. provision the signer's key + bring the stack up ───────────────────────
log "provisioning the signer key"
./scripts/gen-web3signer-keys.sh || fail "could not provision the signer keys"
# Per-role bindings (AGGLAYER_SIGNER_KEYS) for the compose overlay.
set -a; . ./fixtures/web3signer-keys.env; set +a

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
# NOTE: no `| grep -q` here. Under `set -o pipefail`, grep -q exits on its
# FIRST match and closes the pipe, so the upstream `sed` dies of SIGPIPE (141)
# and the pipeline reports failure for a SUCCESSFUL match. Capture first, then
# match on the string.
PROXY_LOG="$(docker logs "$PROXY" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g')"
case "$PROXY_LOG" in
    *"remote signer attached"*) ;;
    *) fail "the proxy did not report attaching the remote signer — is AGGLAYER_SIGNER_URL set?" ;;
esac
# Custody is either/or: the proxy must NOT also be configured for on-disk keys.
# (The proxy refuses to start with both, so reaching a healthy state already
# implies this — asserting it anyway keeps the guarantee visible in the log.)
PROXY_ENV="$(docker inspect "$PROXY" --format '{{json .Config.Env}}' 2>/dev/null)"
case "$PROXY_ENV" in
    *AGGLAYER_INSECURE_LOCAL_KEYSTORE=true*)
        fail "the proxy has AGGLAYER_INSECURE_LOCAL_KEYSTORE=true AND a signer — custody must be either/or" ;;
esac
pass "proxy started in remote-only custody mode (fail-closed startup passed)"

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
# Web3Signer does NOT log individual signing requests (its entire log is ~15
# startup lines), so counting log lines here would report 0 signatures while
# signing worked perfectly — a guaranteed false failure. Its Prometheus counter
# is the authoritative signal; verified to advance exactly once per signature.
signer_signature_count() {
    curl -s "$SIGNER_METRICS/metrics" 2>/dev/null \
        | awk '/^signing_secp256k1_signing_duration_count/ {print $2; found=1}
               END {if (!found) print "MISSING"}'
}
SIGN_BEFORE="$(signer_signature_count)"
if [ "$SIGN_BEFORE" = MISSING ]; then
    fail "the signer exposes no signing counter at $SIGNER_METRICS — is --metrics-enabled set?"
fi

log "running a full L1->L2 deposit with every account signature served remotely"
./scripts/e2e-l1-to-l2.sh > /tmp/e2e-web3signer-l1l2.log 2>&1 \
    || { tail -25 /tmp/e2e-web3signer-l1l2.log; fail "the L1->L2 deposit failed with remote signing"; }
pass "L1->L2 deposit completed with remote signing"

# ── 4. the signer actually served signatures ─────────────────────────────────
SIGN_AFTER="$(signer_signature_count)"
if [ "$SIGN_AFTER" = MISSING ]; then
    fail "the signer stopped exposing its signing counter mid-test — did it die?"
fi
# The counter is a float ("12.0"); compare as integers.
SIGNED=$(( ${SIGN_AFTER%%.*} - ${SIGN_BEFORE%%.*} ))
[ "$SIGNED" -gt 0 ] \
    || fail "the signer served 0 signing requests during the deposit (counter ${SIGN_BEFORE} -> ${SIGN_AFTER}) — signing did not go remote"
pass "the signer served $SIGNED signing request(s) during the deposit (counter ${SIGN_BEFORE} -> ${SIGN_AFTER})"

# ── 5. NEGATIVE CONTROL: without the signer, signing must fail ───────────────
# Without this, a silent local-key fallback would make every assertion above
# pass while custody was never actually remote.
#
# The TRIGGER is the load-bearing part. The first version drove a fabricated
# GER injection, which never reaches the signing path at all: the proxy runs
# with --reject-unverified-ger, so a made-up GER is refused during verification
# long before an account signature is needed. The proxy then correctly logged
# NOTHING, and this control read "no error" as "it signed" and accused the
# product of a local-key fallback that cannot exist. Absence of an error is not
# evidence of a signature.
#
# So drive the operation step 3 just PROVED reaches the signer: a real L1->L2
# deposit, whose claim drew 4 signatures. With the signer down, the proxy must
# report a signing failure on that path.
log "negative control: stopping the signer — account signing must now fail"
# Baselines captured BEFORE the outage. Taking the success baseline after
# `docker start` races a fast recovered retry: the retry can advance the counter
# before the baseline is read, after which recovery looks stuck.
proxy_metric() {
    curl -s "http://127.0.0.1:8546/metrics" 2>/dev/null \
        | awk -v m="$1" '$1 == m {print $2; found=1} END {if (!found) print 0}'
}
FAILS_BEFORE_OUTAGE="$(proxy_metric remote_signer_signature_failures_total)"
docker stop "$SIGNER" >/dev/null 2>&1 || fail "could not stop the signer"
# Safety net only — the real restore happens after the recovery baseline. This
# exists so a failing assertion between here and there cannot leave the signer
# down for whatever runs next.
trap 'docker start "$SIGNER" >/dev/null 2>&1 || true' EXIT
NEG_START="$(date -u +%s)"

./scripts/e2e-l1-to-l2.sh > /tmp/e2e-web3signer-neg.log 2>&1 &
NEG_PID=$!

SAW_FAILURE=""
for _ in $(seq 1 90); do
    # Capture, then match: `| grep -q` under pipefail can report a successful
    # match as a failure (see the note at the top of this file).
    NEG_LOG="$(docker logs "$PROXY" --since "$NEG_START" 2>&1 \
        | sed -e 's/\x1b\[[0-9;]*m//g' | tr '[:upper:]' '[:lower:]')"
    case "$NEG_LOG" in
        *"remote signer failed"*|*"remote signer refused"*|*"remote signer does not hold"*|\
        *"error sending request"*|*"connection refused"*|*"tcp connect error"*)
            SAW_FAILURE=1; break ;;
    esac
    sleep 2
done

# Stop driving traffic. The signer is deliberately NOT restarted here: doing so
# left a window between the restore and the recovery baseline in which a pending
# retry could sign, which is exactly the false-pass this baseline exists to
# prevent. The EXIT trap set when we stopped it guarantees the stack is not left
# wedged if an assertion below fails.
kill "$NEG_PID" 2>/dev/null || true
wait "$NEG_PID" 2>/dev/null || true

# The message names BOTH possibilities rather than asserting a fallback as
# fact: a control that cannot distinguish "never tried to sign" from "signed
# locally" must not claim to have caught the latter.
[ -n "$SAW_FAILURE" ] \
    || fail "with the signer DOWN the proxy never reported a signing failure. Either the trigger did not reach the signing path (harness bug — check /tmp/e2e-web3signer-neg.log), or the proxy signed without the signer, which would mean custody is not actually remote."
pass "with the signer down, account signing fails — signatures genuinely come from the signer"

# ── 6. the failure is OBSERVABLE, and signing RECOVERS ───────────────────────
# A negative control that only proves "it broke" leaves the operator-facing half
# untested: the failure must show up in metrics (that is what pages someone) and
# the system must come back once the signer returns. A custody feature that
# fails visibly but never recovers is not usable.
FAILS_AFTER_OUTAGE="$(proxy_metric remote_signer_signature_failures_total)"
# DELTA, not "> 0": an absolute threshold can be satisfied by a failure from
# earlier in the run, which would let a completely unobserved outage pass.
[ "$(( ${FAILS_AFTER_OUTAGE%%.*} - ${FAILS_BEFORE_OUTAGE%%.*} ))" -gt 0 ] \
    || fail "the signer outage did not advance remote_signer_signature_failures_total \
(${FAILS_BEFORE_OUTAGE} -> ${FAILS_AFTER_OUTAGE}) — the outage would be invisible to monitoring"
pass "signer outage is visible in metrics (failures ${FAILS_BEFORE_OUTAGE} -> ${FAILS_AFTER_OUTAGE})"

# Success baseline taken HERE — after the outage is observed and before the
# signer is restored. Taking it before `docker stop` leaves a window in which a
# signature can land, which would make the recovery check pass instantly even if
# signing never resumed (PR #162 review).
SIGS_BEFORE_RECOVERY="$(proxy_metric remote_signer_signatures_total)"
docker start "$SIGNER" >/dev/null 2>&1 || fail "could not restart the signer"
trap - EXIT
log "asserting signing RESUMES now that the signer is back"
RECOVERED=""
for _ in $(seq 1 45); do
    NOW="$(proxy_metric remote_signer_signatures_total)"
    if [ "${NOW%%.*}" -gt "${SIGS_BEFORE_RECOVERY%%.*}" ]; then RECOVERED=1; break; fi
    sleep 4
done
[ -n "$RECOVERED" ] \
    || fail "signing did not resume after the signer came back (successes stuck at \
${SIGS_BEFORE_RECOVERY}) — the outage is unrecoverable without a restart"
pass "signing resumed after the signer returned — the outage is self-healing"

# ── 7. PROXY RESTART on persisted state — the exact lifecycle that was broken ──
# The defect this suite exists to catch was NOT signer loss: it was that the
# account↔key verification ran only in the temporary phase-1 init client and was
# absent from the serving process and from every subsequent restart. Restarting
# the SIGNER never exercised that. So restart the PROXY against its persisted
# account/node state, require it to re-verify BOTH role bindings on the way up,
# and then drive a real signed operation to prove it can still sign.
log "restarting the PROXY on persisted state — bindings must re-verify on the way up"
RESTART_TS="$(date -u +%s)"
docker restart "$PROXY" >/dev/null 2>&1 || fail "could not restart the proxy"
for _ in $(seq 1 90); do
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] && break
    sleep 2
done
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] \
    || { docker logs --since "$RESTART_TS" "$PROXY" 2>&1 | tail -20
         fail "the proxy did not come back healthy after a restart on persisted state"; }

# Assert the METRIC, not log text. Field rendering (`accounts: 2` vs
# `accounts=2`) depends on the tracing formatter, so grepping it asserts on a
# formatting detail and can false-fail while the property holds (PR #162 review).
VERIFIED="$(proxy_metric remote_signer_verified_accounts)"
[ "${VERIFIED%%.*}" -ge 2 ] \
    || fail "after restart the proxy verified ${VERIFIED} account binding(s), expected 2 — \
either verification did not re-run (the exact init-only lifecycle hole this closes) or it \
covered only one role"
pass "proxy restart re-verified both role bindings against the deployed accounts (gauge=${VERIFIED})"

SIGS_BEFORE_RESTART_OP="$(proxy_metric remote_signer_signatures_total)"
log "driving a signed operation after the restart"
./scripts/e2e-l1-to-l2.sh > /tmp/e2e-web3signer-postrestart.log 2>&1 \
    || { tail -20 /tmp/e2e-web3signer-postrestart.log
         fail "the bridge could not complete a deposit after the proxy restart"; }
SIGS_AFTER_RESTART_OP="$(proxy_metric remote_signer_signatures_total)"
[ "$(( ${SIGS_AFTER_RESTART_OP%%.*} - ${SIGS_BEFORE_RESTART_OP%%.*} ))" -gt 0 ] \
    || fail "the post-restart deposit produced no remote signatures \
(${SIGS_BEFORE_RESTART_OP} -> ${SIGS_AFTER_RESTART_OP}) — signing did not survive the restart"
pass "post-restart deposit signed remotely (signatures ${SIGS_BEFORE_RESTART_OP} -> ${SIGS_AFTER_RESTART_OP})"

# ── 8. DEPLOYED-vs-CONFIGURED mismatch must refuse to serve ──────────────────
# The accounts on chain are signed for by the key --signer-key names. Config is
# cheap to change; a deployed account's auth key is not. If they ever disagree,
# the proxy holds a configuration claiming authority over an account it
# fundamentally CANNOT sign for — and without this check that looks perfectly
# healthy at boot and fails at the first claim or GER injection.
#
# Real ways to reach it: a store restored against the wrong signer, a key rotated
# in the KMS after deployment, an edited --signer-key, or a config copied between
# environments. Reproduce it honestly: give the signer a SPARE key it genuinely
# holds, then point the service role at that spare while the deployed account
# still uses the original. Nothing is malformed — only the binding is wrong,
# which is exactly why only comparing the DEPLOYED commitment can catch it.
log "negative: pointing a role at a key the deployed account does NOT use"
SPARE_WALLET="$(cast wallet new)"
SPARE_PRIV="$(echo "$SPARE_WALLET" | awk '/Private key:/ {print $3}' | sed 's/^0x//')"
SPARE_ADDR="$(echo "$SPARE_WALLET" | awk '/Address:/ {print $2}')"
[ -n "$SPARE_PRIV" ] || fail "could not generate a spare key"
# `spare-` is not a role name, so gen-web3signer-keys.sh ignores it for binding
# while Web3Signer still loads and serves it.
cat > "fixtures/web3signer-keys/spare-${SPARE_ADDR}.yaml" <<SPAREEOF
type: file-raw
keyType: SECP256K1
privateKey: "0x${SPARE_PRIV}"
SPAREEOF
chmod 600 "fixtures/web3signer-keys/spare-${SPARE_ADDR}.yaml"
cleanup_spare() { rm -f "fixtures/web3signer-keys/spare-${SPARE_ADDR}.yaml"; }
trap cleanup_spare EXIT

"${COMPOSE[@]}" restart web3signer >/dev/null 2>&1 || fail "could not reload the signer"
for _ in $(seq 1 60); do
    curl -sf "$SIGNER_URL/api/v1/eth1/publicKeys" 2>/dev/null | grep -q '0x' && break
    sleep 2
done
SPARE_PUB="$(cast wallet public-key --private-key "0x${SPARE_PRIV}" 2>/dev/null)"
[ -n "$SPARE_PUB" ] || fail "could not derive the spare public key"

GER_BINDING="$(printf '%s' "$AGGLAYER_SIGNER_KEYS" | tr ',' '\n' | grep '^ger-manager=')"
MISMATCH_TS="$(date -u +%s)"
AGGLAYER_SIGNER_KEYS="service=${SPARE_PUB},${GER_BINDING}" \
    "${COMPOSE[@]}" up -d --force-recreate --no-deps miden-agglayer >/dev/null 2>&1 || true

# It must NOT become healthy: serving here would mean accepting an account it
# cannot sign for.
BECAME_HEALTHY=""
for _ in $(seq 1 30); do
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] \
        && { BECAME_HEALTHY=1; break; }
    sleep 2
done
MISMATCH_LOG="$(docker logs "$PROXY" --since "$MISMATCH_TS" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g')"

# Restore the correct binding first, so a failed assertion cannot leave the
# stack wedged for whatever runs next.
"${COMPOSE[@]}" up -d --force-recreate --no-deps miden-agglayer >/dev/null 2>&1 || true

[ -z "$BECAME_HEALTHY" ] \
    || fail "the proxy served normally while a role was bound to a key the DEPLOYED account does \
not use — it cannot sign for that account, so this would fail at the first claim instead of at \
startup"
case "$MISMATCH_LOG" in
    *"signed for by a DIFFERENT key"*) ;;
    *) fail "the proxy refused to start but not for the deployed-vs-configured mismatch; the \
operator would have no idea which account or key is wrong. Log tail: $(printf '%s' \
"$MISMATCH_LOG" | tail -5)" ;;
esac
pass "a role bound to the wrong key is refused at STARTUP, naming the account and the cause"

for _ in $(seq 1 90); do
    [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] && break
    sleep 2
done
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] \
    || fail "the proxy did not recover after the correct binding was restored"
cleanup_spare
trap - EXIT
pass "restoring the correct binding brings the proxy back — the refusal is not sticky"

echo ""
pass "WEB3SIGNER E2E COMPLETE — remote custody proven: no local secret, full deposit remote-signed, and signing provably depends on the signer"
