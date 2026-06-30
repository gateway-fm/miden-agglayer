//! Synthetic projector — the (future) sole owner of the synthetic EVM chain.
//!
//! See `docs/SYNTHETIC-INDEXER-REDESIGN.md`. The projector follows the Miden
//! chain block-by-block on a persisted cursor and, for each Miden block `N`,
//! derives the synthetic events of the notes *consumed at block N*
//! (`nullifier_block_height == N`) in a deterministic order, emitting exactly
//! one synthetic block `N`. A single ordered projector means **no ad-hoc
//! block-number reservation and no race** (Finding #5 eliminated by
//! construction): catch-up (cursor → tip) *is* recovery *is* the normal loop.
//!
//! ## ⚠️ SINGLE-PROCESS ONLY — multiple replicas are NOT supported ⚠️
//!
//! The cursor and the synthetic tip are owned by exactly one in-process
//! projector. Running two projectors (two replicas) against the same store
//! would double-advance the tip and interleave log emission non-
//! deterministically. The deployment MUST guarantee a single projector
//! instance; this is a hard invariant from the design doc, asserted loudly at
//! the cut-over phase.
//!
//! ## The sole synthetic-event producer
//!
//! The projector is ALWAYS registered as a [`SyncListener`] and is the **only**
//! synthetic-event producer and the **only** advancer of `latest_block_number`.
//! The legacy writer paths now only submit to Miden (the user's B2AGG note, the
//! ger_manager UpdateGerNote, the CLAIM note); they emit no synthetic logs and
//! never touch the tip. The projector re-derives every BridgeEvent / ClaimEvent /
//! GER `UpdateHashChainValue` log from the consumed Miden notes.
//!
//! The projector writes into the store exactly the way `restore` does — through
//! the shared `project_b2agg_note` / `project_claim_note` / `project_ger_note`
//! derivations — and is idempotent via the existing `is_*_processed` /
//! `is_ger_injected` dedup keys.
//!
//! ## Determinism + numbering contract (Miden-1:1)
//!
//! Synthetic block N == Miden block N. Every synthetic log derived from notes
//! consumed at Miden block N is written at synthetic block N, and the tip is
//! advanced to N once, **after** the block (write-before-advance) — including for
//! EMPTY Miden blocks, so the synthetic chain mirrors Miden block-for-block and
//! `eth_blockNumber` tracks the Miden tip. Within a Miden block, consumed notes
//! are ordered by `(consumed_tx_order, note_id)` before deriving, so re-running
//! the projector over the same chain yields byte-identical synthetic blocks
//! (numbers, hashes, log order, log indices). Because the projector is the sole
//! assigner of the synthetic tip, there is no `get_latest()+1` reservation race —
//! Finding #5 is eliminated by construction.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::miden_client::{MidenClientLib, SyncListener};
use crate::restore::{
    B2AggRestoreOutcome, ClaimProjectOutcome, GerProjectOutcome, project_b2agg_note,
    project_claim_note, project_ger_note,
};
use crate::store::Store;
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_client::sync::SyncSummary;
use miden_protocol::account::AccountId;
use miden_protocol::note::NoteMetadata;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The synthetic projector. Owns the cursor (last projected Miden block height)
/// and, when registered as the live [`SyncListener`], is the **sole** assigner
/// of the synthetic tip (`Store::latest_block_number`) — so there is no
/// reservation race (Finding #5 eliminated by construction).
pub struct SyntheticProjector {
    store: Arc<dyn Store>,
    block_state: Arc<BlockState>,
    /// Bridge account id — the sole legitimate consumer of a bridge-out B2AGG
    /// note (MA#3) and the expected GER target (MA#28).
    bridge_id: AccountId,
    /// This rollup's AggLayer network id. Threaded into `project_b2agg_note` for
    /// the Cantina #13 self-target poison-leaf gate: a B2AGG bridge-out whose
    /// destination IS this network must NOT emit a synthetic BridgeEvent.
    local_network_id: u32,
    /// Expected GER sender (ger_manager, or service for legacy deployments).
    expected_ger_sender: AccountId,
    /// L1 JSON-RPC endpoint for the Cantina #13 Layer-2 ERC-20 metadata
    /// recovery path (mirrors `BridgeOutScanner::l1_rpc_url`). Threaded into
    /// `project_b2agg_note` so legacy/DB-loss faucet rows with empty ERC-20
    /// metadata recover + validate instead of being skipped. `None` disables
    /// the L1 fallback (recovery then relies solely on the all-Miden candidate).
    l1_rpc_url: Option<String>,
    /// Last projected Miden block height — an in-memory cache of the persisted
    /// `Store::get_projector_cursor`. The projector is the single owner of this
    /// cursor (SINGLE-PROCESS ONLY) and persists every advance in `tick`.
    cursor: AtomicU64,
}

