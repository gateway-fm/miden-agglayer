//! Standalone L2→L1 auto-claimer.
//!
//! ## Why this exists
//!
//! Polygon declined to fix the `zkevm-bridge-service` `/pending-bridges`
//! rollup-disambiguation bug (they class shared-rollup-manager L2→L1
//! auto-claiming as an "unsupported mode of operation"), so the forked-image
//! path is dead. The defect: `/pending-bridges`' already-claimed gate matches a
//! recorded L1 claim on `(destination_network, leaf_index)` ONLY, dropping the
//! source rollup. On a shared L1 bridge every rollup claims L2→L1 exits under
//! `network_id = 0` with overlapping per-rollup leaf indices, so once any
//! co-tenant claimed *their* exit `#N`, ours was hidden forever.
//!
//! ## How this sidesteps it
//!
//! This claimer never touches `/pending-bridges`. It:
//!   1. **Discovers** our rollup's L2→L1 exits from the proxy's OWN synthetic
//!      `BridgeEvent` via `eth_getLogs` (filtered to the bridge address +
//!      `BridgeEvent` topic). The proxy only ever emits events for our rollup,
//!      so discovery is rollup-scoped *by construction* — no co-tenant data is
//!      ever in scope, and no `SourceNetworkID` filter is needed.
//!   2. **Gates** each exit on the L1 bridge's on-chain
//!      `isClaimed(leafIndex, sourceBridgeNetwork)` — authoritative and
//!      structurally immune to the leaf-index collision that poisoned
//!      `/pending-bridges`.
//!   3. **Fetches proofs** from the bridge-service `/merkle-proof` endpoint,
//!      which is backed by `GetClaim` (always correctly rollup-qualified) — the
//!      one bridge-service path that never had the bug.
//!   4. **Submits** `claimAsset` on L1 with a sponsor wallet.
//!
//! ## Decision record (see README "L2->L1 auto-claimer" for the prose version)
//!
//! - **Readiness gate = (b) attempt-and-retry**, implemented as a pre-flight
//!   `eth_call` simulation of `claimAsset`. A not-yet-settled GER reverts with
//!   `GlobalExitRootInvalid`; we classify that as transient and retry on the
//!   next poll rather than burning gas on a doomed send. Only a clean
//!   simulation is followed by a signed submission.
//! - **Idempotency = block cursor + on-chain `isClaimed`.** The cursor (a tiny
//!   sqlite file) only bounds how far back we re-scan logs; the real
//!   double-spend guard is `isClaimed`, which is authoritative, so the claimer
//!   is safe even if the cursor is lost or reset.
//! - **Sponsor key = `--sponsor-key-env`.** The private key is read from the
//!   named environment variable (populated from the secret store in
//!   deployment); it is never a CLI flag and never logged.

use alloy::eips::BlockNumberOrTag;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolError, SolEvent};
use alloy_rpc_types_eth::TransactionRequest;
use std::sync::Mutex;
use std::time::Duration;

use crate::claim::claimAssetCall;
use crate::exit::BridgeEvent;

alloy_core::sol! {
    // PolygonZkEVMBridgeV2.isClaimed — the authoritative, rollup-qualified
    // already-claimed check. `sourceBridgeNetwork` is the SOURCE rollup
    // (rollup_index + 1 == our network id), NOT the asset's origin network.
    #[derive(Debug)]
    function isClaimed(uint32 leafIndex, uint32 sourceBridgeNetwork) external view returns (bool);
}

alloy_core::sol! {
    // Bridge custom errors we classify when a claimAsset simulation reverts.
    // `GlobalExitRootInvalid` == GER not settled on L1 yet → transient/retry.
    error GlobalExitRootInvalid();
    error AlreadyClaimed();
    error InvalidSmtProof();
    error MerkleTreeFull();
    error DestinationNetworkInvalid();
}

// ─── Configuration ──────────────────────────────────────────────────────────

