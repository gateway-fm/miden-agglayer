use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::{FixedBytes, LogData, TxHash};
use alloy::sol_types::SolEvent;
use alloy_rpc_types_eth::TransactionRequest;
use miden_base_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::store::NoteFilter;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::note::NoteId;
use sha3::{Digest, Keccak256};
use std::sync::Arc;
use std::time::Duration;

alloy_core::sol! {
    #[derive(Debug)]
    interface IGlobalExitRootV2 {
        function lastMainnetExitRoot() external view returns (bytes32);
        function lastRollupExitRoot() external view returns (bytes32);
    }
}

/// Read the individual exit roots from the L1 GER contract.
pub async fn fetch_l1_exit_roots(
    l1_rpc_url: &str,
    ger_address: &str,
) -> anyhow::Result<([u8; 32], [u8; 32])> {
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::sol_types::SolCall;

    let provider = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let ger_addr: alloy::primitives::Address = ger_address.parse()?;

    let mainnet_call = IGlobalExitRootV2::lastMainnetExitRootCall {};
    let mainnet_result = provider
        .call(
            TransactionRequest::default()
                .to(ger_addr)
                .input(mainnet_call.abi_encode().into()),
        )
        .await?;
    let mainnet_root: [u8; 32] = mainnet_result[..32].try_into()?;

    let rollup_call = IGlobalExitRootV2::lastRollupExitRootCall {};
    let rollup_result = provider
        .call(
            TransactionRequest::default()
                .to(ger_addr)
                .input(rollup_call.abi_encode().into()),
        )
        .await?;
    let rollup_root: [u8; 32] = rollup_result[..32].try_into()?;

    Ok((mainnet_root, rollup_root))
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L166
    #[derive(Debug)]
    function insertGlobalExitRoot(bytes32 root);
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L131
    #[derive(Debug)]
    function updateExitRoot(bytes32 newRollupExitRoot, bytes32 newMainnetExitRoot);
}

/// Compute the combined GER from mainnet and rollup exit roots.
pub fn combined_ger(mainnet: &[u8; 32], rollup: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(mainnet);
    hasher.update(rollup);
    hasher.finalize().into()
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L52
    #[derive(Debug)]
    event UpdateHashChainValue(
        bytes32 indexed newGlobalExitRoot,
        bytes32 indexed newHashChainValue
    );
}

impl UpdateHashChainValue {
    fn new(ger: FixedBytes<32>, chain_hash: FixedBytes<32>) -> Self {
        UpdateHashChainValue {
            newGlobalExitRoot: ger,
            newHashChainValue: chain_hash,
        }
    }
}

/// Result of a GER insertion.
pub struct GerInsertResult {
    pub log_data: LogData,
    pub block_number: u64,
    pub is_new: bool,
}

/// How long `submit_update_ger_note` waits for the bridge account to CONSUME
/// the freshly-created public `UpdateGerNote` before giving up. The note is a
/// `NetworkAccountTarget` note (see `miden-agglayer/src/update_ger_note.rs`):
/// the Miden node's network-transaction (NTX) builder is the party that
/// consumes it into the bridge account's GER storage — typically within
/// ~2-3 blocks (~5s). We poll up to 30×1s so a slow NTX cycle still resolves
/// before we declare the GER visible.
const GER_CONSUME_MAX_ATTEMPTS: usize = 30;
const GER_CONSUME_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Poll the local Miden store for the given note appearing as **consumed**.
///
/// Cantina MA#9 / MA#21 — authoritative bridge-side readiness signal. An
/// `UpdateGerNote` only takes effect once the bridge account consumes it (the
/// node NTX builder runs `update_ger`, writing the GER into bridge storage).
/// Until then the GER is NOT actually claimable, even though the creator tx
/// has committed. `NoteFilter::Consumed` surfaces the note in a consumed state
/// — including `ConsumedExternal`, which is exactly the state a public note
/// reaches once an account *we don't drive locally* (the network bridge
/// account) spends it. Matching on the specific `NoteId` proves THIS GER's
/// note was consumed, not merely that some GER note was.
///
/// Each attempt syncs first so newly-observed nullifiers (the bridge's
/// consumption) land in the local store before we scan. Returns `Ok(true)`
/// once the note is observed consumed, `Ok(false)` on timeout.
async fn wait_for_ger_note_consumed(
    client: &mut MidenClientLib,
    note_id: NoteId,
    max_attempts: usize,
    poll_interval: Duration,
) -> anyhow::Result<bool> {
    for _ in 0..max_attempts {
        // Sync first so the bridge's consumption (a nullifier on a public note
        // we created but do not own) is pulled into the local store before we
        // scan. The persistent client's cursor advances monotonically, so the
        // consumption block is always inside [cursor, tip] on a later poll.
        client.sync_state().await?;
        let consumed = client.get_input_notes(NoteFilter::Consumed).await?;
        if note_consumed_in(consumed.iter().map(|n| n.id()), note_id) {
            return Ok(true);
        }
        tokio::time::sleep(poll_interval).await;
    }
    Ok(false)
}

