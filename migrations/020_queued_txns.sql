-- #146 — Per-signer future-nonce queue ("mempool").
--
-- `eth_sendRawTransaction` accepts a valid transaction whose nonce is GREATER
-- than the signer's next expected nonce (a bursty / out-of-order submission, the
-- way a real Ethereum node tolerates a temporary gap) by PARKING it here and
-- returning its hash immediately, instead of blocking up to 30s and then
-- rejecting with "nonce mismatch". When the gap fills — a tx at exactly the next
-- expected nonce is admitted — the accept path DRAINS the contiguous run of
-- queued txns for that signer in nonce order (promoting each into the writer
-- queue), until the next nonce is missing again.
--
-- Keyed by (signer, nonce): at most one tx may be parked per signer per nonce.
-- Two different txs at the same (signer, nonce): the first wins, the second is
-- rejected — never a silent overwrite ("at most one same-nonce tx wins", the
-- same invariant the executable nonce-CAS enforces). `tx_hash` lets
-- `eth_getTransactionByHash` surface a parked tx as geth's "accepted, not yet
-- mined" pending shape so aggkit treats it as accepted rather than dropped.
-- `envelope` is the raw EIP-2718-encoded signed transaction, replayed verbatim
-- when the gap fills. `expires_at` is a BLOCK NUMBER (same denomination as
-- `transactions.expires_at`); a never-filled gap is dropped by the same expiry
-- sweep that expires pending receipts.
CREATE TABLE IF NOT EXISTS queued_txns (
    signer      TEXT NOT NULL,
    nonce       BIGINT NOT NULL,
    tx_hash     TEXT NOT NULL,
    envelope    BYTEA NOT NULL,
    expires_at  BIGINT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (signer, nonce)
);

-- eth_getTransactionByHash lookup of a parked tx.
CREATE INDEX IF NOT EXISTS idx_queued_txns_tx_hash ON queued_txns (tx_hash);

-- Expiry sweep: drop rows whose expires_at block has passed.
CREATE INDEX IF NOT EXISTS idx_queued_txns_expires_at ON queued_txns (expires_at);