/// Resolved configuration for one claimer run. `sponsor_key` is plaintext at
/// this point (resolved by the binary from `--sponsor-key-env`); it is never
/// logged and never round-tripped through Debug — the field is deliberately
/// excluded from any Debug derive on this struct.
pub struct ClaimerConfig {
    /// L2 proxy JSON-RPC URL (source of `BridgeEvent` via `eth_getLogs`).
    pub l2_rpc_url: String,
    /// L1 JSON-RPC URL (isClaimed view-calls + claimAsset submission).
    pub l1_rpc_url: String,
    /// Bridge contract address. Used both as the `eth_getLogs` address filter on
    /// L2 (the proxy stamps synthetic BridgeEvent logs with this address) and as
    /// the `claimAsset`/`isClaimed` target on L1. Assumes the canonical CDK
    /// deterministic deploy where L1 and L2 share the bridge address.
    pub bridge_address: Address,
    /// Bridge-service base URL (for `/merkle-proof`).
    pub bridge_service_url: String,
    /// Our rollup's agglayer network id (e.g. 1 in kurtosis, 76 on Bali).
    pub network_id: u32,
    /// Sponsor private key (plaintext, resolved from `--sponsor-key-env`).
    pub sponsor_key: String,
    /// Poll cadence.
    pub poll_interval: Duration,
    /// Max L2 block span scanned per poll.
    pub max_range: u64,
    /// Optional explicit start block (overrides the persisted cursor for one boot).
    pub start_block: Option<u64>,
    /// Path to the sqlite cursor file.
    pub cursor_db_path: String,
}

// ─── Pure helpers (unit-tested below; no I/O) ─────────────────────────────────

/// Build the agglayer `globalIndex` U256.
///
/// Layout (big-endian, mirrors `claim.rs::is_mainnet_global_index`):
///   - bits  0..32  (bytes 28..32): leaf index
///   - bits 32..64  (bytes 24..28): rollup index
///   - bit  64      (bytes 20..24): mainnet flag (1 = mainnet, 0 = rollup)
///
/// For an L2→L1 exit the source is a rollup, so `mainnet = false` and
/// `rollup_index = network_id - 1`.
pub fn global_index(mainnet: bool, rollup_index: u32, leaf_index: u32) -> U256 {
    let flag = if mainnet {
        U256::from(1u64) << 64
    } else {
        U256::ZERO
    };
    flag | (U256::from(rollup_index) << 32) | U256::from(leaf_index)
}

/// Convenience: the rollup-source globalIndex for one of *our* exits.
pub fn our_global_index(network_id: u32, leaf_index: u32) -> U256 {
    global_index(false, network_id.saturating_sub(1), leaf_index)
}

/// `sourceBridgeNetwork` argument for `isClaimed` for one of our L2→L1 exits.
/// This is the source ROLLUP network (== our network id), not the asset's
/// origin network (which is 0 for native ETH).
pub fn source_bridge_network(network_id: u32) -> u32 {
    network_id
}

/// Outcome of simulating (or attempting) a claim, used to decide retry vs skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Simulation succeeded — safe to submit the signed tx.
    Ready,
    /// GER not settled on L1 yet — retry on a later poll. Decision (b).
    NotReadyRetry,
    /// Already claimed on L1 — skip (belt-and-braces; we also pre-gate on isClaimed).
    AlreadyClaimed,
    /// Anything else — surfaced loudly, not silently retried as if transient.
    Permanent(String),
}

fn selector_hex<E: SolError>() -> String {
    format!("0x{}", hex::encode(E::SELECTOR))
}

/// Classify a `claimAsset` simulation/submission revert. Matches both the raw
/// 4-byte custom-error selector (what a node returns when it can't decode the
/// ABI) and the decoded error name (when it can), so it's robust either way.
pub fn classify_revert(err_text: &str) -> Readiness {
    let lower = err_text.to_lowercase();
    let contains = |needle: &str| lower.contains(&needle.to_lowercase());

    if contains("globalexitrootinvalid") || contains(&selector_hex::<GlobalExitRootInvalid>()) {
        Readiness::NotReadyRetry
    } else if contains("alreadyclaimed") || contains(&selector_hex::<AlreadyClaimed>()) {
        Readiness::AlreadyClaimed
    } else {
        Readiness::Permanent(err_text.to_string())
    }
}

