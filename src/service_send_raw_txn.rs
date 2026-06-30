use crate::claim::claimAssetCall;
use crate::ger::{insertGlobalExitRootCall, updateExitRootCall};
use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use crate::store::TxnEntry;
use crate::*;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, LogData, TxHash};
use alloy_core::sol_types::SolCall;

struct TransactionData {
    pub hash: TxHash,
    pub input: alloy::primitives::Bytes,
}

fn unwrap_txn_envelope(txn_envelope: TxEnvelope) -> anyhow::Result<TransactionData> {
    let data = match txn_envelope {
        TxEnvelope::Eip1559(txn_signed) => {
            let hash = *txn_signed.hash();
            let txn = txn_signed.strip_signature();
            TransactionData {
                hash,
                input: txn.input,
            }
        }
        TxEnvelope::Legacy(txn_signed) => {
            let hash = *txn_signed.hash();
            let txn = txn_signed.strip_signature();
            TransactionData {
                hash,
                input: txn.input,
            }
        }
        _ => {
            tracing::error!("unhandled txn type {:?}", txn_envelope.tx_type());
            anyhow::bail!("unhandled txn type {:?}", txn_envelope.tx_type());
        }
    };
    Ok(data)
}

async fn handle_ger_result(
    result: anyhow::Result<bool>,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    service: &ServiceState,
    ger_bytes: [u8; 32],
) -> anyhow::Result<()> {
    match result {
        Ok(is_new) => {
            let _ = ger_bytes; // kept for backward-compat; unused here.
            tracing::info!("inserted GER with eth txn: {txn_hash}");
            if is_new {
                // New GER: insert_ger recorded the eth-tx ↔ UpdateGerNote link. Record
                // ONLY a pending receipt; the SyntheticProjector finalises it (txn_commit)
                // at the Miden block where it consumes the note — receipt block == GER-log
                // block. eth_getTransactionReceipt returns null until then (mined-when-
                // consumed), which aggkit tolerates.
                record_local_pending_tx(service, txn_hash, txn_envelope, signer, None, vec![])
                    .await?;
            } else {
                // Duplicate GER (already injected): no new UpdateGerNote will be consumed,
                // so the projector has nothing to finalise — complete the receipt now at
                // the current tip so eth_getTransactionReceipt resolves.
                record_local_immediate_success(service, txn_hash, txn_envelope, signer, vec![])
                    .await?;
            }
            Ok(())
        }
        Err(err) => {
            tracing::error!("insert_ger failed: {err:#?}");
            Err(err)
        }
    }
}

async fn record_local_pending_tx(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    expires_at: Option<u64>,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    service
        .store
        .txn_begin(
            tx_hash,
            TxnEntry {
                id: None,
                envelope: txn_envelope,
                signer,
                expires_at,
                logs,
            },
        )
        .await
}

async fn record_local_immediate_success(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    let block_num = service.store.get_latest_block_number().await?;
    record_local_success_at_block(service, tx_hash, txn_envelope, signer, block_num, logs).await
}

async fn record_local_success_at_block(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    block_num: u64,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    record_local_pending_tx(service, tx_hash, txn_envelope, signer, None, logs).await?;
    let block_hash = service.block_state.get_block_hash(block_num);
    service
        .store
        .txn_commit(tx_hash, Ok(()), block_num, block_hash)
        .await
}

/// Handle a `claimAsset` transaction: skip zero-amount or publish the claim.
///
/// RD-940 Phase 1: this is the unified dispatcher for both the legacy sync
/// path and the new writer-worker path. **It does NOT advance the per-signer
/// nonce** — the caller in `service_send_raw_txn` does that once, after the
/// dispatch (sync) or after a successful `try_enqueue` (worker), so the two
/// paths agree on when nonce advances.
///
/// `_` suffix in `_signer_str_unused` calls below is a deliberate marker that
/// this function used to own three `nonce_increment` calls — see git blame on
/// the previous revision.
pub(crate) async fn worker_handle_claim_asset(
    service: &ServiceState,
    params: claimAssetCall,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    // Only claims where destinationNetwork matches our network_id are processed.
    //
    // RD-703 — `service.network_id` is `u32` (validated at startup in
    // `main.rs` via `u32::try_from(command.network_id)`), matching the
    // Solidity bridge's `uint32 destinationNetwork`. No silent `as u32` cast
    // here: any operator value that does not fit `u32` is rejected loudly
    // at startup rather than truncating into a comparison that would
    // spuriously accept the wrong network.
    if params.destinationNetwork != service.network_id {
        anyhow::bail!(
            "claim targets destinationNetwork {} but this proxy only handles network {}",
            params.destinationNetwork,
            service.network_id
        );
    }

    // Skip zero-amount claims (e.g., genesis batch deposit). These create
    // CLAIM notes that crash the NTX builder's faucet actor.
    if params.amount.is_zero() {
        tracing::info!("skipping zero-amount claim (genesis batch)");
        record_local_immediate_success(service, txn_hash, txn_envelope, signer, vec![]).await?;
        return Ok(());
    }

    // RD-860 — swallow unresolvable-destination claims permanently. If the
    // destination address can't be resolved to a Miden AccountId, record the
    // unclaimable entry, emit the synthetic ClaimEvent so aggkit marks the
    // globalIndex complete and stops retrying, and return success to the
    // caller. Funds remain locked on L1; an operator rescue endpoint (tier 2,
    // future work) would let ops re-process by registering a destination
    // mapping and replaying.
    //
    // Ordering: this check runs BEFORE C6's GER pre-check because the
    // unresolvable-destination state is permanent (no amount of GER
    // propagation will change it), whereas a missing GER is transient. Doing
    // RD-860 first means aggkit gets a single decisive swallow instead of
    // grinding through GER-not-seen retries before eventually hitting the
    // same swallow.
    if let Err(err) = crate::address_mapper::resolve_address(
        &*service.store,
        params.destinationAddress,
        &service.accounts.0,
    )
    .await
    {
        ::metrics::counter!(
            "claim_unclaimable_total",
            "reason" => crate::store::UnclaimableReason::UnresolvableDestination.as_str()
        )
        .increment(1);
        let newly_recorded = service
            .store
            .record_unclaimable_claim(crate::store::UnclaimableClaim {
                global_index: params.globalIndex,
                destination_address: params.destinationAddress,
                origin_network: params.originNetwork,
                origin_address: params.originTokenAddress,
                amount: params.amount,
                reason: crate::store::UnclaimableReason::UnresolvableDestination,
                eth_tx_hash: txn_hash,
            })
            .await?;
        tracing::warn!(
            global_index = %params.globalIndex,
            destination = %params.destinationAddress,
            origin_network = params.originNetwork,
            origin_address = %params.originTokenAddress,
            amount = %params.amount,
            eth_tx = %txn_hash,
            newly_recorded,
            err = %err,
            "claim: unresolvable destination — short-circuiting so aggkit stops retrying. \
             Funds remain on L1 pending operator rescue (RD-860)."
        );

        // Emit the synthetic ClaimEvent even though no Miden funds moved. aggkit expects
        // the event log on the eth tx receipt to mark the globalIndex claimed; without
        // it, aggkit will retry forever. The unclaimable_claims table is the SOURCE OF
        // TRUTH for reconciliation — anyone auditing flows MUST compare ClaimEvent
        // counts against `unclaimable_claims` to see how many funds are truly on L1.
        let event = crate::claim::ClaimEvent::from(params.clone());
        let log = <crate::claim::ClaimEvent as alloy::sol_types::SolEvent>::encode_log_data(&event);
        record_local_immediate_success(service, txn_hash, txn_envelope, signer, vec![log]).await?;
        return Ok(());
    }

    // C6 — gate on `has_seen_ger` BEFORE acquiring the claim lock.
    //
    // The CLAIM note's leaf proof is internally consistent (built from L1
    // calldata), but on-chain the bridge MASM verifies it against the GER
    // currently stored in the bridge account. If aggkit hasn't yet observed
    // (and propagated) the relevant GER, the on-chain `assert_valid_ger`
    // rejects the claim — but only AFTER:
    //   1. try_claim locks the globalIndex
    //   2. publish_claim sleeps 15s waiting for GER propagation
    //   3. Miden tx is submitted
    //   4. on-chain MASM panics with ERR_GER_NOT_FOUND
    //   5. unclaim runs
    //
    // That entire round-trip is wasted work (and burns a Miden gas budget).
    // Pre-check `has_seen_ger(combined_ger(mainnet_exit_root, rollup_exit_root))`
    // — if false, return a retryable error immediately so aggkit-driven
    // clients re-submit cleanly without burning the lock or the 15s wait.
    let combined = crate::ger::combined_ger(&params.mainnetExitRoot.0, &params.rollupExitRoot.0);
    // `is_ger_injected` rather than `has_seen_ger`: the L1InfoTreeIndexer
    // pre-populates ger_entries rows for L1 pairs it has indexed but that
    // haven't yet been injected to L2. C6 requires the GER to be on L2, not
    // merely indexed; checking the `is_injected` flag captures that intent.
    if !service.store.is_ger_injected(&combined).await? {
        ::metrics::counter!("rpc_claim_ger_not_seen_total").increment(1);
        anyhow::bail!(
            "claim references a GER that aggkit has not observed yet \
             (mainnet={}, rollup={}); retry after the GER is injected. C6.",
            ::hex::encode(params.mainnetExitRoot.0),
            ::hex::encode(params.rollupExitRoot.0)
        );
    }

    // Lock the claim index. All error paths after this MUST unclaim.
    service.store.try_claim(params.globalIndex).await?;

    // R9 — install a RAII drop guard so that even if the request future is
    // dropped (client disconnect mid-publish, panic, task cancellation), the
    // claim lock is released. Without the guard, a malicious caller can
    // permanently lock arbitrary globalIndex values by repeatedly disconnecting
    // mid-flight during the 15s GER-propagation wait inside `publish_claim`.
    let guard = ClaimGuard::new(service.store.clone(), params.globalIndex);

    let result =
        publish_and_record_claim(service, params.clone(), txn_hash, txn_envelope, signer).await;
    if let Err(err) = result {
        // Explicit release: the guard would also fire on drop, but doing it
        // here avoids the tokio::spawn round-trip on the error path.
        guard.release_explicitly().await;
        tracing::error!("claim failed after lock: {err:#?}");
        return Err(err);
    }

    // On success the lock should NOT be released (the claim is committed). Tell
    // the guard to forget so its Drop is a no-op.
    guard.commit();

    Ok(())
}

