# In-place upgrade guide

This is the version-neutral procedure for replacing a running
`miden-agglayer-service` image while preserving its Miden account identity and
synthetic EVM history. Release-specific migration and flag assumptions belong
in the release notes, not in this runbook.

## Safety invariants

An in-place upgrade must preserve all of the following:

1. The Miden store directory, including `store.sqlite3`, `keystore/`, and
   `bridge_accounts.toml`.
2. The Postgres store selected by `DATABASE_URL`.
3. Byte-for-byte `eth_getLogs` results for every block exposed before the
   upgrade.
4. The configured `CHAIN_ID`, `NETWORK_ID`, bridge address, Miden network, and
   L1 GER contract.
5. Single-replica ownership of the Miden sqlite store and `SyntheticProjector`.

Changing account IDs, network ID, bridge address, or authoritative chain is a
redeployment/migration, not an in-place upgrade.

## Before scheduling the change

Use immutable image digests for both the old and new images. Record the old
digest, full command/argument list, non-secret environment, mounts, security
context, and termination grace period. Keep a redacted copy of secret names and
sources; do not export secret values into the change ticket.

Review source changes between the deployed ref and target ref:

```bash
git diff --name-status OLD_REF NEW_REF -- migrations
git diff OLD_REF NEW_REF -- src/main.rs Dockerfile
```

The service applies embedded Postgres migrations automatically on startup.
Never assume a new schema is readable by the old image merely because a
migration is additive. If `migrations/` differs, test the exact
upgrade/rollback pair on copies of both stores and treat restoring the backups
as the rollback path.

Check that the target deployment satisfies the current hardening gate. With
`--require-hardening`, startup requires:

- `ADMIN_API_KEY`;
- a non-empty `ALLOWED_SIGNERS` list;
- no wildcard in `CORS_ALLOWED_ORIGINS`;
- no `--insecure-allow-any-signer`;
- a configured and reachable `MIDEN_PROVER_URL`.

Run the target binary's `--help` when preparing its manifest. Older and newer
images may accept different flags.

## Capture the pre-upgrade baseline

Set `PROXY_RPC` to the private/forwarded service endpoint, then record:

```bash
curl -fsS "$PROXY_RPC/health"
curl -fsS "$PROXY_RPC/metrics" > pre-upgrade.metrics
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
```

Capture and retain:

- the synthetic tip;
- all three bridge log families through that tip (or an established canonical
  hash of their JSON responses);
- the `service_state` row and applied migration list;
- the count and identities of pending transactions/note handoffs;
- the Miden account config checksum;
- current alert state and recent error logs.

Useful read-only Postgres queries:

```sql
SELECT * FROM service_state;
SELECT name, checksum, applied_at
FROM schema_migrations ORDER BY name;
SELECT t.tx_hash, t.signer, t.status, t.miden_tx_id,
       l.handoff_state, l.note_id, l.prepared_expiration_block
FROM transactions AS t
LEFT JOIN tx_note_links AS l USING (tx_hash)
WHERE t.status = 'pending'
ORDER BY t.created_at;
```

For upgrades that introduce the LET cardinality gate, keep
`service_state.let_gate_baseline` at its default `0` unless an offline audit proves
that pre-upgrade LET leaves are absent from `deposit_counter`. Follow
[`operations/let-cardinality-gate.md`](operations/let-cardinality-gate.md) while the
proxy is stopped; the runtime never infers or changes this offset.
Do not run a full `--restore` with a nonzero baseline: full replay is zero-based and
requires an offline history reconstruction first.

Take a custom-format `pg_dump` of the proxy database. Also arrange a consistent
snapshot of the entire Miden store directory. The safest filesystem snapshot is
taken after write traffic is quiesced and the old process has stopped cleanly.
Never back up or restore only `store.sqlite3` while omitting its keystore and
account config.

## Quiesce and stop

1. Stop or pause callers that submit `eth_sendRawTransaction` while leaving
   read traffic available.
