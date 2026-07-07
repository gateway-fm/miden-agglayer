//! Store — Unified data persistence layer.
//!
//! The `Store` trait abstracts all persistent and ephemeral state. Two
//! implementations:
//! - `InMemoryStore` — HashMap/RwLock, used as default and in tests
//! - `PgStore` — PostgreSQL-backed, selected via `--database-url`

pub mod memory;
#[cfg(feature = "postgres")]
pub mod migrator;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(all(test, feature = "postgres"))]
mod postgres_tests;

use crate::block_state::BlockState;
use crate::log_synthesis::{GerEntry, LogFilter, SyntheticLog};
use crate::miden_client::{MidenClientLib, SyncListener};
use alloy::consensus::TxEnvelope;
use alloy::primitives::{Address, LogData, TxHash, U256};
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::transaction::TransactionId;
use std::sync::Arc;

// ── eth_getLogs safety ceiling (Cantina #12) ─────────────────────────

/// OOM backstop on the number of **matching** logs a single `eth_getLogs`
/// query may return. This is NOT a normal-operation cap — under any realistic
/// query the store returns the COMPLETE matching set.
///
/// Cantina finding #12 (original): `get_logs` issued `... LIMIT 1000` and
/// returned the truncated slice with no signal. Worse, the `LIMIT` was applied
/// to the UNFILTERED block-range set — address/topic matching happened in Rust
/// AFTER the fetch — so a range holding 5000 logs of which only 3 matched the
/// queried address would error/truncate even though the true answer was 3 rows.
/// A restore replaying a dense window handed a well-behaved consumer
/// (aggkit/aggsender) a *successful* response missing the tail, so it ingested
/// `0..999`, silently skipped `1000`, and later rejected `1001` as out of
/// sequence — permanently stalling withdrawal / GER sync.
///
/// Redesign (this change): the address/topic0 filter is pushed into a SAFE
/// SUPERSET SQL `WHERE` (see `PgStore::get_logs`), the superset is read in FULL
/// (streaming cursor on pg, whole-range scan in memory), and the UNCHANGED
/// `LogFilter::matches` runs as the exact final filter. There is no
/// normal-operation row cap: a sparse match in a dense range returns exactly
/// the matches. The only remaining limit is this generous ceiling on the
/// **post-`matches()`** count — a genuine "this query matched too much"
/// signal (Geth/Alchemy/Infura convention) that guards against unbounded
/// memory and, being shaped as [`getlogs_row_cap_error`], lets aggkit re-chunk.
pub const GETLOGS_SAFETY_CEILING: usize = 500_000;

/// Build the canonical over-ceiling error for a `[from, to]` block range whose
/// **matching** `synthetic_logs` count exceeds [`GETLOGS_SAFETY_CEILING`].
///
/// The message is deliberately shaped as `block range too large, max range: N`
/// so aggkit/aggsender's `ParseMaxRangeFromError`
/// (`aggkit/common/errors.go` regex `block range too large, max range:\s*(\d+)`)
/// extracts `N` and re-chunks the request — the SAME reactive-chunking path the
/// block-span cap (`MAX_GETLOGS_BLOCK_RANGE`, PRST-4030/4055) already relies on.
/// Both the bridge reader (`bridgesync/agglayer_bridge_l2_reader.go`) and the
/// GER reader (`l2gersync/l2_evm_ger_reader.go`) grep the error string only —
/// they do not inspect the JSON-RPC error code — so routing this through
/// `store_error` (InternalError) is fine as long as the substring survives
/// scrubbing, which it does (no path/URL/ALL_CAPS token in the message).
///
/// `N` is HALF the queried span (min 1) so the hint is strictly smaller than the
/// current window and the client provably narrows on each retry. aggkit's
/// `ChunkedRangeQuery` recurses per chunk, so a still-too-dense sub-window shrinks
/// again — convergence is guaranteed because PR #94's 1:1 Miden→block projection
/// puts each event in its own block, so no single block approaches the cap.
/// (The one unreachable degenerate case — a single block matching >500k logs —
/// cannot be narrowed by block range; it is out of reach post-#94 and noted here
/// honestly.)
pub fn getlogs_row_cap_error(from: u64, to: u64) -> anyhow::Error {
    let span = to.saturating_sub(from).saturating_add(1);
    let suggested = (span / 2).max(1);
    anyhow::anyhow!(
        "eth_getLogs block range too large, max range: {suggested} — range [{from}, {to}] \
         matched more than {GETLOGS_SAFETY_CEILING} logs; retry with a smaller block range"
    )
}

