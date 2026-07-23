#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# #157 IN-PLACE UPGRADE recovery test: RELEASE v0.15.9 → THIS BRANCH (main+#157).
#
# Companion to scripts/e2e-upgrade-test.sh (which proves no-data-loss / getLogs
# immutability / liveness across the swap). This one proves the #157-specific risk:
# the new recovery loop runs on startup against DURABLE STATE the OLD (v0.15.9)
# binary wrote, and heals it EXACTLY ONCE.
#
#   R  bring the stack up ON THE RELEASE (v0.15.9 image + release command line, via
#      scripts/upgrade/docker-compose.upgrade-release.yml), same store/volumes.
#   • baseline: a deposit completes normally (terminal state to preserve).
#   • orphan:   a REAL deposit's claim is captured while durably-admitted + unlinked,
#      then the release proxy is SIGKILLed — an orphan v0.15.9 CANNOT self-heal (no
#      recovery loop). bridge-service (ClaimTxManager) is stopped so no rebroadcast.
#   U1 swap ONLY the proxy to the branch image (same volumes). On startup the branch
#      applies the additive migrations (019 + 021) and its recovery loop sweeps.
#   • assert: migrations applied; the pre-upgrade orphan self-heals to the SAME hash
#      exactly once (nonce not re-advanced, credited exactly once via the deposit
#      wrapper's own exact-balance oracle); the baseline terminal state is preserved;
#      a fresh post-upgrade deposit works.
#
# Requires images `miden-agglayer-e2e:v0.15.9` (built here from the tag if absent)
# and `miden-agglayer-e2e:latest` (the branch build). Needs 8546/9545/18080 free.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
WT="$PWD"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
export COMPOSE_PROJECT_NAME="$PROJECT"
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-$(grep -m1 '^MIDEN_NODE_GIT_URL' Makefile | sed 's/.*= *//')}"
MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-$(grep -m1 '^MIDEN_NODE_GIT_REF' Makefile | sed 's/.*= *//')}"
export MIDEN_NODE_GIT_URL MIDEN_NODE_GIT_REF
BASE=(docker compose -f docker-compose.e2e.yml -f docker-compose.l2l2.yml --env-file fixtures/.env)
REL=("${BASE[@]}" -f scripts/upgrade/docker-compose.upgrade-release.yml)
PROXY="${PROJECT}-miden-agglayer-1"
PG="${PROJECT}-agglayer-postgres-1"
BRIDGE_SERVICE="${PROJECT}-bridge-service-1"
L2_RPC="${L2_RPC:-http://localhost:8546}"
CLAIM_SELECTOR="ccaa2d11"
REL_REF="${UPGRADE_FROM_REF:-v0.15.9}"
REL_IMG="miden-agglayer-e2e:${REL_REF}"

ts()   { date '+%H:%M:%S'; }
log()  { echo "[$(ts)] [upg] $*"; }
pass() { echo "[$(ts)] [upg] PASS: $*"; }
fail() { echo "[$(ts)] [upg] FAIL: $*"; exit 1; }
pgq()  { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }
receipt_status() { cast receipt --rpc-url "$L2_RPC" "$1" status 2>/dev/null | awk '{print $1; exit}'; }
container_running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ]; }
proxy_ready() { for _ in $(seq 1 "${1:-60}"); do cast chain-id --rpc-url "$L2_RPC" >/dev/null 2>&1 && return 0; sleep 3; done; return 1; }

# ── Build both images ──────────────────────────────────────────────────────────
log "ensuring branch image miden-agglayer-e2e:latest (this branch = main+#157)"
"${BASE[@]}" build miden-agglayer >/dev/null 2>&1 || fail "branch image build failed"

if ! docker image inspect "$REL_IMG" >/dev/null 2>&1; then
    log "building RELEASE image $REL_IMG from tag $REL_REF (clean worktree)"
    git fetch origin --tags -q 2>/dev/null || true
    git rev-parse -q --verify "refs/tags/${REL_REF}^{commit}" >/dev/null 2>&1 || fail "release tag $REL_REF not found"
    REL_WT="$(mktemp -d /tmp/upg-rel.XXXXXX)"
    git worktree add --detach "$REL_WT" "$REL_REF" >/dev/null 2>&1 || fail "git worktree add $REL_REF failed"
    trap 'git worktree remove --force "$REL_WT" >/dev/null 2>&1 || rm -rf "$REL_WT"' EXIT
    docker build -t "$REL_IMG" "$REL_WT" >/dev/null 2>&1 || fail "release image build from $REL_REF failed"
fi
log "release=$REL_REF  branch=$(git rev-parse --short HEAD)"

