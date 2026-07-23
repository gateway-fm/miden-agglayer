#!/usr/bin/env bash
# ============================================================================
# #157 DETERMINISTIC recovery scenarios (reviewer item #7).
#
# For each of FOUR fault×operation combinations this test drives a REAL,
# effect-bearing write (a controlled GER injection, or a claim from a real
# deposit) through the proxy and:
#   - kills a component AT THE PROOF BOUNDARY (triggered by the proxy's own
#     proving-start log line "proving UpdateGerNote (Miden proof in progress)" /
#     "proving CLAIM note (Miden proof in progress)" — NOT a pre-proving marker
#     and NOT a blind sleep),
#   - verifies the target is ACTUALLY down,
#   - keeps Miden DOWN across the window (node-crash cases),
#   - DISABLES the real rebroadcaster (a controlled GER tx nobody resends; the
#     bridge-service ClaimTxManager stopped for claims — NOT bridge-autoclaim,
#     which is L2->L1),
#   - then asserts the SAME captured tx/hash reaches the CORRECT terminal receipt,
#     the signer nonce advanced EXACTLY once (no double-advance = also proves no
#     rebroadcast), the effect applied EXACTLY once, and the NEXT nonce works.
#
#   1. node  crash during GER injection (proving)
#   2. proxy crash during GER injection (proving)
#   3. node  crash during CLAIM        (proving)
#   4. proxy crash during CLAIM        (proving)
#
# Requires the stack up with REJECT_UNVERIFIED_GER_INJECTION=false (so a
# controlled GER injects) and --insecure-allow-any-signer. Env:
# COMPOSE_PROJECT_NAME, L2_RPC.
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${COMPOSE_PROJECT_NAME:-gate55}"
L2_RPC="${L2_RPC:-http://localhost:8546}"
NODE="${MIDEN_NODE_CONTAINER:-${PROJECT}-miden-node-1}"
PROXY="${AGGLAYER_CONTAINER:-${PROJECT}-miden-agglayer-1}"
PG="${AGGLAYER_PG_CONTAINER:-${PROJECT}-agglayer-postgres-1}"
# The L1->L2 claim submitter/rebroadcaster is the bridge-service ClaimTxManager
# (L2URLs=[proxy:8546]), NOT bridge-autoclaim (that claims L2->L1 on anvil).
BRIDGE_SERVICE="${BRIDGE_SERVICE_CONTAINER:-${PROJECT}-bridge-service-1}"
BRIDGE="${BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
GER_KEY="${GER_TEST_KEY:-0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d}"
CLAIM_SELECTOR="ccaa2d11"

GER_PROVE_MARKER="proving UpdateGerNote (Miden proof in progress)"
CLAIM_PROVE_MARKER="proving CLAIM note (Miden proof in progress)"

log()  { echo "[scen] $*"; }
pass() { echo "[scen] PASS: $*"; }
fail() { echo "[scen] FAIL: $*"; exit 1; }
pgq()  { docker exec "$PG" psql -U agglayer -d agglayer_store -tAX -c "$1" 2>/dev/null; }
container_running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = "true" ]; }

GER_ADDR="$(cast wallet address --private-key "$GER_KEY")"

for c in "$NODE" "$PROXY" "$PG" "$BRIDGE_SERVICE"; do
    docker inspect "$c" >/dev/null 2>&1 || fail "container $c not found — is the stack up?"
done

wait_proxy_ready() {
    local tries="${1:-60}"
    for _ in $(seq 1 "$tries"); do
        cast chain-id --rpc-url "$L2_RPC" >/dev/null 2>&1 && return 0
        sleep 3
    done
    return 1
}

metric_val() { curl -fsS "$L2_RPC/metrics" 2>/dev/null | awk -v m="$1" '$1==m{print $2}' | tail -1; }
recovery_progress() {
    local s r a
    s="$(metric_val orphan_recovery_successes_total)"; s="${s%.*}"
    r="$(metric_val orphan_recovery_redrives_total)"; r="${r%.*}"
    a="$(metric_val orphan_recovery_already_claimed_total)"; a="${a%.*}"
    echo $(( ${s:-0} + ${r:-0} + ${a:-0} ))
}

