# Consumed-note scanner diagnostics

On September 15, 2026, a production export showed 14,798–14,829 consumed
notes on each sync. Loading them took about 0.40 seconds, but the monitor pass
took a median 36.05 seconds (maximum 42.79 seconds across 18 passes). The
monitor repeated per-note tracker writes and MINT history/route lookups over
the entire history. A fresh E2E store with 90–97 consumed notes took a median
0.086 seconds for that pass, so short-chain tests did not exercise this cost.
The logs identify the slow stage; they do not measure each database call or
establish the cause of individual missing claim receipts.

The v0.16.4 hotfix caches completed monitor observations (100,000 entries),
but still loads the full consumed-note inventory every sync. The subsequent
incremental-read change loads the inventory once after process start, then
reads changed records and explicit retries in batches of at most 256 notes.
The projector's historical calldata repair and completeness audit use the
same change-index mechanism, with an independent reader checkpoint.

A block-height cursor alone is insufficient: an old note can be imported,
consumed, or gain metadata/consumer attribution after that block was scanned.
An application-owned SQLite index records those SDK writes transactionally.
It keeps one revision row per changed details commitment, including deletions,
not one record per poll. Triggers cover the pinned SDK's INSERT OR REPLACE,
UPDATE and DELETE paths; unchanged replacement writes do not advance it.

Live reconciliation releases the Miden client between SDK operations. Tracker
SQL, node discovery RPCs and metadata work can wait while other client requests
run. SDK operations still have one serialized owner. Scanner and projector
passes stay ordered, and a new periodic sync starts five seconds after the
whole pass finishes. Actual SDK work (including imports, sync and proof work)
can still occupy the client; this change does not promise a maximum wait.

## Enable and interpret logs

Set this in the proxy's deployment environment and restart it through the
normal deployment procedure:

```text
RUST_LOG=info,bridge_out::scan=debug,writer_diagnostics=debug
```

Search Grafana/Loki for `consumed-note monitor pass completed`. Each pass has:

| Field | Meaning |
|---|---|
| `total` | Consumed records in this batch; the full inventory only on a cold/fallback pass |
| `scanned` | New, changed, evicted, or unresolved observations checked |
| `cached` | Completed observations whose expensive checks were skipped |
| `retryable` | Checked observations left uncached because a check is incomplete or the registry is unavailable |
| `cache_entries` | Completed observations retained in memory |
| `registry_changed` | Registry routing fingerprint differs from the previous pass; cached completions were invalidated |
| `registry_degraded` | Registry read failed; no completed observation is cached or skipped |
| `elapsed_ms` | Monitor work and registry fingerprinting; registry I/O precedes this pass |

Also search `consumed-note change batch loaded`:

| Field | Meaning |
|---|---|
| `reader` | `bridge_scanner` or `projector`; each acknowledges its own batches |
| `full_inventory` | Cold start, changed registry, recreated index, or safe fallback |
| `changed` | Distinct commitments changed since this reader's acknowledged revision |
| `retry` | Previously unresolved notes requested again |
| `extra` | Active expected-MINT tracker IDs checked even if consumption is historical |
| `loaded` | Consumed records actually returned for this batch |
| `revision` | Local SQLite change-index watermark; not a chain block number |

On a quiet chain, a warm batch should have `full_inventory=false` and
`loaded=0`, except for unresolved notes or active tracker IDs. An unresolved
MINT continues its grace ticks even without new note changes. A registry
change forces a full scanner pass; an unreadable registry forces fail-closed
full scanning until it recovers. Cache eviction can repeat checks when a note
is loaded again, but no longer causes a full inventory read on every tick.

`consumed-note change index unavailable; reading full inventory` and
`consumed_note_feed_fallback_total` identify fallback to the safe but slower
read. Capture the attached error, SDK version and image tag. `consumed_note_feed_reads_total{reader,mode}` counts
full/incremental reads, and `consumed_note_feed_retry_notes{reader}` shows
unfinished work. The index depends
on the pinned SDK SQLite schema and must be tested when upgrading that SDK.

For a short diagnostic capture with per-observation retries and MINT grace
reasons, use:

```text
RUST_LOG=info,bridge_out::scan=trace,bridge_out::forged_mint=trace,writer_diagnostics=debug
```

Search `consumed-note observation left retryable`, `MINT identity unresolved`,
`history write failed`, `history read failed`, `tracker store failure`, and
`faucet registry unreadable`. Capture several complete syncs and the existing
`bridge_scanner` stage timings. Include the image tag, restart time, note
counts, and whether recovery or faucet registration occurred during the
window. Trace logging can be noisy while a historical backlog is unresolved.

## Recovery and security invariants

- Reader checkpoints are process-local and acknowledged only after a batch
  completes; failed work replays, and unresolved observations stay retryable.
  Index recreation changes an epoch and forces another full inventory read.
- Metadata, attachments, and consumer attribution are part of an observation;
  enrichment or altered routing triggers another check, including twin detection.
- Registry membership and origin-token routes determine cache validity.
  Registry failures invalidate completions and retain fail-closed monitoring.
- Store failures do not complete the affected observation. Unmatched MINTs
  retain the existing grace/retry behavior and terminal alert deduplication.
- CLAIM history is recorded before MINT reconciliation, including on a cold
  replay after DB loss. Active expected-MINT tracker IDs are read explicitly,
  using both the full NoteId emitted by claim submission and legacy details
  commitments, so an already-consumed CLAIM re-registered later is recognized.
- Durable security tracker tables remain authoritative. Restart rebuilds the
  reader from inventory. Neither note bodies nor recovery history is deleted.
- Restore pauses future listeners and waits for an active pass to finish
  before resetting cursors. Its bounded authoritative block replay retains
  exclusive client ownership and does not depend on incremental-reader state.
- The live projector captures the sync tip and bridge account before queued
  requests can advance SDK state. Its LET gate uses that snapshot. It remains
  the sole event projector; releasing the client does not allow overlapping
  projection passes or relax visibility, order, cardinality or frontier gates.
