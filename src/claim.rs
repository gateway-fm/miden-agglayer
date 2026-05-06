use crate::accounts_config::AccountsConfig;
use crate::faucet_ops;
use crate::miden_client::{MidenClient, MidenClientLib};
use crate::store::{FaucetEntry, Store};
use alloy::primitives::{BlockNumber, Bytes, FixedBytes, LogData};
use alloy::sol_types::SolEvent;
use miden_base_agglayer::{
    ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
    ProofData, SmtNode,
};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::AccountId;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::note::Note;
use miden_protocol::transaction::TransactionId;
use std::sync::{Arc, OnceLock};

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L556
    #[derive(Debug)]
    function claimAsset(
        bytes32[32] calldata smtProofLocalExitRoot,
        bytes32[32] calldata smtProofRollupExitRoot,
        uint256 globalIndex,
        bytes32 mainnetExitRoot,
        bytes32 rollupExitRoot,
        uint32 originNetwork,
        address originTokenAddress,
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        bytes calldata metadata
    );
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L139
    #[derive(Debug)]
    event ClaimEvent(
        uint256 globalIndex,
        uint32 originNetwork,
        address originAddress,
        address destinationAddress,
        uint256 amount
    );
}

impl From<claimAssetCall> for ClaimEvent {
    fn from(value: claimAssetCall) -> Self {
        Self {
            globalIndex: value.globalIndex,
            originNetwork: value.originNetwork,
            originAddress: value.originTokenAddress,
            destinationAddress: value.destinationAddress,
            amount: value.amount,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Faucet {
    id: AccountId,
    decimals: u8,
    origin_token_decimals: u8,
}

/// Look up a faucet for the given origin token, auto-creating one if not found.
///
/// On the first bridge of a new ERC-20 token, the faucet is created on Miden,
/// registered in the bridge, and saved to the Store — all automatically.
///
/// Concurrency: this function is always called inside a
/// `MidenClient::with(|client| ...)` closure, which holds the global Miden
/// client mutex for its duration. Two concurrent first-bridge claims for the
/// same token therefore serialise on that lock — the second call sees the
/// faucet already registered by the first and takes the fast `get_faucet_by_origin`
/// path. The Cantina #1 colliding-network refusal predicate (added in `e6a33ae`)
/// is consequently TOCTOU-safe by virtue of the surrounding lock; a future
/// refactor that moves auto-create outside the client mutex must add an
/// explicit per-token-address mutex (analogous to `PerSignerLocks` for R4).
async fn find_or_create_faucet(
    token_address: alloy::primitives::Address,
    origin_network: u32,
    metadata: &Bytes,
    store: &dyn Store,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
) -> anyhow::Result<Faucet> {
    // 1. Try store lookup first
    if let Some(entry) = store
        .get_faucet_by_origin(&token_address.0.0, origin_network)
        .await?
    {
        return Ok(Faucet {
            id: entry.faucet_id,
            decimals: entry.miden_decimals,
            origin_token_decimals: entry.origin_decimals,
        });
    }

    // 2. Cantina #1 — refuse colliding-network auto-create. The on-chain bridge registry
    //    keys faucets by `hash(origin_token_address)` ALONE, so registering a second faucet
    //    for the same token address under a different `origin_network` will silently
    //    overwrite the first registration on-chain. Reject before we reach that path.
    let same_address_faucets = store
        .find_faucets_by_origin_address(&token_address.0.0)
        .await?;
    if let Some(existing) = same_address_faucets
        .iter()
        .find(|f| f.origin_network != origin_network)
    {
        anyhow::bail!(
            "refusing to auto-create faucet for token {token_address} on network {origin_network}: \
             a faucet for the same token address is already registered under network {} \
             (faucet_id {}). Cross-network token-address collision (Cantina #1) — auto-creating \
             would overwrite the existing on-chain registration. Investigate and resolve manually.",
            existing.origin_network,
            existing.faucet_id,
        );
    }

    // 3. Auto-create: parse token metadata from claimAsset call
    let (symbol, origin_decimals) = faucet_ops::parse_token_metadata(metadata, &token_address)?;
    let miden_decimals: u8 = origin_decimals.min(8);
    let scale = origin_decimals.checked_sub(miden_decimals).ok_or_else(|| {
        anyhow::anyhow!(
            "origin decimals {origin_decimals} < miden decimals {miden_decimals} for token {token_address}"
        )
    })?;

    tracing::info!(
        token_address = %token_address,
        symbol = %symbol,
        origin_decimals,
        scale,
        "auto-creating faucet for new ERC-20 token"
    );

    // 3. Create faucet on Miden, deploy, register in bridge. The faucet's stored
    //    metadata_hash must match the CLAIM note's leaf_data.metadata_hash, which is
    //    keccak256(metadata) (both empty for native ETH and abi.encode(name,symbol,decimals)
    //    for ERC-20s). Using MetadataHash::from_abi_encoded on the raw metadata bytes matches
    //    the L1 bridge contract exactly.
    let metadata_hash = MetadataHash::from_abi_encoded(metadata.as_ref());

    let faucet_account = faucet_ops::create_and_register_faucet(
        client,
        &symbol,
        miden_decimals,
        &token_address.0.0,
        origin_network,
        scale,
        accounts.service.0,
        accounts.bridge.0,
        metadata_hash,
    )
    .await?;

    // 4. Save to store
    let entry = FaucetEntry {
        faucet_id: faucet_account.id(),
        origin_address: token_address.0.0,
        origin_network,
        symbol,
        origin_decimals,
        miden_decimals,
        scale,
    };
    store.register_faucet(entry).await?;

    Ok(Faucet {
        id: faucet_account.id(),
        decimals: miden_decimals,
        origin_token_decimals: origin_decimals,
    })
}

fn bytes32_array_to_smt_nodes(values: [FixedBytes<32>; 32]) -> [SmtNode; 32] {
    values.map(|v| SmtNode::new(v.0))
}

/// Decode the agglayer mainnet flag from a `globalIndex` U256.
///
/// GlobalIndex layout (per miden-agglayer's `eth_types::global_index`):
///   - bytes 0..20  : zero (top 160 bits of the U256)
///   - bytes 20..24 : mainnet flag (limb 5; value = 1 for mainnet, 0 for rollup)
///   - bytes 24..28 : rollup index (limb 6; must be 0 for mainnet deposits)
///   - bytes 28..32 : leaf index (limb 7)
///
/// `GlobalIndexExt::is_mainnet()` is gated behind upstream's `testing` feature so we
/// decode the flag inline.
fn is_mainnet_global_index(global_index_bytes: &[u8; 32]) -> bool {
    let flag = u32::from_be_bytes([
        global_index_bytes[20],
        global_index_bytes[21],
        global_index_bytes[22],
        global_index_bytes[23],
    ]);
    flag == 1
}

/// Build the CLAIM note's `ProofData`, canonicalising the rollup-side fields that the
/// upstream MASM mainnet branch genuinely doesn't read (Cantina #11).
///
/// Self-review of-the-fix history
/// ------------------------------
/// The original Cantina #11 commit zeroed *both* `smt_proof_rollup_exit_root` (256
/// felts) AND `rollup_exit_root` (8 felts) for mainnet claims, on the assumption
/// that neither was read by `bridge_in.masm`'s mainnet branch. That assumption
/// matched the SMT proof but was wrong about the exit root: the dynamic-ERC20
/// e2e (and any second-and-later mainnet claim against a non-zero on-chain
/// `PolygonZkEVMGlobalExitRootV2.rollupExitRoot`) failed with
/// `ERR_GER_NOT_FOUND` (assertion code `0xDF0E804B375D0B3B`).
///
/// Trace: `bridge_in.masm::verify_leaf` (line 532-553) calls `compute_ger`
/// (line 385-391) BEFORE the mainnet/rollup branch split. `compute_ger` is
/// `keccak256(mainnet_exit_root || rollup_exit_root)` and the result is looked
/// up in `GER_MAP_STORAGE_SLOT` by `assert_valid_ger`
/// (`bridge_config.masm::101`). The map is populated by `update_ger`
/// (`bridge_config.masm::48`) when an `UpdateGerNote` is consumed; aggkit
/// `ger.rs::141` injects the *real* L1 GER digest verbatim. Zeroing
/// `rollup_exit_root` on the CLAIM side made the recomputed key
/// `keccak256(mainnet_real || 0)` instead of `keccak256(mainnet_real ||
/// rollup_real)` whenever the L1 contract had advanced
/// `rollupExitRoot` past zero — the lookup then missed and the assertion
/// fired.
///
/// The original Cantina #11 NoteId-determinism property is preserved without
/// the over-zero: for a given mainnet leaf at a given GER, both
/// `mainnet_exit_root` and `rollup_exit_root` are fixed by the L1 contract
/// state, so they are NOT attacker-tunable in the way the SMT rollup proof
/// path bytes (256 felts of merkle siblings — only the SMT *path*-derived
/// root needs to match anything; the rest is attacker-supplied padding) and
/// the rollup-index bytes of `globalIndex` (must be 0 per the upstream layout
/// spec, but unread/unasserted) genuinely were.
///
/// Current canonicalisation (mainnet only):
/// - `smt_proof_rollup_exit_root` → all-zero (256 felts); unread by mainnet branch
/// - `globalIndex` bytes 24..28 (rollup index) → zero per layout spec
///
/// `rollup_exit_root` is left as-is — it IS read by `compute_ger`.
fn build_canonical_proof_data(params: &claimAssetCall) -> ProofData {
    let mut global_index_bytes = params.globalIndex.to_be_bytes::<32>();
    let is_mainnet = is_mainnet_global_index(&global_index_bytes);
    if is_mainnet {
        // Rollup index (limb 6 = bytes 24..28) must be 0 for mainnet deposits.
        // Zero it explicitly so attacker-supplied garbage in those bytes can't
        // change the resulting NoteId.
        global_index_bytes[24..28].fill(0);
    }
    ProofData {
        smt_proof_local_exit_root: bytes32_array_to_smt_nodes(params.smtProofLocalExitRoot),
        smt_proof_rollup_exit_root: if is_mainnet {
            [SmtNode::new([0u8; 32]); 32]
        } else {
            bytes32_array_to_smt_nodes(params.smtProofRollupExitRoot)
        },
        global_index: GlobalIndex::new(global_index_bytes),
        mainnet_exit_root: ExitRoot::new(params.mainnetExitRoot.0),
        // rollup_exit_root MUST NOT be zeroed: bridge_in's compute_ger feeds
        // it through keccak256 to derive the GER lookup key, which must match
        // the digest the GER manager injected (the *real* L1 root pair).
        // See the docstring above for the diagnosis trail.
        rollup_exit_root: ExitRoot::new(params.rollupExitRoot.0),
    }
}

/// Scales an L1 deposit amount into a Miden fungible-token amount using the faucet's
/// decimal layout. Sub-unit wei are truncated (the full value is still preserved in
/// `leaf_data.amount`); the only hard failure is exceeding `FungibleAsset::MAX_AMOUNT`.
fn scale_claim_amount(
    amount: &EthAmount,
    faucet: Faucet,
) -> Result<miden_protocol::Felt, anyhow::Error> {
    let scale_byte = faucet
        .origin_token_decimals
        .checked_sub(faucet.decimals)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "faucet {} has miden_decimals ({}) > origin_token_decimals ({}); \
                 invariant violated, refusing to compute scale",
                faucet.id,
                faucet.decimals,
                faucet.origin_token_decimals,
            )
        })?;
    let scale = u32::from(scale_byte);
    amount
        .scale_to_token_amount(scale)
        .map_err(|e| anyhow::anyhow!("claim amount is not representable on Miden: {e}"))
}