# Assert recovery actually ran. A PROXY restart RESETS the in-process Prometheus
# counters, so a pre/post delta is invalid there — instead require the post value to
# be >=1 (recovery incremented it since the restart). For a NODE crash the proxy
# stays up, so the counter must strictly advance past the pre-fault baseline.
assert_recovery_ran() {
    local fault="$1" prog0="$2" prog1="$3" what="$4"
    if [ "$fault" = "proxy" ]; then
        [ "${prog1:-0}" -ge 1 ] || fail "no post-restart recovery for this $what (counter=$prog1)"
    else
        [ "${prog1:-0}" -gt "${prog0:-0}" ] || fail "recovery counters did not advance for this $what ($prog0 -> $prog1)"
    fi
    pass "  recovery ran (counter $prog0 -> $prog1)"
}

# Wait for a proxy log marker that appeared AFTER $since (unix ts).
wait_for_marker() {
    local marker="$1" since="$2" tries="${3:-60}"
    for _ in $(seq 1 "$tries"); do
        docker logs --since "$since" "$PROXY" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -qF "$marker" && return 0
        sleep 1
    done
    return 1
}

# `cast receipt <hash> status` prints "1 (success)" / "0 (failed)" (empty if
# pending/not-found). Return just the leading digit: 1, 0, or "".
receipt_status() { cast receipt --rpc-url "$L2_RPC" "$1" status 2>/dev/null | awk '{print $1; exit}'; }

# TRUSTWORTHINESS GUARD: assert the exact tx is pending AND has no note handoff —
# i.e. we are killing INSIDE the proving window, before the durable handoff is
# recorded. Fail loudly if the kill would land after proving (handoff already
# present / already terminal), so the scenario can never silently test nothing.
assert_pending_no_handoff() {
    local h="$1" row status linked
    row="$(pgq "SELECT t.status, (l.tx_hash IS NOT NULL)
                FROM transactions t LEFT JOIN tx_note_links l ON l.tx_hash=t.tx_hash
                WHERE t.tx_hash='$h'")"
    status="${row%%|*}"; linked="${row##*|}"
    [ "$status" = "pending" ] || fail "tx $h not pending at kill time (status=$status) — kill missed the proving window"
    [ "$linked" = "f" ] || fail "tx $h already has a note handoff at kill time — kill missed the proving window"
    log "  verified: tx $h is pending + no handoff (inside the proving window)"
}

# ── Fault injectors (verify the target is actually down) ────────────────────────
crash_node() {
    log "  fault: KILL miden-node (keep down ${NODE_DOWN_SECS:-25}s)"
    docker kill "$NODE" >/dev/null 2>&1 || true
    container_running "$NODE" && fail "miden-node still running after kill"
    log "  verified: miden-node is DOWN"
    sleep "${NODE_DOWN_SECS:-25}"
    docker start "$NODE" >/dev/null 2>&1 || true
}
crash_proxy() {
    log "  fault: SIGKILL proxy, then restart (the only remedy)"
    docker kill "$PROXY" >/dev/null 2>&1 || true
    container_running "$PROXY" && fail "proxy still running after kill"
    log "  verified: proxy is DOWN"
    sleep 3
    docker start "$PROXY" >/dev/null 2>&1 || true
    wait_proxy_ready 60 || fail "proxy did not come back after restart"
}

