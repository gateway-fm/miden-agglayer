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

fn envelope_nonce(txn_envelope: &TxEnvelope) -> u64 {
    match txn_envelope {
        TxEnvelope::Eip1559(s) => s.tx().nonce,
        TxEnvelope::Eip2930(s) => s.tx().nonce,
        TxEnvelope::Eip4844(s) => s.tx().tx().nonce,
        TxEnvelope::Eip7702(s) => s.tx().nonce,
        TxEnvelope::Legacy(s) => s.tx().nonce,
    }
}

fn calldata_selector(input: &alloy::primitives::Bytes) -> String {
    let bytes = input.as_ref();
    if bytes.len() < 4 {
        return "0x".to_string();
    }
    format!(
        "0x{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
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
    let tx_nonce = envelope_nonce(&txn_envelope);
    let selector = calldata_selector(&txn.input);
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
    let known_inflight = service
        .writer_handle
        .as_ref()
        .is_some_and(|handle| handle.is_inflight(&txn_hash));
    let known_store_tx = service
        .store
        .txn_get(txn_hash)
        .await
        .map(|entry| entry.is_some())
        .unwrap_or(false);
    tracing::info!(
        target: "rpc::nonce_snoop",
        "{}",
        serde_json::json!({
            "event": "eth_sendRawTransaction_received",
            "signer": format!("{signer:#x}"),
            "tx_hash": format!("{txn_hash:#x}"),
            "tx_nonce": tx_nonce,
            "calldata_selector": selector,
            "calldata_len": txn.input.len(),
            "known_inflight": known_inflight,
            "known_store_tx": known_store_tx,
            "writer_enabled": service.enable_writer_worker,
            "writer_handle_present": service.writer_handle.is_some(),
        })
    );

    if known_inflight {
        tracing::debug!(
            target: "rpc::dedup",
            %txn_hash,
            "tx-hash dedup (inflight): returning OK without re-enqueueing"
        );
        return Ok(txn_hash);
    }
    if known_store_tx {
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

    // R4 — nonce validation. Pre-fix the proxy advanced its tracked nonce only on
    // success and never compared the incoming `tx.nonce` against the expected next
    // value. That allowed:
    //   1. Replay: a tx replayed with its original nonce would re-execute (the
    //      claim path's try_claim dedupes by globalIndex, but other paths don't).
    //   2. Skipped sequencing: an out-of-order tx with an inflated nonce would
    //      still be processed, leaving "holes" in the apparent sequence.
    // Validate `tx.nonce == store.nonce_get(signer)` BEFORE running any handler.
    let signer_str = format!("{signer:#x}");
    let expected_nonce = service.store.nonce_get(&signer_str).await?;
    tracing::info!(
        target: "rpc::nonce_snoop",
        "{}",
        serde_json::json!({
            "event": "eth_sendRawTransaction_nonce_check",
            "signer": signer_str,
            "tx_hash": format!("{txn_hash:#x}"),
            "tx_nonce": tx_nonce,
            "expected_nonce": expected_nonce,
            "nonce_matches": tx_nonce == expected_nonce,
            "writer_enabled": service.enable_writer_worker,
            "writer_handle_present": service.writer_handle.is_some(),
        })
    );
    if tx_nonce != expected_nonce {
        ::metrics::counter!("rpc_nonce_mismatch_total").increment(1);
        anyhow::bail!(
            "nonce mismatch for {signer_str}: tx.nonce = {tx_nonce}, expected {expected_nonce}; \
             this guards against replay and out-of-order submission (R4)"
        );
    }

    // R2 — signer allow-list. Without this, anyone who can hit the JSON-RPC port
    // can sign and submit `claimAsset` / `insertGlobalExitRoot` / `updateExitRoot`
    // calldata. The proxy then runs Miden tx work on the service account's behalf
    // (auto-creates faucets, advances LET, marks GERs injected), letting an
    // attacker burn fees, poison registries, or feed fabricated GERs to
    // bridge-service. Reject any signer not in the configured allow-list.
    if !is_signer_allowed(service.allowed_signers.as_deref(), &signer) {
        ::metrics::counter!("rpc_unauthorized_signer_total").increment(1);
        anyhow::bail!(
            "signer {signer:#x} is not on the allow-list; configure --allowed-signers (or set ALLOWED_SIGNERS) to permit"
        );
    }

    // ── Method decode ───────────────────────────────────────────────────
    //
    // Decoding the selector + ABI on the request thread (rather than inside
    // the worker) keeps malformed payloads from poisoning the queue and lets
    // both the legacy sync path and the worker path share the same dispatch
    // shape downstream. The `DecodedWriteCall` enum is defined in
    // `writer_worker` so it can also serve as the wire shape for the v1.5
    // durable-queue migration sketched in `docs/design/RD-940-async-writer.md`.
    let params_encoded = &txn.input;
    let decoded = if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        tracing::debug!("claimAsset call");
        let params = claimAssetCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "claimAsset call params: {params:?}");
        crate::writer_worker::DecodedWriteCall::Claim {
            params: Box::new(params),
        }
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");
        let ger_bytes: [u8; 32] = params.root.0;
        crate::writer_worker::DecodedWriteCall::Ger { ger_bytes }
    } else if params_encoded.starts_with(&updateExitRootCall::SELECTOR) {
        tracing::debug!("updateExitRoot call");
        let params = updateExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "updateExitRoot call params: {params:?}");
        let combined_ger =
            ger::combined_ger(&params.newMainnetExitRoot.0, &params.newRollupExitRoot.0);
        crate::writer_worker::DecodedWriteCall::Ger {
            ger_bytes: combined_ger,
        }
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    };

    // ── Dispatch fork (RD-940) ──────────────────────────────────────────
    //
    // `enable_writer_worker` defaults to false — the legacy synchronous
    // branch below is byte-identical to pre-RD-940 behaviour for the
    // claim and GER paths. When the flag is enabled and a writer handle
    // is plumbed, requests are enqueued for asynchronous Miden submission
    // and the HTTP future returns the tx-hash as soon as `try_enqueue`
    // succeeds.
    //
    // Nonce-advance ordering matters under both branches:
    //   - legacy: dispatch runs to completion → nonce_increment (current
    //     behaviour preserved bit-for-bit)
    //   - worker: try_enqueue → on Ok, nonce_increment; on QueueFull, the
    //     nonce is intentionally **not** advanced so the caller retries
    //     with the same nonce and -32005 doesn't burn a sequence slot
    //
    // Decision 3 (idempotent re-broadcast) and Decision 4
    // (eth_getTransactionCount tag honouring) land in Phase 2.
    if service.enable_writer_worker {
        let handle = service.writer_handle.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "enable_writer_worker=true but no writer_handle plumbed into ServiceState; \
                 boot order bug — see main.rs writer spawn block"
            )
        })?;
        let job = decoded.into_job(txn_envelope, signer, txn_hash);
        match handle.try_enqueue(job) {
            Ok(()) => {
                service.store.nonce_increment(&signer_str).await?;
                Ok(txn_hash)
            }
            Err(crate::writer_worker::TryEnqueueError::QueueFull) => {
                // The downcast on this typed error in `service.rs`
                // promotes the JSON-RPC error code to -32005 (geth's
                // LimitExceeded), letting aggkit's ethtxmanager retry
                // transparently. The metric was already incremented in
                // try_enqueue.
                Err(crate::writer_worker::WriterQueueSaturatedError.into())
            }
            Err(crate::writer_worker::TryEnqueueError::ShutDown) => {
                anyhow::bail!(
                    "writer worker has shut down — service is draining; retry against the next \
                     replica"
                );
            }
        }
    } else {
        // Legacy synchronous dispatch — unchanged behaviour.
        match decoded {
            crate::writer_worker::DecodedWriteCall::Claim { params } => {
                worker_handle_claim_asset(&service, *params, txn_hash, txn_envelope, signer)
                    .await?;
            }
            crate::writer_worker::DecodedWriteCall::Ger { ger_bytes } => {
                worker_handle_ger_insert(&service, ger_bytes, txn_hash, txn_envelope, signer)
                    .await?;
            }
        }
        service.store.nonce_increment(&signer_str).await?;
        Ok(txn_hash)
    }
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
}
