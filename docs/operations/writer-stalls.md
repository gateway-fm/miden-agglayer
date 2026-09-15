# Writer stalls and claim lease expiry (#210)

## What the incident establishes

The September 14, 2026 report against 0.16.2 shows long pending GERs and a
238-second claim whose ownership fence expired despite a 0.79-second proof.
The available logs do **not** establish which operation caused the 46-minute
stall. The following defects are reproducible from the code:

- Background sync retried every error forever while holding the only Miden
  client. A queued writer could never start, so its prover timeout never ran.
- Commit polling counted attempts, not elapsed time. Every attempt could run
  a full sync and retry RPCs. A nominal 20/30-second wait could take minutes.
- The 120-second claim lease started before acquiring the client and was never
  renewed. A live owner could expire while waiting, executing, or proving.
  The final ownership fence correctly prevented that owner from submitting;
  expiry alone is sufficient, even without a competing owner.
- Proof metrics cover proving, and writer duration histograms update when a
  job finishes. Neither identifies the phase of an ongoing stall.

A fresh local E2E chain and nearby services may never cross these deadlines.
Testnet can add node throttling, network latency and a larger sync/projection
history. The pinned SDK also honors server `retry-after` delays outside its
individual gRPC request timeout. These are possible contributors, not evidence
that any particular one caused this incident. The regressions deliberately
inject a stalled RPC and 238 seconds of claim lifetime.

`transactions.updated_at - created_at` also includes waiting for note consumption
and projection; it is not a measurement of proving or just writer dispatch.
Pending GER/claim rows deliberately use a NULL `miden_tx_id`:
consult the exact note handoff rather than using NULL as proof of no submission.

## Hotfix behavior

Background sync makes one attempt, then yields the client and retries on a
later tick. Background and pre-GER sync attempts have a 120-second elapsed-time
limit (`MIDEN_SYNC_TIMEOUT_SECS`, positive integer seconds; default 120).
A failed initial sync keeps readiness degraded, and a later successful sync
restores it. Missed background ticks do not accumulate. Commit observation is bounded by
`max_attempts * poll_interval`, including sync time.

`ClaimGuard` renews the current executing claim every third of its lease.
`CLAIM_RESUBMIT_TTL_SECS` remains 120 by default (minimum 3 seconds); it now
controls orphan recovery, not the maximum legitimate claim duration. Renewal
requires an unexpired lease, matching owner and fence, and executing state in
both stores. Preparation seals the handoff and stops renewal; dropping the
guard stops its heartbeat. A stale owner cannot revive itself or replace a
successor. No database migration is required.

There is no unsafe outer timeout that abandons a running submission and writes
a failure receipt while it can still reach the node. A timeout after durable
handoff leaves the receipt pending for exact-note reconciliation. Long execution,
submission, listener and store operations are diagnosed by ongoing phase logs;
this hotfix does not assert that every possible writer stall is eliminated.

## Enable and read the traces

Set this on the proxy process through the deployment's normal configuration
and restart procedure; the tracing filter is read at process startup:

```text
RUST_LOG=info,writer_diagnostics=debug
```

Keep any existing deployment-specific filter directives when adding the target.
This target is not suppressed by the production logging filter. It logs no
request payloads, signatures, credentials or endpoint URLs. Generic stage
failures record the outcome and error type; the caller retains error handling.

Each observed operation emits `stage started` and `stage finished` with
`operation`, `stage`, a process-local `stage_id`, elapsed seconds, and outcome
`ok`, `error`, or `cancelled`. A still-pending stage emits a WARN every 30 seconds
even with the default INFO filter. Writer events carry the `writer_job` span
with transaction `hash`, `job_id`, signer and kind, propagated onto the client
owner. Background listeners are identified by their concrete type.

| Last active stage | What to investigate |
|---|---|
| `client_enqueue` / `client_response`, without `client_request` starting | Another operation owns the serialized client; find its active sync/listener/request stage |
| `client/sync` or `commit_wait/sync` | Full SDK sync; correlate with the active node RPC and SDK retry warnings |
| `rpc_pace/<method>` | Local rate-governor wait, before the RPC starts |
| `node_rpc/<method>` | Network call including SDK retries and server retry delays |
| `post_sync` | Named listener; projector reconciliation, calldata backfill, block projection/cursor persistence and bridge scanner stages provide finer detail |
| `l1_evidence` | GER awaiting the configured L1 observation policy |
| `acquire_lease`, `faucet`, `build_note`, `execute`, `remote_sign` | Claim ownership, setup, execution or signer work before proving |
| `prove` / `prove_fallback` | Proof generation; compare the existing prover metrics |
| `prepare_handoff`, `submit`, `apply`, `commit_wait`, `confirm_handoff` | Exact submission boundary; preserve durable handoffs on uncertainty |
| `dispatch` without a more specific active stage | Store work or another unclassified path; retain the surrounding logs and receipt state |

Stages nest: match start/end by `stage_id` within one process and follow the
innermost unfinished stage. Missing heartbeats as well as missing completion
can indicate a blocked runtime or process; retain CPU/thread diagnostics before
restart. A WARN alone indicates elapsed time, not proof of a deadlock.

If the stall recurs, collect the proxy and sender logs from admission through
the stall, two metrics snapshots at least 30 seconds apart, and these read-only
queries for the affected hash (bind `tx_hash` using psql's `-v` option):

```sql
SELECT t.tx_hash, t.status, t.error_message, t.miden_tx_id,
       t.created_at, t.updated_at, t.recovery_attempts,
       l.handoff_state, l.note_id, l.prepared_expiration_block
FROM transactions t LEFT JOIN tx_note_links l USING (tx_hash)
WHERE t.tx_hash = :'tx_hash';

SELECT global_index, owner_tx_hash, fence_token, claim_state,
       lease_expires_at, now() AS observed_at
FROM claimed_indices WHERE owner_tx_hash = :'tx_hash';
```

If a sender polls a hash rejected with `nonce too low` and there is no durable
row, investigate its send-error handling. The proxy does not create a receipt
for an unaccepted transaction. Capture both the originally accepted hash and
the rejected hash, along with the sender's nonce and RPC error; do not weaken
the replay guard or manufacture a success/failure receipt.

## Alert while the operation is still running

Alert on `agglayer_writer_oldest_nonterminal_age_seconds > 300` for one minute
(tune to the deployment's service objective). The sweeper samples age every
30 seconds or less independently of dispatch completion, including when queue
depth is zero. The value returns to zero when there is no nonterminal work.

Use `miden_operation_stage_duration_seconds{operation,stage}` and
`miden_client_queue_wait_seconds{operation}` for completed-wait distributions;
`miden_operation_stages_total{operation,stage,outcome}` includes cancellations.
Watch `miden_sync_timeouts_total`, `miden_commit_wait_timeouts_total`, and
`claim_lease_renewals_total{outcome="error"|"timeout"}` for failures. A stopped
claim heartbeat can be normal after preparation; its debug event says why the
renewal loop stopped. Preserve this evidence if the public-testnet tail remains
after the hotfix; passing local E2E alone does not establish its incident cause.