async fn create_claim(
    params: claimAssetCall,
    faucet: Faucet,
    accounts: &AccountsConfig,
    store: &dyn Store,
    rng: &mut impl FeltRng,
    reject_zero_padding: bool,
) -> anyhow::Result<Note> {
    let sender = accounts.service.0;

    let _dest_account = crate::address_mapper::resolve_address_with_policy(
        store,
        params.destinationAddress,
        accounts,
        reject_zero_padding,
    )
    .await?;

    let proof_data = build_canonical_proof_data(&params);

    let leaf_data = LeafData {
        origin_network: params.originNetwork,
        origin_token_address: EthAddress::new(params.originTokenAddress.0.0),
        destination_network: params.destinationNetwork,
        destination_address: EthAddress::new(params.destinationAddress.0.0),
        amount: EthAmount::new(params.amount.to_be_bytes::<32>()),
        metadata_hash: MetadataHash::from_abi_encoded(params.metadata.as_ref()),
    };

    let miden_claim_amount = scale_claim_amount(&leaf_data.amount, faucet)?;
    let storage = ClaimNoteStorage {
        proof_data,
        leaf_data,
        miden_claim_amount,
    };

    // CLAIM notes now target the bridge account (0.14.x). The bridge validates the proof and
    // produces a MINT note targeted at the faucet. The faucet then creates the final P2ID note
    // for the destination wallet (derived from leaf_data.destination_address).
    let note = miden_base_agglayer::create_claim_note(storage, accounts.bridge.0, sender, rng)?;
    Ok(note)
}