/// Pure predicate: does `note_id` appear among the ids of the consumed notes?
///
/// Cantina MA#9 / MA#21 — the bridge-consumption readiness check matches on the
/// SPECIFIC `UpdateGerNote` id, not on "some GER note is consumed". Factored
/// out of `wait_for_ger_note_consumed` so the matching rule is unit-testable
/// without a live Miden node.
fn note_consumed_in(consumed_ids: impl IntoIterator<Item = NoteId>, note_id: NoteId) -> bool {
    consumed_ids.into_iter().any(|id| id == note_id)
}

/// Submit the actual UpdateGerNote Miden transaction AND wait for the bridge
/// account to consume it. Factored out of `insert_ger` so the caller can run
/// it twice — once eagerly, then again after `reimport_account` if the first
/// attempt failed with a recoverable account-state error.
///
/// Use the long-lived MidenClient. The dedicated ger_manager account
/// (separate from the service account that the NTX builder constantly
/// mutates via claim processing) keeps the account state stable across
/// GER submissions, so we don't need a fresh client per call.
///
/// Fresh-client-per-GER was removed because it shared the main sqlite
/// and advanced the sync cursor past blocks where bridge NTX consumes
/// the UpdateGerNote. The main client's subsequent sync_nullifiers only
/// queries [current_cursor, tip], so those consumption events were never
/// discovered and `NoteFilter::Consumed` returned nothing in restore.
///
/// Cantina MA#9 / MA#21 — this function now returns only once the bridge has
/// CONSUMED the note (observed via `NoteFilter::Consumed`), not merely once the
/// creator tx committed. We deliberately do NOT submit a second transaction
/// against the bridge account to force consumption (the auditor's literal
/// sketch): the bridge is an `AccountStorageMode::Network` account, and the
/// Miden node RPC rejects any post-deployment user-submitted transaction
/// against a network account with "Network transactions may not be submitted
/// by users yet" (miden-node `crates/rpc/src/server/api.rs:255` —
/// `reject_if_any_network_accounts`, gated on the ntx-builder auth header the
/// service does not hold). Consuming network notes is structurally the node
/// NTX builder's job. So we instead OBSERVE the NTX builder's consumption and
/// gate readiness on it, which is the implementable form of the
/// "wait for note consumption, not just creator-tx commit" recommendation.
async fn submit_update_ger_note(
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    ger_bytes: [u8; 32],
) -> anyhow::Result<()> {
    let inner_accounts = accounts.0.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                let ger_manager_id = inner_accounts
                    .ger_manager
                    .as_ref()
                    .map(|a| a.0)
                    .unwrap_or(inner_accounts.service.0);
                let bridge_id = inner_accounts.bridge.0;
                let ger = ExitRoot::new(ger_bytes);
                let note = UpdateGerNote::create(ger, ger_manager_id, bridge_id, client.rng())?;
                let note_id = note.id();
                tracing::info!(
                    note_id = %note_id,
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote created"
                );
                let tx_request = TransactionRequestBuilder::new()
                    .own_output_notes(vec![note])
                    .build()?;
                let tx_id = crate::metrics::meter_proof(
                    crate::metrics::ProofKind::Ger,
                    client.submit_new_transaction(ger_manager_id, tx_request),
                )
                .await?;
                tracing::info!(
                    tx_id = %tx_id,
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote submitted, waiting for commit..."
                );

                let committed = crate::miden_client::wait_for_transaction_commit(
                    client,
                    tx_id,
                    30,
                    std::time::Duration::from_secs(1),
                )
                .await?;
                if !committed {
                    anyhow::bail!("UpdateGerNote tx {tx_id} not committed after 30s");
                }
                tracing::info!(tx_id = %tx_id, "UpdateGerNote transaction committed");

                // Cantina MA#9 / MA#21 — the creator tx committing only means
                // the note now EXISTS on-chain; the bridge has not necessarily
                // consumed it yet. Block here until the bridge account
                // consumes the note (NTX builder runs `update_ger`), so that
                // by the time `insert_ger` advances `is_injected` / publishes
                // the synthetic UpdateHashChainValue log, the GER is genuinely
                // claimable. This is the readiness wait that USED to live in
                // the claim hot path (`publish_claim_internal`'s 15s sleep
                // loop); moving it here means every claim can assume the GER
                // is already injected.
                let consumed = wait_for_ger_note_consumed(
                    client,
                    note_id,
                    GER_CONSUME_MAX_ATTEMPTS,
                    GER_CONSUME_POLL_INTERVAL,
                )
                .await?;
                if !consumed {
                    anyhow::bail!(
                        "UpdateGerNote {note_id} created but not consumed by bridge after {}s; \
                         GER not yet claimable",
                        GER_CONSUME_MAX_ATTEMPTS
                    );
                }
                tracing::info!(
                    note_id = %note_id,
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote consumed by bridge — GER injected and claimable"
                );
                Ok(())
            })
        })
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    mainnet_exit_root: Option<[u8; 32]>,
    rollup_exit_root: Option<[u8; 32]>,
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    block_state: &Arc<crate::block_state::BlockState>,
    txn_hash: TxHash,
) -> anyhow::Result<GerInsertResult> {
    // Check dedup before doing any work.
    //
    // Use `is_ger_injected` (not `has_seen_ger`) because the L1InfoTreeIndexer
    // pre-creates ger_entries rows for every L1 InfoTree pair as it observes
    // them, even before the corresponding Miden inject happens. With
    // `has_seen_ger` we'd skip the actual Miden tx submission as a "duplicate"
    // and the synthetic L2 event would never be emitted, leaving deposits
    // stuck `ready_for_claim=false`. Gating on `is_injected = TRUE` correctly
    // reflects "have we already submitted the Miden tx and committed the
    // synthetic event for this GER?".
    let is_new = !store.is_ger_injected(&ger_bytes).await?;

    let mut block_number = 0u64; // assigned by store.advance_block_number() after Miden commit

    if is_new {
        tracing::info!(
            ger = %hex::encode(ger_bytes),
            "GER injection: submitting to Miden..."
        );

        // Submit with runtime self-heal: if the Miden submission rejects
        // with AccountDataNotFound (local sqlite missing the account row)
        // OR IncorrectAccountInitialCommitment (local commitment stale vs
        // the node's view), reimport the ger_manager account from the
        // live Miden node and retry once. See `src/account_recovery.rs`
        // for the analysis — this is the actual bali production cure.
        match submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await {
            Ok(()) => {}
            Err(err) if crate::account_recovery::is_recoverable_account_error(&err) => {
                tracing::warn!(
                    err = %err,
                    ger = %hex::encode(ger_bytes),
                    "GER injection: recoverable account error, reimporting ger_manager and retrying"
                );
                let ger_manager_id = accounts
                    .0
                    .ger_manager
                    .as_ref()
                    .map(|a| a.0)
                    .unwrap_or(accounts.0.service.0);
                crate::account_recovery::reimport_account(
                    miden_client,
                    ger_manager_id,
                    "ger_manager",
                )
                .await?;
                submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await?;
            }
            Err(err) => return Err(err),
        }

        // Race-safe ordering: write the log at (current_latest + 1) BEFORE
        // bumping `latest_block_number`. See the matching comment in
        // `bridge_out.rs::on_post_sync`: if we advance the counter first,
        // aggsender / bridge-service can poll `eth_blockNumber` in the window
        // where `latest == N` but the log at block `N` hasn't been written yet,
        // permanently skipping the GER event.
        block_number = store.get_latest_block_number().await? + 1;
        let block_hash = block_state.get_block_hash(block_number);
        let timestamp = block_state.get_block_timestamp(block_number);

        // Miden submission AND bridge consumption succeeded — now record the
        // event. Cantina MA#9 — `submit_update_ger_note` only returns `Ok`
        // after the bridge account has CONSUMED the UpdateGerNote
        // (`NoteFilter::Consumed`), so reaching this point proves the GER is
        // genuinely injected into bridge storage and claimable. The synthetic
        // visibility state (`ger_entries`, `is_injected`, the
        // UpdateHashChainValue log) is therefore advanced only after
        // authoritative bridge-side consumption, never on the creator-tx
        // commit alone — closing the false-positive early-visibility half of
        // MA#9.
        //
        // G5 — single atomic store transaction. Replaces the previous
        // three sequential calls (add_ger_update_event,
        // mark_ger_injected, set_latest_block_number) which were not
        // atomic: a process crash between any two left aggkit in a
        // split state. The PgStore override folds all five writes
        // (ger_entries upsert, hash_chain UPDATE, synthetic_logs
        // INSERT, is_injected UPDATE, latest_block_number UPDATE) into
        // one SERIALIZABLE postgres transaction. InMemoryStore uses the
        // default trait impl that just calls the primitives in sequence
        // (safe in-process; no crash window for tests).
        //
        // Supersedes G4's narrowing of the gap.
        let tx_hash_str = format!("{txn_hash:#x}");
        store
            .commit_ger_event_atomic(
                block_number,
                block_hash,
                &tx_hash_str,
                &ger_bytes,
                mainnet_exit_root,
                rollup_exit_root,
                timestamp,
            )
            .await?;
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    let event = UpdateHashChainValue::new(FixedBytes::from(ger_bytes), FixedBytes::default());
    let log_data = event.encode_log_data();

    Ok(GerInsertResult {
        log_data,
        block_number,
        is_new,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_combined_ger_keccak256() {
        let mainnet = [0x01u8; 32];
        let rollup = [0x02u8; 32];
        let result = combined_ger(&mainnet, &rollup);

        // Verify against direct keccak256 computation
        let mut hasher = Keccak256::new();
        hasher.update(mainnet);
        hasher.update(rollup);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_combined_ger_deterministic() {
        let mainnet = [0xAAu8; 32];
        let rollup = [0xBBu8; 32];
        assert_eq!(
            combined_ger(&mainnet, &rollup),
            combined_ger(&mainnet, &rollup)
        );
    }

    #[test]
    fn test_combined_ger_order_matters() {
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];
        assert_ne!(combined_ger(&a, &b), combined_ger(&b, &a));
    }

    // Two distinct, deterministic NoteIds with no node / RNG dependency.
    const NOTE_A: &str = "0xc9d31c82c098e060c9b6e3af2710b3fc5009a1a6f82ef9465f8f35d1f5ba4a80";
    const NOTE_B: &str = "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";

    fn note_id(hex: &str) -> NoteId {
        NoteId::try_from_hex(hex).expect("valid note id hex")
    }

    /// Cantina MA#9 / MA#21 — the bridge-consumption readiness check matches on
    /// the SPECIFIC UpdateGerNote id. A different GER note appearing in the
    /// consumed set must NOT be mistaken for THIS GER being injected; that is
    /// exactly the false-positive early-visibility class MA#9 describes.
    #[test]
    fn ma9_note_consumed_in_matches_only_exact_id() {
        let target = note_id(NOTE_A);
        let other = note_id(NOTE_B);

        // Empty consumed set: not yet consumed — readiness must be false.
        assert!(!note_consumed_in(std::iter::empty(), target));

        // Only some OTHER note is consumed: still not THIS GER.
        assert!(!note_consumed_in([other], target));

        // The exact note appears (alongside an unrelated one): consumed.
        assert!(note_consumed_in([other, target], target));
        assert!(note_consumed_in([target], target));
    }

    /// Cantina MA#9 — pin the invariant that GER visibility is produced ONLY by
    /// `commit_ger_event_atomic` (the call `insert_ger` now performs strictly
    /// AFTER `submit_update_ger_note` has observed bridge consumption). Before
    /// that call the store reports the GER as not-injected, no synthetic block
    /// is advanced, and no UpdateHashChainValue log exists; after it, all three
    /// flip together. Gating that single call behind observed consumption is
    /// what makes "advance ger_entries / is_injected / synthetic log only after
    /// the note is consumed" hold end-to-end.
    #[tokio::test]
    async fn ma9_visibility_only_after_atomic_commit() {
        use crate::log_synthesis::LogFilter;
        use crate::store::Store;
        use crate::store::memory::InMemoryStore;

        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let ger = [0xABu8; 32];

        // BEFORE consumption is confirmed, `insert_ger` would NOT have called
        // `commit_ger_event_atomic` (it bails on a non-consumed note). The
        // store must therefore show the GER as invisible.
        assert!(
            !store.is_ger_injected(&ger).await.unwrap(),
            "GER must not be visible before bridge consumption"
        );
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            0,
            "no synthetic block advanced before consumption"
        );

        // AFTER consumption is observed, `insert_ger` advances all visibility
        // state in one atomic commit at block N = latest + 1.
        let block_number = store.get_latest_block_number().await.unwrap() + 1;
        store
            .commit_ger_event_atomic(
                block_number,
                [0u8; 32],
                "0xdeadbeef",
                &ger,
                Some([0x11u8; 32]),
                Some([0x22u8; 32]),
                1_700_000_000,
            )
            .await
            .unwrap();

        assert!(
            store.is_ger_injected(&ger).await.unwrap(),
            "GER visible only after the post-consumption atomic commit"
        );
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            block_number,
            "synthetic block advanced exactly once, after consumption"
        );
        // Default filter resolves from/to to `current_block`, so this scans
        // exactly the synthetic block the commit advanced to.
        let logs = store
            .get_logs(&LogFilter::default(), block_number)
            .await
            .unwrap();
        assert!(
            !logs.is_empty(),
            "UpdateHashChainValue synthetic log published with the GER commit"
        );
    }
}
