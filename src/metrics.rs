use anyhow::Context;
use metrics::{describe_counter, describe_gauge, describe_histogram};

/// Build and install the process-wide Prometheus recorder, returning the
/// handle the `/metrics` endpoint renders.
///
/// MUST be called before ANY thread/runtime that can emit a metric is
/// spawned — in particular before `MidenClient::new` (which spawns a
/// dedicated thread + second Tokio runtime that starts syncing, running
/// SyncListeners, and emitting immediately), the RD-940 writer worker, and
/// the L1InfoTreeIndexer. `metrics` resolves the global recorder on every
/// macro call (there is no per-thread caching), so a single global install
/// covers every thread and runtime in the process — but every emission that
/// happens BEFORE the install goes to the no-op recorder and is silently
/// lost. Historically main.rs installed this recorder AFTER creating the
/// MidenClient and spawning the writer worker / L1 indexer, so their early
/// emissions (init deploys, first sync ticks, first reconciler windows,
/// restore phases) never reached the registry served by `/metrics`.
///
/// NOTE the /metrics freeze observed on the live reindex had a second,
/// independent cause: `metrics` 0.24.5's broken `KeyHasher` (yanked
/// upstream, fixed in 0.24.6 — see
/// `metrics_from_second_runtime_render_cumulatively` for the full
/// mechanism and reproduction).
///
/// Histograms registered with `metrics-exporter-prometheus` default to the
/// Prometheus *summary* representation (a fixed set of quantiles), which
/// loses p95/p99 fidelity for low-volume metrics and — more importantly —
/// can't be aggregated across replicas in PromQL. For the two latency
/// metrics we actually care about (proof generation, JSON-RPC requests)
/// we install explicit bucket sets so they're emitted as real
/// `*_bucket{le="…"}` series. The proof buckets span 100ms (local prover
/// warm) → 5min (remote prover under load) because bali has empirically
/// seen 60–120s p99 proves; the RPC buckets are typical hot-path latencies
/// (1ms → 5s). Both metric names match the `histogram!()` call-site
/// strings in `metrics.rs` / `service.rs` exactly — using `Matcher::Full`
/// means a typo here silently falls back to summary, so add a test if
/// either name changes.
pub fn install_prometheus_recorder() -> anyhow::Result<metrics_exporter_prometheus::PrometheusHandle>
{
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("miden_proof_duration_seconds".to_string()),
            &[
                0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0,
            ],
        )
        .context("set_buckets_for_metric (miden_proof_duration_seconds) failed")?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("rpc_request_duration_seconds".to_string()),
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ],
        )
        .context("set_buckets_for_metric (rpc_request_duration_seconds) failed")?
        .install_recorder()
        .context("failed to install metrics recorder")?;
    init_metrics();
    Ok(handle)
}

