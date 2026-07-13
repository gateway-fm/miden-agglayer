# LET cardinality gate (Cantina #7, part 2)

The synthetic projector refuses to seal a block while the bridge account's on-chain
**Local Exit Tree leaf count** and the proxy's **feed-visible B2AGG consumption
accounting** disagree. A stalled tick is recoverable; a misnumbered `deposit_count`
(= LET leaf index = the claim's `globalIndex`) is poison — every later exit shifts
with it, and getLogs immutability seals the wrong numbering forever.

## The identity

Only a **bridge-executed consumption of a B2AGG note** appends a LET leaf. That
includes exits the proxy deliberately does NOT emit an event for — quarantined
(`unbridgeable_bridge_outs`), metadata-deferred (Cantina #13 `Unrecoverable`), and
self-targeted (#13 poison-leaf) exits all advanced the on-chain LET at consumption
time. Reclaims never touch bridge storage. So, measured at the chain tip:

```
read_let_num_leaves(bridge) == baseline + visible
```

- `visible` — B2AGG records produced by the AUTHORITATIVE feed
  (`sync_transactions` → the projector's resolve step) for sealed blocks plus the
  window about to be sealed. **Counting happens upstream of the emit gates**, so
  quarantine/deferral/self-target are all counted — a quarantine happening can
  never trip the gate (checkers must mirror emit gates).
- `baseline` — leaves attributed to pre-boot history, captured once when the gate
  **arms** (first tick whose projection ceiling reaches the chain tip). History is
  not re-derivable exactly from durable state (deferred and self-targeted skips
  leave no row by design), so it is absorbed rather than wrongly judged.

The gate **evaluates only at-tip** (`project_to == tip`, the steady-state norm —
every tick). While arming, during catch-up, or while the visibility barrier holds,
it stays out of the way: a fresh restore or a long catch-up can never halt.

## Verdicts and the retry policy

| Verdict | Meaning | Action |
|---|---|---|
| Aligned | identity holds | seal normally; strikes reset |
| `invisible_gap` | chain has leaves no visible consumption accounts for | emission **blocked immediately**; quiet retry for `LET_GATE_RETRY_TICKS` (default 5) ticks — visibility races heal; past the budget → **HALT loud** |
| `local_ahead` | about to emit more exits than the chain has leaves | **HALT immediately** — data corruption; retry cannot heal it |

Halt = `bridge_let_assignment_gate_halted_total{kind}` + a standing error each
tick + tick returns an error. Nothing seals, the cursor does not move; the block
is retried every tick, so a genuine heal (e.g. the reconciler importing the
missing note) resumes projection automatically at the exact blocks.

## Diagnosing a halt

1. Read the standing error: it carries `kind`, `gap`, `on_chain`, and the pending
   visible count.
2. Compare the ledgers yourself:
   - on-chain: `read_let_num_leaves` (the #9 monitor logs it as `on_chain`);
   - local: `SELECT COUNT(*) FROM synthetic_logs WHERE topics[1] = <BridgeEvent topic>`
     (== `deposit_counter`) **plus** `SELECT COUNT(*) FROM unbridgeable_bridge_outs`.
   - The difference between on-chain and that sum should equal the gate's `gap`
     plus any metadata-deferred / self-targeted skips (which have **no rows** —
     grep proxy logs for `bridge_out_self_targeted_total` /
     `metadata ... could not be recovered`).
3. Likely causes of `invisible_gap`:
   - a B2AGG consumption the node's `sync_transactions` feed never returned
     (feed omission — the class the gate exists to catch);
   - a note body unresolvable by the projector (see
     `synthetic_projector_b2agg_fetch_missing_total` — a loud-skipped exit is a
     dropped BridgeEvent AND an unaccounted leaf);
   - node/store rollback skew after a restore.
4. `local_ahead` means the local store double-counted (crash-replay bug, foreign
   note misattributed as bridge-consumed) — treat as corruption; do not restart
   into it repeatedly, snapshot the store and escalate.

## Relationship to the Cantina #9 monitor

`run_let_divergence_check` (alarm-only, post-emit) remains as the independent
second view — now quarantine-aware (`deposit_count + unbridgeable rows`). It still
alarms `on_chain_ahead` for deferred/self-targeted history (no durable rows); the
gate's baseline absorbs those instead. An invisible leaf that predates the current
boot is absorbed into the gate's baseline and only the #9 monitor will show it —
that is the deliberate trade against false halts on by-design states.
