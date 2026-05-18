-- ============================================================================
-- L1InfoTreeIndexer cursor persistence
-- ============================================================================
--
-- Until this migration the indexer reset its cursor to current L1 head on
-- every restart (`src/l1_info_tree_indexer.rs:120`). Any GER emitted on L1
-- during a proxy outage (OOMKill, planned restart, etc.) was permanently
-- stranded: the indexer never observed the corresponding `UpdateL1InfoTree`
-- event, so `ger_entries.set_ger_exit_roots` was never called for those
-- combined hashes, and bridge-service's `zkevm_getExitRootsByGER` returned
-- NULL for the affected GERs forever.
--
-- This table is the simplest possible persistence: a single key/value row
-- tracking the last successfully-processed L1 block. The indexer resumes
-- from `max(last_processed - reorg_margin, 0)` on startup.

CREATE TABLE l1_indexer_state (
    id              INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_processed  BIGINT NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO l1_indexer_state (id) VALUES (1);