pub fn init_metrics() {
    describe_counter!("rpc_requests_total", "Total JSON-RPC requests by method");
    describe_counter!("claims_processed_total", "Total claims processed");
    describe_counter!(
        "claim_resubmission_recovered_total",
        "orphaned claim submissions recovered (SOAK FINDING #1): a try_claim record with no \
         ClaimEvent whose in-flight TTL (CLAIM_RESUBMIT_TTL_SECS, default 120s) expired — the \
         proxy died between recording 'submitted' and the CLAIM landing on Miden — superseded, \
         and the sponsor's resubmission accepted. Nonzero after a crash in the submit window is \
         the recovery WORKING; a steady climb without restarts means claims are failing to land."
    );
    describe_counter!(
        "claim_landed_dedup_reverted_total",
        "#55 accept-and-revert: a claimAsset targeting an ALREADY-LANDED globalIndex (a real \
         ClaimEvent already exists) was ACCEPTED with a reverted (status 0x0) receipt instead of \
         hard-rejected at the JSON-RPC layer — so the submitter's nonce is consumed, geth-faithful \
         AlreadyClaimed. Nonzero means a sponsor/user cross-claimed the same gi and the sponsor's \
         nonce sequence was kept in lockstep (autoclaim NOT wedged). A steady climb means heavy \
         claim front-running, not a bug."
    );
    describe_counter!(
        "rpc_nonce_repaired_after_commit_gap_total",
        "#55 BLOCKER-2 crash-gap repair: on a same-hash rebroadcast, the signer's expected \
         nonce was still equal to the known tx's nonce — meaning the tx's durable receipt was \
         persisted but its nonce advance was lost to a crash BETWEEN the two on the sync accept \
         path — so the nonce was advanced to complete the interrupted accept. Nonzero after a \
         crash in the receipt→nonce window is the recovery WORKING (the signer is NOT wedged); a \
         steady climb without restarts would signal a store that is losing nonce writes."
    );
    describe_counter!(
        "rpc_nonce_reservation_lost_total",
        "#55 BLOCKER 1 cross-replica guard: a submission LOST the atomic (signer, nonce) \
         reservation to a DIFFERENT tx that already owned the slot, so it was rejected without \
         executing (no enqueue/dispatch/receipt). Nonzero means two txs raced the same nonce \
         slot (across replicas or a stale replacement) and the reservation kept exactly one; the \
         winner advances the nonce and the loser is dropped, mirroring geth."
    );
    describe_counter!("ger_injections_total", "Total GER injections");
    describe_gauge!(
        "projector_visibility_barrier_held_blocks",
        "#30 visibility barrier: blocks the projector is holding because the reconciler \
         sweep cursor is behind the Miden tip (0 in steady state; >0 signals reconciler lag)"
    );
    describe_counter!(
        "synthetic_projector_b2agg_authoritative_fetch_total",
        "unified projector: bridge-consumed B2AGG bodies resolved by canonical get_notes_by_id. \
         Increases once per successful resolution attempt; retries before cursor advance may \
         exceed bridge-out volume."
    );
    describe_counter!(
        "synthetic_projector_b2agg_headerless_skip_total",
        "unified projector: headerless bridge inputs with no persisted B2AGG identity. These are \
         normally CLAIM/GER/genesis notes covered by the store feed. A hidden B2AGG cannot seal \
         because the LET cardinality gate fails closed."
    );
    describe_counter!(
        "synthetic_projector_b2agg_fetch_missing_total",
        "unified projector: a bridge-consumed note was absent from get_notes_by_id. The LET \
         cardinality gate prevents sealing if it was a B2AGG leaf, \
         and the next projector tick retries it. MUST stay 0 in a healthy stack."
    );
    describe_counter!(
        "synthetic_projector_completeness_missing_total",
        "in-proxy completeness auditor: consumed B2AGG notes (past the settle margin, with the \
         projector's own emit gates mirrored) that have NO BridgeEvent at exactly their \
         consumption block. Detection only — getLogs immutability forbids late healing. MUST \
         stay 0; the soak gates on it. Alarmed once per note; the counter is cumulative."
    );
    describe_gauge!(
        "synthetic_projector_completeness_audit_lag",
        "in-proxy completeness auditor liveness beacon: the highest block audited so far \
         (projector cursor minus the settle margin). Flat while the chain advances = auditor \
         dead."
    );
    describe_counter!(
        "bridge_let_assignment_gate_halted_total",
        "projector ticks blocked before sealing because the bridge LET leaf count differs \
         from durable reservations plus visible pending B2AGG leaves. kind=invisible_gap: \
         the bridge is ahead; kind=local_ahead: local accounting is ahead. The next tick \
         retries. MUST stay 0; see docs/operations/let-cardinality-gate.md."
    );
    describe_counter!(
        "bridge_within_tx_order_unresolved_total",
        "Cantina #7 FAIL-CLOSED: >=2 B2AGG notes consumed by the SAME bridge transaction \
         whose within-tx order could not be established from the sync_transactions feed — \
         the projection tick HALTS (nothing sealed, retried) because emitting in hash order \
         could misnumber deposit_count/globalIndex, sealed forever by getLogs immutability. \
         MUST stay 0; any increment means feed corruption to investigate."
    );
    describe_counter!("bridge_outs_total", "Total bridge-out operations");
    describe_counter!("store_errors_total", "Total store operation errors");
    describe_histogram!("rpc_request_duration_seconds", "JSON-RPC request duration");
    describe_counter!(
        "miden_client_build_errors_total",
        "Failed attempts to build Miden client connection"
    );
    describe_counter!(
        "miden_client_restarts_total",
        "Background thread restarts after crash"
    );
    describe_counter!("miden_sync_errors_total", "Sync errors by kind");
    describe_counter!(
        "readonly_submissions_refused_total",
        "Transaction submissions refused by --read-only mode at the submit \
         chokepoint. Non-zero during a read-only drill means some code path \
         ATTEMPTED a chain mutation (and was stopped)."
    );
    describe_counter!(
        "bridge_out_self_targeted_total",
        "B2AGG bridge-outs whose destination_network equals our local network_id; \
         each one is a poison leaf that wedges the bridge (Cantina #13)"
    );
    describe_counter!(
        "bridge_burn_serial_collision_total",
        "BURN note serial collisions (Cantina #5). Each increment marks \
         a BURN note whose serial number was already observed for a \
         different leaf — the bridge's `mint_and_send` token_supply is at \
         risk of exhaustion. Page critical."
    );
    describe_counter!(
        "bridge_twin_note_detected_total",
        "Twin-NoteId detections (Cantina #6). Each increment marks a \
         second on-chain note sharing a previously-observed NoteId but \
         differing in metadata — the B2AGG reclaim attack signature. \
         Page critical."
    );
    describe_counter!(
        "bridge_mint_target_mismatch_total",
        "MINT note consumed by a faucet other than its NetworkAccountTarget \
         attachment (Cantina #2). The claimant is about to receive the \
         wrong wrapped asset. Page critical."
    );
    describe_counter!(
        "bridge_faucet_ownership_drift_total",
        "Faucet owner storage slot has changed away from the configured \
         bridge AccountId (Cantina #4). Labels: kind=drift (transferred to \
         another account) or kind=renounced (owner cleared, faucet wedged). \
         Page critical."
    );
    describe_counter!(
        "bridge_forged_mint_total",
        "MINT note observed on-chain that does not correspond to any \
         aggkit-recorded claim (Cantina #4). Forged via NoAuth bridge \
         note authorship. Page critical, freeze claim processing."
    );
    describe_counter!(
        "bridge_expected_mint_stale_total",
        "Expected MINT NoteId did not land within the configured retry \
         threshold (Cantina #7). Indicates batch-dedup censorship via a \
         metadata-distinct twin. Triggers retry; alerts after K retries."
    );
    describe_counter!(
        "store_envelope_decode_errors_total",
        "PgStore TxEnvelope decode failures (S9). Each increment marks a \
         corrupt or schema-drifted transactions row that surfaced as an \
         error rather than masking as not-found. Investigate immediately."
    );
    describe_counter!(
        "bridge_out_invalid_destination_total",
        "B2AGG bridge-out rejected because the destination address is the \
         zero address or in the EVM precompile range (B7). Forwarding such \
         events to bridge-service would waste cert-build work."
    );
    describe_counter!(
        "address_mapper_zero_padding_fallback_total",
        "Address-mapper zero-padding fallback was taken (C5). The EVM \
         destination had no explicit store mapping; a Miden AccountId was \
         reconstructed from the trailing 16 bytes. Account existence on \
         Miden is NOT verified — alert on unusual rates."
    );
    describe_counter!(
        "bridge_out_unknown_faucet_total",
        "B2AGG note referenced a faucet not in the registry (B8). \
         Quarantined to prevent silent re-loop on every sync tick."
    );
    describe_counter!(
        "bridge_unknown_wrapper_consumed_total",
        "Bridge account consumed a note whose script root is neither the \
         canonical B2AGG bridge-out wrapper nor the CLAIM script (Cantina \
         MA#4). The on-chain LET frontier has advanced; aggkit cannot \
         synthesise a BridgeEvent for an unrecognised wrapper. Operator \
         must investigate before more funds are stranded."
    );
    describe_counter!(
        "bridge_out_quarantined_erased_b2agg_total",
        "B2AGG note observed consumed by the bridge but skipped by the \
         indexer because the note contents were unparsable or referenced \
         an unknown faucet (Cantina MA#18). A row was written to the \
         quarantine table so operators have a concrete handle for a \
         future recovery flow."
    );
    describe_counter!(
        "rpc_claim_ger_not_seen_total",
        "Claim submission rejected at the C6 pre-admission gate because \
         the referenced GER was not yet published (`is_ger_injected`). \
         Caller should retry after the GER is injected; no nonce, lock, \
         receipt, or queued job is consumed, so retries are cheap."
    );
    describe_counter!(
        "rpc_estimate_gas_ger_not_ready_total",
        "eth_estimateGas(claimAsset) answered with `execution reverted: \
         GlobalExitRootInvalid()` because the claim's combined GER is not \
         yet published (Cantina #21). Mirrors the EVM bridge's fail-fast \
         _verifyLeaf revert so the ClaimTxManager retries before ever \
         allocating a nonce."
    );
    describe_counter!(
        "claim_watcher_synthesised_total",
        "ClaimWatcher synthesised a ClaimEvent from a consumed CLAIM note \
         that the normal eth_sendRawTransaction path had not recorded \
         (crash recovery or foreign-CLAIM observation)."
    );
    describe_counter!(
        "restore_b2agg_same_details_multiplicity_quarantined_total",
        "restore FAIL-CLOSED: B2AGG exits quarantined because the authoritative feed shows ≥2 \
         distinct on-chain consumptions sharing a details_commitment that the commitment-keyed \
         client store cannot disambiguate — quarantined rather than emit a wrong/collapsed \
         BridgeEvent (review). MUST be rare; each is an operator-recoverable exit."
    );
    describe_counter!(
        "synthetic_claim_calldata_finalized_pending_total",
        "synthesized-claim calldata rows found PENDING (txn_begin ran, txn_commit did not — a \
         crash between them) and finalized by a later persist pass, rather than being stranded \
         pending forever (review blocker 3)."
    );
    describe_counter!(
        "restore_b2agg_authoritative_attributed_total",
        "restore Phase 2 (task #56): consumed B2AGG notes whose consumer the LOCAL store did \
         not know (consumer_account=None — NTX-consumed, the normal bridge path, observed \
         after a store rebuild) but that the bridge's on-chain sync_transactions feed \
         authoritatively attributes to the bridge — rebuilt and re-projected instead of \
         fail-closed skipped. Without this, restore ERASED already-settled BridgeEvents \
         (getLogs-immutability break) and halted aggkit's L2BridgeSyncer."
    );
    describe_counter!(
        "synthetic_claim_calldata_persisted_total",
        "synthesized (derived-hash) claims whose FULL authoritative claimAsset calldata was \
         recovered (CLAIM note storage: both SMT proofs, both exit roots, networks, addresses, \
         amount; faucet registry: hash-verified metadata preimage) and persisted under the \
         derived tx hash for eth_getTransactionByHash / aggkit's full-claim parser."
    );
    describe_counter!(
        "synthetic_claim_calldata_unrecoverable_total",
        "synthesized claims whose metadata preimage could NOT be recovered authoritatively \
         (no registry entry hashing to the note's metadata_hash) — calldata deliberately NOT \
         fabricated; the tx keeps an empty input and aggkit will stall on it. Operator action: \
         register/repair the faucet metadata; the per-tick backfill then self-heals. MUST stay \
         0 in a healthy stack."
    );
    describe_counter!(
        "synthetic_claim_tx_missing_calldata_total",
        "ClaimEvent-bearing synthetic txs served with EMPTY input by eth_getTransactionByHash \
         (no persisted calldata record — unrecoverable, or the backfill has not caught up). \
         Every increment stalls aggkit on that claim; must stay 0 in steady state."
    );
    describe_counter!(
        "claim_event_foreign_skipped_total",
        "A consumed CLAIM-shaped note was NOT provably ours (consumer is not \
         our bridge, and it was not minted by our service targeting our \
         bridge) and was skipped instead of projected as a ClaimEvent. \
         Expected on chains shared with a foreign miden-agglayer deployment \
         (its claims share our ClaimNote script root); on a single-deployment \
         chain any non-zero rate means unverifiable claim consumptions — \
         investigate."
    );
    describe_counter!(
        "claim_watcher_already_recorded_total",
        "ClaimWatcher observed a consumed CLAIM whose ClaimEvent was \
         already in the store (either prior watcher emission or \
         eth_sendRawTransaction emission). Note marked processed and \
         skipped — counted to monitor the dedup-rate."
    );
    describe_counter!(
        "claim_watcher_storage_decode_total",
        "ClaimWatcher could not decode the on-chain storage of a \
         consumed CLAIM note (truncated felts, oversize amount, etc.). \
         Quarantined to prevent re-loop. Investigate any non-zero rate."
    );
    describe_counter!(
        "claim_watcher_unrecoverable_total",
        "Consumed CLAIM note where the watcher cannot synthesise a \
         ClaimEvent at all — currently fires alongside \
         claim_watcher_storage_decode_total when quarantining a \
         malformed note. Page if rate spikes."
    );
    describe_counter!(
        "miden_proof_generations_total",
        "Miden zk-proof generations completed. Labels: \
         kind=claim|ger|faucet|init|bridge_out (call-site category, bounded), \
         op=prove|submit (prove = pure prover call; submit = end-to-end \
         execute+prove+submit+sync, dominated by proving), \
         outcome=ok|fallback_ok|timeout|connect_failure|prover_error|\
         submit_failure|build_failed|fallback_error (bounded)."
    );
    describe_histogram!(
        "miden_proof_duration_seconds",
        "Wall-clock duration of a Miden zk-proof generation in seconds. \
         Labels: kind=claim|ger|faucet|init|bridge_out (bounded), \
         op=prove|submit (op=prove is pure prover latency on Claim; \
         op=submit is end-to-end submit latency dominated by proving on the \
         other call sites). Recorded on both success and error paths."
    );

    // RD-940 — single writer observability (Spec F §4).
    describe_gauge!(
        "agglayer_writer_queue_depth",
        "RD-940: current number of WriteJobs sitting in the writer-worker \
         mpsc channel (gauge). Alert: >0.8×cap for 10 min → warn; \
         >0.95×cap for 2 min → page."
    );
    describe_gauge!(
        "agglayer_writer_inflight_jobs",
        "RD-940: WriteJobs in the in-flight DashMap (Queued + Submitting + \
         not-yet-TTL'd terminal entries). Informational."
    );
    describe_histogram!(
        "agglayer_writer_job_duration_seconds",
        "RD-940: time from worker dequeue to terminal outcome (committed / \
         failed). Labels: kind=claim|ger_insert, outcome=committed|failed. \
         Alert: p99 >60s for 10 min → page (aggkit's WaitTxToBeMined=2m)."
    );
    describe_counter!(
        "agglayer_writer_queue_full_rejections_total",
        "RD-940: eth_sendRawTransaction requests rejected because the \
         writer-worker mpsc channel was at capacity. Wire response is \
         JSON-RPC -32005 'writer queue saturated; retry' (geth's \
         LimitExceeded); aggkit's ethtxmanager retries transparently. \
         Labels: kind=claim|ger_insert. Alert: rate >0.1/s for 5 min → page."
    );

    // RD-940 Phase 5 observability — the remaining 3 metrics from Spec F §4.
    describe_counter!(
        "agglayer_writer_job_failures_total",
        "RD-940: writer-worker jobs that reached a terminal Failed state. \
         Labels: kind=claim|ger_insert|unknown, reason=miden|ttl|panic|store. \
         Alert: burst >0.5/s for 5 min → page."
    );
    describe_counter!(
        "agglayer_writer_dropped_on_restart_total",
        "RD-940: queue-depth snapshot read on boot from the previous \
         process's graceful shutdown. A non-zero value means the previous \
         restart lost that many in-memory dispatches whose signed envelopes \
         remain durable — those callers MUST re-submit the SAME hash. \
         **Hard page on increase[1h]>0** — restart-pressure tripwire. \
         The metric is silent under SIGKILL because the tmpfile is only \
         written on graceful drain; combined with pre-kill queue-depth \
         history this still pinpoints the loss window."
    );
    describe_counter!(
        "agglayer_writer_drain_outcome_total",
        "RD-940: graceful-shutdown drain outcomes. Labels: outcome=clean \
         (queue empty within budget) | partial (budget elapsed, residual \
         jobs left for dropped_on_restart accounting). Dashboard only — \
         not paging."
    );
}

