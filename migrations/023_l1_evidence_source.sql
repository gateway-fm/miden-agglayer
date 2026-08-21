-- Round-2 review finding #6 — audit-H6 corroboration was keyed by confirmation
-- policy, but NOT by which L1 it came from.
--
-- `ger_entries` rows (and the scan cursor that produced them) are evidence that
-- "this GER was observed on L1". Nothing recorded WHICH L1: not the chain id,
-- not the GER manager address. Point the same database at a different chain, a
-- re-genesised devnet, or a redeployed GER contract, and every historic row is
-- still accepted as corroboration — so under strict H6 a GER can be admitted on
-- the strength of evidence gathered from a completely different source. That is
-- the one thing H6 exists to prevent.
--
-- The fix is an identity the store carries with its evidence. It is written
-- once, on the first boot that has an L1 configured, and compared on every boot
-- afterwards; a mismatch is a startup refusal, not a warning, because the
-- alternative is serving corroboration that means nothing.
--
-- Deliberately a single row (id = 1), mirroring `service_state`: a store serves
-- exactly one rollup against exactly one L1.
CREATE TABLE IF NOT EXISTS l1_evidence_source (
    id              INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    -- eth_chainId of the L1 the evidence was scanned from.
    chain_id        BIGINT      NOT NULL,
    -- GER manager contract, lowercase 0x-prefixed hex (normalised by the
    -- caller so a checksum-case change is not read as a different contract).
    ger_address     TEXT        NOT NULL,
    -- latest | safe | finalized — the frontier the cursor's rows were sealed
    -- at. Already enforced separately, recorded here so one row explains the
    -- whole provenance of the evidence set.
    evidence_tag    TEXT        NOT NULL,
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
