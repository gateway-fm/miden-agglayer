# Monitoring miden-agglayer

This doc describes what is exposed by the service, what to scrape, what
to alert on, and how to translate a Prometheus / Loki snapshot into a
verdict on whether bridging is flowing in each direction.

## 1. Endpoints exposed by the service

The service binds a single HTTP listener on `--port` (default `8546`) and
mounts:

| Path | Purpose |
|---|---|
| `POST /` | JSON-RPC. Bridge tooling (aggoracle, aggsender, bridge-service, claim sponsor) talks here. |
| `GET /health` | Returns `{"status":"ok"}` once the service has finished startup. Use as a Kubernetes readiness + liveness probe. |
| `GET /metrics` | Prometheus exposition. All metrics described below come from here. |

See `src/metrics.rs` for the authoritative list of metric names and the
inline documentation on what each one means.

## 2. Prometheus metrics catalogue

Counters and one histogram, grouped by purpose. All metric names are
unprefixed in the exposition (no `miden_agglayer_` prefix in source —
add one via `relabel_configs` if your global registry needs it).

### Throughput — "is the service doing useful work?"

| Metric | What it counts |
|---|---|
| `rpc_requests_total{method=...}` | JSON-RPC requests by method. Strong proxy for upstream tooling liveness. |
| `claims_processed_total` | L1→L2 claim notes successfully submitted to Miden. |
| `ger_injections_total` | UpdateGerNote notes successfully committed on Miden. |
| `bridge_outs_total` | L2→L1 B2AGG notes detected + emitted as `BridgeEvent` synthetic logs. |
| `rpc_request_duration_seconds` | Histogram. Per-method latency; use `_bucket` for tail-latency SLOs. |

If `rpc_requests_total{method="eth_sendRawTransaction"}` is climbing but
`claims_processed_total` + `ger_injections_total` are flat, every tx is
failing pre-submit — go straight to the runbook section on "no
submissions landing".

### Self-heal / divergence — Miden-client store health

| Metric | Meaning | Severity |
|---|---|---|
| `miden_client_build_errors_total` | Failed attempts to build the Miden gRPC connection. Sustained non-zero rate = miden-node down or DNS broken. | warning |
| `miden_client_restarts_total` | Background event-loop thread restarted after a crash. Each tick = a closure panicked. | warning |
| `miden_sync_errors_total{kind=...}` | Sync errors by kind. Spikes correlate with miden-node reorgs / restarts. | warning |
| `miden_locked_accounts_detected_total` | Set at startup if any managed account is `locked` in the miden-client sqlite. Indicates stale-lock divergence — see runbook `"--unlock-miden-accounts"`. | page if non-zero on startup |

### Bridge invariants — Cantina audit findings, hard-page metrics

These exist to satisfy the Cantina audit's "you must page before this
ever silently wedges the bridge" requirement. Treat each one as a
hard-page criterion at any non-zero rate.

| Metric | What it means | Cantina ref |
|---|---|---|
| `bridge_burn_serial_collision_total` | A BURN note's serial number was reused for a different leaf. `mint_and_send` token_supply is at risk of exhaustion. | Cantina #5 |
| `bridge_twin_note_detected_total` | Second on-chain note with a previously-observed NoteId but different metadata — B2AGG reclaim attack signature. | Cantina #6 |
| `bridge_mint_target_mismatch_total` | MINT note consumed by a faucet other than its `NetworkAccountTarget`. Claimant about to receive the wrong wrapped asset. | Cantina #2 |
| `bridge_faucet_ownership_drift_total{kind=drift\|renounced}` | Faucet owner storage slot moved away from the configured bridge AccountId. `renounced` wedges the faucet permanently. | Cantina #4 |
| `bridge_forged_mint_total` | MINT note on chain that does not correspond to any aggkit-recorded claim. Forged via NoAuth bridge note authorship. | Cantina #4 |
| `bridge_out_self_targeted_total` | B2AGG whose `destination_network` equals our `network_id`. Each one is a poison leaf that wedges the bridge. | Cantina #13 |

