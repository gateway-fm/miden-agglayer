#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-full-db-loss-recovery.sh — FULL proxy state loss (postgres store AND
# miden-client sqlite) must restore a FAITHFUL history (#88), keep aggkit's
# cert lineage alive (#89), and mint no poison notes (#86).
#
# This is the disaster-recovery scenario the 2026-08-08 soak proved lossy:
# the drop-restore rebuilt ger_entries with is_injected=false and re-emitted
# NO historical UpdateHashChain logs (MA#28 skipped every historical
# UpdateGerNote as MissingMetadata once the client store was wiped too), so
# aggoracle re-injected registered GERs — every re-inject minting an immortal
# ERR_GER_ALREADY_REGISTERED poison note — and aggsender's next cert
# double-imported claims ("non-inclusion proof for a key present in the SMT").
#
# Post-fix (a0b824f) the restore's block walk feeds node-authoritative
# metadata to the MA#28 check, so the restored synthetic history must be
# IDENTICAL to the pre-drop history. This script asserts exactly that:
#
#   1. UpdateHashChain log count   AFTER == BEFORE
#   2. is_injected count           AFTER == BEFORE
#   3. hash_chain_value            AFTER == BEFORE  (order-sensitive replay)
#   4. BridgeEvent + ClaimEvent    AFTER == BEFORE  (#69/#136 held too)
#   5. ntx-builder: ZERO ERR_GER_ALREADY_REGISTERED kernel asserts post-restore
#   6. liveness: a NEW L1→Miden deposit reaches ready_for_claim and a NEW GER
#      is injected+consumed (is_injected grows) — pipeline works, not just data
#
# Runs against the LIVE stack (any compose project); state accumulated so far
# is the test fixture. DESTRUCTIVE to the proxy store by design — that is the
# scenario. Requires the post-#88 image to be the one the stack runs.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-$(grep -m1 '^MIDEN_NODE_GIT_URL' "$PROJECT_DIR/Makefile" | sed 's/.*= *//')}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-$(grep -m1 '^MIDEN_NODE_GIT_REF' "$PROJECT_DIR/Makefile" | sed 's/.*= *//')}"
export WEB3SIGNER_UID="${WEB3SIGNER_UID:-$(id -u)}" WEB3SIGNER_GID="${WEB3SIGNER_GID:-$(id -g)}"

# PR#164 blocker #6 — this test IRREVERSIBLY DROPS a database. It must NEVER
# guess its victim. Require an explicit PROXY_CONTAINER, or auto-select ONLY
# when exactly one candidate is running; refuse on ambiguity.
CANDIDATES="$(docker ps --format '{{.Names}}' | grep -E -- '-miden-agglayer-1$' || true)"
if [[ -n "${PROXY_CONTAINER:-}" ]]; then
    grep -qxF "$PROXY_CONTAINER" <<<"$CANDIDATES" \
        || { echo "FATAL: PROXY_CONTAINER='$PROXY_CONTAINER' is not a running *-miden-agglayer-1 container"; exit 1; }
else
    n_cand="$(grep -c . <<<"$CANDIDATES" 2>/dev/null || echo 0)"
    if [[ "$n_cand" -eq 0 ]]; then
        echo "FATAL: no running *-miden-agglayer-1 container"; exit 1
    elif [[ "$n_cand" -gt 1 ]]; then
        echo "FATAL: $n_cand proxy stacks are running — refusing to guess which DB to DROP."
        echo "       Set PROXY_CONTAINER=<one of> explicitly:"; sed 's/^/         /' <<<"$CANDIDATES"
        exit 1
    fi
    PROXY_CONTAINER="$CANDIDATES"
fi
PROJECT="${PROXY_CONTAINER%-miden-agglayer-1}"
PG_CONTAINER="$PROJECT-agglayer-postgres-1"
NTX_CONTAINER="$PROJECT-ntx-builder-1"
export COMPOSE_PROJECT_NAME="$PROJECT"

# Verify the postgres we are about to DROP is THIS proxy's configured store:
# the container must exist, be in the same compose project, and the proxy must
# actually reference it (its store URL names this postgres host). A destructive
# test must confirm — not assume — its target.
docker inspect "$PG_CONTAINER" >/dev/null 2>&1 \
    || { echo "FATAL: expected store container $PG_CONTAINER not found for proxy $PROXY_CONTAINER"; exit 1; }
