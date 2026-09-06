# shellcheck shell=bash
#
# Projection quiescence — the precondition for any scenario that fingerprints
# or COUNTS proxy state.
#
# WHY THIS EXISTS
#
# GER injection runs on the aggoracle's own timer, independent of test traffic.
# A scenario that snapshots counts while the pipeline is still advancing is
# comparing a MOVING system, and a later comparison (a restore that re-reads the
# authoritative node, say) legitimately sees MORE state than the snapshot held.
# That produced a #88 "GER history was lost or duplicated" failure on a
# perfectly faithful restore: UHC and injected both 3 -> 4, Bridge/Claim
# unchanged, eth_getLogs byte-identical — one live injection 13s after the
# snapshot.
#
# WHAT "QUIESCED" MEANS HERE
#
# NO PENDING WORK ANYWHERE — not "the numbers stopped moving for a bit". Four
# independent conditions, every one of which must hold on N consecutive
# samples:
#
#   (a) WRITER DRAINED       queue depth 0 AND zero non-terminal writer jobs.
#   (b) STORE DRAINED        no pending receipts, no PREPARED note handoffs,
#                            no parked future-nonce txns.
#   (c) NOTHING LEFT TO      L1's current global exit root is already present
#       INJECT               in ger_entries with is_injected=true.
#   (d) NOTHING LANDING      the synthetic LOG COUNT is unchanged across the
#                            samples (and the projector has caught up to tip).
#
# An earlier revision of this file asserted only "cursor == tip and the writer
# queue is not GROWING", justified by an observed non-zero FLOOR on
# `agglayer_writer_queue_depth`. There was no floor: the gauge was published
# only on enqueue and never on dequeue, so it froze at the last enqueue's fill
# level (1) forever — the drill's "NOT quiesced after 180s
# (projector_cursor=155 tip=155 queue=1)" was a stale metric on an idle
# pipeline, not a stuck job. Fixed in the service; this file now demands a
# genuine zero.
#
# (d) counts LOGS, not block height. Miden keeps producing empty blocks whether
# or not the pipeline has work, so a stable block height proves nothing; a
# stable log count is the actual statement "no new event was projected".
#
# Callers must define pgq() (psql -tAc against the proxy store) before sourcing,
# or set PG_CONTAINER. Metrics come from ${L2_RPC:-http://localhost:8546}/metrics.
#
# Usage:
#   . "$PROJECT_DIR/scripts/lib-quiesce.sh"
#   quiesce_projection 300 || fail "pipeline never quiesced"
#   SNAP_BLOCK=$(projected_height)     # bound every comparison to this

_q_pgq() {
    if declare -F pgq >/dev/null 2>&1; then pgq "$1"
    else docker exec "${PG_CONTAINER:-miden-agglayer-agglayer-postgres-1}" \
             psql -U agglayer -d agglayer_store -tAc "$1"; fi
}
_q_int() { local v; v=$(_q_pgq "$1" | tr -d '[:space:]'); echo "${v:-}"; }
_q_metric() { # $1 = metric name -> value or empty
    curl -sf --max-time 5 "${L2_RPC:-http://localhost:8546}/metrics" 2>/dev/null \
        | awk -v m="$1" '$1==m {print $2; exit}'
}

# The frontier of what this store has actually projected.
projected_height() { _q_int "SELECT projector_cursor FROM service_state WHERE id=1"; }