// ── Types ────────────────────────────────────────────────────────────

/// Faucet registry entry — metadata for a bridged token's Miden faucet.
#[derive(Debug, Clone)]
pub struct FaucetEntry {
    pub faucet_id: AccountId,
    /// EVM token contract address on origin chain (zero = native ETH).
    pub origin_address: [u8; 20],
    /// Origin chain network ID (0 = Ethereum mainnet).
    pub origin_network: u32,
    /// Token symbol, e.g. "ETH", "USDC".
    pub symbol: String,
    /// Token decimals on the origin chain (e.g. 18 for ETH).
    pub origin_decimals: u8,
    /// Token decimals on Miden (typically 8).
    pub miden_decimals: u8,
    /// Decimal scaling factor: `origin_decimals - miden_decimals`.
    pub scale: u8,
    /// Raw ABI-encoded token metadata preimage — `abi.encode(name, symbol,
    /// decimals)` for ERC-20s, empty for native ETH. This is the exact byte
    /// string whose keccak256 is the faucet's on-Miden `MetadataHash`, and the
    /// `metadata` a bridge-out's synthetic `BridgeEvent` must carry so the
    /// downstream exit leaf matches Miden's bridge state and a fresh-destination
    /// `_deployWrappedToken(abi.decode(...))` succeeds (Cantina #13). Empty for
    /// legacy rows written before this field existed.
    pub metadata: Vec<u8>,
}

/// Data for registering a new transaction.
pub struct TxnEntry {
    pub id: Option<TransactionId>,
    pub envelope: TxEnvelope,
    pub signer: Address,
    pub expires_at: Option<u64>,
    pub logs: Vec<LogData>,
}

/// Record of a claim we dropped because the destination could not be resolved to a
/// Miden AccountId. See RD-860: storing these lets operators inspect the backlog and
/// audit what happened to a user's funds when support asks about a specific deposit.
#[derive(Debug, Clone)]
pub struct UnclaimableClaim {
    pub global_index: U256,
    pub destination_address: Address,
    pub origin_network: u32,
    pub origin_address: Address,
    pub amount: U256,
    pub reason: UnclaimableReason,
    pub eth_tx_hash: TxHash,
}

/// Why a claim was dropped. Currently only one variant; kept as an enum so we can
/// extend it without touching the schema (the textual `reason` column carries the
/// variant name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnclaimableReason {
    /// `address_mapper::resolve_address` returned an error — the destination is neither
    /// hardhat, store-registered, nor a zero-padded MidenAccountId.
    UnresolvableDestination,
}

impl UnclaimableReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnresolvableDestination => "unresolvable_destination",
        }
    }
}

/// Record of a B2AGG bridge-out that aggkit observed consumed by the bridge
/// but could NOT translate into a synthetic `BridgeEvent` (Cantina MA#18).
///
/// The on-chain consumption already advanced the LET frontier — funds are
/// effectively burned on L2 — but aggkit failed to parse or process the
/// note. Without this quarantine row, the failure was only surfaced as a
/// symptom by the LET-divergence monitor (Cantina #9); operators had no
/// concrete handle for an individual stranded B2AGG.
///
/// `note_id` is the primary key because erased B2AGGs by definition never
/// reached the deposit-counter stage that would assign a `global_index`.
/// `note_dump` captures everything we knew about the note at quarantine
/// time so a future recovery RPC can re-attempt the BridgeEvent
/// synthesis once the underlying cause is fixed (faucet registered, parse
/// bug patched, etc).
#[derive(Debug, Clone)]
pub struct UnbridgeableBridgeOut {
    pub note_id: String,
    pub bridge_account: AccountId,
    pub reason: UnbridgeableBridgeOutReason,
    /// Free-form diagnostic (the exact error message from the skip site).
    /// Bounded by the caller; the column has no length cap in Postgres but
    /// callers should keep it under 4 KiB so a flood of bad notes cannot
    /// fill the table beyond bounded growth.
    pub detail: String,
    /// JSON-ish dump of the note for later forensic inspection. Today we
    /// capture script root + storage felts + asset metadata — enough for an
    /// operator to identify the depositor and decide on a recovery path.
    pub note_dump: String,
    /// The aggkit synthetic block number at which the consumption was
    /// observed. Useful for cross-referencing with the LET-divergence
    /// monitor that fires in the same on_post_sync tick.
    pub observed_block: u64,
}