/// A discovered, not-yet-claimed L2→L1 exit (decoded from a `BridgeEvent` log).
#[derive(Debug, Clone)]
pub struct PendingExit {
    pub leaf_index: u32,
    pub origin_network: u32,
    pub origin_address: Address,
    pub destination_network: u32,
    pub destination_address: Address,
    pub amount: U256,
    pub metadata: Bytes,
    pub block_number: u64,
}

impl PendingExit {
    /// Decode a `BridgeEvent` log into a `PendingExit`. Returns `None` for
    /// events that are not L2→L1 asset exits we should claim (wrong leaf type
    /// or destination network), so the caller can `filter_map`.
    fn from_bridge_event(ev: BridgeEvent, block_number: u64) -> Option<Self> {
        // leafType 0 == asset. Message bridging (leafType 1) needs an
        // authorized-address allowlist and is out of scope for v1.
        if ev.leafType != 0 {
            return None;
        }
        Some(PendingExit {
            leaf_index: ev.depositCount,
            origin_network: ev.originNetwork,
            origin_address: ev.originAddress,
            destination_network: ev.destinationNetwork,
            destination_address: ev.destinationAddress,
            amount: ev.amount,
            metadata: ev.metadata,
            block_number,
        })
    }
}

/// The merkle-proof bundle the L1 `claimAsset` needs, parsed from the
/// bridge-service `/merkle-proof` response.
#[derive(Debug, Clone)]
pub struct ProofBundle {
    pub main_exit_root: FixedBytes<32>,
    pub rollup_exit_root: FixedBytes<32>,
    pub smt_local: [FixedBytes<32>; 32],
    pub smt_rollup: [FixedBytes<32>; 32],
}

#[derive(serde::Deserialize)]
struct MerkleProofResponse {
    proof: MerkleProofInner,
}

#[derive(serde::Deserialize)]
struct MerkleProofInner {
    main_exit_root: String,
    rollup_exit_root: String,
    merkle_proof: Vec<String>,
    rollup_merkle_proof: Vec<String>,
}

fn parse_b32(s: &str) -> anyhow::Result<FixedBytes<32>> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| anyhow::anyhow!("invalid hex '{s}': {e}"))?;
    if bytes.len() != 32 {
        anyhow::bail!("expected 32 bytes, got {} for '{s}'", bytes.len());
    }
    Ok(FixedBytes::<32>::from_slice(&bytes))
}

/// Parse an SMT proof array, padding to exactly 32 siblings with zero (the
/// bridge-service may return fewer than 32 for shallow trees; `claimAsset`
/// expects a fixed `bytes32[32]`). Mirrors the padding in
/// `scripts/e2e-l2-to-l1.sh`.
fn parse_smt_array(arr: &[String]) -> anyhow::Result<[FixedBytes<32>; 32]> {
    if arr.len() > 32 {
        anyhow::bail!("SMT proof has {} siblings, > 32", arr.len());
    }
    let mut out = [FixedBytes::<32>::ZERO; 32];
    for (i, s) in arr.iter().enumerate() {
        out[i] = parse_b32(s)?;
    }
    Ok(out)
}

impl ProofBundle {
    fn from_response(resp: MerkleProofResponse) -> anyhow::Result<Self> {
        Ok(ProofBundle {
            main_exit_root: parse_b32(&resp.proof.main_exit_root)?,
            rollup_exit_root: parse_b32(&resp.proof.rollup_exit_root)?,
            smt_local: parse_smt_array(&resp.proof.merkle_proof)?,
            smt_rollup: parse_smt_array(&resp.proof.rollup_merkle_proof)?,
        })
    }
}

/// Assemble the `claimAsset` call for an L2→L1 exit. Pure — all dynamic inputs
/// (`exit`, `proof`) plus our `network_id` fully determine the call.
pub fn build_claim_call(
    exit: &PendingExit,
    proof: &ProofBundle,
    network_id: u32,
) -> claimAssetCall {
    claimAssetCall {
        smtProofLocalExitRoot: proof.smt_local,
        smtProofRollupExitRoot: proof.smt_rollup,
        globalIndex: our_global_index(network_id, exit.leaf_index),
        mainnetExitRoot: proof.main_exit_root,
        rollupExitRoot: proof.rollup_exit_root,
        originNetwork: exit.origin_network,
        originTokenAddress: exit.origin_address,
        destinationNetwork: exit.destination_network,
        destinationAddress: exit.destination_address,
        amount: exit.amount,
        metadata: exit.metadata.clone(),
    }
}

