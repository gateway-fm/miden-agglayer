# Monitoring miden-agglayer

What the service exposes, what to scrape, what to alert on, and how to
translate a Prometheus / Loki snapshot into a verdict on whether
bridging is flowing in each direction. Component map + flow diagrams:
[`../ARCHITECTURE.md`](../ARCHITECTURE.md). Recovery actions:
[`runbook.md`](./runbook.md).

## 1. Endpoints exposed by the service

The service binds a single HTTP listener on `--port` (default `8546`) and
mounts:

| Path | Purpose |
|---|---|
| `POST /` | JSON-RPC. Bridge tooling (aggoracle, aggsender, bridge-service, claim sponsor) talks here. |
| `GET /health` | Returns `{"status":"ok"}` once the service has finished startup. Use as a Kubernetes readiness + liveness probe. |
| `GET /metrics` | Prometheus exposition. All metrics described below come from here. |

`src/metrics.rs::init_metrics` is the authoritative registry of metric
names + inline docs; emission sites live next to the code they measure.

## 2. The health line — is the projector caught up?

The single most important log signal on a projector-era build:

```
synthetic projector tick: caught up to Miden tip  miden_tip=N projector_cursor=N synthetic_tip=N
```

Emitted once per sync tick that did work. Healthy =
**`miden_tip == projector_cursor == synthetic_tip`** and N advancing.
The projector follows the *Miden* chain (one synthetic block per Miden
block), so its progress is measured against the Miden tip, not L1.

End-to-end RPC health checks:

| Check | Healthy | Meaning when unhealthy |
|---|---|---|
| `eth_blockNumber` advancing in step with the Miden node tip | yes, every ~5 s sync cadence | Projector stalled — the tip is the write-before-advance gate, so a frozen `eth_blockNumber` means no events are being exposed at all. Check `miden_client_restarts_total` and sync errors. |
| `eth_syncing` | always `false` | The synthetic chain has no download phase; any other answer is a bug. aggkit health-polls this. |
| `GET /health` | 200 `{"status":"ok"}` | Startup not finished / listener dead. |

## 3. Throughput expectations

**~1 proven tx/min per proxy is the ceiling.** All Miden submissions
(CLAIM, GER inject, faucet ops) serialize through the single
`MidenClient` actor, and each carries a zk-proof (~30–60 s at the
prover). Consequences to design alerts around:

- L1→L2 claims serialize: a burst of N deposits drains at ~1/min. A
  growing `ready_for_claim` backlog on bridge-service with the proxy
  committing ~1 claim/min is **saturation, not failure**.
- Under sustained claim load the sync interval stretches (the actor's
  `select!` loop is prover-bound) — expect the R1 reconciler metrics
  (below) to tick up. That is the designed compensation.
- The throughput signal of record is
  `miden_proof_generations_total{kind,op,outcome}` split by `kind`
  (`claim|ger|faucet|init|bridge_out`) + the
  `miden_proof_duration_seconds` histogram.

> **Registered-but-dormant series:** `claims_processed_total`,
> `ger_injections_total`, `bridge_outs_total`, `store_errors_total` are
> described in `init_metrics` but have **no emission site in the current
> code** — they will read 0 forever. Do not alert on them; use
> `miden_proof_generations_total` and `rpc_requests_total` instead.

## 4. Prometheus metrics catalogue

### RPC / throughput

| Metric | What it counts |
|---|---|
| `rpc_requests_total{method=...}` | JSON-RPC requests by method. Strong proxy for upstream tooling liveness. |
| `rpc_request_duration_seconds` | Histogram. Per-method latency; use `_bucket` for tail-latency SLOs. |
| `miden_proof_generations_total{kind,op,outcome}` | Every prove/submit round-trip. `outcome=ok` by `kind` is the real "claims committed / GERs injected" rate. `outcome=timeout|connect_failure` = prover outage; `fallback_ok` = the local-prover safety net fired; `fallback_error` = page critical. |
| `miden_proof_duration_seconds{kind,op}` | Proof wall clock. p50 ~30–60 s is normal; sustained growth eats the 1 tx/min budget. |
| `rpc_nonce_mismatch_total` / `rpc_future_nonce_wait_total` | See "nonce churn" in §6. |
| `rpc_unauthorized_signer_total`, `rpc_admin_auth_rejects_total` | Rejected callers. A steady trickle on an internet-adjacent host is scanner noise; a spike after a deploy is a misconfigured allow-list. |