### Drift detection — Local Exit Tree consistency

| Metric | Meaning |
|---|---|
| `bridge_let_divergence_total{kind=on_chain_ahead}` | A private B2AGG was consumed on chain that we never observed — local LET is behind. |
| `bridge_let_divergence_total{kind=aggkit_ahead}` | Aggkit's LET is ahead of what we can reconstruct from on-chain data — likely local state corruption. |
| `bridge_expected_mint_stale_total` | A MINT NoteId we predicted hasn't landed within the retry threshold (`Cantina #7` — batch-dedup censorship via metadata-distinct twin). |

### Backpressure / dedup signals — operational, not security

| Metric | Use |
|---|---|
| `bridge_out_invalid_destination_total` | B2AGG with zero-address / EVM-precompile destination. We refuse to forward to bridge-service. Steady non-zero rate = upstream client bug. |
| `bridge_out_unknown_faucet_total` | B2AGG referencing a faucet not in our registry. Quarantined to prevent silent re-loop. Investigate any non-zero rate. |
| `address_mapper_zero_padding_fallback_total` | We had no explicit eth→Miden mapping and fell back to zero-padding the trailing 16 bytes. Account existence is NOT verified — alert on unusual rates. |
| `rpc_claim_ger_not_seen_total` | Claim submission was rejected because the GER it referenced was not yet in our store. Cheap, retry-friendly. A sustained spike means claim sponsor and aggoracle are out of sync. |
| `rpc_claim_ger_wait_short_circuit_total` | A claim's GER-propagation wait completed instantly because the GER was already injected. Saves ~12s per claim — useful as a positive-side health signal. |
| `claim_watcher_synthesised_total` | ClaimWatcher synthesised a ClaimEvent for a consumed CLAIM that wasn't recorded by the normal path. Normal during crash recovery; spikes during steady state = `eth_sendRawTransaction` path is broken. |
| `claim_watcher_already_recorded_total` | ClaimWatcher saw a CLAIM that was already in the store. Use to monitor dedup rate. |
| `claim_watcher_storage_decode_total` | ClaimWatcher couldn't decode on-chain CLAIM note storage. Should be zero; any non-zero rate is a parser / schema-drift bug. |
| `claim_watcher_unrecoverable_total` | Consumed CLAIM that can't be synthesised at all. Page if rate spikes. |
| `store_envelope_decode_errors_total` | Tx envelope row in PgStore that won't decode. Either schema drift or DB corruption — investigate immediately. |
| `store_errors_total` | Generic catch-all for any store operation that returned an error. |

## 3. Suggested Prometheus alert rules

These are starter rules. Tune thresholds per cluster after a week of
baseline observation.

```yaml
# <TODO: confirm Prometheus rule file path on bali — likely
# kube-prometheus-stack PrometheusRule CRD in observability namespace>
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
      runbook_url: "https://github.com/gateway-fm/miden-agglayer/blob/main/docs/operations/runbook.md#bridge-invariant-violation"

  # ---- Account divergence (the postmortem-class failure) --------------
  - alert: MidenAgglayerAccountDivergence
    expr: |
      rate({namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
        |~ "account data wasn't found|incorrect account initial commitment" [5m]) > 0
    for: 5m
    labels: { severity: critical }
    annotations:
      summary: "Miden-store account divergence — bridge will not advance until cured"
      runbook_url: "https://github.com/gateway-fm/miden-agglayer/blob/main/docs/operations/runbook.md#accountdatanotfound--iaic"

  # ---- Bridge stalled (no new GER for >1h) ----------------------------
  - alert: MidenAgglayerGerInjectionStalled
    expr: |
      rate(ger_injections_total[15m]) == 0
        and on (pod) (rate(rpc_requests_total{method="eth_sendRawTransaction"}[15m]) > 0)
    for: 1h
    labels: { severity: high }
    annotations:
      summary: "Aggoracle is still pushing but no GER has injected for 1h"

  # ---- Claim throughput stalled while deposits pile up ----------------
  - alert: MidenAgglayerClaimThroughputDry
    expr: |
      rate(claims_processed_total[15m]) == 0
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
      runbook_url: "https://github.com/gateway-fm/miden-agglayer/blob/main/docs/operations/runbook.md#stale-account-lock"
```

