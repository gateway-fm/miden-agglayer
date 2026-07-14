-- Migration 016 / Cantina #7: LET deposit-index reservations. Every Emit-class B2AGG
-- leaf reserves its authoritative LET index in bridge_out_processed at projection time —
-- including quarantined / metadata-deferred / self-targeted classes, which occupy an
-- on-chain LET leaf but deliberately emit no BridgeEvent. `emitted = FALSE` marks such a
-- reservation; the atomic commit reuses the reserved index and flips it TRUE, so a later
-- recovery (e.g. metadata backfill) emits with the SAME index across restarts/retries.
-- Legacy rows were all written by the emit path: default TRUE.
ALTER TABLE bridge_out_processed
    ADD COLUMN IF NOT EXISTS emitted BOOLEAN NOT NULL DEFAULT TRUE;

-- Cantina #7 (part 2): persisted LET cardinality gate state — the baseline must never be
-- re-absorbed on restart (that would convert a standing unsafe gap into accepted
-- history), and a halt must survive a restart.
ALTER TABLE service_state
    ADD COLUMN IF NOT EXISTS let_gate_baseline BIGINT,
    ADD COLUMN IF NOT EXISTS let_gate_halted BOOLEAN NOT NULL DEFAULT FALSE;
