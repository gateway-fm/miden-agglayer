-- ============================================================================
-- L1 finality-tag block tracking (audit H6 BLOCKER 3)
-- ============================================================================
--
-- Strict-H6 authorization of an IRREVERSIBLE GER injection can be qualified
-- either by a confirmation depth below the head cursor OR by an L1 finality
-- tag (`finalized` / `safe`). Under `--require-hardening` the `finalized` tag
-- is MANDATORY. The gate (`ger::ensure_ger_l1_observed`) authorizes a resolved
-- evidence row only when `evidence_block <= finalized_block`.
--
-- This block is tracked SEPARATELY from `last_processed` (the head cursor that
-- drives normal, undelayed decomposition): the indexer records the decomposition
-- up to LATEST for ordinary bridge readiness, but persists the finality-tag
-- block here so strict authorization can wait for finality without delaying
-- normal ops. A stale value only ever DELAYS authorization (fail-closed).
ALTER TABLE l1_indexer_state
    ADD COLUMN IF NOT EXISTS finalized_block BIGINT NOT NULL DEFAULT 0;
