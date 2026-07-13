#!/usr/bin/env bash
# e2e-faucet-tripwire.sh — the faucet-registry SECURITY reconciler (tripwire) + NATIVE
# faucet restore recovery, told as one incident:
#
#   1. Deploy + register a NATIVE faucet via the proxy admin — bridge + store agree.
#   2. DELETE its faucet_registry row. This reproduces exactly the state the tripwire
#      defends against: a faucet the BRIDGE registers but the proxy store has NO row for
#      (an admin-key registration made OUTSIDE the proxy, or a lost row).
#   3. The FaucetRegistryReconciler sees bridge-has / store-lacks past its grace window and
#      HALTS the proxy fail-closed (exit 1). `restart: on-failure` -> the proxy crashloops.
#      Assert the fatal "SECURITY TRIPWIRE" log + a bumped container RestartCount.
#   4. RECOVER via --restore (the sanctioned import path): it rebuilds the missing NATIVE
#      row from the bridge's faucet_metadata_map. Assert the row is back with
#      origin_network == MIDEN_NETWORK_ID + scale 0 (native), same faucet_id (no split-brain).
#   5. Restart the proxy normally: with the row present the reconciler is quiet and the
#      proxy stays healthy (no re-halt).
#
# Runs on the l2l2 stack (project l2l2). The proxy image MUST be built WITH the reconciler
# and recreated with a SHORT interval so the halt is observable quickly:
#   FAUCET_RECONCILER_POLL_SECS=5 FAUCET_RECONCILER_GRACE_TICKS=2
# The driver (scratchpad/tripwire-run.sh) rebuilds+recreates the proxy before calling this.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURES_DIR="$PROJECT_DIR/fixtures"
source "$SCRIPT_DIR/lib-l2l2.sh"

AGG_C="l2l2-miden-agglayer-1"
COMPOSE=(docker compose -f "$PROJECT_DIR/docker-compose.e2e.yml" -f "$PROJECT_DIR/docker-compose.l2l2.yml" -p l2l2 --env-file "$FIXTURES_DIR/.env")

: "${ADMIN_API_KEY:?fixtures/.env must define ADMIN_API_KEY}"
ADMIN_BEARER="Authorization: Bearer ${ADMIN_API_KEY}"

# Fresh per-run origin (admin_registerNativeFaucet is idempotent by origin).
NATIVE_ORIGIN_ADDR="0x0d1de0$(python3 -c 'import secrets;print(secrets.token_hex(17))')"
MINT_UNITS=500000

log "======================================================================"
log "  FAUCET-REGISTRY SECURITY TRIPWIRE + NATIVE RESTORE"
log "  proxy network id = $MIDEN_NETWORK_ID (native origin_network)"
log "======================================================================"

l2l2_ensure_stack
if [[ "${L2L2_PREFLIGHT_DONE:-0}" != "1" ]]; then l2l2_validate_stack; fi
l2l2_miden_identities

# Guard: the running proxy must actually have the reconciler (a stale image without it
# would make this test vacuously "pass"). Also confirm it started with a short interval.
# NB: use grep -c (reads all input) not grep -q — under `set -o pipefail`, grep -q closes
# the pipe on first match and docker logs dies with SIGPIPE (141), failing the pipeline
# even on a match (flaky by log size). Count-and-compare sidesteps that.
[[ "$(docker logs "$AGG_C" 2>&1 | grep -c "FaucetRegistryReconciler starting")" -gt 0 ]] \
  || fail "the running proxy has NO FaucetRegistryReconciler — rebuild the image with the reconciler code"
pass "proxy is running the FaucetRegistryReconciler"

# ── 1. Deploy + register a native faucet (proxy admin) ───────────────────────
step "1. External deploys a native faucet; proxy admin registers it (bridge + store agree)"
NATIVE_FAUCET_ID=$(iso_tool --create-native-faucet --native-symbol "MDN" --native-decimals 8 \
    --mint-units "$MINT_UNITS" --wallet-id "$WALLET_ID" 2>&1 | awk '/faucet-id:/{print $NF}') || true
[[ -n "$NATIVE_FAUCET_ID" ]] || fail "native faucet deploy failed"
REG=$(curl -sf "$L2_RPC" -H "Content-Type: application/json" -H "$ADMIN_BEARER" -d "{
  \"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"admin_registerNativeFaucet\",
  \"params\":[{\"faucet_id\":\"$NATIVE_FAUCET_ID\",\"origin_token_address\":\"$NATIVE_ORIGIN_ADDR\",
    \"symbol\":\"MDN\",\"decimals\":8}]}" 2>/dev/null) || fail "admin_registerNativeFaucet unreachable"
echo "$REG" | python3 -c "import json,sys;sys.exit(0 if 'result' in json.load(sys.stdin) else 1)" \
  || fail "admin_registerNativeFaucet failed: $REG"
