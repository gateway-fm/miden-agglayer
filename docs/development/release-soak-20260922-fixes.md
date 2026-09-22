# Growing-chain release soak: permanent fixes

The September 18 attempt stopped in cycle 1's chaos gate. Operational recovery
on September 22 cleared the backlog; it did not fix the causes or make that
failed attempt pass. These changes build on the unmerged PR #223 harness and
address the failures captured in that attempt, plus retained-PG restore #222.

## Restore ordering (#222)

The global reservation total includes future replay windows. Subtracting only
the current window's reserved prefix gave `14 - 10 = 4`, although the first
window's valid starting index was zero. Restore now advances a canonical LET
frontier from genesis, including the configured audited baseline. Live projection
retains its existing tail-based validation. Both paths reject reordered or
non-prefix reservations, and restore cannot append a missing historical leaf
after retained future reservations.

Restore also checks authoritative replay coverage against the captured LET
count. A retained future crash reservation does not block earlier empty blocks,
but must emit before its own canonical block seals. The final global cardinality
and emitted-frontier gates remain required. A restart begins replay from genesis;
a retry in the same session keeps its frontier and fixed target tip.

Regression coverage uses 14 real reservations and notes spanning blocks 5,000
and 5,001, repeats replay, checks exact event blocks/indices, exercises a nonzero
legacy offset, and rejects missing/reordered history and current poison leaves.
A separate PostgreSQL integration test exercises the same production window gate
with retained reservations after cursor reset.

## Aggkit retry and watchdog evidence

The unchanged Aggkit v0.8.3-rc1 defaults `EstimateGasMaxRetries` to 1. Its pinned
eth-tx-manager v0.2.18 counts send failures toward eviction; the oracle then
keeps finding the evicted GER's monitoring ID instead of resubmitting it. The
fixture explicitly sets the supported retry limit to 0 (no retry eviction).
Harness deadlines, real delivery checks and bounded watchdog attempts still
limit failure; no Aggkit binary or source is modified.

The watchdog now correlates the actual signed hash in failed-send logs as well
as successful broadcasts. A bounded cache keeps signed identities after their
log lines leave the rolling window. Retry age/count must still come from fresh
logs for the same injection and process generation. Ambiguous identities or a
replacement overflow cannot authorize recovery. Admission is checked with signed
hashes and the process generation is rechecked before stopping the container.
The archived failed-send burst is a regression fixture.

## Stage fee budgets

Ordinary transaction runway excludes the large grant sent to each new faucet.
The failed run had 184,485 fee units, enough for 878 worst-case ordinary
transactions but less than one 215,040-unit faucet grant.

Load stages now budget ordinary activity and every planned new faucet using the
actual configured cascade size. Chaos budgets both its storm and its fresh
post-chaos probe before injecting faults. Nested workloads inherit that prepared
budget; no fee top-ups occur inside the measured fault window. Existing bridge
and faucet accounts receive ordinary runway too. The fee-exhaustion test does
not use these load-stage helpers and retains its deliberate exhaustion.

New identity-based fee samples distinguish faucets with equal symbols. Their
refresh timestamps and expected account count let preflights reject stale or
incomplete observations. The helper is restricted to the local Anvil fixture,
checks container/endpoint/genesis-volume ownership, serializes funding, and
journals the transfer intent before submitting. A failed or interrupted transfer
is never automatically resent: required balances must be observed before its
intent clears. Funding failure stops stage preparation.

## Bridge synchronizer recovery

Fresh L2 log lines and a cached `sync.status.synced=true` hid a dead L1 loop.
Preflight now checks fresh successful iteration timestamps for each required
network independently. It does not compare event-bearing block heights with
the current head, since an empty chain can legitimately retain an old event.

During chaos, a bounded watchdog may restart the existing bridge-service after
backing up its database and logs. It checks dependency health and process
generation again immediately before restarting, and defers during paused
PostgreSQL, stopped services or the node partition. It changes no index rows,
readiness flags, image, or mount. It records only that iterations resumed;
existing GER nudges, readiness/proof checks, deliveries and exact-block event
audit determine whether the workload recovered. Both watchdogs stop before the
post-chaos liveness verdict, preserving the existing self-recovery requirement.

## Validation record

- Workspace Rust library and binary tests pass; PostgreSQL-dependent tests need
  a configured database and are not counted as database validation in that run.
- The new retained-window regression passes separately against PostgreSQL 16
  with all migrations applied to an isolated synthetic test database.
- `cargo clippy --workspace --all-targets --features postgres -- -D warnings`
  passes.
- Linux `make test-scripts` passes, including the full GNU/Linux healer lifecycle
  tests, failed-send evidence, fee budget/ambiguous-transfer checks, and
  per-network bridge health/recovery tests.
- Full restore against a copy of the growing-chain PG state with fresh SQLite
  is pending explicit approval to copy that database. It is not claimed passed.

The live application remains 988c18d, its recorded harness remains 00fed23, and
the old attempt remains FAILED with zero completed resumed cycles. These changes
have not been deployed to that fixture or certified by a new N30/chaos cycle.
No merge, release or tag is part of this change.
