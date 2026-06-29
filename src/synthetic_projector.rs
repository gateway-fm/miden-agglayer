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
//! ## Phase 1 status — core only, NOT wired into the live service
//!
//! This module is built in isolation and unit-tested. It is **not** invoked by
//! `main.rs` or any running loop yet, so it causes **zero production behaviour
//! change**. A later phase cuts the live writers over to it. In Phase 1 the
//! projector writes into the store exactly the way `restore` does — through the
//! shared `project_b2agg_note` / `project_claim_note` / `project_ger_note`
//! derivations — and is idempotent via the existing `is_*_processed` /
//! `is_ger_injected` dedup keys.
//!
//! ## Determinism contract
//!
//! Synthetic block `N` is a pure function of Miden block `N`'s consumed notes.
//! Intra-block events are ordered by `(consumed_tx_order, note_id)`, so
//! re-running the projector over the same chain yields byte-identical synthetic
//! blocks (numbers, hashes, log order, log indices).

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::bridge_address::get_bridge_address;
use crate::miden_client::MidenClient;
use crate::restore::{
    B2AggRestoreOutcome, ClaimProjectOutcome, GerProjectOutcome, project_b2agg_note,
    project_claim_note, project_ger_note,
};
use crate::store::Store;
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_protocol::account::AccountId;
use miden_protocol::note::NoteMetadata;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of the consumed-note feed the projector derives synthetic events
/// from. Abstracted behind a trait so the deterministic projection core can be
/// unit-tested with an in-memory feed, and the production adapter
/// ([`MidenClientNoteSource`]) can pull the same data from the live Miden
/// client without the projector core knowing about the `MidenClient::with`
/// closure plumbing.
#[async_trait::async_trait]
pub trait ConsumedNoteSource: Send + Sync {
    /// All consumed input notes known to the client (`NoteFilter::Consumed`).
    ///
    /// There is no server-side block-range filter for notes yet (see the
    /// restore module TODOs), so the projector pulls the full consumed set and
    /// filters by `nullifier_block_height` itself.
    async fn consumed_notes(&self) -> anyhow::Result<Vec<InputNoteRecord>>;

    /// Metadata of our *own* output-note records keyed by details-commitment
    /// bytes. This is the MA#28 provenance fallback for the metadata-less
    /// `ConsumedExternal` state a GER note lands in after the bridge consumes
    /// it (see `restore::project_ger_note`).
    async fn output_note_metadata(&self) -> anyhow::Result<HashMap<[u8; 32], NoteMetadata>>;

    /// The current (synced) Miden chain tip block height.
    async fn miden_tip(&self) -> anyhow::Result<u64>;
}

/// The synthetic projector. Owns the cursor (last projected Miden block height)
/// and is the only thing that would advance the synthetic tip.
pub struct SyntheticProjector {
    store: Arc<dyn Store>,
    block_state: Arc<BlockState>,
    source: Arc<dyn ConsumedNoteSource>,
    /// Bridge account id — the sole legitimate consumer of a bridge-out B2AGG
    /// note (MA#3) and the expected GER target (MA#28).
    bridge_id: AccountId,
    /// Expected GER sender (ger_manager, or service for legacy deployments).
    expected_ger_sender: AccountId,
    /// Last projected Miden block height. The projector is the single owner of
    /// this cursor (SINGLE-PROCESS ONLY).
    cursor: AtomicU64,
}

impl SyntheticProjector {
    /// Build a projector from the account configuration. `start_cursor` is the
    /// last already-projected Miden block height (0 for a fresh chain).
    pub fn new(
        store: Arc<dyn Store>,
        block_state: Arc<BlockState>,
        source: Arc<dyn ConsumedNoteSource>,
        accounts: &AccountsConfig,
        start_cursor: u64,
    ) -> Self {
        // MA#28 — same fallback as `restore_gers` / `submit_update_ger_note`:
        // legacy deployments without a dedicated ger_manager mint GER notes
        // from the service account.
        let expected_ger_sender = accounts
            .ger_manager
            .as_ref()
            .map(|a| a.0)
            .unwrap_or(accounts.service.0);
        Self {
            store,
            block_state,
            source,
            bridge_id: accounts.bridge.0,
            expected_ger_sender,
            cursor: AtomicU64::new(start_cursor),
        }
    }