`<TODO: confirm Max — should the cluster label come from `cluster` (kube-prometheus-stack default) or a custom relabel?>`

## 4. Loki queries — fast triage

The diagnostic skill (`miden-bali-debug`) is the canonical source for
these. Reproduced here for operators who don't have the skill checked
out.

```logql
# Bridge invariant violations (any one of the Cantina hard-page metrics
# logs a structured ERROR with the violation kind alongside the counter
# increment).
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "bridge_invariant_violation|burn_serial_collision|twin_note_detected|mint_target_mismatch|faucet_ownership_drift|forged_mint|self_targeted_b2agg"

# Account divergence (the postmortem class)
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "account data wasn't found|incorrect account initial commitment|AccountIsPrivate|AccountNotFoundOnChain"

# Self-heal events — should fire once per pod restart for normal recoveries
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "reimporting ger_manager|reimported from node|reimport_known_accounts"

# Indexer position — confirm cursor advancing
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "L1InfoTreeIndexer polled|cursor advanced|batch processed"

# GER lifecycle for a specific GER hash (substitute the hex)
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "<ger-hex-without-0x-prefix>"
  |~ "insertGlobalExitRoot|updateExitRoot|UpdateGerNote|exit roots don|is_injected"

# Claim flow for a specific globalIndex (decimal, as logged)
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "globalIndex=<dec>|global_index=<dec>"
```

## 5. Grafana dashboards

`<TODO: Max to paste current Grafana URLs.>` Until then, the key panels
to assemble are:

- Throughput row: `rate(rpc_requests_total[5m])` by method,
  `rate(claims_processed_total[5m])`,
  `rate(ger_injections_total[5m])`,
  `rate(bridge_outs_total[5m])`.
- Latency row: `histogram_quantile(0.99, ...rpc_request_duration_seconds_bucket)`
  by method.
- Bridge-safety row: stacked rate of all Cantina hard-page metrics —
  ideal display is "must be flat at zero".
- Divergence row: `miden_client_restarts_total`,
  `miden_sync_errors_total`, `miden_locked_accounts_detected_total`,
  `store_envelope_decode_errors_total`.
- Backlog row (overlay with bridge-service Postgres exporter): count of
  `sync.deposit WHERE network_id=0 AND ready_for_claim=false` vs count of
  `ger_entries WHERE is_injected` — divergence here is the most direct
  user-impact signal.

## 6. Healthy vs stuck — at-a-glance reference

| Signal | Healthy | Stuck |
|---|---|---|
| `eth_sendRawTransaction` rate | > 0, broadly steady | > 0 but `claims_processed_total` / `ger_injections_total` flat → every tx failing pre-submit |
| L1InfoTreeIndexer cursor | Within ~1 block of L1 head | Falling behind by >100 blocks → L1 RPC slow / hung |
| `bridge-service.sync.deposit WHERE network_id=0 AND ready_for_claim=true` count | Climbing in step with new L1 deposits | Plateaued — either aggoracle isn't pushing or proxy isn't emitting GERs |
| `ger_entries WHERE is_injected AND mainnet_exit_root IS NULL` ("STATE-C orphans") | Stable / shrinking (operator backfill running) | Climbing — RD-862 race firing, no `UseUpdateExitRoot` flag |
| Loki rate of `incorrect account initial commitment` | Zero | Any non-zero rate — see postmortem |
| Pod restart count over 24h | 0-1 (deploys only) | Multiple — check `Last State.Reason` for `OOMKilled` |
| `miden_locked_accounts_detected_total` on latest startup | 0 | > 0 — surgical unlock required |
