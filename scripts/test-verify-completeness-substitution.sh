#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Same-block SUBSTITUTION regressions for the completeness verifier (review 0814).
#
# Counter(block) alone cannot see either scenario:
#   A (block 50, reclaim substitution): a legitimate bridge-consumed B2AGG has
#     NO BridgeEvent while a RECLAIMED (sender-consumed) note HAS a wrongly
#     emitted one. Counts cancel — a false green.
#   B (block 60, deferred substitution): a deferred-faucet note HAS a wrongly
#     emitted BridgeEvent while a legitimate note is missing; count-wise one
#     missing remains and the deferred reclassification would absorb it.
#
# The verifier must work at IDENTITY level: runtime BridgeEvent tx hashes
# derive from bare hex(details_commitment) (project_b2agg_note), resolved via
# the proxy client store and the shipped bridge-out-tool derivation. Scenario A
# must FAIL with a FORBIDDEN line + the legit MISSING; scenario B must FAIL
# with the legit MISSING and NO deferred reclassification.
#
# Drives the EXACT production counting core (lib-verify-completeness.py)
# against fixture node/client DBs and a stub eth RPC. Requires only
# bridge-out-tool (same prerequisite as the verifier itself).
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TOOL_BIN="${TOOL_BIN:-$PROJECT_DIR/target/debug/bridge-out-tool}"
[[ -x "$TOOL_BIN" ]] || { echo "FAIL: $TOOL_BIN not built (cargo build --bin bridge-out-tool)"; exit 1; }
export TOOL_BIN

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
: > "$TMP/unclaimable"

python3 - "$SCRIPT_DIR/lib-verify-completeness.py" "$TMP" "$TOOL_BIN" <<'PY'
import http.server
import json
import sqlite3
import subprocess
import sys
import threading

lib, tmp, tool = sys.argv[1], sys.argv[2], sys.argv[3]

BRIDGE = "AA" * 15          # hex(target_account_id) / hex(account_id) form
OTHER = "BB" * 15           # the reclaiming sender's account
DEF_FAUCET = "FA" * 15      # deferred (deliberate-emit-refusal) faucet id
B2AGG_ROOT = "11" * 32
CLAIM_ROOT = "22" * 32
GER_ROOT = "33" * 32

# (name, note_id, nullifier, details_commitment, consumer, block, faucet)
NOTES = [
    ("legit_a",  "C1" * 32, "D1" * 32, "E1" * 32, BRIDGE, 50, "00" * 15),  # missing -> MUST surface
    ("reclaim",  "C2" * 32, "D2" * 32, "E2" * 32, OTHER,  50, "00" * 15),  # wrongly emitted -> FORBIDDEN
    ("legit_b",  "C3" * 32, "D3" * 32, "E3" * 32, BRIDGE, 60, "00" * 15),  # missing -> MUST surface
    ("deferred", "C4" * 32, "D4" * 32, "E4" * 32, BRIDGE, 60, DEF_FAUCET), # wrongly emitted -> not deferrable
]

db = f"{tmp}/node.sqlite3"
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE notes (note_id BLOB, nullifier BLOB, script_root BLOB,
                    target_account_id BLOB, consumed_at INT, assets BLOB);
