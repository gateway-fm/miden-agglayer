-- ============================================================================
-- L1 finality-tag block tracking (audit H6 BLOCKER 3)
-- ============================================================================
--
-- Strict-H6 authorization of an IRREVERSIBLE GER injection can be qualified
-- either by a confirmation depth below the head cursor OR by an L1 finality
-- tag (`finalized` / `safe`). Under `--require-hardening` the `finalized` tag
-- is MANDATORY. The indexer uses this block as the upper bound of a canonical
-- finality-tag scan; the gate authorizes only rows marked by that scan.
--
-- This block is tracked SEPARATELY from `last_processed` (the head cursor that
-- drives normal, undelayed decomposition): the indexer records the decomposition
-- up to LATEST for ordinary bridge readiness, but persists the finality-tag
-- block here so the canonical scan can advance independently without delaying
-- normal ops. A stale value only ever DELAYS authorization (fail-closed).
ALTER TABLE l1_indexer_state
    ADD COLUMN IF NOT EXISTS finalized_block BIGINT NOT NULL DEFAULT 0;
