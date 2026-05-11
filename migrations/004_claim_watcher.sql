-- Claim watcher: tracks consumed CLAIM notes the `claim_watcher` SyncListener has
-- processed, so a single observation is never replayed across sync ticks and the
-- watcher cannot allocate a duplicate ClaimEvent for the same on-chain note.
--
-- Separate from `bridge_out_processed` because that table's INSERT path bumps
-- `service_state.deposit_counter`, which is the B2AGG bridge-out leaf counter that
-- aggsender reads. CLAIM notes must not consume slots in that sequence.
--
-- See src/claim_watcher.rs and the plan at .claude/plans/typed-orbiting-kurzweil.md.

CREATE TABLE IF NOT EXISTS claim_watcher_processed (
    note_id       TEXT PRIMARY KEY,
    global_index  BYTEA NOT NULL,
    block_number  BIGINT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Secondary lookup: "have we already synthesised a ClaimEvent for this L1 leaf?"
-- The watcher uses this to skip CLAIMs whose corresponding ClaimEvent was already
-- written via the normal eth_sendRawTransaction path (where the watcher would
-- otherwise double-emit). Not unique — though by L1 construction every L1 leaf
-- maps to one global_index, we leave the option open for the watcher to re-link
-- a previously-orphaned record without a constraint violation.
CREATE INDEX IF NOT EXISTS idx_claim_watcher_global_index
    ON claim_watcher_processed (global_index);
