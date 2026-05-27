-- ============================================================================
-- RD-913: persist Cantina #5 / #6 / #7 monitor trackers
-- ============================================================================
--
-- Until this migration the three monitor trackers were pure in-memory data
-- structures (HashSet / HashMap behind RwLock):
--   * src/burn_serial_tracker.rs     — Cantina #5 BURN serial dedup
--   * src/twin_note_detector.rs      — Cantina #6 NoteId→commitment cross-index
--   * src/expected_mint_tracker.rs   — Cantina #7 expected-MINT staleness
--
-- A process restart cleared every observation. After restart a colliding
-- second BURN with a previously-seen serial looked fresh and slipped past
-- Cantina #5 detection (and equivalently for Cantina #6). The trackers also
-- grew without bound: there was no eviction policy, no cap, no TTL.
--
-- This migration adds the on-disk source of truth. The Rust trackers retain
-- a bounded LRU cache on top — DB is authoritative, cache exists only to
-- keep the hot path off the wire. Cache evictions are safe because
-- subsequent observations re-check the DB.
--
-- All three tables are idempotent (`IF NOT EXISTS`) and reversible by
-- straight DROP TABLE (no data the rest of the schema depends on; the
-- trackers are observe-only side state). Long-term retention is documented
-- in docs/REDEPLOY_RUNBOOK_BALI.md — we don't implement automated TTL
-- because the row volume is bounded by L1 deposit volume and queries are
-- keyed on the primary index.

-- Cantina #5 — every observed BURN note serial, ever.
-- A row's presence on second observation is the collision signature.
CREATE TABLE IF NOT EXISTS monitor_burn_serials (
    serial      BYTEA PRIMARY KEY,
    -- Audit columns (no functional role; useful for forensic review when
    -- a duplicate fires and on-call needs to see WHEN the first sighting
    -- happened). Keep narrow — there's no point indexing first_seen_at,
    -- the PRIMARY KEY scan on serial is the only access pattern.
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Cantina #6 — every (NoteId, commitment) pair we've observed.
-- The primary key spans both columns so different commitments under the
-- same NoteId coexist as distinct rows. The twin-detector predicate is
-- "EXISTS row with this NoteId AND a commitment != $observed" → twin.
CREATE TABLE IF NOT EXISTS monitor_twin_notes (
    note_id     BYTEA NOT NULL,
    commitment  BYTEA NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (note_id, commitment)
);
-- Secondary index for the per-NoteId scan (cheaper than full PK scan).
CREATE INDEX IF NOT EXISTS idx_monitor_twin_notes_note_id
    ON monitor_twin_notes (note_id);

-- Cantina #7 — expected-MINT entries for in-flight claims.
-- Rows are deleted when the MINT lands OR after the one-shot StaleAlert
-- fires (Bug B fix). `alerted` exists for crash recovery: if we crash
-- between firing the alert metric and deleting the row, the next tick
-- sees alerted=true and skips re-firing.
CREATE TABLE IF NOT EXISTS monitor_expected_mints (
    global_index    BYTEA PRIMARY KEY,
    expected_mint   BYTEA NOT NULL,
    ticks_pending   INT NOT NULL DEFAULT 0,
    alerted         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
