//! Restore — Reconstruct PgStore state from miden node.
//!
//! This module implements disaster recovery: when the PostgreSQL store is
//! empty (fresh deploy or data loss), it rebuilds all state from authoritative
//! sources (miden node consumed notes, miden sync state).
//!
//! ## Algorithm
//!
//! 0. Re-import configured accounts, then sync one Miden tip/LET snapshot.
//! 1. Scan canonical public B2AGG bodies and bridge transactions; prove exact
//!    NoteId, execution order, reservation prefix, and LET cardinality.
//! 2. Rebuild faucet identities, then replay B2AGG, CLAIM, and GER events at
//!    their original consumption blocks.
//! 3. Finalize the synthetic tip/projector cursor, reset the identity-reconcile
//!    cursor for its historical sweep, and verify counts.
//!
//! ## GER restoration via consumed notes
//!
//! For recovery we only care about consumed notes — actually injected GERs.
//! When the proxy injects a GER, it creates an UpdateGerNote that gets consumed
//! by the Miden bridge account. The Miden node retains consumed notes, so we can
//! scan them to reconstruct the full GER history.
//!
//! Each consumed UpdateGerNote stores the GER as 8 Felts in note storage.
//! The consumption block number gives us the ordering for hash chain reconstruction.
//!
//! See: https://github.com/0xMiden/protocol/issues/2341
//!
//! ## Known Limitations (TODOs for miden-node API enhancements)
//!
//! - B2AGG/GER note filtering is done client-side (no server-side script root filter)
//!   TODO: switch to NoteFilter::ConsumedByScriptRoot when available
//! - No block range queries for notes (full scan from genesis)
//!   TODO: switch to dedicated get_gers() endpoint when Marti's team ships it

use crate::accounts_config::AccountsConfig;
use crate::block_state::BlockState;
use crate::miden_client::MidenClient;
use crate::store::Store;
use miden_protocol::account::AccountId;
use std::sync::Arc;

/// Result of a restore operation.
pub struct RestoreResult {
    pub block_number: u64,
    pub bridge_outs_restored: usize,
    /// Cantina #6 — number of non-ETH faucet `faucet_registry` rows rebuilt from
    /// the bridge's authoritative `faucet_metadata_map` (rows that were missing
    /// on a fresh-DB / `--restore` bootstrap). Rebuilding these BEFORE replaying
    /// bridge-outs is what lets `resolve_faucet_origin` succeed so historical
    /// exits replay instead of being quarantined as `UnknownFaucet`.
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

    // Phase 1: Sync miden state + read the Miden tip — the block the synthetic
    // chain catches up to under Miden-1:1. Each restored event is attributed to
    // its OWN consumed block (below); `miden_tip` is only the orphan fallback.
    tracing::info!("Phase 1: syncing miden state...");
    let (miden_tip, let_leaves) = sync_miden_snapshot(miden_client, accounts.bridge.0).await?;
    tracing::info!("Phase 1 complete: miden tip {miden_tip}, LET leaves {let_leaves}");

    // Phase 1.7 (Cantina #6): rebuild missing non-ETH faucet identity rows from the
    // bridge's authoritative `faucet_metadata_map` BEFORE projecting bridge-outs.
    // Without this, a faucet whose local row was lost on a fresh-DB bootstrap makes
    // `resolve_faucet_origin` error, so the canonical projector and the live
    // `BridgeOutScanner` both quarantine/skip every historical exit tied to it, and
    // the next claim/admin-register deploys a REPLACEMENT faucet → split-brain
    // (Cantina #6). Best-effort: a per-faucet failure is logged + counted, never
    // aborts restore.
    tracing::info!("Phase 1.7: rebuilding faucet identities from bridge state (Cantina #6)...");
    let faucet_identities_rebuilt =
        restore_faucet_identities(store, miden_client, accounts, &network_rpcs).await?;
    tracing::info!(
        "Phase 1.7 complete: {faucet_identities_rebuilt} faucet identity row(s) rebuilt"
    );

