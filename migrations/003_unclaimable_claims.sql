-- RD-860: log of L1→L2 claims we refused to process because the destination could
-- not be resolved to a Miden AccountId.
--
-- Claims land here when `address_mapper::resolve_address` fails (not hardhat, no
-- explicit store mapping, not a zero-padded MidenAccountId). We short-circuit them
-- in `service_send_raw_txn::handle_claim_asset` by emitting a synthetic `ClaimEvent`
-- so aggkit marks the globalIndex complete and stops retrying — WITHOUT actually
-- minting any L2 funds. This row is the only record that the claim was effectively
-- dropped; a future operator rescue endpoint (tier 2) can query this table.
--
-- `global_index` is the authoritative key from the L1 bridge. eth_tx_hash is the
-- aggkit-submitted eth tx that carried the claim. Both are indexed for fast lookup
-- when a user shows up asking "where did my deposit go?".

-- Column conventions here mirror the existing `claimed_indices` table
-- (migrations/001_initial.sql): U256 values stored as lowercase `0x`-prefixed hex
-- TEXT, addresses / tx hashes as `0x`-prefixed TEXT. That lets us join and cross-
-- reference the two tables without conversion.
CREATE TABLE IF NOT EXISTS unclaimable_claims (
    global_index        TEXT PRIMARY KEY,
    destination_address TEXT NOT NULL,
    origin_network      INT NOT NULL,
    origin_address      TEXT NOT NULL,
    amount              TEXT NOT NULL,
    reason              TEXT NOT NULL,
    eth_tx_hash         TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_unclaimable_claims_eth_tx_hash
    ON unclaimable_claims (eth_tx_hash);

CREATE INDEX IF NOT EXISTS idx_unclaimable_claims_destination
    ON unclaimable_claims (destination_address);
