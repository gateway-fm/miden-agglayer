#!/usr/bin/env bash
# Poll Loki via an authenticated cmux browser surface every 30s.
# Captures global_index at stage 3, then tracks the rest of the claim pipeline
# by claim.rs source tag (which doesn't include the eth address).
#
# Exits:
#   0  — "claim published" observed
#   1  — 20-min timeout
#   2  — missing cmux CLI (see fallback message)
#
# Usage:
#   ETH_ADDR_NO_0X=<40-char lowercase hex, no 0x> \
#     [CMUX_SURFACE=surface:N] \
#     [ENV_NAME=bali] \
#     [DEADLINE_MIN=20] \
#     scripts/watch-claim.sh
#
# Required env (auto-loaded from envs/<env>.env + .env.local):
#   LOKI_NAMESPACE, LOKI_SERVICE, LOKI_DATASOURCE_UID, GRAFANA_BASE_URL

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKILL_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_NAME="${ENV_NAME:-bali}"
ENV_FILE="$SKILL_DIR/envs/${ENV_NAME}.env"
LOCAL_FILE="$SKILL_DIR/envs/${ENV_NAME}.env.local"

if [[ -f "$ENV_FILE" ]]; then
  set -a; source "$ENV_FILE"; [[ -f "$LOCAL_FILE" ]] && source "$LOCAL_FILE"; set +a
fi

: "${ETH_ADDR_NO_0X:?set ETH_ADDR_NO_0X to the 40-char lowercase padded eth address (no 0x)}"
: "${LOKI_NAMESPACE:?missing LOKI_NAMESPACE}"
: "${LOKI_SERVICE:?missing LOKI_SERVICE}"
: "${LOKI_DATASOURCE_UID:?missing LOKI_DATASOURCE_UID}"
: "${GRAFANA_BASE_URL:?missing GRAFANA_BASE_URL}"

CMUX_SURFACE="${CMUX_SURFACE:-surface:98}"
DEADLINE_MIN="${DEADLINE_MIN:-20}"

if ! command -v cmux >/dev/null 2>&1; then
  echo "error: cmux CLI not found on PATH" >&2
  echo "  fallback: open $GRAFANA_BASE_URL/explore and watch for the three-stage progression manually" >&2
  echo "  expected log lines in order (filter by 'miden_agglayer_service::claim'):" >&2
  echo "    1. 'creating CLAIM note, global_index: ..., dest_address: 0x...'" >&2
  echo "    2. 'GER propagation wait complete, submitting CLAIM note'" >&2
  echo "    3. 'claim published and ClaimEvent recorded, eth_tx: 0x..., miden_tx: 0x...'" >&2
  exit 2
fi

DEADLINE=$(( $(date -u +%s) + DEADLINE_MIN*60 ))
GIDX=""
SEEN_STAGE=""
ITER=0

loki_query() {
  local expr="$1"
  cmux browser "$CMUX_SURFACE" eval --script '(async () => {
    const expr = `'"${expr//\\/\\\\}"'`;
    const end = Date.now() * 1e6;
    const start = end - 30*60*1e9;
    const url = `/api/datasources/proxy/uid/'"${LOKI_DATASOURCE_UID}"'/loki/api/v1/query_range?query=${encodeURIComponent(expr)}&start=${start}&end=${end}&limit=500&direction=forward`;
    const r = await fetch(url, {credentials: "include", headers: {Accept: "application/json"}});
    if (!r.ok) return JSON.stringify({error: "http "+r.status});
    const j = await r.json();
    const lines = [];
    for (const s of (j.data?.result || [])) {
      for (const [ts, line] of (s.values || [])) lines.push(line.replace(/\x1b\[[0-9;]*m/g, ""));
    }
    return JSON.stringify({n: lines.length, lines});
  })()' 2>/dev/null
}

echo "[$(date -u +%H:%M:%SZ)] watch-claim starting eth=$ETH_ADDR_NO_0X surface=$CMUX_SURFACE deadline=$(date -u -r $DEADLINE +%H:%M:%SZ)"

while (( $(date -u +%s) < DEADLINE )); do
  ITER=$((ITER+1))

  if [[ -z "$GIDX" ]]; then
    # Stage 3 (creating CLAIM note … dest_address … <addr>): match by the eth address.
    EXPR='{namespace="'"$LOKI_NAMESPACE"'", service_name="'"$LOKI_SERVICE"'"} |~ `(?i)'"$ETH_ADDR_NO_0X"'`'
    RAW=$(loki_query "$EXPR")
    LINES=$(printf '%s' "$RAW" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read() or "{}"); [print(x) for x in d.get("lines",[])]' 2>/dev/null)
    if [[ -n "$LINES" ]]; then
      GIDX=$(echo "$LINES" | grep -oE 'global_index: [0-9]+' | head -1 | awk '{print $2}')
      if [[ -n "$GIDX" ]]; then
        echo "[$(date -u +%H:%M:%SZ)] stage=3-creating-note global_index=$GIDX"
        SEEN_STAGE="3-creating-note"
      fi
    fi
  fi

  if [[ -n "$GIDX" ]]; then
    # Stages 4–5: widen to all claim.rs lines; the pipeline after stage 3 doesn't log the address.
    EXPR='{namespace="'"$LOKI_NAMESPACE"'", service_name="'"$LOKI_SERVICE"'"} |~ `miden_agglayer_service::claim` |~ `src/claim.rs`'
    RAW=$(loki_query "$EXPR")
    LINES=$(printf '%s' "$RAW" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read() or "{}"); [print(x) for x in d.get("lines",[])]' 2>/dev/null)

    NEW_STAGE="$SEEN_STAGE"
    if echo "$LINES" | grep -q "ClaimEvent recorded.*eth_tx\|claim published and ClaimEvent recorded"; then
      NEW_STAGE="5-published"
    elif echo "$LINES" | grep -q "claim tx .* committed to block"; then
      NEW_STAGE="5-committed"
    elif echo "$LINES" | grep -q "submitted claim note txn"; then
      NEW_STAGE="4b-submitted"
    elif echo "$LINES" | grep -q "proven tx output note"; then
      NEW_STAGE="4a-proven"
    elif echo "$LINES" | grep -q "GER propagation wait complete"; then
      NEW_STAGE="4-ger-ready"
    fi

    if [[ "$NEW_STAGE" != "$SEEN_STAGE" ]]; then
      echo "[$(date -u +%H:%M:%SZ)] stage=$NEW_STAGE"
      case "$NEW_STAGE" in
        5-published)
          echo "$LINES" | grep -E "submitted claim note txn|claim published|ClaimEvent recorded" | tail -3 | sed 's/^/  | /'
          printf '%s\n' "$LINES" > /tmp/claim-logs.txt
          echo "[$(date -u +%H:%M:%SZ)] DONE — logs saved to /tmp/claim-logs.txt"
          exit 0
          ;;
        *)
          echo "$LINES" | grep -E "(GER propagation|proven tx|submitted claim|committed to block)" | tail -2 | sed 's/^/  | /'
          ;;
      esac
      SEEN_STAGE="$NEW_STAGE"
    fi
  fi

  if (( ITER % 4 == 1 )); then
    echo "[$(date -u +%H:%M:%SZ)] iter=$ITER gidx=${GIDX:-none} stage=${SEEN_STAGE:-none}"
  fi
  sleep 30
done

echo "[$(date -u +%H:%M:%SZ)] TIMEOUT stage=${SEEN_STAGE:-none} gidx=${GIDX:-none}"
printf '%s\n' "${LINES:-}" > /tmp/claim-logs.txt
exit 1
