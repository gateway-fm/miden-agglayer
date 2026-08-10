#!/usr/bin/env bash
# aggkit-preserve-heal.sh — versioned, self-contained heal for the aggkit
# "lost-in-transit GER-inject tx" wedge (PR#164 blocker #8; findings #70/#89).
#
# THE WEDGE: a proxy restart can lose an aggoracle GER-inject tx in transit —
# aggkit's ephemeral monitoring DB marks it sent, the proxy never durably
# admitted it, and aggkit's deterministic tx-ID dedup blocks any re-send
# forever. GER injection freezes.
#
# WHY NOT A BLANKID RECREATE: a plain `force-recreate` wipes the WHOLE container
# fs — including aggsender's cert lineage (unrecoverable after an L2 history
# shift, #89) and the bridgesync cursors (cold resync into anvil's 256-state
# wall permanently halts L2<->L2, #87). The ONLY poisoned file is
# /tmp/ethtxmanager-aggoracle.sqlite.
#
# THIS HEAL: stop -> copy every /tmp/*.sqlite* OUT except ethtxmanager* ->
# `compose create --force-recreate` (NOT started) -> copy the preserved DBs back
# -> start. The monitor DB is cleared; cert lineage + sync cursors survive.
#
# Usage: PROJECT=<compose-project> ./scripts/aggkit-preserve-heal.sh <aggkit|aggkit-l2b>
# Returns 0 on heal, 2 if there is no wedge to heal (no-op), 1 on error.
set -uo pipefail

SVC="${1:?usage: aggkit-preserve-heal.sh <aggkit|aggkit-l2b>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="${PROJECT:-${COMPOSE_PROJECT_NAME:-miden-agglayer}}"
ENV_FILE="${ENV_FILE:-$PROJECT_DIR/fixtures/.env}"
C="$PROJECT-$SVC-1"
PG="$PROJECT-agglayer-postgres-1"
REPEATS_MIN="${REPEATS_MIN:-5}"       # same-hash repeats/60s to call it wedged
FORCE="${FORCE:-0}"                    # 1 = heal without the wedge precheck

COMPOSE=(-f "$PROJECT_DIR/docker-compose.e2e.yml")
[[ -f "$PROJECT_DIR/docker-compose.l2l2.yml" ]] && COMPOSE+=(-f "$PROJECT_DIR/docker-compose.l2l2.yml")
[[ -f "$PROJECT_DIR/docker-compose.web3signer.yml" ]] \
    && docker ps --format '{{.Names}}' | grep -q "^$PROJECT-web3signer-1$" \
    && { COMPOSE+=(-f "$PROJECT_DIR/docker-compose.web3signer.yml")
         [[ -f "$PROJECT_DIR/fixtures/web3signer-keys.env" ]] && { set -a; . "$PROJECT_DIR/fixtures/web3signer-keys.env"; set +a; } }

log() { echo "[$(date '+%H:%M:%S')] preserve-heal($SVC): $*"; }

docker inspect "$C" >/dev/null 2>&1 || { log "container $C not found"; exit 1; }

# ── wedge detection: SAME inject-tx ID repeating AND unknown to the proxy ─────
if [[ "$FORCE" != "1" ]]; then
    top="$(docker logs "$C" --since 60s 2>&1 \
        | grep 'already exists in monitoring DB' \
        | grep -oE 'ID: 0x[a-f0-9]{64}' | awk '{print $2}' \
        | sort | uniq -c | sort -rn | head -1)"
    cnt="$(awk '{print $1}' <<<"$top")"; tx="$(awk '{print $2}' <<<"$top")"
    if [[ -z "${cnt:-}" || "${cnt:-0}" -lt "$REPEATS_MIN" || -z "${tx:-}" ]]; then
        log "no lost-tx wedge signature (repeats=${cnt:-0} < $REPEATS_MIN) — nothing to heal"
        exit 2
    fi
    known="$(docker exec "$PG" psql -U agglayer -d agglayer_store -tAc \
        "SELECT count(*) FROM transactions WHERE tx_hash='$tx'" 2>/dev/null)"
    if [[ "${known:-1}" != "0" ]]; then
        log "tx ${tx:0:18}… is known to the proxy — recovery's job, not a lost-tx wedge; no heal"
        exit 2
    fi
    log "lost-in-transit wedge: tx ${tx:0:18}… x$cnt/60s, unknown to proxy"
fi

# ── the heal: preserve every DB except the poisoned monitor DB ───────────────
B="$(mktemp -d)"; trap 'rm -rf "$B"' EXIT
docker stop "$C" >/dev/null 2>&1
saved=0
for f in aggsender bridgel2sync L1InfoTreeSync reorgdetectorl1 reorgdetectorl2; do
    for ext in sqlite sqlite-wal sqlite-shm; do
        docker cp "$C:/tmp/$f.$ext" "$B/" >/dev/null 2>&1 && saved=$((saved+1)) || true
    done
done
COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    create --force-recreate --no-deps "$SVC" >/dev/null 2>&1 \
    || { log "recreate failed"; exit 1; }
restored=0
for f in "$B"/*; do
    [ -f "$f" ] || continue
    docker cp "$f" "$C:/tmp/" >/dev/null 2>&1 && restored=$((restored+1)) || true
done
COMPOSE_PROJECT_NAME="$PROJECT" docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    start "$SVC" >/dev/null 2>&1 \
    || { log "start failed"; exit 1; }
log "preserve-healed (saved=$saved restored=$restored, ethtxmanager-aggoracle.sqlite wiped)"
exit 0
