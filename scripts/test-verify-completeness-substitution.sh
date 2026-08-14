#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# Same-block SUBSTITUTION regression for the completeness verifier (review 0814).
#
# The scenario Counter(block) alone cannot see: at ONE block, a legitimate
# bridge-consumed B2AGG note has NO BridgeEvent (missing) while a RECLAIMED
# (sender-consumed) note HAS a wrongly-emitted BridgeEvent. Count-per-block
# cancels: expected=1, logs=1, missing=0, extra=0 — a false green.
#
# The verifier must instead: exclude the reclaim from expectations, derive its
# canonical synthetic tx hash (bridge-out-tool, the shipped derivation), and
# HARD-FAIL on the forbidden log — surfacing BOTH the forbidden emission and
# the missing legit event.
#
# Drives the EXACT production counting core (lib-verify-completeness.py)
# against a fixture node DB and a stub eth RPC. Requires only
# target/debug/bridge-out-tool (same prerequisite as the verifier itself).
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
B2AGG_ROOT = "11" * 32
CLAIM_ROOT = "22" * 32
GER_ROOT = "33" * 32
LEGIT_ID = "C1" * 32
LEGIT_NF = "D1" * 32
RECLAIM_ID = "C2" * 32
RECLAIM_NF = "D2" * 32
BLOCK = 50

db = f"{tmp}/node.sqlite3"
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE notes (note_id BLOB, nullifier BLOB, script_root BLOB,
                    target_account_id BLOB, consumed_at INT, assets BLOB);
CREATE TABLE transactions (block_num INT, account_id BLOB, input_notes BLOB);
CREATE TABLE block_headers (block_num INT);
""")
c.execute("INSERT INTO block_headers VALUES (?)", (BLOCK + 5,))
for note_id, nf, consumer in ((LEGIT_ID, LEGIT_NF, BRIDGE), (RECLAIM_ID, RECLAIM_NF, OTHER)):
    c.execute("INSERT INTO notes VALUES (?,?,?,?,?,?)",
              (bytes.fromhex(note_id), bytes.fromhex(nf), bytes.fromhex(B2AGG_ROOT),
               bytes.fromhex(BRIDGE), BLOCK, b"\x00\x00" + bytes(30)))
    c.execute("INSERT INTO transactions VALUES (?,?,?)",
              (BLOCK, bytes.fromhex(consumer), bytes.fromhex(nf)))
c.commit()

# The wrongly-emitted BridgeEvent: served under the reclaim's CANONICAL
# synthetic tx hash — exactly what the proxy would serve if it wrongly emitted.
derived = subprocess.run([tool, "--store-dir", "/tmp", "--node-url", "http://x",
                          "--derive-bridge-out-tx-hash", "0x" + RECLAIM_ID.lower()],
                         capture_output=True, text=True, check=True).stdout.split()[1]
B2AGG_TOPIC = "0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
LOGS = [{
    "blockNumber": hex(BLOCK),
    "transactionHash": derived,
    "topics": [B2AGG_TOPIC],
    "data": "0x",
}]


class Stub(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if req["method"] == "eth_blockNumber":
            result = hex(BLOCK + 5)
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

run = subprocess.run(
    ["python3", lib, db, rpc, "0x" + BRIDGE.lower(), "0x" + B2AGG_ROOT.lower(),
     "0x" + CLAIM_ROOT.lower(), "0x" + GER_ROOT.lower(), "0", "", f"{tmp}/unclaimable"],
    capture_output=True, text=True)
out = run.stdout
print(out)

failures = []
if run.returncode == 0:
    failures.append("verifier PASSED the same-block substitution (the exact false green)")
if "FORBIDDEN" not in out:
    failures.append("no FORBIDDEN line for the reclaimed note's wrongly-emitted BridgeEvent")
if "MISSING candidate: note 0x" + LEGIT_ID.lower() not in out:
    failures.append("the missing legit note was not surfaced as MISSING")
if "RECLAIMED" not in out:
    failures.append("the reclaim was not classified")
if failures:
    print("SUBSTITUTION REGRESSION FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("SUBSTITUTION REGRESSION PASS: forbidden emission flagged, legit miss surfaced, verdict FAIL")
PY
