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
ALLOW_LATE="${ALLOW_LATE:-0}"
TOOL_BIN="${TOOL_BIN:-$PROJECT_DIR/target/debug/bridge-out-tool}"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# 1. Canonical script roots from the same crates the proxy is built from.
[[ -x "$TOOL_BIN" ]] || { echo "FAIL: $TOOL_BIN not built (cargo build --bin bridge-out-tool)"; exit 1; }
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

# 4. Cross-check.
python3 - "$TMP/node.sqlite3" "$L2_RPC" "$BRIDGE_ID" "$B2AGG_ROOT" "$CLAIM_ROOT" "$GER_ROOT" "$ALLOW_LATE" "$DEFERRED_FAUCETS" <<'PY'
import json, sqlite3, sys, urllib.request
from collections import Counter

db, rpc, bridge_id, b2agg_root, claim_root, ger_root, allow_late = sys.argv[1:8]
# Faucets whose bridge-outs the proxy DELIBERATELY refused to emit (poisoned/
# unrecoverable registry rows — see step 3b in the shell). Lower-case hex, no 0x.
deferred_faucets = set((sys.argv[8] if len(sys.argv) > 8 else "").lower().split())
bridge_hex = bridge_id[2:].upper()

TOPICS = {
    "B2AGG->BridgeEvent":  ("0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b", b2agg_root),
    "CLAIM->ClaimEvent":   ("0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d", claim_root),
    "GER->UpdateHashChain":("0x65d3bf36615f1f02a134d12dfa9ea6b1d4a52386e825973cd27ddb70895c2319", ger_root),
}

def rpc_call(method, params):
    req = urllib.request.Request(rpc, json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        resp = json.load(r)
    if "error" in resp:
        raise RuntimeError(f"{method}: {resp['error']}")
    return resp["result"]

tip = int(rpc_call("eth_blockNumber", []), 16)
# Defense vs stale-tip bugs (postmortem 2026-07-04): never let a lagging
# eth_blockNumber truncate the scan window below the node snapshot's tip.
_n = sqlite3.connect(sys.argv[1])
_cut = _n.execute("SELECT max(block_num) FROM block_headers").fetchone()[0] or 0
_n.close()
tip = max(tip, _cut)

def get_logs(topic0):
    # Full range in one call; chunk on failure (range caps).
    try:
        return rpc_call("eth_getLogs", [{"fromBlock": "0x0", "toBlock": hex(tip), "topics": [topic0]}])
    except Exception:
        logs, step = [], 500
        for start in range(0, tip + 1, step):
            end = min(start + step - 1, tip)
            logs += rpc_call("eth_getLogs", [{"fromBlock": hex(start), "toBlock": hex(end), "topics": [topic0]}])
        return logs

n = sqlite3.connect(db); n.row_factory = sqlite3.Row
# Consistency cut: the node snapshot's own chain tip. Only notes consumed at or
# before the cut are expected; only logs at or before the cut can be "extra"
# (later logs may belong to consumptions that happened after the snapshot).
cut = n.execute("SELECT max(block_num) FROM block_headers").fetchone()[0] or 0
overall_fail = False
total_notes = 0
total_logs = 0
print(f"consistency cut: node snapshot tip = block {cut}")
print(f"{'TYPE':<22} {'notes':>6} {'logs':>6} {'exact':>6} {'late':>5} {'missing':>8} {'defer':>6} {'extra':>6}  verdict")
print("-" * 85)
for name, (topic, root) in TOPICS.items():
    rows = list(n.execute(
        "SELECT consumed_at FROM notes WHERE script_root=? AND consumed_at IS NOT NULL "
        "AND consumed_at<=? AND hex(target_account_id)=?",
        (bytes.fromhex(root[2:]), cut, bridge_hex)))
    note_blocks = Counter(r["consumed_at"] for r in rows)
    logs = get_logs(topic)
    all_log_blocks = [int(l["blockNumber"], 16) for l in logs]
    log_blocks = Counter(all_log_blocks)            # all logs (for exact/late matching)
    cut_log_blocks = Counter(b for b in all_log_blocks if b <= cut)  # extra-detection

    exact = sum(min(c, log_blocks.get(b, 0)) for b, c in note_blocks.items())
    n_notes = sum(note_blocks.values())
    n_logs_cut = sum(cut_log_blocks.values())
    # Unmatched notes may have LATE logs (the projector's late-consumption
    # sweep emits at a later synthetic block) — match count-wise against the
    # full log set's surplus. Anything left is genuinely missing.
    exact_cut = sum(min(c, cut_log_blocks.get(b, 0)) for b, c in note_blocks.items())
    unmatched_notes = n_notes - exact
    surplus_all = sum(log_blocks.values()) - exact
    late = min(unmatched_notes, surplus_all)
    missing = unmatched_notes - late
    extra = max(0, n_logs_cut - exact_cut - late)

    # Reclassify DELIBERATE emit refusals: a missing candidate whose asset faucet is in
    # the proxy's refused set is DEFERRED (expected on the live path; recovery is via
    # --restore), not missing. Only unexplained absences remain in `missing`.
    deferred = 0
    if missing > 0:
        unmatched = note_blocks - log_blocks
        det = list(n.execute(
            "SELECT hex(note_id) i, consumed_at b, hex(assets) a FROM notes WHERE script_root=? AND consumed_at IS NOT NULL "
            "AND hex(target_account_id)=? ORDER BY consumed_at", (bytes.fromhex(root[2:]), bridge_hex)))
        for r in det:
            if unmatched.get(r["b"], 0) > 0:
                # asset faucet id: 15 bytes after the 2-byte assets prefix
                fauc = (r["a"] or "")[4:34].lower()
                if fauc and fauc in deferred_faucets and deferred < missing:
                    deferred += 1
                    print(f"    DEFERRED (deliberate emit refusal, recovery via --restore): "
                          f"note 0x{r['i'].lower()} consumed_at={r['b']} faucet={fauc}")
                else:
                    print(f"    MISSING candidate: note 0x{r['i'].lower()} consumed_at={r['b']}")
        missing -= deferred
    total_notes += n_notes
    total_logs += n_logs_cut
    ok = missing == 0 and extra == 0 and (late == 0 or allow_late == "1")
    overall_fail |= not ok
    print(f"{name:<22} {n_notes:>6} {n_logs_cut:>6} {exact:>6} {late:>5} {missing:>8} {deferred:>6} {extra:>6}  {'PASS' if ok else 'FAIL'}")

print("-" * 85)
if total_notes == 0 and total_logs > 0:
    print("SANITY FAIL: node query matched ZERO consumed notes while logs exist —")
    print(f"almost certainly a wrong/bech32 BRIDGE_ID ({bridge_id}); pass the HEX id.")
    sys.exit(2)
if total_notes == 0:
    # Task #26 sweep: an all-zero table is NOT a pass. Zero consumed bridge
    # notes means nothing was verified — wrong NODE_CONTAINER, a bridge id
    # from a previous stack, or an empty run. A verifier that saw no data
    # must say so, not certify completeness.
    print("SANITY FAIL: zero consumed bridge notes in the node snapshot — nothing verified.")
    print(f"Check NODE_CONTAINER ({sys.argv[1]!r} snapshot), BRIDGE_ID ({bridge_id}), and that the run produced traffic.")
    sys.exit(2)
print("VERDICT:", "FAIL" if overall_fail else "PASS",
      "(exact = log at the note's consumption block; late = present but later block)")
sys.exit(1 if overall_fail else 0)
PY
