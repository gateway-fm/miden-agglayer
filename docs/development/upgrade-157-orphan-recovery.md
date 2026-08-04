# Release upgrade note — #157 (automatic orphan recovery)

Release-specific companion to the version-neutral [`docs/UPGRADE.md`](../UPGRADE.md).
Read that first for the safety invariants and the in-place procedure; this note
records what changes in the `main + #157` release and how to verify the upgrade.

## What #157 changes

| Area | Change | Upgrade impact |
| --- | --- | --- |
| **Postgres schema** | Migration `021_orphan_recovery_backoff.sql`: `ALTER TABLE transactions ADD COLUMN IF NOT EXISTS recovery_attempts INTEGER NOT NULL DEFAULT 0, next_recovery_at BIGINT` + partial index `idx_txns_pending_recovery`. | **Additive, idempotent, auto-applied on startup** (in-process migrator). No existing migration is modified. |
| **Startup / runtime behaviour** | A recovery loop runs at startup (spawned + time-bounded, so it never blocks the HTTP bind) and on a 30 s sweep. It walks durable `pending` rows per signer and drives each acknowledged-but-orphaned transaction to a terminal outcome without client rebroadcast. | Acts on **durable state the old binary wrote** — this is the surface the upgrade test exercises. |
| **Miden note creation** | GER/claim submission notes now carry a finite transaction `expiration_delta` (default 64 blocks, env `AGGLAYER_SUBMISSION_NOTE_EXPIRATION_DELTA_BLOCKS`) instead of Miden's "never expire" default. | Note **format is unchanged** (this is a property of the creating tx). Only affects notes created by the new binary. See the rollback edge case below. |
| **Receipt semantics** | A recovered claim whose global index was landed by a different transaction is finalised as a **reverted `AlreadyClaimed`** receipt (status `0x0`, geth-faithful) rather than a false success. | Additive terminal outcome; aggkit/bridge-service already tolerate reverted claim receipts. |

There are **no on-chain / bridge-contract changes, no note-layout changes, and no
breaking `eth_*` RPC changes.** `CHAIN_ID`, `NETWORK_ID`, bridge address, and Miden
network are untouched — this is an in-place upgrade, not a redeployment.

## Compatibility summary

- **Forward:** the new binary applies `021` automatically on first start, then the
  recovery loop reconciles any pre-existing durable state.
- **Backward (rollback):** the old binary runs against a `021`-migrated DB — the two
  added columns are simply ignored. See the caveats below before relying on this.

## Upgrade procedure (delta over `docs/UPGRADE.md`)

Follow `docs/UPGRADE.md` exactly (preserve the Miden store dir + Postgres volume +
single-replica ownership). The only `#157`-specific expectations:

1. From the last release **v0.15.9**, `git diff --name-status v0.15.9 <NEW_REF> --
   migrations` shows two **additive** files: `A migrations/019_claim_calldata_repair_
   pending.sql` (a new `CREATE TABLE IF NOT EXISTS`) and `A migrations/021_orphan_
   recovery_backoff.sql`. Both apply automatically on startup. Anything else is
   another PR in the release and needs its own note.
2. On first start of the new image, expect one startup recovery pass in the logs
   (`target=recovery`). It is bounded by `RECOVERY_SWEEP_INTERVAL_SECS` and spawned,
   so a slow/unavailable Miden node cannot delay the port binding.
3. No manual migration, flag, or config change is required.

## Post-upgrade verification

```sql
-- 021 applied:
SELECT column_name FROM information_schema.columns
 WHERE table_name='transactions' AND column_name IN ('recovery_attempts','next_recovery_at');
SELECT indexname FROM pg_indexes WHERE indexname='idx_txns_pending_recovery';

-- Recovery is draining the pre-upgrade backlog (should trend to ~0):
SELECT count(*) FROM transactions t
  LEFT JOIN tx_note_links l ON l.tx_hash=t.tx_hash
 WHERE t.status='pending' AND l.tx_hash IS NULL AND t.miden_tx_id IS NULL;   -- pure orphans
```

Metrics (`/metrics`) to watch after the cutover:

- `pending_unlinked_txns` / `pending_unlinked_oldest_age_seconds` — should fall after
  the first sweeps; a persistently rising oldest-age is the alert.