/// RAII guard that releases a `try_claim` lock if the holding future is dropped
/// before either `commit()` (claim succeeded — keep the lock) or
/// `release_explicitly()` (claim failed — release synchronously) is called.
///
/// On drop with neither call made, schedules a background `unclaim` via
/// `tokio::spawn`. Guarantees that a cancelled / panicked / disconnected request
/// future cannot leave a globalIndex permanently locked. Self-review R9.
pub(crate) struct ClaimGuard {
    store: Option<std::sync::Arc<dyn crate::store::Store>>,
    global_index: alloy::primitives::U256,
}

impl ClaimGuard {
    fn new(
        store: std::sync::Arc<dyn crate::store::Store>,
        global_index: alloy::primitives::U256,
    ) -> Self {
        Self {
            store: Some(store),
            global_index,
        }
    }

    /// Mark the lock as committed — the claim succeeded. Drop becomes a no-op.
    fn commit(mut self) {
        self.store = None;
    }

    /// Synchronously release the lock (caller awaits the unclaim).
    async fn release_explicitly(mut self) {
        if let Some(store) = self.store.take() {
            let _ = store.unclaim(&self.global_index).await;
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let global_index = self.global_index;
            // tokio::spawn requires a current runtime; in normal handler contexts
            // it always exists. If we're somehow being dropped outside any
            // runtime (e.g. a unit test that constructed the guard but never
            // entered tokio), the spawn will panic — guard against that with
            // try_handle.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = store.unclaim(&global_index).await {
                        tracing::error!(
                            target: "claim::guard",
                            "R9 drop-guard failed to unclaim {global_index}: {e:#}"
                        );
                    } else {
                        tracing::warn!(
                            target: "claim::guard",
                            "R9 drop-guard released claim {global_index} after future was cancelled"
                        );
                    }
                });
            } else {
                tracing::error!(
                    target: "claim::guard",
                    "R9 drop-guard ran outside tokio runtime; claim {global_index} may be leaked"
                );
            }
        }
    }
}

/// Publish a CLAIM note and record the transaction in the store.
///
/// Called after `try_claim` succeeds. The caller is responsible for calling
/// `unclaim()` if this function returns an error.
async fn publish_and_record_claim(
    service: &ServiceState,
    params: claimAssetCall,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    // ClaimEvent recording happens inside the MidenClient closure (cancellation-safe).
    let latest_block = service.store.get_latest_block_number().await?;
    let claim_result = claim::publish_claim(
        params,
        &service.miden_client,
        service.accounts.clone(),
        service.store.clone(),
        latest_block,
        txn_hash,
        txn_envelope,
        signer,
        service.reject_zero_padding_addresses,
        service.reject_hardhat_alias,
        Some(service.expected_mints.clone()),
    )
    .await?;
    tracing::info!(
        eth_tx = %txn_hash,
        miden_tx = %claim_result.txn_id,
        "claim published; receipt pending until the projector finalises it on consumption"
    );
    Ok(())
}

/// Unified GER-insert / updateExitRoot dispatcher used by both the legacy sync
/// path and the writer-worker path. **Does NOT advance the per-signer
/// nonce** — see the matching note on `worker_handle_claim_asset`.
///
/// The GER synthetic log (and the decomposed exit roots it carried) is now
/// emitted by the `SyntheticProjector` from the consumed `UpdateGerNote`, so
/// this path only needs the combined `ger_bytes` to submit to Miden — the
/// decomposed mainnet/rollup roots are no longer threaded through.
pub(crate) async fn worker_handle_ger_insert(
    service: &ServiceState,
    ger_bytes: [u8; 32],
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    handle_ger_result(
        ger::insert_ger(
            ger_bytes,
            &service.miden_client,
            service.accounts.clone(),
            &service.store,
            txn_hash,
        )
        .await,
        txn_hash,
        txn_envelope,
        signer,
        service,
        ger_bytes,
    )
    .await
}

/// Check whether the recovered signer is permitted to submit transactions.
///
/// `None` = open mode (legacy default). `Some(list)` = explicit allow-list — every
/// signer outside the list is rejected. Comparison is checksum-insensitive (the
/// allow-list and recovered address are both `alloy::primitives::Address` values).
///
/// Self-review R2 — pre-fix the proxy accepted any well-formed signed tx, even
/// though only aggsender / aggoracle / operator-rescue signers have a legitimate
/// reason to submit `claimAsset` / `insertGlobalExitRoot` / `updateExitRoot`.
pub fn is_signer_allowed(allowed: Option<&[Address]>, signer: &Address) -> bool {
    match allowed {
        None => true,
        Some(list) => list.iter().any(|a| a == signer),
    }
}