# ── (c) helper: L1's current GER ─────────────────────────────────────────────
# Resolved from the L1 BRIDGE contract's own `globalExitRootManager()` rather
# than a hardcoded address, so a redeployed fixture cannot silently point this
# check at a dead contract and make it vacuous.
QUIESCE_L1_RPC="${QUIESCE_L1_RPC:-${L1_RPC:-http://localhost:8545}}"
QUIESCE_L1_BRIDGE="${QUIESCE_L1_BRIDGE:-${L1_BRIDGE_ADDRESS:-${BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}}}"
_Q_GER_MANAGER=""
_q_ger_manager() {
    [[ -n "$_Q_GER_MANAGER" ]] && { echo "$_Q_GER_MANAGER"; return 0; }
    command -v cast >/dev/null 2>&1 || return 1
    _Q_GER_MANAGER=$(cast call --rpc-url "$QUIESCE_L1_RPC" "$QUIESCE_L1_BRIDGE" \
        'globalExitRootManager()(address)' 2>/dev/null | tr -d '[:space:]')
    [[ "$_Q_GER_MANAGER" =~ ^0x[0-9a-fA-F]{40}$ ]] || { _Q_GER_MANAGER=""; return 1; }
    echo "$_Q_GER_MANAGER"
}
_q_l1_ger() {
    local mgr; mgr=$(_q_ger_manager) || return 1
    cast call --rpc-url "$QUIESCE_L1_RPC" "$mgr" 'getLastGlobalExitRoot()(bytes32)' 2>/dev/null \
        | tr -d '[:space:]' | tr 'A-F' 'a-f'
}

# ── The predicate ────────────────────────────────────────────────────────────
# Sets _Q_WHY to the FIRST unmet condition (so a timeout can name it) and
# _Q_LOGS to the current synthetic log count (the caller compares it across
# samples). Returns 0 only when (a),(b),(c) and cursor==tip all hold.
_q_settled() {
    _Q_WHY=""

    # (a) writer drained — a genuine zero on both gauges.
    local depth nonterm
    depth=$(_q_metric agglayer_writer_queue_depth)
    nonterm=$(_q_metric agglayer_writer_nonterminal_jobs)
    if [[ -z "$depth" || -z "$nonterm" ]]; then
        # Refuse to treat an unscrapable /metrics as "drained": that is the
        # false-green this whole check exists to prevent. A proxy built before
        # the nonterminal gauge landed also lands here, loudly.
        _Q_WHY="(a) writer: /metrics did not serve agglayer_writer_queue_depth ('${depth}') \
and agglayer_writer_nonterminal_jobs ('${nonterm}') — is ${L2_RPC:-http://localhost:8546} up, \
and is the proxy image new enough to publish both?"
        return 1
    fi
    depth=${depth%.*}; nonterm=${nonterm%.*}
    if [[ "$depth" != "0" || "$nonterm" != "0" ]]; then
        _Q_WHY="(a) writer NOT drained: queue_depth=$depth nonterminal_jobs=$nonterm (both must be 0)"
        return 1
    fi

    # (b) store drained — nothing durably owed.
    #
    # PREPARED handoffs are split by whether their reclaim is DUE. A note handoff
    # is reclaimable once `reconcile_cursor > prepared_expiration_block`
    # (see PgStore's prepared-link reclaim); before that it is ordinary pending
    # work and the right response is to keep waiting. After it, the row should
    # have been swept and still being here is a stuck reclaim — a finding, not
    # patience. Both block quiescence, but they mean different things and the
    # timeout must say which.
    local pend prep_live prep_expired parked
    pend=$(_q_int   "SELECT count(*) FROM transactions WHERE status='pending'")
    prep_live=$(_q_int "SELECT count(*) FROM tx_note_links l, service_state s \
                        WHERE l.handoff_state='prepared' AND s.id=1 \
                          AND (l.prepared_expiration_block IS NULL \
                               OR s.reconcile_cursor <= l.prepared_expiration_block)")
    prep_expired=$(_q_int "SELECT count(*) FROM tx_note_links l, service_state s \
                           WHERE l.handoff_state='prepared' AND s.id=1 \
                             AND l.prepared_expiration_block IS NOT NULL \
                             AND s.reconcile_cursor > l.prepared_expiration_block")
    parked=$(_q_int "SELECT count(*) FROM queued_txns")
    if [[ -z "$pend" || -z "$prep_live" || -z "$prep_expired" || -z "$parked" ]]; then
        _Q_WHY="(b) store: could not read the pending counters from postgres (proxy store down?)"
        return 1
    fi
    if [[ "$pend" != "0" || "$prep_live" != "0" || "$prep_expired" != "0" || "$parked" != "0" ]]; then
        _Q_WHY="(b) store NOT drained: pending receipts=$pend PREPARED handoffs=$((prep_live + prep_expired)) \
(live=$prep_live, past-expiry=$prep_expired) parked txns=$parked"
        return 1
    fi

    # (c) nothing left for the aggoracle to inject.
    if [[ "${QUIESCE_SKIP_L1_GER:-0}" != "1" ]]; then
        local l1ger have
        l1ger=$(_q_l1_ger) || {
            _Q_WHY="(c) L1 GER: could not read getLastGlobalExitRoot() via \
${QUIESCE_L1_RPC} (bridge $QUIESCE_L1_BRIDGE). Is 'cast' installed and L1 reachable? \
Set QUIESCE_SKIP_L1_GER=1 to run without this condition — the run then cannot claim \
the aggoracle had nothing left to inject."
            return 1
        }
        # Presence, not "is the newest row": ger_entries carries superseded
        # roots too and same-block rows have no total order, so "the L1 root is
        # already injected" is the exact statement of "the aggoracle owes
        # nothing" and is the only one that is well-defined.
        have=$(_q_int "SELECT count(*) FROM ger_entries \
                       WHERE is_injected=true AND '0x'||encode(ger_hash,'hex') = '$l1ger'")
        if [[ "$have" != "1" ]]; then
            _Q_WHY="(c) L1 GER $l1ger is NOT yet injected on this proxy \
(matching is_injected rows: ${have:-<read failed>}) — the aggoracle still has work"
            return 1
        fi
    fi

    # (d) prerequisite: the projector has reached the synthetic tip. The log
    # count stability itself is checked by the caller across samples.
    local cur tip
    cur=$(projected_height)
    tip=$(_q_int "SELECT latest_block_number FROM service_state WHERE id=1")
    if [[ -z "$cur" || -z "$tip" ]]; then
        _Q_WHY="(d) projector: could not read projector_cursor/latest_block_number"
        return 1
    fi
    if [[ "$cur" != "$tip" ]]; then
        _Q_WHY="(d) projector behind: cursor=$cur tip=$tip"
        return 1
    fi

    _Q_LOGS=$(_q_int "SELECT count(*) FROM synthetic_logs")
    [[ -n "$_Q_LOGS" ]] || { _Q_WHY="(d) could not count synthetic_logs"; return 1; }
    return 0
}

