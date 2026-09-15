# Consumed-note scanner diagnostics

On September 15, 2026, a production export showed 14,798–14,829 consumed
notes on each sync. Loading them took about 0.40 seconds, but the monitor pass
took a median 36.05 seconds (maximum 42.79 seconds across 18 passes). The
monitor repeated per-note tracker writes and MINT history/route lookups over
the entire history. A fresh E2E store with 90–97 consumed notes took a median
0.086 seconds for that pass, so short-chain tests did not exercise this cost.
The logs identify the slow stage; they do not measure each database call or
establish the cause of individual missing claim receipts.

The hotfix retains the full consumed-note inventory read, but caches completed
monitor observations in a bounded, process-local cache (100,000 entries).
Unchanged completed notes avoid the repeated database work. This does not
change event projection, receipt synthesis, or either recovery cursor.

## Enable and interpret logs

Set this in the proxy's deployment environment and restart it through the
normal deployment procedure:

```text
RUST_LOG=info,bridge_out::scan=debug,writer_diagnostics=debug
```

Search Grafana/Loki for `consumed-note monitor pass completed`. Each pass has:

| Field | Meaning |
|---|---|
| `total` | Consumed notes in the current client inventory |
| `scanned` | New, changed, evicted, or unresolved observations checked |
| `cached` | Completed observations whose expensive checks were skipped |
| `retryable` | Checked observations left uncached because a check is incomplete or the registry is unavailable |
| `cache_entries` | Completed observations retained in memory |
| `registry_changed` | Registry routing fingerprint differs from the previous pass; cached completions were invalidated |
| `registry_degraded` | Registry read failed; no completed observation is cached or skipped |
| `elapsed_ms` | Entire monitor pass, including registry read and fingerprinting |

After the first successful pass, a quiet chain should show mostly `cached`
observations. A restart deliberately produces a cold pass. Registry changes
also force a cold pass. Beyond the cache capacity, evicted observations are
checked again, so this optimization does not eliminate all history-dependent
cost or the inventory read itself.

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

- No persisted skip cursor: old notes imported later are still discovered.
- Metadata, attachments, and consumer attribution are part of an observation;
  enrichment or altered routing triggers another check, including twin detection.
- Registry membership and origin-token routes determine cache validity.
  Registry failures invalidate completions and retain fail-closed monitoring.
- Store failures do not complete the affected observation. Unmatched MINTs
  retain the existing grace/retry behavior and terminal alert deduplication.
- CLAIM history is recorded before MINT reconciliation, including on a cold
  replay after DB loss. Cached CLAIMs still feed expected-MINT consumption
  tracking on every tick.
- Durable security tracker tables remain authoritative. Process restart and
  cache eviction re-run checks; the cache is not recovery state.