// ─── Cursor store (block cursor; isClaimed is the real guard) ─────────────────

/// Persists the last L2 block we've scanned for `BridgeEvent`s, so a restart
/// resumes instead of re-scanning from genesis. Not a claim ledger — the
/// authoritative double-spend guard is the on-chain `isClaimed` check.
pub struct CursorStore {
    conn: Mutex<rusqlite::Connection>,
}

impl CursorStore {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS cursor (id INTEGER PRIMARY KEY CHECK (id = 1), last_block INTEGER NOT NULL)",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get(&self) -> anyhow::Result<Option<u64>> {
        let conn = self.conn.lock().unwrap();
        let v: Option<i64> = conn
            .query_row("SELECT last_block FROM cursor WHERE id = 1", [], |r| {
                r.get(0)
            })
            .ok();
        Ok(v.map(|n| n as u64))
    }

    pub fn set(&self, block: u64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cursor (id, last_block) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET last_block = excluded.last_block",
            [block as i64],
        )?;
        Ok(())
    }
}

// ─── I/O ──────────────────────────────────────────────────────────────────

/// Decode raw `BridgeEvent` logs into our L2→L1 (destination network 0) exits.
fn collect_exits(logs: Vec<alloy::rpc::types::Log>, out: &mut Vec<PendingExit>) {
    for log in logs {
        let block = log.block_number.unwrap_or(0);
        match BridgeEvent::decode_log_data(log.data()) {
            Ok(ev) => {
                if let Some(exit) = PendingExit::from_bridge_event(ev, block) {
                    // L2→L1 only: destination is L1 (network 0).
                    if exit.destination_network == 0 {
                        out.push(exit);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, block, "failed to decode BridgeEvent log; skipping");
            }
        }
    }
}

/// Inclusive numeric `[from, to]` windows covering `[from, head]`, each spanning
/// at most `max_range` blocks. The trailing `(head, latest]` window is handled
/// separately by `discover` (it must use the `latest` tag, see below). Returns
/// empty when `from > head` (cursor already at/after the numeric head).
///
/// Pure (no I/O) so the windowing — the part that has to respect the proxy's
/// getLogs cap — is unit-tested.
fn scan_windows(from: u64, head: u64, max_range: u64) -> Vec<(u64, u64)> {
    let step = max_range.max(1);
    let mut windows = Vec::new();
    let mut cur = from;
    while cur <= head {
        // span (to - from) == step - 1, strictly below `max_range`.
        let to = cur.saturating_add(step - 1).min(head);
        windows.push((cur, to));
        cur = to + 1;
    }
    windows
}

/// Discover our rollup's L2→L1 asset exits from block `from` onward, from the
/// proxy's synthetic `BridgeEvent` logs.
///
/// The miden-agglayer proxy caps `eth_getLogs` at `MAX_GETLOGS_BLOCK_RANGE`
/// (10_000 blocks) and rejects wider spans. A single `from -> latest` query (the
/// previous behaviour) therefore failed whenever `(tip - from)` exceeded the cap
/// — on a fresh cursor (`from = 0`) that was *every* poll (PRST-4030). We chunk
/// the scan into `<= max_range` windows instead.
///
/// The upper bound is still the `latest` tag for the final window, NOT
/// `eth_blockNumber`: the proxy's `eth_blockNumber` mirror can lag the
/// synthetic-log tip (it is advanced by some write paths but not the bridge-out
/// synthetic-log path). So we scan the bulk `[from, head]` in numeric windows,
/// then finish with one `latest`-bounded window to capture logs beyond the
/// numeric head. That trailing span is just the (small) lag; each numeric window
/// is `< max_range`, which the caller keeps below the proxy cap.
async fn discover<P: Provider>(
    l2: &P,
    bridge_address: Address,
    from: u64,
    max_range: u64,
) -> anyhow::Result<Vec<PendingExit>> {
    let head = l2.get_block_number().await?;
    let mut out = Vec::new();

    // Bulk catch-up: numeric windows up to the (possibly lagging) numeric head.
    for (w_from, w_to) in scan_windows(from, head, max_range) {
        let filter = Filter::new()
            .address(bridge_address)
            .from_block(w_from)
            .to_block(w_to)
            .event_signature(BridgeEvent::SIGNATURE_HASH);
        let logs = l2.get_logs(&filter).await?;
        tracing::debug!(
            from = w_from,
            to = w_to,
            raw_logs = logs.len(),
            "discover: numeric window"
        );
        collect_exits(logs, &mut out);
    }

    // Trailing window to the true tip via the `latest` tag, to catch synthetic
    // logs the numeric head doesn't yet reflect. `tail_from` is `head + 1` after
    // the loop, or `from` when `from > head` (loop produced no windows).
    let tail_from = from.max(head.saturating_add(1));
    let filter = Filter::new()
        .address(bridge_address)
        .from_block(tail_from)
        .to_block(BlockNumberOrTag::Latest)
        .event_signature(BridgeEvent::SIGNATURE_HASH);
    let logs = l2.get_logs(&filter).await?;
    tracing::debug!(from = tail_from, %bridge_address, raw_logs = logs.len(), "discover: latest tail");
    collect_exits(logs, &mut out);

    Ok(out)
}

/// On-chain authoritative already-claimed check.
async fn is_claimed<P: Provider>(
    l1: &P,
    bridge: Address,
    leaf_index: u32,
    source_network: u32,
) -> anyhow::Result<bool> {
    let call = isClaimedCall {
        leafIndex: leaf_index,
        sourceBridgeNetwork: source_network,
    };
    let ret = l1
        .call(
            TransactionRequest::default()
                .to(bridge)
                .input(call.abi_encode().into()),
        )
        .await?;
    // bool return is a 32-byte word; non-zero == true.
    Ok(ret.iter().any(|b| *b != 0))
}

/// Fetch the merkle proof for `leaf_index` from the bridge-service GetClaim path.
async fn fetch_proof(
    http: &reqwest::Client,
    base_url: &str,
    leaf_index: u32,
    network_id: u32,
) -> anyhow::Result<ProofBundle> {
    let url = format!(
        "{}/merkle-proof?deposit_cnt={}&net_id={}",
        base_url.trim_end_matches('/'),
        leaf_index,
        network_id
    );
    let resp = http.get(&url).send().await?.error_for_status()?;
    let parsed: MerkleProofResponse = resp.json().await?;
    ProofBundle::from_response(parsed)
}

/// Pre-flight `eth_call` simulation of `claimAsset`. This is how decision (b) is
/// implemented: a transient `GlobalExitRootInvalid` revert (GER not settled
/// yet) becomes `NotReadyRetry` instead of a wasted on-chain send.
async fn simulate<P: Provider>(
    l1: &P,
    bridge: Address,
    from: Address,
    call: &claimAssetCall,
) -> Readiness {
    let tx = TransactionRequest::default()
        .from(from)
        .to(bridge)
        .input(call.abi_encode().into());
    match l1.call(tx).await {
        Ok(_) => Readiness::Ready,
        Err(e) => classify_revert(&e.to_string()),
    }
}

/// Submit the signed `claimAsset` tx and wait for the receipt. Returns the tx
/// hash on success; errors (including a status-0 receipt) propagate.
async fn submit<P: Provider>(
    l1: &P,
    bridge: Address,
    call: &claimAssetCall,
) -> anyhow::Result<FixedBytes<32>> {
    let tx = TransactionRequest::default()
        .to(bridge)
        .input(call.abi_encode().into());
    let pending = l1.send_transaction(tx).await?;
    let receipt = pending.get_receipt().await?;
    if !receipt.status() {
        anyhow::bail!(
            "claimAsset tx {} reverted on-chain",
            receipt.transaction_hash
        );
    }
    Ok(receipt.transaction_hash)
}

/// Process one exit end-to-end: gate on isClaimed, fetch proof, simulate, submit.
///
/// Returns `true` when the exit is RESOLVED (claimed now, already claimed, or a
/// permanent failure we won't keep retrying) so the caller may advance the block
/// cursor past it; `false` when it is merely NOT-READY-YET (proof not synced /
/// GER not settled on L1) and must be re-discovered on a later poll. The caller
/// holds the cursor below any not-ready exit so re-discovery actually happens —
/// advancing unconditionally would skip the block forever and the exit would
/// never be claimed.
async fn process_exit<P1: Provider, P2: Provider>(
    l1: &P1,
    _l2: &P2,
    http: &reqwest::Client,
    cfg: &ClaimerConfig,
    sponsor: Address,
    exit: &PendingExit,
) -> anyhow::Result<bool> {
    let src_net = source_bridge_network(cfg.network_id);

    if is_claimed(l1, cfg.bridge_address, exit.leaf_index, src_net).await? {
        tracing::debug!(leaf = exit.leaf_index, "already claimed on L1; skipping");
        return Ok(true);
    }

    let proof = match fetch_proof(
        http,
        &cfg.bridge_service_url,
        exit.leaf_index,
        cfg.network_id,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            // The exit was discovered from the proxy's log, but the
            // bridge-service may not have synced/derived its proof yet. Not
            // ready: hold the cursor so we re-discover it next poll.
            tracing::info!(leaf = exit.leaf_index, error = %e, "merkle-proof not available yet; will retry");
            return Ok(false);
        }
    };

    let call = build_claim_call(exit, &proof, cfg.network_id);

    let resolved = match simulate(l1, cfg.bridge_address, sponsor, &call).await {
        Readiness::Ready => {
            let tx = submit(l1, cfg.bridge_address, &call).await?;
            tracing::info!(
                leaf = exit.leaf_index,
                dest = %exit.destination_address,
                amount = %exit.amount,
                tx = %tx,
                "claimed L2->L1 exit on L1"
            );
            metrics::counter!("bridge_autoclaim_claims_total").increment(1);
            true
        }
        Readiness::NotReadyRetry => {
            tracing::info!(
                leaf = exit.leaf_index,
                "GER not settled yet; will retry next poll"
            );
            metrics::counter!("bridge_autoclaim_not_ready_total").increment(1);
            false
        }
        Readiness::AlreadyClaimed => {
            tracing::debug!(
                leaf = exit.leaf_index,
                "simulation says already claimed; skipping"
            );
            true
        }
        Readiness::Permanent(err) => {
            // Doomed: advance past it rather than re-scanning every poll forever.
            tracing::error!(leaf = exit.leaf_index, error = %err, "claim simulation failed permanently; skipping");
            metrics::counter!("bridge_autoclaim_permanent_failures_total").increment(1);
            true
        }
    };
    Ok(resolved)
}

