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
-- `transactions.expires_at`), but note what it does and does NOT do: it marks a
-- parked tx as STALE for reporting. NOTHING ever deletes that row.
--
-- A parked tx has already been ACKNOWLEDGED to its sender, which will therefore
-- never resubmit it. Dropping one silently orphans it — exactly the permanent
-- claim-stream wedge (#119) this queue exists to prevent — and a TTL cannot
-- distinguish "the gap will never fill" from "the gap is slow". So the sweep
-- SURFACES these rows (gauge `rpc_future_nonce_stale_parked` plus a warning) and
-- leaves them in place for an operator to act on. Memory is bounded by the
-- per-signer and global caps, which reject at SUBMISSION time — an immediate,
-- visible error to a caller that still holds the transaction — rather than by
-- discarding work already accepted.
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
