#!/usr/bin/env bash
# Full e2e battery x4 for PR #176 / issue #167.
# Sequential, unattended, resumable. Records every target to results.tsv and
# regenerates MATRIX.md after each one. Continues past a FAIL so the battery
# makes progress; failures are diagnosed and re-run out of band and then marked.
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TS="${BATTERY_TS:-$(date -u +%Y%m%dT%H%M%SZ)}"; R="e2e-results/${BATTERY_TAG:-167}-$TS"; mkdir -p "$R/logs"
TSV="$R/results.tsv"; [[ -f "$TSV" ]] || printf 'iter\ttarget\tstatus\tsecs\tlog\n' > "$TSV"
BASE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES)

# ── Preflight: build the completeness tool BEFORE any long run ──────────────
# shellcheck source=scripts/lib-tool-preflight.sh
. "$PWD/scripts/lib-tool-preflight.sh"
preflight_bridge_out_tool "$R/logs" || exit 1


matrix() { "$PWD/scripts/e2e-battery-matrix.py" "$TSV" > "$R/MATRIX.md" 2>/dev/null; }

# Scenarios that write durable failure evidence put it here (see
# lib-quiesce.sh's QUIESCE_EVIDENCE_DIR).
export BATTERY_RESULTS_DIR="$PWD/$R"

# GROWING CHAIN (default). One genesis for the whole battery: every target runs
# against the accumulated history, so the from-genesis restores rebuild an
# ever-larger chain and the drills are actually testing scale. Set
# KEEP_CHAIN=0 to go back to wiping genesis before every target.
KEEP_CHAIN="${KEEP_CHAIN:-1}"
# Matrix phase label. The growing-chain run is a DIFFERENT experiment from the
# per-target-fresh-chain iterations that came before it, so it gets its own
# columns ("grow N") rather than overwriting evidence that was honestly
# collected under the old shape. Defaults to the growing-chain prefix because
# that is now the driver's default mode.
ITER_PREFIX="${ITER_PREFIX:-g}"
export KEEP_CHAIN
BASE_ENV+=("KEEP_CHAIN=$KEEP_CHAIN")

# `make e2e-down` is `compose down -v` — it deletes the node_data volume and
# with it the chain. Under KEEP_CHAIN it must never run: `e2e-clean-data` no
# longer touches the volume, so the teardown that used to be required before it
# is required no longer.
down() {
    if [[ "$KEEP_CHAIN" == "1" ]]; then
        echo "[$(date -u +%H:%M:%SZ)] (teardown skipped — KEEP_CHAIN=1)" >>"$R/logs/down.log"
        return 0
    fi
    "${BASE_ENV[@]}" make e2e-down >>"$R/logs/down.log" 2>&1
}