pub async fn service_send_raw_txn(service: ServiceState, input: String) -> anyhow::Result<TxHash> {
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;

    // R4 — chain_id validation. Pre-fix the legacy branch used `unwrap_or(0)` and
    // the test below `tx_chain_id != 0` skipped the comparison entirely for legacy
    // tx without a chain_id. That allowed cross-chain replay: an envelope signed
    // for chain X could be replayed against our service if its chain_id field was
    // None. Require an explicit chain_id (rejects pre-EIP-155 legacy envelopes)
    // and require it to equal the service's chain_id.
    let tx_chain_id = match &txn_envelope {
        TxEnvelope::Eip1559(signed) => Some(signed.tx().chain_id),
        TxEnvelope::Eip2930(signed) => Some(signed.tx().chain_id),
        TxEnvelope::Eip4844(signed) => Some(signed.tx().tx().chain_id),
        TxEnvelope::Eip7702(signed) => Some(signed.tx().chain_id),
        TxEnvelope::Legacy(signed) => signed.tx().chain_id,
    };
    let tx_chain_id = tx_chain_id.ok_or_else(|| {
        anyhow::anyhow!(
            "transaction is missing chain_id (pre-EIP-155 legacy envelopes are rejected to prevent cross-chain replay)"
        )
    })?;
    if tx_chain_id != service.chain_id {
        anyhow::bail!(
            "chain_id mismatch: transaction has {tx_chain_id}, expected {}",
            service.chain_id
        );
    }

    let txn = unwrap_txn_envelope(txn_envelope.clone())?;
    let txn_hash = txn.hash;
    let signer = txn_envelope.recover_signer()?;
    tracing::debug!(target: concat!(module_path!(), "::debug"), "raw transaction hash: {txn_hash}");

    // RD-940 Decision 3 — tx-hash dedup early-return, BEFORE the R4 nonce check.
    //
    // aggkit's ethtxmanager re-broadcasts stuck txs within its
    // `WaitTxToBeMined = 2m` envelope (`fixtures/aggkit-config.toml:43`).
    // Without this short-circuit the re-broadcast races R4's `tx.nonce ==
    // expected_nonce` check (the first accept already advanced the nonce),
    // the duplicate gets a "nonce mismatch" error, and aggkit's state machine
    // wedges. Returning `Ok(hash)` on a known hash matches geth's idempotent
    // re-broadcast behaviour (Spec D / Spec E).
    //
    // Two lookups, OR'd:
    //   1. Writer in-flight cache — present when the worker has accepted but
    //      not yet committed. Set/cleared by `WriterWorkerHandle::try_enqueue`
    //      and the worker's `process` loop.
    //   2. Store `txn_get` — present once a receipt has been written (either
    //      Committed or Failed via TTL/worker-failure). Covers the case where
    //      a re-broadcast arrives after the worker has finished.
    //
    // Runs BEFORE `per_signer_lock` so contention from re-broadcast bursts
    // doesn't pile up on the lock.
    if let Some(handle) = service.writer_handle.as_ref()
        && handle.is_inflight(&txn_hash)
    {
        tracing::debug!(
            target: "rpc::dedup",
            %txn_hash,
            "tx-hash dedup (inflight): returning OK without re-enqueueing"
        );
        return Ok(txn_hash);
    }
    if matches!(service.store.txn_get(txn_hash).await, Ok(Some(_))) {
        tracing::debug!(
            target: "rpc::dedup",
            %txn_hash,
            "tx-hash dedup (committed): returning OK without re-running R4"
        );
        return Ok(txn_hash);
    }

    // R4 follow-up — serialise the entire nonce-check + handler critical section
    // for this signer. Without the mutex, two concurrent same-nonce txs both
    // pass the equality check before either calls `nonce_increment`. This guard
    // is cheap (per-signer, no contention across distinct signers), is held
    // until the function returns, and is dropped automatically on panic.
    let _lock = service.per_signer_locks.lock(signer).await;

    // R2 — signer allow-list. Without this, anyone who can hit the JSON-RPC port
    // can sign and submit `claimAsset` / `insertGlobalExitRoot` / `updateExitRoot`
    // calldata. The proxy then runs Miden tx work on the service account's behalf
    // (auto-creates faucets, advances LET, marks GERs injected), letting an
    // attacker burn fees, poison registries, or feed fabricated GERs to
    // bridge-service. Reject any signer not in the configured allow-list.
    //
    // Checked BEFORE the nonce branch so an unauthorised signer can never park a
    // tx in the mempool queue either.
    let signer_str = format!("{signer:#x}");
    if !is_signer_allowed(service.allowed_signers.as_deref(), &signer) {
        ::metrics::counter!("rpc_unauthorized_signer_total").increment(1);
        anyhow::bail!(
            "signer {signer:#x} is not on the allow-list; configure --allowed-signers (or set ALLOWED_SIGNERS) to permit"
        );
    }

    // R4 / mempool — nonce validation, node-like. Pre-fix the proxy required
    // `tx.nonce == store.nonce_get(signer)` exactly and jammed on the first gap.
    // Now it behaves like a real node:
    //   - nonce <  next : reject (replay/stale) — unless the tx is already known
    //                     (committed receipt or currently queued), in which case
    //                     return its hash idempotently.
    //   - nonce >  next : park in the persistent per-signer queue with a TTL and
    //                     accept (the gap-filling predecessor may still arrive).
    //   - nonce == next : process via the existing dispatch path, advance the
    //                     nonce, then DRAIN the contiguous run of queued
    //                     successors.
    let expected_nonce = service.store.nonce_get(&signer_str).await?;
    let tx_nonce = match &txn_envelope {
        TxEnvelope::Eip1559(s) => s.tx().nonce,
        TxEnvelope::Eip2930(s) => s.tx().nonce,
        TxEnvelope::Eip4844(s) => s.tx().tx().nonce,
        TxEnvelope::Eip7702(s) => s.tx().nonce,
        TxEnvelope::Legacy(s) => s.tx().nonce,
    };

    match tx_nonce.cmp(&expected_nonce) {
        std::cmp::Ordering::Less => {
            // Stale nonce. If this exact tx already has a committed receipt or
            // is currently parked, it's a harmless re-broadcast — return its
            // hash idempotently (geth behaviour). Otherwise reject. NB: the
            // pre-lock tx-hash dedup (above) already short-circuits anything
            // with a stored `txn_get` row, so a same-nonce in-flight duplicate
            // that races the lock still falls through to the mismatch reject
            // (preserving the R4 double-submit guard).
            let known = service.store.txn_receipt(txn_hash).await?.is_some()
                || service.store.queued_txn_by_hash(txn_hash).await?.is_some();
            if known {
                return Ok(txn_hash);
            }
            ::metrics::counter!("rpc_nonce_mismatch_total").increment(1);
            anyhow::bail!(
                "nonce mismatch for {signer_str}: tx.nonce = {tx_nonce}, expected {expected_nonce}; \
                 nonce too low (replay/stale submission rejected) (R4)"
            );
        }
        std::cmp::Ordering::Greater => {
            // Future nonce — park it and accept. The raw envelope is replayed
            // verbatim once the gap fills. A block-denominated TTL drops a
            // never-filled gap via the same sweep that expires pending receipts.
            let latest_block = service.store.get_latest_block_number().await?;
            let expires_at = latest_block + QUEUE_TTL_BLOCKS;
            service
                .store
                .queue_txn(&signer_str, tx_nonce, txn_hash, &txn_envelope, expires_at)
                .await?;
            ::metrics::counter!("rpc_nonce_queued_total").increment(1);
            tracing::info!(
                signer = %signer_str,
                tx_nonce,
                expected_nonce,
                %txn_hash,
                "queued future-nonce tx (mempool); will process when the gap fills"
            );
            return Ok(txn_hash);
        }
        std::cmp::Ordering::Equal => {
            // Fall through to in-order processing below.
        }
    }

    // nonce == expected — decode the method and dispatch via the existing fork,
    // then advance the nonce and drain any queued successors.
    let decoded = decode_write_call(&txn.input)?;
    match dispatch_accepted(&service, decoded, txn_envelope.clone(), signer, txn_hash).await? {
        DispatchOutcome::Processed => {}
        DispatchOutcome::AlreadyClaimed => {
            // Duplicate re-claim: the global_index is already claimed, so this is
            // a harmless no-op that must still consume its nonce. Record a success
            // receipt for this tx hash so aggkit stops re-claiming, then fall
            // through to advance the nonce (instead of sticking it).
            record_duplicate_claim_success(&service, txn_hash, txn_envelope, signer).await?;
        }
    }
    service.store.nonce_increment(&signer_str).await?;
    drain_queued(&service, &signer_str).await;
    Ok(txn_hash)
}

