-- Fenced claim ownership. Existing claimed rows predate the handoff protocol
-- and may already have reached Miden, so upgrade them fail-closed as submitted.
ALTER TABLE claimed_indices
    ADD COLUMN IF NOT EXISTS owner_tx_hash TEXT,
    ADD COLUMN IF NOT EXISTS fence_token BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS claim_state TEXT NOT NULL DEFAULT 'submitted',
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;

-- A note link is first durable as `prepared`, immediately before the external
-- submit. It becomes `submitted` only after commit or exact-note observation.
-- Legacy links are known historical handoffs and therefore upgrade fail-closed.
-- The durable note reconciler confirms exact NoteIds before advancing its
-- cursor. A prepared link may be cleared only once that cursor is strictly past
-- the executed Miden transaction's last possible inclusion block.
ALTER TABLE tx_note_links
    ADD COLUMN IF NOT EXISTS handoff_state TEXT NOT NULL DEFAULT 'submitted',
    ADD COLUMN IF NOT EXISTS note_id TEXT,
    ADD COLUMN IF NOT EXISTS prepared_expiration_block BIGINT;

CREATE INDEX IF NOT EXISTS idx_tx_note_links_prepared_note_id
    ON tx_note_links (note_id) WHERE handoff_state = 'prepared';
