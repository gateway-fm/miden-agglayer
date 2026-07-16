# LET cardinality gate

The projector seals synthetic blocks only when it can assign every Miden-to-AggLayer
`BridgeEvent` its exact Local Exit Tree (LET) index. A wrong `depositCount` produces a
wrong `globalIndex`, and sealed `eth_getLogs` history cannot be repaired in place.

At the Miden tip, the gate requires:

```text
bridge LET leaves
  == let_gate_baseline + deposit_counter + current unreserved B2AGG leaves
```

- `deposit_counter` counts durable reservations, including leaves that emit no event
  because they are quarantined, deferred, or self-targeted.
- Current unreserved leaves are bridge-consumed B2AGG NoteIds in the projection window
  that have not yet received a durable reservation.
- `let_gate_baseline` is an explicit offset for pre-upgrade LET leaves that are absent
  from `deposit_counter`. It defaults to `0` and is never inferred at runtime.

If note reconciliation is behind the Miden tip, the projector waits without sealing an
older frontier. At the tip, a missing bridge account or either cardinality mismatch
returns an error before any block is sealed. The normal projector retry runs the same
check again; there is no strike counter or persisted halt state.

## Upgrade procedure

Most deployments should leave `let_gate_baseline` at `0`. If an existing database has
historical LET leaves that did not advance `deposit_counter`:

1. Stop the proxy.
2. Compare the bridge account's LET leaf count with the durable reservations and audit
   the difference against known pre-upgrade skipped leaves. The offset is safe only for
   an unrepresented trailing suffix, or after proving every existing event already has its
   exact LET index. A missing interior leaf means sealed later events are already shifted;
   rebuild that history instead of adding an offset.
3. Set only the verified safe difference:

   ```sql
   UPDATE service_state
   SET let_gate_baseline = <verified_offset>
   WHERE id = 1;
   ```

4. Restart the proxy and confirm the gate stays aligned.

Never derive the offset from a live pending projection window, and never change it just
to clear a mismatch. Snapshot the database and investigate the node transaction feed,
note-body availability, cursor state, and reservations first.

A full `--restore` replays the complete LET from index `0` and therefore requires
`let_gate_baseline = 0`. A deployment using a nonzero audited offset needs an offline
history reconstruction; do not change the offset to force a restore through.

## Signals

`bridge_let_assignment_gate_halted_total{kind}` increments for:

- `invisible_gap`: the bridge LET has more leaves than local accounting;
- `local_ahead`: local accounting has more leaves than the bridge LET.

`synthetic_projector_b2agg_fetch_missing_total` identifies node lookups that returned no
body for an identified bridge input. That omission directly fails the tick before the
gate. Cardinality remains an independent defense for an unmapped/headerless exit.