PROXY_PROJECT_LABEL="$(docker inspect "$PROXY_CONTAINER" --format '{{ index .Config.Labels "com.docker.compose.project" }}' 2>/dev/null)"
PG_PROJECT_LABEL="$(docker inspect "$PG_CONTAINER"    --format '{{ index .Config.Labels "com.docker.compose.project" }}' 2>/dev/null)"
[[ -n "$PROXY_PROJECT_LABEL" && "$PROXY_PROJECT_LABEL" == "$PG_PROJECT_LABEL" ]] \
    || { echo "FATAL: $PG_CONTAINER (project '$PG_PROJECT_LABEL') is not in the proxy's project ('$PROXY_PROJECT_LABEL')"; exit 1; }
if ! docker inspect "$PROXY_CONTAINER" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null \
        | grep -qiE "(postgres|store|database).*(${PG_CONTAINER}|agglayer-postgres)"; then
    echo "WARN: could not confirm proxy $PROXY_CONTAINER references $PG_CONTAINER via env; relying on project-label match"
fi

COMPOSE=(-f "$PROJECT_DIR/docker-compose.e2e.yml")
[[ -f "$PROJECT_DIR/docker-compose.l2l2.yml" ]] && docker ps --format '{{.Names}}' | grep -q "^$PROJECT-anvil-l2b-1$" \
    && COMPOSE+=(-f "$PROJECT_DIR/docker-compose.l2l2.yml")
if docker ps --format '{{.Names}}' | grep -q "^$PROJECT-web3signer-1$"; then
    COMPOSE+=(-f "$PROJECT_DIR/docker-compose.web3signer.yml")
    # ${AGGLAYER_SIGNER_KEYS:?} is interpolated at compose parse time.
    [[ -f "$PROJECT_DIR/fixtures/web3signer-keys.env" ]] && { set -a; . "$PROJECT_DIR/fixtures/web3signer-keys.env"; set +a; }
fi

L1_RPC="${L1_RPC:-http://localhost:8545}"
L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
SIGNER_KEY="${SIGNER_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"
RUN_SUFFIX="$(date +%s)"
EVIDENCE="/tmp/full-db-loss-recovery-${RUN_SUFFIX}.txt"

