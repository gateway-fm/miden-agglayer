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
    match acquire_claim_lock(&service.store, params.globalIndex, claim_resubmit_ttl()).await? {
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
        // ClaimEvent yet, within TTL). Hard-reject — must not double-publish. In
        // sync mode this returns Err WITHOUT the caller advancing the nonce (a
        // genuine retry, nonce not consumed); once the in-flight claim LANDS, the
        // retry re-enters and takes the `Landed` accept-and-revert arm above.
        ClaimLockOutcome::InFlight => {
            anyhow::bail!(
                "claim already submitted for global_index {}",
                params.globalIndex
            );
        }
        // Fresh lock (or an orphaned record superseded) — proceed to publish.
        ClaimLockOutcome::Acquired => {}
    }

    // R9 — we now HOLD the per-globalIndex lock. Install a RAII drop guard so a
    // cancelled / panicked / disconnected future releases it; every early return
    // below (RD-860 swallow, C6 reject, publish failure) releases it explicitly.
    let guard = ClaimGuard::new(service.store.clone(), params.globalIndex);

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

    // C6 — pre-publish GER publication gate. In writer mode the SAME gate already
    // ran on the request path before `try_enqueue` (PR #127 review point 3); this
    // second run is cheap defense-in-depth. On rejection RELEASE the lock (cheap
    // retryable surface — the claim didn't publish) and return the retryable error.
    // See `ensure_claim_ger_published` for the full rationale.
    if let Err(err) = ensure_claim_ger_published(&service.store, &params).await {
        guard.release_explicitly().await;
        return Err(err);
    }

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