// =====================================================================
// Proof-call instrumentation (Fix 11/7/12/6/14/5)
// =====================================================================
//
// Every prove/submit call site in the proxy goes through `meter_proof`
// (the 7 submit sites) or `meter_proof_with_fallback` (the single prove
// site in `claim.rs` that also wires the local-prover retry from
// `--miden-prover-fallback-to-local`). The wrappers centralise:
//
//   - histogram + counter naming and the (kind, op, outcome) label set
//   - best-effort outcome classification (timeout/connect/prover/submit)
//   - fallback bookkeeping so dashboards can split first-try success,
//     second-try-via-local success, and total failure
//
// Adding a new call site = `meter_proof(ProofKind::X, future).await?;`
// — the kind enum is what stops accidental free-text labels from leaking
// unbounded cardinality into Prometheus.

/// Compile-time–enforced "kind" label for Miden proof metrics.
///
/// Each variant tracks where the proof call originates so dashboards can
/// split prover load by service path (claim hot path vs. boot init vs.
/// admin tooling). Bounded enum → bounded cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofKind {
    /// `claim.rs::publish_claim_internal` — explicit `prove_transaction`
    /// call on the CLAIM hot path.
    Claim,
    /// `ger.rs::insert_ger` — `submit_new_transaction` for UpdateGerNote.
    Ger,
    /// `faucet_ops::create_and_register_faucet` / `register_faucet_in_bridge`.
    Faucet,
    /// `init.rs::deploy_account` / `register_p2id_script` — boot path.
    Init,
    /// `bin/bridge_out_tool.rs` — both the consume-existing-notes path
    /// and the final B2AGG submit.
    BridgeOut,
}

