-- The identity ledger from migration 016 covered B2AGG only. The projector now
-- resolves every bridge-consumed input by exact NoteId (issue #167), so CLAIM
-- and GER identities must be durable too or a client-store loss erases them.
ALTER TABLE IF EXISTS bridge_b2agg_note_ids RENAME TO bridge_note_ids;
CREATE TABLE IF NOT EXISTS bridge_note_ids (
    nullifier TEXT PRIMARY KEY,
    note_id   TEXT NOT NULL
);
-- Existing rows cover B2AGG only; re-sweep once so the rest get recorded
-- before the visibility barrier lets sealing continue (as 017 did).
UPDATE service_state SET reconcile_cursor = 0 WHERE id = 1;
