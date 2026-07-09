use crate::miden_client::{MidenClient, MidenClientLib};
use alloy::primitives::TxHash;
use miden_base_agglayer::{AggLayerBridge, ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::AccountId;
use sha3::{Digest, Keccak256};
use std::sync::Arc;
use std::time::Duration;

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

/// Boxed future returned by a [`poll_until_ready`] check. Mirrors the crate's
/// `MidenClient::with` factory shape (`Box<dyn Future + 'a>` awaited via
/// `Box::into_pin`) so a per-poll closure can borrow `&mut MidenClientLib`.
type PollFuture<'a> = Box<dyn std::future::Future<Output = anyhow::Result<bool>> + 'a>;

/// Cantina #21 — bounded condition poll with early-exit.
///
/// Calls `check` up to `max_polls` times, sleeping `interval` between attempts,
/// and returns `Ok(true)` the INSTANT `check` reports the condition holds — so a
/// condition that is already satisfied costs a single check and zero sleeps.
/// Returns `Ok(false)` when the budget is exhausted, and propagates the first
/// `Err` a `check` produces.
///
/// Extracted from [`wait_for_ger_on_bridge`] so the early-exit semantics are
/// unit-testable without a live Miden client. `check` receives `&mut C` per call
/// (the borrow is released between polls), which is what lets the real caller
/// re-`sync_state` and re-read the bridge account each iteration.
async fn poll_until_ready<C, F>(
    subject: &mut C,
    max_polls: u32,
    interval: Duration,
    mut check: F,
) -> anyhow::Result<bool>
where
    F: for<'a> FnMut(&'a mut C) -> PollFuture<'a>,
{
    for attempt in 0..max_polls {
        if Box::into_pin(check(subject)).await? {
            return Ok(true);
        }
        // No sleep after the FINAL failed poll (PR #127 review): the caller's
        // wall-clock budget is (max_polls - 1) * interval between polls, not
        // max_polls * interval — a trailing sleep would delay the timeout
        // verdict by one interval for nothing.
        if attempt + 1 < max_polls {
            tokio::time::sleep(interval).await;
        }
    }
    Ok(false)
}

/// Cantina #21 — bounded, condition-based wait until the bridge account's GER
/// storage map reflects `ger`.
///
/// The `UpdateGerNote` is submitted by the ger_manager but CONSUMED
/// asynchronously by the network-transaction (NTX) builder on the bridge
/// account. A CLAIM's FPI runs `assert_valid_ger` against that map, so a CLAIM
/// must not execute until the bridge account carries the GER. Historically the
/// claim path padded a blind 5×3s = 15s sleep on EVERY claim to let this
/// consumption happen; this instead does the wait as a REAL condition check
/// (`AggLayerBridge::is_ger_registered` — exactly the value `assert_valid_ger`
/// asserts, `[1,0,0,0]` at `poseidon2::merge(GER)` in the `ger_map` slot),
/// re-`sync_state`-ing each poll and early-exiting the instant the GER appears.
///
/// Returns `Ok(true)` if the GER became visible within the budget, `Ok(false)`
/// on timeout. Callers treat a timeout as advisory: the on-chain MASM
/// `assert_valid_ger` is the hard gate, so a CLAIM submitted without the GER
/// fails closed with `ERR_GER_NOT_FOUND` rather than minting.
pub(crate) async fn wait_for_ger_on_bridge(
    client: &mut MidenClientLib,
    bridge_id: AccountId,
    ger: ExitRoot,
    max_polls: u32,
    interval: Duration,
) -> anyhow::Result<bool> {
    // `bridge_id` and `ger` are `Copy`, so this `FnMut` can re-capture them each
    // poll while `c` (the client borrow) is handed in per call.
    poll_until_ready(client, max_polls, interval, move |c| {
        Box::new(async move {
            // Refresh this client's view so the read reflects any bridge-account
            // consumption the NTX builder has committed since the last poll.
            c.sync_state().await?;
            let present = c
                .get_account(bridge_id)
                .await?
                .map(|acct| AggLayerBridge::is_ger_registered(ger, &acct).unwrap_or(false))
                .unwrap_or(false);
            Ok(present)
        })
    })
    .await
}

