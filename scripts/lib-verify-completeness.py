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
# Proxy client store (note_id -> details_commitment). Runtime BridgeEvent tx
# hashes derive from bare hex(details_commitment) (project_b2agg_note), so the
# identity checks below need the commitment. Missing/unreadable => fail closed.
client_db = sys.argv[10] if len(sys.argv) > 10 else ""
bridge_hex = bridge_id[2:].upper()
tool_bin = os.environ.get("TOOL_BIN", "")

_client = None
if client_db and os.path.exists(client_db):
    _client = sqlite3.connect(client_db)


def _norm_hex(v):
    """BLOB or TEXT ('0x…'/bare) column value -> bare lowercase hex, or None."""
    if v is None:
        return None
    if isinstance(v, (bytes, bytearray)):
        return bytes(v).hex()
    v = str(v).lower()
    return v[2:] if v.startswith("0x") else v


def commitment_for(note_id_hex):
    """Bare lowercase hex details_commitment for a NoteId, from the proxy's
    client store. Encoding-agnostic (review 0814d): miden-client versions have
    stored these columns as BLOB or as TEXT '0x…' — match either, normalize
    either. None when unresolvable (fail-closed at the caller)."""
    if _client is None:
        return None
    want = note_id_hex.lower()
    row = _client.execute(
        "SELECT details_commitment FROM input_notes "
        "WHERE lower(hex(note_id))=? OR lower(note_id)=? OR lower(note_id)=?",
        (want, want, "0x" + want)).fetchone()
    return _norm_hex(row[0]) if row else None

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


def derive_tx_hashes(keys):
    """key -> the EXACT synthetic tx hash the proxy serves a BridgeEvent under,
    via the shipped derivation (bridge-out-tool wraps
    bridge_out::derive_bridge_out_tx_hash). Runtime keys are BARE lowercase
    hex(details_commitment) (project_b2agg_note). Fail-closed: any tool failure
    or INCOMPLETE output (fewer lines than keys) returns None and the caller
    must not exempt anything."""
    if not keys:
        return {}
    if not tool_bin:
        return None
    try:
        out = subprocess.run(
            [tool_bin, "--store-dir", "/tmp", "--node-url", "http://x",
             "--derive-bridge-out-tx-hash", *keys],
            capture_output=True, text=True, timeout=60, check=True).stdout
    except Exception as e:  # noqa: BLE001 — any tool failure must fail closed
        print(f"    WARN: tx-hash derivation failed ({e}) — fail-closed")
        return None
    mapping = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            mapping[parts[0]] = parts[1].lower()
    if len(mapping) != len(set(keys)):
        print(f"    WARN: tx-hash derivation INCOMPLETE ({len(mapping)}/{len(set(keys))}) — fail-closed")
        return None
    return mapping


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
    unresolved_reclaims = 0
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
        # hash — from its DETAILS COMMITMENT, the exact runtime key
        # (project_b2agg_note) — and HARD-FAIL on any served log carrying it.
        # A reclaim whose commitment or hash cannot be resolved stays EXPECTED
        # (fail-closed: it surfaces as MISSING, never absorbs a wrong log).
        reclaim_commits = {}
        for i in reclaimed_ids:
            commit = commitment_for(i)
            if commit is None:
                unresolved_reclaims += 1
            else:
                reclaim_commits[i] = commit
        derived = derive_tx_hashes(sorted(set(reclaim_commits.values())))
        if derived is None:
            unresolved_reclaims += len(reclaim_commits)
            reclaim_commits = {}
            derived = {}
        if unresolved_reclaims:
            # Review 0814d: re-adding an unresolved reclaim to EXPECTED lets
            # its own wrongly-emitted BridgeEvent satisfy that synthetic
            # expectation — a fail-open disguised as fail-closed. An identity
            # the verifier cannot resolve is an EXPLICIT verifier failure.
            print(f"    UNRESOLVED-RECLAIM: {unresolved_reclaims} reclaimed note(s) have no "
                  f"resolvable runtime tx hash (client-store snapshot missing/stale or "
                  f"derivation failed) — the verifier cannot certify this run")
        forbidden = {derived[c] for c in reclaim_commits.values()}
        rows = expected_rows
    note_blocks = Counter(r["consumed_at"] for r in rows)
    expected_ids = {r["i"] for r in rows}
    logs = get_logs(topic)
    served_tx_hashes = {l["transactionHash"].lower() for l in logs}
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
        det_commits = {r["i"]: commitment_for(r["i"]) for r in det if r["i"] in expected_ids}
        det_hashes = derive_tx_hashes(sorted({c for c in det_commits.values() if c}))
        for r in det:
            if r["i"] not in expected_ids:
                continue
            # Identity gate (review 0814c): a candidate whose RUNTIME tx hash is
            # served is EMITTED — it is neither missing nor deferrable, so it
            # must not soak up a missing/deferred slot that belongs to another
            # note in the same block (the deferred-substitution false green).
            # Unresolvable commitment/hash => fail-closed: the note cannot be
            # DEFERRED (only reported MISSING).
            commit = det_commits.get(r["i"])
            r_hash = det_hashes.get(commit) if (det_hashes and commit) else None
            if name == "B2AGG->BridgeEvent" and r_hash is not None and r_hash in served_tx_hashes:
                continue
            if unmatched.get(r["b"], 0) > 0:
                # asset faucet id: 15 bytes after the 2-byte assets prefix
                fauc = (r["a"] or "")[4:34].lower()
                identity_ok = name != "B2AGG->BridgeEvent" or r_hash is not None
                if fauc and fauc in deferred_faucets and identity_ok and deferred < missing:
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
          and (name != "B2AGG->BridgeEvent" or unresolved_reclaims == 0)
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
