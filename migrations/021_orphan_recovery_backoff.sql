-- #156 — automatic recovery of acknowledged pending/unlinked transactions.
--
-- A transaction can be durably admitted (pending row + nonce advanced, RPC hash
-- returned) while its writer job lives only in memory. A crash, a Miden outage,
-- or a clean shutdown that drops buffered jobs before a durable Miden handoff can
-- leave the row `pending` with no `miden_tx_id` and no submitted note handoff,
-- while the durable pending-nonce frontier correctly blocks every later nonce.
-- The signed envelope in `transactions.envelope_bytes` is the recovery source of
-- truth; the background recovery loop re-drives it through the same-hash
-- durable-intent path.
--
-- These columns make the retry schedule DURABLE so recovery pressure survives a
-- restart: `recovery_attempts` counts orphan re-drive attempts and
-- `next_recovery_at` is the earliest unix time (seconds) the next attempt may run
-- (NULL = eligible immediately). Both are meaningless for terminal rows and are
-- reset when a transaction reaches a durable handoff or terminal receipt.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS recovery_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS next_recovery_at BIGINT;

-- Recovery scans pending rows ordered by signer then nonce; this partial index
-- keeps that scan cheap without weighing on the hot terminal-row path.
CREATE INDEX IF NOT EXISTS idx_txns_pending_recovery
    ON transactions (signer)
    WHERE status = 'pending';