### Projector + note-recovery ladder (R1) — event completeness

These are the SyntheticProjector redesign's health surface. The ladder
itself is automatic (see `runbook.md` Part 2, R1) — the metrics tell you
whether it is healing (normal) or being defeated (page).

| Metric | Meaning | Severity |
|---|---|---|
| `synthetic_reconciler_notes_imported_total` | Reconciler back-filled network notes that interest-based sync missed. **Normal background healing** — expect activity under load and a burst after `--restore`. | none (dashboard) |
| `synthetic_reconciler_import_dropped_total` | miden-client silently dropped an import because the note was already spent; the ladder escalated to direct recovery. Upstream quirk being **auto-recovered** — WARN-level normal. | warn if sustained |
| `synthetic_reconciler_direct_recovered_total` | Spent-before-import notes recovered with on-chain consumer proof and directly projected. The ladder's last rung working. | none (dashboard) |
| `synthetic_reconciler_unverified_consumption_total` | A consumed B2AGG could NOT be attributed to any bridge-executed tx — sender reclaim or unknown consumer. Event **deliberately not emitted** (MA#3 fail-closed). | **INVESTIGATE** — possible reclaim or anomaly |
| `synthetic_reconciler_missing_not_consumed_total` | Note expected by the reconciler is neither imported nor consumed; retried after restart (genesis re-sweep). | **INVESTIGATE** if repeating |

### LET divergence — the "missing events" tripwire

| Metric | Meaning |
|---|---|
| `bridge_let_divergence_total{kind="on_chain_ahead"}` | The bridge's on-chain `let_num_leaves` is ahead of the proxy's `deposit_counter`. Increments **once per scanner tick while the gap persists**. Transient increments during load are the R1 ladder mid-heal; the rate **must converge to 0 when the system is idle**. A sustained non-zero rate on an idle system = BridgeEvents permanently missing from the synthetic chain → aggkit's exit tree is short → **page**. |
| `bridge_let_divergence_total{kind="aggkit_ahead"}` | Local deposit count ahead of on-chain LET — should never happen; local state corruption. **Page.** |
| `bridge_expected_mint_stale_total` | A predicted MINT NoteId did not land within the threshold (~6 min) — batch-dedup censorship signature (Cantina #7). Fires once per expectation. **Page.** |

### Miden-client health

| Metric | Meaning | Severity |
|---|---|---|
| `miden_client_build_errors_total` | Failed attempts to build the Miden gRPC connection. Sustained non-zero rate = miden-node down or DNS broken. | warning |
| `miden_client_restarts_total` | The actor loop crashed and was restarted (5 s backoff). Each tick = a closure panicked or the loop died — pairs with the `MidenClient::run crashed` log line. | warning; page if climbing steadily |
| `miden_sync_errors_total{kind=connection\|other}` | Sync errors by kind. Spikes correlate with miden-node restarts / network flaps. | warning |
| `miden_locked_accounts_detected_total` | Set at startup if any managed account is `locked` in the miden-client sqlite. See runbook `--unlock-miden-accounts`. | page if non-zero on startup |
| `miden_account_reimport_total{account,outcome=ok\|failed}` | R3 self-heal firings. `ok` once per incident = working; repeated firings for one account = chronic divergence → runbook A.2. | warn |
| `miden_listener_skipped_paused_total` | Sync listeners skipped because projection is paused (restore in progress). Non-zero outside a restore window is a bug. | info |

### Bridge invariants — Cantina audit findings, hard-page metrics

Treat each one as a hard-page criterion at any non-zero rate.

| Metric | What it means | Cantina ref |
|---|---|---|
| `bridge_burn_serial_collision_total` | A BURN note's serial number was reused for a different leaf. `mint_and_send` token_supply is at risk of exhaustion. | #5 |
| `bridge_twin_note_detected_total` | Second on-chain note with a previously-observed NoteId but different metadata — B2AGG reclaim attack signature. | #6 |
| `bridge_mint_target_mismatch_total` | MINT note consumed by a faucet other than its `NetworkAccountTarget`. Claimant about to receive the wrong wrapped asset. | #2 |
| `bridge_faucet_ownership_drift_total{kind=drift\|renounced}` | Faucet owner storage slot moved away from the configured bridge AccountId. `renounced` wedges the faucet permanently. | #4 |
| `bridge_forged_mint_total` | MINT note on chain that does not correspond to any aggkit-recorded claim. | #4 |
| `bridge_out_self_targeted_total` | B2AGG whose `destination_network` equals our `network_id`. Each one is a poison leaf. | #13 |

### Quarantine / unbridgeable — funds parked, operator handle exists

| Metric | Meaning |
|---|---|
| `bridge_out_unknown_faucet_total` | B2AGG referencing a faucet not in our registry. Quarantined to prevent re-loop; stuck until the registry is updated. Investigate any non-zero rate. |
| `bridge_out_quarantined_erased_b2agg_total` | Consumed B2AGG skipped because its contents were unparsable / faucet unknown (MA#18); a quarantine-table row was written as the recovery handle. |
| `bridge_unknown_wrapper_consumed_total` | Bridge consumed a note whose script root is neither B2AGG nor CLAIM (MA#4) — the LET advanced but no event can be synthesised. Investigate before more funds strand. |
| `bridge_out_invalid_destination_total` | B2AGG with zero-address / EVM-precompile destination. Refused. Steady non-zero rate = upstream client bug. |
| `claim_unclaimable_total{reason}` | Claim recorded as unclaimable (e.g. unresolvable destination). Each one is a user-visible stuck deposit. |
| `address_mapper_zero_padding_fallback_total` | No explicit eth→Miden mapping; fell back to zero-padding. Account existence NOT verified — alert on unusual rates. |
| `address_mapper_hardhat_alias_rejected_total` | Hardhat-alias remap refused (`DISABLE_HARDHAT_ALIAS`). Non-zero in production = someone deposits to the well-known dev address. |

### ClaimWatcher / store

| Metric | Use |
|---|---|
| `claim_watcher_synthesised_total` | Watcher synthesised a ClaimEvent for a consumed CLAIM the normal path didn't record. Normal during crash recovery; sustained in steady state = `eth_sendRawTransaction` path broken. |
| `claim_watcher_already_recorded_total` | Dedup-rate monitor. |
| `claim_event_foreign_skipped_total` | CLAIM-shaped consumed note NOT provably ours (consumer ≠ our bridge, not minted by our service targeting our bridge) — skipped fail-closed. Expected when the chain hosts a foreign miden-agglayer deployment; on a single-deployment chain any non-zero rate = unverifiable claim consumption, investigate. |
| `claim_watcher_storage_decode_total` / `claim_watcher_unrecoverable_total` | On-chain CLAIM storage that won't decode → quarantined. Should be zero; page on spikes. |
| `store_envelope_decode_errors_total` | Corrupt / schema-drifted `transactions` row. Investigate immediately. |
| `rpc_claim_ger_not_seen_total` | Claim rejected because its GER isn't injected yet. Cheap, retry-friendly; sustained spike = aggoracle behind. |
| `rpc_claim_ger_wait_short_circuit_total` | Positive-side signal: claim's GER wait exited instantly. |
| `restore_ger_*_total` | Restore-replay skip reasons (missing metadata / no target / sender or target mismatch). Only meaningful during a `--restore` run. |

## 5. Log lines that matter

| Log line | Meaning | Action |
|---|---|---|
| `database is locked` | Something is contending on the proxy's `store.sqlite3`. With the singleton `MidenClient` and a proxy-private store this **must be 0** — any occurrence means a second replica, an external tool on the store, or an internal regression. The isolated loadtest gates on exactly this count. | **Page** |
| `MidenClient::run crashed: ..., restarting in 5s...` | Actor loop died; auto-restarts (pairs with `miden_client_restarts_total`). One-off after a node hiccup is fine. | Investigate if repeating |
| `synthetic projector tick: caught up to Miden tip` | THE health line (§2). | None |
| `note reconciler: imported network notes missed by sync` | R1 catcher 2 healing. Normal. | None |
| `note reconciler: import silently dropped consumed notes; attempting direct projection recovery` | R1 escalating to catcher 3 — upstream miden-client drop being auto-recovered. WARN-level **normal**. | None |
| `spent-before-import recovery: bridge-consumed B2AGG verified via sync_transactions` | R1 catcher 3 success. | None |
| `spent-before-import recovery: consumed B2AGG was NOT consumed by any bridge transaction ...` | MA#3 fail-closed skip (reclaim/unknown consumer). | Investigate |
| `note reconciler failed (transient — will retry next tick)` | One reconciler pass failed; retried. Sustained repetition = node RPC trouble. | Investigate if sustained |
| `nonce mismatch for 0x...: tx.nonce = N, expected M` | Out-of-order / replayed submission rejected (R4 guard). A background *churn* of these from autoclaim is benign — aggkit retries; the churn is eliminated by the writer worker's future-nonce wait (`AGGLAYER_ENABLE_WRITER_WORKER=true` waits up to 30 s for the gap nonce; `rpc_future_nonce_wait_total` counts the waits). | None unless a signer is stuck |
| `JSON-RPC unsupported method: web3_clientVersion` (also `parity_netPeers`, `debug_*`, `net_peerCount`, …) | Internet wallet-scanner probe noise — observed continuously on a host whose 8546 was bound to `0.0.0.0`. | Verify the port is loopback/private (runbook §1.2); otherwise ignore |
| `account data wasn't found` / `incorrect account initial commitment` followed by `reimported from node` | R3 self-heal firing and (usually) curing. | Investigate only if it loops |
| `reset_miden_store: deleted` / `=== RESTORE: complete ===` | R2 one-shot markers — should only appear during an operator-initiated recovery. | Confirm an operator is driving |

## 6. Suggested Prometheus alert rules

Starter rules; tune thresholds per cluster after a week of baseline.

```yaml
groups:
- name: miden-agglayer.bridge-safety
  rules:
  # ---- Hard-page Cantina findings (any non-zero rate) -----------------
  - alert: MidenAgglayerBridgeInvariantViolation
    expr: |
      sum by (cluster, pod) (
        increase(bridge_burn_serial_collision_total[5m])
        + increase(bridge_twin_note_detected_total[5m])
        + increase(bridge_mint_target_mismatch_total[5m])
        + increase(bridge_faucet_ownership_drift_total[5m])
        + increase(bridge_forged_mint_total[5m])
        + increase(bridge_out_self_targeted_total[5m])
      ) > 0
    for: 0m
    labels: { severity: critical }
    annotations:
      summary: "Bridge invariant violation on {{ $labels.pod }}"
      runbook_url: ".../docs/operations/runbook.md#failure-mode-d--bridge-invariant-violation-cantina-hard-page-metrics"

  # ---- Missing events: LET divergence not converging -------------------
  # on_chain_ahead increments once per scanner tick while the gap is open.
  # Transient bursts under load are the R1 ladder mid-heal; 15 minutes of
  # continuous increments means events are NOT being recovered.
  - alert: MidenAgglayerLetDivergenceSustained
    expr: rate(bridge_let_divergence_total{kind="on_chain_ahead"}[5m]) > 0
    for: 15m
    labels: { severity: critical }
    annotations:
      summary: "LET gap not converging — BridgeEvents missing from the synthetic chain"
      runbook_url: ".../docs/operations/runbook.md#r1--live-recovery-ladder-automatic--needs-no-operator-action"

  - alert: MidenAgglayerLetAggkitAhead
    expr: increase(bridge_let_divergence_total{kind="aggkit_ahead"}[10m]) > 0
    labels: { severity: critical }
    annotations:
      summary: "Local deposit count ahead of on-chain LET — state corruption"

  # ---- Reclaim / anomaly on the recovery path --------------------------
  - alert: MidenAgglayerUnverifiedConsumption
    expr: |
      increase(synthetic_reconciler_unverified_consumption_total[15m]) > 0
        or increase(synthetic_reconciler_missing_not_consumed_total[15m]) > 0
    labels: { severity: high }
    annotations:
      summary: "Reconciler saw a consumption it refuses to attribute to the bridge — possible reclaim"

  # ---- database is locked (from logs; Loki ruler or mtail) --------------
  - alert: MidenAgglayerSqliteLock
    expr: |
      sum(count_over_time({container="miden-agglayer"} |= "database is locked" [10m])) > 0
    labels: { severity: critical }
    annotations:
      summary: "sqlite contention on the proxy store — second accessor or replica"
      runbook_url: ".../docs/operations/runbook.md#12-hard-deployment-constraints"

  # ---- Account divergence (the postmortem-class failure) --------------
  - alert: MidenAgglayerAccountDivergence
    expr: |
      rate(miden_account_reimport_total{outcome="failed"}[15m]) > 0
    for: 5m
    labels: { severity: critical }
    annotations:
      summary: "R3 self-heal failing — miden-store divergence not self-curing"
      runbook_url: ".../docs/operations/runbook.md#failure-mode-a--accountdatanotfound--iaic"

  # ---- Prover degradation ----------------------------------------------
  - alert: MidenAgglayerProverFailures
    expr: |
      sum(rate(miden_proof_generations_total{outcome=~"timeout|connect_failure|prover_error|fallback_error"}[10m])) > 0
    for: 10m
    labels: { severity: high }
    annotations:
      summary: "Miden proof generation failing — remote prover down or degraded"

  # ---- GER flow stalled -------------------------------------------------
  - alert: MidenAgglayerGerInjectionStalled
    expr: |
      rate(miden_proof_generations_total{kind="ger",outcome="ok"}[15m]) == 0
        and on (pod) (rate(rpc_requests_total{method="eth_sendRawTransaction"}[15m]) > 0)
    for: 1h
    labels: { severity: high }
    annotations:
      summary: "Aggoracle is still pushing but no GER has committed on Miden for 1h"

  # ---- Claim throughput stalled while deposits pile up ----------------
  - alert: MidenAgglayerClaimThroughputDry
    expr: |
      rate(miden_proof_generations_total{kind="claim",outcome="ok"}[15m]) == 0
        and on (pod) (rate(rpc_claim_ger_not_seen_total[15m]) > 0.1)
    for: 30m
    labels: { severity: high }
    annotations:
      summary: "Claims are being rejected for missing GER — aggoracle is behind"

  # ---- ClaimWatcher quarantine spike ----------------------------------
  - alert: MidenAgglayerClaimWatcherUnrecoverable
    expr: rate(claim_watcher_unrecoverable_total[10m]) > 0
    for: 10m
    labels: { severity: high }
    annotations:
      summary: "ClaimWatcher is quarantining consumed CLAIM notes — parser/schema bug"

  # ---- Locked accounts on startup -------------------------------------
  - alert: MidenAgglayerLockedAccountsOnStartup
    expr: miden_locked_accounts_detected_total > 0
    for: 1m
    labels: { severity: high }
    annotations:
      summary: "Managed account(s) locked in miden-client sqlite"
      runbook_url: ".../docs/operations/runbook.md#failure-mode-g--stale-account-lock"
```

## 7. Loki queries — fast triage

```logql
# The health line — projector progress
{container="miden-agglayer"} |= "synthetic projector tick"

# The R1 ladder at work (all normal-operation healing chatter)
{container="miden-agglayer"}
  |~ "note reconciler|late-consumption sweep|spent-before-import recovery"

# Hard stops
{container="miden-agglayer"}
  |~ "database is locked|MidenClient::run crashed|bridge_invariant_violation"

# Account divergence + self-heal outcomes
{container="miden-agglayer"}
  |~ "account data wasn't found|incorrect account initial commitment|reimported from node|account reimport failed"

# Indexer position — confirm cursor advancing
{container="miden-agglayer"} |~ "L1InfoTreeIndexer polled"

# GER lifecycle for a specific GER hash (substitute the hex)
{container="miden-agglayer"} |~ "<ger-hex-without-0x-prefix>"

# Scanner probe noise (should correlate with an exposed port — see runbook §1.2)
{container="miden-agglayer"} |= "JSON-RPC unsupported method"
```

## 8. Grafana dashboard panels

- **Throughput row:** `rate(rpc_requests_total[5m])` by method;
  `rate(miden_proof_generations_total{outcome="ok"}[5m])` by `kind`
  (claim ≈ L1→L2 delivery rate, ger ≈ GER injection rate).
- **Latency row:** `histogram_quantile(0.99, ...rpc_request_duration_seconds_bucket)`
  by method; `miden_proof_duration_seconds` heatmap by `kind`.
- **Event-completeness row:** `bridge_let_divergence_total` rate ("must
  return to zero"), the five `synthetic_reconciler_*` counters stacked.
- **Bridge-safety row:** stacked rate of all Cantina hard-page metrics —
  ideal display is "flat at zero".
- **Divergence row:** `miden_client_restarts_total`,
  `miden_sync_errors_total`, `miden_account_reimport_total`,
  `store_envelope_decode_errors_total`.
- **Backlog row** (overlay with bridge-service Postgres exporter): count
  of `sync.deposit WHERE network_id=0 AND ready_for_claim=false` vs the
  claim commit rate — with the ~1 claim/min ceiling in mind (§3).

## 9. Healthy vs stuck — at-a-glance reference

| Signal | Healthy | Stuck |
|---|---|---|
| `synthetic projector tick` log | `miden_tip == projector_cursor == synthetic_tip`, advancing | cursor frozen or lagging tip → projector wedged; `eth_getLogs` consumers starving |
| `eth_blockNumber` | Advancing with the Miden tip (~5 s cadence) | Flat while Miden advances → projector / MidenClient down |
| `eth_syncing` | `false` | anything else is a bug |
| `bridge_let_divergence_total{kind="on_chain_ahead"}` rate | 0 when idle; brief bursts under load | Sustained increments while idle → missing events, page |
| `synthetic_reconciler_*` | imported/dropped/direct ticking under load; unverified/missing at 0 | unverified or missing_not_consumed climbing |
| `eth_sendRawTransaction` rate | > 0, steady | > 0 but `miden_proof_generations_total{outcome="ok"}` flat → every tx failing pre-submit |
| `database is locked` count in logs | **0, always** | any → second accessor on the store |
| L1InfoTreeIndexer cursor | Within ~1 block of L1 head | Falling behind by >100 blocks → L1 RPC slow / hung |
| `ger_entries WHERE is_injected AND mainnet_exit_root IS NULL` | Stable / shrinking | Climbing — RD-862 race firing, no `UseUpdateExitRoot` flag |
| Pod restart count over 24 h | 0-1 (deploys only) | Multiple — check `Last State.Reason` for `OOMKilled` |
| `miden_locked_accounts_detected_total` on latest startup | 0 | > 0 — surgical unlock required |

---

# RD-940 writer worker

This section codifies the alert thresholds for the eight Prometheus
metrics introduced by the RD-940 async writer worker
(`docs/design/RD-940-async-writer.md` Spec F §4). All series are
registered unconditionally in `src/metrics.rs::init_metrics`; they are
silent when `AGGLAYER_ENABLE_WRITER_WORKER=false`.

## Metric reference

| Metric | Type | Source | Description |
|---|---|---|---|
| `agglayer_writer_queue_depth` | gauge | `try_enqueue` | Current fill level of the mpsc channel (cap minus available). |
| `agglayer_writer_inflight_jobs` | gauge | `try_enqueue`, worker, TTL sweeper | Size of the inflight DashMap — Queued + Submitting + pre-eviction terminal. |
| `agglayer_writer_job_duration_seconds{kind,outcome}` | histogram | worker `process` | Time from dequeue to terminal. `kind=claim\|ger_insert`, `outcome=committed\|failed`. |
| `agglayer_writer_queue_full_rejections_total{kind}` | counter | `try_enqueue` | Backpressure events (returns JSON-RPC `-32005`). |
| `agglayer_writer_job_failures_total{kind,reason}` | counter | worker fail path + TTL sweeper | Terminal Failed transitions. `reason=miden\|ttl\|panic\|store`. |
| `agglayer_writer_dropped_on_restart_total` | counter | main.rs at boot | Residual jobs read from `/tmp/agglayer-writer-queue-snapshot`. |
| `agglayer_writer_drain_outcome_total{outcome}` | counter | main.rs after `service::serve` | `outcome=clean\|partial`. |
| `rpc_future_nonce_wait_total` | counter | `service_send_raw_txn` | Out-of-order submissions that entered the bounded future-nonce wait instead of erroring (writer mode only). Dashboard-level: measures how much reordering the worker absorbs. |

## Alerts

| Alert | Query (PromQL) | Severity | Reason |
|---|---|---|---|
| **WriterQueueWarn** | `agglayer_writer_queue_depth > 0.8 * AGGLAYER_WRITER_QUEUE_DEPTH` for 10 m | warn | Sustained backpressure; queue is filling faster than the single worker can drain. Capacity-plan or bump `AGGLAYER_WRITER_QUEUE_DEPTH`. |
| **WriterQueueCritical** | `agglayer_writer_queue_depth > 0.95 * AGGLAYER_WRITER_QUEUE_DEPTH` for 2 m | page | One step from `-32005` rejections; aggkit ethtxmanager retry budgets will start tripping. |
| **WriterJobDurationP99** | `histogram_quantile(0.99, rate(agglayer_writer_job_duration_seconds_bucket[10m])) > 60` | page | p99 > 60 s breaks aggkit's `WaitTxToBeMined = 2 m` envelope (Spec E). Miden submission is degraded. |
| **WriterJobFailures** | `rate(agglayer_writer_job_failures_total[5m]) > 0.5` for 5 m | page | Burst of dispatch failures. Drill down by `kind` + `reason` to distinguish Miden errors from TTL expiries. |
| **WriterDroppedOnRestart** | `increase(agglayer_writer_dropped_on_restart_total[1h]) > 0` | **hard page** | Restart-pressure tripwire: dispatch was lost but the signed envelope remains durable for same-hash recovery. See `docs/operations/runbook.md` Failure mode I. |
| **WriterQueueFullRejections** | `rate(agglayer_writer_queue_full_rejections_total[5m]) > 0.1` for 5 m | page | aggkit retries `-32005` transparently up to its budget; sustained backpressure exhausts the budget and surfaces as a stuck tx. |
| **WriterDrainOutcomePartial** | none — dashboard only | n/a | Counts non-clean shutdowns over time; correlate with restart events when investigating `dropped_on_restart` increments. |
| **WriterInflightSize** | none — dashboard only | n/a | Informational; size of the DashMap. Should track `queue_depth + jobs in flight at Miden`. |

## Useful dashboard panels

- **Throughput.** `rate(agglayer_writer_job_duration_seconds_count[1m])`
  split by `outcome`. Stack committed + failed; the gap to your request
  rate is queue-full rejections.
- **Latency heatmap.**
  `agglayer_writer_job_duration_seconds_bucket` split by `kind`. Watch
  the right tail of `claim` — `publish_claim` includes a GER-propagation
  wait, so claim p50 will sit noticeably higher than ger_insert.
- **Per-signer fairness.** Currently no per-signer label on the metrics
  (cardinality concern), but the structured-logs path emits `signer`
  on every `writer_worker::job` span. Grep / Loki the span events to
  detect a runaway signer monopolising the worker.

## Self-heal correlation

The pre-existing `claim_watcher_synthesised_total` metric remains the
floor signal for `MidenSubmitted × worker-panic`: a CLAIM that was
submitted to Miden but whose `ClaimEvent` was never written by
`service_send_raw_txn` is back-filled by the watcher on its next sync.
A correlated jump in `claim_watcher_synthesised_total` and
`agglayer_writer_job_failures_total{reason=panic}` is the expected
shape under a worker-panic incident.
