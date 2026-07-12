# Unified projector — authoritative per-block consumption sourcing

**Goal:** make the strong invariants (`getLogs` immutability, 0 late notes, reconcile-before-project)
hold **by construction** instead of by enforcement, by removing the store-consumption *lag* that is
the root of every late note. See `docs/SYNTHETIC-INDEXER-REDESIGN.md` → STRONG invariants.

## The root problem (why late notes exist)

The projector reads consumptions from the **miden-client store**, which is filled by two independent,
eventually-consistent processes:
- the **reconciler** imports note *bodies* via `sync_notes` (creation feed), advancing `reconcile_cursor`;
- **`sync_state`** discovers *consumptions* via a nullifier scan.

"bodies imported", "consumption known", and "projected" are three different frontiers moving at
different speeds. Every late note = those frontiers out of step. The #30 barrier, the one-tick lag,
Option 3, and the late-consumption sweep are all machinery to paper over that skew.

## The fix: source consumptions authoritatively per block

A finalized Miden block `N` (`N ≤ tip`) holds its **complete, immutable** consumption set — every
nullifier spent in `N` is baked into the block. Read that, not the lagging store.

**New `project_block(N)`:**
1. **B2AGG (BridgeEvents):** `sync_transactions(N, N, [bridge_id])` → the bridge's txs in block `N` →
   their consumed nullifiers = the *authoritative, complete* set of B2AGG consumptions at `N`
   (MA#3: consumer == bridge is exactly what `classify_b2agg_consumer == Emit` encodes). For each
   nullifier, resolve the note **body** (details) from the imported store notes (keyed by nullifier;
   `get_notes_by_id` as authoritative backfill if a body is missing), derive the `BridgeEvent`
   (`restore_one_b2agg_note`), emit at block `N`. Order intra-block by `(consumed_tx_order, note_id)`.
2. **CLAIM / GER:** proxy-created notes — the proxy *makes* these when it processes claims / GER
   injections, so they are known synchronously with no lag. Keep the current derivation unchanged.
3. Advance the synthetic tip to `N`.

## Barrier: what stays, what goes

- **KEEP** `reconcile_cursor` = "note **bodies** imported up to here" (the `sync_notes` sweep). A note
  consumed at `N` was created at `C ≤ N`, so `reconcile_cursor ≥ N` ⇒ its body is imported. Gate
  `project_to = min(tip, reconcile_cursor)` so a body is always present when we project its
  consumption. This frontier is reliable (creation feed), unlike the consumption frontier.
- **DELETE** everything that existed only to fight the *consumption* lag:
  - the late-consumption sweep + `swept` cache + `projector_late_sweep_anomaly_total`
    (structurally impossible now — consumptions are authoritative, never late);
  - `confirm_window_consumptions` (Option 3) + the one-tick lag (`reconcile_cursor_prev`);
  - `recover_spent_before_import` + `direct_recovered` queue (subsumed: consumptions are sourced
    directly, not "recovered");
  - the store-`Consumed`-feed B2AGG path in `project_block` (replaced by `sync_transactions`).

## Correctness / invariants

- **getLogs immutable:** block `N` is projected from `N`'s finalized consumption set, which never
  changes ⇒ its log set never changes.
- **0 late, all types:** there is no second (consumption) frontier to lag ⇒ the late-sweep cannot
  fire; it is deleted, not silenced. Non-B2AGG notes are never consulted (they aren't bridge txs).
- **reconcile-before-project:** projecting `N` *is* reconciling `N` (authoritative fetch) — one step.
- **Determinism unchanged:** same `(consumed_tx_order, note_id)` ordering; `is_*_processed` dedup kept.

## Risks / open items

- **Note-body availability:** relies on `reconcile_cursor ≥ N` ⇒ body imported. If `sync_notes` ever
  misses a created note, `get_notes_by_id` (authoritative) backfills before emit; fail-closed if a
  body truly cannot be fetched (never emit a BridgeEvent without the note).
- **Perf:** `sync_transactions(N,N,[bridge])` per block. Batch per tick over `[cursor+1, project_to]`
  in one `sync_transactions(cursor+1, project_to, [bridge])` call (it already accepts a range), then
  bucket by `block_num` — O(1 RPC/tick), same cost model as today's single consumed-feed fetch.
- **Nullifier→body map:** build once per tick from the imported store notes (`get_input_notes(All)`
  → map by `nullifier()`), same as today's single feed fetch.
- **Validation:** unit tests for the derivations stay; **3× full e2e** (this touches the hot path) +
  the `getLogs` immutability monitor + `anomaly == 0` (the sweep is gone, so the metric must simply
  not exist / be 0) + N=30.
