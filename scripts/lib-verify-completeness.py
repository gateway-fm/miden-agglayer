#!/usr/bin/env python3
# Core of scripts/verify-event-completeness.sh — extracted to a file so the
# same-block substitution regression (scripts/test-verify-completeness-substitution.sh)
# can drive the EXACT production counting logic against fixtures (review 0814).
#
# argv: node_sqlite l2_rpc bridge_id b2agg_root claim_root ger_root allow_late \
#       deferred_faucets unclaimable_file
# env:  TOOL_BIN — bridge-out-tool, used to derive the canonical synthetic
#       bridge-out tx hash for reclaimed NoteIds (forbidden-log detection).
import json
import os
import sqlite3
import subprocess
import sys
import urllib.request
from collections import Counter

db, rpc, bridge_id, b2agg_root, claim_root, ger_root, allow_late = sys.argv[1:8]
# Faucets whose bridge-outs the proxy DELIBERATELY refused to emit (poisoned/
# unrecoverable registry rows). Lower-case hex, no 0x.
deferred_faucets = set((sys.argv[8] if len(sys.argv) > 8 else "").lower().split())
unclaimable_file = sys.argv[9]
bridge_hex = bridge_id[2:].upper()
tool_bin = os.environ.get("TOOL_BIN", "")

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


def derive_forbidden_hashes(note_ids_hex):
    """The EXACT synthetic tx hash the proxy would serve a BridgeEvent under,
    from the shipped derivation (bridge-out-tool wraps
    bridge_out::derive_bridge_out_tx_hash). Both key forms are derived: the
    modern NoteId key (`0x…`, lowercase) and the legacy commitment-hex key.
    Fail-closed: if the tool cannot derive, the caller keeps the notes
    EXPECTED instead of exempting them blind."""
    if not note_ids_hex or not tool_bin:
        return None if note_ids_hex else set()
    keys = []
    for i in note_ids_hex:
        keys.append("0x" + i.lower())
        keys.append(i.lower())
    try:
        out = subprocess.run(
            [tool_bin, "--store-dir", "/tmp", "--node-url", "http://x",
             "--derive-bridge-out-tx-hash", *keys],
            capture_output=True, text=True, timeout=60, check=True).stdout
    except Exception as e:  # noqa: BLE001 — any tool failure must fail closed
        print(f"    WARN: forbidden-hash derivation failed ({e}) — reclaims stay EXPECTED (fail-closed)")
        return None
    hashes = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            hashes.add(parts[1].lower())
    return hashes


tip = int(rpc_call("eth_blockNumber", []), 16)
# Defense vs stale-tip bugs (postmortem 2026-07-04): never let a lagging
# eth_blockNumber truncate the scan window below the node snapshot's tip.
_n = sqlite3.connect(db)
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