# ── phase R: fresh deployment ON THE RELEASE ─────────────────────────────────────
log "teardown + fresh RELEASE bringup ($REL_IMG proxy, release command line)"
"${BASE[@]}" down -v --remove-orphans >/dev/null 2>&1
left=$(docker ps -aq --filter "name=$PROJECT"); [ -n "$left" ] && docker rm -f $left >/dev/null 2>&1
make e2e-clean-data gen-l2b-configs >/dev/null 2>&1 || fail "clean-data/gen-l2b-configs"
"${REL[@]}" up -d >/tmp/upg-relup.log 2>&1 || fail "release bringup"
until cast chain-id --rpc-url http://localhost:9545 >/dev/null 2>&1; do sleep 2; done
L2B_RPC=http://localhost:9545 ./scripts/setup-l2b.sh >/tmp/upg-setup-l2b.log 2>&1 || fail "setup-l2b"
"${REL[@]}" up -d --force-recreate --wait aggkit-l2b bridge-service-l2b >/dev/null 2>&1 || fail "l2b services"
proxy_ready 100 || fail "release proxy never became ready"
docker inspect "$PROXY" --format '{{.Config.Image}}' | grep -q "$REL_REF" || fail "proxy is not the release image ($REL_REF)"
[ -z "$(pgq "SELECT 1 FROM information_schema.columns WHERE table_name='transactions' AND column_name='recovery_attempts'")" ] \
    || fail "release DB already has recovery_attempts — $REL_REF is not actually pre-#157?"
pass "stack up on the RELEASE $REL_REF; DB is at the pre-#157 schema (no recovery_attempts)"

# ── baseline: a completed deposit on the release (must survive the upgrade) ───────
log "RELEASE: a full L1→L2 deposit that completes normally (terminal state to preserve)"
env COMPOSE_PROJECT_NAME="$PROJECT" ./scripts/e2e-l1-to-l2.sh >/tmp/upg-baseline.log 2>&1 \
    || { sed -E 's/\x1b\[[0-9;]*m//g' /tmp/upg-baseline.log | tail -20; fail "release baseline deposit did not complete"; }
TERMINALS_BEFORE="$(pgq "SELECT count(*) FROM transactions WHERE status <> 'pending'")"
pass "release baseline deposit completed; $TERMINALS_BEFORE terminal txns recorded"

# ── orphan: a REAL claim, captured durably-admitted+unlinked, then SIGKILL ────────
log "RELEASE: driving a deposit and capturing its claim in the recoverable window"
SCEN_START="$(pgq "SELECT now()")"
ORPHAN_DEP="$(mktemp /tmp/upg-orphan.XXXXXX.log)"
env COMPOSE_PROJECT_NAME="$PROJECT" \
    CLAIM_SUBMIT_TIMEOUT=1200 CLAIM_COMMIT_TIMEOUT=400 BALANCE_ATTEMPTS=50 \
    ./scripts/e2e-l1-to-l2.sh >"$ORPHAN_DEP" 2>&1 &
ORPHAN_PID=$!
# capture this deposit's destination, then wait for it to confirm ready_for_claim.
ODEST=""
for _ in $(seq 1 150); do
    ODEST="$(sed -E 's/\x1b\[[0-9;]*m//g' "$ORPHAN_DEP" | grep -aoE 'Dest: +0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}' | head -1)"
    [ -n "$ODEST" ] && break
    kill -0 "$ORPHAN_PID" 2>/dev/null || { sed -E 's/\x1b\[[0-9;]*m//g' "$ORPHAN_DEP" | tail -20; fail "orphan deposit exited before provisioning"; }
    sleep 2
done
[ -n "$ODEST" ] || fail "no orphan-deposit destination captured"
ODESTHEX="$(printf '%s' "${ODEST#0x}" | tr 'A-F' 'a-f')"
for _ in $(seq 1 150); do grep -aq "Deposit is ready_for_claim" "$ORPHAN_DEP" && break; kill -0 "$ORPHAN_PID" 2>/dev/null || fail "orphan deposit exited before ready_for_claim"; sleep 3; done
# poll the DB for THIS deposit's claim, durably admitted + UNLINKED (recoverable window).
ORPHAN_HASH=""
for _ in $(seq 1 400); do
    ORPHAN_HASH="$(pgq "SELECT t.tx_hash FROM transactions t
        LEFT JOIN tx_note_links l ON l.tx_hash=t.tx_hash
        WHERE t.status='pending' AND l.tx_hash IS NULL AND t.miden_tx_id IS NULL
          AND position('${CLAIM_SELECTOR}' in encode(t.envelope_bytes,'hex')) > 0
          AND position('${ODESTHEX}' in encode(t.envelope_bytes,'hex')) > 0
          AND t.created_at >= '${SCEN_START}'
        ORDER BY t.created_at DESC LIMIT 1")"
    [[ "$ORPHAN_HASH" == 0x* ]] && break
    sleep 1
done
[[ "$ORPHAN_HASH" == 0x* ]] || { kill "$ORPHAN_PID" 2>/dev/null||true; fail "orphan claim never entered the recoverable window"; }
OSIGNER="$(pgq "SELECT lower(signer) FROM transactions WHERE tx_hash='$ORPHAN_HASH'")"
ONONCE_BEFORE="$(cast nonce --rpc-url "$L2_RPC" "$OSIGNER" 2>/dev/null)"
log "captured orphan claim $ORPHAN_HASH (signer=$OSIGNER nonce=$ONONCE_BEFORE dest=$ODEST)"

# disable rebroadcast (so ONLY the branch recovery — not the ClaimTxManager — heals it)
docker stop "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
container_running "$BRIDGE_SERVICE" && fail "bridge-service still running after stop"
# SIGKILL the RELEASE proxy with the claim pending+unlinked → an orphan v0.15.9 can't heal.
docker kill "$PROXY" >/dev/null 2>&1 || true
container_running "$PROXY" && fail "release proxy still running after kill"
pass "RELEASE proxy SIGKILLed with $ORPHAN_HASH pending+unlinked (an orphan the release binary cannot self-heal)"

# ── U1: swap ONLY the proxy to the branch image (same volumes) ────────────────────
log "UPGRADE (U1): swap proxy → branch image (miden-agglayer-e2e:latest), same PG + Miden store"
REJECT_UNVERIFIED_GER_INJECTION=false "${BASE[@]}" up -d --no-deps --force-recreate miden-agglayer >/tmp/upg-swap.log 2>&1 || fail "proxy swap failed"
proxy_ready 120 || fail "branch proxy did not come up after the swap"
docker inspect "$PROXY" --format '{{.Config.Image}}' | grep -q "latest" || fail "proxy is not the branch image after swap"
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] || { sleep 25; [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] || fail "branch proxy not healthy after swap"; }
pass "proxy upgraded to the branch binary in place"