# Where a timeout writes its durable evidence. The caller's stack is usually
# destroyed within a minute of a failure — battery iteration 1's post-chaos
# drill blocked on "PREPARED handoffs=1" and the containers were recreated with
# `down -v` 46 SECONDS later, taking the postgres volume and the row's identity
# with them. Stderr alone is not enough: it only survives if the caller happened
# to redirect it into a file that outlives the run.
QUIESCE_EVIDENCE_DIR="${QUIESCE_EVIDENCE_DIR:-}"

# _q_evidence — write the full offending state to a file under
# QUIESCE_EVIDENCE_DIR. Best-effort by construction: this runs on the failure
# path, where a psql error or an unwritable directory must not replace the
# diagnosis it was called to produce. Echoes the path it wrote.
_q_evidence() { # $1 = the unmet condition
    [[ -n "$QUIESCE_EVIDENCE_DIR" ]] || return 0
    mkdir -p "$QUIESCE_EVIDENCE_DIR" 2>/dev/null || return 0
    local f="$QUIESCE_EVIDENCE_DIR/quiesce-timeout-$(date -u +%Y%m%dT%H%M%SZ).txt"
    {
        echo "quiesce timeout $(date -u +%FT%TZ)"
        echo "unmet condition: $1"
        echo
        echo "── cursors ──"
        echo "projector_cursor    : $(_q_int "SELECT projector_cursor FROM service_state WHERE id=1")"
        echo "reconcile_cursor    : $(_q_int "SELECT reconcile_cursor FROM service_state WHERE id=1")"
        echo "latest_block_number : $(_q_int "SELECT latest_block_number FROM service_state WHERE id=1")  (synthetic tip)"
        # The MIDEN tip is not a store column; the projector prints it on every
        # tick, which is the only place it is observable from here.
        if [[ -n "${PROXY_CONTAINER:-}" ]]; then
            echo "miden tip (last projector tick):"
            docker logs --tail 4000 "$PROXY_CONTAINER" 2>&1 \
                | sed -E 's/\x1b\[[0-9;]*m//g' | grep -a 'synthetic projector tick' | tail -1 \
                | sed 's/^/  /' || echo "  (no tick line found)"
        fi
        echo
        echo "── writer ──"
        echo "agglayer_writer_queue_depth       : $(_q_metric agglayer_writer_queue_depth)"
        echo "agglayer_writer_nonterminal_jobs  : $(_q_metric agglayer_writer_nonterminal_jobs)"
        echo "agglayer_writer_inflight_jobs     : $(_q_metric agglayer_writer_inflight_jobs)"
        echo "stranded_prepared_handoffs        : $(_q_metric stranded_prepared_handoffs)"
        echo
        echo "── PREPARED note handoffs (ALL, with reclaim-due) ──"
        _q_pgq "SELECT l.tx_hash, coalesce(l.note_id,'<null>') AS note_id, l.note_commitment, \
                       coalesce(l.prepared_expiration_block::text,'NULL') AS expires_at_block, \
                       s.reconcile_cursor, \
                       (l.prepared_expiration_block IS NOT NULL \
                        AND s.reconcile_cursor > l.prepared_expiration_block) AS reclaim_due, \
                       coalesce(t.status,'<no tx row>') AS owner_tx_status, \
                       round(extract(epoch from now()-l.created_at))::text AS age_secs \
                FROM tx_note_links l \
                JOIN service_state s ON s.id = 1 \
                LEFT JOIN transactions t ON t.tx_hash = l.tx_hash \
                WHERE l.handoff_state = 'prepared' ORDER BY l.created_at" 2>&1
        echo
        echo "── pending receipts ──"
        _q_pgq "SELECT tx_hash, status, signer, coalesce(error_message,'') AS err, \
                       round(extract(epoch from now()-created_at))::text AS age_secs \
                FROM transactions WHERE status='pending' ORDER BY created_at" 2>&1
        echo
        echo "── parked future-nonce txns ──"
        _q_pgq "SELECT signer, nonce, tx_hash, expires_at, parked_during_recovery \
                FROM queued_txns ORDER BY created_at" 2>&1
        echo
        echo "── synthetic log counts by family ──"
        _q_pgq "SELECT substring(topics[1] from 1 for 10) AS topic0, count(*) \
                FROM synthetic_logs GROUP BY 1 ORDER BY 2 DESC" 2>&1
    } > "$f" 2>/dev/null || return 0
    echo "$f"
}