/// C6 — the pre-admission GER publication gate (Cantina #21 / PR #127 review).
///
/// The CLAIM note's leaf proof is internally consistent (built from L1
/// calldata), but on-chain the bridge MASM verifies it against the GER
/// currently stored in the bridge account. Mirroring the real EVM bridge
/// (`AgglayerBridge._verifyLeaf` reads `globalExitRootMap[combinedGER]` once
/// and reverts `GlobalExitRootInvalid()` when zero — it never waits), a claim
/// whose GER the proxy has not yet PUBLISHED is rejected fail-fast with a
/// retryable error: no nonce is consumed, no globalIndex lock is taken, no
/// receipt or queued job is created, and the SAME signed transaction can be
/// re-submitted after GER publication.
///
/// This gate MUST run before every enqueue path, nonce increment, try_claim,
/// txn_begin, or receipt creation:
///   - sync path: `worker_handle_claim_asset` calls it before
///     `acquire_claim_lock` (nonce advances only after the dispatch returns
///     Ok);
///   - writer path: `service_send_raw_txn` calls it on the REQUEST thread
///     before `try_enqueue` (which would otherwise consume the nonce and
///     admit the hash into the inflight dedup cache).
///
/// `is_ger_injected` rather than `has_seen_ger`: the L1InfoTreeIndexer
/// pre-populates ger_entries rows for L1 pairs it has indexed but that
/// haven't yet been injected/published on L2. C6 requires the GER event to be
/// published on L2, not merely indexed; the `is_injected` flag captures that
/// intent (it also holds while the #30 visibility barrier keeps the projector
/// from publishing the consumption event). The final race/security gate stays
/// on-chain: the CLAIM's FPI runs the MASM `assert_valid_ger` against the
/// authoritative bridge-account storage and fails closed with
/// `ERR_GER_NOT_FOUND` — C6 is scheduling/visibility policy, MASM is the hard
/// safety boundary.
pub(crate) async fn ensure_claim_ger_published(
    store: &std::sync::Arc<dyn crate::store::Store>,
    params: &claimAssetCall,
) -> anyhow::Result<()> {
    let combined = crate::ger::combined_ger(&params.mainnetExitRoot.0, &params.rollupExitRoot.0);
    if !store.is_ger_injected(&combined).await? {
        ::metrics::counter!("rpc_claim_ger_not_seen_total").increment(1);
        anyhow::bail!(
            "claim references a GER that aggkit has not observed yet \
             (mainnet={}, rollup={}); retry after the GER is injected. C6.",
            ::hex::encode(params.mainnetExitRoot.0),
            ::hex::encode(params.rollupExitRoot.0)
        );
    }
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

/// Typed, AUTHORITATIVE outcome of [`acquire_claim_lock`]. Landed detection and the
/// #55 accept-and-revert decision are ONE step here (no separate pre-check→lock
/// window), so there is no interleaving in which a claim for an already-landed
/// globalIndex is hard-rejected — the exact nonce-desync wedge #55 fixes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaimLockOutcome {
    /// The submission lock is now held by THIS caller (fresh index, or an orphaned
    /// record superseded — SOAK FINDING #1). Proceed to publish; the caller MUST
    /// arrange `unclaim` on any later failure (via `ClaimGuard`).
    Acquired,
    /// A genuine concurrent submission for the same gi is in flight (locked, no
    /// ClaimEvent yet, record younger than `ttl`). The caller hard-rejects
    /// ("already submitted") — no double publish, and no nonce is consumed.
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
///   1. `try_claim` succeeds (fresh) + no ClaimEvent → `Acquired` (normal path).
///   2. `try_claim` succeeds (fresh) + a ClaimEvent already exists (e.g. a restore
///      wrote the event without a submission lock) → release the spurious lock and
///      return `Landed` — never double-publish onto a landed gi.
///   3. `try_claim` fails + a ClaimEvent exists → `Landed`. This is the closed
///      TOCTOU: even if the claim LANDED after some earlier read, the landed
///      classification is made here, at the lock decision, and routes to
///      accept-and-revert rather than a hard reject.
///   4. `try_claim` fails + no ClaimEvent + record younger than `ttl` → `InFlight`.
///   5. `try_claim` fails + no ClaimEvent + `ttl` expired → ORPHANED: atomically
///      supersede (`Store::try_reclaim_expired` — single UPDATE, one winner under
///      concurrency), warn + `claim_resubmission_recovered_total`, return `Acquired`.
pub(crate) async fn acquire_claim_lock(
    store: &std::sync::Arc<dyn crate::store::Store>,
    global_index: alloy::primitives::U256,
    ttl: std::time::Duration,
) -> anyhow::Result<ClaimLockOutcome> {
    let gi_bytes: [u8; 32] = global_index.to_be_bytes::<32>();
    match store.try_claim(global_index).await {
        Ok(()) => {
            // Fresh lock acquired. Guard the rare "ClaimEvent exists but no lock"
            // (e.g. a restore populated the event without a submission lock): if a
            // ClaimEvent already exists, this is LANDED — release the lock we just
            // took and route to accept-and-revert rather than double-publishing.
            if store.has_claim_event_for_global_index(&gi_bytes).await? {
                store.unclaim(&global_index).await?;
                return Ok(ClaimLockOutcome::Landed);
            }
            Ok(ClaimLockOutcome::Acquired)
        }
        Err(_rejected) => {
            // LANDED is authoritative and checked AT the lock decision — there is
            // no separate pre-check window in which a landed gi could be routed to
            // a hard reject.
            if store.has_claim_event_for_global_index(&gi_bytes).await? {
                return Ok(ClaimLockOutcome::Landed);
            }
            // Not landed at the first read. Atomically supersede IFF the record has
            // out-lived the in-flight TTL; a fresher record means a submission is
            // genuinely in flight.
            if !store.try_reclaim_expired(global_index, ttl).await? {
                // BLOCKER B — RE-READ the authoritative landed state AFTER failing
                // to reclaim. The original claim may have committed its ClaimEvent
                // in the window between the first `has_claim_event` read and the
                // reclaim attempt; without this re-read that just-landed gi would
                // classify `InFlight` and be hard-rejected WITHOUT consuming the
                // nonce — the exact wedge. A gi that landed at ANY point up to here
                // classifies `Landed` (→ accept-and-revert), never `InFlight`.
                if store.has_claim_event_for_global_index(&gi_bytes).await? {
                    return Ok(ClaimLockOutcome::Landed);
                }
                return Ok(ClaimLockOutcome::InFlight);
            }
            // BLOCKER 2 — the reclaim SUCCEEDED (an expired lock was superseded to
            // us). But a claim that landed AFTER the first read yet still holds an
            // expired-looking lock (a slow real claim: publish took > TTL, then its
            // ClaimEvent committed) would be reclaimed here and misclassified
            // `Acquired` → DUPLICATE PUBLISH. So RE-READ landed after the successful
            // reclaim: if it landed, RELEASE the lock we just superseded and return
            // `Landed` (→ accept-and-revert), never Acquired. A gi that landed at any
            // point up to here is Landed. (Residual: an event landing strictly after
            // this final read is caught only by the worker's own re-classification —
            // see the report's single-store-transaction residual.)
            if store.has_claim_event_for_global_index(&gi_bytes).await? {
                store.unclaim(&global_index).await?;
                return Ok(ClaimLockOutcome::Landed);
            }
            ::metrics::counter!("claim_resubmission_recovered_total").increment(1);
            tracing::warn!(
                global_index = %global_index,
                ttl_secs = ttl.as_secs(),
                "orphaned claim submission record for global_index {global_index} (submitted but \
                 never landed — likely a crash mid-flight); accepting resubmission"
            );
            Ok(ClaimLockOutcome::Acquired)
        }
    }
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
        // BLOCKER C/D (#55 review) — crash-gap nonce REPAIR via store-level CAS.
        // On the sync accept path the durable receipt write and the nonce advance
        // are SEPARATE steps; a crash / store error BETWEEN them leaves this tx
        // KNOWN (receipt persisted) while the signer's expected nonce stays STALE
        // at this tx's nonce. Without repair this rebroadcast is served as success
        // forever while the nonce never advances, and the signer's NEXT tx
        // (nonce+1) fails the R4 gate — the exact wedge #55 fixes, reintroduced by
        // a crash in the commit gap. `repair_commit_gap_nonce` CAS-advances the
        // nonce iff it is still stuck at tx_nonce (idempotent; cross-replica-safe;
        // extracted so both stores regression-test it directly).
        repair_commit_gap_nonce(&service, &signer_str, tx_nonce).await?;
        return Ok(txn_hash);
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

        // R4 — nonce validation. Pre-fix the proxy advanced its tracked nonce
        // only on success and never compared the incoming `tx.nonce` against
        // the expected next value. That allowed replay and skipped sequencing.
        let expected_nonce = service.store.nonce_get(&signer_str).await?;
        let can_wait_for_future_nonce = service.enable_writer_worker
            && tx_nonce > expected_nonce
            && future_nonce_wait_started.elapsed() < future_nonce_wait_max;
        let nonce_action = if tx_nonce == expected_nonce {
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
                "nonce_matches": tx_nonce == expected_nonce,
                "action": nonce_action,
                "future_nonce_wait_ms": future_nonce_wait_started.elapsed().as_millis(),
                "writer_enabled": service.enable_writer_worker,
                "writer_handle_present": service.writer_handle.is_some(),
            })
        );

        if tx_nonce == expected_nonce {
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

    // R2 — signer allow-list. Without this, anyone who can hit the JSON-RPC port
    // can sign and submit `claimAsset` / `insertGlobalExitRoot` / `updateExitRoot`
    // calldata. The proxy then runs Miden tx work on the service account's behalf
    // (auto-creates faucets, advances LET, marks GERs injected), letting an
    // attacker burn fees, poison registries, or feed fabricated GERs to
    // bridge-service. Reject any signer not in the configured allow-list.
    // Audit C2 — `None` is fail-closed (no signer accepted); legacy open mode
    // requires the explicit `allow_any_signer` opt-in.
    if !service.allow_any_signer && !is_signer_allowed(service.allowed_signers.as_deref(), &signer)
    {
        ::metrics::counter!("rpc_unauthorized_signer_total").increment(1);
        anyhow::bail!(
            "signer {signer:#x} is not on the allow-list; configure --allowed-signers (or ALLOWED_SIGNERS), \
             or set --insecure-allow-any-signer to explicitly opt into open mode"
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

    // ── #55 BLOCKER 1 — atomic (signer, nonce) reservation ──────────────
    //
    // Reserve the (signer, nonce) slot ATOMICALLY, BEFORE any queue/dispatch/receipt
    // side effect. Two replicas on a shared PostgreSQL that each passed their
    // process-local R4 for two DIFFERENT txs at the same (signer, nonce) both reach
    // here; the store reservation lets exactly ONE win. The loser NEVER executes —
    // no enqueue, no dispatch, no receipt. (This runs AFTER the R4 per-signer lock,
    // which still serialises intra-process and provides the future-nonce wait; the
    // reservation is the cross-replica uniqueness guarantee the process lock can't.)
    match service
        .store
        .reserve_nonce(&signer_str, tx_nonce, txn_hash)
        .await?
    {
        crate::store::NonceReservation::Won => {}
        crate::store::NonceReservation::HeldBy(h) if h == txn_hash => {
            // This exact tx already reserved the slot (its own earlier attempt — e.g.
            // a retry after a RETRYABLE C6/GER rejection that left the reservation but
            // no receipt — or an idempotent rebroadcast). PROCEED so a retryable
            // failure can re-run admission and eventually execute. The upstream
            // tx-hash dedup already short-circuited a tx that truly completed
            // (receipt written / in-flight), so reaching here means re-execution is
            // safe (and idempotent: a landed gi accept-reverts, a GER re-inserts).
            tracing::debug!(
                target: "rpc::reserve",
                %txn_hash, nonce = tx_nonce,
                "reservation held by this same tx — proceeding (retry/re-execute)"
            );
        }
        crate::store::NonceReservation::HeldBy(other) => {
            // A DIFFERENT tx already reserved this (signer, nonce) slot — this
            // submission LOST and must NOT execute. Reject; the winner advances the
            // nonce, so this loser's subsequent retries fail R4 as stale (nonce too
            // low) and aggkit drops it — mirroring geth dropping the losing tx at an
            // already-consumed nonce.
            ::metrics::counter!("rpc_nonce_reservation_lost_total").increment(1);
            anyhow::bail!(
                "nonce {tx_nonce} for {signer_str} is already reserved by a different tx \
                 {other:#x} (concurrent submission at the same nonce slot); this tx must not \
                 execute"
            );
        }
    }

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

        // BLOCKER 3 — landed classification BEFORE C6 in WRITER mode too. The sync
        // worker already classifies landed before RD-860/C6, but the REQUEST path
        // ran C6 (`ensure_claim_ger_published`) before enqueue: an already-LANDED gi
        // with a resolvable destination and an altered/unobserved GER would get a C6
        // RPC error with NO nonce consumption — the wedge. So route an already-landed
        // claim to accept-and-revert HERE (atomic reverted receipt + nonce, synchronous,
        // no enqueue), regardless of GER state. The worker's `acquire_claim_lock` still
        // catches a claim that LANDS after this check (its `Landed` arm accept-reverts).
        if let crate::writer_worker::DecodedWriteCall::Claim { params } = &decoded
            && service
                .store
                .has_claim_event_for_global_index(&params.globalIndex.to_be_bytes::<32>())
                .await?
        {
            accept_and_revert_landed_claim(
                &service,
                params,
                txn_hash,
                txn_envelope,
                signer,
                &signer_str,
                tx_nonce,
            )
            .await?;
            return Ok(txn_hash);
        }

        // C6 on the REQUEST path (PR #127 review point 3). Pre-fix, with the
        // writer enabled the gate only ran inside the worker — AFTER
        // `try_enqueue` had consumed the nonce and admitted the tx hash into
        // the inflight dedup cache, so a GER-not-yet-published claim burned a
        // sequence slot and its re-broadcast short-circuited as a "known"
        // hash. Run the gate here, before any side-effect, so pre-admission
        // failure leaves NOTHING behind: no tx hash/receipt, no nonce, no
        // globalIndex lock, no queued job — the same signed transaction (same
        // nonce) is accepted verbatim once the GER is published.
        //
        // Only claims that would actually reach C6 in the worker are gated,
        // mirroring `worker_handle_claim_asset`'s short-circuit precedence:
        // wrong-network claims hard-fail in the worker regardless of GER,
        // zero-amount claims are swallowed as immediate successes, and
        // unresolvable destinations are swallowed permanently (RD-860 runs
        // BEFORE C6 because that state is permanent while a missing GER is
        // transient).
        if let crate::writer_worker::DecodedWriteCall::Claim { params } = &decoded
            && params.destinationNetwork == service.network_id
            && !params.amount.is_zero()
            && crate::address_mapper::resolve_address(
                &*service.store,
                params.destinationAddress,
                &service.accounts.0,
            )
            .await
            .is_ok()
        {
            ensure_claim_ger_published(&service.store, params).await?;
        }

        let job = decoded.into_job(txn_envelope, signer, txn_hash);
        match handle.try_enqueue(job) {
            Ok(()) => {
                // BLOCKER D — advance via store-level CAS (advance only WHERE nonce
                // == tx_nonce), so two replicas on a shared PostgreSQL can't both
                // advance from the same expected value (N→N+2). A `false` return
                // means another replica (or the worker's own accept-and-revert)
                // already advanced it — the nonce ends advanced either way.
                let _ = service
                    .store
                    .nonce_advance_cas(&signer_str, tx_nonce)
                    .await?;
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
        // BLOCKER D — advance via store-level CAS (advance only WHERE nonce ==
        // tx_nonce), cross-replica-safe. A `false` return means the advance already
        // happened (the accept-and-revert path advances the nonce atomically with
        // its receipt, or a concurrent replica won) — the nonce ends advanced.
        let _ = service
            .store
            .nonce_advance_cas(&signer_str, tx_nonce)
            .await?;
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

    /// PR #127 review point 3 — writer mode. Pre-fix, with
    /// `enable_writer_worker = true` the C6 gate only ran inside the worker,
    /// AFTER `try_enqueue` had consumed the nonce and admitted the tx hash
    /// into the inflight dedup cache. The gate must run on the REQUEST path:
    /// a claim whose GER is unpublished is rejected with no nonce consumed,
    /// no globalIndex lock, no receipt, and no queued job — and the SAME
    /// signed transaction (same nonce) is accepted once the GER is published.
    #[tokio::test]
    async fn c6_writer_mode_missing_ger_rejected_before_enqueue_then_retryable() {
        let mut service = create_test_service();
        service.enable_writer_worker = true;
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

    /// Writer-mode C6 precedence mirror: claims the worker would swallow
    /// WITHOUT reaching C6 (zero-amount genesis claims) must NOT be gated on
    /// GER publication at admission — the gate only covers claims that would
    /// actually reach `ensure_claim_ger_published` in the worker.
    #[tokio::test]
    async fn c6_writer_mode_zero_amount_claim_not_ger_gated() {
        let mut service = create_test_service();
        service.enable_writer_worker = true;
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
        service.enable_writer_worker = true;
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
        let outcome = acquire_claim_lock(&store, gi, std::time::Duration::ZERO)
            .await
            .expect("orphaned record classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::Acquired,
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

        let outcome = acquire_claim_lock(&store, gi, std::time::Duration::ZERO)
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
    /// has a submission genuinely in flight for gi=X (lock record present,
    /// younger than the TTL, no ClaimEvent yet — created directly on the
    /// store, exactly the state `service_send_raw_txn` holds between
    /// `acquire_claim_lock` and publish completion). Signer B — a DIFFERENT
    /// key — submits a full valid claimAsset for the SAME gi:
    ///   1. IN FLIGHT (no ClaimEvent yet), within the TTL → rejected on the
    ///      "already submitted" path, with ZERO side effects for B (no publish,
    ///      no nonce advance, no tx). accept-and-revert does NOT fire here
    ///      because the gi has not LANDED (no ClaimEvent).
    ///   2. after A's claim LANDS (ClaimEvent recorded) → #55 accept-and-revert:
    ///      B's full-RPC resubmission is ACCEPTED with a reverted (status 0x0)
    ///      receipt (nonce consumed, NO new ClaimEvent, no Miden publish) — the
    ///      geth-faithful AlreadyClaimed, so a cross-signer's nonce never
    ///      desyncs. The claim-lock PRIMITIVE (`acquire_claim_lock`) is
    ///      unchanged and still hard-rejects a LANDED gi — accept-and-revert
    ///      lives in the RPC handler ABOVE the lock, not in the lock itself.
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
        let (input_b, tx_b) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let err = service_send_raw_txn(service.clone(), input_b.clone())
            .await
            .expect_err("a different signer's claim for an in-flight gi must be rejected");
        assert!(
            err.to_string().contains("already submitted"),
            "must be the dedup rejection: {err:#}"
        );
        // The rejection must leave NO trace of B's attempt.
        assert!(
            !miden.test_was_called(),
            "B must never reach the Miden publish while A is in flight"
        );
        assert_eq!(
            store.nonce_get(&format!("{addr_b:#x}")).await.unwrap(),
            0,
            "a rejected claim must not advance B's nonce"
        );
        assert!(
            store.txn_get(tx_b).await.unwrap().is_none(),
            "no tx entry may be recorded for B's rejected claim"
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
        let outcome = acquire_claim_lock(&store, gi, std::time::Duration::ZERO)
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
        let (input_b, _) = encode_tx_signed_with_nonce(&key_b, calldata, 0);

        let err = service_send_raw_txn(service, input_b)
            .await
            .expect_err("the stub MidenClient cannot complete the publish");
        // The orphaned record was superseded and B's claim PROCEEDED: the
        // failure is the unit stub's publish failure, NOT the dedup rejection.
        assert!(
            !err.to_string().contains("already submitted"),
            "an orphaned (TTL-expired) record must not keep rejecting: {err:#}"
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
        let calldata = claim_calldata(gi, resolvable_dest(), U256::from(1_000_000u64));
        let (input_hex, tx_hash) = encode_tx_signed_with_nonce(&user_key, calldata, 0);

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
        let (input_hex, _) = encode_tx_signed_with_nonce(&key_c, calldata, 0);
        let err = service_send_raw_txn(service.clone(), input_hex)
            .await
            .expect_err("the stub MidenClient cannot complete the publish");
        let msg = err.to_string();
        assert!(
            !msg.contains("allow-list") && !msg.contains("already submitted"),
            "someone-else's-deposit claim must not be rejected on any authorization \
             or dedup path: {msg}"
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
        let (input_hex, tx_hash) = encode_tx_signed_with_nonce(&key_c, calldata, 0);
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
        service.enable_writer_worker = true;
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
        service.enable_writer_worker = true;
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
            acquire_claim_lock(&store, gi, claim_resubmit_ttl())
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
            acquire_claim_lock(&store, gi, claim_resubmit_ttl())
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

        let outcome = acquire_claim_lock(&store, gi, std::time::Duration::from_secs(3600))
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

    /// BLOCKER 1 — atomic (signer, nonce) reservation: the FIRST tx to reserve a
    /// slot wins; a DIFFERENT tx at the same slot loses (HeldBy the winner); the
    /// SAME tx re-reserving is idempotent (HeldBy itself); a different nonce is free.
    #[tokio::test]
    async fn blocker_1_reserve_nonce_first_wins_different_tx_loses() {
        use crate::store::NonceReservation;
        let store: std::sync::Arc<dyn crate::store::Store> =
            std::sync::Arc::new(crate::store::memory::InMemoryStore::new());
        let addr = "0x00000000000000000000000000000000000000dd";
        let h1 = TxHash::from([0x11u8; 32]);
        let h2 = TxHash::from([0x22u8; 32]);

        assert_eq!(
            store.reserve_nonce(addr, 5, h1).await.unwrap(),
            NonceReservation::Won
        );
        // Same tx re-reserving → HeldBy(itself) (idempotent).
        assert_eq!(
            store.reserve_nonce(addr, 5, h1).await.unwrap(),
            NonceReservation::HeldBy(h1)
        );
        // A DIFFERENT tx at the SAME slot → HeldBy(the winner h1) → it must not execute.
        assert_eq!(
            store.reserve_nonce(addr, 5, h2).await.unwrap(),
            NonceReservation::HeldBy(h1)
        );
        // A different nonce is a free slot.
        assert_eq!(
            store.reserve_nonce(addr, 6, h2).await.unwrap(),
            NonceReservation::Won
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
        assert_eq!(
            store.reserve_nonce(&signer_str, 0, other).await.unwrap(),
            crate::store::NonceReservation::Won
        );

        let err = service_send_raw_txn(service, input_hex)
            .await
            .expect_err("must be rejected — the (signer, nonce) slot is reserved by another tx");
        assert!(
            err.to_string()
                .contains("already reserved by a different tx"),
            "unexpected: {err:#}"
        );
        // Did NOT execute: nonce not advanced.
        assert_eq!(store.nonce_get(&signer_str).await.unwrap(), 0);
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

        let outcome = acquire_claim_lock(&store, gi, claim_resubmit_ttl())
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
        service.enable_writer_worker = true;
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

        // (c) ABSENT hash → the reverted receipt IS written (status 0x0).
        let tx_new = TxHash::from([0x43u8; 32]);
        store
            .commit_reverted_receipt_and_advance_nonce(
                tx_new,
                entry(),
                "revert".into(),
                9,
                [0u8; 32],
                &signer_str,
                2,
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

        let outcome = acquire_claim_lock(&store, gi, std::time::Duration::from_secs(3600))
            .await
            .expect("in-flight classification must not error");
        assert_eq!(
            outcome,
            ClaimLockOutcome::InFlight,
            "an in-flight record (younger than the TTL, no ClaimEvent) must classify InFlight"
        );
        assert!(store.is_claimed(&gi).await.unwrap(), "lock stays held");
    }
}
