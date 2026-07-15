-- Bind persisted L1 evidence to the policy that produced it.
--
-- `finalized_verified`, `finalized_block`, and `finalized_scan_cursor` are not
-- interchangeable between `safe` and `finalized`. The service binds this nullable
-- column on its first clean serving boot and refuses a different setting on later
-- boots. NULL is retained by the migration so an upgraded database with untagged
-- markers can be detected and rejected instead of being silently misclassified.
ALTER TABLE l1_indexer_state
    ADD COLUMN IF NOT EXISTS evidence_tag TEXT;


-- Heal rows written by the pre-release TTL implementation. Its synthetic
-- expired state was hidden from receipt reads and was not retryable after a
-- restart. The single writer worker now owns timeout retry decisions.
UPDATE transactions
SET status = 'pending', error_message = NULL, block_number = 0, updated_at = now()
WHERE status = 'expired';
