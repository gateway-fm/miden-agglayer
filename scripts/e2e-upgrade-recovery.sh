#!/usr/bin/env bash
# ============================================================================
# #157 IN-PLACE UPGRADE test: latest release (pre-#157, origin/main) → main+#157.
#
# The from-scratch gates never exercise the real upgrade risk: #157's recovery loop
# runs on startup against DURABLE STATE the OLD version wrote. This test proves that
# upgrade path end-to-end, on ONE persistent stack (same Postgres + Miden volumes):
#
#   1. MIGRATION — 021 (recovery_attempts / next_recovery_at + partial index) applies
#      cleanly on a DB that the OLD binary populated.
#   2. RECOVERY-OF-OLD-STATE — a PENDING ORPHAN created by a mid-proving crash on the
#      OLD binary (which has NO recovery loop, so it stays stuck) is self-healed by
#      the NEW binary EXACTLY ONCE, with no re-prove and no nonce double-advance.
#   3. PRESERVATION + LIVENESS — an already-terminal (successful) deposit from the OLD
#      binary is untouched, and a fresh deposit works after the upgrade.
#
# Mechanics: the compose proxy image is `miden-agglayer-e2e:latest`. We build the OLD
# binary from origin/main as :preupgrade, tag it AS :latest to bring the stack up on
# the old code, then retag :latest to the NEW image and force-recreate ONLY the proxy
# (volumes preserved) — a true in-place upgrade. REJECT_UNVERIFIED_GER=false so a
# controlled GER can be used to manufacture the orphan deterministically.
#
# Run standalone (needs the 8546/18080 ports free). ~50-70 min incl. the old build.
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WT="$(cd "$HERE/.." && pwd)"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
export COMPOSE_PROJECT_NAME="$PROJECT"
L2_RPC="${L2_RPC:-http://localhost:8546}"
PROXY="${PROJECT}-miden-agglayer-1"
PG="${PROJECT}-agglayer-postgres-1"
BRIDGE="${BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
GER_KEY="${GER_TEST_KEY:-0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d}"
OLD_REF="${UPGRADE_FROM_REF:-origin/main}"
OLD_TAG=miden-agglayer-e2e:preupgrade
NEW_TAG=miden-agglayer-e2e:postupgrade
RUN_TAG=miden-agglayer-e2e:latest          # what the compose actually runs
COMPOSE=(docker compose -f docker-compose.e2e.yml -f docker-compose.l2l2.yml --env-file fixtures/.env)

log()  { echo "[upg] $*"; }
pass() { echo "[upg] PASS: $*"; }
fail() { echo "[upg] FAIL: $*"; exit 1; }
pgq()  { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }
receipt_status() { cast receipt --rpc-url "$L2_RPC" "$1" status 2>/dev/null | awk '{print $1; exit}'; }
container_running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ]; }
proxy_ready() { for _ in $(seq 1 "${1:-60}"); do cast chain-id --rpc-url "$L2_RPC" >/dev/null 2>&1 && return 0; sleep 3; done; return 1; }

cd "$WT" || fail "cannot cd $WT"
GER_ADDR="$(cast wallet address --private-key "$GER_KEY")"

# ── Build both images ──────────────────────────────────────────────────────────
# The NEW image is whatever HEAD currently builds; capture it first.
log "building NEW image (HEAD = main+#157) → $NEW_TAG"
"${COMPOSE[@]}" build miden-agglayer >/dev/null 2>&1 || fail "new image build failed"
docker tag "$RUN_TAG" "$NEW_TAG" || fail "could not tag new image"

log "building OLD image ($OLD_REF, pre-#157) → $OLD_TAG (isolated worktree)"
git fetch origin -q 2>/dev/null || true
OLD_WT="$(mktemp -d /tmp/upg-old.XXXXXX)"
git worktree add --detach "$OLD_WT" "$OLD_REF" >/dev/null 2>&1 || fail "git worktree add $OLD_REF failed"
cleanup_wt() { git worktree remove --force "$OLD_WT" >/dev/null 2>&1 || rm -rf "$OLD_WT"; }
trap cleanup_wt EXIT
docker build -t "$OLD_TAG" "$OLD_WT" >/dev/null 2>&1 || fail "old image build from $OLD_REF failed"
OLD_SHA="$(cd "$OLD_WT" && git rev-parse --short HEAD)"
log "old=$OLD_SHA  new=$(git rev-parse --short HEAD)"

# ── Bring up on the OLD binary ──────────────────────────────────────────────────
log "teardown any prior $PROJECT stack"
"${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
left=$(docker ps -aq --filter "name=$PROJECT"); [ -n "$left" ] && docker rm -f $left >/dev/null 2>&1
vols=$(docker volume ls -q --filter "name=$PROJECT"); [ -n "$vols" ] && docker volume rm $vols >/dev/null 2>&1

