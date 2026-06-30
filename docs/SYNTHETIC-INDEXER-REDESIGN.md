# Synthetic Indexer Redesign — the Miden chain as the single source of truth

**Status:** in progress · **Branch:** `feat/synthetic-indexer-redesign`
**Fixes:** Finding #5 (non-atomic synthetic block allocation) · subsumes Finding #13 Layer 2 (recovery)

## Problem

Today three writers — `bridge_out::on_post_sync`, `claim::publish_claim`, `ger::insert_ger` —
generate the synthetic EVM chain (blocks + `BridgeEvent`/`ClaimEvent`/GER logs) as a **side
effect** of submitting work to Miden, each reserving block numbers ad-hoc with
`get_latest_block_number() + 1`. Consequences:

- **Finding #5:** block-number reservation is non-atomic and outside the store txn. Two writers
  read the same tip, both pick `N+1`, and a late log lands in an already-observed block. The
  synthetic block hash commits only to (number, parent), not the log set, so the late log is
  invisible — a committed bridge-out is hidden from the destination claim flow.
- **Duplication:** `restore_bridge_outs` / `restore_claims` / `restore_gers` already reconstruct
  the *same* events by parsing the Miden chain. Live and recovery are two code paths for one
  derivation.
- **Recovery is a special case** (Finding #13 Layer 2 etc.) instead of the normal path.

## Target: two single-threaded workers

1. **Submitter** (worker 1) — submits txs to Miden (CLAIM notes, GER injections). Bridge-outs are
   user-initiated B2AGG notes. **Worker 1 emits no synthetic events and never touches the tip.**
2. **Projector / Indexer** (worker 2) — the *sole* owner of the synthetic EVM chain. Follows the
   Miden chain block-by-block on a persisted cursor. For each new Miden block `N`, it scans the
   consumed notes attributed to `N` (`nullifier_block_height == N`), derives the synthetic events
   in deterministic order, and emits exactly one synthetic block `N`. Numbering is **Miden-1:1**
   (see "Numbering: Miden-1:1 (final)" below): synthetic block `N` == Miden block `N`, the tip
   advances to `N` even for empty Miden blocks, so `eth_blockNumber` tracks the Miden tip.

Single ordered projector ⇒ **no reservation, no race** (Finding #5 eliminated by construction).
Catch-up (cursor → tip) **is** recovery **is** the normal loop.

## Contract (invariants the projector MUST hold)

- **Determinism.** Synthetic block `N` is a pure function of Miden block `N`'s consumed notes.
  Re-running over the same chain yields byte-identical synthetic blocks (numbers, hashes, log
  order, log indices). Intra-block events are ordered by `(consumed_tx_order, note_id)`.
- **Cursor.** A persisted "last projected Miden block height". Re-processing a block is idempotent
  (existing `is_*_processed` dedup keys are kept).
- **Atomic visibility.** A synthetic block's logs are all written **before** the tip advances to it
  (one unit of work in the projector). No reader can see tip ≥ N without the block-N logs.
- **Block hash commits to logs** (upgrade): make `SyntheticBlock::build_header` mix in the logs
  root so a changed log set changes the hash — permanently closes Finding #5's invisibility hole.
- **Single-process only.** The projector is in-process; **multiple replicas are NOT supported.**
  Documented loudly + asserted at startup.
- **Mapping.** synthetic block `N` ⟷ Miden block `N`. An empty Miden block → an empty synthetic
  block (monotonic, gap-free).

## Event derivations (all already exist)

| Synthetic event | Source on Miden chain          | Existing code                          |
|-----------------|--------------------------------|----------------------------------------|
| `BridgeEvent`   | consumed B2AGG notes           | `restore_one_b2agg_note` (+ faucet metadata, Layer 1/2) |
| `ClaimEvent`    | consumed CLAIM notes           | `restore_claims` + `parse_claim_event_from_storage`     |
| GER hash-chain  | consumed `UpdateGerNote`s      | `restore_gers` (MA#28)                 |

The projector unifies these three per-note derivations into one cursor-driven loop.

## Receipts — the submit ⟂ project handoff

The proxy exposes an EVM-compatible RPC surface; aggkit and the bridge stack fetch
**transaction receipts** (`eth_getTransactionReceipt`). Today the writer produces the synthetic
block + logs synchronously at submit, so the receipt is ready immediately. Under the projector the
receipt lifecycle **splits in two**, and that split is what keeps "sending" decoupled from
"projecting":

- **Submit (worker 1).** Submits the CLAIM/GER note to Miden; cares only that Miden *accepts* it.
  On acceptance it records a **pending receipt** keyed by the caller's EVM `tx_hash` (status known;
  `blockNumber`/`blockHash`/`logs` still empty). On Miden *rejection* it returns an error — no
  receipt. Worker 1 never touches the synthetic tip or produces logs.
- **Projection (worker 2).** When the projector observes that note **consumed** in Miden block `N`,
  it derives the synthetic log and **completes** the receipt: `blockNumber = N`, block hash, `logs`,
  `transactionHash`, `logIndex`, `status = success`. A receipt is immutable once complete.
- **`eth_getTransactionReceipt`** returns `null` until the projector reaches block `N`, then the
  full receipt — i.e. **standard "wait for the tx to be mined."** aggkit already polls receipts, so
  the ≈1-block lag is normal EVM async, and the receipt now reflects *finalized* Miden state instead
  of an optimistic guess.

### Linking a consumed note back to its receipt

The projector must complete the *right* receipt. Two cases:

- **Bridge-outs** — no caller tx; the synthetic `tx_hash` is already a deterministic function of the
  note (`derive_bridge_out_tx_hash(note_id)`). The projector re-derives it from the consumed note —
  **no mapping needed.**
- **Claims / GERs** — the caller signed a real `claimAsset` / `insertGlobalExitRoot` tx and holds
  *its* hash. So worker 1, at submit, writes a small durable mapping **`evm_tx_hash → note
  commitment`**; the projector looks it up when it consumes the note.

That map is the **only** state the two workers share, and it is a **first-write associative map,
not a shared counter** — it carries none of Finding #5's race. The Miden chain remains the real
handoff; the map only answers "which receipt does this note belong to."

### Edge cases (all clean)

- **Rejected at submit** → immediate error, no receipt.
- **Accepted but never consumed** (stuck note) → receipt stays pending → expires, like a dropped EVM
  tx. The existing receipt store already carries `expires_at` on its `TxnReceipt` LRU.
- **Crash between submit and projection** → the mapping is durable and the note is on-chain, so the
  projector completes the receipt during catch-up (recovery ≡ live again).

### Contract

A receipt is `pending` from submit until the projector reaches the note's Miden block, then
`complete` and immutable. **The projector never produces a receipt for a note it has not observed
consumed.** This is the explicit cutover target for Phases 2–3.

## Phased migration — every phase gated by the FULL e2e regression matrix

- **Phase 0 — foundation.** This doc + the cursor/ordering contract + a `SyntheticProjector`
  skeleton (no behavior change). _(in progress)_
- **Phase 1 — projector core, SHADOW mode.** Build the block-by-block projector (unify the three
  `restore_*` derivations, keyed on `nullifier_block_height`, deterministic ordering). Run it in
  **shadow**: it projects into a side store and we assert its output **equals** the live writers'
  output. No production behavior change yet. Gate: full e2e green + shadow equality.
- **Phase 2 — cut over claims.** Switch the live claim path to the projector (the claim watcher
  already observes the chain), remove `publish_claim`'s synthetic-event side-effect + its tip
  management. Gate: full e2e green.
- **Phase 3 — cut over bridge-outs, then GERs.** Same for the other two writers; remove all ad-hoc
  block-number reservation. Worker 1 becomes submit-only. Gate: full e2e green after each.
- **Phase 4 — unify restore.** `restore_*` becomes the projector's catch-up (delete the duplicated
  path). Land the block-hash-commits-to-logs upgrade. Gate: full e2e green incl. recovery suites.

## e2e acceptance — use them all

At every phase boundary, run the **entire** regression matrix (all suites: `e2e-dynamic-erc20`,
`e2e-l2-to-l1`, recovery/restore, GER, claim-watcher, rd-* dedup, …). The redesign is correct iff:
1. the full matrix is green, **and**
2. in shadow mode (Phase 1) the projected synthetic chain is byte-identical to the legacy output.

No phase ships unless both hold.

## Implementation outcome (landed)

The migration is complete: the `SyntheticProjector` is the **sole** synthetic-event producer and
the **sole** advancer of `latest_block_number`. The feature flag, the per-writer
`suppress_synthetic_emission` gates, the `ClaimWatcher`, the `StoreSyncListener` tip-advance, and
the non-atomic `commit_*_event_atomic` reservation primitives have all been deleted — Finding #5 is
eliminated by construction (no `get_latest()+1` reservation exists anymore).

### Numbering: Miden-1:1 (final)

Synthetic block `N` == Miden block `N`. Every synthetic log derived from the notes consumed at
Miden block `N` is written at synthetic block `N`; the tip is advanced to `N` exactly once, **after**
the block (write-before-advance), **including for empty Miden blocks**, so the synthetic chain
mirrors the Miden chain block-for-block and `eth_blockNumber` tracks the Miden tip. (An earlier
"one synthetic block per emitted log" variant was rejected because it raced the legacy
height-tracking and produced tip/log inconsistencies.)

### Bugs found + fixed during the full-matrix validation (each unit-tested)

1. **GER limb byte-order.** `project_ger_note` decoded the `UpdateGerNote` storage felts big-endian;
   the convention is little-endian (matching `ExitRoot::to_elements` / bridge_out / claim_note), so
   every emitted GER was byte-swapped and never matched the GER aggkit injected — bridge-in deposits
   hung on `ready_for_claim`. Fixed to `to_le_bytes`; round-trip test against `ExitRoot::to_elements`.
2. **Synthetic-log receipt fallback.** Synthetic logs carry derived tx hashes with no real txn
   record, so `eth_getTransactionReceipt` returned `null`; aggkit's L2BridgeSyncer fails to append a
   logged tx with a null receipt (`input too short: 0 bytes`) and stalls. `service_get_txn_receipt`
   now synthesises a success receipt from `logs_by_tx` when there is no txn record.
3. **Claim tx-hash linkage.** aggkit decodes the claim tx's `claimAsset` calldata to resolve the
   claim's GER boundary. The projector emitted the ClaimEvent under a derived hash whose synthetic tx
   has empty calldata → no boundary → no certificate. `publish_claim` now records
   `record_tx_note_link(real_claim_tx_hash ↔ note.details_commitment)`; `project_claim_note` emits
   the ClaimEvent under the **real** claim tx (calldata + receipt present), falling back to the
   derived hash only for unlinked notes.
4. **Cantina #13 self-target gate (cutover-extraction gap).** Extracting the B2AGG derivation into
   the shared `project_b2agg_note` had silently dropped the legacy scanner's self-target poison-leaf
   gate — refuse to emit a BridgeEvent for a B2AGG whose `destination_network == local_network_id`.
   The e2e cannot catch this (a malicious-input case). Restored on the projector path (threading
   `local_network_id`) with a regression test, *before* deleting the legacy `process_consumed_note`,
   so the cutover does not ship a security regression.

### Restore

`restore_*` is the projector's catch-up over the same shared derivations. It reconstructs only the
**Miden-derived** synthetic state (logs, GER hash-chain, bridge-out tracking, tip). The eth-side
`transactions` / `transaction_logs` / `tx_note_links` (the proxy's record of `eth_sendRawTransaction`
calldata + receipts) are **durable** — they never existed on Miden and a real restart preserves the
Postgres volume — so the recovery suite preserves them rather than wiping them. (A true full-disk
loss cannot recover the claim `claimAsset` calldata from Miden, since the CLAIM note storage keeps
only the metadata *hash*; that is a documented recovery limitation, not something restore can close.)
