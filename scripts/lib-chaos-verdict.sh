#!/usr/bin/env bash
# lib-chaos-verdict.sh — PURE verdict predicates for the chaos soak + mixed
# loadtest. No side effects, no docker/pg access: every input is a plain
# argument so the verdicts are unit-testable (scripts/test-chaos-verdict.sh).
#
# PR #145 review (release-certification blocker): the independent
# verify-event-completeness verifier is THE completeness authority. Proxy-owned
# store corroboration is an ADDITIONAL failure signal (it can veto a
# verifier-pass on a store drop) and NEVER overrides a verifier fail — both
# sides of the store comparison live in proxy-owned PostgreSQL, so agreeing
# aggregates cannot prove events the proxy never observed, nor catch identity /
# exact-block mismatches the verifier checks.

# chaos_legit_ok VC_RC STORE_OK LOCKS LT_RC
#   Completeness verdict: verifier MUST pass (VC_RC==0), store corroboration
#   MUST be clean (STORE_OK==1 — additional veto, not an alternative), zero
#   proxy store-lock errors, and the mixed-loadtest driver completed (LT_RC==0
#   — with the ops verdict enforced even under MIX_VERIFY=0, a nonzero LT_RC
#   now also means "an operation never landed", not just "driver aborted").
chaos_legit_ok() {
    local vc_rc="$1" store_ok="$2" locks="$3" lt_rc="$4"
    [[ "$vc_rc" == "0" && "$store_ok" == "1" && "$locks" == "0" && "$lt_rc" == "0" ]]
}

# chaos_garbo_ok FOREIGN_LEAK PRIVATE_LEAK
#   Containment verdict: zero foreign-claim ClaimEvent rows for the fabricated
#   global indexes AND zero persisted traces of the garbo private/tag-0 note
#   ids in the proxy store (the direct absence assertion; a leak by definition
#   persists rows in the proxy's own tables before it is served). The
#   independent verifier's extra==0 additionally backs this via
#   chaos_legit_ok's VC_RC requirement in the overall verdict.
chaos_garbo_ok() {
    local foreign_leak="$1" private_leak="$2"
    [[ "$foreign_leak" == "0" && "$private_leak" == "0" ]]
}

# chaos_fired_ok FAULTS PRIVATE_FIRED FOREIGN_ENABLED FOREIGN_FIRED
#   The storm actually happened: >=1 infra fault, the private class fired, and
#   the foreign class fired whenever it was enabled.
chaos_fired_ok() {
    local faults="$1" private_fired="$2" foreign_enabled="$3" foreign_fired="$4"
    [[ "${faults:-0}" -ge 1 ]] || return 1
    [[ "${private_fired:-0}" -ge 1 ]] || return 1
    [[ "$foreign_enabled" != "1" || "${foreign_fired:-0}" -ge 1 ]] || return 1
    return 0
}

# chaos_verdict VC_RC STORE_OK LOCKS LT_RC FOREIGN_LEAK PRIVATE_LEAK \
#               FAULTS PRIVATE_FIRED FOREIGN_ENABLED FOREIGN_FIRED
#   The overall two-sided (+liveness) soak verdict. 0 = PASS.
chaos_verdict() {
    chaos_legit_ok "$1" "$2" "$3" "$4" || return 1
    chaos_garbo_ok "$5" "$6" || return 1
    chaos_fired_ok "$7" "$8" "$9" "${10}" || return 1
    return 0
}

# mixed_ops_ok FWD_OK FWD_SUB BACK_OK BACK_SUB CLASH LT_RC SKIP_L1 VC_RC LOCKS
#   The mixed loadtest's operational verdict. VC_RC may be the literal string
#   "skip" (MIX_VERIFY=0: the caller runs the ONE authoritative verifier
#   post-heal) — that skips ONLY the verifier requirement; every operational
#   requirement (all forwards claimed, all backs released, clash distinct, L1
#   loadtest green unless explicitly skipped, zero store locks) still gates.
mixed_ops_ok() {
    local fwd_ok="$1" fwd_sub="$2" back_ok="$3" back_sub="$4" clash="$5" \
          lt_rc="$6" skip_l1="$7" vc_rc="$8" locks="$9"
    [[ "$fwd_ok" == "$fwd_sub" && "${fwd_sub:-0}" -gt 0 ]] || return 1
    [[ "$back_ok" == "$back_sub" && "${back_sub:-0}" -gt 0 ]] || return 1
    [[ "$clash" == "distinct" ]] || return 1
    [[ "$lt_rc" == "0" || "$skip_l1" == "1" ]] || return 1
    [[ "$vc_rc" == "skip" || "$vc_rc" == "0" ]] || return 1
    [[ "${locks:-1}" == "0" ]] || return 1
    return 0
}