CREATE TABLE transactions (block_num INT, account_id BLOB, input_notes BLOB);
CREATE TABLE block_headers (block_num INT);
""")
c.execute("INSERT INTO block_headers VALUES (?)", (65,))
for _, note_id, nf, _, consumer, block, faucet in NOTES:
    assets = b"\x00\x00" + bytes.fromhex(faucet) + bytes(13)
    c.execute("INSERT INTO notes VALUES (?,?,?,?,?,?)",
              (bytes.fromhex(note_id), bytes.fromhex(nf), bytes.fromhex(B2AGG_ROOT),
               bytes.fromhex(BRIDGE), block, assets))
    c.execute("INSERT INTO transactions VALUES (?,?,?)",
              (block, bytes.fromhex(consumer), bytes.fromhex(nf)))
c.commit()

# Proxy client store fixture: note_id -> details_commitment (the runtime key).
# PRIMARY fixture: BLOB columns — the pinned 0.16 miden-client schema
# (review 0814e). A TEXT '0x…' variant is also exercised below: the lib's
# join stays encoding-agnostic and BOTH encodings are covered.
cdb = f"{tmp}/client.sqlite3"
cc = sqlite3.connect(cdb)
cc.execute("CREATE TABLE input_notes (note_id BLOB, details_commitment BLOB)")
for _, note_id, _, commit, _, _, _ in NOTES:
    cc.execute("INSERT INTO input_notes VALUES (?,?)",
               (bytes.fromhex(note_id), bytes.fromhex(commit)))
cc.commit()
cdb_text = f"{tmp}/client-text.sqlite3"
ct = sqlite3.connect(cdb_text)
ct.execute("CREATE TABLE input_notes (note_id TEXT, details_commitment TEXT)")
for _, note_id, _, commit, _, _, _ in NOTES:
    ct.execute("INSERT INTO input_notes VALUES (?,?)",
               ("0x" + note_id.lower(), "0x" + commit.lower()))
ct.commit()


def runtime_hash(commit_hex):
    # Bare lowercase hex(details_commitment) — the exact project_b2agg_note key.
    out = subprocess.run([tool, "--store-dir", "/tmp", "--node-url", "http://x",
                          "--derive-bridge-out-tx-hash", commit_hex.lower()],
                         capture_output=True, text=True, check=True).stdout
    return out.split()[1]


B2AGG_TOPIC = "0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
LOGS = [
    {"blockNumber": hex(50), "transactionHash": runtime_hash("E2" * 32),
     "topics": [B2AGG_TOPIC], "data": "0x"},
    {"blockNumber": hex(60), "transactionHash": runtime_hash("E4" * 32),
     "topics": [B2AGG_TOPIC], "data": "0x"},
]


class Stub(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if req["method"] == "eth_blockNumber":
            result = hex(65)
        elif req["method"] == "eth_getLogs":
            topic = req["params"][0]["topics"][0]
            result = LOGS if topic == B2AGG_TOPIC else []
        else:
            result = None
        body = json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


srv = http.server.HTTPServer(("127.0.0.1", 0), Stub)
threading.Thread(target=srv.serve_forever, daemon=True).start()
rpc = f"http://127.0.0.1:{srv.server_address[1]}"

def run_verifier(client_db):
    return subprocess.run(
        ["python3", lib, db, rpc, "0x" + BRIDGE.lower(), "0x" + B2AGG_ROOT.lower(),
         "0x" + CLAIM_ROOT.lower(), "0x" + GER_ROOT.lower(), "0",
         DEF_FAUCET.lower(), f"{tmp}/unclaimable", client_db],
        capture_output=True, text=True)


run = run_verifier(cdb)
out = run.stdout
print(out)

failures = []
# The TEXT-store variant must reach the identical identity-level verdict.
run_text = run_verifier(cdb_text)
if run_text.returncode == 0 or "FORBIDDEN" not in run_text.stdout:
    failures.append("TEXT-encoded client store did not reproduce the identity verdict")
if run.returncode == 0:
    failures.append("verifier PASSED the substitutions (the exact false green)")
if "FORBIDDEN" not in out:
    failures.append("no FORBIDDEN line for the reclaimed note's wrongly-emitted BridgeEvent")
for legit in ("C1" * 32, "C3" * 32):
    if "MISSING candidate: note 0x" + legit.lower() not in out:
        failures.append(f"legit note 0x{legit.lower()[:8]}… not surfaced as MISSING")
if "RECLAIMED" not in out:
    failures.append("the reclaim was not classified")
if "DEFERRED" in out:
    failures.append("the wrongly-emitted deferred-faucet note was reclassified DEFERRED "
                    "(deferred substitution false green)")
if failures:
    print("SUBSTITUTION REGRESSION FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("SUBSTITUTION REGRESSION PASS: forbidden reclaim flagged, both legit misses surfaced, "
      "deferred substitution not absorbed")

# ── missing/incomplete client snapshot (review 0814d) ────────────────────────
run2 = subprocess.run(
    ["python3", lib, db, rpc, "0x" + BRIDGE.lower(), "0x" + B2AGG_ROOT.lower(),
     "0x" + CLAIM_ROOT.lower(), "0x" + GER_ROOT.lower(), "0",
     DEF_FAUCET.lower(), f"{tmp}/unclaimable", f"{tmp}/does-not-exist.sqlite3"],
    capture_output=True, text=True)
out2 = run2.stdout
failures = []
if run2.returncode == 0:
    failures.append("verifier PASSED with an unresolvable reclaim (missing client snapshot)")
if "UNRESOLVED-RECLAIM" not in out2:
    failures.append("no explicit UNRESOLVED-RECLAIM failure for the missing snapshot")
if failures:
    print(out2)
    print("MISSING-SNAPSHOT REGRESSION FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("MISSING-SNAPSHOT REGRESSION PASS: unresolvable reclaim fails the verifier explicitly")
PY