impl ProofKind {
    pub fn as_label(&self) -> &'static str {
        match self {
            ProofKind::Claim => "claim",
            ProofKind::Ger => "ger",
            ProofKind::Faucet => "faucet",
            ProofKind::Init => "init",
            ProofKind::BridgeOut => "bridge_out",
        }
    }

    /// Distinguishes pure prover latency from end-to-end submit latency.
    ///
    /// Only `ProofKind::Claim` wraps an explicit `client.prove_transaction`
    /// — the rest wrap `client.submit_new_transaction` which is
    /// execute+prove+submit+sync. The histogram dominates the same way
    /// (proving is the heavy step) but the `op` label is required for
    /// any alert that wants to bound true prover RTT (op=prove) vs.
    /// total submit time including post-prove node calls (op=submit).
    pub fn op_label(&self) -> &'static str {
        match self {
            ProofKind::Claim => "prove",
            ProofKind::Ger | ProofKind::Faucet | ProofKind::Init | ProofKind::BridgeOut => "submit",
        }
    }
}

/// Outcome classification for `miden_proof_generations_total`.
///
/// Finer-grained than ok/error so dashboards can tell a remote-prover
/// outage (`ConnectFailure` / `Timeout`) from a real proof failure
/// (`ProverError`) from a post-prove submission failure (`SubmitFailure`).
/// `FallbackOk` / `FallbackError` only appear on `ProofKind::Claim` when
/// `--miden-prover-fallback-to-local` is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofOutcome {
    /// Proof succeeded against the configured prover (remote or local).
    Ok,
    /// Remote prover failed but the local-prover fallback succeeded.
    /// Indicates the operator's `--miden-prover-fallback-to-local`
    /// safety net actually fired — pair with `Timeout`/`ConnectFailure`
    /// counts to see WHICH remote failure mode is being papered over.
    FallbackOk,
    /// gRPC DeadlineExceeded (per-request timeout fired). On the remote
    /// prover this is the most common OOM-cascade symptom: the prover
    /// process held the connection long enough to exhaust the timeout
    /// but never returned a proof.
    Timeout,
    /// Failed to establish a gRPC channel to the prover. Distinct from
    /// `Timeout` — connect-failure means the prover is gone or
    /// unreachable; timeout means it took the call but never finished.
    ConnectFailure,
    /// The prover ran but returned a real proving error
    /// (`TransactionProvingError`). Default classification for ambiguous
    /// errors so a quiet metric drift doesn't hide a degraded prover.
    ProverError,
    /// Post-prove submission step failed (execute / submit_proven /
    /// mempool admission). Only meaningful for op=submit — for
    /// op=prove this variant is unreachable. Tracks "the proof
    /// generated fine, but the node rejected the tx" separately from
    /// proving issues.
    SubmitFailure,
    /// Pre-prove tx-request build failed — currently only
    /// `bridge_out_tool.rs` outer error arm. No histogram recorded
    /// (duration is meaningless before proving even starts).
    BuildFailed,
    /// Both the configured prover AND the local fallback failed.
    /// Page critical: the fallback is the last line of defence.
    FallbackError,
}