/// Submit the actual UpdateGerNote Miden transaction. Factored out of
/// `insert_ger` so the caller can run it twice — once eagerly, then again
/// after `reimport_account` if the first attempt failed with a recoverable
/// account-state error.
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
/// Submit the `UpdateGerNote` to Miden and return the on-chain note's
/// `details_commitment` (hex), encoded identically to how the projector keys
/// consumed notes (`InputNoteRecord::details_commitment()`) — so `insert_ger`
/// can tie the real `insertGlobalExitRoot` eth-tx to this note via
/// `record_tx_note_link`. Returns `None` only when the submit closure did not
/// execute (a stubbed MidenClient in unit tests).
async fn submit_update_ger_note(
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    ger_bytes: [u8; 32],
) -> anyhow::Result<Option<String>> {
    let inner_accounts = accounts.0.clone();
    // `MidenClient::with` closures resolve to `Result<()>`; surface the note
    // commitment through a captured slot (same pattern as `publish_claim`).
    let commitment_slot = Arc::new(std::sync::OnceLock::<String>::new());
    let commitment_inner = commitment_slot.clone();
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
                // Commitment of the on-chain note, matching the projector's
                // consumed-note key (`InputNoteRecord::details_commitment()`).
                let note_commitment = hex::encode(
                    miden_protocol::note::NoteDetails::from(&note)
                        .commitment()
                        .as_bytes(),
                );
                let _ = commitment_inner.set(note_commitment);
                tracing::info!(
                    note_id = %note.id(),
                    ger = %hex::encode(ger_bytes),
                    "UpdateGerNote created"
                );
                let tx_request = TransactionRequestBuilder::new()
                    .own_output_notes(vec![note])
                    .build()?;
                let tx_id = crate::metrics::meter_proof(
                    crate::metrics::ProofKind::Ger,
                    crate::miden_client::submit_new_transaction(client, ger_manager_id, tx_request),
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

                // Cantina #21 — await GER propagation to the bridge account HERE,
                // once, at injection time, instead of padding every CLAIM with a
                // blind 15s sleep. The NTX builder consumes this UpdateGerNote on
                // the bridge account ASYNCHRONOUSLY (the commit above only lands the
                // note); block until the account actually reflects the GER (bounded,
                // early-exiting), so by the time any CLAIM for this GER arrives the
                // bridge account already carries it and the claim-side poll returns
                // on its first iteration. A timeout is non-fatal: the claim-side
                // safety poll and the MASM `assert_valid_ger` gate still enforce
                // correctness.
                match wait_for_ger_on_bridge(
                    client,
                    bridge_id,
                    ger,
                    20,
                    std::time::Duration::from_secs(1),
                )
                .await
                {
                    Ok(true) => tracing::info!(
                        ger = %hex::encode(ger_bytes),
                        "Cantina #21: GER now reflected on bridge account (consumed by NTX builder)"
                    ),
                    Ok(false) => tracing::warn!(
                        ger = %hex::encode(ger_bytes),
                        "Cantina #21: GER not yet reflected on bridge account within injection \
                         wait budget; claims will re-poll before submitting"
                    ),
                    Err(e) => tracing::warn!(
                        ger = %hex::encode(ger_bytes),
                        error = ?e,
                        "Cantina #21: error awaiting GER propagation to bridge account; \
                         claims will re-poll before submitting"
                    ),
                }
                Ok(())
            })
        })
        .await?;
    Ok(commitment_slot.get().cloned())
}