# Chain growth is the point of the run, so it is recorded, not assumed. Called
# at every drill and at each iteration boundary.
chain_mark() { # $1 = label
    local pg proxy line
    proxy="$(docker ps --format '{{.Names}}' | grep -E -- '-miden-agglayer-1$' | head -1)"
    [[ -n "$proxy" ]] || { echo -e "$1\t(no proxy running)" >> "$R/chain-growth.tsv"; return 0; }
    pg="${proxy%-miden-agglayer-1}-agglayer-postgres-1"
    line="$(docker exec "$pg" psql -U agglayer -d agglayer_store -tAc \
        "SELECT (SELECT latest_block_number FROM service_state WHERE id=1) || E'\t' ||
                (SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%') || E'\t' ||
                (SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x50178120%') || E'\t' ||
                (SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%')" 2>/dev/null | tr -d '\r')"
    [[ -n "$line" ]] || line=$'\t\t\t'
    printf '%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$1" "$line" >> "$R/chain-growth.tsv"
    echo "  chain: $1 -> tip/UHC/Bridge/Claim = $(tr '\t' '/' <<<"$line")" | tee -a "$R/battery.log"
}
[[ -f "$R/chain-growth.tsv" ]] || printf 'when\tmark\tsynthetic_tip\tUHC\tBridge\tClaim\n' > "$R/chain-growth.tsv"

# POST-MORTEM — run on EVERY failure, while the stack is still up.
#
# `make e2e-down` is `docker compose down -v`: it deletes the postgres volume,
# and with it every row that could explain what happened. Iteration 1's
# post-chaos drill failed on "PREPARED handoffs=1" at 17:35:29 and the
# containers were recreated at 17:36:15 — 46 seconds — so the handoff's tx
# hash, note id, expiration block and owner status were gone before anyone
# could look. This runs BEFORE the next teardown, unconditionally, and is
# entirely best-effort: a dead stack must produce a short file, never an error
# that masks the failure being recorded.
post_mortem() { # $1 = iteration, $2 = label
    local out="$R/logs/${ITER_PREFIX:-i}${1}-${2}-postmortem.txt"
    local proxy pg
    proxy="$(docker ps --format '{{.Names}}' | grep -E -- '-miden-agglayer-1$' | head -1)"
    pg="${proxy%-miden-agglayer-1}-agglayer-postgres-1"
    {
        echo "post-mortem for i$1 $2 — $(date -u +%FT%TZ)"
        echo; echo "== containers =="
        docker ps -a --format '{{.Names}}\t{{.Status}}' | grep -E '^miden-agglayer-' || true
        if [[ -z "$proxy" ]]; then
            echo; echo "(no proxy container running — nothing further to capture)"
        else
            echo; echo "== service_state =="
            docker exec "$pg" psql -U agglayer -d agglayer_store -c \
                "SELECT projector_cursor, reconcile_cursor, latest_block_number, nonce_ledger_rebuilt FROM service_state WHERE id=1" 2>&1 || true
            echo; echo "== prepared handoffs =="
            docker exec "$pg" psql -U agglayer -d agglayer_store -c \
                "SELECT l.tx_hash, l.note_id, l.prepared_expiration_block, s.reconcile_cursor, coalesce(t.status,'<no tx row>') AS owner_tx_status, round(extract(epoch from now()-l.created_at)) AS age_secs FROM tx_note_links l JOIN service_state s ON s.id=1 LEFT JOIN transactions t ON t.tx_hash = l.tx_hash WHERE l.handoff_state='prepared' ORDER BY l.created_at" 2>&1 || true
            echo; echo "== pending receipts =="
            docker exec "$pg" psql -U agglayer -d agglayer_store -c \
                "SELECT tx_hash, status, error_message FROM transactions WHERE status='pending' ORDER BY created_at" 2>&1 || true
            echo; echo "== parked txns =="
            docker exec "$pg" psql -U agglayer -d agglayer_store -c \
                "SELECT signer, nonce, tx_hash FROM queued_txns ORDER BY created_at" 2>&1 || true
            echo; echo "== proxy /metrics (writer + recovery) =="
            curl -sf --max-time 5 http://localhost:8546/metrics 2>/dev/null \
                | grep -E '^(agglayer_writer_|pending_unlinked|stranded_prepared)' || true
            echo; echo "== proxy log tail =="
            docker logs --tail 200 "$proxy" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' || true
        fi
    } > "$out" 2>&1
    echo "  post-mortem: $out" | tee -a "$R/battery.log"
}

# run <iter> <label> <keep|fresh> <command...>
run() {
    local iter="$1" label="$2" mode="$3"; shift 3
    local log="$R/logs/${ITER_PREFIX:-i}${iter}-${label}.log" t0 t1 rc
    [[ "$mode" == fresh ]] && down
    echo "[$(date -u +%H:%M:%SZ)] ${ITER_PREFIX:-i}$iter $label START" | tee -a "$R/battery.log"
    t0=$(date +%s)
    "${BASE_ENV[@]}" "$@" > "$log" 2>&1; rc=$?
    t1=$(date +%s)
    local st=PASS; (( rc != 0 )) && st=FAIL
    printf '%s\t%s\t%s\t%s\t%s\n' "${ITER_PREFIX:-}$iter" "$label" "$st" "$((t1-t0))" "$log" >> "$TSV"
    matrix
    echo "[$(date -u +%H:%M:%SZ)] ${ITER_PREFIX:-i}$iter $label $st rc=$rc ($((t1-t0))s)" | tee -a "$R/battery.log"
    # Capture state BEFORE anything tears it down. The very next `run` with
    # mode=fresh calls `down`, which is `compose down -v`.
    [[ "$st" == FAIL ]] && post_mortem "$iter" "$label"
    return 0
}

# scenario targets that provision their own stack (need a teardown first)
FRESH_TARGETS=(
  e2e-l1-to-l2 e2e-claim-watcher e2e-claim-watcher-synthesis
  e2e-l2-to-l1 e2e-l2-to-l1-autoclaim e2e-restore
  e2e-cantina6-faucet-identity-restore e2e-cantina10
  e2e-cantina12-getlogs-returns-all e2e-cantina13
  e2e-ger-decomposition e2e-security e2e-fuzz
  e2e-reconciler-private-note e2e-reconciler-cursor-persistence
  e2e-rd913-restart-burn-collision e2e-rd940
)

# ITER_START lets a stopped battery resume without renumbering: the matrix keys
# on the iteration id, so restarting at 1 would overwrite completed columns.
# THE ONLY WIPE IN THE RUN. Everything after this shares one genesis, one
# node_data volume, one anvil L1 and the same bridge/faucet accounts.
if [[ "${ITER_START:-1}" == "1" ]]; then
  echo "=== one-time genesis wipe before iteration 1 ===" | tee -a "$R/battery.log"
  WIPE_ENV=(env -u WITH_WEB3SIGNER -u EXTRA_COMPOSE_FILES KEEP_CHAIN=0)
  "${WIPE_ENV[@]}" make e2e-down       >>"$R/logs/down.log" 2>&1 || true
  "${WIPE_ENV[@]}" make e2e-clean-data >>"$R/logs/down.log" 2>&1
else
  echo "=== resuming at iteration ${ITER_START} — chain preserved, no wipe ===" | tee -a "$R/battery.log"
fi

for iter in $(seq "${ITER_START:-1}" "${ITERATIONS:-4}"); do
  echo "=== ITERATION $iter start $(date -u +%FT%TZ) ===" | tee -a "$R/battery.log"

  # (a) full suite — self-contained (brings up, tests, tears down)
  run "$iter" "test-e2e" fresh make test-e2e

  # (b) scenario targets
  for t in "${FRESH_TARGETS[@]}"; do
      run "$iter" "$t" fresh make "$t"
      # e2e-claim-provenance needs a stack ALREADY up; l1-to-l2 leaves one
      if [[ "$t" == e2e-l1-to-l2 ]]; then
          run "$iter" "e2e-claim-provenance" keep make e2e-claim-provenance
      fi
  done
  # l2l2 group: bring up its own stack, then run the group
  run "$iter" "e2e-l2l2-up" fresh make e2e-l2l2-up
  run "$iter" "e2e-l2l2"    keep  make e2e-l2l2
  # recovery-readiness is DESTRUCTIVE and provisions via e2e-l2l2-up
  run "$iter" "e2e-recovery-readiness" fresh make e2e-recovery-readiness

  # (c) full-DB-loss drill on a stack carrying real round-trip state
  run "$iter" "fixture-l2-to-l1" fresh make e2e-l2-to-l1
  chain_mark "i$iter before full-db-loss-recovery"
  run "$iter" "full-db-loss-recovery" keep ./scripts/e2e-full-db-loss-recovery.sh
  chain_mark "i$iter after full-db-loss-recovery"

  # (d) load + completeness on that stack
  run "$iter" "loadtest-N30" keep env N=30 ./scripts/e2e-bridge-loadtest-isolated.sh
  run "$iter" "verify-event-completeness" keep ./scripts/verify-event-completeness.sh

  # (e) chaos, then the drill again on the messy post-chaos state
  run "$iter" "chaos-soak" keep env N=30 CHAOS_DURATION=300 GARBO_DURATION=300 ./scripts/e2e-chaos-soak.sh
  # The post-chaos drill runs on a store that ALREADY carries the output of the
  # pre-chaos drill, so `nonce_ledger_rebuilt` is set and the baseline-provenance
  # guard would refuse it. Accept that explicitly: this run is a recovery from a
  # MESSY history (a restore, then 30 bridges of load, then chaos), and the
  # guard's own warning about restore-vs-restore idempotence is printed into the
  # log so the verdict is never read as a fresh fidelity comparison. The
  # fidelity claim is made by `full-db-loss-recovery` above, on a live baseline.
  chain_mark "i$iter before full-db-loss-recovery-postchaos"
  run "$iter" "full-db-loss-recovery-postchaos" keep env ALLOW_RESTORED_BASELINE=1 ./scripts/e2e-full-db-loss-recovery.sh
  chain_mark "i$iter after full-db-loss-recovery-postchaos"

  chain_mark "i$iter END"
  echo "=== ITERATION $iter done $(date -u +%FT%TZ) ===" | tee -a "$R/battery.log"
done
echo "BATTERY COMPLETE $(date -u +%FT%TZ)" | tee -a "$R/battery.log"
