# Follow-ups: H6 evidence provenance (scoped OUT of the rc.1 migration PR)

Two related gaps in audit-H6 corroboration were surfaced by the local review of
`chore/node-0.16.0-rc1`. Both are **pre-existing on `main`** — neither is caused
by the rc.1 migration — and both need more design than a release-candidate
branch should absorb. They are filed here with the requirements the review
established, so the work starts from the analysis rather than repeating it.

An attempt to fix them inside the rc PR was reverted. That attempt is the
evidence for why they need their own change: each round of review found the
partial implementation had a new fail-open or fail-closed edge (unsafe
trust-on-first-use, a genesis wildcard that re-blessed old evidence, a migration
checksum trap that would have bricked upgraded databases, and a provenance flag
that was wrong in three of four boot orderings). A half-correct fail-closed
startup gate is worse than a known gap: the gap is documented and bounded, while
a wrong gate refuses to boot a healthy deployment.

## 1. GER corroboration is not bound to the L1 it came from

`ger_entries` rows and the evidence cursor record that a root was OBSERVED on
L1. Nothing records WHICH L1. Point the same database at a different chain, a
re-genesised devnet, or a redeployed GER manager and every historic row is still
consulted as corroboration — so under strict H6 a GER can be admitted on
evidence gathered somewhere else. Resetting this project's own e2e stack
produces exactly that shape: same chain id, same deterministic contract address,
brand new chain.

Requirements for a correct fix:

* Persist a source fingerprint that includes an immutable chain checkpoint (the
  genesis block hash — chain id and contract address alone cannot see a
  re-genesis), the GER manager address, and the confirmation tag.
* Verify it BEFORE any path that writes evidence. Today the only such path is
  the serving-side indexer (its startup barrier and its ticker): the
  restore-time catch-up was REMOVED precisely because it wrote L1-derived
  evidence without a source check, which base never does. If this work
  reintroduces a restore-time scan, the source check must come first.
* A database that already holds evidence but has no recorded source is the
  UPGRADE state. Recording the current configuration there is trust-on-first-use
  over data that predates the record: it must require an explicit operator
  assertion, not happen by default. "Already holds evidence" means indexed rows
  OR a non-zero cursor — rows are durable before the (best-effort) cursor write.
* An absent/empty checkpoint on an older row must NOT act as a wildcard that
  matches every chain and is then overwritten with the current value; that
  relabels old evidence as belonging to the new source.
* Every identity RPC needs a deadline (the alloy HTTP provider has none).
* Schema changes go in a NEW migration. The migrator rejects a changed checksum
  for an already-applied migration, so editing one in place bricks any database
  that ran the earlier version.
* **Migration names `023` and `024` are BURNED.** Intermediate commits of this
  branch applied files under both names, and those databases still carry the
  applied-migration rows. Start at `025`, and have the migration tolerate
  schema objects left behind by the reverted attempt (`l1_evidence_source`, and
  a `cursor_inherited_from_legacy` column on `l1_indexer_state`).

## 2. The `latest` policy binding inherits an unverified cursor

`bind_l1_evidence_policy` copies the pre-policy `last_processed` into the
selected-policy cursor when binding `latest`, so no events are skipped across
the upgrade restart. Under strict H6 that hands the database a NON-ZERO evidence
cursor describing rows scanned before the policy existed, which still carry
`finalized_verified = false`. `check_h6_backfill_invariant` reads a non-zero
cursor as "this database has a real evidence index" and waives the demand for an
explicit `--l1-indexer-from-block`, so those rows are scanned PAST rather than
corroborated.

Suppressing the inheritance only when the first binding happens to be strict is
NOT sufficient. The binder returns early when the policy tag already matches, so
these orderings all defeat it:

* lenient boot inherits, later strict boot returns early and accepts the
  inherited cursor;
* strict boot tags the database and commits cursor 0, fails the invariant, and a
  lenient retry then returns through the same-tag branch and never inherits —
  starting at the current head and skipping the legacy-to-head interval;
* pre-existing databases carry no provenance at all, so a backfill of the flag
  would be guesswork.

Requirements for a correct fix: durable cursor PROVENANCE (was this position
inherited, or actually scanned under the policy?) plus a retirement transition —
"historical coverage completed from a trusted frontier" — so that a genuine
explicit backfill clears the condition permanently instead of requiring the
operator override on every subsequent strict restart. That is a small state
machine and needs its own upgrade tests, including a live migration test against
each supported lineage.

## 3. L2B chaos watchdog needs an L2B-side admission probe

The chaos watchdog previously watched `aggkit-l2b` but probed the BASE proxy's
`transactions` table, while `aggkit-l2b` submits to `anvil-l2b`. Every healthy
L2B transaction was therefore "unknown to the proxy" by construction, and
unrelated base GER activity could satisfy an L2B recovery proof. The watchdog
now covers only the base aggkit — watching through the wrong database is worse
than not watching. Restoring L2B coverage needs an L2B-side admission probe and
a healer that reads the L2B database.

## 4. L1-GER proof-serving consistency check (retired, needs deriving)

The L2L2 preflight used to assert that bridge-service could serve /merkle-proof
for net-1 deposits (the #111 starvation shape). Three successive models were all
wrong, in both directions:

1. a join over all rows with a newest-N RANK grace — rank is not age, small
   populations were swallowed whole, and a "population grew" branch passed while
   every row was unmatched;
2. an id-based identity rule across probes — defeated by reorg
   delete-and-replay, since the pinned service's `Reset` cascade-deletes
   `sync.exit_root` rows and ids are sequence-backed;
3. the synchronizer's ROOT-HYDRATION predicate (`allowed AND block_id > 0`,
   empty `exit_roots`) — which is the background work queue, not the request
   selector. With `FinalizedGEREnabled` the endpoint serves from the single
   LATEST TRUSTED row (`GetLatestTrustedExitRoot`, id DESC), so a historical
   orphan beneath a newer hydrated row reds a stack the endpoint serves fine.

The verdict is therefore RETIRED; the preflight now only asserts the tables are
readable and says explicitly that consistency is not checked.

Requirements for a correct version: evaluate the row `GetClaimProof` would
actually SELECT under the deployment's own configuration (which differs with
`FinalizedGEREnabled`), pinned to the bridge-service commit in use, with a test
that drives the real selection rather than a count. Until then, #111 detection
rests on the functional gates — the full-DB-loss drill now requires an actual
claim to SETTLE, not merely reach `ready_for_claim`.