# _q_dump <label> <sql> — print rows on stderr, or nothing when there are none.
# Never fails the caller: this runs on the failure path, where a psql error must
# not replace the diagnosis it was called to produce.
_q_dump() {
    local label="$1" sql="$2" out
    out=$(_q_pgq "$sql" 2>/dev/null) || out=""
    [[ -n "${out//[[:space:]]/}" ]] || return 0
    echo "  --- $label ---" >&2
    printf '%s\n' "$out" | sed 's/^/    | /' >&2
}

# quiesce_projection [timeout_secs] [stable_samples]
# Every condition must hold on `stable_samples` CONSECUTIVE samples 5s apart,
# with the synthetic LOG COUNT identical across all of them. One settled
# reading can be the gap between two injections.
# Seconds between samples. Only the predicate unit test moves this; a real run
# wants a gap wide enough for a slow write to become visible. CLAMPED TO >= 1:
# the loop charges this value against the timeout, so 0 would spin forever and
# a "timeout" that never expires is a hang, not a check.
_Q_SAMPLE_SECS="${QUIESCE_SAMPLE_SECS:-5}"
[[ "$_Q_SAMPLE_SECS" =~ ^[0-9]+$ ]] && (( _Q_SAMPLE_SECS >= 1 )) || _Q_SAMPLE_SECS=1

