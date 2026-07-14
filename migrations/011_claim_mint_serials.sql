-- ============================================================================
-- Cantina #4 — permanent claim → expected-MINT IDENTITY history
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
-- SECURITY (second re-review, blocker #1): a serial-membership test alone is
-- bypassable. With NoAuth bridge authorship a forger can copy a public
-- legitimate serial while changing the actual MINT (recipient / asset /
-- amount / destination). So this row stores the FULL derivable expected-MINT
-- IDENTITY, not just the serial, and the forged-MINT monitor requires the
-- observed MINT's identity to match — a MINT reusing a stored serial with
-- different details still alerts. The identity fields are decoded from the
-- CLAIM note's on-chain `ClaimNoteStorage` (see `claim_watcher::
-- parse_claim_event_from_storage` + the `miden_claim_amount` tail felt):
--
--   * minted_amount       — the exact Miden-scaled amount the MINT carries
--                           (CLAIM storage[568]); binds the MINT's asset amount.
--   * destination_address — the EVM claimant the MINT pays (LeafData); binds
--                           the MINT recipient, recorded for forensic audit.
--   * origin_network /
--     origin_address      — the L1 token; resolves (via the faucet registry)
--                           to the wrapped faucet the MINT must mint, binding
--                           the MINT's asset faucet.
--
-- NON-NATIVE ONLY: a native-faucet claim (LeafData.origin_network == this
-- deployment's network id) executes the P2ID unlock path and produces NO
-- MINT — it writes NO row here (its serial must never become a permanent
-- MINT whitelist entry).
--
-- Unlike `monitor_expected_mints` (Cantina #7 staleness tracking — rows are
-- DELETED when the mint lands), this history is PERMANENT: a forged MINT can
-- be consumed at any time, so the reconciliation set must never shrink. Row
-- volume is bounded by claim volume (one row per non-native L1 deposit
-- claimed). First-write-wins (`ON CONFLICT DO NOTHING`) — PROOF_DATA_KEY is
-- unique per deposit so distinct claims never collide.
--
-- Idempotent (`IF NOT EXISTS`), reversible by DROP TABLE (re-derivable from
-- the chain: the scanner recomputes identities from consumed CLAIM notes on
-- every sync tick, so a dropped/rebuilt table self-heals).
CREATE TABLE IF NOT EXISTS monitor_claim_mint_serials (
    serial              BYTEA PRIMARY KEY,
    minted_amount       BYTEA NOT NULL,
    destination_address BYTEA NOT NULL,
    origin_network      BIGINT NOT NULL,
    origin_address      BYTEA NOT NULL,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