impl SyntheticProjector {
    /// Build a projector from the account configuration. The starting cursor is
    /// **loaded from the store** (`Store::get_projector_cursor`, 0 for a fresh
    /// chain), so a restart resumes catch-up from the last persisted block
    /// rather than re-scanning from genesis. `tick` persists each advance.
    pub async fn new(
        store: Arc<dyn Store>,
        block_state: Arc<BlockState>,
        accounts: &AccountsConfig,
        local_network_id: u32,
        l1_rpc_url: Option<String>,
    ) -> anyhow::Result<Self> {
        // MA#28 — same fallback as `restore_gers` / `submit_update_ger_note`:
        // legacy deployments without a dedicated ger_manager mint GER notes
        // from the service account.
        let expected_ger_sender = accounts
            .ger_manager
            .as_ref()
            .map(|a| a.0)
            .unwrap_or(accounts.service.0);
        let start_cursor = store.get_projector_cursor().await?;
        Ok(Self {
            store,
            block_state,
            bridge_id: accounts.bridge.0,
            local_network_id,
            expected_ger_sender,
            l1_rpc_url,
            cursor: AtomicU64::new(start_cursor),
        })
    }

    /// The current cursor (last projected Miden block height).
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// Project the notes consumed at one Miden block (`miden_block`) into the
    /// single synthetic block `miden_block` (**Miden-1:1**): every synthetic log
    /// derived from this block's notes is written at synthetic block == the Miden
    /// block, and the tip is advanced to `miden_block` once, AFTER the block
    /// (write-before-advance) — even when the block produced no logs, so the
    /// synthetic chain mirrors Miden block-for-block.
    ///
    /// Determinism: within the Miden block, consumed notes are ordered by
    /// `(consumed_tx_order, note_id_hex)` before deriving, so re-running over the
    /// same chain yields byte-identical synthetic blocks. Idempotent: the
    /// `project_*` derivations short-circuit on the existing dedup keys.
    ///
    /// `client` (the live `&mut MidenClientLib`) is threaded through to
    /// `project_b2agg_note` for the Cantina #13 Layer-2 ERC-20 metadata
    /// recovery (`None` in unit tests, where the in-memory feed is supplied
    /// directly).
    /// Filter `consumed` to the notes consumed at `miden_block`, then project
    /// them. Used by `project_block` (live single-block) and the unit tests;
    /// `tick` pre-groups the feed once and calls [`Self::project_block_notes`]
    /// directly to avoid an O(blocks × notes) per-block re-scan during catch-up.
    async fn project_notes(
        &self,
        consumed: &[InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        client: Option<&mut MidenClientLib>,
    ) -> anyhow::Result<usize> {
        let block_notes: Vec<&InputNoteRecord> = consumed
            .iter()
            .filter(|n| n.state().consumed_block_height().map(|h| h.as_u64()) == Some(miden_block))
            .collect();
        self.project_block_notes(&block_notes, output_metadata, miden_block, client)
            .await
    }

    /// Project the already-filtered notes consumed at `miden_block` into the
    /// single synthetic block `miden_block` (Miden-1:1), advancing the tip once
    /// after the block (write-before-advance), even when there are zero notes.
    async fn project_block_notes(
        &self,
        block_notes: &[&InputNoteRecord],
        output_metadata: &HashMap<[u8; 32], NoteMetadata>,
        miden_block: u64,
        mut client: Option<&mut MidenClientLib>,
    ) -> anyhow::Result<usize> {
        let mut notes: Vec<&InputNoteRecord> = block_notes.to_vec();

        // Determinism: order intra-block events by (consumed_tx_order, note-id).
        // `consumed_tx_order` is the per-account position of the consuming
        // transaction within the block; the 32-byte details-commitment is the
        // stable tie-breaker. Compare the commitment bytes directly — identical
        // ordering to the old hex-string compare, but no per-comparison
        // allocation (matters when many notes share a block).
        notes.sort_by(|a, b| {
            a.state()
                .consumed_tx_order()
                .cmp(&b.state().consumed_tx_order())
                .then_with(|| {
                    a.details_commitment()
                        .as_bytes()
                        .cmp(&b.details_commitment().as_bytes())
                })
        });

        let bridge_address = get_bridge_address();

        // Miden-1:1 numbering: synthetic block N == Miden block N. Every synthetic
        // log for this Miden block is written AT block `miden_block`; the tip is
        // advanced exactly ONCE, after the whole block (below). The projector is
        // the SOLE advancer of `latest_block_number` — nothing else may touch it.
        let block_hash = self.block_state.get_block_hash(miden_block);
        let timestamp = self.block_state.get_block_timestamp(miden_block);

        let mut logs = 0usize;
        for note in notes {
            // A consumed note matches at most one of the three script roots, so
            // trying all three derivations emits at most one synthetic log per
            // note — the three restore derivations unified into one per-note loop.
            if project_b2agg_note(
                &self.store,
                note,
                self.bridge_id,
                self.local_network_id,
                miden_block,
                block_hash,
                bridge_address,
                // Cantina #13 recovery context: the live client + the projector's
                // L1 RPC, so legacy/empty-metadata ERC-20 bridge-outs recover.
                client.as_deref_mut(),
                self.l1_rpc_url.as_deref(),
            )
            .await?
                == B2AggRestoreOutcome::Emitted
            {
                logs += 1;
                continue;
            }

            if project_claim_note(&self.store, note, miden_block, block_hash, bridge_address)
                .await?
                == ClaimProjectOutcome::Emitted
            {
                logs += 1;
                continue;
            }

            if project_ger_note(
                &self.store,
                note,
                output_metadata,
                self.expected_ger_sender,
                self.bridge_id,
                miden_block,
                block_hash,
                timestamp,
            )
            .await?
                == GerProjectOutcome::Emitted
            {
                logs += 1;
                continue;
            }
        }

        // Write-before-advance: every synthetic log for `miden_block` is now in the
        // DB, so it is safe to advance the synthetic tip to == the Miden block.
        // Runs for EMPTY Miden blocks too (advance the tip even with 0 logs), so the
        // synthetic chain mirrors Miden block-for-block (eth_blockNumber == Miden tip).
        self.store.set_latest_block_number(miden_block).await?;

        Ok(logs)
    }

    /// Project one Miden block `miden_block` from the live client: fetch the
    /// consumed-note feed + our own output-note metadata (the MA#28 GER
    /// provenance fallback) through the passed `&mut MidenClientLib`, then run
    /// the deterministic [`Self::project_notes`] core. Returns the number of
    /// synthetic logs written.
    ///
    /// Fetching through the *passed* client (not `MidenClient::with`) is
    /// mandatory: `on_post_sync` already holds the client borrow inside the sync
    /// loop, and re-entering via `with` would deadlock the request queue.
    pub async fn project_block(
        &self,
        client: &mut MidenClientLib,
        miden_block: u64,
    ) -> anyhow::Result<usize> {
        // There is no server-side block-range filter for notes yet (see the
        // restore module TODOs), so pull the full consumed set and filter by
        // nullifier_block_height in `project_notes`.
        let consumed = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;

        // Protocol 0.15: notes consumed by the bridge land as `ConsumedExternal`,
        // which carries NO metadata — so the MA#28 sender check in
        // `project_ger_note` needs the metadata from our own output-note records
        // (we minted those notes; the client store retains them permanently).
        let output_metadata: HashMap<[u8; 32], NoteMetadata> = client
            .get_output_notes(NoteFilter::All)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
            .into_iter()
            .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
            .collect();

        self.project_notes(&consumed, &output_metadata, miden_block, Some(client))
            .await
    }

    /// Process every Miden block from `cursor + 1` to the current Miden tip in
    /// order, projecting each one and advancing the cursor. Returns the new
    /// cursor (== the projected Miden tip).
    ///
    /// This is the normal projector loop; catch-up after a restart is the same
    /// code path (the cursor simply starts further behind the tip).
    pub async fn tick(&self, client: &mut MidenClientLib) -> anyhow::Result<u64> {
        let tip = client
            .get_sync_height()
            .await
            .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
            .as_u64();
        let mut cursor = self.cursor.load(Ordering::Acquire);
        if cursor >= tip {
            return Ok(cursor);
        }
        // Perf-critical: fetch the consumed-note feed + output-note metadata ONCE
        // per tick, NOT once per block. There is no server-side block-range filter
        // for notes, so a per-block fetch makes tick O(blocks × notes); the
        // projector then never catches up to the Miden tip (observed in e2e as the
        // bridge-in deposit never becoming claimable, because its GER injection
        // never gets projected). The feeds are owned, so `project_notes` filters
        // them per block by `nullifier_block_height` without re-fetching.
        let consumed = client
            .get_input_notes(NoteFilter::Consumed)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;
        let output_metadata: HashMap<[u8; 32], NoteMetadata> = client
            .get_output_notes(NoteFilter::All)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
            .into_iter()
            .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
            .collect();
        // Pre-group the consumed feed by Miden block ONCE (not once per block): a
        // per-block re-scan of the full feed makes catch-up O(blocks_behind ×
        // total_consumed). Each block then projects from its precomputed bucket.
        let mut by_block: HashMap<u64, Vec<&InputNoteRecord>> = HashMap::new();
        for note in &consumed {
            if let Some(h) = note.state().consumed_block_height().map(|h| h.as_u64()) {
                by_block.entry(h).or_default().push(note);
            }
        }
        let no_notes: Vec<&InputNoteRecord> = Vec::new();
        while cursor < tip {
            let next = cursor + 1;
            let bucket = by_block.get(&next).unwrap_or(&no_notes);
            self.project_block_notes(bucket, &output_metadata, next, Some(client))
                .await?;
            // Advance the cursor only after the block is fully projected, so a
            // crash mid-block re-projects (idempotently) rather than skipping.
            // Persist BEFORE updating the in-memory cache so the durable cursor
            // never runs ahead of fully-projected state.
            self.store.set_projector_cursor(next).await?;
            self.cursor.store(next, Ordering::Release);
            cursor = next;
        }
        // Observability: the projector follows the MIDEN chain, so its progress is
        // measured against the Miden tip (NOT L1). `projector_cursor == miden_tip`
        // means fully caught up; `synthetic_tip` is the actual synthetic L2 block
        // number the chain is exposing. Logged once per tick that did work.
        let synthetic_tip = self.store.get_latest_block_number().await?;
        tracing::info!(
            miden_tip = tip,
            projector_cursor = cursor,
            synthetic_tip,
            "synthetic projector tick: caught up to Miden tip"
        );
        Ok(cursor)
    }
}

#[async_trait::async_trait]
impl SyncListener for SyntheticProjector {
    fn on_sync(&self, _summary: &SyncSummary) {
        // no-op — projection happens in `on_post_sync`, where we hold the live
        // client needed to fetch consumed notes and run Cantina #13 recovery.
    }