#[derive(Debug, Clone)]
pub struct PublishClaimTxn {
    pub txn_id: TransactionId,
    pub expires_at: BlockNumber,
    pub log: LogData,
    /// CLAIM note ID for consumption tracking (deferred receipts).
    pub claim_note_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
    store: &dyn Store,
    latest_block_num: BlockNumber,
    reject_zero_padding: bool,
    expected_mints: Option<&Arc<crate::expected_mint_tracker::ExpectedMintTracker>>,
) -> anyhow::Result<PublishClaimTxn> {
    let faucet = find_or_create_faucet(
        params.originTokenAddress,
        params.originNetwork,
        &params.metadata,
        store,
        client,
        accounts,
    )
    .await?;

    tracing::info!(
        global_index = %params.globalIndex,
        origin_network = %params.originNetwork,
        dest_address = %params.destinationAddress,
        amount = %params.amount,
        faucet_id = %crate::accounts_config::AccountIdBech32(faucet.id),
        mainnet_exit_root = %alloy::hex::encode(params.mainnetExitRoot.0),
        rollup_exit_root = %alloy::hex::encode(params.rollupExitRoot.0),
        "creating CLAIM note"
    );

    let claim_note = create_claim(
        params.clone(),
        faucet,
        accounts,
        store,
        client.rng(),
        reject_zero_padding,
    )
    .await?;
    let claim_note_id = claim_note.id().to_string();

    const EXPIRATION_DELTA: u16 = 10;
    let expires_at = latest_block_num + EXPIRATION_DELTA as u64;

    // Wait for the NTX builder to consume the UpdateGerNote on the bridge account.
    // The CLAIM note's FPI calls assert_valid_ger which checks the bridge account's
    // GER storage. If we submit the CLAIM before the GER is stored, it will fail.
    // Typically the GER note is consumed within ~5s (2-3 blocks). We wait up to 5
    // cycles of 3s (15s total) which gives the NTX builder plenty of time.
    //
    // G6 — early-exit when aggkit already records the GER as injected. The
    // `mark_ger_injected` flag is set when the proxy submits the GER inject
    // tx; for any GER that's been through aggkit's own submit path within this
    // process's lifetime, the bridge has already consumed it (or will within
    // milliseconds). We still sync_state once to refresh, but skip the
    // 4×3s = 12s of additional waiting in the common case.
    let claim_ger = crate::ger::combined_ger(&params.mainnetExitRoot.0, &params.rollupExitRoot.0);
    tracing::info!("waiting for GER to propagate to bridge account before submitting CLAIM...");
    for i in 0..5 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        client.sync_state().await?;
        tracing::debug!(cycle = i, "GER propagation sync cycle");
        if store.is_ger_injected(&claim_ger).await.unwrap_or(false) {
            tracing::info!(
                cycle = i,
                "G6: GER recorded as injected by proxy — skipping remaining wait cycles"
            );
            ::metrics::counter!("rpc_claim_ger_wait_short_circuit_total").increment(1);
            break;
        }
    }
    tracing::info!("GER propagation wait complete, submitting CLAIM note");

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![claim_note])
        .build()?;

    // Execute and check the output notes before submission. `ExecutedTransaction` still
    // produces `RawOutputNote::{Full, Partial}`, but the proven transaction now produces
    // `OutputNote::{Public, Private}` — 0.14.x renamed the final-form variants.
    let tx_result = client
        .execute_transaction(accounts.service.0, txn_request)
        .await?;
    let exec_tx = tx_result.executed_transaction();
    for (i, note) in exec_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::RawOutputNote::Full(_) => "Full",
            miden_protocol::transaction::RawOutputNote::Partial(_) => "Partial",
        };
        tracing::info!(note_idx = i, variant = %variant, "executed tx output note");
    }

    let proven_tx = client.prove_transaction(&tx_result).await?;
    for (i, note) in proven_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::OutputNote::Public(_) => "Public",
            miden_protocol::transaction::OutputNote::Private(_) => "Private",
        };
        tracing::info!(note_idx = i, variant = %variant, "proven tx output note");
    }

    let txn_id = tx_result.executed_transaction().id();
    let _submission_height = client
        .submit_proven_transaction(proven_tx, &tx_result)
        .await?;
    client
        .apply_transaction(&tx_result, _submission_height)
        .await?;
    tracing::info!("submitted claim note txn: {txn_id}, claim_note_id: {claim_note_id}");

    // Cantina #7: record the submitted CLAIM in the expected-MINT tracker
    // BEFORE awaiting commit. If wait_for_transaction_commit times out (20s)
    // and we bail!, the entry remains in the tracker. The bridge_out
    // scanner's tick path then escalates to StaleAlert per global_index,
    // giving on-call a list of stuck CLAIMs by L1 leaf. On successful
    // commit (the next code block) we mark_landed to drop the entry.
    if let Some(tracker) = expected_mints {
        let global_index_bytes: [u8; 32] = params.globalIndex.to_be_bytes();
        let claim_id_bytes: [u8; 32] = tx_result
            .executed_transaction()
            .output_notes()
            .iter()
            .map(|n| match n {
                miden_protocol::transaction::RawOutputNote::Full(full) => full.id().as_bytes(),
                miden_protocol::transaction::RawOutputNote::Partial(partial) => {
                    partial.id().as_bytes()
                }
            })
            .next()
            .unwrap_or_default();
        if claim_id_bytes != [0u8; 32] {
            tracker.record_expected(global_index_bytes, claim_id_bytes);
        }
    }

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("claim tx {txn_id} committed to block");
        // Cantina #7: mark Landed once `wait_for_transaction_commit`
        // confirms the CLAIM tx was committed. Aggkit's miden-client
        // operates on the proxy's service account — it CANNOT observe
        // the bridge account's consumption of our CLAIM via
        // NoteFilter::Consumed (the consumed-set returned by miden-client
        // is restricted to our tracked accounts, not the bridge's). The
        // commit confirmation is the right closure point: from there,
        // the bridge's MINT emission is deterministic, and tracking
        // longer would only fire spurious StaleAlerts.
        //
        // We still keep the record_expected → tick path useful: any
        // CLAIM that fails to commit (tx not in block within 20s) does
        // NOT reach this branch, so the tracker entry remains and the
        // tick eventually escalates to StaleAlert with the global_index
        // for operator triage.
        if let Some(tracker) = expected_mints {
            let global_index_bytes: [u8; 32] = params.globalIndex.to_be_bytes();
            tracker.mark_landed(global_index_bytes);
        }
    } else {
        anyhow::bail!("claim tx {txn_id} was submitted but not committed within 20s");
    }

    let event = ClaimEvent::from(params);
    let log = event.encode_log_data();

    Ok(PublishClaimTxn {
        txn_id,
        expires_at,
        log,
        claim_note_id: Some(claim_note_id),
    })
}

