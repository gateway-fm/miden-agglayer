-- #55 BLOCKER 1 — atomic (signer, nonce) admission reservation with a LEASE +
-- FENCING TOKEN (fenced ownership lifecycle).
--
-- A submission must WIN a fenced lease on its (signer, nonce) slot before any
-- queue/dispatch/receipt side effect. Exactly one replica ever executes a given
-- (signer, nonce) at a time:
--   * state 'executing'         — a replica currently owns admission; its lease
--                                 (lease_expires_at) is valid. The SAME tx from
--                                 another replica must NOT execute (dedup); a
--                                 DIFFERENT tx is hard-rejected.
--   * state 'released_success'  — admission completed (a receipt exists / is
--                                 being produced); the SAME tx dedups.
--   * state 'released_failure'  — the prior attempt failed before completing; the
--                                 SAME tx may take over ownership (fence++) and
--                                 retry admission.
-- A lease that has EXPIRED (owner crashed mid-admission) is likewise takeover-able
-- by the SAME tx. Every takeover bumps fence_token; only the current fence owner
-- may release, so a delayed crashed owner cannot clobber the new owner's state.
CREATE TABLE IF NOT EXISTS nonce_reservations (
    signer           TEXT   NOT NULL,
    nonce            BIGINT NOT NULL,
    tx_hash          TEXT   NOT NULL,
    state            TEXT   NOT NULL DEFAULT 'executing',
    lease_expires_at TIMESTAMPTZ NOT NULL,
    fence_token      BIGINT NOT NULL DEFAULT 1,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (signer, nonce)
);
