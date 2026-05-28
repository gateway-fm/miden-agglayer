-- Cantina MA#18: log of L2→L1 B2AGG bridge-outs that aggkit observed consumed
-- by the bridge account but could NOT translate into a synthetic BridgeEvent.
--
-- The on-chain consumption already advanced the LET frontier (the funds are
-- effectively burned on L2), but aggkit failed to parse the note or resolve
-- its faucet, so no BridgeEvent log was written. From aggsender's
-- perspective the bridge-out never happened — without operator intervention,
-- the user's funds are stranded.
--
-- Rows here are the positive quarantine handle that lets an operator (and a
-- future recovery endpoint) reconstruct the missing leaf: the note_id is the
-- on-chain B2AGG that was consumed, and the JSON `note_dump` captures
-- everything we knew about it at quarantine time so a rescue path can
-- re-derive the BridgeEvent fields if and when the underlying parse bug or
-- registry gap is fixed.
--
-- Mirror of `unclaimable_claims` (migration 003), with note_id as the
-- primary key instead of global_index because erased B2AGGs by definition
-- never reached the deposit-counter stage that would assign a global_index.
--
-- Recovery design (NOT IMPLEMENTED YET — see PR body for full sketch):
--   1. Operator inspects this table to identify stranded B2AGGs.
--   2. If the underlying cause is fixable (e.g. faucet now registered, parse
--      bug patched), operator calls a recovery RPC that re-runs
--      process_consumed_note against the original note_dump and, on
--      success, emits the synthetic BridgeEvent + deletes the row.
--   3. If unfixable (truly erased contents, faucet permanently unknown), the
--      operator escalates via the L1 fork-choice path (drop the cert, fork
--      the LET, or accept the leak).
CREATE TABLE IF NOT EXISTS unbridgeable_bridge_outs (
    note_id         TEXT PRIMARY KEY,
    bridge_account  TEXT NOT NULL,
    reason          TEXT NOT NULL,
    detail          TEXT NOT NULL,
    note_dump       TEXT NOT NULL,
    observed_block  BIGINT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_unbridgeable_bridge_outs_reason
    ON unbridgeable_bridge_outs (reason);

CREATE INDEX IF NOT EXISTS idx_unbridgeable_bridge_outs_bridge_account
    ON unbridgeable_bridge_outs (bridge_account);