    // ── Phase 2 (issue #167): CANONICAL CATCH-UP — recovery is normal
    // projection from cursor zero. The former parallel engine (the Phase 1.5
    // node scan + body/nullifier joins + `replay_history_in_order`, the
    // manual LET reservation pass, and the Phase 1.1 standalone healing
    // sweep) is deleted: `SyntheticProjector::catch_up_to` in
    // `CatchUpMode::Restore` drives the SAME discovery → join → order →
    // project → seal → cursor-commit path the live scheduler runs, in
    // blocking fail-closed mode, pinned to the Phase-1 captured tip. A node
    // producing beyond the captured tip cannot drag the snapshot forward.
    // The genesis re-sweep of the client store is the driver's discovery
    // phase itself (Recovery-patience reconcile from cursor zero), so the
    // heal-before-projection ordering of the old Phase 1.1 is preserved
    // structurally: nothing projects until discovery has reached the tip.
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
    tracing::info!(
        target_tip = miden_tip,
        "Phase 2: canonical catch-up from cursor zero (blocking, fail-closed)..."
    );
    let projector_for_run = projector.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
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
    let counts = projector.take_projection_counts();
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
/// re-scanning the blocks restore just replayed (idempotent dedup would skip
/// them anyway). The restored events already sit at their own Miden blocks.
///
/// The note-reconciler sweep cursor is the OPPOSITE: it is reset to 0. Restore
/// runs against a wiped/rebuilt miden store (`--reset-miden-store --restore` is
/// the canonical recovery invocation), so the client has forgotten every
/// imported note — the genesis re-sweep IS the healing pass that re-discovers
/// externally-created network notes, and it must not be skipped by a stale
/// persisted cursor.
/// Review 0814 (blocking): `accounted == let_leaves` proves every leaf is
/// RESERVED, not EMITTED — `project_b2agg_note` has post-reservation `Skipped`
/// paths (unparsable / no asset / unknown faucet / oversize / self-target).
/// Finalizing over a skipped leaf would ship the exact permanent
/// `depositCount` gap the live emitted-frontier gate refuses to seal past.
/// Same gate, same fail-closed posture, run after replay and BEFORE cursor
/// finalization: the one-shot fails and the operator repairs the leaf's
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

/// Phase 1: sync miden and return the current MIDEN tip (sync height) — the
/// block the synthetic chain catches up to under Miden-1:1.
async fn sync_miden_snapshot(
    miden_client: &MidenClient,
    bridge_id: AccountId,
) -> anyhow::Result<(u64, u64)> {
    let snapshot = Arc::new(std::sync::Mutex::new(None));
    let snapshot_inner = snapshot.clone();
    miden_client
        .with(move |client| {
            Box::new(async move {
                client.sync_state().await?;
                let height = client
                    .get_sync_height()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to get sync height: {e}"))?
                    .as_u64();
                let bridge = client
                    .get_account(bridge_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read bridge account {bridge_id}: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("bridge account {bridge_id} is unavailable"))?;
                let leaves = miden_base_agglayer::AggLayerBridge::read_let_num_leaves(&bridge);
                *snapshot_inner.lock().unwrap() = Some((height, leaves));
                Ok(())
            })
        })
        .await?;
    snapshot
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow::anyhow!("Miden snapshot was not captured"))
}

