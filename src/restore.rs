//! Restore — rebuild PostgreSQL state (optionally after `--reset-miden-store`)
//! by driving the same `SyntheticProjector` the live scheduler runs, pinned to
//! a captured tip. There is deliberately no second projection engine (issue
//! #167): a rarely exercised recovery path must not be able to diverge from the
//! continuously exercised one. This module only orchestrates: reimport
//! accounts, reset cursors, catch up, verify, finalize.
//!
//! ## GER restoration via consumed notes
//!
//! For recovery we only care about consumed notes — actually injected GERs.
//! When the proxy injects a GER, it creates an UpdateGerNote that gets consumed
//! by the Miden bridge account; the projector's note-visibility sweep re-imports
//! those public notes from genesis and the shared per-block unit
//! (`crate::projection`) re-emits their events at the original consumption
//! blocks, folding the hash chain in canonical order.
//!
//! See: https://github.com/0xMiden/protocol/issues/2341
//!
//! ## Known Limitations (TODOs for miden-node API enhancements)
//!
//! - B2AGG/GER note filtering is done client-side (no server-side script root filter)
//!   TODO: switch to NoteFilter::ConsumedByScriptRoot when available
//! - No block range queries for notes (full scan from genesis)
//!   TODO: switch to dedicated get_gers() endpoint when Marti's team ships it
//! - An input the node cannot reconstruct fails the restore rather than being
//!   skipped: a silently divergent history is worse than no restore.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::miden_client::MidenClient;
use crate::store::Store;
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
    /// Faucet-identity rows the projector's bootstrap rebuilt (Cantina #6).
    pub faucet_identities_rebuilt: usize,
    /// Cantina MA#27 — number of consumed CLAIM notes for which a synthetic
    /// ClaimEvent was emitted by restore (the offline equivalent of what the
    /// live [`SyntheticProjector`](crate::synthetic_projector) does each tick).
    pub claims_restored: usize,
    pub gers_restored: usize,
    pub logs_created: usize,
}
/// Run the full restore algorithm.
// 8 args: the v0.15.4 merge unions our projector-shared params
// (local_network_id, l1_rpc_url) with the release's PRST-4035 node-scan
// params (node_url, api_key). A config struct here would churn every
// call site for a single-caller function; not worth it.
#[allow(clippy::too_many_arguments)]
pub async fn restore(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    local_network_id: u32,
    block_state: &Arc<BlockState>,
    network_rpcs: crate::metadata_recovery::NetworkRpcMap,
    node_url: &str,
    api_key: Option<&str>,
) -> anyhow::Result<RestoreResult> {
    tracing::info!("=== RESTORE: starting state reconstruction ===");

    // Cantina MA#23 — suppress the live `BridgeOutScanner` / `ClaimWatcher`
    // sync-listener callbacks for the entire restore window. The background
    // sync thread inside `MidenClient` keeps pulling deltas (so the local
    // sqlite store stays fresh, which restore phases below depend on), but
    // `on_post_sync` is gated off. Without this guard, the initial sync's
    // listener pass — fired inside `MidenClient::new` BEFORE `restore()`
    // is reached — and every 5s interval tick interleave with restore's
    // own `.with()` calls, causing the live path to also emit synthetic
    // BridgeEvent / ClaimEvent logs and race the deposit-counter cursor.
    // The guard auto-restores on any exit path (Ok / Err / panic).
    let _pause = miden_client.pause_listeners();

    // Phase 0: Re-import every bridge_accounts.toml account from the live
    // Miden node into the local sqlite. Without this, `--reset-miden-store
    // --restore` is a footgun: reset wipes the sqlite, restore's Phase 1
    // calls `sync_state()` which only syncs deltas for already-tracked
    // accounts (not new imports), and the proxy comes back with zero
    // local rows for any account → every subsequent submission fails
    // with `AccountDataNotFound`. This is the regression chain that
    // locked bali into 20 days of stuck deposits after an operator ran
    // the recovery flags.
    //
    // Best-effort: per-account failures are logged + counted but do not
    // abort restore. The locally-deployed-but-not-network-tracked account
    // (`service`) will return `AccountNotFoundOnChain` here and that's
    // fine — it's healthy until first use.
    tracing::info!("Phase 0: re-importing bridge accounts from Miden node...");
    crate::account_recovery::reimport_known_accounts(miden_client, accounts).await;
    tracing::info!("Phase 0 complete: bridge account reimport pass done");

    // Faucet identities (Cantina #6) are rebuilt by the projector itself, in
    // restore posture, before anything depends on them.

    // The snapshot and the whole catch-up run as ONE actor request: the
    // background sync ticker cannot interleave, so the LET gate and every
    // in-session read see the same chain state even on a producing node.
    // Cursors are reset before the projector is constructed because it loads
    // them once in `new()`.
    store.reset_cursors_to_genesis().await?;
    let projector = std::sync::Arc::new(
        crate::synthetic_projector::SyntheticProjector::new(
            store.clone(),
            block_state.clone(),
            accounts,
            local_network_id,
            network_rpcs,
            node_url.to_string(),
            api_key.map(str::to_string),
        )
        .await?,
    );
    tracing::info!("Phase 2: canonical catch-up from cursor zero (blocking, fail-closed)...");
    let snapshot = Arc::new(std::sync::Mutex::new(None::<(u64, u64)>));
    let snapshot_inner = snapshot.clone();
    let projector_for_run = projector.clone();
    let bridge_id = accounts.bridge.0;
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                let miden_tip = client
                    .get_sync_height()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
                    .as_u64();
                let bridge = client
                    .get_account(bridge_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read bridge account {bridge_id}: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("bridge account {bridge_id} is unavailable"))?;
                let let_leaves = miden_base_agglayer::AggLayerBridge::read_let_num_leaves(&bridge);
                *snapshot_inner.lock().expect("restore snapshot") = Some((miden_tip, let_leaves));
                tracing::info!("Phase 1 complete: miden tip {miden_tip}, LET leaves {let_leaves}");

                projector_for_run
                    .catch_up_to(
                        client,
                        miden_tip,
                        crate::synthetic_projector::CatchUpMode::Restore,
                    )
                    .await
                    .map(|_| ())
            })
        })
        .await?;
    let (miden_tip, let_leaves) = snapshot
        .lock()
        .expect("restore snapshot")
        .take()
        .expect("restore snapshot captured");
    let counts = projector.take_projection_counts();
    let faucet_identities_rebuilt = projector.faucet_identities_rebuilt();
    tracing::info!(
        "Phase 2: {faucet_identities_rebuilt} faucet identity row(s) rebuilt by the bootstrap \
         primitive"
    );
    let (bridge_outs, claims, gers) = (counts.bridge_outs, counts.claims, counts.gers);
    let total_logs = bridge_outs + claims + gers;
    tracing::info!(
        "Phase 2 complete: {bridge_outs} bridge-outs, {claims} claims, {gers} GERs — \
         emitted through the canonical per-block projection unit in canonical order"
    );

    let accounted = store.get_accounted_deposit_count().await?;
    if accounted != let_leaves {
        anyhow::bail!(
            "restore LET accounting mismatch after replay: local={accounted}, \
             on-chain={let_leaves}"
        );
    }

    verify_emitted_frontier(store).await?;

    // Phase 4: cursor finalization (factored into a helper so the reconcile-
    // cursor reset is unit-testable — see `finalize_restore_cursors`).
    finalize_restore_cursors(store, miden_tip, Some(miden_tip)).await?;

    // Phase 5: Verify
    tracing::info!("Phase 5: verification");
    tracing::info!("  bridge_outs={bridge_outs}, claims={claims}, gers={gers}, logs={total_logs}");
    tracing::info!("=== RESTORE: complete ===");

    Ok(RestoreResult {
        block_number: miden_tip,
        bridge_outs_restored: bridge_outs,
        faucet_identities_rebuilt,
        claims_restored: claims,
        gers_restored: gers,
        logs_created: total_logs,
    })
}