/// Run the claimer poll loop until cancelled (Ctrl-C / SIGTERM).
pub async fn run(cfg: ClaimerConfig) -> anyhow::Result<()> {
    let signer: PrivateKeySigner = cfg.sponsor_key.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid sponsor private key (from --sponsor-key-env); refusing to log the value"
        )
    })?;
    let sponsor = signer.address();
    let wallet = EthereumWallet::from(signer);

    let l2 = ProviderBuilder::new().connect_http(cfg.l2_rpc_url.parse()?);
    let l1 = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(cfg.l1_rpc_url.parse()?);
    let http = reqwest::Client::new();
    let cursor = CursorStore::open(&cfg.cursor_db_path)?;

    tracing::info!(
        l2_rpc = %cfg.l2_rpc_url,
        l1_rpc = %cfg.l1_rpc_url,
        bridge = %cfg.bridge_address,
        network_id = cfg.network_id,
        sponsor = %sponsor,
        poll_interval_s = cfg.poll_interval.as_secs(),
        "bridge-autoclaim starting"
    );

    // Resolve the starting cursor: explicit --start-block override, else the
    // persisted cursor, else 0 (full scan; the proxy's synthetic block space is
    // small, so this is cheap on a fresh deployment).
    let mut last_processed: u64 = match cfg.start_block {
        Some(b) => b.saturating_sub(1),
        None => cursor.get()?.unwrap_or(0),
    };

    let mut ticker = tokio::time::interval(cfg.poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown requested");
                break;
            }
            _ = ticker.tick() => {}
        }

        if let Err(e) =
            poll_once(&l1, &l2, &http, &cfg, sponsor, &cursor, &mut last_processed).await
        {
            tracing::warn!(error = %e, last_processed, "poll failed; retrying next tick");
            metrics::counter!("bridge_autoclaim_poll_errors_total").increment(1);
        }
    }
    Ok(())
}

