-- ============================================================================
-- Finalized-chain tie for strict-H6 GER evidence (audit H6 BLOCKER 1)
-- ============================================================================
--
-- Recording the (mainnet, rollup) decomposition from a `latest` observation and
-- authorizing when `evidence.block <= finalized_block` only proved SOME block at
-- that height was finalized — NOT that THIS decomposition is on the canonical
-- finalized chain. A row from a later-reorged fork, whose height is still
-- <= finalized, could authorize an IRREVERSIBLE injection for a GER that never
-- made it onto canonical finalized L1.
--
-- `finalized_verified` is set ONLY by the indexer's FINALIZED-pinned scan (a
-- scan bounded by the L1 finalized/safe block, whose logs are by definition the
-- canonical finalized chain's content). The `finalized`/`safe` strict gate
-- authorizes only rows with this flag, so a latest-observed-then-reorged fork
-- row (never covered by the finalized scan) can no longer authorize. Normal
-- (lenient) decomposition is unaffected — it reads the row regardless.
ALTER TABLE ger_entries
    ADD COLUMN IF NOT EXISTS finalized_verified BOOLEAN NOT NULL DEFAULT FALSE;

-- Progress cursor of the finalized-pinned scan, separate from `last_processed`
-- (the head/latest scan cursor) so a restart resumes the finalized scan without
-- re-walking from genesis and without stranding pre-finalized rows.
ALTER TABLE l1_indexer_state
    ADD COLUMN IF NOT EXISTS finalized_scan_cursor BIGINT NOT NULL DEFAULT 0;