2. Wait for `agglayer_writer_queue_depth` to reach zero. Record any remaining
   `transactions.status = 'pending'` rows; a durable note handoff may remain
   pending until Miden observation and is not by itself safe to discard.
3. Send SIGTERM. The service stops HTTP acceptance, signals the writer, waits
   20 seconds, records a clean/partial drain metric, and then closes its Miden
   client.
4. Configure a container termination grace period comfortably above 20 seconds
   (30 seconds is the minimum practical envelope; allow more for platform
   shutdown overhead).
5. After the old process exits, take the Miden-store snapshot if one was not
   taken by a crash-consistent volume-snapshot mechanism.

Do not run the old and new containers concurrently against the same Miden store.

## Start the new image

Change only the image digest and target-version arguments. Preserve the same
volumes, database, account config, chain/network IDs, and contract addresses.
Start exactly one replica.

At startup, expect the process to:

- validate hardening and probe the remote prover when required;
- run/check every embedded Postgres migration under an advisory lock;
- load the existing `bridge_accounts.toml` instead of initializing accounts;
- resume the projector and reconciler from durable cursors;
- start the single writer, L1 indexer (when both L1 settings exist), faucet
  reconciler, and HTTP service.

The upgrade that introduces the durable B2AGG identity ledger intentionally resets
`service_state.reconcile_cursor` to zero once. The reconciler makes one historical
pass to populate nullifier-to-NoteId identities, and the full-tip visibility barrier
holds synthetic projection until that pass reaches the current tip. This is an
expected one-time availability cost; later starts resume the persisted cursor.

Stop the rollout immediately if startup tries to initialize new accounts,
reports a migration checksum mismatch, points at a different network, or fails
the hardening/prover probe.

## Verify before restoring write traffic

1. `GET /health` returns HTTP 200 with `{"status":"ok"}`.
2. `eth_chainId`, `net_version`, and the account config match the baseline.
3. `service_state.projector_cursor` resumes. `reconcile_cursor` also resumes except
   for the one-time identity-ledger backfill described above; after that sweep it
   must persist and resume normally. A deliberate recovery flag is the only other
   valid reason for a full-history sweep.
4. Re-query the entire pre-upgrade block range and compare the canonical log
   fingerprint. Any difference is a release blocker.
5. The synthetic tip advances while the Miden chain advances.
6. These fail-close signals remain zero/no-increase:
   `synthetic_projector_completeness_missing_total`,
   `synthetic_projector_b2agg_fetch_missing_total`,
   `bridge_out_quarantined_erased_b2agg_total`, and the critical bridge
   integrity counters documented in `operations/monitoring.md`.
7. Previously pending exact handoffs either complete from observation or stay
   pending; they must not become fabricated failures.

Restore upstream write traffic gradually, then complete one L1-to-L2 and one
L2-to-L1 transaction while monitoring receipts, projector progress, writer
latency, GER indexing, and certificates.

## Handling a transaction pending across the restart

The store contains the signed EVM envelope, nonce reservation, and—once the
Miden boundary is crossed—the exact note handoff. The safe retry is always the
original signed transaction, producing the same EVM transaction hash.

- Rebroadcast that exact byte string through `eth_sendRawTransaction`.
- Do not create a different transaction at the same nonce.
- Do not delete `transactions`, `nonce_reservations`, `claimed_indices`, or
  `tx_note_links` rows to force progress.
- A `prepared` handoff remains ambiguous until the exact note is observed or
  the authoritative reconciler cursor passes its expiration bound. The service
  performs that classification; operators should not guess from wall-clock
  age.

## Rollback

Rolling back only the image is allowed **only** when the exact target/old pair
has been proven compatible with the post-upgrade stores and target-version CLI.
Use the recorded old digest and old argument list.

Otherwise:

1. Quiesce writers and stop the new process cleanly.
2. Preserve a forensic copy of the failed state.
3. Restore the Postgres and Miden-store snapshots as one coordinated recovery
   point.
4. Restore the old image digest and old arguments.
5. Start one replica and repeat the complete baseline verification.

