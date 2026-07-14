-- ============================================================================
-- Cantina #4 — permanent claim → expected-MINT-serial history
-- ============================================================================
--
-- The bridge MASM derives the MINT note's serial number from the CLAIM's
-- PROOF_DATA_KEY (`bridge_in_output.masm::build_mint_recipient`:
-- "Generate a serial number for the MINT note (use PROOF_DATA_KEY)"), and
-- PROOF_DATA_KEY is `poseidon2::hash_elements` over the CLAIM note's first
-- 536 storage felts (`claim.masm::write_claim_data_into_advice_map_by_key`).
-- The proxy can therefore recompute, from every consumed CLAIM note it
-- observes, the EXACT serial number of the one legitimate MINT that claim
-- produced.
--
-- This table is that record: one row per legitimate expected-MINT serial,
-- written by `BridgeOutScanner::scan_consumed_notes_monitors` whenever it
-- observes a consumed CLAIM attributable to our deployment. The Cantina #4
-- forged-MINT monitor reconciles every observed consumed MINT against it —
-- a MINT whose serial has no row corresponds to NO recorded claim and is
-- the forged-via-NoAuth signature.
--
-- Unlike `monitor_expected_mints` (Cantina #7 staleness tracking — rows are
-- DELETED when the mint lands), this history is PERMANENT: a forged MINT can
-- be consumed at any time, so the reconciliation set must never shrink. Row
-- volume is bounded by claim volume (one row per L1 deposit claimed).
--
-- Idempotent (`IF NOT EXISTS`), reversible by DROP TABLE (re-derivable from
-- the chain: the scanner recomputes serials from consumed CLAIM notes on
-- every sync tick, so a dropped/rebuilt table self-heals).
CREATE TABLE IF NOT EXISTS monitor_claim_mint_serials (
    serial        BYTEA PRIMARY KEY,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