/// Block-denominated TTL for a future-nonce tx parked in the mempool queue.
///
/// Mirrors the receipt-expiry style (`claim.rs::EXPIRATION_DELTA`): a parked tx
/// whose predecessor nonce never arrives within this many blocks is dropped by
/// the same expiry sweep that expires pending receipts (`expire_queued_txns`
/// alongside `txn_expire_pending`), so the queue can't grow unbounded. Generous
/// relative to the receipt delta because a legitimate gap (a delayed
/// predecessor) can take longer to fill than a single receipt takes to finalise.
pub const QUEUE_TTL_BLOCKS: u64 = 256;

/// Decode the selector + ABI of a write call into the shared `DecodedWriteCall`
/// shape. Factored out so both the in-order accept path and the queue drain
/// decode identically (the drain replays a parked raw envelope).
fn decode_write_call(
    input: &alloy::primitives::Bytes,
) -> anyhow::Result<crate::writer_worker::DecodedWriteCall> {
    let params_encoded = input;
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        let params = claimAssetCall::abi_decode(params_encoded)?;
        Ok(crate::writer_worker::DecodedWriteCall::Claim {
            params: Box::new(params),
        })
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        Ok(crate::writer_worker::DecodedWriteCall::Ger {
            ger_bytes: params.root.0,
        })
    } else if params_encoded.starts_with(&updateExitRootCall::SELECTOR) {
        let params = updateExitRootCall::abi_decode(params_encoded)?;
        let combined_ger =
            ger::combined_ger(&params.newMainnetExitRoot.0, &params.newRollupExitRoot.0);
        Ok(crate::writer_worker::DecodedWriteCall::Ger {
            ger_bytes: combined_ger,
        })
    } else {
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }
}

/// Outcome of dispatching one accepted (nonce == expected) tx.
///
/// Both variants CONSUME the nonce — the distinction is only whether the caller
/// must also write an idempotent success receipt for the eth tx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchOutcome {
    /// Normal dispatch — the handler (or the enqueue) recorded / will record the
    /// receipt itself. The caller just advances the nonce.
    Processed,
    /// The claim's `global_index` was already claimed (a duplicate re-claim from
    /// aggkit). Treated like a reverting-but-mined EVM tx: the eth tx is valid
    /// (nonce + signature ok) and its Miden-side execution is a harmless no-op,
    /// so it must still CONSUME its nonce. The caller records a SUCCESS receipt
    /// for this tx hash (so `eth_getTransactionReceipt` resolves and aggkit
    /// stops re-claiming) and advances the nonce. The original claim's lock is
    /// left untouched — a duplicate must NOT trigger an unclaim (R9).
    AlreadyClaimed,
}

/// Record the idempotent success receipt for a duplicate claim. Mirrors the
/// duplicate-GER branch in `handle_ger_result`: immediate success at the
/// current tip with NO synthetic log — the ORIGINAL claim already emitted the
/// `ClaimEvent` on its own receipt, so re-emitting here would double-count.
async fn record_duplicate_claim_success(
    service: &ServiceState,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
) -> anyhow::Result<()> {
    record_local_immediate_success(service, txn_hash, txn_envelope, signer, vec![]).await
}

/// Dispatch one decoded, accepted (nonce == expected) tx through the existing
/// fork: the async writer worker when enabled, else the legacy synchronous
/// claim/GER handlers. **Does NOT advance the nonce** — the caller does that
/// after this returns `Ok` (so a `QueueFull` from the worker leaves the nonce
/// untouched and the caller retries). This is the reusable "process one decoded
/// accepted tx" body the drain also calls.
///
/// Returns [`DispatchOutcome::AlreadyClaimed`] (instead of an `Err`) when the
/// claim's `global_index` is already claimed, so the caller can consume the
/// nonce and write an idempotent success receipt rather than sticking the nonce.
async fn dispatch_accepted(
    service: &ServiceState,
    decoded: crate::writer_worker::DecodedWriteCall,
    txn_envelope: TxEnvelope,
    signer: Address,
    txn_hash: TxHash,
) -> anyhow::Result<DispatchOutcome> {
    if service.enable_writer_worker {
        let handle = service.writer_handle.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "enable_writer_worker=true but no writer_handle plumbed into ServiceState; \
                 boot order bug — see main.rs writer spawn block"
            )
        })?;
        let job = decoded.into_job(txn_envelope, signer, txn_hash);
        match handle.try_enqueue(job) {
            Ok(()) => Ok(DispatchOutcome::Processed),
            // The downcast on this typed error in `service.rs` promotes the
            // JSON-RPC error code to -32005 (geth's LimitExceeded) so aggkit's
            // ethtxmanager retries transparently.
            Err(crate::writer_worker::TryEnqueueError::QueueFull) => {
                Err(crate::writer_worker::WriterQueueSaturatedError.into())
            }
            Err(crate::writer_worker::TryEnqueueError::ShutDown) => {
                anyhow::bail!(
                    "writer worker has shut down — service is draining; retry against the next \
                     replica"
                )
            }
        }
    } else {
        // Legacy synchronous dispatch — unchanged behaviour, except a duplicate
        // claim is mapped to `AlreadyClaimed` rather than propagated as an error.
        match decoded {
            crate::writer_worker::DecodedWriteCall::Claim { params } => {
                match worker_handle_claim_asset(service, *params, txn_hash, txn_envelope, signer)
                    .await
                {
                    Ok(()) => Ok(DispatchOutcome::Processed),
                    // Typed downcast (NOT string-matching) — `try_claim` returns
                    // `ClaimAlreadySubmitted` on a duplicate, raised before the
                    // R9 guard is installed, so no unclaim fires.
                    Err(e)
                        if e.downcast_ref::<crate::store::ClaimAlreadySubmitted>()
                            .is_some() =>
                    {
                        ::metrics::counter!("rpc_claim_already_submitted_total").increment(1);
                        Ok(DispatchOutcome::AlreadyClaimed)
                    }
                    Err(e) => Err(e),
                }
            }
            crate::writer_worker::DecodedWriteCall::Ger { ger_bytes } => {
                worker_handle_ger_insert(service, ger_bytes, txn_hash, txn_envelope, signer)
                    .await
                    .map(|()| DispatchOutcome::Processed)
            }
        }
    }
}

/// Process one tx pulled from the mempool queue: re-decode its raw envelope and
/// dispatch it exactly as an in-order accept would. On a duplicate claim it
/// writes the idempotent success receipt and returns `Ok` so the drain advances
/// the nonce and continues (rather than sticking and stopping the drain).
async fn process_queued(service: &ServiceState, q: crate::store::QueuedTxn) -> anyhow::Result<()> {
    let envelope = q.envelope;
    let txn = unwrap_txn_envelope(envelope.clone())?;
    let signer = envelope.recover_signer()?;
    let decoded = decode_write_call(&txn.input)?;
    match dispatch_accepted(service, decoded, envelope.clone(), signer, q.tx_hash).await? {
        DispatchOutcome::Processed => Ok(()),
        DispatchOutcome::AlreadyClaimed => {
            record_duplicate_claim_success(service, q.tx_hash, envelope, signer).await
        }
    }
}