Restoring local stores does not undo Miden or L1 transactions submitted by the
new version. If the new image mutated either chain, reconcile those authoritative
effects before serving traffic; never hide them by restoring only local state.

## Rehearsal

For every release pair, rehearse upgrade, write traffic, rollback, and
re-upgrade against cloned state. `scripts/e2e-upgrade-test.sh` is a development
fixture for the versions pinned inside that script; it is not a standing claim
that arbitrary release pairs are rollback-compatible. Update its pinned image,
override, endpoints, and assertions before using it as release evidence.


## Release-specific notes: v0.15.8 → this release

Validated by an in-place upgrade test on 2026-07-19 (fresh v0.15.8 stack →
traffic → force-recreate onto this image → traffic; event history preserved,
tip continuous, certification-grade chaos verdict on the upgraded stack).

### Store migrations (auto-applied on startup)

Eight new migrations land on first boot of the new image
(`011_nonce_reservations` … `018_claim_mint_expected`): fenced nonce
reservations, durable submission handoffs, L1 finality/evidence policy state,
per-exit deposit reservations, B2AGG identity backfill, and expected-mint
tracking. They are additive and idempotent; no manual DDL. First startup after
the upgrade may take marginally longer while they apply — wait for `/health`
before restoring write traffic, per the standard procedure above.

### New flags (both additive; absent flags preserve old behavior)

- `--network-rpc-url ID=URL` (repeatable; env `NETWORK_RPC_URLS`) — per-origin-
  network RPC for ERC-20 metadata recovery. REQUIRED for deployments bridging
  tokens whose origin is a second rollup (e.g. `2=http://<l2b-rpc>`): without
  it, `--restore` and the live recovery path cannot validate an L2B-origin
  token's metadata preimage and will defer those bridge-outs fail-closed.
  Network 0 continues to come from `--l1-rpc-url`.
- `--reject-unverified-ger-injection` — audit-H6 hardening; see the flag's
  help text. Recommended in production together with a `safe`/`finalized`
  `--l1-evidence-tag`.

### Behavior changes to know about

- **`--restore` now rebuilds historical claims** (Phase 2.6, node scan): after
  a full store recovery, aggkit's aggsender can resolve pre-recovery bridge
  exits and certificate settlement resumes. GER/hash-chain history still
  restarts by design (the chain rebuilds through live operation).
- **Duplicate landed claims are accepted-and-reverted** (geth-faithful,
  status-0x0, no ClaimEvent) instead of rejected at admission, so an external
  claim sponsor's transaction manager can never wedge on a user front-run.
- **Abandoned nonce slots self-heal**: a reservation whose transaction
  provably never reached durable admission (crashed mid-flight or released as
  failure) is reclaimable by a fresh transaction at the same nonce. Metric:
  `nonce_reservation_abandoned_reclaimed_total{cause}`.

### Upgrade-procedure hazard validated by this test: run from the SAME deployment directory

The e2e/compose deployment binds the Miden client store as a RELATIVE path
(`./.miden-agglayer-data`). Recreating the service from a different checkout
directory silently attaches a DIFFERENT store; if that store belongs to another
chain, the client's sync is rejected by the node (`accept header validation
failed`, genesis-commitment mismatch) and the synthetic tip freezes while the
node keeps mining. This exact failure was reproduced during the v0.15.8→this
release upgrade rehearsal. Rule: perform the image swap from the SAME compose
working directory (or an absolute, unchanged store path) so every mount
resolves identically — then verify sync (zero accept-header errors) and tip
advancement before restoring traffic. The upgraded proxy resyncs cleanly
through blocks mined during the swap window.

### Operational follow-ups shipped with this release

The operations runbook gained incident procedures for the two known
silent-freeze modes — "Stuck GER injection (interrupted `ger_insert`)" and
"ntx-builder silent death" (`docs/operations/runbook.md` §4) — including their
monitoring signals and watchdog allowances. Review alerting against those
sections when rolling this release out.