/// Why an observed-consumed B2AGG could not be translated into a
/// synthetic BridgeEvent. Each variant maps 1:1 to a skip-return path in
/// `project_b2agg_note`.
///
/// Variant set is closed today; future skip paths must add their own
/// variant + map back via `as_str()` so the Postgres column value remains
/// machine-parseable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbridgeableBridgeOutReason {
    /// `parse_b2agg_storage` errored — the storage section was missing,
    /// truncated, or contained limb values that overflowed u32. "Erased
    /// note" in the Cantina MA#18 sense.
    StorageParseFailed,
    /// The B2AGG carried no fungible asset — the bridge consumed an empty
    /// note. Pre-MA#18 this skipped silently; now quarantined so we have a
    /// row to investigate.
    NoFungibleAsset,
    /// The B2AGG's faucet is not in aggkit's registry (B8). Already had a
    /// metric (`bridge_out_unknown_faucet_total`) and a mark-processed
    /// step, but no quarantine row — this adds one so the operator has a
    /// concrete handle.
    UnknownFaucet,
    /// `reverse_scale_amount` overflowed u128 — the on-chain amount × 10^scale
    /// can't fit. Practically impossible for legitimate ERC-20 amounts but
    /// kept as a distinct skip path so a malicious B2AGG that triggers it
    /// is auditable.
    AmountOverflow,
    /// The atomic store commit failed mid-write (transaction rolled back,
    /// nothing persisted). Quarantine so a retry path or operator can
    /// re-attempt without missing the leaf.
    AtomicCommitFailed,
    /// The faucet's stored metadata exceeds `MAX_BRIDGE_EVENT_METADATA_BYTES`
    /// (Cantina #13 DoS guard). Encoding the synthetic BridgeEvent would drive
    /// an oversized allocation, so we refuse to emit. Quarantined (rather than
    /// silently skipped) so the note is recorded as a permanent skip and is not
    /// re-attempted every sync tick / restore run.
    MetadataTooLarge,
}

impl UnbridgeableBridgeOutReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StorageParseFailed => "storage_parse_failed",
            Self::NoFungibleAsset => "no_fungible_asset",
            Self::UnknownFaucet => "unknown_faucet",
            Self::AmountOverflow => "amount_overflow",
            Self::AtomicCommitFailed => "atomic_commit_failed",
            Self::MetadataTooLarge => "metadata_too_large",
        }
    }
}

/// Full transaction data returned from the store.
#[derive(Debug, Clone)]
pub struct TxnData {
    pub id: Option<TransactionId>,
    pub envelope: TxEnvelope,
    pub signer: Address,
    pub expires_at: Option<u64>,
    pub result: Option<Result<(), String>>,
    pub block_num: u64,
    pub logs: Vec<LogData>,
}

impl TxnData {
    /// Build an `alloy::rpc::types::Transaction` for JSON-RPC responses.
    pub fn to_rpc_transaction(
        &self,
        _tx_hash: TxHash,
        block_state: &BlockState,
    ) -> alloy::rpc::types::Transaction {
        use alloy::consensus::transaction::Recovered;
        use alloy::primitives::B256;

        let is_confirmed = self.result.is_some();
        alloy::rpc::types::Transaction {
            inner: Recovered::new_unchecked(self.envelope.clone(), self.signer),
            block_hash: if is_confirmed {
                Some(B256::from(block_state.get_block_hash(self.block_num)))
            } else {
                None
            },
            block_number: if is_confirmed {
                Some(self.block_num)
            } else {
                None
            },
            transaction_index: if is_confirmed { Some(0) } else { None },
            effective_gas_price: Some(0),
        }
    }
}

