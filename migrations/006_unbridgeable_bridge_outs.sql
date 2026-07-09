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
-- Recovery design (IMPLEMENTED — src/bridge_out_recovery.rs):
--   1. Operator inspects this table to identify stranded B2AGGs.
--   2. If the underlying cause is fixable (e.g. faucet now registered), the
--      recovery path re-derives the BridgeEvent fields from the captured
--      note_dump (storage felts -> destination; assets -> faucet + amount;
--      resolve faucet) and re-emits the synthetic BridgeEvent via the SAME
--      two store primitives a normal bridge-out takes (mark_note_processed +
--      add_bridge_event), then deletes the row. deposit_count advances, so the
--      Cantina #9 LET divergence clears and the funds become claimable. Two
--      triggers exist: automatically, when the LET-divergence monitor detects
--      on_chain_leaves > deposit_count (bridge_out.rs::run_let_divergence_check
--      runs recover_all_unbridgeable_bridge_outs), and on demand via the
--      admin RPC `admin_recoverUnbridgeableBridgeOuts`.
--   3. If unfixable proxy-side, the row is LEFT in place as the durable
--      operator handle. This covers the genuinely-erased case: the on-chain
--      bridge account exposes only the LET frontier/root/count (never leaf
--      preimages), so a note whose storage was erased (StorageParseFailed,
--      single-felt placeholder) cannot have its destination reconstructed from
--      chain state alone. Closing that gap needs the note preimage (from the
--      depositor/sequencer, off-chain) or a protocol-level fix that stops
--      erasing bridge-out nullifiers (miden-node remove_erased_nullifiers /
--      the b2agg.masm consensus path — out of this repo's scope). The
--      operator can then escalate via the L1 fork-choice path (drop the cert,
--      fork the LET, or accept the leak).
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
