-- Migration 016 introduced the durable B2AGG nullifier-to-NoteId ledger. Existing
-- reconcile cursors may already be past unprojected notes, so walk history once to
-- populate it before the full-tip visibility barrier permits further sealing.
UPDATE service_state SET reconcile_cursor = 0 WHERE id = 1;