# ── (1) migrations applied cleanly on the release-populated DB ────────────────────
for c in recovery_attempts next_recovery_at; do
    [ -n "$(pgq "SELECT 1 FROM information_schema.columns WHERE table_name='transactions' AND column_name='$c'")" ] \
        || fail "migration 021 did not add column $c"
done
[ -n "$(pgq "SELECT 1 FROM pg_indexes WHERE indexname='idx_txns_pending_recovery'")" ] || fail "021 index missing"
[ -n "$(pgq "SELECT to_regclass('public.claim_calldata_repair_pending')")" ] || fail "migration 019 table missing"
pass "(1) additive migrations 019 + 021 applied cleanly on the release-populated DB"

# ── (3a) already-terminal state preserved across the upgrade ──────────────────────
TERMINALS_AFTER="$(pgq "SELECT count(*) FROM transactions WHERE status <> 'pending'")"
[ "${TERMINALS_AFTER:-0}" -ge "${TERMINALS_BEFORE:-0}" ] || fail "terminal-txn count dropped across upgrade ($TERMINALS_BEFORE→$TERMINALS_AFTER)"
pass "(3a) already-terminal state preserved ($TERMINALS_BEFORE → $TERMINALS_AFTER terminals)"

# ── (2) the release-created orphan self-heals EXACTLY once (recovery, no rebroadcast)
log "(2) waiting for the branch recovery loop to heal the release orphan $ORPHAN_HASH (bridge-service stopped)"
HEALED=""
for _ in $(seq 1 140); do proxy_ready 5 || true; [ "$(receipt_status "$ORPHAN_HASH")" = "1" ] && { HEALED=1; break; }; sleep 5; done
[ -n "$HEALED" ] || fail "the pre-upgrade orphan was NOT recovered by the branch within ~700s"
pass "(2) SAME hash $ORPHAN_HASH recovered to success by the branch recovery loop — no rebroadcast"
ONONCE_AFTER="$(cast nonce --rpc-url "$L2_RPC" "$OSIGNER" 2>/dev/null)"
[ "$ONONCE_AFTER" = "$ONONCE_BEFORE" ] || fail "signer nonce moved during recovery ($ONONCE_BEFORE→$ONONCE_AFTER) — must NOT re-advance"
pass "(2) signer nonce NOT double-advanced across recovery ($ONONCE_BEFORE held)"

# restart the ClaimTxManager; the deposit wrapper (widened timeouts) is our exact-once
# credit oracle — it now completes off the recovered claim and asserts the balance delta.
docker start "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
if wait "$ORPHAN_PID"; then
    pass "(2) interrupted deposit CREDITED EXACTLY ONCE on dest $ODEST (wrapper exact-balance delta) via recovery across the upgrade"
else
    sed -E 's/\x1b\[[0-9;]*m//g' "$ORPHAN_DEP" | tail -25
    fail "the interrupted deposit did not reach an exact-once credit after the upgrade"
fi

# ── (3b) liveness: a fresh deposit works after the upgrade ────────────────────────
log "(3b) post-upgrade liveness: a fresh L1→L2 deposit must complete end-to-end"
env COMPOSE_PROJECT_NAME="$PROJECT" ./scripts/e2e-l1-to-l2.sh >/tmp/upg-fresh.log 2>&1 \
    || { sed -E 's/\x1b\[[0-9;]*m//g' /tmp/upg-fresh.log | tail -20; fail "post-upgrade fresh deposit failed"; }
pass "(3b) post-upgrade fresh deposit completed (credited exactly once)"

pass "UPGRADE-RECOVERY E2E GREEN: $REL_REF → branch in place — migrations 019+021 clean, release orphan self-healed exactly once (no rebroadcast, no nonce double-advance), terminal state preserved, bridge live"
