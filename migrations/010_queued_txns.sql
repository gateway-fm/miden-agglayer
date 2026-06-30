-- Per-signer future-nonce queue ("mempool").
--
-- `eth_sendRawTransaction` accepts a transaction whose nonce is GREATER than
-- the signer's next expected nonce (an out-of-order / bursty submission, the
-- way a real Ethereum node tolerates a temporary gap) by parking it here
-- instead of rejecting it. When the gap is filled — a tx at exactly the next
-- expected nonce is processed — the accept path DRAINS the contiguous run of
-- queued txns for that signer in nonce order, processing each and advancing
-- the nonce, until the next nonce is missing again.
--
-- Keyed by (signer, nonce): at most one tx may be parked per signer per nonce
-- (two different txs at the same nonce: the first wins, the second is rejected
-- — never a silent overwrite). `tx_hash` lets `eth_getTransactionByHash`
-- surface a parked tx as the geth "accepted, not yet mined" pending shape so
-- aggkit treats it as accepted rather than dropped. `envelope` is the raw
-- 2718-encoded signed transaction, replayed verbatim when the gap fills.
-- `expires_at` is a BLOCK NUMBER (same denomination as `transactions.expires_at`);
-- a never-filled gap is dropped by the same expiry sweep that expires pending
-- receipts (`txn_expire_pending`).
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