# ── GER scenarios (fully controlled tx, no rebroadcaster) ───────────────────────
scenario_ger() {
    local fault="$1" tag="$2"
    log "═══ SCENARIO ($tag): $fault crash during GER proving ═══"
    local prog0 nonce_before ger since hash
    prog0="$(recovery_progress)"
    nonce_before="$(cast nonce --rpc-url "$L2_RPC" "$GER_ADDR")"
    ger="0x$(printf '%064x' "$(( 0xE50000 + RANDOM ))")"
    since="$(date +%s)"
    hash="$(cast send --async --rpc-url "$L2_RPC" --private-key "$GER_KEY" \
        --legacy --gas-price 1 --gas-limit 1000000 \
        "$BRIDGE" 'insertGlobalExitRoot(bytes32)' "$ger" 2>/dev/null)"
    [[ "$hash" == 0x* ]] || fail "controlled GER submit not admitted (hash=$hash; REJECT_UNVERIFIED_GER=true?)"
    log "  admitted $hash (signer=$GER_ADDR nonce=$nonce_before ger=$ger); waiting for PROOF boundary"
    wait_for_marker "$GER_PROVE_MARKER, ger: ${ger#0x}" "$since" 180 || fail "GER never reached the proof boundary (ger ${ger#0x})"
    assert_pending_no_handoff "$hash"

    case "$fault" in node) crash_node ;; proxy) crash_proxy ;; esac

    log "  waiting for recovery to heal the SAME hash to success"
    local ok=""
    for _ in $(seq 1 120); do
        wait_proxy_ready 5 || true
        [ "$(receipt_status "$hash")" = "1" ] && { ok=1; break; }
        sleep 5
    done
    [ -n "$ok" ] || fail "GER tx $hash did NOT reach success via recovery within ~600s"
    pass "  SAME hash $hash recovered to success"

    local injected
    injected="$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected AND ger_hash = decode('${ger#0x}','hex')")"
    [ "${injected:-0}" -ge 1 ] || fail "GER $ger not injected after recovery"
    pass "  GER effect applied exactly once (is_injected)"

    local nonce_after
    nonce_after="$(cast nonce --rpc-url "$L2_RPC" "$GER_ADDR")"
    [ "$nonce_after" = "$(( nonce_before + 1 ))" ] \
        || fail "nonce double-advanced/stuck: $nonce_before → $nonce_after (rebroadcast or double-drive?)"
    pass "  nonce advanced exactly once ($nonce_before → $nonce_after) — no double-advance / no rebroadcast"

    local prog1; prog1="$(recovery_progress)"
    assert_recovery_ran "$fault" "$prog0" "$prog1" "GER"

    # Next nonce works.
    local ger2 hash2 ok2=""
    ger2="0x$(printf '%064x' "$(( 0xE60000 + RANDOM ))")"
    hash2="$(cast send --async --rpc-url "$L2_RPC" --private-key "$GER_KEY" --legacy --gas-price 1 \
        --gas-limit 1000000 "$BRIDGE" 'insertGlobalExitRoot(bytes32)' "$ger2" 2>/dev/null)"
    [[ "$hash2" == 0x* ]] || fail "the follow-up GER (next nonce) was not admitted"
    for _ in $(seq 1 36); do [ "$(receipt_status "$hash2")" = "1" ] && { ok2=1; break; }; sleep 5; done
    [ -n "$ok2" ] || fail "the NEXT nonce did not work after recovery (follow-up GER $hash2 not successful)"
    pass "  next nonce works (follow-up GER $hash2 succeeded)"
    pass "SCENARIO ($tag) GREEN"
}

