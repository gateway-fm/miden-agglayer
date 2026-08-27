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
-- `transactions.expires_at`): a row whose block has passed is EVICTED by the
-- expiry sweep, exactly like geth dropping a stale queued tx from its mempool.
--
-- Design decision (maintainer, 2026-08-27): this queue is EPHEMERAL.
-- Persistence is best-effort crash convenience, NOT a delivery guarantee. What
-- makes eviction safe is the re-broadcast contract: after eviction the tx hash
-- stops resolving via eth_getTransactionByHash, which is the standard EVM
-- signal for "dropped" — the sender's monitoring (claimtxman) re-broadcasts,
-- and the resubmission is judged against the CURRENT nonce state (parks again,
-- executes, or hits a nonce error it already self-heals from via the #111
-- wording contract). Without eviction, a gap that never fills pinned its
-- per-signer/global capacity forever, and enough dead rows could lock every
-- unrelated future-nonce submission out of the queue.
CREATE TABLE IF NOT EXISTS queued_txns (
    signer      TEXT NOT NULL,
    nonce       BIGINT NOT NULL,
    tx_hash     TEXT NOT NULL,
    envelope    BYTEA NOT NULL,
    expires_at  BIGINT NOT NULL,
    -- Proof that this row was parked while the post-rebuild recovery window
    -- (#90) was OPEN. Eligibility to adopt a baseline must be proof-carrying:
    -- inferring it from "signer has parked rows and reads nonce 0" is wrong,
    -- because an ABSENT ledger row also reads 0, so a brand-new wallet
    -- submitting nonce 42 would be seeded to 42 and skip 0..41. This marker
    -- distinguishes a continuing wallet's tx from a new wallet's out-of-order
    -- one, and is durable because its job is to outlive the window and survive
    -- a restart.
    parked_during_recovery BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (signer, nonce)
);

-- eth_getTransactionByHash lookup of a parked tx.
CREATE INDEX IF NOT EXISTS idx_queued_txns_tx_hash ON queued_txns (tx_hash);

-- Expiry sweep: drop rows whose expires_at block has passed.
CREATE INDEX IF NOT EXISTS idx_queued_txns_expires_at ON queued_txns (expires_at);
