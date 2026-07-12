#!/bin/bash
# External completeness watcher: every INTERVAL seconds, diff the miden-node's own DB
# (ground truth: consumed B2AGG notes) against the proxy's eth_getLogs (BridgeEvents),
# up to a safe cut (synthetic tip - MARGIN). Sealed blocks are final (getLogs
# immutability), so any consumed note <= cut without its log at EXACTLY its consumption
# block is flagged within one interval — no waiting for a post-run verify.
#
# Deliberate emit refusals (cantina13 / MA#18 poisoned-registry quarantine) are
# classified EXPECTED-QUARANTINE by cross-checking the proxy's "refusing to emit /
# unrecoverable" warns for the note's faucet — only an UNEXPLAINED absence prints
# "COMPLETENESS VIOLATION" (grep target for harness fast-fail).
#
# Usage:
#   COMPOSE_PROJECT_NAME=miden-origin INTERVAL=10 MARGIN=2 \
#     ./scripts/monitoring/watch-completeness.sh
# Env:
#   COMPOSE_PROJECT_NAME  compose project prefix (default: miden-agglayer)
#   L2_RPC                proxy JSON-RPC (default: http://127.0.0.1:8546)
#   INTERVAL              seconds between sweeps (default: 10)
#   MARGIN                blocks below the synthetic tip to audit (default: 2)
#   B2AGG_ROOT            override the B2AGG note script root (hex, no 0x)
set -u
PROJECT="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
PROXY_CONTAINER="${PROXY_CONTAINER:-${PROJECT}-miden-agglayer-1}"
NODE_CONTAINER="${NODE_CONTAINER:-${PROJECT}-miden-node-1}"
L2_RPC="${L2_RPC:-http://127.0.0.1:8546}"
INTERVAL="${INTERVAL:-10}"
MARGIN="${MARGIN:-2}"
B2AGG_TOPIC="0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
B2AGG_ROOT="${B2AGG_ROOT:-fae9ac3f6b4a64fde2e6e03a847cb0f4b9d0f4ab1cf7aa99d6de52bc8d087098}"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
ts() { TZ=${TZ_DISPLAY:-Europe/Berlin} date '+%H:%M:%S'; }
echo "[$(ts)] completeness watcher up: project=$PROJECT interval=${INTERVAL}s margin=${MARGIN} blocks"
ALERTED=""
while true; do
  sleep "$INTERVAL"
  # synthetic tip == projector cursor; eth_blockNumber is log-flood immune
  CUR=$(curl -s -m5 -X POST "$L2_RPC" -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))' 2>/dev/null)
  [ -z "${CUR:-}" ] && continue
  CUT=$((CUR - MARGIN)); [ "$CUT" -le 0 ] && continue
  # WAL-aware snapshot: recent commits live in the -wal file until checkpointed; cat'ing
  # only the main db blinds the watcher to the newest blocks (where tip-edge races live).
  docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3 > "$TMPDIR/node.sqlite3" 2>/dev/null || continue
  docker exec "$NODE_CONTAINER" cat /data/node/miden-store.sqlite3-wal > "$TMPDIR/node.sqlite3-wal" 2>/dev/null || rm -f "$TMPDIR/node.sqlite3-wal"
  rm -f "$TMPDIR/node.sqlite3-shm"
  RES=$(python3 - "$TMPDIR/node.sqlite3" "$CUT" "$B2AGG_ROOT" "$B2AGG_TOPIC" "$L2_RPC" <<'PY'
import sqlite3, sys, json, urllib.request
db, cut, root, topic, rpc = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
c = sqlite3.connect(db)
notes = {}  # note_id -> (block, nullifier)
for nid, blk, nul in c.execute(
    "SELECT hex(note_id), consumed_at, hex(nullifier) FROM notes "
    "WHERE consumed_at IS NOT NULL AND consumed_at <= ? AND hex(script_root) = upper(?)",
    (cut, root)):
    notes[nid.lower()] = (blk, (nul or '').lower())
req = urllib.request.Request(rpc,
    json.dumps({"jsonrpc":"2.0","id":1,"method":"eth_getLogs",
      "params":[{"fromBlock":"0x0","toBlock":hex(cut),"topics":[topic]}]}).encode(),
    {"Content-Type":"application/json"})
logs = json.load(urllib.request.urlopen(req, timeout=20)).get("result", [])
log_blocks = {}
for l in logs:
    b = int(l["blockNumber"], 16)
    log_blocks[b] = log_blocks.get(b, 0) + 1
missing = []
note_blocks = {}
for nid, (blk, nul) in notes.items():
    note_blocks[blk] = note_blocks.get(blk, 0) + 1
for blk, cnt in sorted(note_blocks.items()):
    have = log_blocks.get(blk, 0)
    if have < cnt:
        for nid, (b, nul) in notes.items():
            if b == blk:
                # faucet id from the assets blob (skip 2-byte prefix, 15-byte id) so the
                # shell can cross-check the proxy's deliberate emit-refusal warns
                fauc = ''
                try:
                    r = c.execute("SELECT hex(assets) FROM notes WHERE hex(note_id)=upper(?)", (nid,)).fetchone()
                    if r and r[0]: fauc = r[0][4:34].lower()
                except Exception: pass
                missing.append(f"{nid}@{blk} nullifier={nul} faucet={fauc}")
print(f"OK notes={len(notes)} logs={sum(log_blocks.values())} cut={cut}" if not missing
      else "MISSED " + " | ".join(missing))
PY
) || continue
  case "$RES" in
    MISSED*)
      # Classify: a DELIBERATE emit refusal (poisoned-registry quarantine) is expected —
      # the proxy logs "refusing to emit" naming the faucet. Only unexplained = violation.
      EXPECTED=1
      for F in $(echo "$RES" | grep -aoE "faucet=[0-9a-f]+" | cut -d= -f2 | sort -u); do
        if ! docker logs --since 10m "$PROXY_CONTAINER" 2>&1 \
             | grep -aiE "refusing to emit|unrecoverable" | grep -aqi "$F"; then
          EXPECTED=0
        fi
      done
      KEY=$(echo "$RES" | head -c 120)
      if [ "$EXPECTED" -eq 1 ]; then
        [ "$KEY" != "$ALERTED" ] && { ALERTED="$KEY"; echo "[$(ts)] EXPECTED-QUARANTINE (deliberate emit refusal, not a violation): $RES"; }
      elif [ "$KEY" != "$ALERTED" ]; then
        ALERTED="$KEY"
        echo "[$(ts)] ████ COMPLETENESS VIOLATION DETECTED ████"
        echo "[$(ts)] $RES"
      fi
      ;;
    *) echo "[$(ts)] $RES";;
  esac
done
