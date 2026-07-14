-- #55 BLOCKER 1 — atomic (signer, nonce) reservation.
--
-- Before any queue/dispatch side effect, a submission reserves its (signer, nonce)
-- slot mapped to its tx_hash. The reservation is an insert-if-absent keyed on
-- (signer, nonce): the FIRST tx to reserve a slot WINS and executes; a later
-- DIFFERENT tx at the same slot LOSES and is rejected (never executes); the SAME
-- tx re-reserving is idempotent. This makes nonce acceptance correct across
-- rolling replicas sharing one PostgreSQL — the process-local per-signer lock only
-- serialises within one replica.
CREATE TABLE IF NOT EXISTS nonce_reservations (
    signer     TEXT   NOT NULL,
    nonce      BIGINT NOT NULL,
    tx_hash    TEXT   NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (signer, nonce)
);
