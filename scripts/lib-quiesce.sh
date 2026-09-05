# shellcheck shell=bash
#
# Projection quiescence — the precondition for any scenario that fingerprints or
# COUNTS proxy state.
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
# Wall-clock sleeps do not fix this; they only narrow the window. Quiesce on the
# OBSERVED frontier instead: the projector must have caught up to the synthetic
# tip and the writer queue must be drained, held stable across consecutive
# samples so an in-flight note cannot land between the check and the snapshot.
#
# Callers must define pgq() (psql -tAc against the proxy store) before sourcing,
# or set PG_CONTAINER. Metrics come from ${L2_RPC:-http://localhost:8546}/metrics.
#
# Usage:
#   . "$PROJECT_DIR/scripts/lib-quiesce.sh"
#   quiesce_projection 180 || fail "pipeline never quiesced"
#   SNAP_BLOCK=$(projected_height)     # bound every comparison to this

_q_pgq() {
    if declare -F pgq >/dev/null 2>&1; then pgq "$1"
    else docker exec "${PG_CONTAINER:-miden-agglayer-agglayer-postgres-1}" \
             psql -U agglayer -d agglayer_store -tAc "$1"; fi
}
_q_metric() { # $1 = metric name -> value or empty
    curl -sf --max-time 5 "${L2_RPC:-http://localhost:8546}/metrics" 2>/dev/null \
        | awk -v m="$1" '$1==m {print $2; exit}'
}

# The frontier of what this store has actually projected.
projected_height() { _q_pgq "SELECT projector_cursor FROM service_state WHERE id=1" | tr -d '[:space:]'; }

# True when the projector has caught up to the synthetic tip and nothing is
# queued in the writer.
_q_settled() {
    local cur tip depth
    cur=$(_q_pgq "SELECT projector_cursor FROM service_state WHERE id=1" | tr -d '[:space:]')
    tip=$(_q_pgq "SELECT latest_block_number FROM service_state WHERE id=1" | tr -d '[:space:]')
    depth=$(_q_metric agglayer_writer_queue_depth); depth=${depth:-0}
    [[ -n "$cur" && -n "$tip" ]] || return 1
    [[ "$cur" == "$tip" ]] || return 1
    # NOT `depth == 0`: on a live stack this gauge has a non-zero FLOOR (observed
    # steady at 1 for 180s with cursor == tip == 1098, which made quiescence
    # unreachable and failed the drill with "projection never quiesced"). The
    # aggoracle keeps injecting and Miden keeps producing, so "empty" is not a
    # state this stack reaches. What matters is that the queue is not GROWING —
    # a rising depth means work is still arriving for the projector.
    _Q_DEPTH_NOW="${depth%.*}"
    return 0
}

# quiesce_projection [timeout_secs] [stable_samples]
# NOTE: "quiesced" here means STEADY, not IDLE. This stack never goes idle.
# Requires the settled condition to hold on N CONSECUTIVE samples AND the
# projected height to be unchanged across them — a single settled reading can be
# a gap between two injections.
quiesce_projection() {
    local timeout="${1:-180}" want="${2:-3}" waited=0 ok=0 prev="" h
    while (( waited < timeout )); do
        if _q_settled; then
            h=$(projected_height)
            # Both the projected height AND the queue depth must be unchanged
            # across consecutive samples: height stable == nothing new projected,
            # depth non-growing == nothing new queued. A steady non-zero depth is
            # fine; a climbing one is not.
            if [[ "$h" == "$prev" ]] && [[ "${_Q_DEPTH_NOW:-0}" -le "${prev_depth:-999999}" ]]; then
                ok=$((ok+1))
            else
                ok=1
            fi
            prev="$h"; prev_depth="${_Q_DEPTH_NOW:-0}"
            (( ok >= want )) && { echo "quiesced at projected height $h (writer depth ${_Q_DEPTH_NOW:-0}, steady)" >&2; return 0; }
        else
            ok=0; prev=""; prev_depth=""
        fi
        sleep 5; waited=$((waited+5))
    done
    echo "NOT quiesced after ${timeout}s (projector_cursor=$(projected_height) tip=$(_q_pgq "SELECT latest_block_number FROM service_state WHERE id=1") queue=$(_q_metric agglayer_writer_queue_depth))" >&2
    return 1
}