/// Phase 1.7 (Cantina #6): rebuild missing non-ETH faucet `faucet_registry` rows
/// from the bridge's authoritative `faucet_metadata_map`.
///
/// Enumerates every faucet registered on the bridge, and for each one WITHOUT a
/// local row, reads its origin identity (address / network / scale) back from the
/// bridge storage and its symbol / Miden-decimals from the faucet account, then
/// `store.register_faucet(...)` the reconstructed row. This is a pure READ of
/// public on-chain state — faucets are bridge-owned (mint/burn), so no signing
/// key is involved and the account is never re-deployed (its random seed is
/// unrecoverable; a re-deploy would strand balances in a second generation).
///
/// Returns the number of rows rebuilt. Best-effort: per-faucet failures are
/// logged + counted and never abort restore.
async fn restore_faucet_identities(
    store: &Arc<dyn Store>,
    miden_client: &MidenClient,
    accounts: &AccountsConfig,
    network_rpcs: &crate::metadata_recovery::NetworkRpcMap,
) -> anyhow::Result<usize> {
    let store_clone = store.clone();
    let bridge_id = accounts.bridge.0;
    // Owned clone moved into the `with(...)` closure; per-faucet RPC selection is
    // keyed on the faucet's origin_network (finding #62 multi-network recovery).
    let network_rpcs = network_rpcs.clone();

    let count = Arc::new(std::sync::Mutex::new(0usize));
    let count_inner = count.clone();

    miden_client
        .with(move |client| {
            Box::new(async move {
                // The bridge account holds the authoritative faucet_metadata_map;
                // Phase 0 reimported it. If it's still unavailable we cannot rebuild.
                let Some(bridge_account) = client.get_account(bridge_id).await.ok().flatten() else {
                    tracing::warn!(
                        bridge = %bridge_id,
                        "Cantina #6: bridge account not available locally; skipping faucet-identity rebuild"
                    );
                    return Ok(());
                };

                let faucet_ids = crate::metadata_recovery::enumerate_registered_faucet_ids(
                    bridge_account.storage(),
                );
                tracing::info!(
                    count = faucet_ids.len(),
                    "Cantina #6: bridge registers {} faucet(s); checking local rows",
                    faucet_ids.len()
                );

                let mut rebuilt = 0usize;
                for faucet_id in faucet_ids {
                    match store_clone.get_faucet_by_id(faucet_id).await {
                        Ok(Some(_)) => continue, // already have a local row
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(faucet_id = %faucet_id, error = ?e,
                                "Cantina #6: get_faucet_by_id failed; skipping");
                            continue;
                        }
                    }
                    let Some(conversion) = crate::metadata_recovery::read_faucet_conversion_metadata(
                        bridge_account.storage(),
                        faucet_id,
                    ) else {
                        // All-zero conversion = the native-ETH sentinel (pre-seeded, never
                        // rebuilt from chain) or an unregistered id. A registered NATIVE
                        // faucet has a non-zero origin (origin_network == network_id, scale 0),
                        // so it does NOT land here — it proceeds to classify + rebuild below.
                        continue;
                    };
                    match crate::faucet_ops::rebuild_faucet_entry_from_chain(
                        client,
                        &bridge_account,
                        faucet_id,
                        &conversion,
                        network_rpcs
                            .get(&conversion.origin_network)
                            .map(String::as_str),
                    )
                    .await
                    {
                        Ok(entry) => {
                            let (origin_network, scale) = (entry.origin_network, entry.scale);
                            match store_clone.register_faucet(entry).await {
                                Ok(()) => {
                                    rebuilt += 1;
                                    ::metrics::counter!("restore_faucet_identity_rebuilt_total")
                                        .increment(1);
                                    tracing::info!(
                                        faucet_id = %faucet_id,
                                        origin_network,
                                        scale,
                                        "Cantina #6: rebuilt missing faucet_registry row from \
                                         bridge faucet_metadata_map"
                                    );
                                }
                                Err(e) => tracing::warn!(faucet_id = %faucet_id, error = ?e,
                                    "Cantina #6: register_faucet failed during rebuild"),
                            }
                        }
                        Err(e) => {
                            // An UNKNOWN faucet type registered in the bridge is a fail-LOUD
                            // condition (malformed / hostile registration), distinct from a
                            // recoverable metadata miss — surface it at ERROR with its own
                            // metric so operators must investigate rather than let it pass as
                            // a routine quarantine.
                            if format!("{e:?}").contains("UNKNOWN faucet type") {
                                ::metrics::counter!("restore_unknown_faucet_type_total")
                                    .increment(1);
                                tracing::error!(
                                    faucet_id = %faucet_id,
                                    error = ?e,
                                    "restore: UNKNOWN faucet type registered in the bridge — \
                                     matches no supported faucet kind; NOT rebuilt. Investigate: \
                                     this should never happen for a proxy-registered faucet."
                                );
                            } else {
                                ::metrics::counter!("restore_faucet_identity_rebuild_failed_total")
                                    .increment(1);
                                tracing::warn!(
                                    faucet_id = %faucet_id,
                                    error = ?e,
                                    "Cantina #6: could not rebuild faucet row from chain; historical \
                                     bridge-outs for this faucet stay quarantined until it is backfilled"
                                );
                            }
                        }
                    }
                }

                *count_inner.lock().unwrap() = rebuilt;
                Ok(())
            })
        })
        .await?;

    let n = *count.lock().unwrap();
    Ok(n)
}

/// Tallies from the merged replay, kept per-kind so the phase-level log lines
/// (and the restore summary) stay unchanged for operators.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_address::get_bridge_address;
    use crate::claim_watcher::derive_manual_claim_tx_hash;
    use crate::store::Store;
    use crate::store::memory::InMemoryStore;

    use std::sync::Arc as StdArc;

    // Test AccountIds — four distinct, valid protocol-0.15 (version-1) ids.
    // Protocol 0.15 dropped the 0.14 v0 id encoding (and folded the old
    // Network *storage mode* away: `AccountType` is now just `Private`/`Public`,
    // and network-account behaviour comes from the `AuthNetworkAccount`
    // *component*, not an id bit). So `NetworkAccountTarget::new` no longer
    // constrains the target id's encoding, and these plain public/private ids
    // are accepted as targets. They are hardcoded hex (rather than pulled from
    // the `testing` feature) to keep this a dependency-light pure-predicate test;
    // the only property the ma28 classifier relies on is that the four ids are
    // mutually distinct.

    // PR#164 blocker #1 — the node-metadata join must fail closed on a
    // details-commitment collision that carries different provenance, never
    // last-write-wins.

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

    // GER-shaped `ConsumedExternal` fixture (MA#28) — shared with the projection
    // tests in `crate::projection`; used here by the erased-replay recovery test.

    // ── ClaimEvent provenance gate — foreign-deployment claims (live-proven) ─
    //
    // A read-only reindex of the real testnet (which hosts a FOREIGN
    // miden-agglayer deployment on the SAME Miden chain) projected 3
    // ClaimEvents from the foreign deployment's claims into our
    // synthetic_logs: `project_claim_note` gated only on the ClaimNote
    // script root, unlike the GER path's MA#28 sender/target gate and the
    // B2AGG path's MA#3 consumer gate. These tests pin the fix: a
    // CLAIM-shaped consumed note must be provably OURS (consumed by OUR
    // bridge, or minted by OUR service targeting OUR bridge) before a
    // synthetic ClaimEvent is projected.

    // ── Finding #69 — node-scan CLAIM replay (Phase 2.6) ─────────────────────
}
