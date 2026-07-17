# Monitoring

The service exposes JSON-RPC, health, and Prometheus metrics on the same
listener configured by `--bind`/`--port` (default `0.0.0.0:8546`):

| Route | Meaning |
|---|---|
| `POST /` | JSON-RPC |
| `GET /health` | HTTP 200 while the background Miden client is alive; HTTP 503 after node connection loss |
| `GET /metrics` | Prometheus exposition from the process-wide recorder |

All three routes share the per-IP rate limit. Scrape over the private service
network and do not publish port 8546 directly to the internet.

Metric descriptions in `src/metrics.rs` and emission sites are authoritative.
Prometheus may omit a counter until its first emission; handle absent series as
zero only for metrics whose semantics make that safe.

## Minimum service-level checks

Monitor all of these independently:

1. `/health` status and latency.
2. `eth_blockNumber` advancement relative to the authoritative Miden tip.
3. Projector/reconciler cursor progress and completeness signals.
4. Writer queue, latency, failures, and restart drain outcome.
5. Miden connection/prover health.
6. L1 InfoTree indexer cursor/errors when GER indexing is configured.
7. Bridge integrity and quarantine counters.
8. One synthetic L1-to-L2 and L2-to-L1 canary in an environment where canary
   chain mutations are approved.

`eth_syncing` currently returns `false`; it is compatibility output, not a
progress signal.

## Projector and reconciler

| Metric | Healthy interpretation | Action condition |
|---|---|---|
| `synthetic_reconciler_cursor` | Advances toward the Miden tip | Flat/lagging while Miden advances |
| `projector_visibility_barrier_held_blocks` | `0` in steady state | Positive and not falling: projection is held behind note visibility |
| `synthetic_projector_completeness_audit_lag` | Highest audited block advances | Flat while projector advances |
| `synthetic_projector_completeness_missing_total` | No increase | Any increase is a missing historical `BridgeEvent`; page |
| `synthetic_projector_b2agg_authoritative_fetch_total` | Tracks successful body-resolution attempts | Compare with bridge-out volume; retries before cursor advance can make it higher |
| `synthetic_projector_b2agg_headerless_skip_total` | Expected for headerless non-B2AGG bridge inputs | Investigate if paired with a held LET gate |
| `synthetic_projector_b2agg_fetch_missing_total` | No increase | Any increase means an identified bridge consumption body was unavailable; the projector fails the tick before sealing |
| `bridge_let_assignment_gate_halted_total` | No increase | Any increase means LET and local reservation cardinality disagree; projection is held before sealing |
| `synthetic_reconciler_notes_imported_total` | May increase during catch-up | Sustained burst indicates ordinary sync missed notes; inspect node/sync health |
| `synthetic_reconciler_private_skipped_total` | May increase for historical private tag-0 notes | Informational unless sweep cursor stops |

Also compare the durable `service_state.projector_cursor`,
`service_state.reconcile_cursor`, and `latest_block_number` in Postgres during
an incident. Never write those cursors manually.

## Writer and nonce admission

The writer queue capacity defaults to 64 and is configured by
`AGGLAYER_WRITER_QUEUE_DEPTH`. `AGGLAYER_WRITER_TX_TTL` defaults to 300 seconds
and applies to time waiting in the queue before dispatch; it also controls how
long terminal entries remain in the process-local status cache.

| Metric | Labels/meaning |
|---|---|
| `agglayer_writer_queue_depth` | Jobs currently waiting in the bounded channel |
| `agglayer_writer_inflight_jobs` | Queued, submitting, and terminal entries not yet evicted |
| `agglayer_writer_job_duration_seconds{kind,outcome}` | Dequeue-to-outcome latency; `kind=claim|ger_insert`; current outcomes include `committed`, `failed`, and `pending` for ambiguous durable handoffs |
| `agglayer_writer_queue_full_rejections_total{kind}` | JSON-RPC `-32005` backpressure responses |
| `agglayer_writer_job_failures_total{kind,reason}` | Terminal failures; reasons emitted by current paths include `ttl`, `miden`, and `panic` |
| `agglayer_writer_drain_outcome_total{outcome}` | Graceful shutdowns labelled `clean` or `partial` |
| `agglayer_writer_dropped_on_restart_total` | Residual in-memory jobs reported from the prior graceful-shutdown snapshot |
| `rpc_future_nonce_wait_total` | Future nonces that entered the bounded ordering wait |
| `rpc_nonce_mismatch_total` | Nonce requests rejected after the wait/check |
| `rpc_nonce_reservation_lost_total` | A different transaction won the durable `(signer, nonce)` slot |
| `rpc_nonce_repaired_after_commit_gap_total` | Same-hash replay repaired a receipt-to-nonce crash gap |

Recommended alerts from the code's metric contract:

- queue above 80% of the configured capacity for 10 minutes: warning;
- queue above 95% for 2 minutes: page;
- p99 writer duration above 60 seconds for 10 minutes: page;
- queue-full rejection rate above 0.1/second for 5 minutes: page;
- writer failure rate above 0.5/second for 5 minutes: page;
- any increase in `agglayer_writer_dropped_on_restart_total`: page and arrange
  rebroadcast of the original signed transactions.