    /// The current cursor (last projected Miden block height).
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// Project exactly one synthetic block `miden_block` from the notes consumed
    /// at that Miden block.
    ///
    /// Fetches the consumed-note feed, keeps only notes whose consumed-state
    /// `nullifier_block_height == miden_block`, sorts them deterministically by
    /// `(consumed_tx_order, note_id_hex)`, and runs each through the shared
    /// `project_*` derivations, writing logs at synthetic block `== miden_block`.
    /// Returns the number of synthetic logs written.
    ///
    /// Idempotent: re-projecting the same Miden block writes no duplicate logs
    /// (the `project_*` derivations short-circuit on the existing dedup keys).
    pub async fn project_block(&self, miden_block: u64) -> anyhow::Result<usize> {
        let consumed = self.source.consumed_notes().await?;

        // Keep only notes attributed to this Miden block by their consumed
        // state's nullifier_block_height. Notes that aren't in a consumed state
        // (no nullifier_block_height) are not attributed to any block.
        let mut notes: Vec<&InputNoteRecord> = consumed
            .iter()
            .filter(|n| n.state().consumed_block_height().map(|h| h.as_u64()) == Some(miden_block))
            .collect();

        // Determinism: order intra-block events by (consumed_tx_order,
        // note_id_hex). `consumed_tx_order` is the per-account position of the
        // consuming transaction within the block; the note-id hex is the stable
        // tie-breaker (matches the G7 sort the restore phases use). `None`
        // tx-orders sort first and stay stable under the secondary key.
        notes.sort_by(|a, b| {
            let order = a
                .state()
                .consumed_tx_order()
                .cmp(&b.state().consumed_tx_order());
            order.then_with(|| {
                let ka = hex::encode(a.details_commitment().as_bytes());
                let kb = hex::encode(b.details_commitment().as_bytes());
                ka.cmp(&kb)
            })
        });

        let block_hash = self.block_state.get_block_hash(miden_block);
        let timestamp = self.block_state.get_block_timestamp(miden_block);
        let bridge_address = get_bridge_address();

        // GER provenance fallback map — only needed if a GER-shaped note shows
        // up, but fetched once for the block to keep derivation pure/ordered.
        let output_metadata = self.source.output_note_metadata().await?;

        let mut logs = 0usize;
        for note in notes {
            // A consumed note matches at most one of the three script roots, so
            // trying all three derivations emits at most one synthetic log per
            // note. This is exactly the unification the design doc calls for:
            // the three restore derivations collapsed into one per-note loop.
            if project_b2agg_note(
                &self.store,
                note,
                self.bridge_id,
                miden_block,
                block_hash,
                bridge_address,
                // Cantina #13 metadata recovery (client + L1 RPC) is wired in at
                // the cut-over phase, alongside the persisted cursor; Phase-1 the
                // projector is isolated, so no recovery context is threaded yet.
                None,
                None,
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
                &output_metadata,
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

        // Atomic visibility: the block's logs are all written above BEFORE the
        // synthetic tip advances to `miden_block`. The block hash is a pure
        // function of the block number (`BlockState`), so this is deterministic.
        // Mapping is 1:1 and gap-free — an empty Miden block yields an empty
        // synthetic block and the tip still advances.
        self.block_state.set_current_block(miden_block);
        if self.store.get_latest_block_number().await? < miden_block {
            self.store.set_latest_block_number(miden_block).await?;
        }

        Ok(logs)
    }

    /// Process every Miden block from `cursor + 1` to the current Miden tip in
    /// order, projecting each one and advancing the cursor. Returns the new
    /// cursor (== the projected Miden tip).
    ///
    /// This is the normal projector loop; catch-up after a restart is the same
    /// code path (the cursor simply starts further behind the tip).
    pub async fn tick(&self) -> anyhow::Result<u64> {
        let tip = self.source.miden_tip().await?;
        let mut cursor = self.cursor.load(Ordering::Acquire);
        while cursor < tip {
            let next = cursor + 1;
            self.project_block(next).await?;
            // Advance the cursor only after the block is fully projected, so a
            // crash mid-block re-projects (idempotently) rather than skipping.
            self.cursor.store(next, Ordering::Release);
            cursor = next;
        }
        Ok(cursor)
    }
}

/// Production adapter pulling the consumed-note feed from the live
/// [`MidenClient`]. **Phase 1: defined but not wired into any running loop** —
/// the cut-over phase constructs the projector with this source.
pub struct MidenClientNoteSource {
    client: Arc<MidenClient>,
}

impl MidenClientNoteSource {
    pub fn new(client: Arc<MidenClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ConsumedNoteSource for MidenClientNoteSource {
    async fn consumed_notes(&self) -> anyhow::Result<Vec<InputNoteRecord>> {
        let out = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out_inner = out.clone();
        self.client
            .with(move |client| {
                Box::new(async move {
                    let notes = client
                        .get_input_notes(NoteFilter::Consumed)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to get consumed notes: {e}"))?;
                    *out_inner.lock().unwrap() = notes;
                    Ok(())
                })
            })
            .await?;
        let notes = std::mem::take(&mut *out.lock().unwrap());
        Ok(notes)
    }

    async fn output_note_metadata(&self) -> anyhow::Result<HashMap<[u8; 32], NoteMetadata>> {
        let out = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let out_inner = out.clone();
        self.client
            .with(move |client| {
                Box::new(async move {
                    let map: HashMap<[u8; 32], NoteMetadata> = client
                        .get_output_notes(NoteFilter::All)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to get output notes: {e}"))?
                        .into_iter()
                        .map(|rec| (rec.details_commitment().as_bytes(), *rec.metadata()))
                        .collect();
                    *out_inner.lock().unwrap() = map;
                    Ok(())
                })
            })
            .await?;
        let map = std::mem::take(&mut *out.lock().unwrap());
        Ok(map)
    }

    async fn miden_tip(&self) -> anyhow::Result<u64> {
        let out = Arc::new(std::sync::Mutex::new(0u64));
        let out_inner = out.clone();
        self.client
            .with(move |client| {
                Box::new(async move {
                    let height = client
                        .get_sync_height()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?;
                    *out_inner.lock().unwrap() = height.as_u64();
                    Ok(())
                })
            })
            .await?;
        let tip = *out.lock().unwrap();
        Ok(tip)
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

    /// In-memory consumed-note source for the deterministic projector tests.
    struct VecNoteSource {
        notes: Vec<InputNoteRecord>,
        output_metadata: HashMap<[u8; 32], NoteMetadata>,
        tip: u64,
    }

    #[async_trait::async_trait]
    impl ConsumedNoteSource for VecNoteSource {
        async fn consumed_notes(&self) -> anyhow::Result<Vec<InputNoteRecord>> {
            // InputNoteRecord isn't Clone-cheap to assume; rebuild refs by
            // cloning the records (cheap for the small test feeds).
            Ok(self.notes.clone())
        }
        async fn output_note_metadata(&self) -> anyhow::Result<HashMap<[u8; 32], NoteMetadata>> {
            Ok(self.output_metadata.clone())
        }
        async fn miden_tip(&self) -> anyhow::Result<u64> {
            Ok(self.tip)
        }
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
    /// GER note projects to ONE synthetic block carrying all three logs, in the
    /// deterministic `(consumed_tx_order, note_id)` order.
    #[tokio::test]
    async fn projects_three_derivations_into_one_block() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(5, Some(0));
        let n_claim = claim_note(5, Some(1));
        let (n_ger, ger_meta) = ger_note(5, Some(2), 0x11);

        let source = StdArc::new(VecNoteSource {
            notes: vec![n_ger.clone(), n_claim.clone(), n_b2agg.clone()],
            output_metadata: HashMap::from([ger_meta]),
            tip: 5,
        });
        let block_state = StdArc::new(BlockState::new());
        let projector =
            SyntheticProjector::new(store.clone(), block_state, source, &test_accounts(), 0);

        let written = projector.project_block(5).await.unwrap();
        assert_eq!(written, 3, "all three derivations must emit one log each");

        let logs = logs_in_range(&store, 5, 5).await;
        assert_eq!(logs.len(), 3, "exactly three logs in synthetic block 5");
        // All logs land in synthetic block 5.
        assert!(logs.iter().all(|l| l.block_number == 5));
        // Log indices are sequential in projection order.
        assert_eq!(
            logs.iter().map(|l| l.log_index).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );

        // Deterministic order matches the consumed_tx_order we set: B2AGG(0),
        // CLAIM(1), GER(2). Identify each by its distinctive tx-hash shape.
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
    /// logs (the `project_*` dedup keys short-circuit).
    #[tokio::test]
    async fn reprojecting_same_block_is_idempotent() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(7, Some(0));
        let n_claim = claim_note(7, Some(1));
        let (n_ger, ger_meta) = ger_note(7, Some(2), 0x22);
        let source = StdArc::new(VecNoteSource {
            notes: vec![n_b2agg, n_claim, n_ger],
            output_metadata: HashMap::from([ger_meta]),
            tip: 7,
        });
        let projector = SyntheticProjector::new(
            store.clone(),
            StdArc::new(BlockState::new()),
            source,
            &test_accounts(),
            0,
        );

        let first = projector.project_block(7).await.unwrap();
        assert_eq!(first, 3);
        let second = projector.project_block(7).await.unwrap();
        assert_eq!(second, 0, "second projection must emit no new logs");

        let logs = logs_in_range(&store, 7, 7).await;
        assert_eq!(logs.len(), 3, "no duplicate logs after re-projection");
    }

    /// (iii) Notes with different `nullifier_block_height` project into
    /// different synthetic blocks.
    #[tokio::test]
    async fn distinct_nullifier_heights_project_into_distinct_blocks() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        register_faucet(&store).await;

        let n_b2agg = b2agg_note(3, Some(0)); // block 3
        let n_claim = claim_note(8, Some(0)); // block 8
        let source = StdArc::new(VecNoteSource {
            notes: vec![n_b2agg.clone(), n_claim.clone()],
            output_metadata: HashMap::new(),
            tip: 8,
        });
        let projector = SyntheticProjector::new(
            store.clone(),
            StdArc::new(BlockState::new()),
            source,
            &test_accounts(),
            0,
        );

        // Project block 3: only the B2AGG note belongs here.
        assert_eq!(projector.project_block(3).await.unwrap(), 1);
        // Project block 8: only the CLAIM note belongs here.
        assert_eq!(projector.project_block(8).await.unwrap(), 1);

        let logs3 = logs_in_range(&store, 3, 3).await;
        let logs8 = logs_in_range(&store, 8, 8).await;
        assert_eq!(logs3.len(), 1);
        assert_eq!(logs8.len(), 1);
        assert_eq!(logs3[0].block_number, 3);
        assert_eq!(logs8[0].block_number, 8);
        assert_eq!(
            logs3[0].transaction_hash,
            crate::bridge_out::derive_bridge_out_tx_hash(&hex::encode(
                n_b2agg.details_commitment().as_bytes()
            ))
        );
        assert_eq!(
            logs8[0].transaction_hash,
            derive_manual_claim_tx_hash(&hex::encode(n_claim.details_commitment().as_bytes()))
        );
        // No cross-contamination.
        assert!(logs_in_range(&store, 4, 7).await.is_empty());
    }

    /// (iv) Two independent runs over the same consumed-note set produce
    /// byte-identical synthetic logs: same block numbers, block hashes, log
    /// indices and ordering.
    #[tokio::test]
    async fn two_runs_are_byte_identical() {
        async fn run() -> Vec<SyntheticLog> {
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
            register_faucet(&store).await;
            let (n_ger, ger_meta) = ger_note(9, Some(2), 0x33);
            let source = StdArc::new(VecNoteSource {
                // Intentionally shuffled input order to prove the projector's
                // deterministic sort — not arrival order — fixes the output.
                notes: vec![n_ger, claim_note(9, Some(1)), b2agg_note(9, Some(0))],
                output_metadata: HashMap::from([ger_meta]),
                tip: 9,
            });
            let projector = SyntheticProjector::new(
                store.clone(),
                StdArc::new(BlockState::new()),
                source,
                &test_accounts(),
                0,
            );
            let cursor = projector.tick().await.unwrap();
            assert_eq!(cursor, 9, "tick must advance the cursor to the Miden tip");
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
}
