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
    # restart:on-failure would auto-restart a SIGKILLed node; disable it so the
    # down-window is real and recovery (not the auto-restarted original attempt) heals.
    docker update --restart=no "$NODE" >/dev/null 2>&1 || true
    docker kill "$NODE" >/dev/null 2>&1 || true
    container_running "$NODE" && fail "miden-node still running after kill"
    log "  verified: miden-node is DOWN (auto-restart disabled)"
    sleep "${NODE_DOWN_SECS:-25}"
    docker start "$NODE" >/dev/null 2>&1 || true
    docker update --restart=on-failure "$NODE" >/dev/null 2>&1 || true
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

# ── CLAIM scenarios ─────────────────────────────────────────────────────────────
# The real deposit is the AUTHORITATIVE oracle (widened Step-3/4/5 timeouts so it
# OUTLASTS recovery and still asserts the EXACT balance delta on THIS destination).
# We bind the claim row to this deposit, FREEZE the proxy before the no-handoff
# assertion + fault (no check-to-fault window), keep the node down until this claim's
# submission-failure evidence, and disable the ClaimTxManager rebroadcaster.
scenario_claim() {
    local fault="$1" tag="$2"
    log "═══ SCENARIO ($tag): $fault crash during CLAIM proving ═══"
    local prog0 dep_log scen_start
    prog0="$(recovery_progress)"
    # DB-clock scenario start: correlate the captured claim to THIS run, since the
    # isolated-wallet destination is REUSED across deposits and a stale/concurrent
    # pending claim for the same wallet must never be selected.
    scen_start="$(pgq "SELECT now()")"
    dep_log="$(mktemp /tmp/scen-claim.XXXXXX.log)"
    env COMPOSE_PROJECT_NAME="$PROJECT"         CLAIM_SUBMIT_TIMEOUT=1000 CLAIM_COMMIT_TIMEOUT=300 BALANCE_ATTEMPTS=40         RECV_POLL_TRIES=120 RECV_POLL_INTERVAL=10         bash "$HERE/e2e-l1-to-l2.sh" > "$dep_log" 2>&1 &
    local dep_pid=$!

    # Capture THIS deposit's destination (printed right after provisioning) to BIND
    # the claim row to this deposit — not "the newest arbitrary pending claim".
    local dest desthex
    for _ in $(seq 1 150); do
        dest="$(sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | grep -aoE 'Dest: +0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}' | head -1)"
        [ -n "$dest" ] && break
        kill -0 "$dep_pid" 2>/dev/null || { sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | tail -20; fail "deposit exited before provisioning a destination"; }
        sleep 2
    done
    [ -n "$dest" ] || { kill "$dep_pid" 2>/dev/null||true; fail "no deposit destination captured"; }
    desthex="$(printf '%s' "${dest#0x}" | tr 'A-F' 'a-f')"

    # The wrapper's Step 2 (ready_for_claim) queries bridge-service; we must let it
    # CONFIRM ready_for_claim BEFORE we stop bridge-service to disable rebroadcast,
    # otherwise its Step 2 stalls (deposits_seen=?) and the deposit hard-times-out.
    # After Step 2 the wrapper only greps the proxy log + checks the wallet, so
    # stopping bridge-service then is safe.
    log "  waiting for the wrapper to CONFIRM ready_for_claim (before disabling rebroadcast)"
    local rfc=""
    for _ in $(seq 1 150); do
        grep -aq "Deposit is ready_for_claim" "$dep_log" && { rfc=1; break; }
        kill -0 "$dep_pid" 2>/dev/null || { sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | tail -20; fail "deposit exited before ready_for_claim"; }
        sleep 3
    done
    [ -n "$rfc" ] || { kill "$dep_pid" 2>/dev/null||true; fail "deposit never reached ready_for_claim within 450s"; }

    # Poll the DB for THIS deposit's claim in the durably-admitted + UNLINKED window,
    # BOUND to this destination (its 20-byte address appears in the claimAsset calldata).
    log "  polling for THIS deposit's claim (dest $dest) in the recoverable window"
    local hash=""
    for _ in $(seq 1 400); do
        hash="$(pgq "SELECT t.tx_hash FROM transactions t
                     LEFT JOIN tx_note_links l ON l.tx_hash = t.tx_hash
                     WHERE t.status='pending' AND l.tx_hash IS NULL AND t.miden_tx_id IS NULL
                       AND position('${CLAIM_SELECTOR}' in encode(t.envelope_bytes,'hex')) > 0
                       AND position('${desthex}' in encode(t.envelope_bytes,'hex')) > 0
                       AND t.created_at >= '${scen_start}'
                     ORDER BY t.created_at DESC LIMIT 1")"
        [[ "$hash" == 0x* ]] && break
        sleep 1
    done
    [[ "$hash" == 0x* ]] || { kill "$dep_pid" 2>/dev/null||true; sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | tail -20; fail "THIS deposit's claim (dest $dest) never entered the recoverable window"; }
    local signer nonce_before
    signer="$(pgq "SELECT lower(signer) FROM transactions WHERE tx_hash='$hash'")"
    nonce_before="$(cast nonce --rpc-url "$L2_RPC" "$signer" 2>/dev/null)"
    log "  captured claim $hash (signer=$signer nonce=$nonce_before dest=$dest)"

    # FREEZE the proxy so writer/projector cannot mutate the row between the
    # no-handoff assertion and the fault (eliminates the check-to-fault window).
    docker pause "$PROXY" >/dev/null 2>&1 || true
    [ "$(docker inspect -f '{{.State.Status}}' "$PROXY" 2>/dev/null)" = "paused" ] || fail "proxy did not freeze (pause)"
    assert_pending_no_handoff "$hash"
    log "  proxy FROZEN at pending + no-handoff"

    # Disable rebroadcast (ClaimTxManager) for the whole recovery window.
    docker stop "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
    container_running "$BRIDGE_SERVICE" && fail "bridge-service still running after stop (rebroadcast not disabled)"
    log "  verified: bridge-service (ClaimTxManager) is DOWN — no rebroadcast"

    if [ "$fault" = "proxy" ]; then
        docker kill "$PROXY" >/dev/null 2>&1 || { docker unpause "$PROXY" >/dev/null 2>&1; docker kill "$PROXY" >/dev/null 2>&1; }
        container_running "$PROXY" && fail "proxy still running after kill"
        log "  fault: proxy SIGKILLed while frozen; restarting (the only remedy)"
        docker start "$PROXY" >/dev/null 2>&1 || true
        wait_proxy_ready 60 || fail "proxy did not come back after restart"
    else
        # NODE crash. The node has restart:on-failure, so a SIGKILL would auto-restart
        # it — disable the policy first so it stays DOWN under our control until we see
        # the failure evidence (else the original submit could succeed on the auto-
        # restarted node and bypass recovery — the exact concern here).
        docker update --restart=no "$NODE" >/dev/null 2>&1 || true
        docker kill "$NODE" >/dev/null 2>&1 || true
        container_running "$NODE" && fail "miden-node still running after kill"
        local kill_ts; kill_ts="$(date +%s)"
        docker unpause "$PROXY" >/dev/null 2>&1 || true
        log "  fault: miden-node DOWN (auto-restart disabled); proxy unfrozen — awaiting THIS claim's submission-failure evidence"
        # Proof that THIS claim's SUBMISSION (not just its pre-submit prepared handoff)
        # FAILED under the outage: the writer logged an ambiguous/left-pending/errored
        # submit or a recovery backoff for THIS hash. A `prepared` handoff is written
        # BEFORE submit_miden_transaction, so it is NOT such proof — bringing the node
        # back at `prepared` could let the ORIGINAL attempt succeed and bypass recovery.
        local evidence="" ev_grep
        ev_grep="${hash#0x}.*(ambiguous|leaving receipt pending|not committed|submit.*fail|backoff|error)"
        ev_grep="${ev_grep}|(ambiguous|leaving receipt pending|not committed|submit.*fail|backoff).*${hash#0x}"
        for _ in $(seq 1 180); do
            container_running "$NODE" && fail "miden-node came back before submission-failure evidence (auto-restart not disabled?)"
            if ( set +o pipefail; docker logs --since "$kill_ts" "$PROXY" 2>&1 \
                    | sed -E 's/\x1b\[[0-9;]*m//g' | grep -qiE "$ev_grep" ); then evidence=1; break; fi
            sleep 2
        done
        [ -n "$evidence" ] || { docker start "$NODE" >/dev/null 2>&1||true; docker update --restart=on-failure "$NODE" >/dev/null 2>&1||true; fail "no TARGET-HASH submission-failure/backoff evidence for $hash while the node was down (prepared alone is not proof)"; }
        log "  evidence: THIS claim's submission DEFINITIVELY failed under the node outage; bringing node back"
        docker start "$NODE" >/dev/null 2>&1 || true
        docker update --restart=on-failure "$NODE" >/dev/null 2>&1 || true
    fi

    # The wrapper (widened timeouts) is the oracle: it waits for recovery to re-drive
    # the claim, then asserts the EXACT balance delta credited on THIS destination.
    log "  waiting for recovery to heal + the wrapper's EXACT-balance assertion on dest $dest"
    local rc
    if wait "$dep_pid"; then rc=0; else rc=$?; fi
    if [ "$rc" -ne 0 ]; then
        docker start "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
        sed -E 's/\x1b\[[0-9;]*m//g' "$dep_log" | tail -30
        fail "the interrupted deposit did NOT self-heal to an exact-once credit (dest $dest, rc=$rc)"
    fi
    pass "  interrupted deposit CREDITED EXACTLY ONCE on dest $dest (wrapper exact-balance delta), via recovery alone"

    # SAME captured claim hash reached success (recovery healed the exact tx).
    [ "$(receipt_status "$hash")" = "1" ] || fail "captured claim $hash did not reach a success receipt"
    pass "  SAME claim hash $hash recovered to success"

    # Claim signer nonce advanced EXACTLY once (also proves no rebroadcast).
    local nonce_after; nonce_after="$(cast nonce --rpc-url "$L2_RPC" "$signer" 2>/dev/null)"
    [ "$nonce_after" = "$(( nonce_before + 1 ))" ] \
        || fail "claim signer nonce double-advanced/stuck: $nonce_before → $nonce_after"
    pass "  claim signer nonce advanced exactly once ($nonce_before → $nonce_after) — no rebroadcast"

    local prog1; prog1="$(recovery_progress)"
    assert_recovery_ran "$fault" "$prog0" "$prog1" "claim"

    docker start "$BRIDGE_SERVICE" >/dev/null 2>&1 || true
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