NET=""
for _i in $(seq 1 40); do
  NET=$(pgq "SELECT origin_network FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
  [[ "$NET" == "$MIDEN_NETWORK_ID" ]] && break; sleep 3
done
[[ "$NET" == "$MIDEN_NETWORK_ID" ]] || fail "native row not written (origin_network='$NET')"
PRE_SCALE=$(pgq "SELECT scale FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
pass "native faucet $NATIVE_FAUCET_ID registered on bridge + store (origin_network=$NET, scale=$PRE_SCALE)"

# ── 2. Delete the store row — the tripwire condition ─────────────────────────
step "2. Delete the faucet_registry row (bridge still registers it => tripwire condition)"
RESTART_BEFORE=$(docker inspect -f '{{.RestartCount}}' "$AGG_C" 2>/dev/null || echo 0)
DELETE_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
pgq "DELETE FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');" >/dev/null
[[ "$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")" == "0" ]] \
  || fail "row not deleted"
pass "native faucet_registry row deleted (bridge still has it => proxy store now inconsistent)"

# ── 3. The reconciler must HALT the proxy fail-closed ────────────────────────
step "3. Reconciler must detect the unknown bridge faucet + HALT (fatal + crashloop)"
TRIPPED=0
for _i in $(seq 1 40); do   # up to ~120s; short interval (5s x 2 grace) should trip in ~15s
  if [[ "$(docker logs --since "$DELETE_TIME" "$AGG_C" 2>&1 | grep -c "SECURITY TRIPWIRE")" -gt 0 ]]; then TRIPPED=1; break; fi
  sleep 3
done
[[ "$TRIPPED" == "1" ]] || fail "reconciler did NOT log SECURITY TRIPWIRE within 120s of the anomaly"
# Fail-closed exit(1) + restart:on-failure => the container restarts. Confirm it bumped.
BUMPED=0
for _i in $(seq 1 30); do
  RC=$(docker inspect -f '{{.RestartCount}}' "$AGG_C" 2>/dev/null || echo 0)
  [[ "$RC" -gt "$RESTART_BEFORE" ]] && { BUMPED=1; break; }
  sleep 3
done
[[ "$BUMPED" == "1" ]] || fail "proxy logged the tripwire but did not exit/restart (RestartCount stuck at $RESTART_BEFORE)"
pass "TRIPWIRE FIRED: proxy logged SECURITY TRIPWIRE + exited fail-closed (RestartCount $RESTART_BEFORE -> $RC)"

# ── 4. Recover via --restore — rebuilds the NATIVE row (native restore) ──────
step "4. --restore recovers the NATIVE faucet row from bridge state (native restore)"
"${COMPOSE[@]}" stop miden-agglayer >/dev/null 2>&1; sleep 2
"${COMPOSE[@]}" run --rm --no-deps miden-agglayer \
    --miden-node=http://miden-node:57291 \
    --miden-store-dir=/var/lib/miden-agglayer-service \
    --restore 2>&1 | while IFS= read -r l; do echo "  [restore] $l"; done
RESTORE_EXIT=${PIPESTATUS[0]}
[[ "$RESTORE_EXIT" -eq 0 ]] || fail "--restore exited $RESTORE_EXIT"
RB=$(pgq "SELECT origin_network, scale FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
[[ -n "$RB" ]] || fail "NATIVE faucet_registry row NOT rebuilt by --restore (restore skips native faucets — the gap)"
RB_NET=$(echo "$RB" | awk -F'|' '{print $1}'); RB_SCALE=$(echo "$RB" | awk -F'|' '{print $2}')
[[ "$RB_NET" == "$MIDEN_NETWORK_ID" ]] || fail "rebuilt origin_network=$RB_NET != $MIDEN_NETWORK_ID (native must be network_id)"
[[ "$RB_SCALE" == "0" ]] || fail "rebuilt scale=$RB_SCALE != 0 (native is unscaled)"
ROWS=$(pgq "SELECT COUNT(*) FROM faucet_registry WHERE lower(faucet_id)=lower('$NATIVE_FAUCET_ID');")
[[ "$ROWS" -eq 1 ]] || fail "expected exactly 1 rebuilt row, got $ROWS (split-brain)"
pass "NATIVE row REBUILT by --restore (origin_network=$RB_NET, scale=0, same faucet_id, no split-brain)"

# ── 5. Restart normally — proxy stays healthy (no re-halt) ────────────────────
step "5. Restart the proxy normally — with the row restored the reconciler stays quiet"
RESTART_HEALTHY_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
"${COMPOSE[@]}" start miden-agglayer >/dev/null 2>&1
HEALTHY=0
for _i in $(seq 1 40); do
  curl -sf "$L2_RPC" -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 && { HEALTHY=1; break; }
  sleep 3
done
[[ "$HEALTHY" == "1" ]] || fail "proxy did not become healthy after restore+restart"
# Let a few reconciler ticks pass, then confirm it did NOT re-trip.
sleep 25
if [[ "$(docker logs --since "$RESTART_HEALTHY_TIME" "$AGG_C" 2>&1 | grep -c "SECURITY TRIPWIRE")" -gt 0 ]]; then
  fail "proxy RE-HALTED after restore — the row was not usable / not recognized"
fi
curl -sf "$L2_RPC" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null 2>&1 \
  || fail "proxy not healthy after the quiet window (crashlooped again?)"
pass "proxy healthy post-restore; reconciler quiet (row present) — no re-halt"

log "======================================================================"
log "  TRIPWIRE + NATIVE RESTORE PASS"
log "  unknown bridge faucet -> HALT fail-closed -> --restore rebuilt the NATIVE"
log "  row (origin_network=$MIDEN_NETWORK_ID, scale 0) -> healthy, no re-halt."
log "======================================================================"
