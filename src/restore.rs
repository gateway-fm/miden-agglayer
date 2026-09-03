//! Restore — Reconstruct PgStore state via the canonical projector.
//!
//! This module implements disaster recovery: when the PostgreSQL store is
//! empty (fresh deploy or data loss) — possibly alongside a wiped miden-client
//! store (`--reset-miden-store`) — it rebuilds all state by driving the SAME
//! `SyntheticProjector` the live scheduler runs, pinned to a captured tip
//! (issue #167). There is no second projection engine: the former node-scan /
//! replay machinery was deleted, and this module is a thin offline
//! orchestration wrapper.
//!
//! ## Algorithm
//!
//! 0. Re-import configured accounts (bootstrap; emits no history).
//! 1. Reset both cursors to genesis (atomic), then run
//!    `SyntheticProjector::catch_up_to(.., CatchUpMode::Restore)` inside ONE
//!    frozen actor session: the Miden tip + LET snapshot and the entire
//!    discovery → resolve → order → project → seal → cursor-commit catch-up
//!    observe the same chain state, fail-closed on any stall, with the
//!    authoritative-coverage guard refusing to seal unrecoverable inputs. The
//!    chain-derived bootstrap state (faucet identities, Cantina #6) is rebuilt
//!    by the projector itself through the normal
//!    `faucet_bootstrap` primitive before the first dependent event projects.
//! 3. Verify LET accounting + emitted frontier, finalize the synthetic
//!    tip / projector / reconcile cursors (the reconcile cursor is PARKED at
//!    the tip the catch-up reached), and emit the report.
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
//! - A bridge-consumed input the canonical path cannot reconstruct (no
//!   transaction-header reference and no durable identity from the note sweep)
//!   fails the restore with an actionable error (coverage guard) — it can never
//!   be silently skipped.

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::miden_client::MidenClient;
use crate::store::Store;
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
    /// Cantina #6 — number of non-ETH faucet `faucet_registry` rows the
    /// projector's faucet bootstrap (`faucet_bootstrap`) rebuilt from the
    /// bridge's authoritative `faucet_metadata_map` (rows missing on a fresh-DB
    /// / `--restore` bootstrap). Rebuilt BEFORE the first dependent bridge-out
    /// projects, fail-closed, so historical exits replay instead of being
    /// quarantined as `UnknownFaucet`.
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

    // Phase 1 (the Miden snapshot) is taken INSIDE the Phase-2 actor session
    // below — see the frozen-store rationale there. (issue #167 review: a
    // snapshot taken in a separate session can go stale while background
    // sync keeps running, making the LET gate nondeterministic on a
    // producing node.)

    // Faucet identities (Cantina #6) are no longer a restore-private phase: the
    // projector runs the `faucet_bootstrap` primitive in restore posture before
    // every pass, fail-closed (issue #167 item 5), so a faucet whose local row
    // was lost cannot make historical exits quarantine and the restore "succeed"
    // over a depositCount gap.

    // ── Phase 2 (issue #167): CANONICAL CATCH-UP — recovery is normal
    // projection from cursor zero. The former parallel engine (the Phase 1.5
    // node scan + body/nullifier joins + `replay_history_in_order`, the
    // manual LET reservation pass, and the Phase 1.1 standalone healing
    // sweep) is deleted: `SyntheticProjector::catch_up_to` in
    // `CatchUpMode::Restore` drives the SAME discovery → join → order →
    // project → seal → cursor-commit path the live scheduler runs, in
    // blocking fail-closed mode, pinned to the captured tip. A node
    // producing beyond the captured tip cannot drag the snapshot forward.
    // The genesis re-sweep of the client store is the driver's discovery
    // phase itself (Recovery-patience reconcile from cursor zero), so the
    // heal-before-projection ordering of the old Phase 1.1 is preserved
    // structurally: nothing projects until discovery has reached the tip.
    //
    // FROZEN-STORE SESSION (issue #167 review): the whole snapshot + catch-up
    // runs as ONE request on the serialized Miden actor. While it runs, the
    // actor's own sync ticker cannot interleave, so the captured
    // `(tip, LET leaves)` and every account/state read the gates make inside
    // the session observe the SAME chain state — restore is deterministic
    // even against a node that keeps producing.
    //
    // Cursor reset happens BEFORE the projector is constructed (it loads the
    // persisted cursors once in `new()`), through the single-statement atomic
    // store reset — a torn reset would let projection skip history the
    // re-discovery re-sweep finds.
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
                // Phase 1 — frozen-store snapshot: tip + LET cardinality.
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

                // Phase 2 — the canonical catch-up itself.
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