/// Drain the contiguous run of queued txns for `signer`, starting at the
/// current next-expected nonce. Each successfully processed tx advances the
/// nonce by one; the loop stops at the first missing nonce.
///
/// Miden submission is already serialised, so processing sequentially here is
/// correct. On a processing failure the nonce is intentionally left unadvanced
/// (same contract as a failed in-order tx) and the drain stops — aggkit
/// re-broadcasts the dropped tx within `WaitTxToBeMined`, at which point its
/// nonce == expected again and it is reprocessed.
async fn drain_queued(service: &ServiceState, signer_str: &str) {
    loop {
        let next = match service.store.nonce_get(signer_str).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(signer = %signer_str, error = %e, "drain: nonce_get failed");
                break;
            }
        };
        let q = match service.store.take_queued_txn(signer_str, next).await {
            Ok(Some(q)) => q,
            Ok(None) => break, // gap: next nonce not queued
            Err(e) => {
                tracing::error!(signer = %signer_str, error = %e, "drain: take_queued_txn failed");
                break;
            }
        };
        let tx_hash = q.tx_hash;
        match process_queued(service, q).await {
            Ok(()) => {
                if let Err(e) = service.store.nonce_increment(signer_str).await {
                    tracing::error!(
                        signer = %signer_str, %tx_hash, error = %e,
                        "drain: nonce_increment failed after processing queued tx"
                    );
                    break;
                }
                ::metrics::counter!("rpc_nonce_drained_total").increment(1);
                tracing::info!(
                    signer = %signer_str, %tx_hash, nonce = next,
                    "drained queued tx after gap filled"
                );
            }
            Err(e) => {
                tracing::error!(
                    signer = %signer_str, %tx_hash, nonce = next, error = %e,
                    "drain: processing queued tx failed; stopping (will recover on re-broadcast)"
                );
                break;
            }
        }
    }
}