ts()   { date +%H:%M:%S; }
say()  { printf '[%s] %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }
step() { printf '\n[%s] STEP: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }
fail() { printf '[%s] FAIL: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE" >&2; exit 1; }
pass() { printf '[%s] PASS: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }

pgq() { docker exec "$PG_CONTAINER" psql -U agglayer -d agglayer_store -tAc "$1"; }

# PR#164 blocker #7 — COUNT comparison is unsafe: losing one row and
# reconstructing a different row passes with the same count. `fingerprint`
# digests the ORDERED identities/content of each row set (so a swapped row
# changes the digest), and `counts` stays for human logging + the thinness gate.
#   - synthetic_logs digest: md5 over (tx_hash:log_index:data) ordered
#   - injected-GER set digest: md5 over ger_hash ordered
#   - hash_chain_value: the order-sensitive rolling-chain assertion (kept as-is)
log_digest() { # $1 = topic0 hex prefix
    pgq "SELECT md5(coalesce(string_agg(transaction_hash || ':' || log_index || ':' || data, '|' \
         ORDER BY transaction_hash, log_index), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '$1%'"
}
# GER / UpdateHashChain content digest — deliberately EXCLUDES transaction_hash
# and log_index, because neither can survive a full DB loss even when the
# restored history is perfectly faithful:
#
#   * transaction_hash — the injection is carried by an eth-tx aggoracle submits
#     to THIS proxy's own L2 RPC (`eth_sendRawTransaction` -> `insert_ger`). Its
#     hash is derived from the signed raw tx and is recorded ONLY in the proxy's
#     `transactions` / `tx_note_links` rows, which this drill drops. It was never
#     on L1 and is not carried by the Miden note, so there is no source left to
#     recover it from; restore falls back to a deterministic derived hash.
#   * log_index — a SINGLE global counter shared by Bridge/Claim/UHC emissions.
#     Restore replays by phase (all B2AGG, then CLAIM, then GER) rather than in
#     the original interleaved arrival order, so the indices are re-numbered even
#     when every event is faithfully reproduced.
#
# What MUST be identical is the part that carries meaning: each injected GER's
# VALUE, its per-block attribution, its ORDER (asserted separately and exactly by
# the rolling `hash_chain_value`) and its count. Those are asserted strictly.
# Bridge/Claim keep the FULL digest (tx identity included) — theirs is
# reconstructed from node-authoritative note bodies, so it must survive.
uhc_content_digest() {
    pgq "SELECT md5(coalesce(string_agg(block_number || ':' || data, '|' \
         ORDER BY block_number, data), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%'"
}
fingerprint() {  # -> "uhc_d inj_d bridge_d claim_d hcv"  (digests, for identity assertion)
    local uhc inj bridge claim hcv
    uhc=$(uhc_content_digest)
    inj=$(pgq "SELECT md5(coalesce(string_agg(encode(ger_hash,'hex'), '|' ORDER BY ger_hash), '')) \
               FROM ger_entries WHERE is_injected=true")
    bridge=$(log_digest '0x50178120')
    claim=$(log_digest '0x1df3f2a9')
    hcv=$(pgq "SELECT encode(hash_chain_value,'hex') FROM service_state WHERE id=1")
    echo "$uhc $inj $bridge $claim $hcv"
}
counts() {  # -> "uhc inj bridge claim"  (integers, for logging + thinness gate)
    local uhc inj bridge claim
    uhc=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%'")
    inj=$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected=true")
    bridge=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x50178120%'")
    claim=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%'")
    echo "$uhc $inj $bridge $claim"
}

wait_healthy() {
    local deadline=$((SECONDS + ${1:-180}))
    while (( SECONDS < deadline )); do
        [[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY_CONTAINER" 2>/dev/null)" == healthy ]] && return 0
        sleep 3
    done
    return 1
}

# ── Phase 0: pre-drop fingerprint on the live, quiesced stack ────────────────
step "Phase 0 — pre-drop fingerprint (accumulated state is the fixture)"
read -r NUHC0 NINJECTED0 NBR0 NCL0 <<<"$(counts)"
read -r UHC0 INJ0 BR0 CL0 HCV0 <<<"$(fingerprint)"
say "before: counts UHC=$NUHC0 injected=$NINJECTED0 Bridge=$NBR0 Claim=$NCL0  hash_chain=${HCV0:0:16}…"
say "before: digests uhc=${UHC0:0:12} inj=${INJ0:0:12} bridge=${BR0:0:12} claim=${CL0:0:12}"
[[ "$NUHC0" -ge 1 && "$NINJECTED0" -ge 1 ]] || fail "fixture too thin (UHC=$NUHC0 inj=$NINJECTED0) — run traffic first"
[[ "$NUHC0" == "$NINJECTED0" ]] || say "note: UHC($NUHC0) != injected($NINJECTED0) pre-drop — carrying the delta forward"
NTX_MARK=$(docker logs "$NTX_CONTAINER" 2>&1 | grep -c "1007209807211405110" || true)
say "ntx kernel-assert (poison) lines so far: $NTX_MARK"

# ── Phase 1: FULL loss — drop the postgres store AND the miden-client store ──
step "Phase 1 — full state loss: stop proxy, DROP agglayer_store, wipe client sqlite"
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" stop miden-agglayer >/dev/null
docker exec "$PG_CONTAINER" psql -U agglayer -d postgres -c \
    "DROP DATABASE agglayer_store WITH (FORCE);" >/dev/null
docker exec "$PG_CONTAINER" psql -U agglayer -d postgres -c \
    "CREATE DATABASE agglayer_store OWNER agglayer;" >/dev/null
pass "postgres store dropped + recreated empty (migrations run on next boot)"
# The client sqlite is wiped by --reset-miden-store in the one-shot below.

# ── Phase 2: documented operator recovery, faithful to the RUNNING config ────
step "Phase 2 — one-shot: <live proxy command> --reset-miden-store --restore"
# Clone the live container's exact command (signer flags, hardening flags, …)
# so the restore runs under the same custody/config as the service.
mapfile -t LIVE_CMD < <(docker inspect "$PROXY_CONTAINER" --format '{{range .Config.Cmd}}{{println .}}{{end}}' | sed '/^$/d')
RESTORE_LOG="/tmp/full-db-loss-restore-${RUN_SUFFIX}.log"
set +e
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    run --rm --no-deps miden-agglayer \
    "${LIVE_CMD[@]}" --reset-miden-store --restore \
    >"$RESTORE_LOG" 2>&1
RESTORE_EXIT=$?
set -e
say "restore one-shot exit=$RESTORE_EXIT (log: $RESTORE_LOG)"
[[ "$RESTORE_EXIT" -eq 0 ]] || { tail -15 "$RESTORE_LOG" | tee -a "$EVIDENCE"; fail "restore one-shot failed"; }
grep -q "reset_miden_store" "$RESTORE_LOG" || fail "reset_miden_store marker missing — client store was NOT wiped"
GER_RESTORED=$(grep -c "rebuilt GER from consumed UpdateGerNote" "$RESTORE_LOG" || true)
say "restore log: rebuilt $GER_RESTORED GER(s) from consumed UpdateGerNotes"

# ── Phase 3: normal start, then fidelity assertions ──────────────────────────
step "Phase 3 — start proxy, assert restored history is IDENTICAL"
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" start miden-agglayer >/dev/null
wait_healthy 180 || fail "proxy not healthy within 180s after restore"
pass "proxy healthy"
sleep 10   # let the reconcile catch-up settle anything the one-shot left

read -r NUHC1 NINJECTED1 NBR1 NCL1 <<<"$(counts)"
read -r UHC1 INJ1 BR1 CL1 HCV1 <<<"$(fingerprint)"
say "after : counts UHC=$NUHC1 injected=$NINJECTED1 Bridge=$NBR1 Claim=$NCL1  hash_chain=${HCV1:0:16}…"
# Ordered-content digests (blocker #7): a swapped/reconstructed row changes the
# digest even when the count is identical.
[[ "$NUHC1" == "$NUHC0" ]] || fail "#88: UpdateHashChain log COUNT changed across restore ($NUHC0 -> $NUHC1) — GER history was lost or duplicated"
[[ "$UHC1" == "$UHC0" ]] || fail "#88: UpdateHashChain CONTENT differs across restore (per-block GER values; digest $UHC0 -> $UHC1; counts $NUHC0 -> $NUHC1)"
pass "UpdateHashChain content identical — same GER values at the same blocks (count $NUHC1, digest ${UHC1:0:12}); tx_hash/log_index intentionally excluded (unrecoverable after full DB loss — see log_digest notes), with ORDER asserted exactly by hash_chain_value below"
[[ "$INJ1" == "$INJ0" ]] || fail "#88: injected-GER set differs across restore (digest $INJ0 -> $INJ1; counts $NINJECTED0 -> $NINJECTED1)"
pass "injected-GER set identical (count $NINJECTED1, digest ${INJ1:0:12})"
[[ "$HCV1" == "$HCV0" ]] || fail "#88: hash_chain_value diverged (order-sensitive replay broke): $HCV0 vs $HCV1"
pass "hash_chain_value identical (order-faithful replay)"
[[ "$BR1" == "$BR0" ]] || fail "#69/#136 regression: BridgeEvent rows differ (digest $BR0 -> $BR1)"
[[ "$CL1" == "$CL0" ]] || fail "#69/#136 regression: ClaimEvent rows differ (digest $CL0 -> $CL1)"
pass "BridgeEvent (count $NBR1) + ClaimEvent (count $NCL1) rows identical"

# ── Phase 4: no poison minted, pipeline alive ────────────────────────────────
step "Phase 4 — no ERR_GER_ALREADY_REGISTERED poison; pipeline processes NEW traffic"
sleep 45   # window for aggoracle to (wrongly) re-inject + ntx to (wrongly) assert
NTX_NOW=$(docker logs "$NTX_CONTAINER" 2>&1 | grep -c "1007209807211405110" || true)
[[ "$NTX_NOW" -le "$NTX_MARK" ]] \
    || fail "#86: NEW poison-note kernel asserts after restore ($NTX_MARK -> $NTX_NOW) — aggoracle re-injected registered GERs"
pass "zero new poison-note kernel asserts ($NTX_NOW)"

CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
DEST="0x00000000000000000000000000$(printf '%014x' "$RUN_SUFFIX")"
say "liveness: bridgeAsset cnt=$CNT dest=$DEST"
cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

deadline=$((SECONDS + 300))
while :; do
    READY=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=25&offset=0" 2>/dev/null \
        | python3 -c "import json,sys; ds=json.load(sys.stdin).get('deposits',[]); print(any(d.get('ready_for_claim') for d in ds))" 2>/dev/null || echo err)
    [[ "$READY" == "True" ]] && break
    (( SECONDS >= deadline )) && fail "post-restore deposit never ready_for_claim in 300s — pipeline dead after restore"
    sleep 5
done
pass "post-restore deposit ready_for_claim"

deadline=$((SECONDS + 240))
while :; do
    INJ2=$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected=true")
    [[ "$INJ2" -gt "$INJ1" ]] && break
    (( SECONDS >= deadline )) && fail "no NEW GER injected+consumed post-restore in 240s (was $INJ1)"
    sleep 5
done
pass "new GER injected+consumed post-restore ($INJ1 -> $INJ2)"

step "RESULT"
pass "FULL DB LOSS RECOVERY: faithful history + no poison + live pipeline (evidence: $EVIDENCE)"