impl ProofOutcome {
    pub fn as_label(&self) -> &'static str {
        match self {
            ProofOutcome::Ok => "ok",
            ProofOutcome::FallbackOk => "fallback_ok",
            ProofOutcome::Timeout => "timeout",
            ProofOutcome::ConnectFailure => "connect_failure",
            ProofOutcome::ProverError => "prover_error",
            ProofOutcome::SubmitFailure => "submit_failure",
            ProofOutcome::BuildFailed => "build_failed",
            ProofOutcome::FallbackError => "fallback_error",
        }
    }

    /// Best-effort classification from an arbitrary error's `Display`.
    ///
    /// `ClientError` and `RemoteProverClientError` don't expose a stable
    /// discriminator (the variants we care about — `Timeout`,
    /// `ConnectionFailed`, `TransactionProvingError` — are buried inside
    /// `#[from]` chains and `Box<dyn Error>` sources). Matching on the
    /// `Display` rendering is the heuristic miden-client itself uses in
    /// `unwrap_connection_error`-style call sites; it's good enough for
    /// dashboard splits. Default is `ProverError` so any unclassified
    /// error still falls into a "proof-side failure" bucket rather than
    /// silently dropping off the dashboard.
    pub fn from_error<E: std::fmt::Display>(err: &E) -> Self {
        let msg = err.to_string();
        let lower = msg.to_ascii_lowercase();
        if lower.contains("deadlineexceeded")
            || lower.contains("deadline exceeded")
            || lower.contains("timed out")
            || lower.contains("timeout")
        {
            ProofOutcome::Timeout
        } else if lower.contains("connectionfailed")
            || lower.contains("connection error")
            || lower.contains("failed to connect")
            || lower.contains("unavailable")
            || lower.contains("transport error")
        {
            ProofOutcome::ConnectFailure
        } else if lower.contains("transactionprovingerror")
            || lower.contains("transaction proving failed")
            || lower.contains("proving failed")
        {
            ProofOutcome::ProverError
        } else if lower.contains("submit")
            || lower.contains("mempool")
            || lower.contains("admission")
            || lower.contains("incorrectaccountinitialcommitment")
        {
            ProofOutcome::SubmitFailure
        } else {
            ProofOutcome::ProverError
        }
    }
}