n = sqlite3.connect(db)
n.row_factory = sqlite3.Row
unclaimable = []
with open(unclaimable_file, encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        gi, block, tx_hash = line.split("|", 2)
        unclaimable.append((int(gi, 0), int(block) if block else None, tx_hash.lower()))
# Consistency cut: the node snapshot's own chain tip. Only notes consumed at or
# before the cut are expected; only logs at or before the cut can be "extra"
# (later logs may belong to consumptions that happened after the snapshot).
cut = n.execute("SELECT max(block_num) FROM block_headers").fetchone()[0] or 0
overall_fail = False
total_notes = 0
total_logs = 0
print(f"consistency cut: node snapshot tip = block {cut}")
print(f"{'TYPE':<22} {'notes':>6} {'logs':>6} {'exact':>6} {'late':>5} {'missing':>8} {'defer':>6} {'unclaim':>8} {'forbid':>6} {'extra':>6}  verdict")
print("-" * 104)
for name, (topic, root) in TOPICS.items():
    # Release-gate hardening (review 0814): resolve each note's canonical
    # consumer BEFORE building the expected set. A bridge-targeted note consumed
    # by a NON-bridge transaction (sender reclaim of a timed-out bridge-out)
    # must not be EXPECTED. An UNRESOLVED consumer stays expected (fail-closed).
    rows = list(n.execute(
        "SELECT consumed_at, hex(note_id) i, hex(nullifier) nf FROM notes "
        "WHERE script_root=? AND consumed_at IS NOT NULL "
        "AND consumed_at<=? AND hex(target_account_id)=?",
        (bytes.fromhex(root[2:]), cut, bridge_hex)))
    reclaimed_ids = []
    forbidden = set()
    if name == "B2AGG->BridgeEvent":
        expected_rows = []
        for r in rows:
            consumer = None
            if r["nf"]:
                tx = n.execute(
                    "SELECT hex(account_id) acct FROM transactions "
                    "WHERE block_num=? AND hex(input_notes) LIKE '%' || ? || '%'",
                    (r["consumed_at"], r["nf"])).fetchone()
                consumer = tx["acct"] if tx else None
            if consumer is not None and consumer != bridge_hex:
                reclaimed_ids.append(r["i"])
                print(f"    RECLAIMED (consumed by 0x{consumer.lower()}, not the bridge — "
                      f"no deposit/event expected): note 0x{r['i'].lower()} consumed_at={r['consumed_at']}")
            else:
                expected_rows.append(r)
        # Counter(block) alone cannot see SAME-BLOCK SUBSTITUTION: one missing
        # legit note + one wrongly-emitted reclaimed note in the same block
        # cancel out count-wise. Derive each reclaim's canonical synthetic tx
        # hash and HARD-FAIL on any served log carrying it — identity-level,
        # not count-level. If derivation is unavailable the reclaims stay
        # EXPECTED (fail-closed: they then surface as MISSING, never absorb).
        forbidden = derive_forbidden_hashes(reclaimed_ids)
        if forbidden is None:
            forbidden = set()  # derivation unavailable: every row stays expected
        else:
            rows = expected_rows
    note_blocks = Counter(r["consumed_at"] for r in rows)
    expected_ids = {r["i"] for r in rows}
    logs = get_logs(topic)
    all_logs_count = sum(1 for l in logs if int(l["blockNumber"], 16) <= cut)
    forbidden_ct = 0
    if name == "B2AGG->BridgeEvent" and forbidden:
        flagged = [l for l in logs if l["transactionHash"].lower() in forbidden]
        for l in flagged:
            forbidden_ct += 1
            print(f"    FORBIDDEN: BridgeEvent served for a RECLAIMED note "
                  f"(tx {l['transactionHash']} block {int(l['blockNumber'], 16)}) — "
                  f"a non-deposit must never emit")
        logs = [l for l in logs if l["transactionHash"].lower() not in forbidden]

    # ClaimEvents for durable unclaimable records have no corresponding Miden
    # CLAIM note. Match and remove only their exact (block, GI, tx-hash) logs;
    # every other surplus ClaimEvent remains an error.
    unclaim_exact = 0
    unclaim_missing = 0
    if name == "CLAIM->ClaimEvent":
        expected = Counter(
            (block, gi, tx_hash)
            for gi, block, tx_hash in unclaimable
            if block is not None and block <= cut
        )
        unclaim_missing += sum(1 for _, block, _ in unclaimable if block is None)
        kept = []
        for log in logs:
            block = int(log["blockNumber"], 16)
            data = log.get("data", "")
            gi = int((data[2:66] or "0"), 16)
            key = (block, gi, log["transactionHash"].lower())
            if expected[key] > 0:
                expected[key] -= 1
                unclaim_exact += 1
            else:
                kept.append(log)
        unclaim_missing += sum(expected.values())
        logs = kept
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
    # Review 0814: candidates are restricted to the FILTERED expected set — a
    # reclaimed (excluded) note must never soak up a deferred slot.
    deferred = 0
    if missing > 0:
        unmatched = note_blocks - log_blocks
        det = list(n.execute(
            "SELECT hex(note_id) i, consumed_at b, hex(assets) a FROM notes WHERE script_root=? AND consumed_at IS NOT NULL "
            "AND consumed_at<=? AND hex(target_account_id)=? ORDER BY consumed_at",
            (bytes.fromhex(root[2:]), cut, bridge_hex)))
        for r in det:
            if r["i"] not in expected_ids:
                continue
            if unmatched.get(r["b"], 0) > 0:
                # asset faucet id: 15 bytes after the 2-byte assets prefix
                fauc = (r["a"] or "")[4:34].lower()
                if fauc and fauc in deferred_faucets and deferred < missing:
                    deferred += 1
                    unmatched[r["b"]] -= 1
                    print(f"    DEFERRED (deliberate emit refusal, recovery via --restore): "
                          f"note 0x{r['i'].lower()} consumed_at={r['b']} faucet={fauc}")
                else:
                    print(f"    MISSING candidate: note 0x{r['i'].lower()} consumed_at={r['b']}")
        missing -= deferred
    total_notes += n_notes
    total_logs += all_logs_count
    ok = (missing == 0 and extra == 0 and unclaim_missing == 0 and forbidden_ct == 0
          and (late == 0 or allow_late == "1"))
    overall_fail |= not ok
    unclaim_col = f"{unclaim_exact}/{unclaim_exact + unclaim_missing}" if name == "CLAIM->ClaimEvent" else "-"
    forbid_col = str(forbidden_ct) if name == "B2AGG->BridgeEvent" else "-"
    print(f"{name:<22} {n_notes:>6} {all_logs_count:>6} {exact:>6} {late:>5} {missing:>8} {deferred:>6} {unclaim_col:>8} {forbid_col:>6} {extra:>6}  {'PASS' if ok else 'FAIL'}")

print("-" * 104)
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
    print(f"Check NODE_CONTAINER ({db!r} snapshot), BRIDGE_ID ({bridge_id}), and that the run produced traffic.")
    sys.exit(2)
print("VERDICT:", "FAIL" if overall_fail else "PASS",
      "(exact = log at the note's consumption block; late = present but later block)")
sys.exit(1 if overall_fail else 0)