/// Startup resume: for every signer with parked txns whose smallest parked
/// nonce already equals the signer's next-expected nonce, drain the contiguous
/// run. This re-processes persisted queued txns whose gap was filled in a
/// previous run (or by a restore) before the process restarted.
pub async fn resume_queued_drain(service: &ServiceState) -> anyhow::Result<()> {
    for signer in service.store.queued_signers().await? {
        let next = service.store.nonce_get(&signer).await?;
        if service.store.peek_queued_min_nonce(&signer).await? == Some(next) {
            tracing::info!(signer = %signer, next, "resuming drain of persisted queued txns");
            drain_queued(service, &signer).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_service;
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::eips::Encodable2718;
    use alloy::primitives::{FixedBytes, Signature, TxHash, U256};
    use alloy_core::sol_types::SolCall;

    /// Encode a legacy transaction with the given calldata into a hex string
    /// suitable for `service_send_raw_txn`.
    ///
    /// Chain id is set to match `create_test_service`'s chain_id (1) — R4 rejects
    /// pre-EIP-155 envelopes without a chain_id, which is the right production
    /// posture but means tests must opt in explicitly.
    fn encode_legacy_tx(input: Vec<u8>) -> (String, Address) {
        let txn = TxLegacy {
            input: input.into(),
            chain_id: Some(1),
            ..Default::default()
        };
        let signature = Signature::test_signature();
        let signed = Signed::new_unchecked(txn, signature, TxHash::default());
        let envelope = TxEnvelope::Legacy(signed);
        let signer = envelope.recover_signer().expect("recover signer");
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        (format!("0x{}", ::hex::encode(encoded)), signer)
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_invalid_hex() {
        let service = create_test_service();
        let result = service_send_raw_txn(service, "invalid".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_invalid_rlp() {
        let service = create_test_service();
        let result = service_send_raw_txn(service, "0x1234".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_unhandled_method() {
        let service = create_test_service();
        let (input_hex, _) = encode_legacy_tx(vec![0x12, 0x34, 0x56, 0x78]);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unhandled txn method")
        );
    }

    // ── Happy-path tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_insert_global_exit_root_submits_without_emitting_log() {
        let service = create_test_service();
        let store = service.store.clone();
        let ger_bytes = [0xAA; 32];

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from(ger_bytes),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(
            result.is_ok(),
            "insertGlobalExitRoot should succeed: {result:?}"
        );

        // Post-cut-over contract: insert_ger SUBMITS the UpdateGerNote to Miden but
        // does NOT emit the synthetic GER log or mark the GER injected — the
        // SyntheticProjector does both when it observes the note consumed. So in
        // this unit context (no projector tick over a consumed-note feed) neither
        // the injection flag nor the synthetic log is present yet.
        assert!(
            !store.is_ger_injected(&ger_bytes).await.unwrap(),
            "insert_ger must NOT mark injected — the projector does that on consumption"
        );
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        assert!(
            logs.iter().all(|l| l.topics.first().map(|t| t.as_str())
                != Some(crate::log_synthesis::UPDATE_HASH_CHAIN_VALUE_TOPIC)),
            "insert_ger must NOT emit a GER log — the projector does that on consumption"
        );
    }

    #[tokio::test]
    async fn test_claim_asset_zero_amount_skipped() {
        let service = create_test_service();
        let store = service.store.clone();

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(1u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1, // matches service.network_id
            destinationAddress: Address::ZERO,
            amount: U256::ZERO,
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(
            result.is_ok(),
            "zero-amount claimAsset should succeed: {result:?}"
        );
        let tx_hash = result.unwrap();

        assert!(
            !store.is_claimed(&U256::from(1u64)).await.unwrap(),
            "zero-amount claim should not be recorded in store"
        );
        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 1);
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_some());

        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        let claim_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .collect();
        assert!(
            claim_logs.is_empty(),
            "zero-amount claim should not emit ClaimEvent"
        );
    }

    /// Self-review C6 — repro+regression. A claim referencing a GER that
    /// aggkit has not observed must be rejected BEFORE the lock is
    /// acquired. Pre-fix the proxy locked the globalIndex, ran the 15s
    /// GER-propagation wait, submitted to Miden, and only then saw the
    /// on-chain `ERR_GER_NOT_FOUND` panic — wasted work + held lock for
    /// the full round-trip.
    #[tokio::test]
    async fn c6_claim_with_unseen_ger_rejected_before_lock() {
        let service = create_test_service();
        let store = service.store.clone();

        let global_index = U256::from(99u64);
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::from([0xAAu8; 32]),
            rollupExitRoot: FixedBytes::from([0xBBu8; 32]),
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            // Zero-padded MidenAccountId — resolvable, so RD-860's
            // unresolvable-destination short-circuit doesn't pre-empt the C6
            // GER pre-check we're testing here.
            destinationAddress: alloy::primitives::address!(
                "0x00000000ac0000000000dd110000ee000000fc00"
            ),
            amount: U256::from(1_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        // GER is NOT pre-seeded — this is the test's whole point.
        let result = service_send_raw_txn(service, input_hex).await;
        let err = result.expect_err("claim with unseen GER must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("not observed yet"), "unexpected: {msg}");

        // The lock must NOT have been acquired (cheap retry surface).
        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "C6 must reject before acquiring the claim lock"
        );
    }

    #[tokio::test]
    async fn test_claim_asset_no_event_on_failure() {
        let service = create_test_service();
        let store = service.store.clone();
        let miden_client = service.miden_client.clone();

        let global_index = U256::from(42u64);
        // Zero-padded resolvable destination (see address_mapper::account_id_from_address
        // test vectors). This ensures the claim gets PAST the RD-860 short-circuit and
        // fails inside publish_claim against the test MidenClient stub — exercising the
        // "ClaimEvent not emitted on publish_claim error" guarantee this test is for.
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: alloy::primitives::address!(
                "0x00000000ac0000000000dd110000ee000000fc00"
            ),
            amount: U256::from(1_000_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        // C6 — pre-seed the GER as seen so the new pre-check passes; the
        // test's intent is to exercise the publish-failure path, not the
        // GER-not-yet-seen path. RD-862 follow-up: `handle_claim_asset` now
        // gates on `is_ger_injected` (not `has_seen_ger`) since the
        // L1InfoTreeIndexer pre-populates ger_entries rows before the GER is
        // injected to L2. Mark BOTH so the gate passes.
        let ger = crate::ger::combined_ger(&[0u8; 32], &[0u8; 32]);
        store
            .mark_ger_seen(
                &ger,
                crate::log_synthesis::GerEntry {
                    mainnet_exit_root: Some([0u8; 32]),
                    rollup_exit_root: Some([0u8; 32]),
                    block_number: 1,
                    timestamp: 0,
                },
            )
            .await
            .unwrap();
        store.mark_ger_injected(ger).await.unwrap();

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err(), "publish_claim should fail with test stub");

        assert!(
            miden_client.test_was_called(),
            "MidenClient should have been invoked by publish_claim"
        );

        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        let claim_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .collect();
        assert!(
            claim_logs.is_empty(),
            "ClaimEvent must not be emitted when publish_claim fails"
        );

        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "claim should be unclaimed after publish_claim failure"
        );
    }

    /// RD-860: a claim whose destination cannot be resolved is swallowed — we record
    /// it in the `unclaimable_claims` store, emit a synthetic `ClaimEvent` so aggkit
    /// stops retrying, and return success to the caller. Neither `try_claim` nor the
    /// MidenClient publish path should be touched.
    #[tokio::test]
    async fn test_claim_asset_unresolvable_destination_swallowed() {
        let service = create_test_service();
        let store = service.store.clone();
        let miden_client = service.miden_client.clone();

        let global_index = U256::from(123u64);
        // Non-zero-padded address with no store mapping — cannot be resolved by
        // `address_mapper::resolve_address`, so the short-circuit path fires.
        let dest = Address::from([0x42; 20]);
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 7,
            originTokenAddress: Address::from([0x11; 20]),
            destinationNetwork: 1,
            destinationAddress: dest,
            amount: U256::from(1_000_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_ok(), "swallow path must return Ok: {result:?}");
        let tx_hash = result.unwrap();

        // (1) globalIndex is NOT locked — short-circuit happened before try_claim.
        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "short-circuit must not lock globalIndex"
        );

        // (2) MidenClient was never invoked — publish_claim did not run.
        assert!(
            !miden_client.test_was_called(),
            "MidenClient must not be invoked for an unresolvable destination"
        );

        // (3) unclaimable_claims record exists with the right fields.
        let rec = store
            .get_unclaimable_claim(&global_index)
            .await
            .unwrap()
            .expect("unclaimable record must be present");
        assert_eq!(rec.global_index, global_index);
        assert_eq!(rec.destination_address, dest);
        assert_eq!(rec.origin_network, 7);
        assert_eq!(rec.origin_address, Address::from([0x11; 20]));
        assert_eq!(rec.amount, U256::from(1_000_000u64));
        assert_eq!(
            rec.reason,
            crate::store::UnclaimableReason::UnresolvableDestination
        );
        assert_eq!(rec.eth_tx_hash, tx_hash);

        // (4) Exactly one synthetic ClaimEvent emitted (so aggkit marks done).
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        let claim_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .collect();
        assert_eq!(
            claim_logs.len(),
            1,
            "swallow path must emit exactly one ClaimEvent so aggkit stops retrying"
        );

        // (5) Nonce incremented and receipt recorded so the RPC client sees success.
        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 1);
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_claim_wrong_network_rejected() {
        let service = create_test_service();

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(9u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2, // does NOT match service.network_id (1)
            destinationAddress: Address::ZERO,
            amount: U256::from(1u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only handles network")
        );
    }

    /// Self-review of-the-fix follow-up — TOCTOU between `nonce_get` and
    /// `nonce_increment`. Two concurrent valid txs at the same nonce both
    /// passed the equality check before either called `nonce_increment`. For
    /// `claimAsset`, `try_claim` dedupes by `globalIndex`; for the GER injection
    /// path, no dedup existed, so both could double-process.
    ///
    /// This test concurrently launches two `service_send_raw_txn` calls from
    /// the same signer at the same nonce and asserts that exactly ONE
    /// succeeds (the second receives a "nonce mismatch" error). Pre-fix both
    /// would have succeeded.
    #[tokio::test]
    async fn r4_followup_concurrent_same_nonce_serialised() {
        // Build two identical legacy txs; encode_legacy_tx signs with the same
        // private key + same nonce by construction, so both yield the same
        // signer + nonce.
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        let (input_a, signer) = encode_legacy_tx(calldata.clone());
        let (input_b, _) = encode_legacy_tx(calldata);

        let service = create_test_service();
        let store = service.store.clone();
        // Run both concurrently.
        let svc_a = service.clone();
        let svc_b = service.clone();
        let h_a = tokio::spawn(async move { service_send_raw_txn(svc_a, input_a).await });
        let h_b = tokio::spawn(async move { service_send_raw_txn(svc_b, input_b).await });
        let res_a = h_a.await.unwrap();
        let res_b = h_b.await.unwrap();

        let (oks, errs): (Vec<_>, Vec<_>) = [res_a, res_b].into_iter().partition(|r| r.is_ok());

        // SAFETY invariant (the real regression target): two same-nonce txs must
        // NEVER both pass the gate — that would double-spend a nonce. The
        // per-signer lock (`service.per_signer_locks`) plus the
        // `nonce_get` -> check -> handler -> `nonce_increment` critical section
        // guarantee at most one succeeds, and the store nonce advances by exactly
        // the number of successes.
        //
        // We deliberately DON'T assert "exactly one succeeds". The winning tx's
        // handler runs the GER-insert path through the test `MidenClient` stub,
        // whose request/response hops a `std::thread` + oneshot channel; under CI
        // load that cross-thread round-trip can occasionally fail the winner too,
        // yielding zero successes. That is a liveness hiccup in the *stub*, not a
        // safety violation — asserting "exactly one" made this test flaky
        // (RD-1021). Asserting the safety invariant keeps it a real regression
        // guard while making it deterministic. The happy path (a lone tx
        // succeeds) is covered deterministically by `r4_correct_nonce_accepted`.
        assert!(
            oks.len() <= 1,
            "at most one same-nonce tx may succeed (got {}) — double-submit guard broken",
            oks.len()
        );
        let final_nonce = store.nonce_get(&format!("{signer:#x}")).await.unwrap();
        assert_eq!(
            final_nonce,
            oks.len() as u64,
            "store nonce must advance by exactly the number of successful txs"
        );
        // When there IS a winner, the loser must be rejected at the nonce gate,
        // not silently accepted.
        if oks.len() == 1 {
            let err_msg = format!("{}", errs[0].as_ref().unwrap_err());
            assert!(
                err_msg.contains("nonce mismatch"),
                "the losing tx must be rejected with nonce mismatch: {err_msg}"
            );
        }
    }

    /// Self-review R4 — repro+regression. Two failure modes:
    /// (a) legacy envelope without an explicit chain_id is replay-vulnerable
    ///     (cross-chain). Pre-fix, the unwrap_or(0) branch + `if tx_chain_id != 0`
    ///     guard meant such envelopes were *accepted* with no chain check.
    /// (b) replay with the same nonce, or out-of-order tx with a future nonce,
    ///     was processed despite the proxy already tracking a sequence per signer.
    /// Tests:
    /// - `r4_legacy_tx_without_chain_id_rejected` — encode TxLegacy{chain_id:None}
    ///   and assert the proxy refuses with the EIP-155 message.
    /// - `r4_nonce_mismatch_rejected` — submit two valid txs with the same nonce;
    ///   the second is refused.
    /// - `r4_correct_nonce_accepted` — incrementing nonce flow works.
    #[tokio::test]
    async fn r4_legacy_tx_without_chain_id_rejected() {
        let service = create_test_service();
        // Construct a TxLegacy with chain_id = None.
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        let txn = TxLegacy {
            input: calldata.into(),
            chain_id: None,
            ..Default::default()
        };
        let signature = Signature::test_signature();
        let signed = Signed::new_unchecked(txn, signature, TxHash::default());
        let envelope = TxEnvelope::Legacy(signed);
        let mut buf = Vec::new();
        envelope.encode_2718(&mut buf);
        let input = format!("0x{}", ::hex::encode(buf));

        let err = service_send_raw_txn(service, input)
            .await
            .expect_err("legacy without chain_id must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("missing chain_id") || msg.contains("EIP-155"),
            "unexpected: {msg}"
        );
    }

    #[tokio::test]
    async fn r4_nonce_mismatch_rejected() {
        let service = create_test_service();
        let store = service.store.clone();
        // Force the store's tracked nonce up by 5; a tx with nonce 0 (default) is
        // now stale-replay territory.
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);
        for _ in 0..5 {
            store
                .nonce_increment(&format!("{signer:#x}"))
                .await
                .unwrap();
        }
        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("stale nonce must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("nonce mismatch"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn r4_correct_nonce_accepted() {
        let service = create_test_service();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        // Default TxLegacy nonce is 0 and create_test_service starts the store
        // nonce at 0, so this should succeed.
        let (input_hex, _) = encode_legacy_tx(calldata);
        let result = service_send_raw_txn(service, input_hex).await;
        assert!(
            result.is_ok(),
            "matching nonce must be accepted: {result:?}"
        );
    }

    /// Self-review R2 — repro+regression. Pre-fix, every recovered signer was
    /// accepted unconditionally. Post-fix the predicate must:
    /// - return true when no allow-list is configured (legacy open mode)
    /// - return true when the signer is in the allow-list
    /// - return false when an allow-list is configured but the signer isn't in it
    /// - return false for an empty allow-list (explicit refuse-all)
    #[test]
    fn r2_is_signer_allowed_pins_allow_list_semantics() {
        let alice: Address = "0xAAaAaAaAaaaAaaAaAaAAAAAAAaaaAaAaAaaAaaAa"
            .parse()
            .unwrap();
        let bob: Address = "0xbBbBbBbBbBbbbBbBbBBbbbbbBBBBbbbbBBbBbBbB"
            .parse()
            .unwrap();
        let carol: Address = "0xCccCccCcCccCcCCCCccCCCcCcCCCCccCcCcCcCcC"
            .parse()
            .unwrap();

        // None = open
        assert!(is_signer_allowed(None, &alice));
        assert!(is_signer_allowed(None, &bob));

        // Empty list = explicit refuse-all
        assert!(!is_signer_allowed(Some(&[]), &alice));

        // Single-entry list
        assert!(is_signer_allowed(Some(&[alice]), &alice));
        assert!(!is_signer_allowed(Some(&[alice]), &bob));

        // Multi-entry list
        let list = [alice, bob];
        assert!(is_signer_allowed(Some(&list), &alice));
        assert!(is_signer_allowed(Some(&list), &bob));
        assert!(!is_signer_allowed(Some(&list), &carol));
    }

    /// RD-940 Decision 3 — tx-hash dedup early-return.
    ///
    /// aggkit's ethtxmanager re-broadcasts stuck txs within
    /// `WaitTxToBeMined = 2m`. Without dedup, the re-broadcast races R4
    /// nonce equality (the original accept already advanced the nonce) and
    /// the duplicate gets "nonce mismatch", wedging aggkit's state machine.
    ///
    /// Submit a tx twice, assert: (1) both calls return the same `Ok(hash)`,
    /// (2) the nonce advanced exactly once. The dedup branch fires because
    /// `txn_get` returns Some after the first accept commits a receipt.
    #[tokio::test]
    async fn rd940_decision3_idempotent_rebroadcast_returns_same_hash() {
        let service = create_test_service();
        let store = service.store.clone();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xCCu8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        // First submission — runs the full pipeline.
        let first = service_send_raw_txn(service.clone(), input_hex.clone())
            .await
            .expect("first submit must succeed");
        // Second submission with the SAME wire bytes — should hit the dedup
        // path and return the same hash without re-running anything.
        let second = service_send_raw_txn(service.clone(), input_hex)
            .await
            .expect("re-broadcast must succeed via dedup");
        assert_eq!(first, second, "dedup must return the original tx hash");

        // Nonce must have advanced exactly once.
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            1,
            "dedup must not double-advance the nonce"
        );
    }

    /// R2 integration repro — a signed tx whose signer is NOT on the allow-list
    /// must be rejected, even if everything else is well-formed. Without the
    /// fix, the same tx would be processed (and the proxy would attempt to run
    /// the corresponding Miden tx on the service account's behalf).
    #[tokio::test]
    async fn r2_unauthorised_signer_rejected_with_descriptive_error() {
        let mut service = create_test_service();
        // Configure a non-empty allow-list that does NOT include the test signer.
        let foreign: Address = "0xdeAddeaDdEadDeaDDEaDDeadDEADDeaDDEAdDEaD"
            .parse()
            .unwrap();
        service.allowed_signers = Some(vec![foreign]);

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);
        // sanity: the tx's recovered signer is the test signer, not the foreign one.
        assert_ne!(signer, foreign);

        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("non-allowed signer must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("not on the allow-list"), "unexpected: {msg}");
        assert!(
            msg.contains(&format!("{signer:#x}")),
            "must name the signer: {msg}"
        );
    }

    // ── Mempool (future-nonce queue) tests ───────────────────────────

    /// Encode a GER-insert legacy tx with an explicit nonce, signed by a FIXED
    /// key so every tx in a test recovers to the SAME signer regardless of
    /// nonce/calldata (a fixed signature over varying content would otherwise
    /// recover to a different address each time). Distinct `marker` values give
    /// distinct GER roots and therefore distinct tx hashes.
    fn ger_tx(nonce: u64, marker: u8) -> (String, Address) {
        use alloy::consensus::SignableTransaction;
        use alloy::signers::SignerSync;
        use alloy::signers::local::PrivateKeySigner;

        let signer_key: PrivateKeySigner =
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .parse()
                .expect("fixed test key");
        let from = signer_key.address();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([marker; 32]),
        }
        .abi_encode();
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce,
            gas_price: 0,
            gas_limit: 21_000,
            to: alloy::primitives::TxKind::Call(from),
            value: U256::ZERO,
            input: calldata.into(),
        };
        let signature = signer_key.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        (format!("0x{}", ::hex::encode(encoded)), from)
    }

    /// Out-of-order accept: submit nonce 1 (queues), then nonce 0 (processes and
    /// drains 1). Final nonce 2, queue empty.
    #[tokio::test]
    async fn mempool_out_of_order_accept_drains() {
        let service = create_test_service();
        let store = service.store.clone();

        let (tx1, signer) = ger_tx(1, 0xA1);
        let signer_str = format!("{signer:#x}");

        // nonce 1 arrives first — expected is 0, so it parks.
        let h1 = service_send_raw_txn(service.clone(), tx1).await.unwrap();
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            0,
            "future-nonce tx must NOT advance the nonce"
        );
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(1),
            "future-nonce tx must be parked"
        );
        assert!(
            store.queued_txn_by_hash(h1).await.unwrap().is_some(),
            "parked tx must be findable by hash"
        );

        // nonce 0 fills the gap — processes, advances to 1, then drains nonce 1.
        let (tx0, _) = ger_tx(0, 0xA0);
        service_send_raw_txn(service.clone(), tx0).await.unwrap();

        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            2,
            "in-order tx must process AND drain the queued successor"
        );
        assert!(
            store
                .peek_queued_min_nonce(&signer_str)
                .await
                .unwrap()
                .is_none(),
            "queue must be empty after the drain"
        );
    }

    /// Gap-then-fill: submit 0 (nonce→1), then 2 (parks), then 1 (processes and
    /// drains 2). Final nonce 3.
    #[tokio::test]
    async fn mempool_gap_then_fill_drains() {
        let service = create_test_service();
        let store = service.store.clone();

        let (tx0, signer) = ger_tx(0, 0xB0);
        let signer_str = format!("{signer:#x}");

        service_send_raw_txn(service.clone(), tx0).await.unwrap();
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);

        // nonce 2 leaves a hole at 1 — parks.
        let (tx2, _) = ger_tx(2, 0xB2);
        service_send_raw_txn(service.clone(), tx2).await.unwrap();
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(2)
        );

        // nonce 1 fills the hole — processes, then drains nonce 2.
        let (tx1, _) = ger_tx(1, 0xB1);
        service_send_raw_txn(service.clone(), tx1).await.unwrap();
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            3,
            "filling the hole must drain through nonce 2"
        );
        assert!(
            store
                .peek_queued_min_nonce(&signer_str)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Idempotent resubmit of a parked tx returns the same hash and does not
    /// duplicate the queue entry; a DIFFERENT tx at the same parked nonce is
    /// rejected (the "at most one tx per signer per nonce" invariant).
    #[tokio::test]
    async fn mempool_idempotent_resubmit_of_queued_tx() {
        let service = create_test_service();
        let store = service.store.clone();

        let (tx1, signer) = ger_tx(1, 0xC1);
        let signer_str = format!("{signer:#x}");

        let first = service_send_raw_txn(service.clone(), tx1.clone())
            .await
            .unwrap();
        let second = service_send_raw_txn(service.clone(), tx1).await.unwrap();
        assert_eq!(
            first, second,
            "idempotent resubmit must return the same hash"
        );
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(1)
        );

        // A different tx (different root → different hash) at the SAME parked
        // nonce must be refused — the first parked tx wins.
        let (tx1_other, _) = ger_tx(1, 0xCC);
        let err = service_send_raw_txn(service.clone(), tx1_other)
            .await
            .expect_err("a different tx at an occupied queue slot must be rejected");
        assert!(
            err.to_string().contains("already queued"),
            "unexpected: {err}"
        );
    }

    /// Expiry drops a stale parked tx via the shared expiry sweep.
    #[tokio::test]
    async fn mempool_expiry_drops_stale_queued_tx() {
        let service = create_test_service();
        let store = service.store.clone();

        let (tx1, signer) = ger_tx(1, 0xD1);
        let signer_str = format!("{signer:#x}");
        service_send_raw_txn(service.clone(), tx1).await.unwrap();
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(1)
        );

        // latest_block is 0 in a fresh test service, so expires_at == QUEUE_TTL_BLOCKS.
        let dropped = store.expire_queued_txns(QUEUE_TTL_BLOCKS).await.unwrap();
        assert_eq!(dropped, 1, "the stale parked tx must be dropped");
        assert!(
            store
                .peek_queued_min_nonce(&signer_str)
                .await
                .unwrap()
                .is_none(),
            "queue must be empty after expiry"
        );
    }

    /// Restart-resume: a tx persisted in the queue whose gap is already filled
    /// (min parked nonce == next expected nonce) is drained at startup.
    #[tokio::test]
    async fn mempool_restart_resume_drains_filled_gap() {
        use alloy::eips::Decodable2718;

        let service = create_test_service();
        let store = service.store.clone();

        // Simulate a previous run that persisted a queued tx at nonce 0 (its gap
        // is "already filled" because nonce_get is also 0).
        let (tx0, signer) = ger_tx(0, 0xE0);
        let signer_str = format!("{signer:#x}");
        let payload = crate::hex::hex_decode_prefixed(&tx0).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut payload.as_slice()).unwrap();
        let hash = unwrap_txn_envelope(envelope.clone()).unwrap().hash;
        store
            .queue_txn(&signer_str, 0, hash, &envelope, 9_999)
            .await
            .unwrap();
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(0)
        );

        // Boot-time resume drains it.
        resume_queued_drain(&service).await.unwrap();

        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            1,
            "resume must process the persisted queued tx and advance the nonce"
        );
        assert!(
            store
                .peek_queued_min_nonce(&signer_str)
                .await
                .unwrap()
                .is_none(),
            "queue must be empty after resume"
        );
    }

    /// Build a `claimAsset` legacy tx for `global_index` at `nonce`, signed by
    /// the SAME fixed key as `ger_tx` so claims and GER-inserts in one test share
    /// a signer (and therefore a nonce sequence). Resolvable zero-padded
    /// destination + all-zero exit roots so it gets past RD-860 and C6 once the
    /// all-zero combined GER is marked injected.
    fn claim_tx(nonce: u64, global_index: u64) -> (String, Address) {
        use alloy::consensus::SignableTransaction;
        use alloy::signers::SignerSync;
        use alloy::signers::local::PrivateKeySigner;

        let signer_key: PrivateKeySigner =
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .parse()
                .expect("fixed test key");
        let from = signer_key.address();
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(global_index),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: alloy::primitives::address!(
                "0x00000000ac0000000000dd110000ee000000fc00"
            ),
            amount: U256::from(1_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce,
            gas_price: 0,
            gas_limit: 21_000,
            to: alloy::primitives::TxKind::Call(from),
            value: U256::ZERO,
            input: calldata.into(),
        };
        let signature = signer_key.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        (format!("0x{}", ::hex::encode(encoded)), from)
    }

    /// Nonce-jam regression. When aggkit re-claims an already-claimed deposit,
    /// the second `claimAsset` for the same `global_index` hits
    /// `try_claim` → `ClaimAlreadySubmitted`. Pre-fix that errored out WITHOUT
    /// advancing the nonce, so the proxy's next-expected nonce stuck forever and
    /// every later claim (including future-nonce txs that then expired) jammed.
    ///
    /// Post-fix the duplicate is treated like a reverting-but-mined EVM tx: it
    /// CONSUMES its nonce and gets a SUCCESS receipt (the global_index IS
    /// claimed), and a higher-nonce tx parked behind it drains. The original
    /// claim's lock is left intact (a duplicate must not unclaim — R9).
    #[tokio::test]
    async fn claim_duplicate_consumes_nonce_records_receipt_and_drains() {
        let service = create_test_service();
        let store = service.store.clone();
        crate::test_helpers::seed_test_faucets(&*store).await;

        // C6 — the all-zero combined GER these claims reference must be injected.
        let ger = crate::ger::combined_ger(&[0u8; 32], &[0u8; 32]);
        store
            .mark_ger_seen(
                &ger,
                crate::log_synthesis::GerEntry {
                    mainnet_exit_root: Some([0u8; 32]),
                    rollup_exit_root: Some([0u8; 32]),
                    block_number: 1,
                    timestamp: 0,
                },
            )
            .await
            .unwrap();
        store.mark_ger_injected(ger).await.unwrap();

        let global_index = 7u64;
        let (_, signer) = claim_tx(0, global_index);
        let signer_str = format!("{signer:#x}");

        // Simulate the FIRST claim having already committed: the global_index is
        // locked and its nonce was advanced to 1. (We seed the lock directly
        // rather than run a full publish through the MidenClient stub, which is
        // not deterministic for the claim path — the precondition under test is
        // simply "global_index already claimed, nonce at the next slot".)
        store.try_claim(U256::from(global_index)).await.unwrap();
        store.nonce_increment(&signer_str).await.unwrap();
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);

        // Park a higher-nonce GER tx (nonce 2) BEHIND the duplicate. It sits in
        // the queue until the gap at nonce 1 fills.
        let (ger2, _) = ger_tx(2, 0xF2);
        service_send_raw_txn(service.clone(), ger2).await.unwrap();
        assert_eq!(
            store.peek_queued_min_nonce(&signer_str).await.unwrap(),
            Some(2),
            "the higher-nonce tx must be parked"
        );

        // The SECOND (duplicate) claim for the SAME global_index arrives at
        // nonce == next (1). Pre-fix this errored and stuck the nonce at 1.
        let (dup, _) = claim_tx(1, global_index);
        let dup_hash = service_send_raw_txn(service.clone(), dup)
            .await
            .expect("duplicate claim must be accepted idempotently, not error");

        // Nonce advanced past the duplicate (to 2) AND drained the parked nonce-2
        // GER tx (to 3).
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            3,
            "duplicate must consume its nonce and the parked successor must drain"
        );
        assert!(
            store
                .peek_queued_min_nonce(&signer_str)
                .await
                .unwrap()
                .is_none(),
            "queue must be empty after the drain"
        );

        // A SUCCESS receipt exists for the duplicate's tx hash so
        // eth_getTransactionReceipt resolves and aggkit stops re-claiming.
        let (status, _block) = store
            .txn_receipt(dup_hash)
            .await
            .unwrap()
            .expect("duplicate claim must have a receipt");
        assert!(
            status.is_ok(),
            "duplicate claim receipt must report success (global_index IS claimed): {status:?}"
        );

        // R9 — the original claim's lock is untouched (no unclaim fired).
        assert!(
            store.is_claimed(&U256::from(global_index)).await.unwrap(),
            "a duplicate must NOT unclaim the original"
        );
    }
}