/// `accounted == let_leaves` proves every leaf is reserved, not emitted;
/// `project_b2agg_note` has post-reservation skip paths. Finalizing over one
/// would ship the permanent `depositCount` gap the live emitted-frontier gate
/// exists to refuse, so the same gate runs before cursor finalization.
pub(crate) async fn verify_emitted_frontier(store: &Arc<dyn Store>) -> anyhow::Result<()> {
    if let Some((idx, note)) = store.first_unemitted_reservation().await? {
        anyhow::bail!(
            "restore: note {note} (LET index {idx}) is reserved but its BridgeEvent was \
             never emitted (quarantined / unrecoverable metadata); refusing to finalize \
             a store with a depositCount gap — repair the leaf's metadata and re-run \
             `--restore`"
        );
    }
    Ok(())
}

/// The synthetic tip and projector cursor go to the Miden tip so the live
/// projector resumes there. The sweep cursor is left where the catch-up's
/// discovery reached, which avoids a second full-history walk on the next
/// boot; if discovery stopped short it falls back to genesis so the serving
/// proxy still heals the gap.
pub(crate) async fn finalize_restore_cursors(
    store: &Arc<dyn Store>,
    miden_tip: u64,
    swept_to: Option<u64>,
) -> anyhow::Result<()> {
    store.set_latest_block_number(miden_tip).await?;
    store.set_projector_cursor(miden_tip).await?;
    tracing::info!("Phase 4: synthetic tip + projector cursor set to Miden tip {miden_tip}");

    // #90 — the `nonces` table has NO chain source, so a rebuilt store starts with
    // an EMPTY nonce ledger while CONTINUING signers (aggkit's aggoracle/aggsender
    // wallets) keep submitting from where they left off. Without this marker the
    // admission path sees `expected 0` vs `tx.nonce N`, parks the tx in the
    // future-nonce queue, and waits for a nonce that can never be submitted — on
    // the 2026-08-11 gate the GER injector spun 34,287 times in ~30min and
    // injection never resumed. Record that the ledger was rebuilt so admission can
    // adopt each signer's first observed nonce as its baseline instead of
    // demanding 0.
    store.set_nonce_ledger_rebuilt(true).await?;
    tracing::warn!(
        "Phase 4: nonce ledger is EMPTY after restore — flagged for first-contact \
         bootstrap (#90); ordinary R4 ordering resumes per signer once seeded"
    );

    match swept_to {
        Some(t) if t >= miden_tip => {
            store.set_reconcile_cursor(t).await?;
            tracing::info!(
                swept_to = t,
                "reconcile cursor left at the swept tip — discovery already reached it; \
                 no redundant genesis re-walk on the next boot"
            );
        }
        other => {
            store.set_reconcile_cursor(0).await?;
            tracing::warn!(
                swept_to = ?other,
                "reconcile cursor reset to genesis — discovery did not reach the tip, so the \
                 serving proxy must re-sweep"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_address::get_bridge_address;
    use crate::claim_watcher::derive_manual_claim_tx_hash;
    use crate::store::Store;
    use crate::store::memory::InMemoryStore;

    use std::sync::Arc as StdArc;

    /// A reserved-but-unemitted leaf must fail finalization, not ship as a
    /// permanent `depositCount` gap.
    #[tokio::test]
    async fn restore_refuses_to_finalize_over_reserved_but_unemitted_leaf() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        verify_emitted_frontier(&store)
            .await
            .expect("a store with no reservations passes the frontier gate");

        // The state a quarantined leaf leaves behind.
        store.reserve_deposit_index("0xdeadbeef").await.unwrap();
        let err = verify_emitted_frontier(&store)
            .await
            .expect_err("a reserved-but-unemitted leaf must fail the one-shot");
        assert!(
            err.to_string().contains("never emitted"),
            "the failure names the poison leaf: {err}"
        );
    }

    // MA#27 — the two claim dedups a restore relies on, pinned at the store
    // level: a re-observed note is a no-op, and a note whose global_index
    // already has a ClaimEvent (from the live path) is marked processed
    // without a second emission.
    #[tokio::test]
    async fn ma27_restore_claims_emits_and_dedups() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        let note_id = "0xnoteA".to_string();
        let gi = [0x42u8; 32];
        let bridge = get_bridge_address();
        let tx_hash = derive_manual_claim_tx_hash(&note_id);

        assert!(!store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(!store.has_claim_event_for_global_index(&gi).await.unwrap());

        store
            .commit_manual_claim_event_atomic(
                note_id.clone(),
                bridge,
                1,
                [0u8; 32],
                &tx_hash,
                gi,
                7,
                &[1u8; 20],
                &[2u8; 20],
                1_000,
            )
            .await
            .unwrap();

        assert!(store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(store.has_claim_event_for_global_index(&gi).await.unwrap());
        assert_eq!(
            store.get_latest_block_number().await.unwrap(),
            0,
            "an individual note projection must not seal its block"
        );

        let already_processed = store.is_claim_note_processed(&note_id).await.unwrap();
        assert!(
            already_processed,
            "second restore must see Dedup 1 fire and skip emission"
        );

        // Different note id, same global_index.
        let other_note = "0xnoteB".to_string();
        assert!(
            store.has_claim_event_for_global_index(&gi).await.unwrap(),
            "global_index dedup predicate must fire for a second observation"
        );
        store
            .mark_claim_note_processed(other_note.clone(), gi, 1)
            .await
            .unwrap();
        assert!(store.is_claim_note_processed(&other_note).await.unwrap());
    }

    // MA#27 — if the restore and live derivations drift, a restore-then-live
    // pair double-emits ClaimEvents under different tx hashes.
    #[test]
    fn ma27_restore_synthetic_tx_hash_matches_live_watcher() {
        let note_id = "0xfeed".to_string();
        let restore_path = derive_manual_claim_tx_hash(&note_id);
        let live_path = crate::claim_watcher::derive_manual_claim_tx_hash(&note_id);
        assert_eq!(
            restore_path, live_path,
            "restore and the live projector must derive identical synthetic tx-hashes"
        );
    }

    // MA#27 — operators verify a restore by its counts; pin the shape.
    #[test]
    fn ma27_restore_result_exposes_claims_restored() {
        let r = RestoreResult {
            block_number: 7,
            bridge_outs_restored: 1,
            faucet_identities_rebuilt: 0,
            claims_restored: 2,
            gers_restored: 3,
            logs_created: 6,
        };
        assert_eq!(r.claims_restored, 2);
    }

    /// Regression lock for the prod restart-resync incident: when discovery
    /// reached the tip during the restore, the next boot must not re-walk all
    /// of history.
    #[tokio::test]
    async fn restore_leaves_reconcile_cursor_at_swept_tip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        store.set_reconcile_cursor(123_456).await.unwrap();
        store.set_projector_cursor(100_000).await.unwrap();

        finalize_restore_cursors(&store, 130_000, Some(130_000))
            .await
            .unwrap();

        assert_eq!(
            store.get_reconcile_cursor().await.unwrap(),
            130_000,
            "recovery already swept to the tip — the next boot must not redo the full walk"
        );
        assert_eq!(
            store.get_projector_cursor().await.unwrap(),
            130_000,
            "projector cursor resumes at the Miden tip (restore already replayed history)"
        );
        assert_eq!(store.get_latest_block_number().await.unwrap(), 130_000);
    }

    /// Parking the sweep cursor at a short tip would strand the un-swept range
    /// forever.
    #[tokio::test]
    async fn restore_resets_reconcile_cursor_when_heal_incomplete() {
        for swept in [None, Some(129_999u64)] {
            let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
            store.set_reconcile_cursor(123_456).await.unwrap();

            finalize_restore_cursors(&store, 130_000, swept)
                .await
                .unwrap();

            assert_eq!(
                store.get_reconcile_cursor().await.unwrap(),
                0,
                "an incomplete heal ({swept:?} vs tip 130000) must fall back to a genesis re-sweep"
            );
        }
    }
}
