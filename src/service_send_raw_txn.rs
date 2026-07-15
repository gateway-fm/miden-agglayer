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

pub(crate) fn envelope_nonce(txn_envelope: &TxEnvelope) -> u64 {
    crate::store::envelope_nonce(txn_envelope)
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

pub(crate) fn decode_write_call(
    params_encoded: &alloy::primitives::Bytes,
) -> anyhow::Result<crate::writer_worker::DecodedWriteCall> {
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        tracing::debug!("claimAsset call");
        let params = claimAssetCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "claimAsset call params: {params:?}");
        Ok(crate::writer_worker::DecodedWriteCall::Claim {
            params: Box::new(params),
        })
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");
        Ok(crate::writer_worker::DecodedWriteCall::Ger {
            ger_bytes: params.root.0,
        })
    } else if params_encoded.starts_with(&updateExitRootCall::SELECTOR) {
        tracing::debug!("updateExitRoot call");
        let params = updateExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "updateExitRoot call params: {params:?}");
        Ok(crate::writer_worker::DecodedWriteCall::Ger {
            ger_bytes: ger::combined_ger(&params.newMainnetExitRoot.0, &params.newRollupExitRoot.0),
        })
    } else {
        anyhow::bail!("unhandled txn method {params_encoded:?}")
    }
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

pub(crate) fn decode_envelope_write_call(
    txn_envelope: &TxEnvelope,
) -> anyhow::Result<crate::writer_worker::DecodedWriteCall> {
    let transaction = unwrap_txn_envelope(txn_envelope.clone())?;
    decode_write_call(&transaction.input)
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
                // New GER: insert_ger's serialized-client closure
                // (`ger::record_ger_submission_handoff`) records BOTH the
                // eth-tx ↔ UpdateGerNote link AND the pending receipt
                // (txn_begin) behind the projection-exclusion boundary — so
                // the SyntheticProjector can never resolve a consumed note's
                // real linked hash to a receipt that was never durably begun
                // (the review guarantee; pre-fix the pending row was created
                // out here, AFTER the client was released, and the projector
                // could tick in that gap, silently finalise zero rows on
                // PostgreSQL, and the late row then stayed pending forever).
                // The projector finalises the receipt (txn_commit) at the
                // Miden block where it consumes the note — receipt block ==
                // GER-log block; eth_getTransactionReceipt returns null until
                // then (mined-when-consumed), which aggkit tolerates.
                //
                // RD-940 Decision 3 dedup independence: that handoff runs
                // INSIDE the serialized Miden client, so its row is not
                // guaranteed to be `txn_get`-findable on the accept path by
                // the time we return the hash (the writer worker runs the
                // closure asynchronously; the client stub in tests skips the
                // closure body entirely). The tx-hash dedup early-return reads
                // `txn_get` on the accept path, so we GUARANTEE a dedup-serving
                // pending row exists synchronously here, before
                // service_send_raw_txn returns — otherwise an aggkit
                // re-broadcast racing the closure would miss dedup, hit the R4
                // nonce check against the already-advanced nonce, and wedge
                // ethtxmanager. Idempotent: a no-op when the closure already
                // produced the row (production sync mode, and the writer path
                // once the worker has run it — where the inflight cache covers
                // the accept-path gap regardless); it materialises the row
                // only when the boundary handoff hasn't. Production therefore
                // always writes link+receipt behind the boundary; this is a
                // synchronous dedup safety net, never a second write.
                if service.store.txn_get(txn_hash).await?.is_none() {
                    record_local_pending_tx(service, txn_hash, txn_envelope, signer, None, vec![])
                        .await?;
                } else {
                    drop(txn_envelope);
                }
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
            let tx_key = format!("{txn_hash:#x}");
            if service
                .store
                .get_note_handoff_for_tx(&tx_key)
                .await?
                .is_some()
            {
                tracing::warn!(
                    %txn_hash,
                    error = %err,
                    "GER outcome is ambiguous after durable note handoff; leaving receipt pending"
                );
                return Ok(());
            }
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
        .txn_begin_if_absent(
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
        .map(|_| ())
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

/// #55 — geth-faithful ACCEPT-AND-REVERT for a claimAsset that targets an
/// already-LANDED globalIndex. Increments the observability counter, then ATOMICALLY
/// (one store transaction — BLOCKER C) persists a durable REVERTED receipt (status
/// 0x0, EMPTY logs, NO ClaimEvent) AND CAS-advances the signer's nonce, so a crash
/// can never leave a half state (no pending-forever receipt, no stale nonce).
///
/// The nonce CAS advances iff the current nonce == `tx_nonce` (the sync accept path,
/// where the caller has not yet advanced it). In async-writer mode the enqueue
/// already CAS-advanced it, so the CAS here is a no-op and only the receipt is
/// written — the caller's/enqueue's own advance is authoritative. The receipt is
/// shaped identically to the writer-worker's failure receipt, which
/// `service_get_txn_receipt` renders as `status: 0x0` with every numeric field
/// present (blockNumber/blockHash/transactionIndex/gasUsed/cumulativeGasUsed/
/// effectiveGasPrice/from/to/logs=[]/logsBloom/type), so aggkit's Go ethtxmanager
/// sees the monitored tx as MINED-but-failed and advances instead of re-broadcasting.
async fn accept_and_revert_landed_claim(
    service: &ServiceState,
    params: &claimAssetCall,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    signer_str: &str,
    tx_nonce: u64,
) -> anyhow::Result<()> {
    ::metrics::counter!("claim_landed_dedup_reverted_total").increment(1);
    tracing::warn!(
        global_index = %params.globalIndex,
        eth_tx = %tx_hash,
        signer = %signer,
        "claim targets an already-landed globalIndex (a ClaimEvent already exists); \
         accepting and writing a REVERTED receipt (status 0x0, no new event) + advancing \
         the nonce ATOMICALLY so the submitter's nonce is consumed — geth-faithful \
         AlreadyClaimed revert (#55). The real landed claim is untouched."
    );
    let block_num = service.store.get_latest_block_number().await?;
    let block_hash = service.block_state.get_block_hash(block_num);
    let nonce_advanced = service
        .store
        .commit_reverted_receipt_and_advance_nonce(
            tx_hash,
            TxnEntry {
                id: None,
                envelope: txn_envelope,
                signer,
                expires_at: None,
                logs: vec![],
            },
            format!(
                "claim for globalIndex {} already landed (AlreadyClaimed); reverted (#55)",
                params.globalIndex
            ),
            block_num,
            block_hash,
            signer_str,
            tx_nonce,
        )
        .await?;
    tracing::debug!(
        target: "rpc::accept_revert",
        %tx_hash,
        nonce = tx_nonce,
        nonce_advanced,
        "accept-and-revert committed atomically (receipt + nonce CAS)"
    );
    Ok(())
}

/// #55 BLOCKER C/D — idempotent crash-gap nonce repair via store-level CAS.
///
/// On the sync accept path the durable receipt write and the nonce advance are
/// separate steps; a crash / store error BETWEEN them leaves the tx KNOWN (receipt
/// persisted) but the signer's expected nonce STALE at that tx's nonce. Called from
/// the RD-940 same-hash dedup path on a rebroadcast: the store-level
/// `nonce_advance_cas(signer, tx_nonce)` advances the nonce EXACTLY ONCE iff it is
/// still stuck at `tx_nonce` (the crash-gap signature — a normally-advanced tx has
/// `expected > tx.nonce`, and async mode advances at enqueue so likewise), so the
/// rebroadcast HEALS the nonce rather than serving stale forever.
///
/// The CAS is atomic at the store level, so this is correct even when two replicas
/// on a shared PostgreSQL race the same rebroadcast (BLOCKER D) — exactly one wins.
/// Returns `true` iff it advanced the nonce.
pub(crate) async fn repair_commit_gap_nonce(
    service: &ServiceState,
    signer_str: &str,
    tx_nonce: u64,
) -> anyhow::Result<bool> {
    let advanced = service
        .store
        .nonce_advance_cas(signer_str, tx_nonce)
        .await?;
    if advanced {
        ::metrics::counter!("rpc_nonce_repaired_after_commit_gap_total").increment(1);
        tracing::warn!(
            target: "rpc::nonce_repair",
            signer = %signer_str,
            nonce = tx_nonce,
            "healed a stale signer nonce on rebroadcast: a known tx's receipt was persisted but \
             its nonce advance was lost to a crash in the commit gap; CAS-advanced the expected \
             nonce to complete the interrupted accept (#55 BLOCKER C/D)"
        );
    }
    Ok(advanced)
}

/// Handle a `claimAsset` transaction: skip zero-amount or publish the claim.
///
/// This is the writer-worker dispatcher. It runs only after the signed envelope
/// is durable and the nonce CAS has accepted the transaction. It never
/// advances or reopens the nonce itself; any later
/// failure becomes a status-0 receipt for the already-accepted hash.
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

    // #55 BLOCKER A — the AUTHORITATIVE landed classification runs FIRST, before
    // RD-860 (unresolvable-destination) and C6 (GER-observed). A landed globalIndex
    // must route to accept-and-revert regardless of destination-resolvability or
    // GER-observed state, so nothing downstream can (1) take RD-860's SUCCESS path
    // and emit a SECOND ClaimEvent for an already-claimed gi (double-emit), or
    // (2) hard-reject on an unobserved GER WITHOUT consuming the nonce (wedge).
    //
    // `acquire_claim_lock` is the ONE atomic classification (BLOCKER B): a gi that
    // landed at any point in the try_claim window classifies `Landed`, never
    // `InFlight`, so no interleaving hard-rejects a landed gi.
    let tx_nonce = envelope_nonce(&txn_envelope);
    let signer_str = format!("{signer:#x}");
    let claim_fence = match acquire_claim_lock(
        &service.store,
        params.globalIndex,
        txn_hash,
        claim_resubmit_ttl(),
    )
    .await?
    {
        // Geth-faithful accept-and-revert: ACCEPT, write a REVERTED receipt (status
        // 0x0, empty logs, NO new ClaimEvent) AND advance the nonce ATOMICALLY (one
        // store transaction — BLOCKER C), so the submitter's nonce is consumed like
        // a normal accept and no crash can leave a half state. The real landed claim
        // is untouched.
        ClaimLockOutcome::Landed => {
            accept_and_revert_landed_claim(
                service,
                &params,
                txn_hash,
                txn_envelope,
                signer,
                &signer_str,
                tx_nonce,
            )
            .await?;
            return Ok(());
        }
        // A genuine concurrent submission for this gi is in flight (locked, no
        // ClaimEvent yet, within TTL). Do not double-publish. Because this dispatcher
        // runs after durable admission, the outer sync/worker path records status 0;
        // a same-hash rebroadcast then deduplicates to that terminal receipt.
        ClaimLockOutcome::InFlight => {
            anyhow::bail!(
                "claim already submitted for global_index {}",
                params.globalIndex
            );
        }
        // Fresh lock (or an orphaned record superseded) — proceed to publish.
        ClaimLockOutcome::Acquired { fence } => fence,
    };

    // R9 — install a fenced RAII guard. Cancellation or a pre-submit failure can
    // release only this executing fence; after `mark_submitted_fenced`, release is
    // fail-closed and neither this guard nor a stale owner can reopen the claim.
    let guard = ClaimGuard::new(
        service.store.clone(),
        params.globalIndex,
        txn_hash,
        claim_fence,
    );

    // RD-860 — swallow unresolvable-destination claims permanently. If the
    // destination address can't be resolved to a Miden AccountId, record the
    // unclaimable entry, emit the synthetic ClaimEvent so aggkit marks the
    // globalIndex complete and stops retrying, RELEASE the lock, and return success.
    // Funds remain locked on L1; an operator rescue endpoint (tier 2, future work)
    // would let ops re-process by registering a destination mapping and replaying.
    //
    // This runs AFTER the landed classification (BLOCKER A): a LANDED gi already
    // took the accept-and-revert arm above, so RD-860 can only fire for a FRESH gi
    // and can never emit a second ClaimEvent for an already-claimed one. Ordering
    // vs C6: RD-860 first because unresolvable-destination is permanent while a
    // missing GER is transient.
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
        // The gi is now handled (unclaimable record + ClaimEvent); drop the lock so
        // a resubmit classifies `Landed` (the ClaimEvent exists) → accept-and-revert.
        guard.release_explicitly().await;
        return Ok(());
    }

    let result = publish_and_record_claim(
        service,
        params.clone(),
        txn_hash,
        txn_envelope,
        signer,
        &guard,
    )
    .await;
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

/// How long a `try_claim` record may sit WITHOUT its ClaimEvent landing before it is
/// treated as an orphaned (crashed-mid-flight) submission and superseded on the next
/// retry. Env-tunable via `CLAIM_RESUBMIT_TTL_SECS`; the default comfortably covers the
/// slowest legitimate in-flight path (Miden proof + commit, tens of seconds)
/// while unwedging a crash-orphaned deposit within ~2 sponsor retries.
pub(crate) fn claim_resubmit_ttl() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 120;
    let secs = std::env::var("CLAIM_RESUBMIT_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// #55 BLOCKER 1 — how long a won `(signer, nonce)` admission lease is owned before
/// another replica presenting the SAME hash may take it over on expiry. Kept comfortably
/// BELOW aggkit's re-broadcast envelope so a rebroadcast after an owner crash can
/// take over promptly, yet above the slowest legitimate admission (a sync claim
/// publish). Env-tunable via `NONCE_RESERVATION_LEASE_SECS`.
pub(crate) fn reservation_lease() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 90;
    let secs = std::env::var("NONCE_RESERVATION_LEASE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs.max(3))
}

/// A spawned lease-renewal task must never outlive the request that owns it.
/// Tokio detaches a `JoinHandle` on drop, so an unguarded handle would keep an
/// abandoned reservation alive forever after request cancellation.
struct AbortTaskOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortTaskOnDrop {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    fn abort(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.abort();
    }
}

/// #55 BLOCKER 1 — execute a WON admission through the writer worker and return
/// the tx hash. Extracted so `service_send_raw_txn` can
/// wrap it with the reservation-lease RELEASE (success → future same-hash dedups;
/// failure → the same tx may retry via lease takeover). Only ever called after the
/// caller won the `(signer, nonce)` reservation.
async fn durably_admit_and_advance_nonce(
    service: &ServiceState,
    txn_hash: TxHash,
    txn_envelope: &TxEnvelope,
    signer: Address,
    signer_str: &str,
    tx_nonce: u64,
) -> anyhow::Result<()> {
    // The pending row is the durable queue intent. It is committed before the
    // nonce CAS, so a crash can never consume a nonce while leaving no work to
    // recover from the signed envelope.
    service
        .store
        .txn_begin_if_absent(
            txn_hash,
            TxnEntry {
                id: None,
                envelope: txn_envelope.clone(),
                signer,
                expires_at: None,
                logs: vec![],
            },
        )
        .await?;
    let advanced = service
        .store
        .nonce_advance_cas(signer_str, tx_nonce)
        .await?;
    if !advanced {
        let current = service.store.nonce_get(signer_str).await?;
        if current != tx_nonce.saturating_add(1) {
            anyhow::bail!(
                "lost nonce CAS for durable transaction {txn_hash:#x}: expected {tx_nonce}, current {current}"
            );
        }
    }
    Ok(())
}

/// State-only compatibility gate for `claimAsset`. One bridge snapshot answers
/// both questions in EVM order: already-claimed wins; otherwise a missing GER
/// fails before nonce consumption. Returns true only for an applied claim.
async fn claim_state_gate(service: &ServiceState, params: &claimAssetCall) -> anyhow::Result<bool> {
    let combined = crate::ger::combined_ger(&params.mainnetExitRoot.0, &params.rollupExitRoot.0);
    let (claimed, ger_applied) =
        crate::applied_state::claim_and_ger_applied(service, params.globalIndex, &combined).await?;
    if claimed {
        return Ok(true);
    }
    if !params.amount.is_zero()
        && crate::address_mapper::resolve_address(
            &*service.store,
            params.destinationAddress,
            &service.accounts.0,
        )
        .await
        .is_ok()
        && !ger_applied
    {
        ::metrics::counter!("rpc_claim_ger_not_seen_total").increment(1);
        anyhow::bail!(
            "claim references a GER that aggkit has not observed yet or that is not applied on the Miden bridge \
             (mainnet={}, rollup={}); retry after the GER is injected. C6.",
            ::hex::encode(params.mainnetExitRoot.0),
            ::hex::encode(params.rollupExitRoot.0)
        );
    }
    Ok(false)
}

async fn validate_before_nonce_reservation(
    service: &ServiceState,
    decoded: &crate::writer_worker::DecodedWriteCall,
) -> anyhow::Result<()> {
    let crate::writer_worker::DecodedWriteCall::Claim { params } = decoded else {
        return Ok(());
    };
    if params.destinationNetwork != service.network_id {
        anyhow::bail!(
            "claim targets destinationNetwork {} but this proxy only handles network {}",
            params.destinationNetwork,
            service.network_id
        );
    }

    // One state-only bridge snapshot answers both compatibility questions.
    let _already_claimed = claim_state_gate(service, params).await?;
    Ok(())
}

async fn dispatch_after_reservation(
    service: &ServiceState,
    decoded: crate::writer_worker::DecodedWriteCall,
    txn_envelope: TxEnvelope,
    signer: Address,
    txn_hash: TxHash,
    signer_str: &str,
    tx_nonce: u64,
) -> anyhow::Result<TxHash> {
    // Repeat the single state snapshot after reservation to close the landing
    // race. The bridge maps are monotonic, so no third pre-publish read is needed.
    if let crate::writer_worker::DecodedWriteCall::Claim { params } = &decoded
        && claim_state_gate(service, params).await?
    {
        accept_and_revert_landed_claim(
            service,
            params,
            txn_hash,
            txn_envelope,
            signer,
            signer_str,
            tx_nonce,
        )
        .await?;
        return Ok(txn_hash);
    }

    if let crate::writer_worker::DecodedWriteCall::Ger { ger_bytes } = &decoded
        && crate::applied_state::ger_applied(service, ger_bytes).await?
    {
        durably_admit_and_advance_nonce(
            service,
            txn_hash,
            &txn_envelope,
            signer,
            signer_str,
            tx_nonce,
        )
        .await?;
        handle_ger_result(
            Ok(false),
            txn_hash,
            txn_envelope,
            signer,
            service,
            *ger_bytes,
        )
        .await?;
        return Ok(txn_hash);
    }

    if let Some(handle) = service.writer_handle.as_ref() {
        if handle.available_capacity() == 0 {
            return Err(crate::writer_worker::WriterQueueSaturatedError.into());
        }
        durably_admit_and_advance_nonce(
            service,
            txn_hash,
            &txn_envelope,
            signer,
            signer_str,
            tx_nonce,
        )
        .await?;
        let job = decoded.into_job(txn_envelope, signer, txn_hash);
        match handle.try_enqueue(job) {
            Ok(()) => Ok(txn_hash),
            Err(crate::writer_worker::TryEnqueueError::QueueFull) => {
                Err(crate::writer_worker::WriterQueueSaturatedError.into())
            }
            Err(crate::writer_worker::TryEnqueueError::ShutDown) => {
                anyhow::bail!("writer worker has shut down; retry the same signed transaction")
            }
        }
    } else {
        #[cfg(not(test))]
        anyhow::bail!("single writer handle missing from production ServiceState");

        // Lower-level unit tests deliberately omit the background runtime so
        // they can assert dispatch results synchronously. This path is not
        // compiled into production.
        #[cfg(test)]
        {
            durably_admit_and_advance_nonce(
                service,
                txn_hash,
                &txn_envelope,
                signer,
                signer_str,
                tx_nonce,
            )
            .await?;
            let dispatch: anyhow::Result<()> = match decoded {
                crate::writer_worker::DecodedWriteCall::Claim { params } => {
                    worker_handle_claim_asset(service, *params, txn_hash, txn_envelope, signer)
                        .await
                }
                crate::writer_worker::DecodedWriteCall::Ger { ger_bytes } => {
                    worker_handle_ger_insert(service, ger_bytes, txn_hash, txn_envelope, signer)
                        .await
                }
            };
            if let Err(err) = dispatch {
                // Once the durable row and nonce CAS commit, the transaction is accepted.
                // Any later sync failure is represented as a status-0 receipt instead of
                // reopening the nonce slot after an outcome-ambiguous external call.
                let block_num = service.store.get_latest_block_number().await?;
                let block_hash = service.block_state.get_block_hash(block_num);
                service
                    .store
                    .txn_commit(
                        txn_hash,
                        Err(format!("sync dispatch failed: {err:#}")),
                        block_num,
                        block_hash,
                    )
                    .await?;
                tracing::error!(%txn_hash, error = %err, "accepted sync transaction reverted");
            }
            Ok(txn_hash)
        }
    }
}

/// Typed, AUTHORITATIVE outcome of [`acquire_claim_lock`]. Landed detection and the
/// #55 accept-and-revert decision are ONE step here (no separate pre-check→lock
/// window), so there is no interleaving in which a claim for an already-landed
/// globalIndex is hard-rejected — the exact nonce-desync wedge #55 fixes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaimLockOutcome {
    /// The submission lock is now held by THIS caller (fresh index, or an orphaned
    /// record superseded — SOAK FINDING #1). Proceed to publish; the caller MUST
    /// arrange a fenced conditional release on any pre-submit failure (via `ClaimGuard`).
    Acquired { fence: u64 },
    /// A genuine concurrent submission for the same gi is in flight (locked, no
    /// ClaimEvent yet, record younger than `ttl`). Publication is denied so there
    /// is no double submit; an already-accepted RPC hash is finalised status 0.
    InFlight,
    /// A real `ClaimEvent` already exists for this globalIndex — the claim LANDED.
    /// The caller routes this to #55 accept-and-revert (consume the nonce + write a
    /// reverted receipt), NEVER a hard reject. Any lock this call transiently took
    /// has been released before returning.
    Landed,
}

/// Classify a claim submission against the per-`global_index` submission lock and the
/// authoritative landed state, with orphaned-record recovery (SOAK FINDING #1).
///
/// This is the SINGLE authoritative step for landed detection: whatever the
/// interleaving, an index whose `ClaimEvent` already exists is classified `Landed`
/// (→ #55 accept-and-revert), never hard-rejected. Cases:
///
///   1. `try_claim_fenced` succeeds (fresh) + no ClaimEvent → `Acquired` (normal path).
///   2. `try_claim_fenced` succeeds (fresh) + a ClaimEvent already exists (e.g. a restore
///      wrote the event without a submission lock) → release the spurious lock and
///      return `Landed` — never double-publish onto a landed gi.
///   3. fenced acquisition conflicts + a ClaimEvent exists → `Landed`. This is the closed
///      TOCTOU: even if the claim LANDED after some earlier read, the landed
///      classification is made here, at the lock decision, and routes to
///      accept-and-revert rather than a hard reject.
///   4. fenced acquisition conflicts + no ClaimEvent + record younger than `ttl` → `InFlight`.
///   5. fenced acquisition conflicts + no ClaimEvent + `ttl` expired → ORPHANED: atomically
///      supersede (`Store::try_reclaim_claim_fenced` — single UPDATE, one winner under
///      concurrency), warn + `claim_resubmission_recovered_total`, return `Acquired`.
pub(crate) async fn acquire_claim_lock(
    store: &std::sync::Arc<dyn crate::store::Store>,
    global_index: alloy::primitives::U256,
    owner_tx_hash: TxHash,
    ttl: std::time::Duration,
) -> anyhow::Result<ClaimLockOutcome> {
    let gi_bytes: [u8; 32] = global_index.to_be_bytes::<32>();
    if let Some(claim) = store
        .try_claim_fenced(global_index, owner_tx_hash, ttl)
        .await?
    {
        if store.has_claim_event_for_global_index(&gi_bytes).await? {
            store
                .unclaim_fenced(&global_index, owner_tx_hash, claim.fence)
                .await?;
            return Ok(ClaimLockOutcome::Landed);
        }
        return Ok(ClaimLockOutcome::Acquired { fence: claim.fence });
    }

    if store.has_claim_event_for_global_index(&gi_bytes).await? {
        return Ok(ClaimLockOutcome::Landed);
    }
    let Some(claim) = store
        .try_reclaim_claim_fenced(global_index, owner_tx_hash, ttl)
        .await?
    else {
        if store.has_claim_event_for_global_index(&gi_bytes).await? {
            return Ok(ClaimLockOutcome::Landed);
        }
        return Ok(ClaimLockOutcome::InFlight);
    };
    if store.has_claim_event_for_global_index(&gi_bytes).await? {
        store
            .unclaim_fenced(&global_index, owner_tx_hash, claim.fence)
            .await?;
        return Ok(ClaimLockOutcome::Landed);
    }
    ::metrics::counter!("claim_resubmission_recovered_total").increment(1);
    tracing::warn!(
        global_index = %global_index,
        ttl_secs = ttl.as_secs(),
        fence = claim.fence,
        "reclaimed an expired claim submission with a new ownership fence"
    );
    Ok(ClaimLockOutcome::Acquired { fence: claim.fence })
}

/// RAII guard that conditionally releases only its own executing claim fence if
/// the holding future is dropped before `commit()` or `release_explicitly()`. On
/// drop it schedules `unclaim_fenced` on the runtime; a stale owner cannot delete a
/// successor fence, and a submitted claim remains fail-closed. Self-review R9.
#[derive(Clone)]
pub(crate) struct ClaimSubmissionFence {
    store: std::sync::Arc<dyn crate::store::Store>,
    global_index: alloy::primitives::U256,
    owner_tx_hash: TxHash,
    fence: u64,
}

impl ClaimSubmissionFence {
    pub(crate) async fn prepare(
        &self,
        note_commitment: &str,
        note_id: &str,
        expiration_block: u64,
    ) -> anyhow::Result<()> {
        if !self
            .store
            .prepare_claim_submission_fenced(
                self.global_index,
                self.owner_tx_hash,
                self.fence,
                self.owner_tx_hash,
                note_commitment,
                note_id,
                expiration_block,
            )
            .await?
        {
            anyhow::bail!(
                "claim ownership fence lost before submission for global_index {}",
                self.global_index
            );
        }
        Ok(())
    }

    pub(crate) async fn confirm(&self, note_commitment: &str) -> anyhow::Result<()> {
        let tx_key = format!("{:#x}", self.owner_tx_hash);
        if !self
            .store
            .confirm_note_handoff(&tx_key, note_commitment)
            .await?
        {
            anyhow::bail!(
                "claim note handoff changed before commit confirmation for global_index {}",
                self.global_index
            );
        }
        Ok(())
    }
}

pub(crate) struct ClaimGuard {
    store: Option<std::sync::Arc<dyn crate::store::Store>>,
    global_index: alloy::primitives::U256,
    owner_tx_hash: TxHash,
    fence: u64,
}

impl ClaimGuard {
    fn new(
        store: std::sync::Arc<dyn crate::store::Store>,
        global_index: alloy::primitives::U256,
        owner_tx_hash: TxHash,
        fence: u64,
    ) -> Self {
        Self {
            store: Some(store),
            global_index,
            owner_tx_hash,
            fence,
        }
    }

    fn submission_fence(&self) -> ClaimSubmissionFence {
        ClaimSubmissionFence {
            store: self.store.as_ref().expect("active claim guard").clone(),
            global_index: self.global_index,
            owner_tx_hash: self.owner_tx_hash,
            fence: self.fence,
        }
    }

    fn commit(mut self) {
        self.store = None;
    }

    async fn release_explicitly(mut self) {
        if let Some(store) = self.store.take() {
            let _ = store
                .unclaim_fenced(&self.global_index, self.owner_tx_hash, self.fence)
                .await;
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            let global_index = self.global_index;
            let owner_tx_hash = self.owner_tx_hash;
            let fence = self.fence;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    match store
                        .unclaim_fenced(&global_index, owner_tx_hash, fence)
                        .await
                    {
                        Ok(true) => tracing::warn!(
                            target: "claim::guard",
                            %global_index, fence,
                            "released current fenced claim after cancellation"
                        ),
                        Ok(false) => tracing::debug!(
                            target: "claim::guard",
                            %global_index, fence,
                            "stale or submitted claim guard release was fenced out"
                        ),
                        Err(e) => tracing::error!(
                            target: "claim::guard",
                            %global_index, fence, error = %e,
                            "failed to release fenced claim guard"
                        ),
                    }
                });
            }
        }
    }
}

