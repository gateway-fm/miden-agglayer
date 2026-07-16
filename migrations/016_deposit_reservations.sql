-- Reserve LET indices even when a B2AGG leaf emits no event. Legacy rows were emitted.
ALTER TABLE bridge_out_processed
    ADD COLUMN IF NOT EXISTS emitted BOOLEAN NOT NULL DEFAULT TRUE;

-- Explicit, operator-audited offset for pre-migration LET leaves not represented by
-- deposit_counter. Runtime inference could absorb the very missing leaf the gate detects.
ALTER TABLE service_state
    ADD COLUMN IF NOT EXISTS let_gate_baseline BIGINT;
UPDATE service_state SET let_gate_baseline = 0 WHERE let_gate_baseline IS NULL;
ALTER TABLE service_state
    ALTER COLUMN let_gate_baseline SET DEFAULT 0,
    ALTER COLUMN let_gate_baseline SET NOT NULL;
ALTER TABLE service_state
    ADD CONSTRAINT service_state_let_gate_baseline_nonnegative
    CHECK (let_gate_baseline >= 0);

-- miden-client 0.15 discards input note headers from sync_transactions. Keep the
-- minimal join needed to recover a public B2AGG body by NoteId after a restart.
CREATE TABLE IF NOT EXISTS bridge_b2agg_note_ids (
    nullifier TEXT PRIMARY KEY,
    note_id   TEXT NOT NULL
);
