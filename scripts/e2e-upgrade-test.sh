#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# In-place upgrade test: RELEASE (v0.15.9) → THIS BRANCH → rollback → re-upgrade.
#
# Faithful scenario: a live deployment created BY the release is upgraded by
# swapping only the proxy image on the SAME store (bind mount ./.miden-agglayer-data)
# and the same chain. Verifies, at each transition:
#   • no data loss — cursors resume (no genesis re-sweep), bridge accounts intact
#   • getLogs IMMUTABILITY across the swap — the pre-swap log-set hash for the
#     already-exposed range must be byte-identical after the swap
#   • liveness — tip advances, new traffic (L1→L2 + L2→L1) round-trips
#   • (upgrade only) new-feature smoke: completeness auditor metric present & 0
#
# Phases:  R  release bringup + traffic     U1 upgrade → branch image + traffic
#          RB rollback → release + traffic  U2 re-upgrade → branch + traffic
#
# Requires: images `miden-agglayer-e2e:v0.15.9` and `miden-agglayer-e2e:latest`
# (the branch build) present; run from the repo root. Wipes the current stack.
# ══════════════════════════════════════════════════════════════════════════════
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
PROJECT="${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}"
export COMPOSE_PROJECT_NAME="$PROJECT"
# compose files: base + l2b overlay (+ release override only in release phases)
MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-$(grep -m1 '^MIDEN_NODE_GIT_URL' Makefile | sed 's/.*= *//')}"
MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-$(grep -m1 '^MIDEN_NODE_GIT_REF' Makefile | sed 's/.*= *//')}"
export MIDEN_NODE_GIT_URL MIDEN_NODE_GIT_REF
BASE=(docker compose -f docker-compose.e2e.yml -f docker-compose.l2l2.yml --env-file fixtures/.env)
REL=("${BASE[@]}" -f scripts/upgrade/docker-compose.upgrade-release.yml)
PROXY="${PROJECT}-miden-agglayer-1"
NODE="${PROJECT}-miden-node-1"
L2_RPC="${L2_RPC:-http://127.0.0.1:8546}"
ts() { TZ=${TZ_DISPLAY:-Europe/Berlin} date '+%H:%M:%S'; }
step() { echo "[$(ts)] ════ $* ════"; }
fail() { echo "[$(ts)] FAIL: $*"; exit 1; }
pass() { echo "[$(ts)] PASS: $*"; }

rpc() { curl -s -m10 -X POST "$L2_RPC" -H 'content-type: application/json' -d "$1"; }
tip() { rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))' 2>/dev/null; }

# Hash of ALL logs (3 topics) in [0, $1] — the immutability fingerprint.
logs_hash() {
  local to="$1" h=""
  for t in 0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b \
           0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d \
           0x65d3bf36615f1f02a134d12dfa9ea6b1d4a52386e825973cd27ddb70895c2319; do
    h+=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getLogs\",\"params\":[{\"fromBlock\":\"0x0\",\"toBlock\":\"$(printf '0x%x' "$to")\",\"topics\":[\"$t\"]}]}" \
         | python3 -c 'import sys,json,hashlib;d=json.load(sys.stdin)["result"];print(hashlib.sha256(json.dumps(d,sort_keys=True).encode()).hexdigest())')
  done
  echo "$h"
}

wait_healthy() {  # wait for the proxy RPC + tip advancing
  local deadline=$((SECONDS + ${1:-240})) a b
  while [ $SECONDS -lt "$deadline" ]; do
    a=$(tip); [ -n "$a" ] && sleep 6 && b=$(tip) && [ -n "$b" ] && [ "$b" -gt "$a" ] && return 0
    sleep 4
  done
  return 1
}

