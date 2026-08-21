-- Round-4 review. Two additions, both consequences of earlier fixes.
--
-- 1. `l1_evidence_source.genesis_hash`.
--    (chain_id, ger_address) cannot see a RE-GENESISED devnet that kept its
--    chain id and redeployed the GER manager to the same deterministic
--    address — which is exactly what resetting this project's e2e stack
--    produces. Block 0's hash is the immutable checkpoint that can.
--
--    This lives in 024 rather than being edited into 023 on purpose: the
--    migrator rejects a changed checksum for an already-applied migration
--    (store/migrator.rs), so editing 023 in place would have made every
--    database that already ran it refuse to boot.
ALTER TABLE l1_evidence_source
    ADD COLUMN IF NOT EXISTS genesis_hash TEXT NOT NULL DEFAULT '';

-- 2. `l1_indexer_state.cursor_inherited_from_legacy`.
--    Binding the `latest` policy on an upgraded database copies the old
--    pre-policy scan cursor (`last_processed`) into the selected-policy cursor
--    so no events are skipped over the restart. Convenient — but it makes the
--    cursor look like real, policy-scanned evidence to
--    `check_h6_backfill_invariant`, which treats "cursor > 0" as "this
--    database has a genuine evidence index" and therefore waives the demand
--    for an explicit --l1-indexer-from-block. The rows behind an inherited
--    cursor were never corroborated under the policy (they still carry
--    finalized_verified = false), so under strict H6 they are scanned PAST
--    rather than verified.
--
--    Suppressing the inheritance only in the first strict binding was not
--    enough: the binder returns early when the policy tag already matches, so
--    a lenient boot that inherited first, followed by a strict boot, walked
--    straight past the check. Recording the PROVENANCE of the cursor makes the
--    invariant correct in every ordering — strict treats an inherited cursor
--    as no cursor at all.
ALTER TABLE l1_indexer_state
    ADD COLUMN IF NOT EXISTS cursor_inherited_from_legacy BOOLEAN NOT NULL DEFAULT FALSE;
