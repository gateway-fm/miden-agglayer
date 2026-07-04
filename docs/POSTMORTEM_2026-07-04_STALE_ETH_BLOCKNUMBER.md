# Postmortem: eth_blockNumber frozen at a stale tip (2026-07-04)

## Impact
During the N=250 strict loadtest, `eth_blockNumber` returned **659** while the
synthetic tip was **2702**. Delivery was unaffected (250/250 claimed — aggkit
derives its scan ranges from store-backed paths), but the event-completeness
verifier used `eth_blockNumber` as its `eth_getLogs` scan bound, truncating the
window to blocks 0–659 and mis-reporting ~83% of events as missing across all
three types. Post-hoc verification with a corrected bound: **PASS — 127/127
B2AGG, 133/133 CLAIM, 158/158 GER, all exact-block**.

## Root cause: two individually-correct features, incompatible when combined
1. **RD-940 Phase 3** optimized `eth_blockNumber` to hot-read a `BlockMonitor`
   AtomicU64 tip mirror (`current_tip()`), falling back to the store only while
   the mirror is 0. The mirror was kept fresh by the **writer worker**
   (`record_tip` in `writer_worker.rs`) — its only steady-state updater.
2. **The synthetic-indexer redesign (reopen-92)** made the SyntheticProjector
   the **sole advancer** of the synthetic tip. The projector never calls
   `record_tip()`. With the writer worker disabled (default), *nothing* updates
   the mirror after the cold-boot fallback seeds it once.

Result: the first `eth_blockNumber` call seeded the mirror with the then-current
tip (659) and every subsequent call served that frozen value. `current_tip()`
had exactly one consumer (`service.rs` `eth_blockNumber`); all other `latest`
paths (`eth_getBlockByNumber`, log synthesis, block-tag resolution) read the
store directly and stayed correct — which is why only measurement, not
delivery, was affected. Earlier rungs (N=10/25/50) passed by timing: the mirror
was seeded late enough to cover their ranges.

## Detection
The N=250 verifier reported mass `missing` with contradictory facts (100%
delivery — impossible if events were absent). Cross-check showed the projector
log at `miden_tip == projector_cursor == synthetic_tip == 2702` while
`eth_blockNumber` returned 659; grep proved `record_tip` had no live caller.

## Fixes (this commit)
- `eth_blockNumber` reads the store (single source of truth) and refreshes the
  mirror for any writer-mode consumers. The Phase-3 micro-optimization is
  forfeited until a caller that actually maintains the mirror exists.
- `verify-event-completeness.sh` scan bound is now
  `max(eth_blockNumber, node-snapshot tip)` — a stale tip can never truncate
  the audit window again (defense in depth; it would have caught this class).

## Lessons
- A cache is only as correct as its *active* invalidation path; feature flags
  (writer worker off) can silently orphan a cache's only writer.
- Verifiers must not derive their measurement window from the system under
  test's own possibly-buggy view (use independent truth — the node DB tip).
- "Impossible" verifier results (missing events + 100% delivery) mean the
  instrument, not just the system, must be suspected.

## Follow-ups
- [ ] RD-940 owners: either have the projector call `record_tip()` or retire
      the mirror fast-path entirely.
- [ ] ntx-builder crash-loop during the run (h2 `error reading a body from
      connection`, RestartCount=4) — node-side, tolerated by design (retries),
      upstream issue worth filing.

## Addendum (same day): reproduced on v0.15.2 — production impact
During the upgrade-path test's seed phase, the **unmodified v0.15.2 proxy**
showed the same signature (`eth_blockNumber=25` vs `latest.number=446`). The
redesign is NOT a necessary ingredient: any deployment running RD-940 Phase 3
with the writer worker disabled (the default) has a frozen `eth_blockNumber`
today, including production v0.15.2. Downstream impact is limited because
aggkit and the bridge-service derive ranges from store-backed paths — but any
client trusting `eth_blockNumber` (health checks, explorers, tooling) sees a
frozen tip. The fix in this branch (store-backed read) plus the
`e2e-rpc-tip-consistency.sh` liveness gate cover both eras; backporting the
one-line fix to a 0.15.2 point release is recommended.