    async fn on_post_sync(&self, client: &mut MidenClientLib) -> anyhow::Result<()> {
        self.tick(client).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts_config::{AccountIdBech32, AccountsConfig};
    use crate::claim_watcher::derive_manual_claim_tx_hash;
    use crate::log_synthesis::{LogFilter, SyntheticLog};
    use crate::store::memory::InMemoryStore;
    use miden_base_agglayer::{
        B2AggNote, ClaimNote, ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex,
        LeafData, MetadataHash, ProofData, SmtNode, UpdateGerNote,
    };
    use miden_client::store::InputNoteState;
    use miden_client::store::input_note_states::ConsumedExternalNoteState;
    use miden_protocol::Felt;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, FungibleAsset};
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{
        NoteAssets, NoteAttachment, NoteAttachments, NoteDetails, NoteMetadata, NoteRecipient,
        NoteStorage, NoteType, PartialNoteMetadata,
    };
    use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint};
    use std::sync::Arc as StdArc;

    // Four mutually-distinct, valid protocol-0.15 account ids reused from the
    // restore/bridge_out test fixtures. `FAUCET` is a real fungible-faucet id so
    // `FungibleAsset::new` accepts it.
    const FAUCET: &str = "0xac0000000000dd110000ee000000fc";
    const BRIDGE: &str = "0xaa0000000000bb110000cc000000dd";
    const GER_MANAGER: &str = "0xfa0000000000bb010000cc000000de";
    const SERVICE: &str = "0xbf0000000000cc010000dc000000ee";

