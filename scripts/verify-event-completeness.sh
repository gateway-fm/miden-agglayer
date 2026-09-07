#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Independent event-completeness verifier.
#
# Cross-checks TWO independent sources:
#   • TRUTH:  the miden-node's own DB (notes table) — every consumed note,
#     classified by canonical script root (B2AGG / CLAIM / UpdateGer) with the
#     bridge as consumer (reclaims and foreign consumers excluded).
#   • VIEW:   eth_getLogs on the proxy's synthetic L2 RPC — BridgeEvent /
#     ClaimEvent / UpdateHashChainValue logs.
#
# Verifies, per event type:
#   1. COUNT    — one log per consumed note (no missing, no extra).
#   2. BLOCK    — the log sits at EXACTLY the note's consumption block
#     (synthetic block N == Miden block N). Logs at a later block (the
#     projector's late-consumption sweep / reconciler recovery) are counted
#     as LATE — present but not on time.
#
# Exit: 0 = PASS (all present at the right block; LATE allowed only with
# ALLOW_LATE=1), 1 = FAIL. Prints a per-type verdict table.
#
# Requires: the stack up; target/debug/bridge-out-tool (built) for the
# canonical script roots. No writes anywhere — read-only.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

L2_RPC="${L2_RPC:-http://localhost:8546}"
NODE_CONTAINER="${NODE_CONTAINER:-miden-agglayer-miden-node-1}"
AGGLAYER_CONTAINER="${AGGLAYER_CONTAINER:-miden-agglayer-miden-agglayer-1}"
PG_HOST="${PG_HOST:-localhost}"; PG_PORT="${PG_PORT:-5434}"
PG_USER="${PG_USER:-agglayer}"; PG_PASS="${PG_PASS:-agglayer}"; PG_DB="${PG_DB:-agglayer_store}"
ALLOW_LATE="${ALLOW_LATE:-0}"