fn record_proof_metrics(kind: ProofKind, outcome: ProofOutcome, elapsed_secs: Option<f64>) {
    if let Some(secs) = elapsed_secs {
        metrics::histogram!(
            "miden_proof_duration_seconds",
            "kind" => kind.as_label(),
            "op" => kind.op_label(),
        )
        .record(secs);
    }
    metrics::counter!(
        "miden_proof_generations_total",
        "kind" => kind.as_label(),
        "op" => kind.op_label(),
        "outcome" => outcome.as_label(),
    )
    .increment(1);
}

/// Record a proof-call metric inline for a known outcome (no duration,
/// e.g. build-time failures before proving started). The 8th, 9th call
/// sites can call this directly when `meter_proof` doesn't fit (e.g.
/// `bridge_out_tool.rs` outer error arm for `build_consume_notes`).
pub fn record_proof_outcome(kind: ProofKind, outcome: ProofOutcome) {
    record_proof_metrics(kind, outcome, None);
}

/// Wraps a proof-producing future and records both the duration
/// histogram and the per-outcome counter on completion. Replaces the
/// inline `__proof_start`/`__res` pattern at every submit site.
///
/// Outcome is derived via `ProofOutcome::from_error` when the future
/// returns `Err`. Call sites that need to override the classification
/// (e.g. the bridge_out_tool consume-notes branch wants to split
/// SubmitFailure from a real prover failure) should call
/// `meter_proof_classified` or emit metrics inline.
pub async fn meter_proof<F, T, E>(kind: ProofKind, fut: F) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let start = std::time::Instant::now();
    let res = fut.await;
    let elapsed = start.elapsed().as_secs_f64();
    let outcome = match &res {
        Ok(_) => ProofOutcome::Ok,
        Err(e) => ProofOutcome::from_error(e),
    };
    record_proof_metrics(kind, outcome, Some(elapsed));
    res
}

