-- Cantina #5 — track the raw Miden sync height SEPARATELY from the synthetic
-- EVM tip (service_state.latest_block_number).
--
-- Pre-fix the sync loop (StoreSyncListener::on_post_sync) wrote the raw Miden
-- block number directly into latest_block_number, conflating the two counters
-- and risking the synthetic tip being rolled backwards below a block a
-- store-owned allocator had already published a synthetic log into. The
-- synthetic tip is now owned by the atomic commit helpers (which allocate the
-- next synthetic block inside their transaction); the raw Miden height lives
-- here and is only ever observed, never used to clobber the synthetic tip.
ALTER TABLE service_state
    ADD COLUMN IF NOT EXISTS raw_miden_height BIGINT NOT NULL DEFAULT 0;