Prometheus does not know the configured queue capacity. Encode it as deployment
metadata/a recording rule or substitute the correct numeric threshold; do not
compare the gauge to the string environment-variable name in PromQL.

The restart counter is written through
`/tmp/agglayer-writer-queue-snapshot` only during graceful shutdown and read on
the next boot. SIGKILL and an ephemeral `/tmp` replacement can erase that
signal. Correlate it with pre-restart queue/inflight history and durable pending
rows; absence of the counter is not proof that no work was interrupted.

An ambiguous exact note handoff intentionally remains pending. Do not alert on
the `outcome="pending"` label as a fabricated failure; alert when it fails to
resolve and follow the same-hash procedure in the runbook.

## Miden and proof generation

| Metric | Meaning |
|---|---|
| `miden_client_build_errors_total` | Failed client construction/reconnection attempts |
| `miden_client_restarts_total` | Background client loop restarts after a crash |
| `miden_sync_errors_total{kind="connection|other"}` | Sync failures |
| `miden_account_reimport_total{account,outcome}` | Automatic account self-heal attempts |
| `miden_locked_accounts_detected_total` | Locked managed accounts found by the startup diagnostic |
| `miden_proof_generations_total{kind,op,outcome}` | Proof/submit outcomes; kinds are bounded to claim, GER, faucet, init, and bridge-out paths |
| `miden_proof_duration_seconds{kind,op,outcome}` | Proof/submit duration histogram |
| `readonly_submissions_refused_total` | A mutation path attempted to submit while `--read-only` was active |

Remote-prover failures are labelled by outcomes such as `timeout`,
`connect_failure`, `prover_error`, `submit_failure`, `build_failed`, and
`fallback_error`; `fallback_ok` means the explicitly enabled local fallback
succeeded. In production hardening mode the remote prover is mandatory and is
probed at startup.

Any `readonly_submissions_refused_total` increase during a supposedly passive
reindex is evidence that a code path attempted a chain mutation. The guard
stopped it, but the attempt still requires investigation.

## L1 GER indexing

When both `L1_RPC_URL` and `GER_L1_ADDRESS` are configured, monitor:

- `l1_info_tree_indexer_pairs_indexed_total`;
- `l1_info_tree_indexer_poll_errors_total`;
- `l1_info_tree_indexer_log_errors_total`;
- `l1_info_tree_indexer_cursor_persist_errors_total`;
- `l1_indexer_state.last_processed` relative to the L1 head;
- injected `ger_entries` with a null mainnet or rollup exit root.

A fresh deployment with no cursor starts at the current L1 head. A persisted
cursor resumes with a 64-block reorg margin. `--l1-indexer-from-block` overrides
both for a deliberate backfill and should be removed after that boot.

`rpc_claim_ger_not_seen_total` counts claims rejected before nonce/queue
admission because their GER is not yet injected.
`rpc_estimate_gas_ger_not_ready_total` is the corresponding fail-fast
`GlobalExitRootInvalid()` simulation response. A sustained rate points to
aggoracle or GER-indexing lag, not writer saturation.

## Bridge security monitors

| Metric | What it means | Cantina ref |
|---|---|---|
| `bridge_burn_serial_collision_total` | A BURN note's serial number was reused for a different leaf. `mint_and_send` token_supply is at risk of exhaustion. | #5 |
| `bridge_twin_note_detected_total` | Second on-chain note with a previously-observed NoteId but different metadata — B2AGG reclaim attack signature. | #6 |
| `bridge_mint_target_mismatch_total` | MINT note **in our deployment's flow** whose intended faucet is unregistered or differs from the faucet that consumed it. This covers both misregistration and the Cantina #2 cross-faucet exploit. Provenance-scoped: a foreign deployment's MINT does not trip this (see `bridge_mint_foreign_skipped_total`). | #2 |
| `bridge_faucet_ownership_drift_total{kind=drift\|renounced}` | Faucet owner storage slot moved away from the configured bridge AccountId. `renounced` wedges the faucet permanently. | #4 |
| `bridge_forged_mint_total{reason}` | MINT note **in our deployment's flow** that does not reconcile to an aggkit-recorded claim — forged via NoAuth. `reason=no_claim`: its serial matches no recorded claim's PROOF_DATA_KEY (after a short grace window). `reason=detail_mismatch`: its serial is recorded but the observed recipient, amount, or asset differs from the claim's expected MINT — fires immediately. Provenance-scoped (foreign MINTs skipped; native claims are never whitelisted). | #4 |
| `bridge_out_self_targeted_total` | B2AGG whose `destination_network` equals our `network_id`. Each one is a poison leaf. | #13 |

