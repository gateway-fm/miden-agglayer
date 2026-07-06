-- ============================================================================
-- Synthetic-indexer redesign — projector cursor + receipts map (Phase 2a)
-- ============================================================================
--
-- See docs/SYNTHETIC-INDEXER-REDESIGN.md. This migration adds the durable
-- substrate for the `SyntheticProjector`, which is the always-on, sole producer
-- of synthetic events and the sole advancer of `latest_block_number`. The
-- running service reads + writes these at runtime: the projector persists its
-- cursor here every tick, and the claim path records `tx_note_links` so the
-- projected ClaimEvent rides the real `claimAsset` tx hash.
--
-- 1. Projector cursor — the "last fully-projected Miden block height". The
--    projector is the single in-process owner of this cursor (SINGLE-PROCESS
--    ONLY). It lives as a column on the existing single-row `service_state`
--    table, mirroring `latest_block_number` / `log_counter`. Defaults to 0 on
--    a fresh chain; the existing row is backfilled by the column DEFAULT.
ALTER TABLE service_state
    ADD COLUMN IF NOT EXISTS projector_cursor BIGINT NOT NULL DEFAULT 0;

-- 2. Receipts map — the submit ⟂ project handoff (see the "Receipts" section of
--    the design doc). A first-write-wins association `evm_tx_hash ->
--    note_commitment`: worker 1 (submit) records it when it submits a CLAIM/GER
--    note to Miden; worker 2 (the projector) looks it up when it observes the
--    note consumed, to complete the *right* receipt. It is a first-write
--    associative map, NOT a shared counter — it carries none of Finding #5's
--    race. The Miden chain remains the real handoff; this map only answers
--    "which receipt does this note belong to". UNUSED in Phase 2a.
--
--    `tx_hash` is the PRIMARY KEY so first-write-wins is enforced by
--    `ON CONFLICT (tx_hash) DO NOTHING`. The reverse lookup (note -> tx) is
--    served by a secondary index.
CREATE TABLE IF NOT EXISTS tx_note_links (
    tx_hash         TEXT PRIMARY KEY,
    note_commitment TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_tx_note_links_note_commitment
    ON tx_note_links (note_commitment);