    fn aid(hex: &str) -> AccountId {
        AccountId::from_hex(hex).expect("hex must decode")
    }

    /// A minimal `AccountsConfig` for projector construction — only the
    /// `bridge`, `ger_manager` and `service` ids are read by the projector.
    fn test_accounts() -> AccountsConfig {
        AccountsConfig {
            service: AccountIdBech32(aid(SERVICE)),
            bridge: AccountIdBech32(aid(BRIDGE)),
            faucet_eth: None,
            faucet_agg: None,
            wallet_hardhat: AccountIdBech32(aid(SERVICE)),
            ger_manager: Some(AccountIdBech32(aid(GER_MANAGER))),
        }
    }

    /// Wrap a `NoteDetails` + attachments into a consumed `InputNoteRecord`
    /// attributed to `block` with `tx_order`. This is the projector analogue of
    /// the restore/bridge_out `build_*` helpers, with `nullifier_block_height`
    /// set so the projector can attribute the note to a Miden block.
    fn consumed_note(
        details: NoteDetails,
        attachments: NoteAttachments,
        consumer: Option<AccountId>,
        block: u32,
        tx_order: Option<u32>,
    ) -> InputNoteRecord {
        let state = InputNoteState::ConsumedExternal(ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(block),
            consumer_account: consumer,
            consumed_tx_order: tx_order,
        });
        InputNoteRecord::new(details, attachments, None, state)
    }

    /// Build a bridge-consumed B2AGG note carrying a fungible asset from
    /// `FAUCET`, consumed by the bridge at `block` with `tx_order`.
    fn b2agg_note(block: u32, tx_order: Option<u32>) -> InputNoteRecord {
        // B2AGG storage: 6 felts (network + 5 address limbs); zeros parse fine.
        let storage = NoteStorage::new(vec![Felt::from(0u32); 6]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), B2AggNote::script(), storage);
        let asset: Asset = FungibleAsset::new(aid(FAUCET), 50).unwrap().into();
        let assets = NoteAssets::new(vec![asset]).unwrap();
        let details = NoteDetails::new(assets, recipient);
        consumed_note(
            details,
            NoteAttachments::default(),
            Some(aid(BRIDGE)),
            block,
            tx_order,
        )
    }

    /// Build a consumed CLAIM note with a valid `ClaimNoteStorage`, consumed at
    /// `block` with `tx_order`.
    fn claim_note(block: u32, tx_order: Option<u32>) -> InputNoteRecord {
        let mut gi_bytes = [0u8; 32];
        gi_bytes[23] = 1;
        gi_bytes[31] = 0x42;
        let mut origin_addr = [0u8; 20];
        origin_addr[19] = 0xAB;
        let mut dest_addr = [0u8; 20];
        dest_addr[..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut amount_bytes = [0u8; 32];
        amount_bytes[28..32].copy_from_slice(&1_000_000u32.to_be_bytes());

        let claim_storage = ClaimNoteStorage {
            proof_data: ProofData {
                smt_proof_local_exit_root: [SmtNode::new([0u8; 32]); 32],
                smt_proof_rollup_exit_root: [SmtNode::new([0u8; 32]); 32],
                global_index: GlobalIndex::new(gi_bytes),
                mainnet_exit_root: ExitRoot::new([0u8; 32]),
                rollup_exit_root: ExitRoot::new([0u8; 32]),
            },
            leaf_data: LeafData {
                origin_network: 0x12345678,
                origin_token_address: EthAddress::new(origin_addr),
                destination_network: 0xAABBCCDD,
                destination_address: EthAddress::new(dest_addr),
                amount: EthAmount::new(amount_bytes),
                metadata_hash: MetadataHash::from_abi_encoded(&[]),
            },
            miden_claim_amount: Felt::ZERO,
        };
        let storage = NoteStorage::try_from(claim_storage).expect("claim storage round-trips");
        let recipient = NoteRecipient::new(Word::default(), ClaimNote::script(), storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);
        consumed_note(
            details,
            NoteAttachments::default(),
            Some(aid(BRIDGE)),
            block,
            tx_order,
        )
    }

    /// Build a sanctioned consumed GER note (UpdateGerNote) targeting the bridge
    /// and minted by `GER_MANAGER`, encoding `ger_byte` in every 32-bit limb.
    /// Returns the record plus the details-commitment → metadata entry the
    /// projector needs for the MA#28 `ConsumedExternal` provenance fallback.
    fn ger_note(
        block: u32,
        tx_order: Option<u32>,
        ger_byte: u8,
    ) -> (InputNoteRecord, ([u8; 32], NoteMetadata)) {
        // 8 felts, each a u32 limb. restore reads each limb as a big-endian u32.
        let limb = u32::from_be_bytes([ger_byte; 4]);
        let storage = NoteStorage::new(vec![Felt::from(limb); 8]).unwrap();
        let recipient = NoteRecipient::new(Word::default(), UpdateGerNote::script(), storage);
        let assets = NoteAssets::new(vec![]).unwrap();
        let details = NoteDetails::new(assets, recipient);

        // Provenance: sender = ger_manager, attachment = NetworkAccountTarget(bridge).
        let attachment = NoteAttachment::from(
            NetworkAccountTarget::new(aid(BRIDGE), NoteExecutionHint::Always).expect("nat"),
        );
        let attachments = NoteAttachments::from(attachment);
        let partial = PartialNoteMetadata::new(aid(GER_MANAGER), NoteType::Public);
        let metadata = NoteMetadata::new(partial, &attachments);

        let record = consumed_note(details, attachments, Some(aid(BRIDGE)), block, tx_order);
        let key = record.details_commitment().as_bytes();
        (record, (key, metadata))
    }

    /// Build a projector for the deterministic-core tests. The cursor/tip loop
    /// (`tick`) needs a live `&mut MidenClientLib`, so the unit tests drive the
    /// `project_notes` core directly with an in-memory consumed-note feed and a
    /// `None` recovery client.
    async fn test_projector(
        store: &StdArc<dyn Store>,
        block_state: &StdArc<BlockState>,
    ) -> SyntheticProjector {
        SyntheticProjector::new(
            store.clone(),
            block_state.clone(),
            &test_accounts(),
            7,
            None,
        )
        .await
        .unwrap()
    }

    async fn register_faucet(store: &StdArc<dyn Store>) {
        store
            .register_faucet(crate::store::FaucetEntry {
                faucet_id: aid(FAUCET),
                origin_address: [0u8; 20],
                origin_network: 0,
                symbol: "ETH".into(),
                origin_decimals: 18,
                miden_decimals: 8,
                scale: 10,
                metadata: vec![],
            })
            .await
            .unwrap();
    }

    /// Collect all synthetic logs in `[from, to]` via the public `get_logs`
    /// API, preserving (block, insertion) order.
    async fn logs_in_range(store: &StdArc<dyn Store>, from: u64, to: u64) -> Vec<SyntheticLog> {
        let filter = LogFilter {
            from_block: Some(format!("0x{from:x}")),
            to_block: Some(format!("0x{to:x}")),
            ..Default::default()
        };
        store.get_logs(&filter, to).await.unwrap()
    }

    /// (i) A Miden block with a bridge-consumed B2AGG note + a CLAIM note + a
    /// GER note projects THREE synthetic logs into the SAME synthetic block
    /// (Miden-1:1: synthetic block N == Miden block N), in the deterministic
    /// `(consumed_tx_order, note_id)` order, with sequential log indices.
    #[tokio::test]
    async fn projects_three_derivations_into_one_miden_block() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(5, Some(0));
        let n_claim = claim_note(5, Some(1));
        let (n_ger, ger_meta) = ger_note(5, Some(2), 0x11);

        // Intentionally shuffled input order — the projector's deterministic
        // sort, not arrival order, fixes the output.
        let notes = vec![n_ger.clone(), n_claim.clone(), n_b2agg.clone()];
        let output_metadata = HashMap::from([ger_meta]);
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let written = projector
            .project_notes(&notes, &output_metadata, 5, None)
            .await
            .unwrap();
        assert_eq!(written, 3, "all three derivations must emit one log each");

        // Miden-1:1: all three logs land in synthetic block 5 (== the Miden block).
        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 3, "three logs in the one synthetic block");
        assert_eq!(
            logs.iter().map(|l| l.block_number).collect::<Vec<_>>(),
            vec![5, 5, 5],
            "Miden-1:1: every log for Miden block 5 lands in synthetic block 5",
        );
        // The synthetic tip == the Miden block.
        assert_eq!(store.get_latest_block_number().await.unwrap(), 5);
        // Log indices are sequential in projection order.
        assert_eq!(
            logs.iter().map(|l| l.log_index).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );

        // Deterministic order matches the consumed_tx_order we set: B2AGG(0)@1,
        // CLAIM(1)@2, GER(2)@3. Identify each by its distinctive tx-hash shape.
        let b2agg_id = hex::encode(n_b2agg.details_commitment().as_bytes());
        let claim_id = hex::encode(n_claim.details_commitment().as_bytes());
        assert_eq!(
            logs[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&b2agg_id),
            "first log must be the B2AGG bridge-out (tx_order 0)"
        );
        assert_eq!(
            logs[1].transaction_hash,
            derive_manual_claim_tx_hash(&claim_id),
            "second log must be the CLAIM (tx_order 1)"
        );
        assert!(
            logs[2].transaction_hash.starts_with("0x"),
            "third log must be the GER (tx_order 2)"
        );
    }

    /// (ii) Re-projecting the same Miden block is idempotent — no duplicate
    /// logs and no tip advance (the `project_*` dedup keys short-circuit).
    #[tokio::test]
    async fn reprojecting_same_block_is_idempotent() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(7, Some(0));
        let n_claim = claim_note(7, Some(1));
        let (n_ger, ger_meta) = ger_note(7, Some(2), 0x22);
        let notes = vec![n_b2agg, n_claim, n_ger];
        let output_metadata = HashMap::from([ger_meta]);
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        let first = projector
            .project_notes(&notes, &output_metadata, 7, None)
            .await
            .unwrap();
        assert_eq!(first, 3);
        assert_eq!(store.get_latest_block_number().await.unwrap(), 7);

        let second = projector
            .project_notes(&notes, &output_metadata, 7, None)
            .await
            .unwrap();
        assert_eq!(second, 0, "second projection must emit no new logs");
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            7,
            "tip stays at the Miden block on a no-op re-projection",
        );

        let logs = logs_in_range(&store, 0, 7).await;
        assert_eq!(logs.len(), 3, "no duplicate logs after re-projection");
    }

    /// (iii) Notes consumed at different Miden blocks project into the synthetic
    /// blocks matching their Miden heights (Miden-1:1) — synthetic block N is
    /// exactly Miden block N, including the gaps between them.
    #[tokio::test]
    async fn distinct_nullifier_heights_project_into_their_miden_blocks() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(3, Some(0)); // consumed at Miden block 3
        let n_claim = claim_note(8, Some(0)); // consumed at Miden block 8
        let notes = vec![n_b2agg.clone(), n_claim.clone()];
        let output_metadata = HashMap::new();
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;

        // Project Miden block 3: only the B2AGG note belongs here → synthetic 3.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 3, None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 3);
        // Project Miden block 8: only the CLAIM note belongs here → synthetic 8.
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 8, None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 8);

        let logs = logs_in_range(&store, 0, 8).await;
        assert_eq!(logs.len(), 2);
        // Miden-1:1: synthetic blocks 3 and 8 (== the Miden heights), not 1 and 2.
        assert_eq!(logs[0].block_number, 3);
        assert_eq!(logs[1].block_number, 8);
        assert_eq!(
            logs[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
                n_b2agg.details_commitment().as_bytes()
            ))
        );
        assert_eq!(
            logs[1].transaction_hash,
            derive_manual_claim_tx_hash(&hex::encode(n_claim.details_commitment().as_bytes()))
        );
    }

    /// (iv) Two independent runs over the same consumed-note set produce
    /// byte-identical synthetic logs: same (Miden-1:1) block numbers, block
    /// hashes, log indices and ordering.
    #[tokio::test]
    async fn two_runs_are_byte_identical() {
        async fn run() -> Vec<SyntheticLog> {
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
            register_faucet(&store).await;
            let (n_ger, ger_meta) = ger_note(9, Some(2), 0x33);
            // Intentionally shuffled input order to prove the projector's
            // deterministic sort — not arrival order — fixes the output.
            let notes = vec![n_ger, claim_note(9, Some(1)), b2agg_note(9, Some(0))];
            let output_metadata = HashMap::from([ger_meta]);
            let block_state = StdArc::new(BlockState::new());
            let projector = test_projector(&store, &block_state).await;
            let written = projector
                .project_notes(&notes, &output_metadata, 9, None)
                .await
                .unwrap();
            assert_eq!(written, 3);
            logs_in_range(&store, 0, 9).await
        }

        let run_a = run().await;
        let run_b = run().await;
        assert_eq!(run_a.len(), 3);
        assert_eq!(run_a.len(), run_b.len());
        for (a, b) in run_a.iter().zip(run_b.iter()) {
            assert_eq!(a.block_number, b.block_number, "block numbers must match");
            assert_eq!(a.block_hash, b.block_hash, "block hashes must be identical");
            assert_eq!(a.log_index, b.log_index, "log indices must match");
            assert_eq!(
                a.transaction_hash, b.transaction_hash,
                "log ordering / tx hashes must match"
            );
            assert_eq!(a.topics, b.topics, "topics must match");
            assert_eq!(a.data, b.data, "data must match");
        }
    }

    /// Certificate-settlement regression: when `publish_claim` has linked the
    /// real claim eth-tx to the CLAIM note (`record_tx_note_link`), the projected
    /// ClaimEvent MUST ride that real tx hash — not a derived one. aggkit's
    /// L2BridgeSyncer fetches the claim tx by hash and decodes its `claimAsset`
    /// calldata to resolve the GER boundary; a derived hash points at a synthetic
    /// tx with EMPTY calldata, so aggkit fails "input too short: 0 bytes" and
    /// never settles the certificate.
    #[tokio::test]
    async fn claim_event_rides_linked_real_tx_hash() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_claim = claim_note(5, Some(0));
        let note_commitment = hex::encode(n_claim.details_commitment().as_bytes());
        let real_tx = "0x1111111111111111111111111111111111111111111111111111111111111111";
        // publish_claim records this link when it submits the CLAIM note.
        store
            .record_tx_note_link(real_tx, &note_commitment)
            .await
            .unwrap();

        let notes = vec![n_claim.clone()];
        let output_metadata = HashMap::new();
        let block_state = StdArc::new(BlockState::new());
        let projector = test_projector(&store, &block_state).await;
        assert_eq!(
            projector
                .project_notes(&notes, &output_metadata, 5, None)
                .await
                .unwrap(),
            1
        );

        let logs = logs_in_range(&store, 0, 5).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].transaction_hash, real_tx,
            "ClaimEvent must ride the linked real claim tx hash (carries claimAsset calldata)"
        );
        assert_ne!(
            logs[0].transaction_hash,
            derive_manual_claim_tx_hash(&note_commitment),
            "must NOT fall back to the derived hash when a link exists"
        );
    }
}
