# #146 finding 1 — durable `parked_during_recovery` marker

Codex blocking finding 1 on PR #155. Design settled and every touch point
located; implementation is the only thing outstanding.

## The bug

After a full DB loss the nonce ledger is empty (#90), so a continuing wallet
submitting nonce 21 would be refused "expected 0" forever. #90's fix opens a
bounded **post-rebuild recovery window** during which a signer may adopt a
baseline: seed its ledger from the lowest parked nonce.

Adoption is not immediate — the sweep first waits `BOOTSTRAP_SETTLE_BLOCKS` to
be sure no *lower* nonce is still arriving (seeding to 21 while 19 is in flight
would strand 19). Correct, but it means a tx parked inside the final settle
margin cannot be seeded on that sweep, and by the next sweep the global window
has closed for good. That transaction sits behind nonces that will never arrive.
It was ACKNOWLEDGED, so nobody resubmits it — #119 reached by another route.

`src/service_send_raw_txn.rs` already documents this as an accepted residual and
names the fix; this is that fix.

## Why the obvious predicate is wrong

Grandfathering "signer has parked rows AND `nonce_get() == 0`" was tried in
round 4 and reverted. An ABSENT ledger row also reads 0, so on a fresh
deployment a genuinely new wallet could submit nonce 42, park, and be seeded to
42 — skipping 0..41. A nonce-ordering violation, and a permanent amnesty rather
than a bounded one. The predicate cannot distinguish a continuing wallet from a
new one, so it must not be asked to.

## The fix: proof-carrying eligibility

Stamp the QUEUE ROW at park time with "parked while the recovery window was
open". Eligibility becomes `global_window_open || row.parked_during_recovery`.

A stamped row is provably a continuing wallet's tx from the rebuild era. A new
wallet's out-of-order tx never gets stamped, so it can never be amnestied. The
marker must be durable precisely because its job is to outlive the window and
survive a restart.

## Touch points (verified 2026-08-26)

| # | File | Change |
|---|---|---|
| 1 | `migrations/020_queued_txns.sql` | add `parked_during_recovery BOOLEAN NOT NULL DEFAULT FALSE` to the `CREATE TABLE` |
| 2 | `src/store/mod.rs:371` | `QueuedTxn` gains `pub parked_during_recovery: bool` |
| 3 | `src/store/mod.rs:824` | `queue_txn` gains a `parked_during_recovery: bool` parameter |
| 4 | `src/store/postgres.rs:2000` | INSERT the column |
| 5 | `src/store/postgres.rs:2020` | `SELECT tx_hash, envelope, expires_at, parked_during_recovery` and populate the struct |
| 6 | `src/store/memory.rs` | mirror on the in-memory store |
| 7 | `src/service_send_raw_txn.rs` (`signer_may_bootstrap`) | `global \|\| lowest-parked-row carries the marker` |
| 8 | park call site | pass `recovery_bootstrap_active(service).await` |

**No new migration.** `queued_txns` is created by 020, which ships in THIS PR —
the txpool has never been deployed, so no database anywhere has the table and
there is nothing to migrate from. Editing 020 changes its checksum, so any dev
stack that already applied it needs a DB recreate; the restore drill wipes the
DB regardless. A 026 `ALTER` would avoid the recreate at the cost of a migration
that exists only to patch an unshipped one.

## Validation — the drill, not unit tests

Unit tests can prove the marker is written and read back. Only the full-DB-loss
drill proves the actual property, because the bug exists only after a rebuild:

1. continuing wallet parked inside the final settle margin RECOVERS
   (previously stranded forever), and
2. a new wallet submitting a high nonce on a fresh store is STILL REFUSED
   (the round-4 regression must not come back).

Point 2 is the one that matters. Two previous attempts at this predicate were
wrong in exactly that direction and were caught only by reasoning, not by a
test — so the test has to exist before the fix is trusted.