| Metric | Meaning |
|---|---|
| `bridge_out_unknown_faucet_total` | B2AGG referencing a faucet not in our registry. Quarantined to prevent re-loop; stuck until the registry is updated. Investigate any non-zero rate. |
| `bridge_out_quarantined_erased_b2agg_total` | Consumed B2AGG skipped because its contents were unparsable / faucet unknown (MA#18); a quarantine-table row was written as the recovery handle. |
| `bridge_unknown_wrapper_consumed_total` | Bridge consumed a note whose script root is neither B2AGG nor CLAIM (MA#4) — the LET advanced but no event can be synthesised. Investigate before more funds strand. |
| `bridge_out_invalid_destination_total` | B2AGG with zero-address / EVM-precompile destination. Refused. Steady non-zero rate = upstream client bug. |
| `claim_unclaimable_total{reason}` | Claim recorded as unclaimable (e.g. unresolvable destination). Each one is a user-visible stuck deposit. |

### Foreign-deployment provenance skips — informational, DO NOT page

The note scripts (MINT/BURN/CLAIM/B2AGG) are deployment-independent, so a chain
shared with a FOREIGN agglayer deployment leaks that deployment's notes into our
store. These counters record notes the provenance gates attributed to another
deployment and skipped, so a false critical alert is never raised. A steady rate
is normal on a shared chain; a sudden spike just means a foreign deployment got
busier. Investigate only if it coincides with our own funds behaving oddly.

| Metric | Meaning | Cantina ref |
|---|---|---|
| `bridge_mint_foreign_skipped_total` | Consumed MINT whose immutable content positively references another deployment. Skipped by the #2/#4 gate. | #2/#4 |
| `bridge_twin_note_foreign_skipped_total` | Consumed note whose immutable content positively references another deployment and is skipped by the twin detector. | #6 |
| `bridge_burn_foreign_skipped_total` | Consumed BURN whose immutable content positively references another deployment and is skipped by the serial tracker. | #5 |
| `address_mapper_zero_padding_fallback_total` | No explicit eth→Miden mapping; fell back to zero-padding. Account existence NOT verified — alert on unusual rates. |
| `address_mapper_hardhat_alias_rejected_total` | Hardhat-alias remap refused (`DISABLE_HARDHAT_ALIAS`). Non-zero in production = someone deposits to the well-known dev address. |

## Bridge integrity: page on increase

These counters represent fail-close integrity detections, not routine traffic:

- `synthetic_projector_completeness_missing_total`;
- `synthetic_projector_b2agg_fetch_missing_total`;
- `bridge_burn_serial_collision_total`;
- `bridge_twin_note_detected_total`;
- `bridge_mint_target_mismatch_total`;
- `bridge_faucet_ownership_drift_total`;
- `bridge_forged_mint_total`;
- `bridge_unknown_wrapper_consumed_total`;
- `bridge_out_self_targeted_total`;
- `bridge_out_invalid_destination_total`;
- `bridge_out_quarantined_erased_b2agg_total`;
- `bridge_out_metadata_unrecoverable_total`;
- `claim_watcher_storage_decode_total`;
- `claim_watcher_unrecoverable_total`;
- `store_envelope_decode_errors_total`;
- `faucet_registry_reconciler_unknown_faucet_total`.

`bridge_out_unknown_faucet_total` also requires immediate triage and normally
correlates with a quarantine row. `claim_event_foreign_skipped_total` can be
expected on a Miden chain shared with a foreign deployment; on a
single-deployment chain it is anomalous.

## Example PromQL

Adapt job/instance selectors to the deployment:

```promql
# RPC p99 by method
histogram_quantile(
  0.99,
  sum by (le, method) (rate(rpc_request_duration_seconds_bucket[10m]))
)

# Writer p99 by kind
histogram_quantile(
  0.99,
  sum by (le, kind) (rate(agglayer_writer_job_duration_seconds_bucket[10m]))
)

# Hard event-completeness page
increase(synthetic_projector_completeness_missing_total[5m]) > 0

# Authoritative B2AGG body unavailable after retry
increase(synthetic_projector_b2agg_fetch_missing_total[5m]) > 0

# Writer backpressure
rate(agglayer_writer_queue_full_rejections_total[5m]) > 0.1

# Any partial graceful drain
increase(agglayer_writer_drain_outcome_total{outcome="partial"}[15m]) > 0
```

For counters that may not yet exist, use an explicit absent-series rule suited
to the alert. Do not globally coerce all missing metrics to zero because a dead
scrape target and a never-emitted healthy counter are different conditions.

## Logs and dashboards

Keep these structured fields searchable: image/digest, pod UID, `tx_hash`,
`job_id`, `kind`, `signer`, `note_id`, `global_index`, block/cursor fields, and
error chain. High-value message families include:

- `heartbeat`;
- `note reconciler` and `visibility barrier`;
- `authoritative duplicate reconciliation`;
- `writer_worker` and `durable note handoff`;
- `L1InfoTreeIndexer`;
- `SECURITY TRIPWIRE`;
- `reimported from node` / `account reimport failed`;
- `quarantin` / `completeness`.

A compact dashboard should show endpoint health, Miden/synthetic tips, all
three durable cursors, writer depth/latency/outcomes, proof latency/outcomes,
L1 indexer position, restart/drain events, and a single zero-baseline panel for
the integrity counters.

For continuous test-environment checks, the repository also provides
`scripts/monitoring/watch-completeness.sh` and
`scripts/monitoring/immutability-monitor.py`. Read their prerequisites and
endpoint assumptions before using them outside the checked-in Compose stack.