/// Records primary-attempt metrics. Returns the result unchanged so the
/// caller can decide whether to invoke a fallback (see
/// `record_fallback_attempt`). This split-helper API exists because the
/// CLAIM hot path retries against a `LocalTransactionProver` borrowed
/// from the same `&mut MidenClientLib`, and a single combined helper
/// can't construct both closures at once without tripping the borrow
/// checker — primary and fallback both want `&mut client`. Splitting
/// the metric emission keeps the call site readable while still
/// centralising the label set.
///
/// Returns the result unchanged plus the elapsed seconds so the caller
/// can attribute fallback retry time correctly.
pub fn record_primary_attempt<T, E>(
    kind: ProofKind,
    result: Result<T, E>,
    elapsed_secs: f64,
    has_fallback: bool,
) -> (Result<T, E>, Option<ProofOutcome>)
where
    E: std::fmt::Display,
{
    match &result {
        Ok(_) => {
            record_proof_metrics(kind, ProofOutcome::Ok, Some(elapsed_secs));
            (result, None)
        }
        Err(e) => {
            let outcome = ProofOutcome::from_error(e);
            record_proof_metrics(kind, outcome, Some(elapsed_secs));
            if has_fallback {
                (result, Some(outcome))
            } else {
                (result, None)
            }
        }
    }
}

