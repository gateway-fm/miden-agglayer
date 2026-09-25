-- Keep the account identity and exact prepared transactions across failures
-- before the completed faucet_registry row can be written.
CREATE TABLE IF NOT EXISTS faucet_provisioning (
    deployment_key TEXT PRIMARY KEY,
    binding BYTEA NOT NULL,
    initial_account BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Attempts are immutable. A replacement is permitted only after the prior
-- creating transaction expires and fresh chain evidence proves no effect.
CREATE TABLE IF NOT EXISTS faucet_provisioning_steps (
    deployment_key TEXT NOT NULL REFERENCES faucet_provisioning(deployment_key),
    step TEXT NOT NULL CHECK (step IN ('fund', 'deploy', 'register')),
    generation BIGINT NOT NULL CHECK (generation >= 0),
    expiration_block BIGINT NOT NULL CHECK (expiration_block >= 0),
    executed BYTEA NOT NULL,
    proven BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (deployment_key, step, generation)
);
