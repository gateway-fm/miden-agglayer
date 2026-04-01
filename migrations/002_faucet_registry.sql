-- Dynamic faucet registry for bridged tokens.
-- Each entry maps an L1 origin token to a Miden faucet account.

CREATE TABLE IF NOT EXISTS faucet_registry (
    faucet_id       TEXT PRIMARY KEY,
    origin_address  BYTEA NOT NULL,
    origin_network  INT NOT NULL,
    symbol          TEXT NOT NULL,
    origin_decimals SMALLINT NOT NULL,
    miden_decimals  SMALLINT NOT NULL,
    scale           SMALLINT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_faucet_origin
    ON faucet_registry (origin_address, origin_network);