quiesce_projection() {
    local timeout="${1:-300}" want="${2:-3}" waited=0 ok=0 prev_logs="" h
    _Q_WHY="(never sampled)"
    while (( waited < timeout )); do
        if _q_settled; then
            if [[ "$_Q_LOGS" == "$prev_logs" ]]; then
                ok=$((ok+1))
            else
                ok=1
            fi
            prev_logs="$_Q_LOGS"
            if (( ok >= want )); then
                h=$(projected_height)
                echo "quiesced: writer drained, store drained, L1 GER injected, \
$_Q_LOGS synthetic logs stable over $want samples; projected height $h" >&2
                return 0
            fi
        else
            ok=0; prev_logs=""
        fi
        sleep "$_Q_SAMPLE_SECS"; waited=$((waited+_Q_SAMPLE_SECS))
    done
    echo "NOT quiesced after ${timeout}s — unmet condition: ${_Q_WHY:-(log count never stable: last=${prev_logs:-?})}" >&2
    if [[ -z "$_Q_WHY" ]]; then
        echo "  all four conditions held but the synthetic log count kept moving \
(last stable-run length $ok/$want, last count ${prev_logs:-?}) — traffic is still arriving" >&2
    fi
    echo "  state: cursor=$(projected_height) tip=$(_q_int "SELECT latest_block_number FROM service_state WHERE id=1") \
queue=$(_q_metric agglayer_writer_queue_depth) nonterminal=$(_q_metric agglayer_writer_nonterminal_jobs) \
pending=$(_q_int "SELECT count(*) FROM transactions WHERE status='pending'") \
prepared=$(_q_int "SELECT count(*) FROM tx_note_links WHERE handoff_state='prepared'") \
parked=$(_q_int "SELECT count(*) FROM queued_txns") logs=$(_q_int "SELECT count(*) FROM synthetic_logs")" >&2
    # WHICH rows. A count alone is not actionable, and the caller's stack is
    # usually destroyed within seconds of this returning — a post-chaos drill
    # blocked on "PREPARED handoffs=1" for 600s left nothing at all to look at.
    _q_dump "pending receipts" \
        "SELECT tx_hash || '  status=' || status || '  signer=' || signer \
         || '  age=' || round(extract(epoch from now()-created_at))::text || 's' \
         || coalesce('  err=' || error_message, '') \
         FROM transactions WHERE status='pending' ORDER BY created_at LIMIT 10"
    _q_dump "PREPARED note handoffs" \
        "SELECT l.tx_hash || '  note=' || coalesce(l.note_id, l.note_commitment) \
         || '  expires_at_block=' || coalesce(l.prepared_expiration_block::text, 'NULL') \
         || '  reconcile_cursor=' || s.reconcile_cursor \
         || '  reclaim_due=' || (l.prepared_expiration_block IS NOT NULL \
                                 AND s.reconcile_cursor > l.prepared_expiration_block)::text \
         || '  age=' || round(extract(epoch from now()-l.created_at))::text || 's' \
         FROM tx_note_links l, service_state s \
         WHERE l.handoff_state='prepared' AND s.id=1 ORDER BY l.created_at LIMIT 10"
    _q_dump "parked future-nonce txns" \
        "SELECT signer || '  nonce=' || nonce || '  tx=' || tx_hash \
         || '  expires_at=' || expires_at || '  parked_during_recovery=' || parked_during_recovery::text \
         FROM queued_txns ORDER BY created_at LIMIT 10"
    # Durable copy, written BEFORE returning — the caller may be torn down
    # seconds later and stderr only survives if it was redirected somewhere
    # that outlives the run.
    local ev; ev="$(_q_evidence "${_Q_WHY:-log count never stable}")"
    [[ -n "$ev" ]] && echo "  evidence written to $ev" >&2
    return 1
}
