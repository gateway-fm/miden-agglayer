#!/usr/bin/env bash
# test-chaos-verdict.sh — unit tests for the PURE verdict predicates in
# lib-chaos-verdict.sh (PR#145 review regression coverage). Touches NO stack:
# every predicate input is a plain argument.
#
#   ./scripts/test-chaos-verdict.sh   # exits 0 iff every assertion holds
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib-chaos-verdict.sh
source "$SCRIPT_DIR/lib-chaos-verdict.sh"

FAILS=0
ok()   { local d="$1"; shift; if "$@"; then echo "PASS  $d"; else echo "FAIL  $d (expected pass)"; FAILS=$((FAILS+1)); fi; }
nok()  { local d="$1"; shift; if "$@"; then echo "FAIL  $d (expected fail)"; FAILS=$((FAILS+1)); else echo "PASS  $d"; fi; }

echo "── chaos_legit_ok: the independent verifier is REQUIRED ──"
#           desc                                                fn             VC STORE LOCKS LT
nok "verifier FAIL + store CLEAN must FAIL (no override)"  chaos_legit_ok  1  1  0  0
nok "verifier FAIL + store DROP must FAIL"                 chaos_legit_ok  1  0  0  0
nok "verifier PASS + store DROP must FAIL (veto)"          chaos_legit_ok  0  0  0  0
nok "verifier PASS + store CLEAN + locks must FAIL"        chaos_legit_ok  0  1  3  0
nok "verifier PASS + store CLEAN + loadtest fail must FAIL" chaos_legit_ok 0  1  0  1
ok  "verifier PASS + store CLEAN + no locks + lt ok PASSES" chaos_legit_ok 0  1  0  0

echo "── chaos_garbo_ok: containment needs zero leaks of BOTH classes ──"
#           desc                                       fn            FOREIGN PRIVATE
nok "foreign leak must FAIL"                     chaos_garbo_ok  2  0
nok "private-derived persisted event must FAIL"  chaos_garbo_ok  0  1
nok "both leaks must FAIL"                       chaos_garbo_ok  3  1
ok  "zero leaks PASSES"                          chaos_garbo_ok  0  0

echo "── chaos_fired_ok: an empty storm must not certify ──"
#           desc                                fn             FAULTS PRIV F_EN F_FIRED
nok "no faults must FAIL"                 chaos_fired_ok  0  2  1  1
nok "private never fired must FAIL"       chaos_fired_ok  3  0  1  1
nok "foreign enabled but 0 fired FAILS"   chaos_fired_ok  3  2  1  0
ok  "foreign disabled + 0 fired PASSES"   chaos_fired_ok  3  2  0  0
ok  "all classes fired PASSES"            chaos_fired_ok  3  2  1  1

echo "── chaos_verdict: end-to-end combinations ──"
#           desc                              fn            VC ST LK LT FL PL FA PF FE FF
nok "VC_RC=1 STORE_OK=1 fails overall"  chaos_verdict  1  1  0  0  0  0  3  2  1  1
nok "private leak fails overall"        chaos_verdict  0  1  0  0  0  1  3  2  1  1
nok "incomplete loadtest fails overall" chaos_verdict  0  1  0  1  0  0  3  2  1  1
ok  "complete ops + VC_RC=0 passes"     chaos_verdict  0  1  0  0  0  0  3  2  1  1

echo "── mixed_ops_ok: MIX_VERIFY=0 must still enforce every operation ──"
#           desc                                          fn           F_OK F_SUB B_OK B_SUB CLASH    LT SKIP VC   LOCKS
nok "incomplete forwards fail even with vc=skip"    mixed_ops_ok  1  2  2  2  distinct 0  0  skip 0
nok "incomplete backs fail even with vc=skip"       mixed_ops_ok  2  2  1  2  distinct 0  0  skip 0
nok "zero submitted forwards fail (empty run)"      mixed_ops_ok  0  0  2  2  distinct 0  0  skip 0
nok "non-distinct clash fails even with vc=skip"    mixed_ops_ok  2  2  2  2  same     0  0  skip 0
nok "L1 loadtest fail (not skipped) fails"          mixed_ops_ok  2  2  2  2  distinct 1  0  skip 0
ok  "L1 loadtest fail but SKIP_L1=1 passes"         mixed_ops_ok  2  2  2  2  distinct 1  1  skip 0
nok "store locks fail even with vc=skip"            mixed_ops_ok  2  2  2  2  distinct 0  0  skip 2
nok "verifier fail (MIX_VERIFY=1) fails"            mixed_ops_ok  2  2  2  2  distinct 0  0  1    0
ok  "all ops complete + vc=skip passes"             mixed_ops_ok  2  2  2  2  distinct 0  0  skip 0
ok  "all ops complete + vc=0 passes"                mixed_ops_ok  2  2  2  2  distinct 0  0  0    0


# ── l1_ops_ok (PR#145 follow-up: the nested L1<->Miden child's STRICT_OPS
#    verdict — the real operational failure the parent consumes as LT_RC) ─────
#                                                            SUB1 PLN1 SUB2 PLN2 F1 F2 CLM SUB
ok  "l1: all planned submitted, none failed, all claimed"  l1_ops_ok 15 15 15 15 0 0 30 30
nok "l1: target shortfall L1->L2 (sub < plan) fails"       l1_ops_ok 14 15 15 15 0 0 29 29
nok "l1: target shortfall L2->L1 (sub < plan) fails"       l1_ops_ok 15 15 12 15 0 0 27 27
nok "l1: explicit submission failure fails"                l1_ops_ok 15 15 15 15 1 0 30 30
nok "l1: submitted-but-unclaimed work fails"               l1_ops_ok 15 15 15 15 0 0 28 30
nok "l1: empty/unset counters fail closed"                 l1_ops_ok "" "" "" "" "" "" "" ""

echo "──────────────────────────────────────────────"
if [[ "$FAILS" == "0" ]]; then
    echo "CHAOS-VERDICT TESTS: ALL PASS"
    exit 0
else
    echo "CHAOS-VERDICT TESTS: $FAILS FAILURE(S)"
    exit 1
fi
