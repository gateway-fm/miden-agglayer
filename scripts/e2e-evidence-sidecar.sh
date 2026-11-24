#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Read-only evidence sidecar for a running e2e gate / test-e2e.
#
# `make test-e2e` tears its stack down (`down -v`) as soon as a tier fails, so a
# post-mortem taken afterwards finds no containers: no proxy log, no DB rows.
# This sidecar captures them WHILE the stack is up, without touching it:
#
#   - every POLL seconds, appends `docker logs --timestamps --since/--until`
#     windows of the stack's containers to <out>/<svc>.log (survives proxy
#     restarts and the final teardown — the last window is flushed on exit);
#   - while the #146 future-nonce test runs (detected in the watched log),
#     snapshots `transactions` and `queued_txns` every second into
#     <out>/future-nonce-db.txt.
#
# Usage: [SINCE=<RFC3339>] e2e-evidence-sidecar.sh <watched-test-log> <out-dir>
# Exits when the watched log stops growing for IDLE seconds (default 600) or
# `touch <out-dir>/STOP`.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
WATCH="$1"; OUT="$2"; mkdir -p "$OUT"
PROJECT="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
POLL="${POLL:-5}"; IDLE="${IDLE:-600}"
SVCS=(miden-agglayer miden-node ntx-builder validator tx-prover aggkit bridge-service agglayer)
PG="$PROJECT-agglayer-postgres-1"
strip() { sed -E 's/\x1b\[[0-9;]*m//g' "$WATCH" 2>/dev/null; }

since="${SINCE:-$(date -u +%FT%T.%NZ)}"   # SINCE=<RFC3339> backfills from the test's start
flush() { # one --since/--until window per container, gaps-free across restarts
    local until svc; until="$(date -u +%FT%T.%NZ)"
    for svc in "${SVCS[@]}"; do
        docker logs --timestamps --since "$since" --until "$until" "$PROJECT-$svc-1" \
            >> "$OUT/$svc.log" 2>&1 || true
    done
    since="$until"
}
snapshot_db() {
    {
        echo "=== $(date -u +%FT%T.%3NZ)"
        docker exec "$PG" psql -U agglayer -d agglayer_store -c \
            "SELECT signer, nonce, tx_hash, expires_at, parked_during_recovery, created_at FROM queued_txns ORDER BY created_at" \
            -c "SELECT address, nonce FROM nonces ORDER BY address" \
            -c "SELECT tx_hash, signer, status, error_message, miden_tx_id, block_number, created_at, updated_at FROM transactions ORDER BY created_at DESC LIMIT 20" 2>&1
    } >> "$OUT/future-nonce-db.txt"
}

echo "sidecar: watching $WATCH -> $OUT (project $PROJECT)" > "$OUT/sidecar.txt"
last_size=-1; last_change="$(date +%s)"; next_flush=0
while [[ ! -f "$OUT/STOP" ]]; do
    now="$(date +%s)"
    size="$(stat -c %s "$WATCH" 2>/dev/null || echo 0)"
    [[ "$size" != "$last_size" ]] && { last_size="$size"; last_change="$now"; }
    (( now - last_change > IDLE )) && { echo "idle ${IDLE}s — exiting" >> "$OUT/sidecar.txt"; break; }
    (( now >= next_flush )) && { flush; next_flush=$((now + POLL)); }
    # #146 window: from its step 1 until the test passes or fails.
    if strip | grep -q 'STEP: 1. submit nonce 1 (future)' \
       && ! strip | grep -qE '#146 future-nonce mempool:|^\[[0-9:]+\] FAIL: (the in-order|nonce-0|pending nonce|1\.|2\.|3\.)'; then
        snapshot_db
    fi
    if strip | grep -qE 'FAIL: the in-order nonce-0|FAIL: pending nonce did not reach 2|FAIL: nonce-0 submission'; then
        [[ -f "$OUT/FAILED-146" ]] || { date -u +%FT%T.%3NZ > "$OUT/FAILED-146"; snapshot_db; flush; }
    fi
    sleep 1
done
flush
echo "done $(date -u +%FT%TZ)" >> "$OUT/sidecar.txt"