/// Publish a CLAIM note and record the transaction in the store.
///
/// Called after fenced claim acquisition. The caller owns `ClaimGuard`, whose
/// conditional release cannot delete a successor or a submitted claim.
async fn publish_and_record_claim(
    service: &ServiceState,
    params: claimAssetCall,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    guard: &ClaimGuard,
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
        Some(service.expected_mints.clone()),
        guard.submission_fence(),
    )
    .await;
    match claim_result {
        Ok(claim_result) => {
            tracing::info!(
                eth_tx = %txn_hash,
                miden_tx = %claim_result.txn_id,
                "claim published; receipt pending until the projector finalises it on consumption"
            );
            Ok(())
        }
        Err(err) => {
            let tx_key = format!("{txn_hash:#x}");
            if service
                .store
                .get_note_handoff_for_tx(&tx_key)
                .await?
                .is_some()
            {
                tracing::warn!(
                    %txn_hash,
                    error = %err,
                    "claim outcome is ambiguous after durable note handoff; leaving receipt pending"
                );
                Ok(())
            } else {
                Err(err)
            }
        }
    }
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
            // The envelope + signer ride into `insert_ger` so the pending
            // receipt row is created INSIDE the serialized Miden-client
            // closure, together with the tx↔note link (handoff-before-
            // projection — see `ger::record_ger_submission_handoff`).
            txn_envelope.clone(),
            signer,
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
/// Whether `signer` is permitted to submit `eth_sendRawTransaction`.
///
/// Audit C2 — pre-fix, `None` (the default) meant OPEN to any signer, which
/// combined with the `0.0.0.0` bind made the service accept anonymous claims /
/// GER injections from anyone who could reach the port. `None` now means CLOSED
/// (fail-closed). Legacy open mode is an explicit opt-in via
/// `ServiceState::allow_any_signer` (`--insecure-allow-any-signer`), checked at
/// the call site.
pub fn is_signer_allowed(allowed: Option<&[Address]>, signer: &Address) -> bool {
    match allowed {
        None => false,
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
    let signer_str = format!("{signer:#x}");
    let tx_nonce = envelope_nonce(&txn_envelope);
    let selector = calldata_selector(&txn.input);
    tracing::debug!(target: concat!(module_path!(), "::debug"), "raw transaction hash: {txn_hash}");

    // Reject unauthorized signers before any store read or per-signer lock
    // allocation. In fail-closed mode this keeps untrusted addresses from
    // growing the lock registry or consuming database capacity.
    if !service.allow_any_signer && !is_signer_allowed(service.allowed_signers.as_deref(), &signer)
    {
        ::metrics::counter!("rpc_unauthorized_signer_total").increment(1);
        anyhow::bail!(
            "signer {signer:#x} is not on the allow-list; configure --allowed-signers (or ALLOWED_SIGNERS), \
             or set --insecure-allow-any-signer to explicitly opt into open mode"
        );
    }

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
    let tx_key = format!("{txn_hash:#x}");
    let mut known_store_entry = service.store.txn_get(txn_hash).await?;
    let mut handoff = service.store.get_note_handoff_for_tx(&tx_key).await?;
    if let Some(prepared) = handoff
        .as_ref()
        .filter(|handoff| handoff.state == crate::store::NoteHandoffState::Prepared)
    {
        if service
            .store
            .clear_expired_prepared_note_handoff(&tx_key, &prepared.note_commitment)
            .await?
        {
            tracing::warn!(
                %txn_hash,
                "authoritative reconcile cursor passed prepared transaction expiration; retrying exact transaction"
            );
            handoff = None;
            known_store_entry = service.store.txn_get(txn_hash).await?;
        } else {
            repair_commit_gap_nonce(&service, &signer_str, tx_nonce).await?;
            tracing::debug!(
                target: "rpc::dedup",
                %txn_hash,
                "prepared note handoff is still ambiguous; leaving receipt pending"
            );
            return Ok(txn_hash);
        }
    }
    let known_store_tx = known_store_entry.is_some();
    let known_store_linked = handoff
        .as_ref()
        .is_some_and(|handoff| handoff.state == crate::store::NoteHandoffState::Submitted);
    // A pending row without a note handoff is the durable queue intent. It must
    // be resumed after a crashed process loses its in-memory mpsc contents.
    let mut known_durable_intent = known_store_entry
        .as_ref()
        .is_some_and(|entry| entry.result.is_none() && !known_store_linked);
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
            "known_store_linked": known_store_linked,
            "known_store_prepared": handoff.as_ref().is_some_and(|handoff| handoff.state == crate::store::NoteHandoffState::Prepared),
            "known_durable_intent": known_durable_intent,
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
    if known_store_tx && !known_durable_intent {
        tracing::debug!(
            target: "rpc::dedup",
            %txn_hash,
            "tx-hash dedup (handed off or terminal): returning OK"
        );
        repair_commit_gap_nonce(&service, &signer_str, tx_nonce).await?;
        return Ok(txn_hash);
    }

    let decoded = decode_write_call(&txn.input)?;
    // Deterministic and side-effect-free rejection belongs before the signer
    // lock. Stateful checks repeat after reservation to close landing races.
    validate_before_nonce_reservation(&service, &decoded).await?;
    if let Some(handle) = service.writer_handle.as_ref()
        && handle.available_capacity() == 0
    {
        return Err(crate::writer_worker::WriterQueueSaturatedError.into());
    }
    #[cfg(not(test))]
    if service.writer_handle.is_none() {
        anyhow::bail!("single writer handle missing from production ServiceState");
    }

    // R4 follow-up — serialise the entire nonce-check + enqueue/handler
    // critical section for this signer. Without the mutex, two concurrent
    // same-nonce txs both pass the equality check before either calls
    // `nonce_increment`.
    //
    // With the writer worker enabled, also tolerate bounded future-nonce
    // reordering from concurrent HTTP delivery: if nonce N+1 reaches us before
    // nonce N, release the lock, wait briefly for N to be accepted, then
    // re-check. This is a small in-process txpool behavior; stale/replay nonces
    // still fail immediately, and missing gaps still fail after the bound.
    let future_nonce_wait_max = std::time::Duration::from_secs(30);
    let future_nonce_poll = std::time::Duration::from_millis(50);
    let future_nonce_wait_started = tokio::time::Instant::now();
    let mut logged_future_nonce_wait = false;

    let _lock = loop {
        let lock = service.per_signer_locks.lock(signer).await;

        // Close the race between the optimistic dedup read above and this lock.
        // A concurrent identical request may have admitted the hash while we
        // waited; it must deduplicate here instead of failing as a stale nonce.
        if service
            .writer_handle
            .as_ref()
            .is_some_and(|handle| handle.is_inflight(&txn_hash))
        {
            return Ok(txn_hash);
        }
        if !known_durable_intent
            && let Some(refreshed_entry) = service.store.txn_get(txn_hash).await?
        {
            let refreshed_handoff = service.store.get_note_handoff_for_tx(&tx_key).await?;
            if refreshed_entry.result.is_some() || refreshed_handoff.is_some() {
                repair_commit_gap_nonce(&service, &signer_str, tx_nonce).await?;
                return Ok(txn_hash);
            }
            known_durable_intent = true;
        }

        // R4 — nonce validation. Pre-fix the proxy advanced its tracked nonce
        // only on success and never compared the incoming `tx.nonce` against
        // the expected next value. That allowed replay and skipped sequencing.
        let expected_nonce = service.store.nonce_get(&signer_str).await?;
        let durable_frontier = service.store.pending_nonce_frontier(&signer_str).await?;
        if let Some(lower_nonce) = durable_frontier.lowest_unlinked
            && lower_nonce < tx_nonce
        {
            let lower_is_live = service
                .writer_handle
                .as_ref()
                .is_some_and(|handle| handle.has_non_terminal_nonce(&signer, lower_nonce));
            if !lower_is_live {
                anyhow::bail!(
                    "cannot admit nonce {tx_nonce} for {signer_str}: durable transaction at lower nonce {lower_nonce} has not reached the Miden handoff; re-submit that exact signed transaction first"
                );
            }
        }
        let durable_resume_nonce =
            known_durable_intent && expected_nonce == tx_nonce.saturating_add(1);
        let can_wait_for_future_nonce = service.writer_handle.is_some()
            && tx_nonce > expected_nonce
            && future_nonce_wait_started.elapsed() < future_nonce_wait_max;
        let nonce_action = if tx_nonce == expected_nonce || durable_resume_nonce {
            "accept"
        } else if can_wait_for_future_nonce {
            "wait_future"
        } else {
            "reject"
        };
        tracing::info!(
            target: "rpc::nonce_snoop",
            "{}",
            serde_json::json!({
                "event": "eth_sendRawTransaction_nonce_check",
                "signer": signer_str,
                "tx_hash": format!("{txn_hash:#x}"),
                "tx_nonce": tx_nonce,
                "expected_nonce": expected_nonce,
                "durable_lowest_unlinked": durable_frontier.lowest_unlinked,
                "nonce_matches": tx_nonce == expected_nonce,
                "durable_resume": durable_resume_nonce,
                "action": nonce_action,
                "future_nonce_wait_ms": future_nonce_wait_started.elapsed().as_millis(),
                "writer_handle_present": service.writer_handle.is_some(),
            })
        );

        if tx_nonce == expected_nonce || durable_resume_nonce {
            break lock;
        }

        if can_wait_for_future_nonce {
            if !logged_future_nonce_wait {
                ::metrics::counter!("rpc_future_nonce_wait_total").increment(1);
                logged_future_nonce_wait = true;
            }
            drop(lock);
            tokio::time::sleep(future_nonce_poll).await;
            continue;
        }

        ::metrics::counter!("rpc_nonce_mismatch_total").increment(1);
        anyhow::bail!(
            "nonce mismatch for {signer_str}: tx.nonce = {tx_nonce}, expected {expected_nonce}; this guards against replay and out-of-order submission (R4)"
        );
    };

    // ── #55 BLOCKER 1 — atomic (signer, nonce) reservation ──────────────
    //
    // Reserve the (signer, nonce) slot ATOMICALLY, BEFORE any queue/dispatch/receipt
    // side effect. Two replicas on a shared PostgreSQL that each passed their
    // process-local R4 for two DIFFERENT txs at the same (signer, nonce) both reach
    // here; the store reservation lets exactly ONE win. The loser NEVER executes —
    // no enqueue, no dispatch, no receipt. (This runs AFTER the R4 per-signer lock,
    // which still serialises intra-process and provides the future-nonce wait; the
    // reservation is the cross-replica uniqueness guarantee the process lock can't.)
    // Reserve the (signer, nonce) admission LEASE atomically, BEFORE any
    // queue/dispatch/receipt side effect. This is a FENCED-OWNERSHIP lifecycle,
    // not a bare row: exactly one replica ever executes a given (signer, nonce),
    // and a genuine retry after a failed/expired attempt is allowed via takeover.
    let reservation_fence = match service
        .store
        .reserve_nonce(&signer_str, tx_nonce, txn_hash, reservation_lease())
        .await?
    {
        crate::store::NonceReservation::Won { fence } => fence,
        crate::store::NonceReservation::OwnedBySame => {
            // The slot is currently owned+executing by THIS SAME tx under a valid
            // lease (another replica is admitting it). Do NOT execute — dedup-return
            // the hash; the owner produces the receipt. This is the authoritative
            // single-executor guarantee that
            // the process-local dedup reads (which can miss cross-replica) cannot give.
            tracing::debug!(
                target: "rpc::reserve",
                %txn_hash, nonce = tx_nonce,
                "reservation owned by this same tx under a valid executing lease — \
                 dedup-returning hash, NOT executing"
            );
            return Ok(txn_hash);
        }
        crate::store::NonceReservation::HeldByOther(other) => {
            // A DIFFERENT tx owns/owned this (signer, nonce) slot — this submission
            // LOST and must NOT execute. Reject; the winner advances the nonce, so
            // this loser's retries fail R4 as stale and aggkit drops it — mirroring
            // geth dropping the losing tx at an already-consumed nonce.
            ::metrics::counter!("rpc_nonce_reservation_lost_total").increment(1);
            anyhow::bail!(
                "nonce {tx_nonce} for {signer_str} is reserved by a different tx {other:#x} \
                 (concurrent submission at the same nonce slot); this tx must not execute"
            );
        }
    };

    // Keep ownership live while sync publication or request-thread admission is
    // running. Without renewal, a slow proof can outlive the lease and let a
    // second replica execute the same hash concurrently.
    let heartbeat_store = service.store.clone();
    let heartbeat_signer = signer_str.clone();
    let heartbeat_lease = reservation_lease();
    let heartbeat_period = std::cmp::max(std::time::Duration::from_secs(1), heartbeat_lease / 3);
    let mut reservation_heartbeat = AbortTaskOnDrop::new(tokio::spawn(async move {
        loop {
            tokio::time::sleep(heartbeat_period).await;
            match heartbeat_store
                .renew_reservation(&heartbeat_signer, tx_nonce, txn_hash, heartbeat_lease)
                .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(err) => tracing::warn!(
                    target: "rpc::reserve", %txn_hash, error = %err,
                    "failed to renew nonce reservation; retrying until the lease expires"
                ),
            }
        }
    }));

    // Execute the WON admission, then RELEASE the lease on the outcome: success →
    // `released_success` (a future same-hash durable recovery may reacquire);
    // failure → `released_failure` (the SAME tx may retry via lease takeover). Only
    // the current fence owner may release, so a delayed crashed owner whose lease was
    // taken over cannot clobber the new owner's state. A crash before release leaves
    // the lease to EXPIRE and be taken over. This is the durable admission lifecycle.
    let admission = dispatch_after_reservation(
        &service,
        decoded,
        txn_envelope,
        signer,
        txn_hash,
        &signer_str,
        tx_nonce,
    )
    .await;
    reservation_heartbeat.abort();
    if let Err(release_err) = service
        .store
        .release_reservation(
            &signer_str,
            tx_nonce,
            txn_hash,
            reservation_fence,
            admission.is_ok(),
        )
        .await
    {
        tracing::warn!(
            target: "rpc::reserve",
            %txn_hash, nonce = tx_nonce, err = %release_err,
            "failed to release the nonce reservation lease; it will expire and be taken over"
        );
    }
    admission
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
        encode_legacy_tx_with_nonce(input, 0)
    }

    fn encode_legacy_tx_with_nonce(input: Vec<u8>, nonce: u64) -> (String, Address) {
        let txn = TxLegacy {
            input: input.into(),
            chain_id: Some(1),
            nonce,
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

    /// Build + encode a legacy tx REALLY signed by `signer_key` with an explicit
    /// `gas_price`. The nonce is intentionally FIXED to 0 (not a parameter): this
    /// helper exists solely for the same-nonce concurrency test, so don't reuse it
    /// for arbitrary nonces. Two calls with the same key + nonce 0 + calldata but
    /// different `gas_price` recover to the SAME signer yet produce DIFFERENT
    /// tx-hashes (the hash is keccak over the signed RLP, which includes
    /// gas_price). Distinct hashes are the point: they stop the RD-940 tx-hash
    /// dedup (idempotent re-broadcast) from short-circuiting the loser, so the
    /// per-signer nonce lock is the actual guard exercised by the concurrency test.
    fn encode_legacy_tx_signed(
        signer_key: &alloy::signers::local::PrivateKeySigner,
        input: Vec<u8>,
        gas_price: u128,
    ) -> String {
        use alloy::consensus::SignableTransaction;
        use alloy::signers::SignerSync;
        let txn = TxLegacy {
            // Explicit so the test's "same nonce" semantics don't silently ride
            // on `TxLegacy::default()` (would break if the default ever changes).
            nonce: 0,
            input: input.into(),
            chain_id: Some(1),
            gas_price,
            ..Default::default()
        };
        let signature = signer_key
            .sign_hash_sync(&txn.signature_hash())
            .expect("signing the legacy test tx must succeed");
        let envelope: TxEnvelope = txn.into_signed(signature).into();
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        format!("0x{}", ::hex::encode(encoded))
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
    async fn already_registered_ger_returns_success_without_second_event() {
        let service = create_test_service();
        let store = service.store.clone();
        let ger = [0xacu8; 32];
        store
            .commit_ger_event_atomic(1, [0u8; 32], "0xoriginal-ger", &ger, None, None, 0)
            .await
            .unwrap();
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xffff".to_string()),
            ..Default::default()
        };
        let events_before = store
            .get_logs(&filter, 0xffff)
            .await
            .unwrap()
            .into_iter()
            .filter(|log| {
                log.topics.first().map(String::as_str)
                    == Some(crate::log_synthesis::UPDATE_HASH_CHAIN_VALUE_TOPIC)
            })
            .count();

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from(ger),
        }
        .abi_encode();
        let (raw, signer) = encode_legacy_tx(calldata);
        let tx_hash = service_send_raw_txn(service.clone(), raw)
            .await
            .expect("already-registered GER must be accepted");
        let receipt = crate::service_get_txn_receipt::service_get_txn_receipt(
            service,
            format!("{tx_hash:#x}"),
        )
        .await
        .unwrap()
        .expect("duplicate GER receipt must be terminal");
        assert!(matches!(
            receipt.inner.as_receipt().unwrap().status,
            alloy::consensus::Eip658Value::Eip658(true)
        ));
        assert!(receipt.inner.as_receipt().unwrap().logs.is_empty());
        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 1);
        let events_after = store
            .get_logs(&filter, 0xffff)
            .await
            .unwrap()
            .into_iter()
            .filter(|log| {
                log.topics.first().map(String::as_str)
                    == Some(crate::log_synthesis::UPDATE_HASH_CHAIN_VALUE_TOPIC)
            })
            .count();
        assert_eq!(events_after, events_before, "duplicate GER emits no event");
        assert!(store.is_ger_injected(&ger).await.unwrap());
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

    /// Self-review C6 — repro+regression. A claim referencing a GER that aggkit
    /// has not observed must be rejected retryably, and must leave the globalIndex
    /// UNLOCKED afterwards (cheap retry surface — no lock held across the long
    /// publish path). Post-#55-BLOCKER-A, the authoritative landed classification
    /// runs first, so the lock is transiently acquired then RELEASED on the C6
    /// rejection (`guard.release_explicitly`); the end state is still unclaimed, and
    /// C6 is only a fast `is_ger_injected` read (no 15s wait) while the lock is held.
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
        let result = service_send_raw_txn(service, input_hex.clone()).await;
        let err = result.expect_err("claim with unseen GER must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("not observed yet"), "unexpected: {msg}");

        // The lock must be RELEASED after the C6 rejection (transiently acquired
        // then dropped) — the retry surface stays cheap (no lock left held).
        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "C6 rejection must leave the globalIndex unlocked (lock released)"
        );

        // PR #127 review point 5 — pre-admission failure must leave NOTHING
        // behind: no nonce consumed (the same signed tx/nonce is retryable
        // after GER publication), no receipt, no stored tx row (which would
        // make the retry short-circuit through the RD-940 dedup as "known").
        let payload = crate::hex::hex_decode_prefixed(&input_hex).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut payload.as_slice()).unwrap();
        let tx_hash = *envelope.tx_hash();
        let signer = envelope.recover_signer().unwrap();
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            0,
            "C6 rejection must not consume the nonce"
        );
        assert!(
            store.txn_get(tx_hash).await.unwrap().is_none(),
            "C6 rejection must not create a tx row / receipt"
        );
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_none(),
            "C6 rejection must not create a receipt"
        );
    }

    /// PR #127 review point 3. Pre-fix, the C6 gate only ran inside the worker,
    /// AFTER `try_enqueue` had consumed the nonce and admitted the tx hash
    /// into the inflight dedup cache. The gate must run on the REQUEST path:
    /// a claim whose GER is unpublished is rejected with no nonce consumed,
    /// no globalIndex lock, no receipt, and no queued job — and the SAME
    /// signed transaction (same nonce) is accepted once the GER is published.
    #[tokio::test]
    async fn c6_writer_mode_missing_ger_rejected_before_enqueue_then_retryable() {
        let mut service = create_test_service();
        let (handle, _shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            8,
            std::time::Duration::from_secs(60),
        );
        let handle = std::sync::Arc::new(handle);
        service.writer_handle = Some(handle.clone());
        let store = service.store.clone();

        let global_index = U256::from(77u64);
        let mainnet = [0xA7u8; 32];
        let rollup = [0xB7u8; 32];
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::from(mainnet),
            rollupExitRoot: FixedBytes::from(rollup),
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            // Zero-padded resolvable destination so the RD-860 short-circuit
            // doesn't pre-empt the C6 gate under test.
            destinationAddress: alloy::primitives::address!(
                "0x00000000ac0000000000dd110000ee000000fc00"
            ),
            amount: U256::from(1_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);
        let payload = crate::hex::hex_decode_prefixed(&input_hex).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut payload.as_slice()).unwrap();
        let tx_hash = *envelope.tx_hash();

        // GER unpublished → rejected on the request path.
        let err = service_send_raw_txn(service.clone(), input_hex.clone())
            .await
            .expect_err("writer mode must reject an unpublished-GER claim at admission");
        assert!(
            format!("{err}").contains("not observed yet"),
            "unexpected: {err}"
        );

        // Nothing left behind: no nonce, no lock, no receipt, no queued job.
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            0,
            "pre-admission failure must not consume the nonce"
        );
        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "pre-admission failure must not lock the globalIndex"
        );
        assert!(
            store.txn_get(tx_hash).await.unwrap().is_none(),
            "pre-admission failure must not create a tx row"
        );
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_none(),
            "pre-admission failure must not create a receipt"
        );
        assert!(
            !handle.is_inflight(&tx_hash),
            "pre-admission failure must not admit the hash into the inflight cache"
        );
        assert_eq!(
            handle.available_capacity(),
            handle.queue_depth(),
            "no job may be queued (channel must remain at full capacity)"
        );

        // Publish the GER (what the SyntheticProjector does on consumption) —
        // the SAME signed transaction, same nonce, must now be accepted.
        let ger = crate::ger::combined_ger(&mainnet, &rollup);
        store
            .commit_ger_event_atomic(1, [0u8; 32], "0xger-pub", &ger, None, None, 0)
            .await
            .unwrap();
        let accepted_hash = service_send_raw_txn(service.clone(), input_hex)
            .await
            .expect("the identical signed tx must be accepted after GER publication");
        assert_eq!(accepted_hash, tx_hash);
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            1,
            "acceptance advances the nonce exactly once"
        );
        assert!(
            handle.is_inflight(&tx_hash),
            "accepted claim must be admitted to the writer queue"
        );
    }

    /// Zero-amount genesis claims do not require a GER. The combined state gate
    /// still checks AlreadyClaimed first, then deliberately skips GER validation.
    #[tokio::test]
    async fn c6_writer_mode_zero_amount_claim_not_ger_gated() {
        let mut service = create_test_service();
        let (handle, _shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            8,
            std::time::Duration::from_secs(60),
        );
        service.writer_handle = Some(std::sync::Arc::new(handle));

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(1u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: Address::ZERO,
            amount: U256::ZERO,
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        // No GER seeded — the zero-amount claim must still be accepted
        // (the worker swallows it as an immediate success, never touching C6).
        service_send_raw_txn(service, input_hex)
            .await
            .expect("zero-amount claim must not be GER-gated at admission");
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
            .commit_ger_event_atomic(
                1,
                [0u8; 32],
                "0xger-seed",
                &ger,
                Some([0u8; 32]),
                Some([0u8; 32]),
                0,
            )
            .await
            .unwrap();

        let tx_hash = service_send_raw_txn(service, input_hex)
            .await
            .expect("post-accept publish failure returns the accepted hash");
        let (receipt, _) = store.txn_receipt(tx_hash).await.unwrap().unwrap();
        assert!(
            receipt.is_err(),
            "publish failure is represented as status 0x0"
        );

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
    /// Two DISTINCT txs from the SAME signer at the SAME nonce (identical
    /// calldata + nonce, different `gas_price`, so they share a signer but have
    /// different tx-hashes). Asserts the per-signer lock lets at most ONE pass
    /// the nonce gate. Pre-fix (no lock) both would have advanced the nonce — a
    /// same-nonce double-spend.
    #[tokio::test]
    async fn r4_followup_concurrent_same_nonce_serialised() {
        // Distinct hashes are deliberate: with identical hashes the loser would
        // RD-940-dedup-return Ok WITHOUT touching the nonce, masking the lock and
        // making the old "exactly one Ok" assertion flaky. Different gas_price ->
        // different hash -> the dedup can't conflate them, so the nonce lock is
        // the real guard under test.
        let signer_key = alloy::signers::local::PrivateKeySigner::random();
        let signer = signer_key.address();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xAAu8; 32]),
        }
        .abi_encode();
        let input_a = encode_legacy_tx_signed(&signer_key, calldata.clone(), 0);
        let input_b = encode_legacy_tx_signed(&signer_key, calldata, 1);
        // Enforce the test's premise: distinct gas_price -> distinct signed RLP ->
        // distinct tx hashes, so the RD-940 tx-hash dedup can't conflate the two
        // and short-circuit the loser before the per-signer nonce lock is hit.
        // Fails loudly if alloy encoding/signing ever makes these collide.
        assert_ne!(
            input_a, input_b,
            "distinct gas_price must produce distinct encoded txs (distinct tx hashes)"
        );

        let service = create_test_service();
        let store = service.store.clone();
        // Run both concurrently.
        let svc_a = service.clone();
        let svc_b = service.clone();
        let h_a = tokio::spawn(async move { service_send_raw_txn(svc_a, input_a).await });
        let h_b = tokio::spawn(async move { service_send_raw_txn(svc_b, input_b).await });
        let res_a = h_a.await.unwrap();
        let res_b = h_b.await.unwrap();

        let oks = [&res_a, &res_b].iter().filter(|r| r.is_ok()).count();

        // SAFETY invariant (RD-1021): the per-signer lock serialises both
        // same-nonce txs through the `nonce_get` -> check -> handler ->
        // `nonce_increment` section, and the nonce advances ONLY on a handler
        // that succeeds. So at most one can succeed: once one reaches
        // `nonce_increment` (expected 0 -> 1) the other fails the equality check
        // (tx.nonce 0 != expected 1) and is rejected; and if the first instead
        // fails inside the handler WITHOUT incrementing, expected stays 0 and the
        // second passes the check (0 == 0) for a fresh attempt — but only its own
        // success could then increment, so the success count still can't exceed
        // one. Distinct tx hashes make this reachable: identical hashes would let
        // the RD-940 dedup mask the second tx before the lock is ever exercised.
        //
        // Hence `oks` can be 0 (not only 1): the handler runs the GER-insert path
        // through the test `MidenClient` stub, whose request/response hops a
        // `std::thread` + oneshot channel; under load that round-trip can fail the
        // tx that holds the gate — a liveness hiccup, not a safety violation. The
        // lone-tx happy path is covered deterministically by
        // `r4_correct_nonce_accepted`.
        assert!(
            oks <= 1,
            "at most one same-nonce tx may succeed (got {oks}) — double-submit guard broken"
        );
        // The store nonce must advance by EXACTLY the number of successes — never
        // twice for two same-nonce txs (the double-spend), never out of step.
        let final_nonce = store.nonce_get(&format!("{signer:#x}")).await.unwrap();
        assert_eq!(
            final_nonce, oks as u64,
            "store nonce ({final_nonce}) must equal the number of successful txs ({oks})"
        );
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

    #[tokio::test]
    async fn rd940_future_nonce_waits_for_missing_nonce_acceptance() {
        let mut service = create_test_service();
        let store = service.store.clone();
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        service.writer_handle = Some(std::sync::Arc::new(handle));

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xBBu8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx_with_nonce(calldata, 1);

        let svc = service.clone();
        let pending = tokio::spawn(async move { service_send_raw_txn(svc, input_hex).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !pending.is_finished(),
            "future nonce should wait for the missing nonce instead of failing immediately"
        );

        store
            .nonce_increment(&format!("{signer:#x}"))
            .await
            .expect("simulate nonce 0 acceptance");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .expect("future nonce waiter should complete")
            .expect("task should not panic");
        assert!(
            result.is_ok(),
            "future nonce should be accepted: {result:?}"
        );
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            2,
            "accepting nonce 1 should advance the next accepted nonce to 2"
        );

        let _ = shutdown.send(());
    }

    /// Self-review R2 + audit C2 — repro+regression. Pre-fix, every recovered
    /// signer was accepted unconditionally; the allow-list then additionally
    /// failed OPEN (None => true). Post-C2 the predicate must:
    /// - return FALSE when no allow-list is configured (fail-closed default —
    ///   legacy open mode now requires the explicit `allow_any_signer` opt-in)
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

        // None = CLOSED (audit C2 — was OPEN pre-fix)
        assert!(!is_signer_allowed(None, &alice));
        assert!(!is_signer_allowed(None, &bob));

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

    /// A duplicate can pass the optimistic dedup read while the original request
    /// owns the signer lock. Recheck after lock acquisition so it returns the same
    /// hash instead of observing the advanced nonce as stale.
    #[tokio::test]
    async fn concurrent_same_hash_rechecks_dedup_inside_signer_lock() {
        let service = create_test_service();
        let store = service.store.clone();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xCDu8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        // Hold the signer lock while both calls complete their initial empty-store
        // dedup reads, forcing the race this regression covers.
        let gate = service.per_signer_locks.lock(signer).await;
        let first_service = service.clone();
        let first_input = input_hex.clone();
        let first =
            tokio::spawn(async move { service_send_raw_txn(first_service, first_input).await });
        let second_service = service.clone();
        let second =
            tokio::spawn(async move { service_send_raw_txn(second_service, input_hex).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(gate);

        let first = first.await.unwrap().expect("first submit must succeed");
        let second = second
            .await
            .unwrap()
            .expect("concurrent rebroadcast must deduplicate");
        assert_eq!(first, second);
        assert_eq!(
            store.nonce_get(&format!("{signer:#x}")).await.unwrap(),
            1,
            "concurrent dedup must advance the nonce exactly once"
        );
    }

    /// R2 integration repro — a signed tx whose signer is NOT on the allow-list
    /// must be rejected, even if everything else is well-formed. Without the
    /// fix, the same tx would be processed (and the proxy would attempt to run
    /// the corresponding Miden tx on the service account's behalf).
    #[tokio::test]
    async fn r2_unauthorised_signer_rejected_with_descriptive_error() {
        let mut service = create_test_service();
        // Audit C2 — disable the test-helper's open mode so the allow-list is
        // actually enforced here.
        service.allow_any_signer = false;
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

    /// Audit C2 — the fail-closed default. With NO allow-list configured AND
    /// `allow_any_signer = false` (the production default), a well-formed signed
    /// tx MUST be rejected. Pre-fix, `None` meant open and this tx would be
    /// accepted — letting anyone who could reach the port inject claims / GERs.
    #[tokio::test]
    async fn c2_default_fail_closed_rejects_signer_without_allow_list() {
        let mut service = create_test_service();
        // Production default: no allow-list, no open-mode opt-in.
        service.allow_any_signer = false;
        service.allowed_signers = None;

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xBBu8; 32]),
        }
        .abi_encode();
        let (input_hex, _signer) = encode_legacy_tx(calldata);

        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("C2: signer must be rejected under the fail-closed default");
        let msg = err.to_string();
        assert!(
            msg.contains("not on the allow-list"),
            "must cite the allow-list: {msg}"
        );
        assert!(
            msg.contains("--insecure-allow-any-signer"),
            "must point the operator at the explicit opt-in: {msg}"
        );
    }

    /// SOAK FINDING #1 — the orphaned-claim recovery. A `try_claim` record with NO
    /// ClaimEvent whose TTL has expired (the proxy died between the lock write and the
    /// CLAIM landing) must be superseded and the resubmission ACCEPTED, unwedging the
    /// sponsor. The lock is held again afterwards (superseded, not deleted).
    #[tokio::test]
    async fn orphaned_claim_record_recovers_after_ttl() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let gi = U256::from(0x8000000000000028u64); // the wedged soak gi shape
        store.try_claim(gi).await.expect("first submission locks");

        // "Crash": no ClaimEvent ever lands, and the record out-lives the TTL
        // (Duration::ZERO = instantly expired, keeps the test clock-free).
        let outcome = acquire_claim_lock(&store, gi, TxHash::ZERO, std::time::Duration::ZERO)
            .await
            .expect("orphaned record classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Acquired { fence: 1 },
            "orphaned record (no ClaimEvent, TTL expired) must be superseded → Acquired"
        );
        assert!(
            store.is_claimed(&gi).await.unwrap(),
            "recovery SUPERSEDES the record (lock re-held by the new submission), not deletes it"
        );
    }

    /// A LANDED claim is classified `Landed` (→ #55 accept-and-revert), NOT a hard
    /// reject — even with the TTL long expired, the LANDED classification wins over
    /// orphan recovery, for any submitter. (Pre-#55 this returned an "already
    /// submitted" error; the accept-and-revert routing replaces that at the lock.)
    #[tokio::test]
    async fn landed_claim_classified_as_landed() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let gi = U256::from(77u64);
        store.try_claim(gi).await.unwrap();
        // The claim LANDED: a ClaimEvent exists for this global_index.
        store
            .commit_manual_claim_event_atomic(
                "test-claim-note".into(),
                "0x00000000000000000000000000000000000000aa",
                5,
                [0u8; 32],
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                gi.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0u8; 20],
                1_000,
            )
            .await
            .unwrap();

        let outcome = acquire_claim_lock(&store, gi, TxHash::ZERO, std::time::Duration::ZERO)
            .await
            .expect("landed classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Landed,
            "a LANDED gi must classify as Landed (→ accept-and-revert), never a hard reject, \
             even with the TTL expired"
        );
    }

    // ── MANUAL USER CLAIM tests ─────────────────────────────────────────
    //
    // There is NO sponsor concept in this proxy: the bridge-service sponsor's
    // claims and an ordinary user's manual claims take the IDENTICAL path
    // through `service_send_raw_txn` (signer recovery → nonce → allow-list →
    // claim dispatch), and the claim dedup lock is keyed by globalIndex only.
    // These tests pin that behavior from both sides: a user CAN manually claim
    // (their own or anyone's deposit) if allow-listed, and the dedup / TTL
    // takeover semantics are signer-agnostic.

    /// Shared helpers for the manual-user-claim tests.
    ///
    /// Build + encode a legacy tx REALLY signed by `key` at an explicit
    /// `nonce`. Unlike `encode_legacy_tx_signed` (same-nonce concurrency
    /// helper, nonce pinned to 0), this one is general-purpose. Returns the
    /// hex payload and the tx hash (for receipt / store assertions).
    fn encode_tx_signed_with_nonce(
        key: &alloy::signers::local::PrivateKeySigner,
        input: Vec<u8>,
        nonce: u64,
    ) -> (String, TxHash) {
        use alloy::consensus::SignableTransaction;
        use alloy::signers::SignerSync;
        let txn = TxLegacy {
            nonce,
            input: input.into(),
            chain_id: Some(1),
            ..Default::default()
        };
        let signature = key
            .sign_hash_sync(&txn.signature_hash())
            .expect("signing the legacy test tx must succeed");
        let envelope: TxEnvelope = txn.into_signed(signature).into();
        let hash = match &envelope {
            TxEnvelope::Legacy(s) => *s.hash(),
            _ => unreachable!("constructed as legacy"),
        };
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        (format!("0x{}", ::hex::encode(encoded)), hash)
    }

    /// A valid `claimAsset` calldata for `create_test_service`'s network (1).
    /// Zero exit roots pair with `seed_zero_ger` for the C6 gate.
    fn claim_calldata(global_index: U256, destination: Address, amount: U256) -> Vec<u8> {
        claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: destination,
            amount,
            metadata: Default::default(),
        }
        .abi_encode()
    }

    /// Zero-padded MidenAccountId — resolvable by `address_mapper` without a
    /// store mapping, so a claim to it gets PAST the RD-860 swallow and onto
    /// the real lock + publish path.
    fn resolvable_dest() -> Address {
        alloy::primitives::address!("0x00000000ac0000000000dd110000ee000000fc00")
    }

    /// Mark the all-zero mainnet/rollup GER pair injected so the C6 pre-check
    /// passes (mirrors `test_claim_asset_no_event_on_failure`'s seeding).
    async fn seed_zero_ger(store: &std::sync::Arc<dyn crate::store::Store>) {
        let ger = crate::ger::combined_ger(&[0u8; 32], &[0u8; 32]);
        store
            .commit_ger_event_atomic(
                1,
                [0u8; 32],
                "0xger-seed",
                &ger,
                Some([0u8; 32]),
                Some([0u8; 32]),
                0,
            )
            .await
            .unwrap();
    }

    async fn count_claim_events(store: &std::sync::Arc<dyn crate::store::Store>) -> usize {
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        store
            .get_logs(&filter, 0xFFFF)
            .await
            .unwrap()
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .count()
    }

    /// MANUAL USER CLAIM happy path — an ordinary, explicitly allow-listed
    /// USER key (NOT open mode, NOT any sponsor identity) submits a valid
    /// `claimAsset` and is accepted end-to-end: ClaimEvent emitted, receipt
    /// recorded, and the recorded signer is the USER's address. Pins that a
    /// user needs nothing beyond allow-list membership to claim manually —
    /// there is no sponsor-only gate anywhere on the path.
    ///
    /// Unit-harness note: the stub `MidenClient` never runs the publish
    /// closure, so the only claim route that completes SYNCHRONOUSLY is the
    /// RD-860 unresolvable-destination swallow — which still exercises the
    /// full RPC pipeline (chain-id, nonce, allow-list, dispatch) and emits the
    /// synthetic ClaimEvent + receipt. The user here claims a deposit destined
    /// to their own EVM address (no Miden mapping registered → swallow). The
    /// real-Miden happy path is covered by scripts/e2e-manual-user-claim.sh.
    #[tokio::test]
    async fn manual_user_claim_succeeds() {
        let user_key = alloy::signers::local::PrivateKeySigner::random();
        let user_addr = user_key.address();

        let mut service = create_test_service();
        // A real allow-list containing ONLY the user — the manual claim must
        // pass on allow-list membership alone.
        service.allow_any_signer = false;
        service.allowed_signers = Some(vec![user_addr]);
        let store = service.store.clone();

        let gi = U256::from(0x1001u64);
        let calldata = claim_calldata(gi, user_addr, U256::from(1_000_000u64));
        let (input_hex, expected_hash) = encode_tx_signed_with_nonce(&user_key, calldata, 0);

        let tx_hash = service_send_raw_txn(service, input_hex)
            .await
            .expect("allow-listed user's manual claim must be accepted");
        assert_eq!(
            tx_hash, expected_hash,
            "returned hash must be the user's tx hash"
        );

        assert_eq!(
            count_claim_events(&store).await,
            1,
            "exactly one ClaimEvent must be emitted for the user's claim"
        );
        let txn = store
            .txn_get(tx_hash)
            .await
            .unwrap()
            .expect("the user's claim tx must be recorded");
        assert_eq!(
            txn.signer, user_addr,
            "the recorded signer must be the USER (no sponsor substitution anywhere)"
        );
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_some(),
            "a receipt must exist for the user's claim tx"
        );
        assert_eq!(
            store.nonce_get(&format!("{user_addr:#x}")).await.unwrap(),
            1,
            "the user's tracked nonce must advance on acceptance"
        );
    }

    /// Signer-agnostic dedup, in-flight then landed. Signer A ("the sponsor")
    /// has a submission genuinely in flight for gi=X (a fresh fenced lock,
    /// no ClaimEvent yet). Signer B — a DIFFERENT key — submits a full valid
    /// claimAsset for the SAME gi:
    ///   1. while A is IN FLIGHT, B crosses the durable acceptance boundary but
    ///      cannot acquire the claim fence, so it receives a status-0 receipt;
    ///      its nonce is consumed, with no Miden publish and no ClaimEvent.
    ///   2. after A lands, B same-hash rebroadcast deduplicates to the existing
    ///      status-0 receipt. A landed claim submitted under a fresh B nonce
    ///      follows the same geth-faithful accept-and-revert path.
    #[tokio::test]
    async fn user_sponsor_double_submit_same_global_index() {
        let service = create_test_service();
        let store = service.store.clone();
        let miden = service.miden_client.clone();
        seed_zero_ger(&store).await;

        let gi = U256::from(0x2002u64);
        store.try_claim(gi).await.expect("A's submission locks gi");

        // B: different key, same globalIndex, within the (default 120s) TTL.
        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let nonce_b = store.nonce_get(&format!("{addr_b:#x}")).await.unwrap();
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, nonce_b);

        let accepted_inflight = service_send_raw_txn(service.clone(), input_b.clone())
            .await
            .expect("the structurally valid transaction is accepted then reverted");
        assert_eq!(accepted_inflight, tx_b);
        // The claim must not reach Miden while A is in flight.
        assert!(
            !miden.test_was_called(),
            "B must never reach the Miden publish while A is in flight"
        );
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "accepted status-0 transaction consumes B nonce"
        );
        let (failed, _) = store.txn_receipt(tx_b).await.unwrap().unwrap();
        assert!(
            failed.is_err(),
            "in-flight claim becomes a status-0 receipt"
        );

        // A's claim LANDS: a ClaimEvent now exists for gi.
        store
            .commit_manual_claim_event_atomic(
                "manual-user-claim-test-note".into(),
                "0x00000000000000000000000000000000000000aa",
                5,
                [0u8; 32],
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                gi.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0u8; 20],
                1_000,
            )
            .await
            .unwrap();

        // #55 — B retries the same tx. Now that gi has LANDED, B's full-RPC
        // submission is ACCEPT-AND-REVERTED (geth-faithful AlreadyClaimed): it
        // is ACCEPTED (nonce consumed) with a reverted (status 0x0) receipt and
        // NO new ClaimEvent — instead of the old hard "already submitted"
        // reject — so a cross-signer's nonce sequence never desyncs.
        let claim_events_before = count_claim_events(&store).await;
        let accepted = service_send_raw_txn(service, input_b)
            .await
            .expect("a landed gi from a DIFFERENT tx is accept-and-reverted (#55), not rejected");
        assert_eq!(accepted, tx_b, "accept-and-revert returns B's own tx hash");
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "accept-and-revert must CONSUME B's nonce, exactly like a normal accept"
        );
        let (result, _blk) = store
            .txn_receipt(tx_b)
            .await
            .unwrap()
            .expect("accept-and-revert writes a durable receipt");
        assert!(
            result.is_err(),
            "the accept-and-revert receipt is status 0x0 (reverted)"
        );
        assert_eq!(
            count_claim_events(&store).await,
            claim_events_before,
            "accept-and-revert must NOT emit a second ClaimEvent"
        );
        assert!(
            !miden.test_was_called(),
            "accept-and-revert must not publish a second CLAIM to Miden"
        );

        // The claim-lock classification: a direct acquire_claim_lock on a LANDED
        // gi returns `Landed` even with the TTL forced to zero — the LANDED check
        // wins over orphan recovery, regardless of submitter (routes to
        // accept-and-revert, never a hard reject).
        let outcome = acquire_claim_lock(&store, gi, TxHash::ZERO, std::time::Duration::ZERO)
            .await
            .expect("landed classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Landed,
            "LANDED beats TTL-expiry recovery"
        );
    }

    /// TTL takeover by a DIFFERENT signer — PINS CURRENT (deliberate) BEHAVIOR:
    /// the claim submission lock is SIGNER-AGNOSTIC. The lock record stores no
    /// signer at all (`InMemoryStore::claimed` maps globalIndex → Instant;
    /// `acquire_claim_lock` takes no signer), so once a record is orphaned
    /// (no ClaimEvent + TTL expired), ANY allow-listed party may supersede it
    /// and finish the stranded claim. This is intentional: the claim's effect
    /// is identical regardless of submitter — destination, amount, and token
    /// are bound by the claimAsset calldata (whose proof commits to the L1
    /// leaf), so a takeover changes only who paid to submit, never where the
    /// funds go. The same property holds on the L1 PolygonZkEVMBridge, where
    /// claimAsset is fully permissionless.
    #[tokio::test]
    async fn ttl_expired_lock_superseded_by_different_signer() {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let service = crate::test_helpers::create_test_service_with_store(store.clone());
        let miden = service.miden_client.clone();
        seed_zero_ger(&store).await;

        let gi = U256::from(0x3003u64);
        // Signer A's submission crashed mid-flight: lock record present, no
        // ClaimEvent ever landed...
        store.try_claim(gi).await.expect("A's submission locks gi");
        // ...and the record has out-lived CLAIM_RESUBMIT_TTL_SECS (backdated —
        // no sleeping, no process-global env mutation).
        concrete.test_backdate_claim(
            gi,
            claim_resubmit_ttl() + std::time::Duration::from_secs(10),
        );

        // Signer B (a different key — the record wouldn't know: it carries no
        // signer) submits the same gi through the FULL RPC path.
        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let nonce_b = store
            .nonce_get(&format!("{:#x}", key_b.address()))
            .await
            .unwrap();
        let (input_b, _) = encode_tx_signed_with_nonce(&key_b, calldata, nonce_b);

        let accepted = service_send_raw_txn(service, input_b)
            .await
            .expect("the orphaned record is reclaimed and the tx is accepted");
        let (failed, _) = store.txn_receipt(accepted).await.unwrap().unwrap();
        assert!(
            failed.is_err(),
            "the stub publish failure becomes status 0x0"
        );
        assert!(
            miden.test_was_called(),
            "B's claim must reach the Miden publish — the orphaned lock was superseded"
        );
    }

    /// Allow-list × claimAsset — the existing R2/C2 tests exercise the gate
    /// with insertGlobalExitRoot calldata only; this pins it on the CLAIM path
    /// specifically, including the fail-closed default, and that the rejection
    /// happens BEFORE any claim side effect. The claim is fully valid (GER
    /// seeded, resolvable destination), so if the gate failed to fire it WOULD
    /// proceed to the lock — making the no-side-effect assertions meaningful.
    #[tokio::test]
    async fn unauthorized_signer_claim_rejected() {
        let user_key = alloy::signers::local::PrivateKeySigner::random();
        let user_addr = user_key.address();
        let gi = U256::from(0x4004u64);

        // (a) allow-list configured, signer NOT on it.
        let mut service = create_test_service();
        service.allow_any_signer = false;
        let foreign: Address = "0xdeAddeaDdEadDeaDDEaDDeadDEADDeaDDEAdDEaD"
            .parse()
            .unwrap();
        service.allowed_signers = Some(vec![foreign]);
        let store = service.store.clone();
        let miden = service.miden_client.clone();
        seed_zero_ger(&store).await;
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let nonce = store.nonce_get(&format!("{user_addr:#x}")).await.unwrap();
        let (input_hex, tx_hash) = encode_tx_signed_with_nonce(&user_key, calldata, nonce);

        let err = service_send_raw_txn(service, input_hex.clone())
            .await
            .expect_err("non-allow-listed signer's claimAsset must be rejected");
        assert!(
            err.to_string().contains("not on the allow-list"),
            "unexpected: {err:#}"
        );
        // Rejected BEFORE any lock / receipt / nonce / Miden side effect.
        assert!(
            !store.is_claimed(&gi).await.unwrap(),
            "no claimed_indices entry may exist for a rejected signer's gi"
        );
        assert!(
            store.txn_get(tx_hash).await.unwrap().is_none(),
            "no tx entry may be recorded"
        );
        assert!(
            store.txn_receipt(tx_hash).await.unwrap().is_none(),
            "no receipt may be recorded"
        );
        assert_eq!(
            store.nonce_get(&format!("{user_addr:#x}")).await.unwrap(),
            0,
            "the nonce must not advance for a rejected signer"
        );
        assert!(!miden.test_was_called(), "Miden must never be touched");
        assert_eq!(count_claim_events(&store).await, 0);

        // (b) fail-closed default (audit C2): NO allow-list configured at all →
        // the same valid claimAsset is rejected identically.
        let mut service = create_test_service();
        service.allow_any_signer = false;
        service.allowed_signers = None;
        let store = service.store.clone();
        seed_zero_ger(&store).await;

        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("claimAsset must be rejected under the fail-closed default");
        assert!(
            err.to_string().contains("not on the allow-list"),
            "unexpected: {err:#}"
        );
        assert!(!store.is_claimed(&gi).await.unwrap());
    }

    /// Claims are PERMISSIONLESS — pins the EVM-bridge-equivalent design: on
    /// the L1 PolygonZkEVMBridge anyone may call claimAsset for any leaf, and
    /// the funds go to the destinationAddress bound in the (merkle-proven)
    /// calldata, never to the caller. The proxy mirrors that: the submitter
    /// only needs to pass the allow-list; NOTHING compares the recovered
    /// signer to destinationAddress. Signer C claims a deposit whose
    /// destination is someone else entirely, on both claim routes.
    #[tokio::test]
    async fn claim_for_someone_elses_deposit_permissionless() {
        let key_c = alloy::signers::local::PrivateKeySigner::random();
        let addr_c = key_c.address();

        let mut service = create_test_service();
        service.allow_any_signer = false;
        service.allowed_signers = Some(vec![addr_c]);
        let store = service.store.clone();
        let miden = service.miden_client.clone();
        seed_zero_ger(&store).await;

        // Leg 1 — REAL claim route: destination is a resolvable Miden-mapped
        // address that is NOT C. The claim passes every signer gate, acquires
        // the lock, and reaches the Miden publish (the unit stub cannot
        // complete it — but the failure is the stub's, not any
        // signer≠destination authorization error, because no such check
        // exists).
        let gi_real = U256::from(0x5005u64);
        assert_ne!(resolvable_dest(), addr_c);
        let calldata = claim_calldata(gi_real, resolvable_dest(), U256::from(1_000_000u64));
        let nonce_real = store.nonce_get(&format!("{addr_c:#x}")).await.unwrap();
        let (input_hex, tx_real) = encode_tx_signed_with_nonce(&key_c, calldata, nonce_real);
        let accepted = service_send_raw_txn(service.clone(), input_hex)
            .await
            .expect("permissionless structurally valid claim is accepted");
        assert_eq!(accepted, tx_real);
        let (failed, _) = store.txn_receipt(tx_real).await.unwrap().unwrap();
        assert!(
            failed.is_err(),
            "the unit-stub publish failure becomes status 0x0"
        );
        assert!(
            miden.test_was_called(),
            "C's claim for someone else's deposit must reach the Miden publish"
        );

        // Leg 2 — synchronous accept (RD-860 swallow route, the only one that
        // completes under the stub): destination is a third party's EVM
        // address ≠ C. Accepted, ClaimEvent emitted, and the recorded signer
        // is C — the destination in the calldata is untouched by who signed.
        //
        // Fresh service so C's nonce-0 slot is a clean reservation: leg 1 already
        // reserved (addr_c, 0) for its (different-calldata) tx — #55 BLOCKER 1
        // correctly refuses a DIFFERENT tx at that same slot, so a genuinely new
        // leg-2 scenario needs its own reservation namespace (a real sponsor would
        // retry leg 1's SAME tx, not submit a different claim at the same nonce).
        let mut service2 = create_test_service();
        service2.allow_any_signer = false;
        service2.allowed_signers = Some(vec![addr_c]);
        let store = service2.store.clone();
        seed_zero_ger(&store).await;
        let gi_swallow = U256::from(0x5006u64);
        let someone_else = Address::from([0x77u8; 20]);
        assert_ne!(someone_else, addr_c);
        let calldata = claim_calldata(gi_swallow, someone_else, U256::from(1_000_000u64));
        let nonce_swallow = store.nonce_get(&format!("{addr_c:#x}")).await.unwrap();
        let (input_hex, tx_hash) = encode_tx_signed_with_nonce(&key_c, calldata, nonce_swallow);
        let accepted = service_send_raw_txn(service2, input_hex)
            .await
            .expect("permissionless claim for someone else's deposit must be accepted");
        assert_eq!(accepted, tx_hash);
        assert_eq!(count_claim_events(&store).await, 1);
        let txn = store.txn_get(tx_hash).await.unwrap().expect("tx recorded");
        assert_eq!(
            txn.signer, addr_c,
            "the recorded signer is the submitter; the destination stays the \
             calldata's, not the signer's"
        );
        let rec = store
            .get_unclaimable_claim(&gi_swallow)
            .await
            .unwrap()
            .expect("swallow route records the unclaimable entry");
        assert_eq!(
            rec.destination_address, someone_else,
            "funds are bound to the calldata's destination regardless of submitter"
        );
    }

    // ── #55 ACCEPT-AND-REVERT tests ─────────────────────────────────────
    //
    // A claimAsset targeting an already-LANDED globalIndex (a real ClaimEvent
    // exists) from a DIFFERENT tx must be ACCEPTED with a reverted (status 0x0)
    // receipt so the submitter's nonce is consumed — the geth-faithful
    // AlreadyClaimed revert. This is what keeps the aggkit sponsor's nonce
    // sequence in lockstep after a user front-runs a gi (#55).

    /// Land a claim for `gi` directly on the store (lock + ClaimEvent), exactly
    /// the state left behind by signer A's successful claim.
    async fn land_claim_for(store: &std::sync::Arc<dyn crate::store::Store>, gi: U256) {
        store.try_claim(gi).await.expect("A's submission locks gi");
        store
            .commit_manual_claim_event_atomic(
                "accept-revert-landed-note".into(),
                "0x00000000000000000000000000000000000000aa",
                5,
                [0u8; 32],
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                gi.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0u8; 20],
                1_000,
            )
            .await
            .expect("landing A's ClaimEvent");
    }

    /// Test 1 — landed globalIndex + matching nonce + DIFFERENT signer →
    /// ACCEPTED, hash returned, signer nonce consumed, a status-0x0 receipt is
    /// retrievable via `eth_getTransactionReceipt` with EMPTY logs, NO new
    /// ClaimEvent, and the real landed claim is untouched.
    #[tokio::test]
    async fn landed_gi_different_signer_accept_and_reverts() {
        let service = create_test_service();
        let store = service.store.clone();
        let miden = service.miden_client.clone();
        seed_zero_ger(&store).await;

        let gi = U256::from(0x5501u64);
        land_claim_for(&store, gi).await;
        assert_eq!(count_claim_events(&store).await, 1, "A's ClaimEvent landed");

        // Signer B — a DIFFERENT key — submits a full valid claim for the SAME
        // gi at its expected nonce (0). Its hash is not in the store, so the
        // RD-940 tx-hash dedup does not fire; it reaches the accept-and-revert.
        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b).await.expect(
            "a landed-gi claim from a different signer must be ACCEPTED (accept-and-revert)",
        );
        assert_eq!(accepted, tx_b, "must return B's own tx hash");

        // (1) B's nonce is CONSUMED — exactly like a normal accept.
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "accept-and-revert must consume the signer's nonce"
        );

        // (2) A durable REVERTED receipt is retrievable, status 0x0, empty logs.
        let receipt = crate::service_get_txn_receipt::service_get_txn_receipt(
            service.clone(),
            format!("{tx_b:#x}"),
        )
        .await
        .unwrap()
        .expect("eth_getTransactionReceipt must return a non-null receipt");
        assert!(
            matches!(
                receipt.inner.as_receipt().unwrap().status,
                alloy::consensus::Eip658Value::Eip658(false)
            ),
            "receipt status must be 0x0 (reverted)"
        );
        assert!(
            receipt.inner.as_receipt().unwrap().logs.is_empty(),
            "the reverted receipt must carry EMPTY logs (no second event)"
        );
        assert_eq!(
            receipt.from, addr_b,
            "receipt.from must be the submitting signer"
        );
        assert_eq!(
            receipt.block_number,
            Some(store.get_latest_block_number().await.unwrap())
        );

        // (3) NO new ClaimEvent was emitted — the real landed one is authoritative.
        assert_eq!(
            count_claim_events(&store).await,
            1,
            "accept-and-revert must NOT emit a second ClaimEvent"
        );

        // (4) B never reached the Miden publish, and the real claim's lock stands.
        assert!(
            !miden.test_was_called(),
            "accept-and-revert must not publish a second CLAIM to Miden"
        );
        assert!(
            store.is_claimed(&gi).await.unwrap(),
            "the real landed claim's lock is untouched"
        );
        assert!(
            store
                .has_claim_event_for_global_index(&gi.to_be_bytes::<32>())
                .await
                .unwrap(),
            "the real landed claim's ClaimEvent is untouched"
        );
    }

    /// Test 2 — THE ANTI-WEDGE SEQUENCE (the core #55 regression). Signer S
    /// submits gi=X (already landed) at nonce N → accepted+reverted, its
    /// expected nonce advances to N+1; S then submits gi=Y (a normal,
    /// not-yet-landed claim) at nonce N+1 → accepted normally. No desync / wedge.
    ///
    /// Without accept-and-revert the first tx would HARD-REJECT without
    /// consuming the nonce, S's expected nonce would stay N, and the second tx
    /// (nonce N+1) would fail the R4 gate with "nonce mismatch" forever — the
    /// permanent autoclaim wedge. See the mutation-check note in the PR report.
    #[tokio::test]
    async fn anti_wedge_sequence_landed_then_normal_stays_in_lockstep() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;

        // The sponsor key S.
        let key_s = alloy::signers::local::PrivateKeySigner::random();
        let addr_s = key_s.address();

        // gi=X is already landed (a user front-ran it).
        let gi_x = U256::from(0x5502u64);
        land_claim_for(&store, gi_x).await;

        // S submits its (persisted) monitored tx for gi=X at nonce 0.
        let calldata_x = claim_calldata(gi_x, resolvable_dest(), U256::from(1_000_000u64));
        let (input_x, _tx_x) = encode_tx_signed_with_nonce(&key_s, calldata_x, 0);
        service_send_raw_txn(service.clone(), input_x)
            .await
            .expect("landed gi=X must be accept-and-reverted, not rejected");
        assert_eq!(
            store.nonce_get(&format!("{addr_s:#x}")).await.unwrap(),
            1,
            "S's expected nonce must advance to 1 after accept-and-revert"
        );

        // S's NEXT claim gi=Y at nonce 1 — a normal claim (unresolvable dest so
        // the unit harness completes it synchronously via the RD-860 swallow).
        // The point is it PASSES the R4 nonce gate (nonce 1 == expected 1).
        let gi_y = U256::from(0x5503u64);
        let calldata_y =
            claim_calldata(gi_y, Address::from([0x99u8; 20]), U256::from(1_000_000u64));
        let (input_y, _tx_y) = encode_tx_signed_with_nonce(&key_s, calldata_y, 1);
        let res_y = service_send_raw_txn(service.clone(), input_y).await;
        assert!(
            res_y.is_ok(),
            "S's next claim at nonce 1 must be accepted (NO wedge): {res_y:?}"
        );
        assert!(
            !format!("{:?}", res_y).contains("nonce mismatch"),
            "the sequence must not produce a nonce-mismatch wedge"
        );
        assert_eq!(
            store.nonce_get(&format!("{addr_s:#x}")).await.unwrap(),
            2,
            "S's nonce advances to 2 — perfectly in lockstep, no desync"
        );
    }

    /// Test 3 — RD-940 idempotent rebroadcast in the accept-and-revert context:
    /// resubmitting the SAME tx-hash (the aggkit ethtxmanager re-broadcast) must
    /// return the same hash via the tx-hash dedup and must NOT create a second /
    /// new receipt or advance the nonce again.
    #[tokio::test]
    async fn accept_and_revert_rebroadcast_same_hash_no_duplicate_receipt() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;

        let gi = U256::from(0x5504u64);
        land_claim_for(&store, gi).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        // First submission → accept-and-revert (reverted receipt written).
        let first = service_send_raw_txn(service.clone(), input_b.clone())
            .await
            .expect("first submit accept-and-reverts");
        assert_eq!(first, tx_b);
        assert_eq!(store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(), 1);
        let (r1, _b1) = store
            .txn_receipt(tx_b)
            .await
            .unwrap()
            .expect("a receipt exists after accept-and-revert");
        assert!(r1.is_err(), "the receipt is a revert (status 0x0)");

        // Re-broadcast the SAME wire bytes → RD-940 tx-hash dedup returns the
        // same hash WITHOUT a new accept-and-revert (nonce unchanged, still one
        // receipt, still exactly one ClaimEvent).
        let second = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("re-broadcast must dedup to the same hash");
        assert_eq!(second, tx_b, "dedup returns the original hash");
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "a same-hash rebroadcast must NOT advance the nonce again"
        );
        assert_eq!(
            count_claim_events(&store).await,
            1,
            "no second ClaimEvent from the rebroadcast"
        );
    }

    /// Test 4a — a landed-gi claim whose nonce is STALE must still be rejected by
    /// the R4 gate (NOT accept-and-reverted). accept-and-revert must never paper
    /// over a real nonce gap.
    #[tokio::test]
    async fn landed_gi_stale_nonce_still_rejected_not_reverted() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;

        let gi = U256::from(0x5505u64);
        land_claim_for(&store, gi).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        // Advance B's tracked nonce so a nonce-0 tx is stale.
        for _ in 0..5 {
            store
                .nonce_increment(&format!("{addr_b:#x}"))
                .await
                .unwrap();
        }
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let err = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect_err(
                "a stale-nonce landed claim must be rejected by R4, not accept-and-reverted",
            );
        assert!(
            err.to_string().contains("nonce mismatch"),
            "must be the R4 nonce-mismatch rejection, not an accept: {err:#}"
        );
        // No receipt was written (it was rejected, not accept-and-reverted).
        assert!(
            store.txn_receipt(tx_b).await.unwrap().is_none(),
            "a rejected stale-nonce claim must NOT leave a receipt"
        );
    }

    /// Test 4b — a landed-gi claim whose nonce is in the FUTURE (writer-worker
    /// mode) must take the normal R4 retryable future-nonce WAIT, not
    /// accept-and-revert immediately. Proves accept-and-revert only fires for a
    /// tx the R4 gate would otherwise admit (nonce == expected).
    #[tokio::test]
    async fn landed_gi_future_nonce_waits_not_reverted() {
        let mut service = create_test_service();
        let store = service.store.clone();
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        service.writer_handle = Some(std::sync::Arc::new(handle));
        seed_zero_ger(&store).await;

        let gi = U256::from(0x5506u64);
        land_claim_for(&store, gi).await;

        // Submit at nonce 1 while expected is 0 → future nonce → must WAIT.
        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, _tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 1);

        let svc = service.clone();
        let pending = tokio::spawn(async move { service_send_raw_txn(svc, input_b).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !pending.is_finished(),
            "a future-nonce landed claim must WAIT on R4, not short-circuit to accept-and-revert"
        );
        pending.abort();
        let _ = shutdown.send(());
    }

    /// Test 5 — async-writer-mode variant of Test 1: a landed-gi claim from a
    /// DIFFERENT signer, enqueued to the writer worker, is accept-and-reverted
    /// by the worker (reverted receipt, no second ClaimEvent), and the nonce is
    /// consumed at enqueue time (RD-940 flow).
    #[tokio::test]
    async fn landed_gi_accept_and_revert_async_writer_mode() {
        let mut service = create_test_service();
        let store = service.store.clone();
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        service.writer_handle = Some(std::sync::Arc::new(handle));
        seed_zero_ger(&store).await;

        let gi = U256::from(0x5507u64);
        land_claim_for(&store, gi).await;
        assert_eq!(count_claim_events(&store).await, 1);

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("enqueue must return the tx hash");
        assert_eq!(accepted, tx_b);
        // Nonce consumed at enqueue (RD-940 flow).
        assert_eq!(store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(), 1);

        // Poll for the worker to write the reverted receipt.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some((result, _b)) = store.txn_receipt(tx_b).await.unwrap() {
                assert!(
                    result.is_err(),
                    "the async accept-and-revert receipt must be status 0x0"
                );
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("worker did not write the reverted receipt within 10s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            count_claim_events(&store).await,
            1,
            "async accept-and-revert must NOT emit a second ClaimEvent"
        );
        let _ = shutdown.send(());
    }

    /// BLOCKER 1 (landed-state TOCTOU) — deterministic race, at the lock boundary.
    ///
    /// Pre-fix a standalone `has_claim_event` pre-check ran BEFORE the lock: it
    /// could read "not landed", the real claim could LAND, and `acquire_claim_lock`
    /// would then hard-reject → the sponsor's nonce is NOT consumed → wedge. The
    /// fix makes "landed" a TYPED, ATOMIC outcome of `acquire_claim_lock` itself.
    ///
    /// This models the exact interleaving deterministically: the same lock record
    /// classifies `InFlight` while in flight (no ClaimEvent) and flips to `Landed`
    /// the instant the ClaimEvent is recorded — the landed detection is atomic with
    /// the lock decision, so there is NO interleaving in which a landed gi is
    /// hard-rejected.
    #[tokio::test]
    async fn blocker1_landed_at_lock_boundary_flips_inflight_to_landed() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let gi = U256::from(0x550au64);
        // Signer A's claim is IN FLIGHT: locked, NO ClaimEvent yet — the exact state
        // a pre-check would read as "not landed".
        store.try_claim(gi).await.expect("A locks gi");
        assert_eq!(
            acquire_claim_lock(&store, gi, TxHash::ZERO, claim_resubmit_ttl())
                .await
                .expect("classify must not error"),
            ClaimLockOutcome::InFlight,
            "in flight (no ClaimEvent) → InFlight — a pre-check here would read 'not landed'"
        );

        // The racing interleaving: A's claim LANDS (ClaimEvent recorded) in the
        // window a pre-check-then-lock design would have left open.
        store
            .commit_manual_claim_event_atomic(
                "toctou-note".into(),
                "0x00000000000000000000000000000000000000aa",
                5,
                [0u8; 32],
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                gi.to_be_bytes::<32>(),
                0,
                &[0u8; 20],
                &[0u8; 20],
                1_000,
            )
            .await
            .unwrap();

        // The SAME lock call now classifies `Landed` — routed to accept-and-revert,
        // NEVER a hard reject. The TOCTOU window is closed.
        assert_eq!(
            acquire_claim_lock(&store, gi, TxHash::ZERO, claim_resubmit_ttl())
                .await
                .expect("classify must not error"),
            ClaimLockOutcome::Landed,
            "once landed, the lock classifies Landed (→ accept-and-revert), never hard-reject"
        );
    }

    /// BLOCKER 1 — end-to-end: a landed-at-lock-boundary claim from a DIFFERENT
    /// signer goes through the FULL RPC path and is accept-and-reverted (nonce
    /// consumed, reverted receipt), never hard-rejected — no nonce desync.
    #[tokio::test]
    async fn blocker1_landed_at_lock_boundary_accept_and_reverts_e2e() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;
        let gi = U256::from(0x550bu64);
        // The landed state (locked + ClaimEvent) is present before B's submission
        // classifies it — i.e. A's claim has LANDED by the time B reaches the lock.
        land_claim_for(&store, gi).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("landed-at-lock-boundary must accept-and-revert, not hard-reject");
        assert_eq!(accepted, tx_b);
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "nonce consumed — no desync (the wedge is unreachable)"
        );
        let (result, _blk) = store
            .txn_receipt(tx_b)
            .await
            .unwrap()
            .expect("reverted receipt written");
        assert!(result.is_err(), "status 0x0 reverted");
    }

    /// BLOCKER 2 (sync-path crash/error atomicity) — deterministic crash-boundary.
    ///
    /// On the sync accept path the durable receipt write and the per-signer
    /// `nonce_increment` are separate steps; a crash BETWEEN them leaves the tx
    /// KNOWN (receipt persisted) but the nonce STALE. We simulate exactly that
    /// durable state (persist the reverted receipt WITHOUT advancing the nonce),
    /// then rebroadcast the same tx: the RD-940 same-hash dedup path REPAIRS the
    /// nonce (advances it, completing the interrupted accept), and the signer's
    /// NEXT tx (nonce+1) then passes R4 — the sponsor is NOT left wedged.
    #[tokio::test]
    async fn blocker2_crash_gap_rebroadcast_repairs_stale_nonce() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let signer_str = format!("{addr_b:#x}");
        let gi = U256::from(0x550cu64);
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        // Reconstruct the envelope and persist a COMMITTED (reverted) receipt for
        // B's tx at nonce 0 WITHOUT advancing the nonce — the exact durable state a
        // crash between the receipt commit and the nonce advance leaves.
        let payload = crate::hex::hex_decode_prefixed(&input_b).unwrap();
        let mut slice = payload.as_slice();
        let env = TxEnvelope::decode_2718(&mut slice).unwrap();
        store
            .txn_begin(
                tx_b,
                crate::store::TxnEntry {
                    id: None,
                    envelope: env,
                    signer: addr_b,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        let blk = store.get_latest_block_number().await.unwrap();
        store
            .txn_commit(
                tx_b,
                Err("simulated crash-gap reverted receipt".into()),
                blk,
                [0u8; 32],
            )
            .await
            .unwrap();
        // Precondition: tx KNOWN, nonce STALE at 0.
        assert!(
            store.txn_get(tx_b).await.unwrap().is_some(),
            "receipt persisted"
        );
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            0,
            "nonce not yet advanced"
        );

        // REBROADCAST the same tx → dedup path detects expected(0) == tx.nonce(0)
        // and repairs the nonce.
        let res = service_send_raw_txn(service.clone(), input_b.clone())
            .await
            .expect("rebroadcast returns the known hash");
        assert_eq!(res, tx_b);
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            1,
            "crash-gap nonce REPAIRED on rebroadcast — the interrupted accept completed"
        );

        // Idempotent: a further rebroadcast does NOT advance again (expected 1 != 0).
        service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("second rebroadcast still Ok");
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            1,
            "repair is idempotent — no double advance"
        );

        // The signer's NEXT tx (nonce 1) now passes R4 — NOT wedged. Use an
        // unresolvable destination so it completes synchronously (RD-860 swallow).
        let calldata_next = claim_calldata(
            U256::from(0x550du64),
            Address::from([0x99u8; 20]),
            U256::from(1u64),
        );
        let (input_next, _) = encode_tx_signed_with_nonce(&key_b, calldata_next, 1);
        service_send_raw_txn(service, input_next)
            .await
            .expect("next tx at nonce 1 must be accepted — the sponsor is not wedged");
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            2,
            "sequence stays in lockstep after crash-gap recovery"
        );
    }

    /// BLOCKER A — a claim for an already-LANDED gi whose destination is
    /// UNRESOLVABLE must route to accept-and-revert (landed classification FIRST),
    /// NOT take RD-860's success path and emit a SECOND ClaimEvent (double-emit).
    #[tokio::test]
    async fn blocker_a_landed_unresolvable_destination_accept_and_reverts_no_double_emit() {
        let service = create_test_service();
        let store = service.store.clone();
        seed_zero_ger(&store).await;
        let gi = U256::from(0x55a1u64);
        land_claim_for(&store, gi).await;
        let events_before = count_claim_events(&store).await;

        // B: SAME landed gi, but an UNRESOLVABLE destination (non-zero-padded, no
        // mapping) — RD-860 would normally swallow + emit a ClaimEvent.
        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, Address::from([0x99u8; 20]), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b).await.expect(
            "landed gi + unresolvable dest must accept-and-revert (landed classified FIRST)",
        );
        assert_eq!(accepted, tx_b);
        assert_eq!(
            count_claim_events(&store).await,
            events_before,
            "RD-860 must NOT run for a landed gi — no SECOND ClaimEvent (double-emit)"
        );
        assert!(
            store.get_unclaimable_claim(&gi).await.unwrap().is_none(),
            "RD-860 must NOT record an unclaimable entry for a landed gi"
        );
        assert_eq!(store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(), 1);
        let (result, _blk) = store.txn_receipt(tx_b).await.unwrap().expect("receipt");
        assert!(result.is_err(), "reverted (status 0x0)");
    }

    /// BLOCKER A — a claim for an already-LANDED gi whose GER is NOT observed must
    /// route to accept-and-revert (landed classification FIRST), NOT hard-reject on
    /// C6 WITHOUT consuming the nonce.
    #[tokio::test]
    async fn blocker_a_landed_unobserved_ger_accept_and_reverts_nonce_consumed() {
        let service = create_test_service();
        let store = service.store.clone();
        // Deliberately DO NOT seed the GER — C6 would reject "not observed yet".
        let gi = U256::from(0x55a2u64);
        land_claim_for(&store, gi).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("landed gi + unobserved GER must accept-and-revert (landed classified FIRST)");
        assert_eq!(accepted, tx_b);
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "nonce CONSUMED (accept-and-revert), not a C6 hard-reject that leaves it stale"
        );
        let (result, _blk) = store.txn_receipt(tx_b).await.unwrap().expect("receipt");
        assert!(result.is_err(), "reverted (status 0x0)");
    }

    /// BLOCKER B — atomic classification. Drive the exact interleaving: the claim
    /// LANDS in the window between `acquire_claim_lock`'s first `has_claim_event`
    /// read (miss) and the reclaim attempt. The re-read AFTER the failed reclaim
    /// must classify `Landed` (→ accept-and-revert), NEVER `InFlight`.
    #[tokio::test]
    async fn blocker_b_lands_in_reclaim_window_classifies_landed() {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let gi = U256::from(0x55b0u64);
        // A genuine in-flight lock (fresh, within TTL, NO ClaimEvent yet): the first
        // read misses and try_reclaim_expired returns false → would be InFlight.
        store.try_claim(gi).await.unwrap();
        // Arm the race: the FIRST has_claim_event miss lands the claim, so the
        // BLOCKER-B re-read (after the failed reclaim) observes it.
        concrete.test_land_gi_after_next_has_claim_miss(gi.to_be_bytes::<32>());

        let outcome = acquire_claim_lock(
            &store,
            gi,
            TxHash::ZERO,
            std::time::Duration::from_secs(3600),
        )
        .await
        .expect("classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Landed,
            "a gi that lands in the try_claim-Err → reclaim window must classify Landed, \
             never InFlight (BLOCKER B re-read)"
        );
    }

    /// BLOCKER C — the atomic store op `commit_reverted_receipt_and_advance_nonce`
    /// commits the reverted receipt AND advances the nonce in ONE call (no half
    /// state). Tested DIRECTLY on the store (not via the RPC path, whose caller-side
    /// CAS would otherwise mask the method's own nonce advance): the method itself
    /// must (a) write a COMMITTED reverted receipt — never a pending-forever row —
    /// and (b) CAS-advance the nonce, returning whether it won; and be idempotent.
    /// (The `test_pgstore_commit_reverted_receipt_and_advance_nonce` twin asserts
    /// the same on PgStore.)
    #[tokio::test]
    async fn blocker_c_accept_and_revert_atomic_receipt_and_nonce() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let signer = Address::from([0x5cu8; 20]);
        let signer_str = format!("{signer:#x}");
        let tx_hash = TxHash::from([0x5du8; 32]);
        let envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            alloy::primitives::Signature::test_signature(),
            tx_hash,
        ));
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);

        // ONE atomic call: receipt committed-reverted + nonce CAS-advanced.
        let advanced = store
            .commit_reverted_receipt_and_advance_nonce(
                tx_hash,
                crate::store::TxnEntry {
                    id: None,
                    envelope: envelope.clone(),
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
                "landed (AlreadyClaimed) #55".into(),
                7,
                [0u8; 32],
                &signer_str,
                0,
            )
            .await
            .unwrap();
        assert!(
            advanced,
            "the nonce CAS must win on the sync accept path (expected == 0)"
        );

        // (a) receipt is COMMITTED (non-null → eth_getTransactionReceipt resolves)
        // and reverted (status 0x0) — never a pending-forever row.
        let (result, block) = store
            .txn_receipt(tx_hash)
            .await
            .unwrap()
            .expect("receipt is committed, never pending");
        assert!(result.is_err(), "reverted status 0x0");
        assert_eq!(block, 7);
        // (b) nonce advanced ATOMICALLY with the receipt — no half state.
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);

        // Idempotent re-entry (expected 0 again): receipt re-affirmed, nonce NOT
        // double-advanced (the CAS no-ops because the nonce already moved to 1).
        let advanced_again = store
            .commit_reverted_receipt_and_advance_nonce(
                tx_hash,
                crate::store::TxnEntry {
                    id: None,
                    envelope,
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
                "landed (AlreadyClaimed) #55".into(),
                7,
                [0u8; 32],
                &signer_str,
                0,
            )
            .await
            .unwrap();
        assert!(
            !advanced_again,
            "re-entry must not double-advance the nonce"
        );
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_some());
    }

    /// BLOCKER D — store-level nonce CAS: two concurrent advances at expected N →
    /// exactly ONE wins, the nonce ends at N+1 (never N+2 — the cross-replica skip).
    #[tokio::test]
    async fn blocker_d_nonce_cas_concurrent_advances_exactly_one_wins() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let addr = "0x00000000000000000000000000000000000000dd";
        // Prime the nonce to N = 5 (two replicas both read expected 5).
        for _ in 0..5 {
            store.nonce_increment(addr).await.unwrap();
        }
        assert_eq!(store.nonce_get(addr).await.unwrap(), 5);

        let s1 = store.clone();
        let s2 = store.clone();
        let h1 = tokio::spawn(async move { s1.nonce_advance_cas(addr, 5).await.unwrap() });
        let h2 = tokio::spawn(async move { s2.nonce_advance_cas(addr, 5).await.unwrap() });
        let (w1, w2) = (h1.await.unwrap(), h2.await.unwrap());

        assert_eq!(
            [w1, w2].iter().filter(|w| **w).count(),
            1,
            "exactly ONE CAS at expected=5 may win"
        );
        assert_eq!(
            store.nonce_get(addr).await.unwrap(),
            6,
            "nonce advances to exactly N+1 (never N+2 — the cross-replica skip)"
        );
    }

    /// BLOCKER 1 — the fenced admission-lease LIFECYCLE: fresh → Won(fence); the
    /// SAME tx under a valid lease → OwnedBySame (dedup, another replica must not
    /// execute); a DIFFERENT tx → HeldByOther (hard reject); release-FAILURE lets the
    /// SAME tx take over (fence bumps); a fenced-out stale release is ignored;
    /// release-SUCCESS remains recoverable by the SAME durable tx; only that hash
    /// can ever take over the slot.
    #[tokio::test]
    async fn blocker_1_reserve_nonce_fenced_lifecycle() {
        use crate::store::NonceReservation;
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let addr = "0x00000000000000000000000000000000000000dd";
        let h1 = TxHash::from([0x11u8; 32]);
        let h2 = TxHash::from([0x22u8; 32]);
        let lease = std::time::Duration::from_secs(90);

        // Fresh → Won(fence 1).
        let NonceReservation::Won { fence } =
            store.reserve_nonce(addr, 5, h1, lease).await.unwrap()
        else {
            panic!("fresh slot must be Won");
        };
        assert_eq!(fence, 1);
        // Same tx, VALID lease → OwnedBySame (another replica must NOT execute).
        assert_eq!(
            store.reserve_nonce(addr, 5, h1, lease).await.unwrap(),
            NonceReservation::OwnedBySame
        );
        // A DIFFERENT tx at the same slot → HeldByOther(the winner) → hard reject.
        assert_eq!(
            store.reserve_nonce(addr, 5, h2, lease).await.unwrap(),
            NonceReservation::HeldByOther(h1)
        );

        // release-FAILURE → the SAME tx may TAKE OVER (fence bumps).
        store
            .release_reservation(addr, 5, h1, fence, false)
            .await
            .unwrap();
        let NonceReservation::Won { fence: fence2 } =
            store.reserve_nonce(addr, 5, h1, lease).await.unwrap()
        else {
            panic!("after release-failure the same tx must retake ownership");
        };
        assert!(fence2 > fence, "takeover must bump the fence");
        // A STALE prior owner (old fence) is fenced out — its release-FAILURE must be
        // IGNORED. Without the fence guard it would flip the CURRENT owner's lease to
        // released_failure and let a spurious takeover in; with it, the current owner
        // (fence2) still holds the slot.
        store
            .release_reservation(addr, 5, h1, fence, false)
            .await
            .unwrap();
        assert_eq!(
            store.reserve_nonce(addr, 5, h1, lease).await.unwrap(),
            NonceReservation::OwnedBySame,
            "a fenced-out stale release must not flip the current owner's state"
        );

        // release-SUCCESS (by the current owner) → the SAME durable tx can
        // resume after a restart loses the in-memory queue.
        store
            .release_reservation(addr, 5, h1, fence2, true)
            .await
            .unwrap();
        assert!(matches!(
            store.reserve_nonce(addr, 5, h1, lease).await.unwrap(),
            NonceReservation::Won { fence } if fence > fence2
        ));

        // A different nonce is a free slot.
        assert!(matches!(
            store.reserve_nonce(addr, 6, h2, lease).await.unwrap(),
            NonceReservation::Won { .. }
        ));

        // Lease-EXPIRY takeover (crash recovery): valid lease dedups; once expired
        // the SAME tx takes over.
        let addr2 = "0x00000000000000000000000000000000000000ee";
        let h3 = TxHash::from([0x33u8; 32]);
        assert!(matches!(
            store.reserve_nonce(addr2, 0, h3, lease).await.unwrap(),
            NonceReservation::Won { .. }
        ));
        assert_eq!(
            store.reserve_nonce(addr2, 0, h3, lease).await.unwrap(),
            NonceReservation::OwnedBySame
        );
        concrete.test_expire_reservation_lease(addr2, 0);
        assert!(
            matches!(
                store.reserve_nonce(addr2, 0, h3, lease).await.unwrap(),
                NonceReservation::Won { .. }
            ),
            "an EXPIRED lease must be taken over by the same tx (crash recovery)"
        );
    }

    /// BLOCKER 1 — end-to-end: a submission whose (signer, nonce) slot was already
    /// reserved by a DIFFERENT tx (another replica won) is REJECTED and does NOT
    /// execute — no nonce advance, no receipt.
    #[tokio::test]
    async fn blocker_1_service_rejects_when_slot_reserved_by_another() {
        let service = create_test_service();
        let store = service.store.clone();
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xE1u8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata); // nonce 0
        let signer_str = format!("{signer:#x}");
        // Simulate another replica having won the (signer, 0) slot with a different tx.
        let other = TxHash::from([0xEEu8; 32]);
        assert!(matches!(
            store
                .reserve_nonce(&signer_str, 0, other, reservation_lease())
                .await
                .unwrap(),
            crate::store::NonceReservation::Won { .. }
        ));

        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("must be rejected — the (signer, nonce) slot is reserved by another tx");
        assert!(
            err.to_string().contains("reserved by a different tx"),
            "unexpected: {err:#}"
        );
        // Did NOT execute: nonce not advanced.
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);
    }

    /// Crash outcomes are ambiguous: a nonce slot is permanently bound to the
    /// first signed transaction, even after failure or lease expiry. Only that
    /// exact hash may recover the durable intent.
    #[tokio::test]
    async fn blocker_a_different_tx_never_takes_over_ambiguous_slot() {
        use crate::store::NonceReservation;
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let addr = "0x00000000000000000000000000000000000000aa";
        let ha = TxHash::from([0xa1u8; 32]);
        let hb = TxHash::from([0xb2u8; 32]);
        let lease = std::time::Duration::from_secs(90);

        let NonceReservation::Won { fence } =
            store.reserve_nonce(addr, 1, ha, lease).await.unwrap()
        else {
            panic!("fresh must win");
        };
        store
            .release_reservation(addr, 1, ha, fence, false)
            .await
            .unwrap();
        assert_eq!(
            store.reserve_nonce(addr, 1, hb, lease).await.unwrap(),
            NonceReservation::HeldByOther(ha),
            "failure cannot authorize a replacement after an ambiguous external outcome"
        );
        assert!(matches!(
            store.reserve_nonce(addr, 1, ha, lease).await.unwrap(),
            NonceReservation::Won { .. }
        ));

        assert!(matches!(
            store.reserve_nonce(addr, 2, ha, lease).await.unwrap(),
            NonceReservation::Won { .. }
        ));
        concrete.test_expire_reservation_lease(addr, 2);
        assert_eq!(
            store.reserve_nonce(addr, 2, hb, lease).await.unwrap(),
            NonceReservation::HeldByOther(ha),
            "expiry is crash recovery for the same hash, never replacement permission"
        );
        assert!(matches!(
            store.reserve_nonce(addr, 2, ha, lease).await.unwrap(),
            NonceReservation::Won { .. }
        ));
    }

    /// Writer admission persists the recoverable envelope before nonce CAS and enqueue.
    /// A CAS error leaves a durable, unlinked intent and no live in-memory job.
    #[tokio::test]
    async fn blocker_b_writer_durable_intent_precedes_nonce_cas() {
        // Happy path: exactly-once advance + job enqueued.
        let mut service = create_test_service();
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        let handle = std::sync::Arc::new(handle);
        service.writer_handle = Some(handle.clone());
        let store = service.store.clone();

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xB1u8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);
        let signer_str = format!("{signer:#x}");
        let tx_hash = service_send_raw_txn(service, input_hex)
            .await
            .expect("enqueue must succeed");
        assert_eq!(
            store.nonce_get(&signer_str).await.unwrap(),
            1,
            "a won CAS advances the nonce exactly once"
        );
        assert!(handle.is_inflight(&tx_hash), "the job must be enqueued");
        let _ = shutdown.send(());

        // CAS-FAILURE path: no enqueued job, nonce unchanged.
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let mut service2 = crate::test_helpers::create_test_service_with_store(concrete.clone());
        let (handle2, shutdown2) = crate::writer_worker::WriterWorker::spawn(
            service2.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        let handle2 = std::sync::Arc::new(handle2);
        service2.writer_handle = Some(handle2.clone());

        // Arm a one-shot CAS store failure.
        concrete.test_fail_next_nonce_cas();
        let calldata2 = insertGlobalExitRootCall {
            root: FixedBytes::from([0xB2u8; 32]),
        }
        .abi_encode();
        let (input_hex2, signer2) = encode_legacy_tx(calldata2);
        let raw = hex_decode_prefixed(&input_hex2).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
        let durable_hash = unwrap_txn_envelope(envelope).unwrap().hash;
        let err = service_send_raw_txn(service2, input_hex2)
            .await
            .expect_err("a nonce-CAS store failure must abort admission");
        assert!(
            err.to_string().contains("simulated nonce_advance_cas"),
            "unexpected: {err:#}"
        );
        assert_eq!(
            concrete.nonce_get(&format!("{signer2:#x}")).await.unwrap(),
            0,
            "the nonce must be UNCHANGED after a pre-enqueue CAS failure"
        );
        assert_eq!(
            handle2.inflight_len(),
            0,
            "NO job may be enqueued when the CAS failed before enqueue"
        );
        let intent = concrete.txn_get(durable_hash).await.unwrap().unwrap();
        assert!(
            intent.result.is_none(),
            "durable admission remains retryable"
        );
        assert!(
            concrete
                .get_note_link_for_tx(&format!("{durable_hash:#x}"))
                .await
                .unwrap()
                .is_none(),
            "pre-dispatch durable intent is deliberately unlinked"
        );
        let _ = shutdown2.send(());
    }

    /// A crash after nonce CAS but before mpsc enqueue is recovered from the
    /// durable unlinked envelope without advancing twice.
    #[tokio::test]
    async fn writer_crash_after_nonce_cas_resumes_same_durable_hash() {
        use crate::store::NonceReservation;
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let mut service = crate::test_helpers::create_test_service_with_store(concrete.clone());
        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from([0xc7u8; 32]),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);
        let raw = hex_decode_prefixed(&input_hex).unwrap();
        let envelope = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
        let tx_hash = unwrap_txn_envelope(envelope.clone()).unwrap().hash;
        let signer_str = format!("{signer:#x}");
        let NonceReservation::Won { .. } = concrete
            .reserve_nonce(&signer_str, 0, tx_hash, reservation_lease())
            .await
            .unwrap()
        else {
            panic!("fresh reservation must win");
        };
        durably_admit_and_advance_nonce(&service, tx_hash, &envelope, signer, &signer_str, 0)
            .await
            .unwrap();
        assert_eq!(concrete.nonce_get(&signer_str).await.unwrap(), 1);
        assert!(
            concrete
                .txn_get(tx_hash)
                .await
                .unwrap()
                .unwrap()
                .result
                .is_none()
        );

        // Simulate process death: the durable row and nonce survived, the mpsc did
        // not, and the reservation heartbeat stopped until its lease expired.
        concrete.test_expire_reservation_lease(&signer_str, 0);
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        let handle = std::sync::Arc::new(handle);
        service.writer_handle = Some(handle.clone());
        let retried = service_send_raw_txn(service, input_hex).await.unwrap();
        assert_eq!(retried, tx_hash);
        assert_eq!(concrete.nonce_get(&signer_str).await.unwrap(), 1);
        assert!(handle.is_inflight(&tx_hash));
        let _ = shutdown.send(());
    }

    /// A stale claim owner cannot seal or release a successor after lease reclaim.
    #[tokio::test]
    async fn claim_reclaim_is_fenced_through_external_submission_boundary() {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let gi = U256::from(0xf3ceu64);
        let tx_a = TxHash::from([0xaau8; 32]);
        let tx_b = TxHash::from([0xbbu8; 32]);
        let ttl = std::time::Duration::from_secs(90);

        let a = store
            .try_claim_fenced(gi, tx_a, ttl)
            .await
            .unwrap()
            .unwrap();
        concrete.test_backdate_claim(gi, ttl + std::time::Duration::from_secs(1));
        let b = store
            .try_reclaim_claim_fenced(gi, tx_b, ttl)
            .await
            .unwrap()
            .unwrap();
        assert!(b.fence > a.fence);
        assert!(
            !store
                .prepare_claim_submission_fenced(
                    gi,
                    tx_a,
                    a.fence,
                    tx_a,
                    "stale-note",
                    "stale-id",
                    100,
                )
                .await
                .unwrap()
        );
        assert!(!store.unclaim_fenced(&gi, tx_a, a.fence).await.unwrap());
        assert!(
            store
                .prepare_claim_submission_fenced(
                    gi,
                    tx_b,
                    b.fence,
                    tx_b,
                    "winner-note",
                    "winner-id",
                    100,
                )
                .await
                .unwrap()
        );
        assert!(!store.unclaim_fenced(&gi, tx_b, b.fence).await.unwrap());
        assert_eq!(
            store
                .get_note_link_for_tx(&format!("{tx_b:#x}"))
                .await
                .unwrap()
                .as_deref(),
            Some("winner-note")
        );
        assert!(
            store
                .try_reclaim_claim_fenced(gi, tx_a, ttl)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// BLOCKER 2 — the reclaim-SUCCESS window. A gi whose lock is EXPIRED but which
    /// LANDED (its ClaimEvent committed after the first read) must classify `Landed`,
    /// NOT `Acquired` (which would duplicate-publish). Driven deterministically: the
    /// first `has_claim_event` read misses (arming the landing), the expired lock is
    /// reclaimed, and the re-read AFTER the successful reclaim observes the landing.
    #[tokio::test]
    async fn blocker_2_landed_in_reclaim_success_window_classifies_landed() {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let gi = U256::from(0x55b2u64);
        store.try_claim(gi).await.unwrap();
        // Lock is EXPIRED (backdated past the TTL) so try_reclaim_expired SUCCEEDS.
        concrete.test_backdate_claim(
            gi,
            claim_resubmit_ttl() + std::time::Duration::from_secs(10),
        );
        // The claim LANDS on the first has_claim_event miss (so the re-read sees it).
        concrete.test_land_gi_after_next_has_claim_miss(gi.to_be_bytes::<32>());

        let outcome = acquire_claim_lock(&store, gi, TxHash::ZERO, claim_resubmit_ttl())
            .await
            .expect("classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Landed,
            "a gi that landed but has an expired lock must classify Landed via the \
             reclaim-success re-read, never Acquired (no duplicate publish)"
        );
    }

    /// BLOCKER 3 — writer mode: an already-LANDED gi with an UNOBSERVED GER must
    /// accept-and-revert on the REQUEST path (before C6/enqueue), not get a C6 GER
    /// rejection with no nonce consumption.
    #[tokio::test]
    async fn blocker_3_writer_mode_landed_before_c6() {
        let mut service = create_test_service();
        let store = service.store.clone();
        let (handle, shutdown) = crate::writer_worker::WriterWorker::spawn(
            service.clone(),
            64,
            std::time::Duration::from_secs(60),
        );
        service.writer_handle = Some(std::sync::Arc::new(handle));
        // gi LANDED; GER deliberately NOT seeded → C6 would reject "not observed yet".
        let gi = U256::from(0x55c3u64);
        land_claim_for(&store, gi).await;

        let key_b = alloy::signers::local::PrivateKeySigner::random();
        let addr_b = key_b.address();
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let accepted = service_send_raw_txn(service.clone(), input_b)
            .await
            .expect("writer-mode landed gi must accept-and-revert before C6, not reject on GER");
        assert_eq!(accepted, tx_b);
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            1,
            "nonce consumed via accept-and-revert (not a C6 hard-reject)"
        );
        let (result, _blk) = store.txn_receipt(tx_b).await.unwrap().expect("receipt");
        assert!(result.is_err(), "reverted (status 0x0)");
        let _ = shutdown.send(());
    }

    /// BLOCKER 4 — the conditional reverted-receipt write must NEVER overwrite a REAL
    /// receipt (pending or successful) with status 0. Cross-replica: one path lands
    /// the real claim; another later classifies Landed and calls accept-and-revert on
    /// the same hash — the real outcome must survive.
    #[tokio::test]
    async fn blocker_4_reverted_receipt_does_not_overwrite_real_receipt() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let signer = Address::from([0x44u8; 20]);
        let signer_str = format!("{signer:#x}");
        let envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            alloy::primitives::Signature::test_signature(),
            TxHash::default(),
        ));
        let entry = || crate::store::TxnEntry {
            id: None,
            envelope: envelope.clone(),
            signer,
            expires_at: None,
            logs: vec![],
        };

        // (a) SUCCESS receipt must not be overwritten to failed.
        let tx_ok = TxHash::from([0x41u8; 32]);
        store.txn_begin(tx_ok, entry()).await.unwrap();
        store.txn_commit(tx_ok, Ok(()), 5, [0u8; 32]).await.unwrap();
        store
            .commit_reverted_receipt_and_advance_nonce(
                tx_ok,
                entry(),
                "revert".into(),
                9,
                [0u8; 32],
                &signer_str,
                0,
            )
            .await
            .unwrap();
        let (r_ok, _) = store.txn_receipt(tx_ok).await.unwrap().expect("receipt");
        assert!(
            r_ok.is_ok(),
            "a REAL success receipt must NOT be overwritten to failed"
        );

        // (b) PENDING receipt (begun, awaiting the projector) must not be finalised to failed.
        let tx_pending = TxHash::from([0x42u8; 32]);
        store.txn_begin(tx_pending, entry()).await.unwrap();
        store
            .record_tx_note_link(&format!("{tx_pending:#x}"), "real-note")
            .await
            .unwrap();
        store
            .commit_reverted_receipt_and_advance_nonce(
                tx_pending,
                entry(),
                "revert".into(),
                9,
                [0u8; 32],
                &signer_str,
                1,
            )
            .await
            .unwrap();
        assert!(
            store.txn_receipt(tx_pending).await.unwrap().is_none(),
            "a REAL pending receipt must stay pending (null), not be finalised to failed"
        );

        // (c) An unlinked pending row is only a durable pre-admission intent and
        // must be finalised to failed when the landed-claim race is detected.
        let tx_intent = TxHash::from([0x43u8; 32]);
        store.txn_begin_if_absent(tx_intent, entry()).await.unwrap();
        store
            .commit_reverted_receipt_and_advance_nonce(
                tx_intent,
                entry(),
                "revert".into(),
                9,
                [0u8; 32],
                &signer_str,
                2,
            )
            .await
            .unwrap();
        let (r_intent, _) = store
            .txn_receipt(tx_intent)
            .await
            .unwrap()
            .expect("receipt");
        assert!(
            r_intent.is_err(),
            "unlinked durable intent must become status 0x0"
        );

        // (d) ABSENT hash → the reverted receipt IS written (status 0x0).
        let tx_new = TxHash::from([0x44u8; 32]);
        store
            .commit_reverted_receipt_and_advance_nonce(
                tx_new,
                entry(),
                "revert".into(),
                9,
                [0u8; 32],
                &signer_str,
                3,
            )
            .await
            .unwrap();
        let (r_new, _) = store.txn_receipt(tx_new).await.unwrap().expect("receipt");
        assert!(
            r_new.is_err(),
            "an absent hash gets the reverted (status 0x0) receipt"
        );
    }

    /// No double-submit race: a record YOUNGER than the TTL (a submission genuinely in
    /// flight, no ClaimEvent yet) classifies as `InFlight` — the caller hard-rejects.
    #[tokio::test]
    async fn in_flight_claim_within_ttl_still_rejected() {
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let gi = U256::from(88u64);
        store.try_claim(gi).await.unwrap();

        let outcome = acquire_claim_lock(
            &store,
            gi,
            TxHash::ZERO,
            std::time::Duration::from_secs(3600),
        )
        .await
        .expect("in-flight classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::InFlight,
            "an in-flight record (younger than the TTL, no ClaimEvent) must classify InFlight"
        );
        assert!(store.is_claimed(&gi).await.unwrap(), "lock stays held");
    }

    #[tokio::test]
    async fn invalid_claim_does_not_bind_nonce_reservation() {
        let service = create_test_service();
        let store = service.store.clone();
        let key = alloy::signers::local::PrivateKeySigner::random();
        let gi = U256::from(0xd001u64);

        let mut bad =
            claimAssetCall::abi_decode(&claim_calldata(gi, resolvable_dest(), U256::ZERO)).unwrap();
        bad.destinationNetwork = service.network_id + 1;
        let signer_key = format!("{:#x}", key.address());
        let nonce = store.nonce_get(&signer_key).await.unwrap();
        let (bad_raw, bad_hash) = encode_tx_signed_with_nonce(&key, bad.abi_encode(), nonce);
        let err = service_send_raw_txn(service.clone(), bad_raw)
            .await
            .expect_err("wrong destination network must be rejected");
        assert!(err.to_string().contains("destinationNetwork"));
        assert!(store.txn_get(bad_hash).await.unwrap().is_none());

        // A corrected, newly-signed hash at the same nonce is still admissible.
        let (good_raw, good_hash) = encode_tx_signed_with_nonce(
            &key,
            claim_calldata(gi, resolvable_dest(), U256::ZERO),
            store.nonce_get(&signer_key).await.unwrap(),
        );
        assert_eq!(
            service_send_raw_txn(service, good_raw).await.unwrap(),
            good_hash
        );
        assert_eq!(
            store
                .nonce_get(&format!("{:#x}", key.address()))
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn durable_pending_floor_survives_restart_and_blocks_nonce_skip() {
        let service = create_test_service();
        let store = service.store.clone();
        let key = alloy::signers::local::PrivateKeySigner::random();
        let signer = key.address();
        let signer_str = format!("{signer:#x}");
        let call0 = insertGlobalExitRootCall {
            root: FixedBytes::from([0xd0; 32]),
        }
        .abi_encode();
        let nonce0 = store.nonce_get(&signer_str).await.unwrap();
        let (raw0, hash0) = encode_tx_signed_with_nonce(&key, call0, nonce0);
        let payload0 = hex_decode_prefixed(&raw0).unwrap();
        let envelope0 = TxEnvelope::decode_2718(&mut payload0.as_slice()).unwrap();

        // Model a process kill after durable intent + nonce CAS, with no DashMap.
        store
            .txn_begin_if_absent(
                hash0,
                TxnEntry {
                    id: None,
                    envelope: envelope0,
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        assert!(store.nonce_advance_cas(&signer_str, nonce0).await.unwrap());

        let frontier = store.pending_nonce_frontier(&signer_str).await.unwrap();
        assert_eq!(frontier.lowest_pending, Some(0));
        assert_eq!(frontier.lowest_unlinked, Some(0));
        assert_eq!(
            crate::service::select_transaction_count(1, "latest", frontier),
            0
        );
        assert_eq!(
            crate::service::select_transaction_count(1, "pending", frontier),
            1
        );

        let call1 = insertGlobalExitRootCall {
            root: FixedBytes::from([0xd1; 32]),
        }
        .abi_encode();
        let nonce1 = store.nonce_get(&signer_str).await.unwrap();
        let (raw1, _) = encode_tx_signed_with_nonce(&key, call1, nonce1);
        let err = service_send_raw_txn(service, raw1)
            .await
            .expect_err("nonce 1 must not skip recoverable durable nonce 0");
        assert!(err.to_string().contains("lower nonce 0"));
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn reservation_heartbeat_is_aborted_on_owner_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ticks = std::sync::Arc::new(AtomicUsize::new(0));
        let task_ticks = ticks.clone();
        let guard = AbortTaskOnDrop::new(tokio::spawn(async move {
            loop {
                task_ticks.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let after_abort = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), after_abort);
    }

    #[tokio::test]
    async fn same_hash_claim_lease_is_immediately_reclaimable() {
        let concrete = std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let store: std::sync::Arc<dyn crate::store::Store> = concrete.clone();
        let gi = U256::from(0xd004u64);
        let owner = TxHash::from([0xd4; 32]);
        let ttl = std::time::Duration::from_secs(120);
        store
            .try_claim_fenced(gi, owner, ttl)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            acquire_claim_lock(&store, gi, owner, ttl).await.unwrap(),
            ClaimLockOutcome::Acquired { .. }
        ));
    }
}