/// Publish a claim using a fresh miden-client instance (Igor's approach).
///
/// Creates a new client per call to avoid stale account state from the
/// long-lived MidenClient's background sync loop. After faucet creation
/// or prior CLAIMs, the service account's state drifts in the long-lived
/// client, causing `IncorrectAccountInitialCommitment` errors. Recording
/// of the ClaimEvent happens before the result is sent back so the event
/// is in the store even if the HTTP caller disconnects.
#[allow(clippy::too_many_arguments)]
pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: Arc<dyn Store>,
    block_state: std::sync::Arc<crate::block_state::BlockState>,
    latest_block_num: BlockNumber,
    txn_hash: alloy::primitives::TxHash,
    txn_envelope: alloy::consensus::TxEnvelope,
    signer: alloy::primitives::Address,
    store_dir: std::path::PathBuf,
    node_url: String,
    reject_zero_padding: bool,
    expected_mints: Option<Arc<crate::expected_mint_tracker::ExpectedMintTracker>>,
) -> anyhow::Result<PublishClaimTxn> {
    let result = Arc::new(OnceLock::<PublishClaimTxn>::new());
    let result_inner = result.clone();

    if node_url.is_empty() {
        // Test path: use the existing MidenClient.
        //
        // Race-safe ordering: write the txn+log at (current_latest + 1) BEFORE
        // bumping `latest_block_number`. See the matching comment in
        // `bridge_out.rs::on_post_sync`: advancing the counter first leaves a
        // window where `eth_blockNumber` returns N but no log exists at block N
        // yet, so aggsender / bridge-service skip the event entirely.
        let result_test = result.clone();
        client
            .with(move |client| {
                Box::new(async move {
                    let value = publish_claim_internal(
                        params,
                        client,
                        &accounts.0,
                        &*store,
                        latest_block_num,
                        reject_zero_padding,
                        expected_mints.as_ref(),
                    )
                    .await?;
                    let block_num = store.get_latest_block_number().await? + 1;
                    let block_hash = block_state.get_block_hash(block_num);
                    store
                        .txn_begin(
                            txn_hash,
                            crate::store::TxnEntry {
                                id: Some(value.txn_id),
                                envelope: txn_envelope,
                                signer,
                                expires_at: Some(value.expires_at),
                                logs: vec![value.log.clone()],
                            },
                        )
                        .await?;
                    store
                        .txn_commit(txn_hash, Ok(()), block_num, block_hash)
                        .await?;
                    store.set_latest_block_number(block_num).await?;
                    tracing::info!(
                        eth_tx = %txn_hash,
                        block_num,
                        "ClaimEvent recorded (cancellation-safe)"
                    );
                    let _ = result_test.set(value);
                    Ok(())
                })
            })
            .await?;
        return result
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("publish_claim: result not set"));
    }

    let keystore = client.get_keystore();
    let store_path = store_dir.join("store.sqlite3");

    // Production path: fresh client per call (Igor's approach).
    let store_clone = store.clone();
    let accounts_inner = accounts.0.clone();
    let expected_mints_clone = expected_mints.clone();
    let join_result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            use ::miden_client::DebugMode;
            use ::miden_client::builder::ClientBuilder;
            use ::miden_client_sqlite_store::ClientBuilderSqliteExt;

            // Resolve via the same helper `MidenClient::new` uses, so shortcut
            // strings ("devnet" / "testnet") map to the same Endpoint across both
            // code paths. See RD-856 — an asymmetric URL parse was how the fresh
            // client ended up dialing the wrong hostname in the first place.
            let ep = crate::miden_client::parse_node_url(&node_url)
                .map_err(|e| anyhow::anyhow!("invalid node URL {node_url}: {e}"))?;
            tracing::info!(
                node_url = %node_url,
                resolved = %ep,
                "publish_claim: building fresh Miden client to dial node"
            );
            let mut client = ClientBuilder::new()
                .grpc_client(&ep, Some(10_000))
                .sqlite_store(store_path)
                .authenticator(keystore)
                .in_debug_mode(DebugMode::Enabled)
                .build()
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "publish_claim: failed to build Miden client for {node_url}: {e}"
                    )
                })?;
            client.sync_state().await?;

            let value = publish_claim_internal(
                params,
                &mut client,
                &accounts_inner,
                &*store_clone,
                latest_block_num,
                reject_zero_padding,
                expected_mints_clone.as_ref(),
            )
            .await?;

            // Record the ClaimEvent — cancellation-safe.
            // Race-safe ordering: write the txn+log at (current_latest + 1)
            // BEFORE bumping `latest_block_number`. See `bridge_out.rs::on_post_sync`
            // for the SIGPIPE/cursor-advance rationale.
            let block_num = store_clone.get_latest_block_number().await? + 1;
            let block_hash = block_state.get_block_hash(block_num);
            store_clone
                .txn_begin(
                    txn_hash,
                    crate::store::TxnEntry {
                        id: Some(value.txn_id),
                        envelope: txn_envelope,
                        signer,
                        expires_at: Some(value.expires_at),
                        logs: vec![value.log.clone()],
                    },
                )
                .await?;
            store_clone
                .txn_commit(txn_hash, Ok(()), block_num, block_hash)
                .await?;
            store_clone.set_latest_block_number(block_num).await?;
            tracing::info!(
                eth_tx = %txn_hash,
                block_num,
                "ClaimEvent recorded (cancellation-safe)"
            );

            let _ = result_inner.set(value);
            Ok::<_, anyhow::Error>(())
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("claim spawn_blocking: {e}"))?;

    join_result?;

    result
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("publish_claim: closure completed but result was not set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::seed_test_faucets;
    use alloy::primitives::address;

    #[test]
    fn test_metadata_hash_empty() {
        // Empty metadata → keccak256("") → 0xc5d246...a470. This is what the L1 bridge
        // contract puts in `leaf_data.metadata_hash` for native ETH deposits.
        let hash = MetadataHash::from_abi_encoded(&[]);
        let expected =
            hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
                .unwrap();
        assert_eq!(hash.as_bytes(), expected.as_slice());
    }

    #[tokio::test]
    async fn test_find_faucet_eth_from_store() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        let entry = store
            .get_faucet_by_origin(&[0u8; 20], 0)
            .await
            .unwrap()
            .expect("ETH faucet should be registered");
        assert_eq!(entry.origin_decimals, 18);
        assert_eq!(entry.miden_decimals, 8);
        assert_eq!(entry.symbol, "ETH");
    }

    #[tokio::test]
    async fn test_find_faucet_unknown_returns_none() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        // Address not registered in the test seed
        let entry = store.get_faucet_by_origin(&[0xBB; 20], 0).await.unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn test_metadata_hash_non_empty() {
        // Non-empty raw bytes → keccak256(bytes). Sanity check that
        // `MetadataHash::from_abi_encoded` is just keccak256, not ABI-aware.
        let hash = MetadataHash::from_abi_encoded(&[0x01, 0x02, 0x03]);
        let expected =
            hex::decode("f1885eda54b7a053318cd41e2093220dab15d65381b1157a3633a83bfd5c9239")
                .unwrap();
        assert_eq!(hash.as_bytes(), expected.as_slice());
    }

    #[test]
    fn test_claim_event_from_claim_asset_call() {
        use alloy::primitives::{Address, U256};

        let call = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(42u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 1,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: address!("1234567890abcdef1234567890abcdef12345678"),
            amount: U256::from(1000u64),
            metadata: Default::default(),
        };
        let event = ClaimEvent::from(call);
        assert_eq!(event.globalIndex, U256::from(42u64));
        assert_eq!(event.originNetwork, 1);
        assert_eq!(event.amount, U256::from(1000u64));
    }

    #[test]
    fn test_bytes32_array_to_smt_nodes_converts() {
        let mut values = [FixedBytes::ZERO; 32];
        values[0] = FixedBytes::from([0xAA; 32]);
        values[31] = FixedBytes::from([0xBB; 32]);
        let nodes = bytes32_array_to_smt_nodes(values);
        // Verify we get back 32 nodes (basic structural check)
        assert_eq!(nodes.len(), 32);
    }

    mod scale_claim_amount {
        use super::*;
        use alloy::primitives::U256;
        use miden_protocol::Felt;
        use miden_protocol::account::AccountId;
        use std::ops::{Add, Mul};

        const DUMMY_ACCOUNT_HEX: &str = "0x3d7c9747558851900f8206226dfbea";

        fn faucet(origin_decimals: u8, miden_decimals: u8) -> Faucet {
            Faucet {
                id: AccountId::from_hex(DUMMY_ACCOUNT_HEX).unwrap(),
                decimals: miden_decimals,
                origin_token_decimals: origin_decimals,
            }
        }

        fn eth_amount(wei: U256) -> EthAmount {
            EthAmount::new(wei.to_be_bytes::<32>())
        }

        #[test]
        fn accepts_amount_above_old_u32_ceiling() {
            let wei = U256::from(43u64).mul(U256::from(10u64).pow(U256::from(18u64)));
            let amount = scale_claim_amount(&eth_amount(wei), faucet(18, 8)).unwrap();
            assert_eq!(amount, Felt::try_from(4_300_000_000u64).unwrap());
        }

        #[test]
        fn truncates_sub_unit_wei_remainder() {
            let wei = U256::from(42u64)
                .mul(U256::from(10u64).pow(U256::from(18u64)))
                .add(U256::from(1u64));
            let amount = scale_claim_amount(&eth_amount(wei), faucet(18, 8)).unwrap();
            assert_eq!(amount, Felt::try_from(4_200_000_000u64).unwrap());
        }

        #[test]
        fn rejects_amount_above_max_fungible_asset() {
            let err = scale_claim_amount(&eth_amount(U256::MAX), faucet(18, 8)).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("claim amount is not representable on Miden"),
                "unexpected error message: {msg}"
            );
        }

        /// Cantina #12 — repro+regression. The on-chain MASM
        /// `verify_u256_to_native_amount_conversion` advertises a 2^128 outer gate
        /// but the inner verifier algebra only succeeds for x < ~2^123; values in
        /// [2^123, 2^128) panic later with `ERR_UNDERFLOW`. Aggkit's scaling path
        /// goes through `EthAmount::scale_to_token_amount` which enforces the
        /// real protocol cap (`FungibleAsset::MAX_AMOUNT = 2^63 - 2^31`), so any
        /// amount that falls in the upstream gap is rejected here BEFORE we
        /// build a CLAIM note that would panic on Miden. This test pins that
        /// boundary so a future regression that loosens the cap (e.g. switches
        /// to a tighter or looser `try_from` path) is caught immediately.
        #[test]
        fn cantina_12_amount_cap_pins_fungible_asset_max() {
            use miden_client::asset::FungibleAsset;

            // Boundary: an amount that scales to exactly MAX_AMOUNT must succeed.
            let max_native = FungibleAsset::MAX_AMOUNT; // 2^63 - 2^31
            // For an 18→8 decimal layout, scale = 10. Pre-image wei = max_native * 10^10.
            let wei_at_max = U256::from(max_native).mul(U256::from(10u64).pow(U256::from(10u64)));
            let amount = scale_claim_amount(&eth_amount(wei_at_max), faucet(18, 8))
                .expect("exact MAX_AMOUNT must be accepted");
            assert_eq!(amount, Felt::try_from(max_native).unwrap());

            // Off-by-one above MAX_AMOUNT must be rejected.
            let wei_just_over =
                U256::from(max_native + 1).mul(U256::from(10u64).pow(U256::from(10u64)));
            assert!(
                scale_claim_amount(&eth_amount(wei_just_over), faucet(18, 8)).is_err(),
                "MAX_AMOUNT + 1 must be rejected"
            );

            // An amount that would fall in the upstream MASM's [2^123, 2^128) gap
            // must also be rejected here. With scale=10, even 2^123 wei scales to
            // 2^123 / 10^10 ≈ 2^90, well above MAX_AMOUNT (2^63 - 2^31), so we
            // catch it before any MASM path could panic.
            let wei_in_gap = U256::from(1u64) << 123;
            assert!(
                scale_claim_amount(&eth_amount(wei_in_gap), faucet(18, 8)).is_err(),
                "2^123 wei must be rejected client-side (Cantina #12 gap)"
            );
        }

        #[test]
        fn passes_through_when_decimals_match() {
            let wei = U256::from(1_234_567u64);
            let amount = scale_claim_amount(&eth_amount(wei), faucet(6, 6)).unwrap();
            assert_eq!(amount, Felt::try_from(1_234_567u64).unwrap());
        }

        #[test]
        fn rejects_faucet_with_inverted_decimals() {
            let err = scale_claim_amount(&eth_amount(U256::from(1u64)), faucet(6, 8)).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("invariant violated"),
                "unexpected error: {msg}"
            );
        }
    }

    /// Cantina #11 — repro+regression. The on-chain CLAIM verifier's mainnet branch
    /// does not constrain `smt_proof_rollup_exit_root` (256 felts) or `rollup_exit_root`
    /// (8 felts) — any garbage prover supplies still folds into the note's RECIPIENT
    /// digest and PROOF_DATA_KEY, so equivalent mainnet claims with different rollup-side
    /// bytes produce different NoteIds. Aggkit canonicalises by zeroing those fields
    /// when the globalIndex's mainnet flag is set.
    mod cantina_11_canonical_mainnet_proof_data {
        use super::*;
        use alloy::primitives::{Address, Bytes, FixedBytes, U256};

        /// Build a claimAssetCall with a chosen mainnet flag and rollup-side garbage.
        fn make_call(mainnet: bool, rollup_garbage_byte: u8) -> claimAssetCall {
            // GlobalIndex layout: bytes 0..20 zero, bytes 20..24 mainnet flag (BE),
            // bytes 24..28 rollup index, bytes 28..32 leaf index.
            let mut gi = [0u8; 32];
            if mainnet {
                gi[23] = 1; // BE-low byte of the flag word
            } // else flag stays 0 = rollup
            gi[31] = 42; // leaf index = 42

            let smt_local: [FixedBytes<32>; 32] =
                std::array::from_fn(|i| FixedBytes([i as u8; 32]));
            let smt_rollup: [FixedBytes<32>; 32] =
                std::array::from_fn(|_| FixedBytes([rollup_garbage_byte; 32]));

            claimAssetCall {
                smtProofLocalExitRoot: smt_local,
                smtProofRollupExitRoot: smt_rollup,
                globalIndex: U256::from_be_bytes(gi),
                mainnetExitRoot: FixedBytes([0xAAu8; 32]),
                rollupExitRoot: FixedBytes([rollup_garbage_byte; 32]),
                originNetwork: 0,
                originTokenAddress: Address::ZERO,
                destinationNetwork: 1,
                destinationAddress: Address::ZERO,
                amount: U256::from(0u64),
                metadata: Bytes::new(),
            }
        }

        #[test]
        fn mainnet_claim_zeroes_rollup_proof_path_only() {
            let call = make_call(true, 0xCC);
            let proof = build_canonical_proof_data(&call);

            let zero_node = SmtNode::new([0u8; 32]);
            for n in proof.smt_proof_rollup_exit_root.iter() {
                assert_eq!(*n, zero_node, "mainnet smt_proof_rollup must be zeroed");
            }
            // rollup_exit_root MUST be preserved — it's read by `compute_ger` in
            // bridge_in.masm to derive the GER lookup key. Zeroing it broke the
            // dynamic-ERC20 e2e with ERR_GER_NOT_FOUND. See claim.rs docstring.
            assert_eq!(
                proof.rollup_exit_root,
                ExitRoot::new([0xCCu8; 32]),
                "mainnet rollup_exit_root must NOT be zeroed (load-bearing for compute_ger)"
            );
            // Mainnet exit root is preserved.
            assert_eq!(proof.mainnet_exit_root, ExitRoot::new([0xAAu8; 32]));
        }

        #[test]
        fn mainnet_claim_note_id_invariant_to_smt_proof_garbage() {
            // Two mainnet claims for the same leaf, but different smt_proof_rollup
            // garbage AND identical real (mainnet,rollup)-exit-root pair. Post-fix
            // the canonicalised ProofData must be byte-identical wrt the SMT proof
            // path (the only field genuinely unread by the bridge's mainnet branch
            // and therefore safely zeroable for NoteId determinism).
            //
            // We CANNOT canonicalise rollup_exit_root because it's used in the GER
            // keccak; the test pins the now-correct subset of the determinism
            // property.
            let mut call_a = make_call(true, 0x00);
            let mut call_b = make_call(true, 0xFF);
            // Force rollup_exit_root to match for both — this is what real claims
            // look like (the L1 GER manager dictates the value).
            call_a.rollupExitRoot = FixedBytes([0x33u8; 32]);
            call_b.rollupExitRoot = FixedBytes([0x33u8; 32]);

            let a = build_canonical_proof_data(&call_a);
            let b = build_canonical_proof_data(&call_b);
            assert_eq!(
                a.smt_proof_rollup_exit_root, b.smt_proof_rollup_exit_root,
                "rollup smt_proof must be canonical for mainnet (zeroed)"
            );
            assert_eq!(
                a.rollup_exit_root, b.rollup_exit_root,
                "rollup_exit_root must be preserved verbatim (=L1 GER manager value)"
            );
        }

        /// Regression for the dynamic-ERC20 e2e fix. ERR_GER_NOT_FOUND fired
        /// because the original canonicalisation zeroed `rollup_exit_root` for
        /// mainnet claims, but `compute_ger` in `bridge_in.masm` keccaks
        /// `mainnet_exit_root || rollup_exit_root` to derive the GER lookup key.
        /// This test pins that the canonicalised proof_data preserves whatever
        /// `rollup_exit_root` the caller supplied — including non-zero values
        /// from a live L1 GER manager.
        #[test]
        fn mainnet_claim_preserves_nonzero_rollup_exit_root_for_ger_lookup() {
            let mut call = make_call(true, 0);
            call.rollupExitRoot = FixedBytes([0x77u8; 32]); // simulates live L1
            let proof = build_canonical_proof_data(&call);
            assert_eq!(
                proof.rollup_exit_root,
                ExitRoot::new([0x77u8; 32]),
                "rollup_exit_root must reach the bridge unchanged (else ERR_GER_NOT_FOUND)"
            );
        }

        /// Self-review of-the-fix follow-up — the original Cantina #11 fix
        /// preserved the full `globalIndex` u256, including bytes 24..28 (limb
        /// 6 = rollup index). A malicious caller setting *both* the mainnet
        /// flag AND non-zero rollup-index bytes could still produce different
        /// NoteIds for the same mainnet leaf. This test pins the tightening:
        /// the rollup-index bytes are zeroed when the mainnet flag is set.
        #[test]
        fn mainnet_claim_zeroes_rollup_index_bytes_in_global_index() {
            // Build two mainnet claims where the rollup-index bytes (24..28)
            // differ, everything else identical.
            let mut gi_a = [0u8; 32];
            gi_a[23] = 1; // mainnet flag
            gi_a[24] = 0xAA; // attacker-supplied rollup-index garbage
            gi_a[31] = 42;

            let mut gi_b = gi_a;
            gi_b[24] = 0xBB;
            gi_b[25] = 0xCC;

            let mut call_a = make_call(true, 0);
            call_a.globalIndex = U256::from_be_bytes(gi_a);
            let mut call_b = make_call(true, 0);
            call_b.globalIndex = U256::from_be_bytes(gi_b);

            let a = build_canonical_proof_data(&call_a);
            let b = build_canonical_proof_data(&call_b);

            // After canonicalisation the GlobalIndex bytes must match — the
            // rollup-index garbage was zeroed for both.
            assert_eq!(
                a.global_index, b.global_index,
                "globalIndex rollup-index bytes must be zeroed for mainnet"
            );
        }

        #[test]
        fn rollup_claim_preserves_rollup_proof() {
            // Non-mainnet claims must NOT be canonicalised — those fields are load-bearing
            // for rollup-leaf verification AND the GER keccak.
            let call = make_call(false, 0xCC);
            let proof = build_canonical_proof_data(&call);

            let cc_node = SmtNode::new([0xCCu8; 32]);
            for n in proof.smt_proof_rollup_exit_root.iter() {
                assert_eq!(*n, cc_node, "rollup smt_proof must be preserved verbatim");
            }
            assert_eq!(proof.rollup_exit_root, ExitRoot::new([0xCCu8; 32]));
        }

        #[test]
        fn is_mainnet_global_index_decodes_layout() {
            let mut gi = [0u8; 32];
            assert!(!is_mainnet_global_index(&gi), "all-zero is rollup");
            gi[23] = 1;
            assert!(is_mainnet_global_index(&gi), "flag at byte 23 → mainnet");
            // Garbage outside the flag bytes must not flip the result.
            gi[20] = 0;
            gi[21] = 0;
            gi[22] = 0;
            gi[24] = 0xFF; // rollup index garbage
            gi[31] = 0xFF; // leaf index garbage
            assert!(is_mainnet_global_index(&gi));
            // Flag = 2 is technically out of spec but our decoder must only treat 1 as mainnet.
            gi[23] = 2;
            assert!(!is_mainnet_global_index(&gi), "flag must be exactly 1");
        }
    }
}
