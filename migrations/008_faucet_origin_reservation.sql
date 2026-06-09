-- Cantina MA#10 — faucet origin reservation.
--
-- The faucet registry must be reserved by its L1 origin key
-- (origin_address, origin_network) BEFORE any live faucet is deployed and
-- registered in the bridge. Two concurrent (possibly cross-process) first
-- claims for the same unseen token must NOT both deploy + register a live
-- faucet route and then race their local writes — that left the live bridge
-- route pointing at faucet B while the local row was pinned to faucet A,
-- making B's later withdrawals unresolvable (orphaned generation).
--
-- To support reserving the origin BEFORE the Miden faucet account id is
-- known, faucet_id stops being the table PRIMARY KEY and becomes nullable:
-- a NULL faucet_id is a *reservation* placeholder, filled in once the live
-- deploy completes. The authoritative uniqueness / conflict key is now the
-- origin key, matching register_faucet's ON CONFLICT target.

-- Drop the old faucet_id primary key (was the only conflict target before).
ALTER TABLE faucet_registry
    DROP CONSTRAINT IF EXISTS faucet_registry_pkey;

-- faucet_id is NULL while a row is merely a reservation.
ALTER TABLE faucet_registry
    ALTER COLUMN faucet_id DROP NOT NULL;

-- Keep faucet_id unique among *committed* (non-reservation) rows so two
-- distinct origins can never collapse onto the same Miden faucet account,
-- while allowing many NULL reservation placeholders to coexist transiently.
CREATE UNIQUE INDEX IF NOT EXISTS idx_faucet_id_not_null
    ON faucet_registry (faucet_id)
    WHERE faucet_id IS NOT NULL;

-- idx_faucet_origin (origin_address, origin_network) from migration 002 is the
-- authoritative reservation / conflict key and already exists as UNIQUE.