// ── Store Trait ───────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait Store: Send + Sync + 'static {
    // === Block number ===
    async fn get_latest_block_number(&self) -> anyhow::Result<u64>;
    async fn set_latest_block_number(&self, n: u64) -> anyhow::Result<()>;
    /// Increment block number by 1 and return the new value.
    async fn advance_block_number(&self) -> anyhow::Result<u64>;

    // === L1 indexer cursor (RD-862 follow-up) ===
    /// Last successfully-polled L1 block. Returns 0 if the indexer has
    /// never persisted a cursor on this deployment.
    async fn get_l1_indexer_cursor(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
    /// Persist the last-processed L1 block. Called after each successful
    /// batch so a restart resumes from here instead of jumping to L1 head.
    async fn set_l1_indexer_cursor(&self, _block: u64) -> anyhow::Result<()> {
        Ok(())
    }

    // === Synthetic projector cursor (synthetic-indexer redesign, Phase 2a) ===
    /// Last fully-projected Miden block height owned by the `SyntheticProjector`
    /// (`docs/SYNTHETIC-INDEXER-REDESIGN.md`). Returns 0 if the projector has
    /// never persisted a cursor on this deployment (fresh chain). The projector
    /// is the single in-process owner of this cursor (SINGLE-PROCESS ONLY).
    async fn get_projector_cursor(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
    /// Persist the last fully-projected Miden block height. Called by the
    /// projector after each block so a restart resumes catch-up from here
    /// instead of re-scanning the whole chain.
    async fn set_projector_cursor(&self, _block: u64) -> anyhow::Result<()> {
        Ok(())
    }

    // === Note-reconciler sweep cursor (restart must not re-sweep from genesis) ===
    /// Last Miden block fully swept by the note-visibility reconciler
    /// (`SyntheticProjector::reconcile_notes`). Returns 0 if the reconciler has
    /// never persisted a cursor on this deployment — the very first boot then
    /// sweeps from genesis, which is the designed first-boot heal. Before this
    /// cursor was persisted it was memory-only, so EVERY container restart
    /// re-walked the sweep from genesis (~3h of resync + node load on prod
    /// history per restart).
    async fn get_reconcile_cursor(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
    /// Persist the last reconciler-swept Miden block. Written write-behind
    /// AFTER a sweep window completes, so the durable cursor never runs ahead
    /// of work actually done (a crash mid-window redoes that window — safe,
    /// the sweep is idempotent). Recovery flows (`--restore`,
    /// `--reset-miden-store`) and the `--resweep-from-genesis` escape hatch
    /// reset this to 0 so the full-history heal sweep runs again.
    async fn set_reconcile_cursor(&self, _block: u64) -> anyhow::Result<()> {
        Ok(())
    }

    // === Receipts map (synthetic-indexer redesign, Phase 2b substrate) ===
    //
    // See the "Receipts — the submit ⟂ project handoff" section of
    // `docs/SYNTHETIC-INDEXER-REDESIGN.md`. The map is the ONLY state the two
    // workers share, and it is a *first-write associative map, not a shared
    // counter*, so it carries none of Finding #5's race. UNUSED in Phase 2a —
    // it is the substrate the Phase-2b receipts lifecycle is built on.
    /// First-write-wins association `evm_tx_hash -> note_commitment`. Worker 1
    /// (submit) records this when it submits a CLAIM/GER note to Miden; worker 2
    /// (the projector) looks it up when it observes the note consumed, to
    /// complete the *right* receipt. A second write for an already-linked
    /// `tx_hash` is a no-op (first-write-wins): the on-chain note is the real
    /// handoff and this map only answers "which receipt does this note belong
    /// to". UNUSED in Phase 2a.
    async fn record_tx_note_link(
        &self,
        _tx_hash: &str,
        _note_commitment: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Forward lookup: the note commitment first-associated with `tx_hash`, or
    /// `None`. See [`Store::record_tx_note_link`]. UNUSED in Phase 2a.
    async fn get_note_link_for_tx(&self, _tx_hash: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// Reverse lookup: the `evm_tx_hash` first-associated with `note_commitment`,
    /// or `None`. The projector uses this direction when it holds the consumed
    /// note and needs the caller's receipt key. See
    /// [`Store::record_tx_note_link`]. UNUSED in Phase 2a.
    async fn get_tx_for_note(&self, _note_commitment: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    // === Synthetic logs ===
    async fn add_log(&self, log: SyntheticLog) -> anyhow::Result<()>;
    async fn get_logs(
        &self,
        filter: &LogFilter,
        current_block: u64,
    ) -> anyhow::Result<Vec<SyntheticLog>>;
    async fn get_logs_for_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<SyntheticLog>>;

    // === GER state ===
    async fn has_seen_ger(&self, ger: &[u8; 32]) -> anyhow::Result<bool>;
    /// Mark GER as seen. Returns true if newly inserted.
    async fn mark_ger_seen(&self, ger: &[u8; 32], entry: GerEntry) -> anyhow::Result<bool>;
    async fn get_latest_ger(&self) -> anyhow::Result<Option<[u8; 32]>>;
    async fn get_ger_entry(&self, ger: &[u8; 32]) -> anyhow::Result<Option<GerEntry>>;
    /// Set the `(mainnet, rollup)` decomposition and L1 origin metadata for a
    /// GER. Called by `L1InfoTreeIndexer` after observing the source
    /// `UpdateL1InfoTree` / `UpdateGlobalExitRoot` event on L1, so the
    /// `l1_block_number` / `l1_timestamp` here are the L1 block where the
    /// event was emitted (the authoritative source for `zkevm_getExitRootsByGER`).
    /// UPSERTs: on conflict, all four columns are overwritten — the indexer's
    /// L1-sourced view supersedes any earlier hardcoded-0 row that may have
    /// been left by an L2-side write path that didn't know the L1 origin yet.
    async fn set_ger_exit_roots(
        &self,
        ger: &[u8; 32],
        mainnet_exit_root: [u8; 32],
        rollup_exit_root: [u8; 32],
        l1_block_number: u64,
        l1_timestamp: u64,
    ) -> anyhow::Result<()>;
    async fn is_ger_injected(&self, ger: &[u8; 32]) -> anyhow::Result<bool>;
    async fn mark_ger_injected(&self, ger: [u8; 32]) -> anyhow::Result<()>;
    /// Atomically: mark GER seen, update hash chain, emit UpdateHashChainValue log.
    #[allow(clippy::too_many_arguments)]
    async fn add_ger_update_event(
        &self,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_exit_root: &[u8; 32],
        mainnet_exit_root: Option<[u8; 32]>,
        rollup_exit_root: Option<[u8; 32]>,
        timestamp: u64,
    ) -> anyhow::Result<()>;

    // === Transactions ===
    async fn txn_begin(&self, tx_hash: TxHash, entry: TxnEntry) -> anyhow::Result<()>;
    async fn txn_commit(
        &self,
        tx_hash: TxHash,
        result: Result<(), String>,
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()>;
    async fn txn_receipt(
        &self,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<(Result<(), String>, u64)>>;
    async fn txn_get(&self, tx_hash: TxHash) -> anyhow::Result<Option<TxnData>>;
    async fn txn_pending_by_miden_id(&self, id: TransactionId) -> anyhow::Result<Option<TxHash>>;
    async fn txn_commit_pending(
        &self,
        ids: &[TransactionId],
        block_num: u64,
        block_hash: [u8; 32],
    ) -> anyhow::Result<()>;
    async fn txn_expire_pending(&self, block_num: u64, block_hash: [u8; 32]) -> anyhow::Result<()>;

    // === Nonces ===
    async fn nonce_get(&self, addr: &str) -> anyhow::Result<u64>;
    /// Increment nonce, returning the value **before** increment.
    async fn nonce_increment(&self, addr: &str) -> anyhow::Result<u64>;

    // === Claims ===
    async fn try_claim(&self, global_index: U256) -> anyhow::Result<()>;
    async fn unclaim(&self, global_index: &U256) -> anyhow::Result<()>;
    async fn is_claimed(&self, global_index: &U256) -> anyhow::Result<bool>;

    /// Record a claim we refused to process because its destination could not be
    /// resolved. Idempotent by `global_index` — duplicate retries from aggkit must not
    /// error or duplicate rows; the first record wins. Returns `true` if this was a new
    /// insert (not a duplicate).
    ///
    /// See [RD-860] and `src/service_send_raw_txn::handle_claim_asset` for the
    /// short-circuit path that calls this.
    async fn record_unclaimable_claim(&self, entry: UnclaimableClaim) -> anyhow::Result<bool>;

    /// Look up an unclaimable record by `global_index`. `None` if not dropped.
    async fn get_unclaimable_claim(
        &self,
        global_index: &U256,
    ) -> anyhow::Result<Option<UnclaimableClaim>>;

    // === Address mappings ===
    async fn get_address_mapping(&self, eth: &Address) -> anyhow::Result<Option<AccountId>>;
    async fn set_address_mapping(&self, eth: Address, miden: AccountId) -> anyhow::Result<()>;

    // === Monitor trackers (RD-913) — persistent source-of-truth for
    //     burn-serial / twin-note / expected-mint observations so the
    //     trackers survive process restart.
    //     The in-memory tracker structs layer a bounded LRU cache on top;
    //     these methods are the cache miss / write-through path.

    /// Has this BURN serial been observed before? (Cantina #5)
    async fn burn_serial_seen(&self, _serial: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(false)
    }
    /// Record an observed BURN serial. Returns `true` if newly inserted
    /// (caller treats this as `New`); `false` if it already existed
    /// (caller treats this as `Duplicate` and fires the Cantina #5 alert).
    async fn burn_serial_observe(&self, _serial: &[u8; 32]) -> anyhow::Result<bool> {
        Ok(true)
    }

    /// Look up every prior commitment seen for this NoteId. Empty vec if
    /// the NoteId is novel. (Cantina #6)
    async fn twin_note_commitments(&self, _note_id: &[u8; 32]) -> anyhow::Result<Vec<[u8; 32]>> {
        Ok(Vec::new())
    }
    /// Insert a (note_id, commitment) pair. Returns `true` on a new
    /// insertion, `false` if the pair already existed.
    async fn twin_note_observe(
        &self,
        _note_id: &[u8; 32],
        _commitment: &[u8; 32],
    ) -> anyhow::Result<bool> {
        Ok(true)
    }

    /// Persist an expected-MINT entry for a submitted claim. (Cantina #7)
    /// Upserts on global_index — re-submission of the same claim resets
    /// the staleness window, which matches the in-memory contract.
    async fn expected_mint_record(
        &self,
        _global_index: &[u8; 32],
        _expected_mint: &[u8; 32],
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Delete the entry for `global_index` (called on Landed / mark_landed).
    async fn expected_mint_remove(&self, _global_index: &[u8; 32]) -> anyhow::Result<()> {
        Ok(())
    }
    /// Load all live entries for the staleness tick. Each row carries
    /// `(global_index, expected_mint, ticks_pending, alerted)`.
    async fn expected_mint_load_all(&self) -> anyhow::Result<Vec<([u8; 32], [u8; 32], u32, bool)>> {
        Ok(Vec::new())
    }
    /// Persist updated tick / alerted flags after a tick. Default impl
    /// no-ops so InMemoryStore-only callers (tests) don't have to wire
    /// it through — those callers reconstruct the tracker from the
    /// in-memory cache directly.
    async fn expected_mint_update_tick(
        &self,
        _global_index: &[u8; 32],
        _ticks_pending: u32,
        _alerted: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    // === Bridge-out ===
    async fn is_note_processed(&self, note_id: &str) -> anyhow::Result<bool>;
    /// Mark note as processed, return the deposit count assigned to it.
    async fn mark_note_processed(&self, note_id: String) -> anyhow::Result<u32>;
    /// Roll back a processed-note marker when later persistence fails.
    async fn unmark_note_processed(&self, note_id: &str) -> anyhow::Result<()>;
    /// Read the current deposit_counter (number of B2AGG-out notes aggkit has
    /// processed since genesis). Used by the Cantina #9 LET-divergence monitor
    /// to compare against the bridge account's `let_num_leaves` storage slot.
    async fn get_deposit_count(&self) -> anyhow::Result<u64>;

    /// Record a B2AGG bridge-out that was observed consumed by the bridge but
    /// could NOT be translated into a synthetic BridgeEvent (Cantina MA#18).
    ///
    /// Idempotent by `note_id` — multiple sync ticks observing the same
    /// erased note must not duplicate rows; the first record wins. Returns
    /// `true` if this was a new insert (not a duplicate).
    ///
    /// Default impl is a no-op so InMemoryStore in tests that don't care
    /// about quarantine state still compiles; the real impls (memory + pg)
    /// override below.
    async fn record_unbridgeable_bridge_out(
        &self,
        _entry: UnbridgeableBridgeOut,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }

    /// Look up an unbridgeable B2AGG by `note_id`. `None` if not quarantined.
    ///
    /// Default impl returns `None` so stores without the quarantine table
    /// (e.g. legacy deployments before migration 006) don't crash readers.
    async fn get_unbridgeable_bridge_out(
        &self,
        _note_id: &str,
    ) -> anyhow::Result<Option<UnbridgeableBridgeOut>> {
        Ok(None)
    }

    // === Claim watcher ===
    //
    // Tracks consumed CLAIM notes the `claim_watcher` SyncListener has already
    // turned into a synthetic ClaimEvent. Separate from the B2AGG idempotency
    // tracker (`*_note_processed` above) so CLAIM observations cannot bump the
    // `deposit_counter` sequence that aggsender reads for bridge-outs.
    //
    // The watcher itself lives in `src/claim_watcher.rs`; the use case is
    // crash-recovery (proxy submitted a CLAIM but died before writing the log)
    // and foreign CLAIMs (operator-issued via another miden-client).

    /// Has the watcher already processed this CLAIM note?
    async fn is_claim_note_processed(&self, note_id: &str) -> anyhow::Result<bool>;
    /// Mark the CLAIM note as processed, recording the global_index it carried
    /// and the synthetic block number the ClaimEvent landed at.
    async fn mark_claim_note_processed(
        &self,
        note_id: String,
        global_index: [u8; 32],
        block_number: u64,
    ) -> anyhow::Result<()>;
    /// Has a ClaimEvent already been written for this L1 leaf (`global_index`)?
    /// Both the normal `eth_sendRawTransaction` path and the watcher path
    /// write ClaimEvent logs; this guards against double-emission when the
    /// watcher observes a CLAIM whose corresponding ClaimEvent was already
    /// recorded via the normal path.
    async fn has_claim_event_for_global_index(
        &self,
        global_index: &[u8; 32],
    ) -> anyhow::Result<bool>;

    /// Atomic commit for a watcher-synthesised ClaimEvent. Combines:
    ///   1. `mark_claim_note_processed`
    ///   2. `add_claim_event` (synthetic log emission)
    ///   3. `set_latest_block_number` (cursor advance)
    ///
    /// PgStore overrides with a single SERIALIZABLE postgres txn; the default
    /// impl below chains the three primitives sequentially, which is fine for
    /// `InMemoryStore` where every primitive is an in-process lock. The
    /// race-safe ordering (log THEN cursor) is the same invariant
    /// `the projector B2AGG commit` and `the projector GER commit` enforce — see
    /// the canonical comment at `src/bridge_out.rs::on_post_sync`.
    #[allow(clippy::too_many_arguments)]
    async fn commit_manual_claim_event_atomic(
        &self,
        note_id: String,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_index: [u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) -> anyhow::Result<()> {
        self.mark_claim_note_processed(note_id, global_index, block_number)
            .await?;
        self.add_claim_event(
            bridge_address,
            block_number,
            block_hash,
            tx_hash,
            &global_index,
            origin_network,
            origin_address,
            destination_address,
            amount,
        )
        .await?;
        self.set_latest_block_number(block_number).await?;
        Ok(())
    }

    // === Faucet registry ===
    /// Register or update a faucet entry (upsert by faucet_id).
    async fn register_faucet(&self, entry: FaucetEntry) -> anyhow::Result<()>;
    /// Look up a faucet by its L1 origin token address and network.
    async fn get_faucet_by_origin(
        &self,
        origin_address: &[u8; 20],
        origin_network: u32,
    ) -> anyhow::Result<Option<FaucetEntry>>;
    /// Find every faucet registered under a given origin token address, regardless of network.
    /// Used to detect cross-network token-address collisions before auto-creating a faucet
    /// (the on-chain registry keys by token address only, so a same-address-different-network
    /// auto-create would silently overwrite the existing on-chain registration — Cantina #1).
    async fn find_faucets_by_origin_address(
        &self,
        origin_address: &[u8; 20],
    ) -> anyhow::Result<Vec<FaucetEntry>>;
    /// Look up a faucet by its Miden account ID.
    async fn get_faucet_by_id(&self, faucet_id: AccountId) -> anyhow::Result<Option<FaucetEntry>>;
    /// List all registered faucets.
    async fn list_faucets(&self) -> anyhow::Result<Vec<FaucetEntry>>;

    // === Convenience: claim event log ===
    #[allow(clippy::too_many_arguments)]
    async fn add_claim_event(
        &self,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_index: &[u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) -> anyhow::Result<()> {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()],
            data: crate::log_synthesis::encode_claim_event_data_u64(
                global_index,
                origin_network,
                origin_address,
                destination_address,
                amount,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log).await
    }

    // === Convenience: bridge event log ===
    #[allow(clippy::too_many_arguments)]
    async fn add_bridge_event(
        &self,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        leaf_type: u8,
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_network: u32,
        destination_address: &[u8; 20],
        amount: u128,
        metadata: &[u8],
        deposit_count: u32,
    ) -> anyhow::Result<()> {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![crate::log_synthesis::BRIDGE_EVENT_TOPIC.to_string()],
            data: crate::bridge_out::encode_bridge_event_data(
                leaf_type,
                origin_network,
                origin_address,
                destination_network,
                destination_address,
                amount,
                metadata,
                deposit_count,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log).await
    }
}

// ── StoreSyncListener ────────────────────────────────────────────────

/// Adapts the Store to the MidenClient sync loop.
///
/// Buffers sync data in `on_sync` (sync), processes in `on_post_sync` (async).
/// Replaces the old TxnManager + BlockNumTracker sync listeners.
pub struct StoreSyncListener {
    pub store: Arc<dyn Store>,
    pub block_state: Arc<BlockState>,
    pending: std::sync::Mutex<Option<SyncData>>,
}

struct SyncData {
    block_num: u64,
    committed_ids: Vec<TransactionId>,
}

impl StoreSyncListener {
    pub fn new(store: Arc<dyn Store>, block_state: Arc<BlockState>) -> Self {
        Self {
            store,
            block_state,
            pending: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl SyncListener for StoreSyncListener {
    fn on_sync(&self, summary: &SyncSummary) {
        let data = SyncData {
            block_num: summary.block_num.as_u64(),
            committed_ids: summary.committed_transactions.clone(),
        };
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(data);
    }

    async fn on_post_sync(&self, _client: &mut MidenClientLib) -> anyhow::Result<()> {
        let data = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(data) = data {
            let block_hash = self.block_state.get_block_hash(data.block_num);
            // The SyntheticProjector is the SOLE advancer of `latest_block_number`
            // (Miden-1:1, Finding #5 eliminated by construction); this listener
            // only finalises pending tx receipts.
            self.store
                .txn_commit_pending(&data.committed_ids, data.block_num, block_hash)
                .await?;
            self.store
                .txn_expire_pending(data.block_num, block_hash)
                .await?;
        }
        Ok(())
    }
}