- `orphan_recovery_successes_total` / `orphan_recovery_redrives_total` /
  `orphan_recovery_already_claimed_total` — should advance as the backlog drains.
- `orphan_recovery_persistent_failures_total` — must stay flat; any increase means a
  row is failing recovery repeatedly and needs operator attention.

Functional check: submit one fresh deposit/withdrawal after the cutover and confirm it
completes end-to-end.

## Rollback caveats (new → old)

The DB migration is additive, so the old binary starts against a `021` DB. Two things
to know before rolling back:

1. **Notes created by the new binary have a finite expiration.** A GER/claim note the
   new binary created but that is not yet consumed will expire after
   `~AGGLAYER_SUBMISSION_NOTE_EXPIRATION_DELTA_BLOCKS`; the old binary has no recovery
   loop to rebuild it, so an interrupted-at-that-moment submission would need a client
   rebroadcast (the pre-#157 behaviour). Prefer rolling back during a quiet window.
2. **The old binary cannot self-heal orphans.** Any orphan the new binary had not yet
   finished recovering reverts to being stuck until it is re-driven forward again.

Rollback is possible but not perfectly clean; a database backup taken per
`docs/UPGRADE.md` remains the authoritative rollback path.

## Known edge case: pre-#157 `prepared` handoffs with no finite expiration

A transaction that the **old** binary left with a `prepared` note handoff (crashed
between recording the note and confirming its Miden submission) has a note with the
old "never expire" transaction expiration. After the upgrade the new recovery's
expiration-gated clear (`clear_expired_prepared_note_handoff`) can never fire for it
(the reconcile cursor never passes `u32::MAX`), so recovery **polls it safely but does
not re-drive it** — it is *never* re-proved or double-claimed, but it also does not
self-heal. This is the same terminal situation as the old binary (which had no
recovery at all), so the upgrade does not regress it. Identify any such rows and
resolve them out of band:

```sql
SELECT t.tx_hash, l.handoff_state, l.prepared_expiration_block
  FROM transactions t JOIN tx_note_links l ON l.tx_hash=t.tx_hash
 WHERE t.status='pending' AND l.handoff_state='prepared'
   AND l.prepared_expiration_block >= 4294967295;   -- u32::MAX == pre-#157 "never expire"
```

New-binary submissions do not create these (they carry a finite expiration), so this
set is bounded to whatever existed at cutover and does not grow.

## Automated upgrade test

Two complementary tests share the release-override mechanism
(`scripts/upgrade/docker-compose.upgrade-release.yml`, which pins the **release image
and the release command line** so the old binary is not handed flags it would reject):

- **`scripts/e2e-upgrade-test.sh`** — the general in-place upgrade harness
  (R → U1 → RB → U2): no data loss (cursors resume, no genesis re-sweep), **getLogs
  immutability** across each swap, and live L1↔L2 traffic in every phase.
- **`scripts/e2e-upgrade-recovery.sh`** — the #157-specific test. On one persistent
  stack it brings the stack up ON THE RELEASE **v0.15.9** (release override), runs a
  baseline deposit to completion, then manufactures a **pending orphan** by capturing a
  REAL deposit's claim while it is durably-admitted-but-unlinked and SIGKILLing the
  release proxy (v0.15.9 has no recovery loop, so it stays stuck; bridge-service is
  stopped so no rebroadcast). It then swaps **only the proxy** to the branch image on
  the same volumes and asserts: (1) migrations 019 + 021 applied; (2) the pre-upgrade
  orphan self-heals to the **same hash exactly once** — nonce **not** re-advanced,
  wallet credited exactly once via the deposit wrapper's own balance-delta oracle, and
  healed by recovery (not a rebroadcast); (3) the baseline terminal state is preserved
  and a fresh post-upgrade deposit works.

Both build `miden-agglayer-e2e:v0.15.9` from the tag (clean worktree) if absent and
`miden-agglayer-e2e:latest` from the branch. Run standalone (they need the
`8546`/`9545`/`18080` ports free — not alongside the from-scratch gates):

```bash
COMPOSE_PROJECT_NAME=gate55 ./scripts/e2e-upgrade-test.sh        # general
COMPOSE_PROJECT_NAME=gate55 ./scripts/e2e-upgrade-recovery.sh    # #157 recovery-of-old-state
# optional: UPGRADE_FROM_REF=<tag> to upgrade from a different released tag
```