/// Submit a GER injection to Miden. Returns `true` if a new `UpdateGerNote` was
/// submitted (and the real eth-tx ↔ note link recorded so the projector finalises
/// the receipt + emits the GER log on consumption), `false` if the GER was already
/// injected (a duplicate — the caller completes its receipt immediately).
pub async fn insert_ger(
    ger_bytes: [u8; 32],
    miden_client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: &Arc<dyn crate::store::Store>,
    txn_hash: TxHash,
) -> anyhow::Result<bool> {
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
        let note_commitment = match submit_update_ger_note(
            miden_client,
            accounts.clone(),
            ger_bytes,
        )
        .await
        {
            Ok(commitment) => commitment,
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
                submit_update_ger_note(miden_client, accounts.clone(), ger_bytes).await?
            }
            Err(err) => return Err(err),
        };

        // Tie the real `insertGlobalExitRoot` eth-tx to the on-chain UpdateGerNote so
        // the SyntheticProjector finalises THIS receipt (and emits the GER log) under
        // the real tx hash when it observes the note consumed — making the receipt
        // block == the GER-log block. No synthetic log / tip advance / receipt
        // completion happens in this path. (`note_commitment` is `None` only under a
        // stubbed test client; the projector then falls back to the derived hash.)
        if let Some(note_commitment) = note_commitment {
            store
                .record_tx_note_link(&format!("{txn_hash:#x}"), &note_commitment)
                .await?;
        }
    } else {
        tracing::debug!(
            ger = %hex::encode(ger_bytes),
            "GER already seen, skipping duplicate"
        );
    }

    Ok(is_new)
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

    /// Probe standing in for "the bridge account, checked each poll". `ready_at`
    /// is the poll index (0-based) at which the condition first holds.
    struct Probe {
        polls: u32,
        ready_at: u32,
    }

    /// Cantina #21 — a GER already present on the bridge account (the common case
    /// once `insert_ger` has awaited propagation) must early-exit on the FIRST
    /// poll with zero sleeps. This is the whole point of moving the wait to
    /// injection time: the per-claim path no longer pays the old 15s pad.
    #[tokio::test]
    async fn poll_until_ready_early_exits_on_first_poll_when_condition_holds() {
        let mut probe = Probe {
            polls: 0,
            ready_at: 0,
        };
        let out = poll_until_ready(&mut probe, 5, Duration::from_millis(0), |p| {
            Box::new(async move {
                let i = p.polls;
                p.polls += 1;
                Ok(i >= p.ready_at)
            })
        })
        .await
        .unwrap();
        assert!(out);
        assert_eq!(probe.polls, 1, "must early-exit on the first poll");
    }

    /// When the GER is not yet present it must keep polling and return the instant
    /// the condition flips true (here: the 3rd poll).
    #[tokio::test]
    async fn poll_until_ready_polls_until_condition_becomes_true() {
        let mut probe = Probe {
            polls: 0,
            ready_at: 2,
        };
        let out = poll_until_ready(&mut probe, 10, Duration::from_millis(0), |p| {
            Box::new(async move {
                let i = p.polls;
                p.polls += 1;
                Ok(i >= p.ready_at)
            })
        })
        .await
        .unwrap();
        assert!(out);
        assert_eq!(probe.polls, 3, "returns as soon as the condition holds");
    }

    /// On budget exhaustion it checks exactly `max_polls` times then returns
    /// `false` (advisory timeout — the caller submits anyway and the MASM
    /// `assert_valid_ger` remains the hard gate).
    #[tokio::test]
    async fn poll_until_ready_returns_false_on_budget_exhaustion() {
        let mut probe = Probe {
            polls: 0,
            ready_at: u32::MAX,
        };
        let out = poll_until_ready(&mut probe, 4, Duration::from_millis(0), |p| {
            Box::new(async move {
                p.polls += 1;
                Ok(false)
            })
        })
        .await
        .unwrap();
        assert!(!out);
        assert_eq!(
            probe.polls, 4,
            "checks exactly max_polls times then gives up"
        );
    }

    /// PR #127 review — no sleep after the FINAL failed poll: the timeout
    /// verdict must arrive after (max_polls - 1) * interval of waiting, not
    /// max_polls * interval. Paused-clock test: tokio auto-advances virtual
    /// time on sleep, so elapsed time counts exactly the sleeps performed.
    #[tokio::test(start_paused = true)]
    async fn poll_until_ready_does_not_sleep_after_last_attempt() {
        let mut probe = Probe {
            polls: 0,
            ready_at: u32::MAX,
        };
        let interval = Duration::from_secs(1);
        let t0 = tokio::time::Instant::now();
        let out = poll_until_ready(&mut probe, 3, interval, |p| {
            Box::new(async move {
                p.polls += 1;
                Ok(false)
            })
        })
        .await
        .unwrap();
        assert!(!out);
        assert_eq!(probe.polls, 3);
        assert_eq!(
            t0.elapsed(),
            interval * 2,
            "3 polls must sleep exactly twice (between polls), never after the last"
        );
    }
}
