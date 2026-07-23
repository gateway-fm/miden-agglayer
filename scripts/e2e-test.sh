#!/usr/bin/env bash
# Full bidirectional bridge E2E test — L1→L2 deposit+claim then L2→L1 bridge-out.
# Runs both directions sequentially. Use e2e-l1-to-l2.sh / e2e-l2-to-l1.sh for individual tests.
#
# Usage:
#   ./scripts/e2e-test.sh            # run both directions
#   ./scripts/e2e-test.sh l1-to-l2   # L1→L2 only
#   ./scripts/e2e-test.sh l2-to-l1   # L2→L1 only
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

GREEN='\033[0;32m'; RED='\033[0;31m'; NC='\033[0m'
log() { echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $*"; }

test_filter="${1:-all}"

log "======================================================================"
log "  Miden Bridge E2E Test Suite"
log "======================================================================"
echo ""

case "$test_filter" in
    all)
        # First and cheapest: RPC tip coherence + liveness (postmortem
        # 2026-07-04 frozen-eth_blockNumber regression). Quick-fails the suite.
        "$SCRIPT_DIR/e2e-rpc-tip-consistency.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l1-to-l2.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2-to-l1.sh"
        echo ""
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        echo ""
        # Audit H6 — L1 GER corroboration (positive + negative phases). Needs
        # the compose default REJECT_UNVERIFIED_GER_INJECTION=true; SKIPs
        # itself (with a warning) when the container runs lenient.
        "$SCRIPT_DIR/e2e-ger-l1-verification.sh"
        echo ""
        "$SCRIPT_DIR/e2e-security.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina12-getlogs-returns-all.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina10-concurrent-faucet.sh"
        echo ""
        "$SCRIPT_DIR/e2e-dynamic-erc20.sh"
        echo ""
        "$SCRIPT_DIR/e2e-cantina6-faucet-identity-restore.sh"
        echo ""
        # Foreign-bridge claim provenance: deploys a SECOND agglayer deployment
        # on the same chain and drives a claim through it — must not leak a
        # ClaimEvent into our synthetic_logs. Runs before the proxy-restarting
        # tests (it needs the steady-state reconciler/projector, no restarts).
        "$SCRIPT_DIR/e2e-claim-provenance.sh"
        echo ""
        # Proxy-restarting tests run LAST (they must not race the other
        # scripts' steady-state assumptions), in this order: the cursor test
        # does a plain restart and asserts the sweep RESUMES from the
        # persisted cursor; the private-note test then ends with
        # reset-miden-store + restore, which deliberately RESETS that cursor
        # for a full re-sweep — so it must come after, or the cursor
        # assertion races the restore's intentional genesis walk.
        "$SCRIPT_DIR/e2e-reconciler-cursor-persistence.sh"
        echo ""
        # Audit H2 — GER atomic-commit crash consistency: plain restart, asserts
        # the hash chain is NOT re-rolled and no duplicate UpdateHashChainValue
        # log is emitted. Runs after cursor-persistence (both plain restarts) but
        # BEFORE the private-note test, which ends with a destructive
        # reset-miden-store + restore that would perturb the chain/log counts.
        "$SCRIPT_DIR/e2e-ger-atomic-commit.sh"
        echo ""
        "$SCRIPT_DIR/e2e-reconciler-private-note.sh"
        echo ""
        # ── L2<->L2 + native Miden-originated tiers — run BEFORE cantina13 so the
        # recovery below is exercised against real multi-network state (finding #62: an
        # L2B-origin (net 2) token's metadata must restore via its OWN chain's RPC, not
        # L1). They need the docker-compose.l2l2.yml overlay (postgres-l2b /
        # bridge-service-l2b), so GUARD on the overlay being up: on the base stack they're
        # skipped with a warning; on the l2l2 stack they run. `e2e-test.sh l2l2` runs the
        # L2<->L2 group (forward/clash/back); the native round-trips exercise a
        # Miden-originated token to BOTH destinations (->L2B and ->L1). Running them before
        # cantina13's poison leaf also lets their certificate settlement complete un-wedged.
        if docker ps --format '{{.Names}}' 2>/dev/null | grep -q 'postgres-l2b'; then
            echo "== L2<->L2 group (L2B overlay present) =="
            "$SCRIPT_DIR/e2e-test.sh" l2l2
            echo ""
            echo "== native Miden-originated round-trips (->L2B, ->L1) =="
            DEST=l2b "$SCRIPT_DIR/e2e-miden-origin.sh"
            echo ""
            DEST=l1  "$SCRIPT_DIR/e2e-miden-origin.sh"
            echo ""
            # #148 recovery-readiness is DELIBERATELY NOT run inside `all`. It is genuinely
            # DESTRUCTIVE — it stops the proxy, resets the Miden store, drops bridge_db, and
            # --force-recreates aggkit (fresh BridgeL2Sync cursor), leaving bridge-service +
            # aggkit re-syncing. Its own assertions pass, but the follow-on cantina13 tier
            # then cannot get a fresh deposit to ready_for_claim on the still-resyncing
            # consumers. And it CANNOT be reordered after cantina13 either — cantina13's
            # poison leaf fail-closed halts the reconcile re-sweep recovery depends on. So it
            # must run ISOLATED: the dedicated #148 gate runs e2e-recovery-readiness.sh 3x on
            # its own fresh stack (REQUIRE_RECOVERY_READINESS=1). (It used to run here only
            # because it failed fast at step 1 and never reached its destructive step 4.)
            # #149 restore acceptance (native custom-name row + net-2 row survive
            # --restore with byte-identical preimages + full identity) is asserted by
            # cantina13 below, which reuses the WMDN name!=symbol native row created by
            # the DEST=l2b run above and a net-2 row from the l2l2 tier, and does its own
            # COORDINATED proxy + bridge-service drop+restore (PR #150 re-review — no
            # separate destructive proxy-only reset that would wedge cantina13).
        else
            echo "SKIP L2<->L2 + native tiers — L2B overlay not up (base stack). Run 'make e2e-l2l2-up' to include them."
        fi
        echo ""
        # cantina13 recovery — runs AFTER the L2<->L2 + native tiers so its from-scratch
        # DROP + --restore rebuilds the whole multi-network faucet set (L1 net0, L2B net2,
        # native net1) from on-chain — the finding #62 proof inside the full suite. Its
        # final phase leaves a self-targeted poison leaf in the LET (Cantina #13
        # circuit-break repro) that wedges any certificate settlement after it, so only the
        # L1->L2-only manual-user-claim may follow.
        # #149: when the L2B overlay is up (so the native+net-2 prerequisite rows were
        # created by the tiers above), REQUIRE the restore-survival assertion — a missing
        # row then FAILS the gate instead of silently skipping (PR #150 re-review).
        REQUIRE_S149_RESTORE="$(docker ps --format '{{.Names}}' 2>/dev/null | grep -q 'postgres-l2b' && echo 1 || echo 0)" \
            "$SCRIPT_DIR/e2e-cantina13-metadata-recovery.sh"
        echo ""
        # VERY LAST in the suite — defense in depth: the manual-user-claim
        # script deliberately front-runs the sponsor's autoclaimer, which
        # wedges the sponsor's ethtxmanager head nonce MID-RUN. The script
        # heals the sponsor itself before exiting (consumes the wedged nonces
        # with benign no-op txs and HARD-asserts that a fresh deposit
        # autoclaims), but if it dies mid-leg the sponsor may stay wedged — so
        # nothing that depends on sponsor autoclaim may run after it. Its legs
        # are L1→L2-only (GER injection via aggoracle, no certificate
        # settlement), so running after cantina13's poison leaf is safe. The
        # optional ALLOWLIST_LEG=1 phase (restarts the proxy) is NOT enabled
        # here — it is only for disposable stacks, run manually.
        "$SCRIPT_DIR/e2e-manual-user-claim.sh"

        # #156 chaos MUST run LAST: it SIGKILLs the miden-node and the proxy
        # process repeatedly to prove acknowledged work self-heals with at most a
        # proxy restart. Running it earlier would disrupt every later test.
        "$SCRIPT_DIR/e2e-orphan-recovery-chaos.sh"
        ;;
    chaos)
        "$SCRIPT_DIR/e2e-orphan-recovery-chaos.sh"
        ;;
    recovery-scenarios)
        # #157 reviewer #7 — deterministic {node,proxy}×{GER,claim} crash-during-
        # proving recovery. DEDICATED gate (NOT in `all`): the GER cases require the
        # stack to be brought up with REJECT_UNVERIFIED_GER_INJECTION=false so a
        # controlled GER injects.
        "$SCRIPT_DIR/e2e-recovery-scenarios.sh"
        ;;
    tip-consistency)
        "$SCRIPT_DIR/e2e-rpc-tip-consistency.sh"
        ;;
    l1-to-l2)
        "$SCRIPT_DIR/e2e-l1-to-l2.sh"
        ;;
    l2-to-l1)
        "$SCRIPT_DIR/e2e-l2-to-l1.sh"
        ;;
    dynamic-erc20)
        "$SCRIPT_DIR/e2e-dynamic-erc20.sh"
        ;;
    cantina13)
        "$SCRIPT_DIR/e2e-cantina13-metadata-recovery.sh"
        ;;
    cantina10)
        "$SCRIPT_DIR/e2e-cantina10-concurrent-faucet.sh"
        ;;
    ger-decomposition)
        "$SCRIPT_DIR/e2e-ger-decomposition.sh"
        ;;
    ger-l1-verification)
        "$SCRIPT_DIR/e2e-ger-l1-verification.sh"
        ;;
    security)
        "$SCRIPT_DIR/e2e-security.sh"
        ;;
    cantina12-getlogs-returns-all)
        "$SCRIPT_DIR/e2e-cantina12-getlogs-returns-all.sh"
        ;;
    cantina6-faucet-identity-restore)
        "$SCRIPT_DIR/e2e-cantina6-faucet-identity-restore.sh"
        ;;
    fuzz)
        "$SCRIPT_DIR/e2e-fuzz-bridge.sh"
        ;;
    reconciler-private-note)
        "$SCRIPT_DIR/e2e-reconciler-private-note.sh"
        ;;
    reconciler-cursor)
        "$SCRIPT_DIR/e2e-reconciler-cursor-persistence.sh"
        ;;
    ger-atomic)
        "$SCRIPT_DIR/e2e-ger-atomic-commit.sh"
        ;;
    claim-provenance)
        "$SCRIPT_DIR/e2e-claim-provenance.sh"
        ;;
    manual-user-claim)
        "$SCRIPT_DIR/e2e-manual-user-claim.sh"
        ;;
    l2l2)
        # SIMPLE L2<->L2 pipeline group (NOT in `all` — needs the
        # docker-compose.l2l2.yml overlay: second rollup + aggkit-l2b on top of
        # the base stack). Ordered: forward (deploy+bridge L2B->Miden+claim,
        # foreign-origin faucet), clash (same-address L1-vs-L2B faucet isolation,
        # #108), then back (bridge-out Miden->L2B+claim, net-zero). forward brings
        # the L2B overlay up idempotently (reused if already registered) and hands
        # off a shared wallet + OPT0 state file the clash and back legs consume.
        #
        # Preflight ONCE up front (fail-loud: nothing runs against a
        # half-configured/port-colliding stack), then pin ONE per-run evidence
        # timestamp so forward+back write the SAME NDJSON file for this run (a
        # fresh invocation => a fresh file: 3x cert => 3 evidence artifacts).
        PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
        source "$SCRIPT_DIR/lib-l2l2.sh"
        export EVIDENCE_RUN_TS="$(date +%s)"
        l2l2_ensure_stack
        l2l2_validate_stack
        export L2L2_PREFLIGHT_DONE=1
        "$SCRIPT_DIR/e2e-l2l2-forward.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2l2-clash.sh"
        echo ""
        "$SCRIPT_DIR/e2e-l2l2-back.sh"
        ;;
    l2l2-clash)
        "$SCRIPT_DIR/e2e-l2l2-clash.sh"
        ;;
    l2l2-forward)
        "$SCRIPT_DIR/e2e-l2l2-forward.sh"
        ;;
    l2l2-back)
        "$SCRIPT_DIR/e2e-l2l2-back.sh"
        ;;
    *)
        echo -e "${RED}Unknown test: $test_filter${NC}" >&2
        echo "Usage: $0 [all|tip-consistency|l1-to-l2|l2-to-l1|dynamic-erc20|cantina13|cantina10|ger-decomposition|ger-l1-verification|security|cantina12-getlogs-returns-all|cantina6-faucet-identity-restore|fuzz|reconciler-private-note|reconciler-cursor|ger-atomic|claim-provenance|l2l2|l2l2-forward|l2l2-clash|l2l2-back]" >&2
        echo "       (l2l2 is optional and not part of 'all' — it needs the docker-compose.l2l2.yml overlay)" >&2
        exit 1
        ;;
esac

echo ""
log "======================================================================"
log "  ALL TESTS COMPLETE"
log "======================================================================"
