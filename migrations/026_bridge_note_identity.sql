-- Issue #167 (item 3): generalize the durable nullifier -> NoteId identity ledger.
--
-- Migration 016 introduced `bridge_b2agg_note_ids`, populated ONLY for B2AGG
-- (bridge-out) notes. The canonical projector now resolves EVERY bridge-consumed
-- input — B2AGG, CLAIM, UpdateGerNote and setup notes — from the bridge's
-- authoritative transaction feed by exact NoteId, so CLAIM/GER history must
-- survive a client-store loss the same way B2AGG already does. The ledger is
-- therefore keyed for every PUBLIC note the reconciler discovers in the bridge's
-- tag space, regardless of kind.
ALTER TABLE IF EXISTS bridge_b2agg_note_ids RENAME TO bridge_note_ids;
CREATE TABLE IF NOT EXISTS bridge_note_ids (
    nullifier TEXT PRIMARY KEY,
    note_id   TEXT NOT NULL
);
-- Existing rows cover B2AGG only. Walk history once more so CLAIM/GER identities
-- are recorded before the full-tip visibility barrier permits further sealing
-- (same mechanism as 017).
UPDATE service_state SET reconcile_cursor = 0 WHERE id = 1;