TMP="$(mktemp -d)"
# The client-store snapshot below briefly PAUSES the proxy; the trap guarantees
# unpause on every exit path (an accidentally-left-paused proxy is an outage) —
# but ONLY for a pause THIS script acquired (CPAUSED=1): unconditionally
# unpausing would remove someone else's intentional pause (review 0814e nit).
CPAUSED=0
cleanup() {
    if [ "${CPAUSED:-0}" = "1" ]; then
        docker unpause "$AGGLAYER_CONTAINER" >/dev/null 2>&1 || true
        CPAUSED=0
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

# 1. Canonical script roots from the same crates the proxy is built from.
# shellcheck source=scripts/lib-tool-preflight.sh
. "$SCRIPT_DIR/lib-tool-preflight.sh"
preflight_bridge_out_tool || exit 1
"$TOOL_BIN" --print-script-roots --store-dir /tmp --node-url http://x > "$TMP/roots" \
    || { echo "FAIL: --print-script-roots failed"; exit 1; }
B2AGG_ROOT=$(awk -F= '$1=="b2agg"{print $2}' "$TMP/roots")
CLAIM_ROOT=$(awk -F= '$1=="claim"{print $2}' "$TMP/roots")
GER_ROOT=$(awk -F= '$1=="ger"{print $2}' "$TMP/roots")
[[ -n "$B2AGG_ROOT" && -n "$CLAIM_ROOT" && -n "$GER_ROOT" ]] || { echo "FAIL: could not parse script roots"; exit 1; }

# 2. Bridge account id (consumer gate). Overridable: after an in-place
#    upgrade the CURRENT container never deployed the bridge (its predecessor
#    did), so the log grep comes up empty — pass BRIDGE_ID explicitly then
#    (recover it from bridge_accounts.toml or the node DB).
BRIDGE_ID="${BRIDGE_ID:-$(docker logs "$AGGLAYER_CONTAINER" 2>&1 | grep -oE "deploying bridge account 0x[0-9a-f]+" | head -1 | awk '{print $NF}')}"
# Self-heal when the id is absent (recreated/upgraded container never logged the
# deployment) or bech32 (harness shells export the toml form for miden tooling):
# derive the HEX id from the node DB — the bridge is the target of every consumed
# B2AGG note. Requires traffic to exist, which any completeness run implies.
if [[ ! "$BRIDGE_ID" =~ ^0x[0-9a-fA-F]+$ ]]; then
    docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3 > "$TMP/bid.sqlite3" 2>/dev/null
    docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3-wal > "$TMP/bid.sqlite3-wal" 2>/dev/null || rm -f "$TMP/bid.sqlite3-wal"
    BRIDGE_ID=$(python3 - "$TMP/bid.sqlite3" "$B2AGG_ROOT" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
r = c.execute("SELECT hex(target_account_id) FROM notes WHERE consumed_at IS NOT NULL "
              "AND hex(script_root)=upper(?) LIMIT 1", (sys.argv[2][2:] if sys.argv[2].startswith('0x') else sys.argv[2],)).fetchone()
print('0x' + r[0].lower() if r and r[0] else '')
PYEOF
)
    [[ -n "$BRIDGE_ID" ]] && echo "bridge id derived from node DB (log/env unavailable or bech32): $BRIDGE_ID"
fi
[[ "$BRIDGE_ID" =~ ^0x[0-9a-fA-F]+$ ]] || { echo "FAIL: bridge account id not resolvable (logs, env, node DB)"; exit 1; }

# 3. Snapshot the node DB (truth), then wait for the synthetic projector to
#    catch up to that snapshot before reading logs. GER injections flow
#    CONTINUOUSLY (aggoracle), so there is no global quiescence — instead the
#    python below applies a consistency cut at the snapshot's chain tip:
#    only notes consumed ≤ tip are expected, and only logs ≤ tip can be
#    "extra" (later logs may belong to post-snapshot consumptions).
docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3 > "$TMP/node.sqlite3" \
    || { echo "FAIL: cannot snapshot node store"; exit 1; }
# WAL-aware: recent commits live in the -wal file until checkpointed; without it the
# snapshot under-counts the newest consumptions (a false-PASS direction, still wrong).
docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3-wal > "$TMP/node.sqlite3-wal" 2>/dev/null \
    || rm -f "$TMP/node.sqlite3-wal"

# An unresolvable destination is intentionally terminal without a Miden CLAIM
# note: the proxy records the exception durably and emits one ClaimEvent so
# AggKit stops retrying funds that require operator rescue. Keep these events
# strict too: match the durable record to the exact receipt block, globalIndex,
# and transaction hash instead of weakening the generic extra-log check.
PGPASSWORD="$PG_PASS" psql -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" -d "$PG_DB" \
    -t -A -F '|' -c \
    "SELECT u.global_index, COALESCE(t.block_number::text, ''), u.eth_tx_hash
       FROM unclaimable_claims u
       LEFT JOIN transactions t ON lower(t.tx_hash) = lower(u.eth_tx_hash)
      ORDER BY u.global_index" > "$TMP/unclaimable-claims" \
    || { echo "FAIL: cannot read durable unclaimable-claim records"; exit 1; }

# 3b. Deliberately-DEFERRED bridge-outs are NOT missing. The proxy refuses to emit a
#     BridgeEvent for a poisoned/unrecoverable faucet-registry row (MA#18; cantina13's
#     unrecoverable-row scenario) — recovery is via --restore, so the note stays
#     log-less on the live path BY DESIGN. Collect the refused faucet ids from the
#     proxy logs; the python reclassifies matching missing candidates to "deferred"
#     (reported, non-failing). Only an UNEXPLAINED absence fails the verdict.
#     (Root cause of two false FAILs on 2026-07-12: the post-suite chain always
#     carries exactly one such note, at the suite's cantina13 block.)
#     NOTE: strip ANSI first — tracing colors the field names, which breaks the grep.
DEFERRED_FAUCETS="${DEFERRED_FAUCETS:-$(docker logs "$AGGLAYER_CONTAINER" 2>&1 \
    | sed -e 's/\x1b\[[0-9;]*m//g' \
    | grep -aiE "refusing to emit|unrecoverable" \
    | grep -aoE "faucet_id: 0x[0-9a-f]+" | awk '{print $2}' | sed 's/^0x//' | sort -u | tr '\n' ' ')}"

# BARRIER-AWARE SETTLE (vb #30). The visibility barrier holds synthetic
# projection at project_to = min(tip, reconcile_cursor), so under load a note
# consumed at/below the snapshot tip may not be SEALED yet — its BridgeEvent
# then reads as MISSING though the barrier is working exactly as designed
# (0 late). A blind fixed sleep is a race: heavier load (N=30 vs N=20) makes the
# reconciler lag more, so the timer expires before the last note's block is
# projected. Instead, snapshot FIRST (fixes the cut), then WAIT for the projector
# to report projector_cursor >= that cut. The tip only grows, so cursor >= cut
# guarantees every note consumed <= cut has been projected (its BridgeEvent
# sealed). On timeout we fall through and count anyway, so a genuinely stuck
# barrier FAILS loud rather than hanging.
cut=$(python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute('SELECT max(block_num) FROM block_headers').fetchone()[0] or 0)" "$TMP/node.sqlite3")
catchup_timeout="${PROJECTOR_CATCHUP_TIMEOUT:-300}"; waited=0; cur=""
echo "barrier-aware settle: waiting for projector_cursor >= ${cut} (snapshot tip)…"
while [ "$waited" -lt "$catchup_timeout" ]; do
    cur=$(docker logs --tail 40 "$AGGLAYER_CONTAINER" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g' \
          | grep -oE 'projector_cursor: [0-9]+' | tail -1 | awk '{print $2}')
    if [ -n "$cur" ] && [ "$cur" -ge "$cut" ]; then
        echo "  projector reached cursor=${cur} >= ${cut} after ${waited}s"; break
    fi
    sleep 5; waited=$((waited + 5))
done
[ "$waited" -ge "$catchup_timeout" ] && \
    echo "  WARN: projector_cursor (${cur:-none}) did not reach ${cut} within ${catchup_timeout}s — counting anyway (a genuinely stuck barrier will now FAIL loud)"
# Small extra margin for eth_getLogs / synthetic-store read propagation.
sleep "${SETTLE_MARGIN_SECS:-20}"

# 4. Cross-check — the counting core lives in lib-verify-completeness.py so
#    the same-block substitution regression can drive the EXACT production
#    logic against fixtures (review 0814).
#    The PROXY CLIENT store supplies note_id -> details_commitment: runtime
#    BridgeEvent tx hashes derive from bare hex(details_commitment)
#    (project_b2agg_note), so identity-level checks need the commitment, not
#    the NoteId. Unavailable snapshot => the lib fails closed (reclaims stay
#    EXPECTED, deferred slots unmatchable).
#    POINT-IN-TIME snapshot (review 0814e): sequential file copies of a live
#    SQLite are not consistent — a write/checkpoint can interleave (the store
#    may run rollback-journal OR wal mode). PAUSE the proxy for the copy
#    window (~1s), copy main + every sidecar, then unpause — the EXIT trap
#    guarantees unpause on every failure path too.
SNAP_CONSISTENT=0
if docker pause "$AGGLAYER_CONTAINER" >/dev/null 2>&1; then
    CPAUSED=1
    SNAP_CONSISTENT=1
fi
for side in "" "-wal" "-shm" "-journal"; do
    docker cp "$AGGLAYER_CONTAINER:/var/lib/miden-agglayer-service/store.sqlite3$side" \
        "$TMP/client.sqlite3$side" 2>/dev/null || rm -f "$TMP/client.sqlite3$side"
done
if [ "$CPAUSED" = "1" ]; then
    # The normal-path unpause must SUCCEED — continuing into RPC verification
    # against a still-paused proxy judges a frozen server. Abort loudly (the
    # EXIT trap retries the unpause once more on the way out).
    if docker unpause "$AGGLAYER_CONTAINER" >/dev/null 2>&1; then
        CPAUSED=0
    else
        echo "FAIL: could not unpause $AGGLAYER_CONTAINER after the client snapshot — refusing to verify against a paused proxy"
        exit 1
    fi
fi
if [ "$SNAP_CONSISTENT" != "1" ]; then
    # Could not pause => the copy is not point-in-time; drop it so the lib
    # fails closed (UNRESOLVED-RECLAIM) instead of judging on a torn snapshot.
    echo "WARN: could not pause $AGGLAYER_CONTAINER for a consistent client snapshot — identity checks will fail closed"
    rm -f "$TMP/client.sqlite3" "$TMP/client.sqlite3-wal" "$TMP/client.sqlite3-shm" "$TMP/client.sqlite3-journal"
fi
export TOOL_BIN
python3 "$SCRIPT_DIR/lib-verify-completeness.py" \
    "$TMP/node.sqlite3" "$L2_RPC" "$BRIDGE_ID" "$B2AGG_ROOT" "$CLAIM_ROOT" "$GER_ROOT" \
    "$ALLOW_LATE" "$DEFERRED_FAUCETS" "$TMP/unclaimable-claims" "$TMP/client.sqlite3"
