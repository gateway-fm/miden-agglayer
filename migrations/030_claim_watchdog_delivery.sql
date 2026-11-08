-- Preserve due CLAIM-consumption alerts until their caller acknowledges
-- emission. A sequence gives every resubmission a unique acknowledgement token,
-- even when an older row was deleted. Legacy names remain upgrade-compatible.
ALTER TABLE monitor_expected_mints
    ADD COLUMN generation BIGSERIAL,
    ADD COLUMN alert_pending BOOLEAN NOT NULL DEFAULT FALSE;

-- Old releases normally never wrote alerted=true. Preserve any such pending
-- signal rather than silently dropping it during the upgrade.
UPDATE monitor_expected_mints SET alert_pending = alerted;

-- Prior releases stored the first output NoteId, which could even be TX_FEE.
-- When fenced claim ownership links this exact global index to a submitted
-- CLAIM, its durable note link is the authoritative details identity. Preserve
-- unresolved rows and their tick/delivery state; never guess or delete history.
UPDATE monitor_expected_mints AS watchdog
SET expected_mint = CASE
    WHEN link.note_commitment ~ '^[0-9a-fA-F]{64}$'
    THEN decode(link.note_commitment, 'hex')
    ELSE watchdog.expected_mint
END
FROM claimed_indices AS claim
JOIN tx_note_links AS link ON link.tx_hash = claim.owner_tx_hash
WHERE claim.global_index ~ '^0x[0-9a-fA-F]{1,64}$'
  AND encode(watchdog.global_index, 'hex') = lower(lpad(substr(claim.global_index, 3), 64, '0'));