log "pinning the stack to the OLD binary (tag $OLD_TAG → $RUN_TAG) and bringing it up"
docker tag "$OLD_TAG" "$RUN_TAG" || fail "retag old→latest failed"
REJECT_UNVERIFIED_GER_INJECTION=false make e2e-l2l2-up >/tmp/upg-up.log 2>&1 || log "(up nonzero — verifying)"
for i in $(seq 1 100); do [ "$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "${PROJECT}-anvil-1" 2>/dev/null)" = healthy ] && break; sleep 3; done
for retry in 1 2 3 4 5; do
    created=$(docker ps -a --filter "name=$PROJECT" --filter status=created -q | wc -l); [ "$created" -eq 0 ] && break
    MIDEN_NODE_GIT_URL=https://github.com/0xMiden/node.git MIDEN_NODE_GIT_REF=v0.15.0 REJECT_UNVERIFIED_GER_INJECTION=false \
        "${COMPOSE[@]}" up -d >>/tmp/upg-up.log 2>&1 || true; sleep 15
done
STABLE=0; for i in $(seq 1 120); do if docker exec "$PROXY" cat /var/lib/miden-agglayer-service/bridge_accounts.toml >/dev/null 2>&1; then STABLE=$((STABLE+1)); [ "$STABLE" -ge 6 ] && break; else STABLE=0; fi; sleep 5; done
sleep 15
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] || fail "old-binary proxy not healthy"
# Confirm we are actually on the OLD binary (it must NOT know migration 021).
docker exec "$PROXY" /usr/local/bin/miden-agglayer-service --version >/dev/null 2>&1 || true
pass "stack up on the OLD binary ($OLD_SHA); Postgres NOT yet migrated to 021"
[ -z "$(pgq "SELECT 1 FROM information_schema.columns WHERE table_name='transactions' AND column_name='recovery_attempts'")" ] \
    || fail "OLD DB already has recovery_attempts — old image is not actually pre-#157?"
pass "confirmed: pre-upgrade DB has NO recovery_attempts column (021 not applied)"

# ── (a) A COMPLETED deposit on the OLD binary (must be preserved across upgrade) ──
log "OLD binary: a full L1→L2 deposit that completes normally (terminal state to preserve)"
DONE_LOG="$(mktemp /tmp/upg-done.XXXXXX.log)"
env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l1-to-l2.sh" > "$DONE_LOG" 2>&1 \
    || { sed -E 's/\x1b\[[0-9;]*m//g' "$DONE_LOG" | tail -20; fail "OLD-binary baseline deposit did not complete"; }
DONE_DEST="$(sed -E 's/\x1b\[[0-9;]*m//g' "$DONE_LOG" | grep -aoE 'Dest: +0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}' | head -1)"
DONE_TERMINALS_BEFORE="$(pgq "SELECT count(*) FROM transactions WHERE status <> 'pending'")"
pass "OLD-binary baseline deposit completed (dest $DONE_DEST); $DONE_TERMINALS_BEFORE terminal txns recorded"

# ── (b) Manufacture a PENDING ORPHAN on the OLD binary (mid-proving crash) ────────
# The old binary has no recovery loop, so this row stays stuck: a durably-admitted
# GER (pending row + advanced nonce) whose in-memory writer job dies with the proxy.
log "OLD binary: submitting a controlled GER, then SIGKILL the proxy in its recoverable window"
NONCE_BEFORE="$(cast nonce --rpc-url "$L2_RPC" "$GER_ADDR")"
ORPHAN_GER="0x$(printf '%064x' "$(( 0xE70000 + RANDOM ))")"
ORPHAN_HASH="$(cast send --async --rpc-url "$L2_RPC" --private-key "$GER_KEY" --legacy --gas-price 1 --gas-limit 1000000 \
    "$BRIDGE" 'insertGlobalExitRoot(bytes32)' "$ORPHAN_GER" 2>/dev/null)"
[[ "$ORPHAN_HASH" == 0x* ]] || fail "controlled GER not admitted on OLD binary (REJECT_UNVERIFIED_GER=true?)"
# Wait until it is durably admitted + still UNLINKED (recoverable window), then kill.
CAUGHT=""
for _ in $(seq 1 60); do
    st="$(pgq "SELECT t.status FROM transactions t LEFT JOIN tx_note_links l ON l.tx_hash=t.tx_hash
               WHERE t.tx_hash='$ORPHAN_HASH' AND t.status='pending' AND l.tx_hash IS NULL AND t.miden_tx_id IS NULL")"
    [ "$st" = "pending" ] && { CAUGHT=1; break; }
    sleep 1
done
[ -n "$CAUGHT" ] || fail "controlled GER never reached the pending+unlinked window on OLD binary"
docker kill "$PROXY" >/dev/null 2>&1 || true
container_running "$PROXY" && fail "OLD proxy still running after kill"
pass "OLD proxy SIGKILLed with $ORPHAN_HASH pending+unlinked (an orphan the old binary cannot self-heal)"