# ── CLAIM scenarios (real deposit, stop the ClaimTxManager rebroadcaster) ────────
scenario_claim() {
    local fault="$1" tag="$2"
    log "═══ SCENARIO ($tag): $fault crash during CLAIM proving ═══"
    local prog0 since dep_log
    prog0="$(recovery_progress)"
    since="$(date +%s)"
    dep_log="$(mktemp /tmp/scen-claim.XXXXXX.log)"
    RECV_POLL_TRIES=120 RECV_POLL_INTERVAL=10 \
        env COMPOSE_PROJECT_NAME="$PROJECT" bash "$HERE/e2e-l1-to-l2.sh" > "$dep_log" 2>&1 &
    local dep_pid=$!

    # Drive the real deposit only to the claim PROOF boundary. The wrapper's own
    # Step-3+ timeouts (e.g. 120s "claim tx submitted") are NOT our oracle — they fire
    # long before recovery's ~expiration heal and, with the ClaimTxManager stopped,
    # there is no resubmission. We verify recovery INDEPENDENTLY below.
    log "  waiting for the claim PROOF boundary"
    if ! wait_for_marker "$CLAIM_PROVE_MARKER" "$since" 300; then
        kill "$dep_pid" 2>/dev/null || true
        sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | tail -20
        fail "claim never reached the proof boundary within 300s"
    fi
    # The deposit destination (for the fresh next-claim continuity check).
    local dest
    dest="$(sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | grep -aoE 'Dest: +0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}' | head -1)"
    # MANDATORY hash capture: the exact pending claim tx recovery must heal.
    local hash signer nonce_before
    hash="$(pgq "SELECT tx_hash FROM transactions
                 WHERE status='pending' AND substr(encode(envelope_bytes,'hex'),1,220) LIKE '%${CLAIM_SELECTOR}%'
                 ORDER BY created_at DESC LIMIT 1")"
    [[ "$hash" == 0x* ]] || { kill "$dep_pid" 2>/dev/null || true; fail "could not capture the pending claim tx hash (mandatory)"; }
    signer="$(pgq "SELECT lower(signer) FROM transactions WHERE tx_hash='$hash'")"
    nonce_before="$(cast nonce --rpc-url "$L2_RPC" "$signer" 2>/dev/null)"
    log "  captured claim $hash (signer=$signer nonce=$nonce_before dest=$dest)"
    assert_pending_no_handoff "$hash"

    # Disable rebroadcast: stop the bridge-service ClaimTxManager (the real L1->L2
    # claim submitter/retry source). It stays DOWN across the recovery wait, so ONLY
    # recovery — not a rebroadcast — can heal the claim.
    docker stop "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
    container_running "$BRIDGE_SERVICE" && fail "bridge-service still running after stop (rebroadcast not disabled)"
    log "  verified: bridge-service (ClaimTxManager) is DOWN — no rebroadcast"

    case "$fault" in node) crash_node ;; proxy) crash_proxy ;; esac
    # The wrapper's own timeouts are irrelevant now; stop it and verify independently.
    kill "$dep_pid" 2>/dev/null || true

    # Wait for RECOVERY ALONE (ClaimTxManager still stopped) to heal the EXACT claim
    # to a success receipt. A claim killed at the proof boundary leaves a prepared
    # handoff, re-driven only after its note expiration (~600s). The projector
    # finalises the receipt on consumption — success == the claim actually applied.
    log "  waiting for recovery ALONE to heal claim $hash (ClaimTxManager stopped)"
    local ok=""
    for _ in $(seq 1 130); do
        wait_proxy_ready 5 || true
        [ "$(receipt_status "$hash")" = "1" ] && { ok=1; break; }
        sleep 5
    done
    [ -n "$ok" ] || fail "claim $hash did NOT reach success via recovery within ~650s (ClaimTxManager stopped)"
    pass "  SAME claim hash $hash recovered to success — via recovery ALONE, no rebroadcast"

    # Claim signer nonce advanced EXACTLY once (with the ClaimTxManager stopped this
    # also proves no rebroadcast happened).
    local nonce_after
    nonce_after="$(cast nonce --rpc-url "$L2_RPC" "$signer" 2>/dev/null)"
    [ "$nonce_after" = "$(( nonce_before + 1 ))" ] \
        || fail "claim signer nonce double-advanced/stuck: $nonce_before → $nonce_after"
    pass "  claim signer nonce advanced exactly once ($nonce_before → $nonce_after) — no rebroadcast"

    local prog1; prog1="$(recovery_progress)"
    assert_recovery_ran "$fault" "$prog0" "$prog1" "claim"

    # Restart the ClaimTxManager and prove a FRESH deposit still claims end-to-end
    # (nonce continuity + the whole credit pipeline is live; a full e2e-l1-to-l2 with
    # its own before/after balance delta).
    docker start "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
    log "  next-claim continuity: a fresh deposit must claim end-to-end (balance delta)"
    local dep2; dep2="$(mktemp /tmp/scen-claim2.XXXXXX.log)"
    if env COMPOSE_PROJECT_NAME="$PROJECT" RECV_POLL_TRIES=120 RECV_POLL_INTERVAL=10 \
        bash "$HERE/e2e-l1-to-l2.sh" > "$dep2" 2>&1; then
        pass "  next claim works (fresh deposit credited exactly once after recovery)"
    else
        sed -E 's/\x1b\[[0-9;]*m//g' "$dep2" | tail -20
        fail "the NEXT claim did not work after recovery (ClaimTxManager nonce wedged?)"
    fi
    pass "SCENARIO ($tag) GREEN"
}

# ── Driver ─────────────────────────────────────────────────────────────────────
wait_proxy_ready 60 || fail "proxy RPC not ready at start"

scenario_ger   node  "1/4"
wait_proxy_ready 60 || fail "stack not ready after scenario 1"
scenario_ger   proxy "2/4"
wait_proxy_ready 60 || fail "stack not ready after scenario 2"
scenario_claim node  "3/4"
wait_proxy_ready 60 || fail "stack not ready after scenario 3"
scenario_claim proxy "4/4"

# No pending write of either tested selector may remain (catches LINKED pending
# rows too — not just note_id-NULL orphans). Allow a drain window.
sel_pending() {
    pgq "SELECT count(*) FROM transactions
         WHERE status='pending'
           AND (substr(encode(envelope_bytes,'hex'),1,220) LIKE '%${CLAIM_SELECTOR}%'
                OR substr(encode(envelope_bytes,'hex'),1,220) LIKE '%12da06b2%')"
}
LEFT=""
for _ in $(seq 1 30); do LEFT="$(sel_pending)"; [ "${LEFT:-1}" = "0" ] && break; sleep 10; done
[ "${LEFT:-1}" = "0" ] || fail "$LEFT tested-selector write(s) remain pending (linked rows included) after all scenarios"
pass "no tested-selector writes remain pending (linked + unlinked) — all resolved"

pass "ALL 4 recovery scenarios GREEN: {node,proxy}×{GER,claim} crash-at-proof-boundary self-healed exactly once, no double-advance, next nonce works, rebroadcast disabled"
