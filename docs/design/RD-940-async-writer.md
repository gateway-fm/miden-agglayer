# RD-940 — Consolidated design spec (async writer worker, BlockMonitor unification, ClaimGuard cancellation)

> **Source:** Synthesis of Specs A–G (MCPlexer task notes on `01KSMS5PAWCCM22H50DHQMC4MV`, `01KSMS68THXQQS85ZSZSVVEH9A`, `01KSMS6V2ZXNGWV3D1TH9KXCAC`, `01KSMS7DDNBRWP6ARQQ476DTT4`, `01KSMS7YTV4J89DKRMZ3152E2A`, `01KSMS8H3ZKZZ7NDCBN8ZH8APK`, `01KSMS94YBBHR8VHV7WWQ17WKB`). Date: 2026-05-27. Repo: `gateway-fm/miden-agglayer`.
>
> This is the design-of-record for the implementation that ships under [`feat/rd-940-async-writer`](https://github.com/gateway-fm/miden-agglayer/tree/feat/rd-940-async-writer). Linear ticket: [RD-940](https://linear.app/gateway-fm/issue/RD-940).

## Ship status (updated 2026-07-15)

The implementation lands the writer worker + RPC wire contract + observability surface as a **flag-gated default-off** rollout. The full BlockMonitor unification (§2.3) remains explicitly deferred.

| Section | Status | Commit |
|---|---|---|
| §1 system overview | ✅ shipped | Phase 0 (`2ba4b92`) — design doc, flag wiring |
| §2.1 writer worker + queue | ✅ shipped | Phase 1 (`95d6e4f`) |
| §2.2 ClaimGuard placement | ✅ shipped — ClaimGuard taken inside `worker_handle_claim_asset`, dispatched on the worker task | Phase 1 (`95d6e4f`) |
| §2.3 BlockMonitor unification | **⏸️ deferred** — see "Phase 3 deferred" | follow-up PR |
| §2.4 RPC wire contract | ✅ shipped — geth pending shape, top-level null pre-commit, `eth_getBlockByNumber("pending")` aliased | Phase 4 (`ae35fe9`) |
| §3 consumer interop | ✅ shipped — `-32005` mapping, idempotent re-broadcast, `eth_getTransactionCount` tag honour | Phase 1+2 (`95d6e4f`, `e1dfe0c`) |
| §4 metrics + observability | ✅ shipped — 8 metrics, tracing spans per job, graceful drain, `dropped_on_restart` tmpfile | Phase 5 (`9da5aa2`) |
| §5 unit tests | ✅ shipped — admission, receipt, nonce, lease, and claim-fence regressions | Phases 1–4 + #55 hardening |
| §5 e2e scripts | **⏸️ deferred** — require live fixture environment; will land in follow-up | follow-up PR |
| `service_get_txn_receipt.rs:40` `from` co-fix | ✅ shipped | Phase 4 (`ae35fe9`) |
| 5-minute writer TTL (Decision 5) | ✅ shipped — consumer expires queue-aged work before dispatch; sweeper only renews leases and evicts terminal cache entries | Phase 4 (`ae35fe9`) + crash-safety hardening |
| `docs/operations/runbook.md` + `monitoring.md` | ✅ shipped — Failure Mode I + 8-metric alert table | Phase 7 |
| Default flag flip false→true | **⏸️ deferred** — operational rollout in a follow-up after canary validation | follow-up PR |

### Phase 3 deferred — BlockMonitor unification

The atomic-swap structural refactor that absorbs `BlockState` + `StoreSyncListener` + the inline emitters in `claim.rs`, `ger.rs`, `bridge_out.rs`, `claim_watcher.rs` into a single `BlockMonitor::record()` writer is held back as a focused follow-up PR. Reasons:

1. **High blast radius** — touches 9+ source files, all on the synthetic-log emission hot path. The existing log-first/cursor-second ordering at the four call sites is correct today; the value of BlockMonitor is making that ordering a structural invariant, not a tribal-knowledge comment. That's a real cleanup but it isn't load-bearing for the async-writer functionality.
2. **Diff-size hygiene** — bundling it with the worker + RPC + observability work would push this PR past the reasonable review threshold and make rollback granularity worse.
3. **Worker already concentrates writes** — the new `worker_handle_claim_asset` + `worker_handle_ger_insert` already serve as the single dispatch surface under the worker-enabled path; the most error-prone surface BlockMonitor was guarding against is moot when there's only one dispatch entrypoint.

The follow-up PR will own §2.3 in its entirety + the cross-emitter race elimination claims under that section. The RD-862 cure (L1InfoTreeIndexer + `commit_ger_event_atomic` UPSERT) is unaffected by either choice.

### Phase 6 deferred — e2e scripts

The six new `scripts/e2e-rd940-*.sh` scripts described in §5 (async-submit golden path, pending-receipt wire shape, queue-backpressure 600 req/s, restart-inflight, worker-panic, claim-guard-cancellation) require a running fixture environment (miden-node + bridge contracts + aggkit) that can't be stood up from a code review. They will land in a follow-up PR alongside `scripts/setup-iaic-fixture.sh` that lets the IAIC regression sentinel run strict in CI.

Unit tests cover the load-bearing per-component invariants. Two failure modes still require a live-stack test:

- aggkit ethtxmanager-loop interaction under real wire JSON (the `insta` snapshot test in `build_inflight_pending_tx_json_emits_geth_wire_shape` pins the Rust-side shape; an aggkit-side parse smoke is the follow-up).
- 50 req/s × 10 min v1 acceptance gate (drop-rate <1%, p99 < 60 s) needs a real Miden node — runs on bali staging once the flag is enabled.

## TL;DR

The async write path performs cheap request validation, reserves `(signer, nonce)`, persists the full signed envelope as an unlinked pending intent, CAS-advances the nonce, and then enqueues into a bounded `tokio::sync::mpsc(64)`. A single worker dispatches through `MidenClient::with(...)`; the SyntheticProjector finalises the receipt and event after note consumption. The mpsc is a dispatch buffer, not the durability boundary.

**Five load-bearing decisions** (resolved cross-spec conflicts, see §6):

1. **The fenced `ClaimGuard` is acquired inside the worker, not at enqueue.** Before external submission, its owner/fence and exact note handoff are sealed durably; stale owners cannot release or overwrite a successor.
2. **Durable intent precedes nonce acceptance.** The handler idempotently persists the full signed envelope before the nonce CAS. The mpsc is only a dispatch buffer: after restart, a same-hash rebroadcast reconstructs and re-enqueues an unlinked pending intent without advancing the nonce twice.
3. **`eth_sendRawTransaction` classifies known hashes _before_ R4 nonce rejection.** In-flight, handed-off, and terminal hashes deduplicate to `Ok(hash)`; an unlinked pending intent is the one exception and resumes dispatch without advancing its nonce twice.
4. **`eth_getTransactionCount` finally honours its block tag.** `latest` = next-committed; `pending` = next-accepted. Current code ignores the tag (`src/service.rs:370-377`) — claim-sponsor's `nonce_cache.go:35` reads `latest` and will race itself if pool-queued txs leak into `latest`.
5. **5-minute owner-local queue TTL and terminal-cache TTL.** After dequeue, the consuming worker fails an over-age queued item _before_ constructing its dispatch future. Once dispatch starts, no independent sweeper may publish `status:0x0`: `MidenClient::with(...)` hands the closure to another task, so dropping the outer waiter does not cancel the Miden operation. A completed dispatch error may fail only when no durable exact-note handoff exists; ambiguous handed-off work remains pending for projector/reconciler recovery. Terminal cache entries are evicted after the same TTL.

**v1 acceptance gate:** 50 req/s × 10 min, drop-rate < 1%, p99 worker-job-duration < 60 s (inside aggkit's 2 m `WaitTxToBeMined`), zero `agglayer_writer_dropped_on_restart_total`, zero per-signer nonce gaps. 500 req/s stays as a nightly stress benchmark — at queue cap 64 and p50 commit ≈ 10 s, sustainable throughput tops out near 6 jobs/s by design.

**Regression sentinel:** `scripts/e2e-iaic-mempool-conflict.sh MODE=expect_no_iaic PARALLEL=10`. If it goes red post-RD-940, the channel-of-1 invariant is broken and IAIC is back. Wire it into `make test-e2e-coverage` alongside `repro-rd862`; ship `scripts/setup-iaic-fixture.sh` so it runs strict in CI (today it SKIPs without the manually-built fixture).

## 1. System overview

```
                       HTTP request thread                             writer worker (single)                Miden node
                     ─────────────────────────                       ─────────────────────────              ─────────────
service_send_raw_txn                                   mpsc(64)
  │ decode + deterministic validation                   ──▶  recv  ──▶  fenced ClaimGuard
  │ R4 chain_id                                                          │ MidenClient::with(...)
  │ R2 signer allow-list                                                 │   seal claim fence + note link
  │ per-signer lock + nonce equality                                     │   submit_proven_transaction ──▶ apply
  │ tx-hash/durable-intent classification                                │ SyntheticProjector
  │ reserve (signer, nonce) lease                                        │   = receipt/event finalisation
  │ txn_begin_if_absent(full envelope)
  │ nonce_advance_cas
  │ try_send(WriteJob)        ───────────────────────────────────────────┘
  │ release per-signer lock
  ▼
return tx_hash
```

The shipped `BlockMonitor` is currently a monotonic tip mirror over `BlockState`. Full ownership of block headers, listeners, and event writers remains the explicit Phase-3 follow-up below.

In-flight map (`DashMap<TxHash, JobState>`) is a hot read cache backing `eth_getTransactionByHash` for not-yet-committed envelopes; it is **not** a durability boundary. Entries linger 5 min past terminal then a sweeper evicts (mirrors `L1InfoTreeIndexer::spawn`, `src/l1_info_tree_indexer.rs:124-223`).

## 2. Component design

### 2.1 Writer worker + queue — Spec A

- Channel: `tokio::sync::mpsc::channel::<WriteJob>(64)`, `try_send` (not `.send().await` — that would re-block the request thread and defeat async).
- On `TrySendError::Full`: JSON-RPC error `-32005 "writer queue saturated; retry"`. HTTP 200 JSON-body (axum-jrpc convention). Spec E confirms aggkit's ethtxmanager retries on `-32005`.
- Single worker task mirroring `L1InfoTreeIndexer::spawn`, with a `oneshot` shutdown signal. Worker is a **translator only** — no retry logic of its own; existing self-heal at `src/claim.rs:591-628` and `src/ger.rs:201-224` already handles recoverable errors. A pool adds nothing because `MidenClient::with(...)` is a channel-of-1 (`src/miden_client.rs:126`).
- `WriteJob` carries decoded params + `eth_tx_hash` (idempotency key). Decode on the request thread so malformed payloads cannot poison the queue.
- Per-signer lock (`src/service_state.rs:21-44`): serialises nonce classification and admission, then releases it after durable admission/enqueue. The bounded single writer is the only production dispatch path.

### 2.2 ClaimGuard placement — Spec B, recommendation (b) with amendments

- **Where the lock is taken:** worker dequeues → `try_claim_fenced(global_index, tx_hash, lease)` → `ClaimGuard` → `publish_claim`. The request thread owns only the nonce reservation.
- **Drop semantics:** before submission, cancellation conditionally releases only the caller's current fence. After the exact note handoff is sealed as `submitted`, stale release and lease reclaim are fail-closed.
- **Retry across self-heal:** retry is allowed only before the durable note handoff. Once that boundary exists, an ambiguous result does not build a second random note.
- **Recovery from `Queued × restart`:** the pending transaction row is the durable intent. A same-hash rebroadcast after the reservation lease expires re-decodes the stored signed envelope, resumes the job, and accepts either the pre-CAS nonce or its already-advanced value. A different hash can never take over the nonce slot.
- **`MidenSubmitted × worker-panic`** is fail-closed: the submitted fence and tx↔note link remain durable, and the projector/watcher can attribute later note consumption to the original hash.

### 2.3 BlockMonitor unification — Spec C (target design; deferred)

The current implementation ships only `current_tip()` / `record_tip()`. The deferred structural target makes `BlockMonitor` the sole caller of `store.set_latest_block_number` and has it own:
- the synthetic-block header cache (absorbs `BlockState::blocks` / `BlockState::hash_to_number`, `src/block_state.rs:137-148`);
- an `AtomicU64` tip mirror for `eth_blockNumber` (hot-read, no async hop);
- the single write entrypoint `async fn record(&self, BlockEvent) -> Result<u64>` wrapping the existing `store.commit_*_atomic` helpers (`src/store/mod.rs:208-231, 339-368, 438-471`) so the "log first / cursor second" ordering is a property of `record()` rather than a tribal-knowledge comment at four call sites (`src/bridge_out.rs:555-561`, `src/ger.rs:226-231`, `src/claim.rs:557-561`).

Implements `SyncListener`; `StoreSyncListener` (`src/store/mod.rs:520-569`) and `BlockState::on_sync` (`src/block_state.rs:251-255`) both delete. `claim_watcher` stays an independent `SyncListener` (owns its own consumed-note scan).

**Race elimination:**
- Cross-emitter `+1` collisions + TOCTOU close because `record` is the only writer.
- RD-862 GER-decomposition race stays cured (already covered by `L1InfoTreeIndexer` UPSERT + `commit_ger_event_atomic`); BlockMonitor preserves both.
- **New race introduced:** `AtomicU64` tip vs store tip. Resolved by always bumping `tip.fetch_max` _after_ `store.commit_*_atomic` returns Ok. Stale-low safe (readers re-query); stale-high forbidden; ordering rules it out.

**RD-913 coordination:** persisted Cantina trackers (`burn_serial_observe`, `twin_note_observe`, `expected_mint_record`) stay independent of BlockMonitor — observation-only, no blocks, no logs. One coordination point: `ExpectedMintTracker.record_expected` should land in the same store transaction as the `ClaimCommitted` `BlockEvent` — extend `commit_manual_claim_event_atomic` or expose `commit_claim_with_expected_mint_atomic`. API decision deferred to RD-913 owner.

**StaleAlert stays in `bridge_out.rs`** (`:541-575`). Hoisting it would force the consumed-notes scan to run twice or push a `landed_mint_ids` set across listeners with no other coupling. Not log emission anyway — metrics + tracing only.

### 2.4 RPC wire contract — Spec D

- `eth_sendRawTransaction` wire return **unchanged**: hex `0x<32-byte-hash>` string.
- `eth_getTransactionByHash` for in-flight: geth's pending shape — `blockHash`, `blockNumber`, `transactionIndex` are JSON `null`; every other numeric field MUST be a hex string. Go's pointer types on those three handle null cleanly; value-type unmarshallers (`hexutil.Uint`, `hexutil.Uint64`, `hexutil.Big`) on every other field panic on null.
- `eth_getTransactionReceipt` pre-commit: whole-body `null` (never a partial stub). Go's `ethclient.TransactionReceipt` reads JSON `null` as `(nil, ethereum.NotFound)` — exactly aggkit's "not yet mined, keep polling" path. A stub with `status:0x0` reads as "tx failed" and ethtxmanager stops retrying.
- `eth_getBlockByNumber("pending")`: alias to `latest`. No synthetic pending block — would break bridge reorg-detection re-hashing.
- `eth_pendingTransactions` / `txpool_content`: stay unimplemented.
- **Latent bug co-fix:** `src/service_get_txn_receipt.rs:40` returns `from: Default::default()` (0x0…0) instead of `TxnData.signer`.
- **JSON snapshot test:** pin in-flight tx JSON byte-string with `insta` to catch alloy upgrade regressions. A single missing/null field is undetectable from Rust-only tests but breaks aggkit silently.

## 3. Consumer interop — Spec E

aggkit's **only** on-proxy signer is `aggoracle`. `aggsender` uses gRPC `SendCertificate` to agglayer directly (`fixtures/aggkit-config.toml:69-86`) — does not touch the proxy. `claim-sponsor` lives in zkevm-bridge-service and uses its own custom monitor (`claimtxman/monitortxs.go`), not zkevm-ethtx-manager.

- aggoracle uses `github.com/0xPolygon/zkevm-ethtx-manager v0.2.18` with `WaitPeriodMonitorTx=5s`, `FrequencyToMonitorTxs=1s`, `WaitTxToBeMined=2m`, `EstimateGasMaxRetries=0` (unbounded). Re-broadcasts stuck txs by bumping gas + re-signing. Proxy MUST accept duplicate `eth_sendRawTransaction` calls with the same tx-hash idempotently (Decision 3).
- claim-sponsor's `RetryNumber=10` (`bridge-config.toml:66`) — finite retry budget. Calls `NonceAt(ctx, from, nil)` = `latest`; `nonce_cache.go:35` LRU breaks if `latest` leaks pool-queued txs. Decision 4 fixes this.
- **Stay strictly geth-compatible.** Do NOT add `gateway_getTxStatus` or any non-`eth_` namespace. aggkit's state machine maps cleanly to geth semantics; a new RPC would require forking aggkit or a translation shim.
- **Receipt with `status:0x0` on definite pre-handoff failure** is non-negotiable. This includes queue-age expiry before dispatch and completed Miden rejections for which the store disproves an exact-note handoff. Submitting or handed-off work is never terminalised concurrently; an ambiguous outcome remains pending so a still-running Miden closure cannot later contradict a failure receipt.
- **IAIC-class regression guard:** every async dispatch funnels through `MidenClient::with(...)` (`src/miden_client.rs:126`). The worker may dequeue concurrently in a future version, but Miden submission is always serial.

## 4. Failure modes + observability — Spec F

`Queued × {sigkill, host-restart, worker-oom}` is recoverable from the durable unlinked envelope on same-hash rebroadcast. Nonce reservations are renewed while live and are permanently bound to the first hash. Self-heal floor for `Submitting/MidenSubmitted`: `claim_watcher_synthesised_total` (`src/metrics.rs:106`).

**New Prometheus metrics:**

| Metric | Type | Alert |
|---|---|---|
| `agglayer_writer_queue_depth` | gauge | `>0.8×cap` 10m → warn; `>0.95×cap` 2m → page |
| `agglayer_writer_inflight_jobs` | gauge | informational |
| `agglayer_writer_job_duration_seconds{kind,outcome}` | histogram | p99 `>60s` 10m → page |
| `agglayer_writer_job_failures_total{kind,reason}` | counter | burst `>0.5/s` 5m → page |
| `agglayer_writer_dropped_on_restart_total` | counter | **hard page on `increase[1h]>0`** — restart-pressure tripwire; durable envelope remains recoverable |
| `agglayer_writer_queue_full_rejections_total{kind}` | counter | `rate[5m]>0.1` 5m → page |
| `agglayer_writer_drain_outcome_total{outcome}` | counter | dashboard only |

`dropped_on_restart` impl: tmpfile snapshot of `queue.len()` at graceful shutdown; read+reset on next boot. SIGKILL leaves it 0 — that absence combined with pre-kill queue-depth history is the signal.

**Graceful shutdown:** new `writer_shutdown_tx: tokio::sync::watch` plumbed into `ServiceState`. SIGTERM flips it; new sendRawTx returns `-32005 "service shutting down"`; worker drains for up to 20 s; `state.miden_client.shutdown()` runs only after worker exit. Bump k8s `terminationGracePeriodSeconds` 30 → 45 s.

**Caller-facing contract:** "If `eth_sendRawTransaction` returned a hash and the process restarted before dispatch, re-submit the same signed transaction. The durable envelope resumes after its lease expires; a replacement hash at that nonce is rejected."

**Logging:** one tracing span per job, fields `tx_hash`, `job_id` (ULID), `kind`, `signer`, `queue_wait_ms`, `miden_submit_ms`, `commit_ms`. INFO emits one line per terminal (~10 GB/day at 500 req/s; well within bali envelope).

## 5. Test plan + acceptance gate — Spec G

**Unit tests** (`cargo test --lib`): writer enqueue/failure/TTL behavior; pending RPC wire shape; nonce CAS and reservation lifecycle; durable-intent recovery after a lost enqueue; landed/in-flight claim classification; fenced stale-owner release; and accept-and-revert receipt/nonce invariants.

**Existing e2e** (`make test-e2e-coverage`, ~12 min):

| Target | Behaviour |
|---|---|
| `e2e-l1-to-l2`, `e2e-l2-to-l1` | unchanged (aggsender path bypasses writer) |
| `e2e-claim-watcher-synthesis` | most load-bearing — log-emission ordering canary |
| `e2e-rd862-repro` | orphan-rate canary |
| `e2e-iaic-mempool-conflict.sh MODE=expect_no_iaic PARALLEL=10` | **v1 regression sentinel** |
| `e2e-fuzz-bridge` | extend with `FUZZ_ROUND_ASYNC_BURST` |

**New e2e** (`scripts/e2e-rd940-*.sh`, ~14 min): `async-submit` (golden path), `pending-receipt` (hexutil-safe JSON shape), `queue-backpressure` (600 req/s vs cap 64), `restart-inflight` (kill before dequeue, wait for lease expiry, assert same-hash durable recovery with no second nonce advance), `worker-panic` (assert ClaimGuard release + watcher backfill), `claim-guard-cancellation` (32 concurrent disconnects).

**Acceptance gate (v1):** 50 req/s × 10 min, drop-rate <1%, accept-latency p50 <20 ms / p99 <100 ms, worker-job-duration p50 <10 s / p99 <60 s, `dropped_on_restart = 0`, zero nonce gaps, LET-divergence = 0. **Total v1-gate CI cycle ≈ 40 min wall-clock.**

## 6. Cross-spec design decisions (resolved conflicts)

| # | Conflict | Resolution | Rationale |
|---|---|---|---|
| 1 | ClaimGuard placement (A: enqueue / B: worker) | Worker | Eliminates `Queued/Submitting × restart` wedge defects. v1 in-memory queue means a crash before dequeue must leave _no_ on-disk lock. |
| 2 | Durable intent placement | Handler before nonce CAS | Prevents nonce advancement without recoverable work; downstream handoffs use idempotent begin/update. |
| 3 | R4 nonce equality (A) vs idempotent re-broadcast (D/E) | Classify known hash before R4 rejection | In-flight/handed-off/terminal hashes deduplicate; an unlinked durable intent resumes with its already-advanced nonce. Novel stale hashes still fail R4. |
| 4 | `eth_getTransactionCount` semantics (D punted / E forced) | `latest` = next-committed, `pending` = next-accepted; honour tag (today ignored at `src/service.rs:370-377`) | claim-sponsor's `nonce_cache.go:35` reads `latest` and breaks if pool-queued txs leak in. |
| 5 | Queue cap 64 vs 500 req/s gate | v1 gate at 50 req/s; 500 → nightly stress | At cap 64 and p50 ≈ 10 s, sustainable throughput ~6 jobs/s. 500 req/s is 100× current bali load. |

## 7. Open questions for Igor

1. **Queue depth 64** — agreeable, or size against an observed aggsender burst (~100+ changes drain math)? — **Answered (build): 64, env-overridable.**
2. **Nonce-burn on failure** — stay advanced (recommended) vs rewind? — **Default: stay advanced.**
3. **TTL default 5 min** — env-configurable? — **Default: yes, env-overridable; applies to queue-age admission and terminal-cache eviction, never concurrent submitting-work failure.**
4. **`expected_mint_record` store API** for RD-913 coordination — extend `commit_manual_claim_event_atomic` or new variant? (RD-913 owner's call.) — **Deferred; hook point only.**
5. **`dropped_on_restart` persistence** — tmpfile in `/tmp`, k8s `emptyDir`, or SIGKILL → 0 + queue-depth-history-before-kill as the signal? — **Default: tmpfile in `/tmp/agglayer-writer-queue-snapshot`.**
6. **`-32005` vs `-32603`** on backpressure? — **Answered (build): -32005.**
7. **Co-fix `service_get_txn_receipt.rs:40` `from` bug** in same patch? — **Answered (build): yes, folded into BlockMonitor PR.**
8. **`insta` JSON snapshot** for in-flight tx wire shape? — **Default: yes.**
9. **k8s `terminationGracePeriodSeconds`** bump 30 → 45 s — any HPA / PDB interaction on bali? — **Out of scope for this repo; downstream gateway-deploy coordination.**
10. **`scripts/setup-iaic-fixture.sh`** as a CI-friendly fixture builder so the regression sentinel runs strict? — **Default: yes.**

## 8. Risks + scope notes

- **+50% spike risk (RPC contract / receipt-availability race)** fully covered by Decisions 3, 4, 5 + Spec D's wire-shape audit. Residual: alloy upgrade regression — mitigated by JSON snapshot test.
- **No standalone durable queue table in v1.** The signed transaction row is the durable admission intent; same-hash rebroadcast reconstructs dispatch after a restart. A future queue/outbox table would add autonomous recovery without requiring that rebroadcast.
- **`MidenSubmitted × worker-panic`** retains the submitted claim fence and exact note link; the projector/watcher remains the recovery floor.
- **Adjacent tickets:** RD-913 (persisted trackers) needs explicit coordination (§2.3). RD-891 (gas budget) orthogonal — merges independently. RD-862 (GER decomposition race) structurally already cured; BlockMonitor preserves the cure.
- **Not in scope for RD-940:** worker pool, a standalone queue table, `gateway_getTxStatus` RPC, `pending` block enumeration, real wallclock block timestamps. Explicitly deferred.

— end consolidated spec —