# Bring the OLD proxy back and confirm it does NOT recover the orphan (no recovery loop).
docker start "$PROXY" >/dev/null 2>&1 || true
proxy_ready 60 || fail "OLD proxy did not restart"
sleep 45
ORPHAN_STATE_OLD="$(pgq "SELECT status FROM transactions WHERE tx_hash='$ORPHAN_HASH'")"
[ "$ORPHAN_STATE_OLD" = "pending" ] || fail "expected the orphan to stay pending on the OLD binary, got '$ORPHAN_STATE_OLD'"
NONCE_STUCK="$(cast nonce --rpc-url "$L2_RPC" "$GER_ADDR")"
pass "OLD binary left $ORPHAN_HASH STUCK pending (nonce advanced $NONCE_BEFORE→$NONCE_STUCK, blocking later nonces) — as expected"

# ── UPGRADE IN PLACE: swap ONLY the proxy to the NEW binary, volumes preserved ────
log "UPGRADE: retag $NEW_TAG → $RUN_TAG and force-recreate ONLY the proxy (same PG + Miden volumes)"
docker tag "$NEW_TAG" "$RUN_TAG" || fail "retag new→latest failed"
# Keep REJECT_UNVERIFIED_GER_INJECTION=false on the recreated proxy so the controlled
# GER orphan can be re-driven to injection by the new recovery loop.
REJECT_UNVERIFIED_GER_INJECTION=false "${COMPOSE[@]}" up -d --no-deps --force-recreate miden-agglayer >/tmp/upg-recreate.log 2>&1 || fail "proxy recreate failed"
proxy_ready 90 || fail "NEW-binary proxy did not come up after upgrade"
[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] || { sleep 20; [ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY" 2>/dev/null)" = healthy ] || fail "NEW-binary proxy not healthy after upgrade"; }
pass "proxy upgraded to the NEW binary in place"

# ── (1) MIGRATION applied cleanly ────────────────────────────────────────────────
for c in recovery_attempts next_recovery_at; do
    [ -n "$(pgq "SELECT 1 FROM information_schema.columns WHERE table_name='transactions' AND column_name='$c'")" ] \
        || fail "migration 021 did not add column $c"
done
[ -n "$(pgq "SELECT 1 FROM pg_indexes WHERE indexname='idx_txns_pending_recovery'")" ] \
    || fail "migration 021 did not create idx_txns_pending_recovery"
pass "(1) migration 021 applied cleanly on the old-populated DB (columns + index present)"

# ── (3) PRESERVATION: the old completed deposit's terminals are untouched ─────────
DONE_TERMINALS_AFTER="$(pgq "SELECT count(*) FROM transactions WHERE status <> 'pending'")"
[ "${DONE_TERMINALS_AFTER:-0}" -ge "${DONE_TERMINALS_BEFORE:-0}" ] \
    || fail "terminal-txn count decreased across upgrade ($DONE_TERMINALS_BEFORE → $DONE_TERMINALS_AFTER) — old state lost?"
pass "(3a) already-terminal state preserved across upgrade ($DONE_TERMINALS_BEFORE → $DONE_TERMINALS_AFTER terminals)"

# ── (2) RECOVERY-OF-OLD-STATE: the pre-upgrade orphan self-heals EXACTLY once ─────
log "(2) waiting for the NEW recovery loop to heal the OLD-binary orphan $ORPHAN_HASH"
HEALED=""
for _ in $(seq 1 120); do
    proxy_ready 5 || true
    [ "$(receipt_status "$ORPHAN_HASH")" = "1" ] && { HEALED=1; break; }
    sleep 5
done
[ -n "$HEALED" ] || fail "the pre-upgrade orphan was NOT recovered by the new binary within ~600s"
pass "(2) SAME hash $ORPHAN_HASH recovered to success by the NEW recovery loop (no client action)"
INJECTED="$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected AND ger_hash = decode('${ORPHAN_GER#0x}','hex')")"
[ "${INJECTED:-0}" -ge 1 ] || fail "orphan GER effect not applied after recovery"
NONCE_AFTER="$(cast nonce --rpc-url "$L2_RPC" "$GER_ADDR")"
[ "$NONCE_AFTER" = "$NONCE_STUCK" ] \
    || fail "signer nonce moved during recovery ($NONCE_STUCK → $NONCE_AFTER) — recovery must NOT re-advance the nonce"
pass "(2) orphan effect applied EXACTLY once (is_injected) and nonce NOT double-advanced ($NONCE_STUCK held)"

# ── (3b) LIVENESS: a fresh deposit works after the upgrade ───────────────────────
log "(3b) post-upgrade liveness: a fresh L1→L2 deposit must complete end-to-end"
FRESH_LOG="$(mktemp /tmp/upg-fresh.XXXXXX.log)"
if env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l1-to-l2.sh" > "$FRESH_LOG" 2>&1; then
    pass "(3b) post-upgrade fresh deposit completed (credited exactly once)"
else
    sed -E 's/\x1b\[[0-9;]*m//g' "$FRESH_LOG" | tail -20
    fail "post-upgrade fresh deposit failed — pipeline not healthy after upgrade"
fi

# restore :latest to the NEW image for any subsequent runs
docker tag "$NEW_TAG" "$RUN_TAG" >/dev/null 2>&1 || true
pass "UPGRADE E2E GREEN: $OLD_SHA → main+#157 in place — migration 021 clean, old orphan self-healed exactly once, terminal state preserved, bridge live"