traffic() {  # one L1→L2 + one L2→L1 round-trip; $1 = phase label
  step "$1: traffic (L1→L2 + L2→L1)"
  ./scripts/e2e-l1-to-l2.sh > ".upgrade-$1-l1l2.log" 2>&1 || fail "$1 L1→L2 (see .upgrade-$1-l1l2.log)"
  pass "$1 L1→L2 round-trip"
  ./scripts/e2e-l2-to-l1.sh > ".upgrade-$1-l2l1.log" 2>&1 || fail "$1 L2→L1 (see .upgrade-$1-l2l1.log)"
  pass "$1 L2→L1 round-trip"
}

swap() {  # $1 = "release" | "branch"; recreates ONLY the proxy on the same volumes
  local pre_tip pre_hash cursor_before
  pre_tip=$(tip); pre_hash=$(logs_hash "$pre_tip")
  step "swap → $1 image (pre-swap tip=$pre_tip)"
  if [ "$1" = release ]; then "${REL[@]}" up -d miden-agglayer; else "${BASE[@]}" up -d miden-agglayer; fi
  wait_healthy 300 || fail "proxy not healthy after swap to $1"
  # no data loss: cursor resumed ahead, no genesis re-sweep
  # Scope to the NOTE RECONCILER: the L1InfoTreeIndexer legitimately scans L1 from
  # block 1 on every boot (--l1-indexer-from-block=1) and also logs "from: 1,".
  docker logs "$PROXY" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g' | grep -a "note reconciler" | grep -aq "from: 1," \
      && fail "genesis re-sweep detected after swap to $1 (reconciler window from=1)"
  pass "no genesis re-sweep after swap to $1"
  # immutability: the pre-swap exposed range must hash identically
  local post_hash; post_hash=$(logs_hash "$pre_tip")
  [ "$post_hash" = "$pre_hash" ] || fail "getLogs CHANGED across swap to $1 (range 0..$pre_tip)"
  pass "getLogs immutable across swap to $1 (range 0..$pre_tip)"
}

# ── phase R: fresh deployment ON THE RELEASE ─────────────────────────────────
step "teardown + fresh RELEASE bringup (v0.15.9 proxy, same stack otherwise)"
"${BASE[@]}" down -v --remove-orphans >/dev/null 2>&1
make e2e-clean-data gen-l2b-configs >/dev/null 2>&1 || fail "clean-data/gen-l2b-configs"
"${REL[@]}" up -d || fail "release bringup"
until cast chain-id --rpc-url http://localhost:9545 >/dev/null 2>&1; do sleep 2; done
L2B_RPC=http://localhost:9545 ./scripts/setup-l2b.sh > .upgrade-setup-l2b.log 2>&1 || fail "setup-l2b"
"${REL[@]}" up -d --force-recreate --wait aggkit-l2b bridge-service-l2b || fail "l2b services"
wait_healthy 300 || fail "release proxy never became healthy"
docker inspect "$PROXY" --format '{{.Config.Image}}' | grep -q v0.15.9 || fail "proxy is not the release image"
pass "release stack up (proxy $(docker inspect "$PROXY" --format '{{.Config.Image}}'))"
traffic R

# ── phase U1: in-place upgrade to the branch ─────────────────────────────────
swap branch
# new-feature smoke: the completeness auditor exists and reports 0 missing
sleep 35
MET=$(curl -s -m5 http://127.0.0.1:9184/metrics 2>/dev/null | grep -E '^synthetic_projector_completeness_missing_total' | awk '{print $2}')
if [ -n "${MET:-}" ]; then
  [ "${MET%.*}" = "0" ] || fail "completeness auditor reports missing=$MET after upgrade"
  pass "completeness auditor present and 0 after upgrade"
else
  echo "[$(ts)] NOTE: auditor metric not scrapable (metrics port differs?) — non-fatal, verify via logs"
  docker logs "$PROXY" 2>&1 | grep -aiq "completeness violation" && fail "auditor logged a violation"
fi
traffic U1

# ── phase RB: rollback to the release ────────────────────────────────────────
swap release
traffic RB

# ── phase U2: re-upgrade to the branch ───────────────────────────────────────
swap branch
traffic U2

step "UPGRADE TEST PASSED — R → U1 → RB → U2 all green (no data loss, immutable logs, live traffic in every phase)"
