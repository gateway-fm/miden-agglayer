-- Singleton row for global counters
CREATE TABLE service_state (
    id                  INT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    latest_block_number BIGINT NOT NULL DEFAULT 0,
    log_counter         BIGINT NOT NULL DEFAULT 0,
    hash_chain_value    BYTEA NOT NULL DEFAULT '\x0000000000000000000000000000000000000000000000000000000000000000',
    deposit_counter     INT NOT NULL DEFAULT 0,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO service_state (id) VALUES (1);

-- Synthetic EVM logs (replaces LogStore)
CREATE TABLE synthetic_logs (
    id                BIGSERIAL PRIMARY KEY,
    log_index         BIGINT NOT NULL,
    address           TEXT NOT NULL,
    topics            TEXT[] NOT NULL,
    data              TEXT NOT NULL,
    block_number      BIGINT NOT NULL,
    block_hash        BYTEA NOT NULL,
    transaction_hash  TEXT NOT NULL,
    transaction_index BIGINT NOT NULL DEFAULT 0,
    removed           BOOLEAN NOT NULL DEFAULT FALSE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_logs_block_range_address ON synthetic_logs (block_number, lower(address));
CREATE INDEX idx_logs_tx_hash ON synthetic_logs (lower(transaction_hash));

-- GER entries (merges LogStore.seen_gers + GerTracker)
CREATE TABLE ger_entries (
    ger_hash          BYTEA PRIMARY KEY,
    mainnet_exit_root BYTEA,
    rollup_exit_root  BYTEA,
    block_number      BIGINT NOT NULL,
    timestamp         BIGINT NOT NULL,
    is_injected       BOOLEAN NOT NULL DEFAULT FALSE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Transaction receipts (replaces TxnManager LRU)
CREATE TABLE transactions (
    tx_hash         TEXT PRIMARY KEY,
    miden_tx_id     TEXT,
    envelope_bytes  BYTEA NOT NULL,
    signer          TEXT NOT NULL,
    expires_at      BIGINT,
    status          TEXT NOT NULL DEFAULT 'pending',
    error_message   TEXT,
    block_number    BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_txns_status ON transactions (status);
CREATE INDEX idx_txns_miden_id ON transactions (miden_tx_id) WHERE miden_tx_id IS NOT NULL;

-- Transaction log data (LogData attached at begin time)
CREATE TABLE transaction_logs (
    id        BIGSERIAL PRIMARY KEY,
    tx_hash   TEXT NOT NULL REFERENCES transactions(tx_hash) ON DELETE CASCADE,
    topics    BYTEA[] NOT NULL,
    data      BYTEA NOT NULL
);
CREATE INDEX idx_txn_logs_tx_hash ON transaction_logs (tx_hash);

-- Nonce tracking
CREATE TABLE nonces (
    address    TEXT PRIMARY KEY,
    nonce      BIGINT NOT NULL DEFAULT 0
);

-- Claimed indices (replaces claimed_indices.json)
CREATE TABLE claimed_indices (
    global_index TEXT PRIMARY KEY,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Address mappings (replaces address_mappings.json)
CREATE TABLE address_mappings (
    eth_address   TEXT PRIMARY KEY,
    miden_account TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Bridge-out processed notes (replaces bridge_out_tracker.json)
CREATE TABLE bridge_out_processed (
    note_id       TEXT PRIMARY KEY,
    deposit_count INT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Note: block_transactions table was removed — it was never referenced in code.