/// Records the fallback-retry metric.
///
/// `outcome` is `FallbackOk` on success and `FallbackError` on a second
/// failure. Histogram is recorded for the fallback alone (independent
/// of the primary timing) so dashboards can see how much extra wall
/// clock the safety net adds.
pub fn record_fallback_attempt<T, E>(
    kind: ProofKind,
    result: Result<T, E>,
    elapsed_secs: f64,
) -> Result<T, E> {
    let outcome = match &result {
        Ok(_) => ProofOutcome::FallbackOk,
        Err(_) => ProofOutcome::FallbackError,
    };
    record_proof_metrics(kind, outcome, Some(elapsed_secs));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RED→GREEN regression for the live /metrics unreliability (reindex run:
    /// gauge frozen at 2000 while the true value was 156000; counters
    /// rendering per-window values instead of cumulative ones — all from code
    /// on the MidenClient's dedicated second Tokio runtime).
    ///
    /// Root cause (reproduced deterministically, then fixed): `metrics`
    /// 0.24.5 — since YANKED upstream — shipped a broken `KeyHasher` whose
    /// hash of a `Key` was inconsistent between the registry's insert and
    /// lookup paths, so (almost) every emission MISSED the existing registry
    /// entry and created a fresh phantom entry holding only that emission's
    /// delta. The Prometheus exporter's render dedups by (name, labels) via
    /// overwrite, surfacing an arbitrary phantom per scrape: counters showed
    /// small non-cumulative "windows", gauges showed stale early sets.
    /// Fixed in `metrics` 0.24.6 (rewritten `KeyHasher`, Cargo.lock updated);
    /// this test FAILED on 0.24.5 (rendered `..._events_total 1` and
    /// `..._cursor 2000`) and passes on 0.24.6.
    ///
    /// Contract pinned: with the process-wide recorder installed via
    /// `install_prometheus_recorder`, metrics emitted from a *separate thread
    /// running its own Tokio runtime* — the exact construction
    /// `MidenClient::new` uses — land in the ONE global registry the endpoint
    /// handle renders: counters cumulative (increment twice → 2), gauges at
    /// their LATEST set value.
    #[test]
    fn metrics_from_second_runtime_render_cumulatively() {
        // Sole global-recorder install in the test binary (installing twice
        // errors, so this test owns it; unique metric names keep concurrent
        // tests' emissions from mattering).
        let handle = install_prometheus_recorder().expect("install must succeed once");

        // Mirror MidenClient::new: a dedicated OS thread driving its OWN
        // multi-thread Tokio runtime, emitting both from the block_on future
        // (the run loop) and from a spawned task (runtime worker thread).
        let t = std::thread::spawn(|| {
            let runtime = tokio::runtime::Runtime::new().expect("second runtime");
            runtime.block_on(async {
                ::metrics::counter!("test_second_runtime_events_total").increment(1);
                ::metrics::gauge!("test_second_runtime_cursor").set(2_000.0);
                tokio::spawn(async {
                    ::metrics::counter!("test_second_runtime_events_total").increment(1);
                    ::metrics::gauge!("test_second_runtime_cursor").set(156_000.0);
                })
                .await
                .expect("spawned emitter");
            });
        });
        t.join().expect("second-runtime thread");

        let rendered = handle.render();
        assert!(
            rendered.contains("test_second_runtime_events_total 2"),
            "counter emitted from the second runtime must render CUMULATIVELY \
             (2 after two increments) on the endpoint handle; got:\n{rendered}"
        );
        assert!(
            rendered.contains("test_second_runtime_cursor 156000"),
            "gauge emitted from the second runtime must render its LATEST value \
             (156000), not a stale early one; got:\n{rendered}"
        );
    }

    #[test]
    fn proof_kind_label_stable() {
        assert_eq!(ProofKind::Claim.as_label(), "claim");
        assert_eq!(ProofKind::Ger.as_label(), "ger");
        assert_eq!(ProofKind::Faucet.as_label(), "faucet");
        assert_eq!(ProofKind::Init.as_label(), "init");
        assert_eq!(ProofKind::BridgeOut.as_label(), "bridge_out");
    }

    #[test]
    fn proof_kind_op_label_distinguishes_prove_from_submit() {
        assert_eq!(ProofKind::Claim.op_label(), "prove");
        assert_eq!(ProofKind::Ger.op_label(), "submit");
        assert_eq!(ProofKind::Faucet.op_label(), "submit");
        assert_eq!(ProofKind::Init.op_label(), "submit");
        assert_eq!(ProofKind::BridgeOut.op_label(), "submit");
    }

    #[test]
    fn proof_outcome_label_stable() {
        assert_eq!(ProofOutcome::Ok.as_label(), "ok");
        assert_eq!(ProofOutcome::FallbackOk.as_label(), "fallback_ok");
        assert_eq!(ProofOutcome::Timeout.as_label(), "timeout");
        assert_eq!(ProofOutcome::ConnectFailure.as_label(), "connect_failure");
        assert_eq!(ProofOutcome::ProverError.as_label(), "prover_error");
        assert_eq!(ProofOutcome::SubmitFailure.as_label(), "submit_failure");
        assert_eq!(ProofOutcome::BuildFailed.as_label(), "build_failed");
        assert_eq!(ProofOutcome::FallbackError.as_label(), "fallback_error");
    }

    #[test]
    fn from_error_classifies_timeout() {
        let e = std::io::Error::new(std::io::ErrorKind::TimedOut, "DeadlineExceeded");
        assert_eq!(ProofOutcome::from_error(&e), ProofOutcome::Timeout);
        let e2 = "request to Miden node timed out; the node may be under heavy load";
        assert_eq!(ProofOutcome::from_error(&e2), ProofOutcome::Timeout);
    }

    #[test]
    fn from_error_classifies_connect_failure() {
        let e = "failed to connect to prover http://miden-prover:50051";
        assert_eq!(ProofOutcome::from_error(&e), ProofOutcome::ConnectFailure);
        let e2 = "Miden node is unavailable; check that the node is running and reachable";
        assert_eq!(ProofOutcome::from_error(&e2), ProofOutcome::ConnectFailure);
    }

    #[test]
    fn from_error_classifies_prover_error() {
        let e = "TransactionProvingError(...)";
        assert_eq!(ProofOutcome::from_error(&e), ProofOutcome::ProverError);
        let e2 = "transaction proving failed";
        assert_eq!(ProofOutcome::from_error(&e2), ProofOutcome::ProverError);
    }

    #[test]
    fn from_error_defaults_to_prover_error() {
        let e = "some weird error nobody's seen before";
        assert_eq!(ProofOutcome::from_error(&e), ProofOutcome::ProverError);
    }

    #[tokio::test]
    async fn meter_proof_ok_path_returns_value() {
        let res: Result<i32, &str> =
            meter_proof(ProofKind::Ger, async { Ok::<i32, &str>(42) }).await;
        assert_eq!(res, Ok(42));
    }

    #[tokio::test]
    async fn meter_proof_err_path_returns_error() {
        let res: Result<i32, &str> =
            meter_proof(ProofKind::Ger, async { Err::<i32, &str>("boom") }).await;
        assert_eq!(res, Err("boom"));
    }

    #[test]
    fn record_primary_attempt_ok_returns_value_and_no_outcome() {
        let res: Result<i32, &str> = Ok(7);
        let (out, retry) = record_primary_attempt(ProofKind::Claim, res, 0.5, true);
        assert_eq!(out, Ok(7));
        assert_eq!(retry, None);
    }

    #[test]
    fn record_primary_attempt_err_with_fallback_returns_outcome() {
        let res: Result<i32, &str> = Err("DeadlineExceeded");
        let (out, retry) = record_primary_attempt(ProofKind::Claim, res, 1.0, true);
        assert_eq!(out, Err("DeadlineExceeded"));
        assert_eq!(retry, Some(ProofOutcome::Timeout));
    }

    #[test]
    fn record_primary_attempt_err_without_fallback_returns_none_retry() {
        let res: Result<i32, &str> = Err("boom");
        let (out, retry) = record_primary_attempt(ProofKind::Claim, res, 1.0, false);
        assert_eq!(out, Err("boom"));
        assert_eq!(retry, None);
    }

    #[test]
    fn record_fallback_attempt_passes_result_through() {
        let res: Result<i32, &str> = Ok(42);
        assert_eq!(record_fallback_attempt(ProofKind::Claim, res, 0.3), Ok(42));
        let res2: Result<i32, &str> = Err("nope");
        assert_eq!(
            record_fallback_attempt(ProofKind::Claim, res2, 0.3),
            Err("nope")
        );
    }
}