async fn poll_once<P1: Provider, P2: Provider>(
    l1: &P1,
    l2: &P2,
    http: &reqwest::Client,
    cfg: &ClaimerConfig,
    sponsor: Address,
    cursor: &CursorStore,
    last_processed: &mut u64,
) -> anyhow::Result<()> {
    let from = *last_processed + 1;
    // Chunked scan (see `discover`): numeric windows up to the head + a final
    // `latest`-bounded window, each <= cfg.max_range to respect the proxy's
    // eth_getLogs cap (PRST-4030).
    let exits = discover(l2, cfg.bridge_address, from, cfg.max_range).await?;
    if !exits.is_empty() {
        tracing::info!(from, count = exits.len(), "discovered L2->L1 exits");
    }
    // Track the lowest block of any exit that is NOT yet resolved (proof not
    // synced / GER not settled / transient RPC error) — the cursor must not pass
    // it or that block is never re-scanned and the exit is never claimed — and
    // the highest block we DID resolve, so we can advance past settled work.
    let mut retry_from: Option<u64> = None;
    let mut max_resolved: u64 = *last_processed;
    for exit in &exits {
        let resolved = match process_exit(l1, l2, http, cfg, sponsor, exit).await {
            Ok(resolved) => resolved,
            Err(e) => {
                // Transient error (e.g. RPC) — treat as not-ready and retry.
                tracing::warn!(leaf = exit.leaf_index, error = %e, "failed to process exit");
                metrics::counter!("bridge_autoclaim_exit_errors_total").increment(1);
                false
            }
        };
        if resolved {
            max_resolved = max_resolved.max(exit.block_number);
        } else {
            retry_from = Some(retry_from.map_or(exit.block_number, |b| b.min(exit.block_number)));
        }
    }

    // Advance only up to (but not past) the first not-ready exit, so it is
    // re-discovered next poll; otherwise advance past everything we resolved.
    // Never move backwards (no logs / all not-ready => cursor stays put and the
    // range is cheaply re-scanned; isClaimed makes re-processing idempotent).
    let new_cursor = match retry_from {
        Some(b) => b.saturating_sub(1).max(*last_processed),
        None => max_resolved,
    };
    if new_cursor > *last_processed {
        *last_processed = new_cursor;
        if let Err(e) = cursor.set(new_cursor) {
            tracing::warn!(error = %e, cursor = new_cursor, "failed to persist cursor; continuing in-memory");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scan_windows: the chunking that must respect the proxy getLogs cap ──

    /// Every numeric window spans strictly less than `max_range` blocks, so each
    /// `eth_getLogs` stays under the proxy's MAX_GETLOGS_BLOCK_RANGE cap. This is
    /// the regression guard for PRST-4030 (a single `0 -> latest` query, span
    /// ~196k, was rejected on every poll).
    #[test]
    fn scan_windows_each_under_max_range_and_contiguous() {
        let from = 0;
        let head = 196_476; // ~the L2 synthetic head that triggered the bug
        let max_range = 10_000;
        let ws = scan_windows(from, head, max_range);
        assert_eq!(ws.first().unwrap().0, from);
        assert_eq!(ws.last().unwrap().1, head, "windows must cover up to head");
        let mut expected_next = from;
        for (f, t) in &ws {
            assert_eq!(*f, expected_next, "windows must be contiguous, no gaps");
            assert!(t >= f);
            assert!(
                t - f < max_range,
                "span {} must be < cap {max_range}",
                t - f
            );
            expected_next = t + 1;
        }
    }

    /// `from > head` (cursor at/after the numeric head) yields no numeric
    /// windows — `discover` then does only the trailing `latest` query.
    #[test]
    fn scan_windows_empty_when_from_past_head() {
        assert!(scan_windows(500, 499, 10_000).is_empty());
        assert!(scan_windows(1, 0, 10_000).is_empty());
    }

    /// A span that fits in one window produces exactly one `[from, head]` window.
    #[test]
    fn scan_windows_single_small_span() {
        assert_eq!(scan_windows(100, 600, 10_000), vec![(100, 600)]);
    }

    /// `max_range = 0` must not divide-by-zero / loop forever; clamps to 1.
    #[test]
    fn scan_windows_zero_max_range_is_safe() {
        let ws = scan_windows(0, 3, 0);
        assert_eq!(ws, vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn global_index_layout_rollup() {
        // network_id 76 -> rollup_index 75, leaf 23.
        let gi = our_global_index(76, 23);
        let bytes = gi.to_be_bytes::<32>();
        // leaf index = bytes 28..32
        assert_eq!(u32::from_be_bytes(bytes[28..32].try_into().unwrap()), 23);
        // rollup index = bytes 24..28
        assert_eq!(u32::from_be_bytes(bytes[24..28].try_into().unwrap()), 75);
        // mainnet flag = bytes 20..24 -> 0 for a rollup exit
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 0);
    }

    #[test]
    fn global_index_kurtosis_network_1_is_leaf() {
        // network_id 1 -> rollup_index 0 -> globalIndex == leaf index.
        assert_eq!(our_global_index(1, 5), U256::from(5u64));
    }

    #[test]
    fn global_index_mainnet_flag_set() {
        let gi = global_index(true, 0, 42);
        let bytes = gi.to_be_bytes::<32>();
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(bytes[28..32].try_into().unwrap()), 42);
    }

    #[test]
    fn source_bridge_network_is_our_network_id() {
        // Distinct from the asset origin network (0 for ETH).
        assert_eq!(source_bridge_network(76), 76);
    }

    #[test]
    fn classify_ger_not_settled_is_retry() {
        // By decoded name...
        assert_eq!(
            classify_revert("execution reverted: GlobalExitRootInvalid()"),
            Readiness::NotReadyRetry
        );
        // ...and by raw selector hex.
        let sel = format!(
            "reverted, data: \"{}\"",
            selector_hex::<GlobalExitRootInvalid>()
        );
        assert_eq!(classify_revert(&sel), Readiness::NotReadyRetry);
    }

    #[test]
    fn classify_already_claimed() {
        assert_eq!(
            classify_revert("AlreadyClaimed()"),
            Readiness::AlreadyClaimed
        );
        let sel = selector_hex::<AlreadyClaimed>();
        assert_eq!(classify_revert(&sel), Readiness::AlreadyClaimed);
    }

    #[test]
    fn classify_unknown_is_permanent() {
        match classify_revert("execution reverted: InvalidSmtProof()") {
            Readiness::Permanent(_) => {}
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn parse_smt_array_pads_to_32() {
        let arr = vec![
            "0x".to_string() + &"11".repeat(32),
            "0x".to_string() + &"22".repeat(32),
        ];
        let out = parse_smt_array(&arr).unwrap();
        assert_eq!(out[0], FixedBytes::<32>::from([0x11u8; 32]));
        assert_eq!(out[1], FixedBytes::<32>::from([0x22u8; 32]));
        assert_eq!(out[2], FixedBytes::<32>::ZERO);
        assert_eq!(out[31], FixedBytes::<32>::ZERO);
    }

    #[test]
    fn parse_smt_array_rejects_over_32() {
        let arr: Vec<String> = (0..33)
            .map(|_| "0x".to_string() + &"00".repeat(32))
            .collect();
        assert!(parse_smt_array(&arr).is_err());
    }

    #[test]
    fn parse_b32_rejects_wrong_length() {
        assert!(parse_b32("0x1234").is_err());
        assert!(parse_b32(&("0x".to_string() + &"ab".repeat(32))).is_ok());
    }

    #[test]
    fn cursor_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bac-cursor-test-{}.sqlite", std::process::id()));
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(path_str);
        let store = CursorStore::open(path_str).unwrap();
        assert_eq!(store.get().unwrap(), None);
        store.set(100).unwrap();
        assert_eq!(store.get().unwrap(), Some(100));
        store.set(250).unwrap();
        assert_eq!(store.get().unwrap(), Some(250));
        let _ = std::fs::remove_file(path_str);
    }
}