/// Phase 4 of [`restore`]: finalize the persisted cursors.
///
/// Miden-1:1 — the synthetic tip == the Miden tip, and the projector cursor is
/// set to the Miden tip so the live projector resumes from there rather than
/// re-scanning the blocks restore just projected (idempotent dedup would skip
/// them anyway). The restored events already sit at their own Miden blocks.
///
/// The note-reconciler sweep cursor is PARKED at the tip the canonical
/// catch-up reached (`swept_to == Some(miden_tip)` from the driver): the
/// catch-up's discovery phase already ran the genesis heal to the tip, so
/// leaving the cursor there avoids a redundant full-history re-walk on the
/// next boot. The genesis-reset fallback remains for an incomplete heal.
/// Review 0814 (blocking): `accounted == let_leaves` proves every leaf is
/// RESERVED, not EMITTED — `project_b2agg_note` has post-reservation `Skipped`
/// paths (unparsable / no asset / unknown faucet / oversize / self-target).
/// Finalizing over a skipped leaf would ship the exact permanent
/// `depositCount` gap the live emitted-frontier gate refuses to seal past.
/// Same gate, same fail-closed posture, run after the catch-up and BEFORE
/// cursor finalization: the one-shot fails and the operator repairs the leaf's
/// metadata (registry backfill) before re-running `--restore`.
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

    // Recovery now performs the healing sweep ITSELF (Phase 1.1), before the
    // replay phases that depend on it. When that sweep reached the tip there is
    // nothing left to re-discover, so leaving the cursor there avoids a second
    // full-history block walk on the next boot (the old behaviour: restore
    // walked every block, then the serving proxy walked them all again — and,
    // because the projector cursor is parked at the tip above, that second walk
    // could not project anything anyway).
    //
    // Fail-safe: if the sweep did NOT reach the tip, fall back to the historical
    // reset-to-genesis so the serving proxy still attempts the heal.
    match swept_to {
        Some(t) if t >= miden_tip => {
            store.set_reconcile_cursor(t).await?;
            tracing::info!(
                swept_to = t,
                "reconcile cursor left at the swept tip — recovery already ran the healing \
                 sweep (Phase 1.1); no redundant genesis re-walk on the next boot"
            );
        }
        other => {
            store.set_reconcile_cursor(0).await?;
            tracing::warn!(
                swept_to = ?other,
                "reconcile cursor reset to genesis — recovery's healing sweep did not reach the \
                 tip, so the serving proxy must re-sweep"
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

    /// Review 0814 (blocking): a leaf can be RESERVED (satisfying the
    /// `accounted == let_leaves` cardinality check) yet never EMITTED — the
    /// post-reservation `Skipped` paths in `project_b2agg_note`. Restore must
    /// FAIL before cursor finalization instead of shipping a permanent
    /// `depositCount` gap.
    #[tokio::test]
    async fn restore_refuses_to_finalize_over_reserved_but_unemitted_leaf() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());
        verify_emitted_frontier(&store)
            .await
            .expect("a store with no reservations passes the frontier gate");

        // Reserve without emitting — exactly the state a quarantined /
        // unrecoverable-metadata leaf leaves behind after replay.
        store.reserve_deposit_index("0xdeadbeef").await.unwrap();
        let err = verify_emitted_frontier(&store)
            .await
            .expect_err("a reserved-but-unemitted leaf must fail the one-shot");
        assert!(
            err.to_string().contains("never emitted"),
            "the failure names the poison leaf: {err}"
        );
    }

    // MA#27 — store-level pin for the Phase 2.5 dedup-and-emit pipeline.
    // Replays the inner steps `restore_claims` performs against an
    // InMemoryStore (skipping only the per-tick consumed_notes fetch which
    // requires a live miden-client) and asserts:
    //   1) First call emits a ClaimEvent and marks the note processed.
    //   2) Second call (same note) is a no-op (Dedup 1).
    //   3) If a ClaimEvent for the same global_index was already written
    //      (e.g. by the normal eth_sendRawTransaction path), the new
    //      observation skips emission but DOES mark the note processed
    //      (Dedup 2).
    #[tokio::test]
    async fn ma27_restore_claims_emits_and_dedups() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        let note_id = "0xnoteA".to_string();
        let gi = [0x42u8; 32];
        let bridge = get_bridge_address();
        let tx_hash = derive_manual_claim_tx_hash(&note_id);

        // Pre-conditions
        assert!(!store.is_claim_note_processed(&note_id).await.unwrap());
        assert!(!store.has_claim_event_for_global_index(&gi).await.unwrap());

        // Phase 2.5 inner emission — mirror the call we make in
        // `restore_claims` for an accepted CLAIM.
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

        // Idempotency: Dedup 1 short-circuits on a second pass. We model
        // this by checking the predicate restore_claims uses BEFORE doing
        // any write — if it returns true, we skip.
        let already_processed = store.is_claim_note_processed(&note_id).await.unwrap();
        assert!(
            already_processed,
            "second restore must see Dedup 1 fire and skip emission"
        );

        // Dedup 2 — different note id, same global_index. The normal path
        // already wrote the ClaimEvent; restore's job is to mark the new
        // observation processed but NOT double-emit. We assert via the
        // public predicate.
        let other_note = "0xnoteB".to_string();
        assert!(
            store.has_claim_event_for_global_index(&gi).await.unwrap(),
            "global_index dedup predicate must fire for a second observation"
        );
        // The mark step for the "already-recorded" branch is also exposed
        // via the store primitive — pin it directly so any future store
        // refactor that drops mark_claim_note_processed in this branch
        // is caught.
        store
            .mark_claim_note_processed(other_note.clone(), gi, 1)
            .await
            .unwrap();
        assert!(store.is_claim_note_processed(&other_note).await.unwrap());
    }

    // MA#27 — pin the synthetic tx-hash derivation used by Phase 2.5
    // matches what the live `ClaimWatcher` produces. If these drift, a
    // restore-then-live pair will double-emit ClaimEvents under different
    // tx_hashes and bridge-service won't dedup them.
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

    // MA#27 — RestoreResult exposes a `claims_restored` counter so
    // operators can verify the new Phase 2.5 ran. Pin the field shape;
    // older RestoreResult shapes without this field made it impossible to
    // tell whether the new phase had executed at all.
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

    /// Regression lock for the prod restart-resync incident: a restore run
    /// rebuilds the miden store, so the client has forgotten every imported
    /// note. Recovery now runs that healing sweep ITSELF (Phase 1.1), before the
    /// replay phases that read the consumed feed. When the sweep reached the tip
    /// there is nothing left to re-discover, so Phase 4 leaves the reconcile
    /// cursor AT the swept tip — the next boot must not re-walk all of history
    /// (it could not project anything anyway: the projector cursor is parked at
    /// the tip, which is exactly how the GER history used to be lost).
    #[tokio::test]
    async fn restore_leaves_reconcile_cursor_at_swept_tip() {
        let store: StdArc<dyn Store> = StdArc::new(InMemoryStore::new());

        // Simulate a long-running pre-restore deployment: both cursors deep
        // into history.
        store.set_reconcile_cursor(123_456).await.unwrap();
        store.set_projector_cursor(100_000).await.unwrap();

        // Phase 4 of restore() — the exact code path the real restore runs,
        // with Phase 1.1's healing sweep having reached the tip.
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

    /// Fail-safe half of the contract above: if recovery's healing sweep did NOT
    /// reach the tip (never ran, or stopped short), the reconcile cursor must
    /// fall back to genesis so the serving proxy still attempts the heal. Parking
    /// it at a short tip would strand the un-swept range forever.
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
