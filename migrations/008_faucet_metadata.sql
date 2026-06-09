-- Cantina MA#13 — persist the raw ERC-20 metadata preimage in the faucet
-- registry so bridge-out reconstruction can emit the EXACT metadata bytes the
-- on-chain `MetadataHash` was built from (keccak256(metadata)). Emitting empty
-- metadata for an ERC-20 whose hash came from non-empty metadata diverges the
-- synthetic BridgeEvent leaf/root from the certified Miden bridge state and
-- reverts the first claim of that token on a fresh destination chain.
--
-- Native ETH has MetadataHash = keccak256("") so its preimage is the empty
-- byte string; existing/new ETH rows carry an empty BYTEA.

ALTER TABLE faucet_registry
    ADD COLUMN IF NOT EXISTS metadata BYTEA NOT NULL DEFAULT '\x'::bytea;

-- ── Backfill note for existing rows ─────────────────────────────────────────
-- Rows created before this migration default to empty metadata (the column
-- DEFAULT above). That is correct for native ETH but WRONG for any ERC-20 whose
-- on-chain MetadataHash came from non-empty `abi.encode(name, symbol,
-- decimals)`: the bridge-out reconstruction would still emit empty metadata and
-- diverge the exit tree. Before synthesising any further ERC-20 bridge-outs,
-- operators MUST rebuild `metadata` for every non-ETH row from authoritative
-- faucet state — either by:
--   (a) re-running `admin_registerFaucet` for each ERC-20 (idempotent upsert;
--       it now recomputes and persists abi.encode(name, symbol, decimals)), or
--   (b) replaying the original claimAsset metadata preimage captured at
--       auto-create time.
-- The empty default is intentionally NOT a silent "good enough" — it is a
-- visible TODO marker (metadata = '\x' on a non-ETH faucet) that the backfill
-- has not yet run. A follow-up operational task tracks the per-deployment
-- backfill; no in-place data migration is attempted here because the
-- authoritative preimage is not derivable from the columns already stored
-- (symbol+decimals alone cannot reconstruct the token `name`).
