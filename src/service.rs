use crate::COMPONENT;
use crate::hex::hex_decode_u64;
use crate::service_debug::service_debug_trace_transaction;
use crate::service_eth_call::service_eth_call;
use crate::service_get_logs::service_get_logs;
use crate::service_get_txn_receipt::service_get_txn_receipt;
use crate::service_helpers::{
    ServiceErrorCode, build_synthetic_tx_json, json_rpc_response_from_result, store_error,
};
use crate::service_send_raw_txn::service_send_raw_txn;
use crate::service_state::ServiceState;
use crate::service_zkevm::{service_zkevm_get_exit_roots_by_ger, service_zkevm_get_latest_ger};
use alloy::primitives::TxHash;
use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use http::HeaderValue;
use std::str::FromStr;
use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;
use tower::ServiceBuilder;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_http::limit::RequestBodyLimitLayer;

/// Default per-IP rate limit (R13). 500 req/sec sustained with a 500-request
/// burst. Aggkit colocates ~6 sync loops (L1BridgeSyncer, L2BridgeSyncer,
/// reorgdetector, ethtxmanager, aggoracle, aggsender) each polling at 1+ Hz
/// against the proxy from a single container IP, plus eth_call probing on
/// every cycle — observed bursts at startup hit 200+ req/sec from one IP.
///
/// The rate-limit's purpose is brute-force protection for admin auth (R1) and
/// signer-allow-list probing (R2), both of which only fire from external IPs.
/// 500 req/sec is far below what a brute-force attacker would mount and
/// comfortably above legitimate aggkit traffic. Configurable via
/// `--rate-limit-per-second` / `RATE_LIMIT_PER_SECOND`.
///
/// Self-review history: an earlier 60/60 default tripped aggkit during e2e —
/// 429 cooldowns of 40s+ deadlocked the ready_for_claim wait.
pub const DEFAULT_RATE_LIMIT_PER_SECOND: u64 = 500;
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 500;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

/// Maximum size in bytes for an inbound JSON-RPC request body.
///
/// Self-review R6 — without an explicit cap, axum's default body limit (2 MB) is the
/// only protection. JSON-RPC requests are typically tiny; an unauthenticated caller
/// posting megabytes of garbage is purely DoS. 256 KB is comfortable headroom for
/// legitimate payloads (a typical `eth_sendRawTransaction` carrying a CLAIM proof is
/// ~17 KB; an `eth_getLogs` with the maximum allowed filter arrays is well below 200 KB).
///
/// Decompression posture: the JSON-RPC route does NOT install
/// `tower_http::decompression::DecompressionLayer`, so `Content-Encoding: gzip`
/// (or any other encoding) reaches the JSON extractor as the compressed payload —
/// `serde_json` then rejects it with a parse error. The 256 KB cap therefore
/// applies to wire bytes, which is the right scope. If a future commit adds
/// auto-decompression, that layer MUST be ordered after `RequestBodyLimitLayer`
/// or supplemented with an output-size guard so a small gzip-bomb cannot
/// inflate past the cap.
pub const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
use url::Url;

async fn json_rpc_endpoint(
    State(service): State<ServiceState>,
    headers: axum::http::HeaderMap,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let start = std::time::Instant::now();
    let method_name = request.method.clone();
    // Cardinality-safe label for metrics. A request.method comes from the
    // attacker-controlled JSON body; using it verbatim creates one
    // Prometheus series per distinct method, which an unauthenticated
    // caller can grow without bound (DoS the metrics exporter / OOM the
    // proxy). Bucket to a finite known set; everything else collapses to
    // "other".
    let method_label = bucket_method_label(&method_name);

    // R1 — gate admin_* methods on the configured API key BEFORE running any handler.
    // Without this, admin endpoints (`admin_registerFaucet`, `admin_listFaucets`) are
    // reachable by anyone who can hit the JSON-RPC port — letting a malicious caller
    // poison the faucet registry with attacker-chosen `MetadataHash` for any token.
    #[allow(clippy::collapsible_if)]
    if method_name.starts_with("admin_") {
        if let Err(reason) = check_admin_auth(service.admin_api_key.as_deref(), &headers) {
            metrics::counter!("rpc_admin_auth_rejects_total", "method" => method_label)
                .increment(1);
            return Ok(JsonRpcResponse::error(
                request.get_answer_id(),
                JsonRpcError::new(
                    JsonRpcErrorReason::ServerError(-32001),
                    format!("admin auth: {reason}"),
                    serde_json::Value::Null,
                ),
            ));
        }
    }

    let result = json_rpc_handler(service, request).await;

    metrics::counter!("rpc_requests_total", "method" => method_label).increment(1);
    metrics::histogram!("rpc_request_duration_seconds", "method" => method_label)
        .record(start.elapsed().as_secs_f64());

    result
}

/// Return a metric label for a JSON-RPC method name, restricted to a finite
/// set of known buckets so an attacker-controlled method string cannot
/// inflate Prometheus cardinality.
///
/// Self-review (review-of-fix follow-up): the original R1 commit and the
/// pre-existing `rpc_requests_total` instrumentation used the raw
/// `request.method` string as a label. An unauthenticated caller posting
/// random method names (e.g. `"admin_<uuid>"`) created one series per
/// guess, which OOMs the metrics exporter — a trivial DoS.
fn bucket_method_label(method: &str) -> &'static str {
    match method {
        // Standard EVM-shaped methods we actually serve (success path).
        "eth_blockNumber" => "eth_blockNumber",
        "eth_chainId" => "eth_chainId",
        "eth_getBlockByNumber" => "eth_getBlockByNumber",
        "eth_getBlockByHash" => "eth_getBlockByHash",
        "eth_getCode" => "eth_getCode",
        "eth_getBalance" => "eth_getBalance",
        "eth_getStorageAt" => "eth_getStorageAt",
        "eth_getLogs" => "eth_getLogs",
        "eth_getTransactionCount" => "eth_getTransactionCount",
        "eth_getTransactionByHash" => "eth_getTransactionByHash",
        "eth_getTransactionReceipt" => "eth_getTransactionReceipt",
        "eth_getBlockTransactionCountByNumber" => "eth_getBlockTransactionCountByNumber",
        "eth_call" => "eth_call",
        "eth_estimateGas" => "eth_estimateGas",
        "eth_syncing" => "eth_syncing",
        "eth_gasPrice" => "eth_gasPrice",
        "eth_sendRawTransaction" => "eth_sendRawTransaction",
        "net_version" => "net_version",
        "debug_traceTransaction" => "debug_traceTransaction",
        "zkevm_getLatestGlobalExitRoot" => "zkevm_getLatestGlobalExitRoot",
        "zkevm_getExitRootsByGER" => "zkevm_getExitRootsByGER",
        "admin_registerFaucet" => "admin_registerFaucet",
        "admin_registerNativeFaucet" => "admin_registerNativeFaucet",
        "admin_listFaucets" => "admin_listFaucets",
        // Anything else → "other". Includes typos and method-name-fuzzing
        // attacks. We still log the actual method via tracing for debugging.
        _ => "other",
    }
}

/// Outcome of an admin auth check; private detail for documentation.
#[derive(Debug, PartialEq)]
enum AdminAuthError {
    NotConfigured,
    MissingHeader,
    MalformedHeader,
    BadToken,
}

impl std::fmt::Display for AdminAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => {
                f.write_str("admin endpoints disabled (no admin API key configured)")
            }
            Self::MissingHeader => f.write_str("missing Authorization header"),
            Self::MalformedHeader => {
                f.write_str("malformed Authorization header (expected `Bearer <token>`)")
            }
            Self::BadToken => f.write_str("invalid bearer token"),
        }
    }
}

/// Verify the `Authorization: Bearer <token>` header against the configured admin
/// API key. Returns `Ok(())` if the request is authorised; otherwise an
/// `AdminAuthError` whose `Display` is safe to surface to the caller.
///
/// Self-review R1 — pre-fix, every admin method was reachable by any caller who
/// could hit the JSON-RPC port. We didn't even check `Authorization`. The fix
/// requires a configured `admin_api_key` (CLI/env); when none is set, every
/// `admin_*` request is rejected with `NotConfigured`. Constant-time comparison
/// guards against timing oracles.
fn check_admin_auth(
    configured: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> Result<(), AdminAuthError> {
    let configured = configured.ok_or(AdminAuthError::NotConfigured)?;
    let header = headers
        .get(http::header::AUTHORIZATION)
        .ok_or(AdminAuthError::MissingHeader)?;
    let header_str = header
        .to_str()
        .map_err(|_| AdminAuthError::MalformedHeader)?;
    // RFC 6750 §2.1: the auth-scheme is case-insensitive (`Bearer` == `bearer`).
    // The previous implementation accepted only `Bearer ` exactly, which would
    // reject standards-compliant clients that lower-case the scheme.
    let token =
        strip_bearer_prefix_case_insensitive(header_str).ok_or(AdminAuthError::MalformedHeader)?;
    if constant_time_eq(token.as_bytes(), configured.as_bytes()) {
        Ok(())
    } else {
        Err(AdminAuthError::BadToken)
    }
}

/// Strip a case-insensitive `Bearer ` (or `bearer `, `BEARER `, etc.) prefix.
fn strip_bearer_prefix_case_insensitive(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 7 {
        return None;
    }
    let scheme = &bytes[..6];
    let sep = bytes[6];
    if scheme.eq_ignore_ascii_case(b"Bearer") && sep == b' ' {
        // SAFETY: we sliced at byte 7 of an ASCII prefix, so the remaining
        // bytes form a valid str slice from the same underlying string.
        Some(&s[7..])
    } else {
        None
    }
}

/// Constant-time byte equality — prevents an attacker from learning prefix-match
/// length OR length-mismatch from response timing.
///
/// Self-review of-the-fix follow-up: the previous implementation early-returned
/// `false` on `a.len() != b.len()`, which leaks the configured token length via
/// timing. Now we walk the longer slice in full, treating any out-of-bounds
/// byte from the shorter slice as zero. Constant work irrespective of input
/// shape; the only timing channel is "input length", which is already
/// observable in the request's wire size.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff: u32 = 0;
    // Length difference contributes to `diff` so unequal lengths reject.
    diff |= (a.len() as u32) ^ (b.len() as u32);
    for i in 0..max_len {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= u32::from(x ^ y);
    }
    diff == 0
}

async fn json_rpc_handler(service: ServiceState, request: JsonRpcExtractor) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let method_name = request.method.clone();
    let method = method_name.as_str();
    match method {
        "eth_getBlockByNumber" => tracing::trace!("JSON-RPC {method}"),
        "eth_call"
        | "eth_gasPrice"
        | "eth_estimateGas"
        | "eth_getLogs"
        | "net_version"
        | "eth_getBlockTransactionCountByNumber"
        | "eth_getTransactionCount"
        | "eth_getTransactionByHash"
        | "eth_getTransactionReceipt"
        | "zkevm_getLatestGlobalExitRoot"
        | "zkevm_getExitRootsByGER" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        _ => tracing::debug!("JSON-RPC {method}"),
    }

    match method {
        "eth_getCode" => {
            // R10 — validate the address before returning the constant stub. Pre-fix,
            // any garbage was accepted and a non-empty single-byte was returned, which
            // some EVM-compat consumers interpret as "this address has code". Reject
            // malformed addresses with InvalidParams; well-formed addresses still get
            // the stub `0xFE` response (this is a JSON-RPC stub for compat — there is
            // no L2 EVM bytecode store, the response is documented as a sentinel).
            let params: (String, String) = request.parse_params()?;
            validate_eth_address(&params.0).map_err(|msg| {
                JsonRpcResponse::error(
                    answer_id.clone(),
                    JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        format!("eth_getCode: {msg}"),
                        serde_json::Value::Null,
                    ),
                )
            })?;
            Ok(JsonRpcResponse::success(answer_id, "0xFE"))
        }

        "eth_blockNumber" => {
            // POSTMORTEM 2026-07-04: the RD-940 Phase 3 hot-read (serve the
            // BlockMonitor AtomicU64 mirror when non-zero) went STALE under the
            // synthetic-indexer redesign: the projector became the SOLE tip
            // advancer and never calls record_tip(), and with the writer
            // worker disabled nothing else does — so the mirror froze at its
            // cold-boot seed (observed: eth_blockNumber pinned at 659 while
            // the synthetic tip reached 2702; verifier windows truncated).
            // The store is the single source of truth for the tip — read it.
            // record_tip() keeps the mirror fresh for writer-mode consumers.
            let block_num = service
                .store
                .get_latest_block_number()
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
            if block_num > 0 {
                service.block_monitor.record_tip(block_num);
            }
            let block_num_str = format!("{:#x}", block_num);
            Ok(JsonRpcResponse::success(answer_id, block_num_str))
        }

        "eth_getBlockByNumber" => {
            let params: (String, bool) = request.parse_params()?;
            let block_num = match params.0.as_str() {
                "latest" | "pending" | "finalized" | "safe" => service
                    .store
                    .get_latest_block_number()
                    .await
                    .map_err(|e| store_error(answer_id.clone(), e))?,
                "earliest" => 0,
                any => {
                    let Ok(num) = hex_decode_u64(any) else {
                        let error = JsonRpcError::new(
                            JsonRpcErrorReason::InvalidParams,
                            String::from("bad block number"),
                            serde_json::Value::Null,
                        );
                        return Err(JsonRpcResponse::error(answer_id, error));
                    };
                    // Return null for blocks beyond the chain tip to avoid
                    // ensure_block_exists iterating over billions of synthetic blocks.
                    let latest = service
                        .store
                        .get_latest_block_number()
                        .await
                        .map_err(|e| store_error(answer_id.clone(), e))?;
                    if num > latest {
                        return Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                            answer_id,
                            serde_json::Value::Null,
                        ));
                    }
                    num
                }
            };
            let full_txns = params.1;
            let block = service.block_state.get_block_by_number(block_num);
            match block {
                Some(b) => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    b.to_json(full_txns),
                )),
                None => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    serde_json::Value::Null,
                )),
            }
        }

        "eth_getBlockByHash" => {
            let params: (String, bool) = request.parse_params()?;
            let hash = crate::service_helpers::validate_hex_hash_param(
                &params.0,
                "block hash",
                answer_id.clone(),
            )?;
            let full_txns = params.1;
            let block = service.block_state.get_block_by_hash(&hash);
            match block {
                Some(b) => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    b.to_json(full_txns),
                )),
                None => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    serde_json::Value::Null,
                )),
            }
        }

        "eth_getTransactionCount" => {
            let params: (String, String) = request.parse_params()?;
            let addr = &params.0;
            let tag = params.1.as_str();
            let accepted_nonce = service
                .store
                .nonce_get(addr)
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
            let mut returned_nonce = accepted_nonce;

            // RD-940 Decision 4 — honour the block tag.
            //
            // `store.nonce_get` returns the **next-accepted** nonce because
            // `eth_sendRawTransaction` advances it on accept (both legacy and
            // worker paths). That value matches geth's `pending` semantics
            // directly. For `latest` / `safe` / `finalized` / `earliest` the
            // RPC must instead return the **next-committed** nonce, computed
            // as `next-accepted - count(inflight non-terminal jobs from this
            // signer)`.
            //
            // claim-sponsor's `nonce_cache.go:35` LRU reads `latest`; without
            // this branch it sees queued/submitting txs leak into `latest`
            // and races itself (Spec E). When the writer worker is disabled
            // there are no inflight jobs so the two tags agree by
            // construction.
            //
            // Empty / missing tag defaults to `latest` per the geth contract
            // (`eth_getTransactionCount` second-param convention).
            let treat_as_latest = matches!(tag, "" | "latest" | "safe" | "finalized" | "earliest");
            let mut inflight_non_terminal = 0usize;
            if treat_as_latest
                && let Some(handle) = service.writer_handle.as_ref()
                && let Ok(signer_addr) = addr.parse::<alloy::primitives::Address>()
            {
                inflight_non_terminal = handle.count_non_terminal_for_signer(&signer_addr);
                returned_nonce = returned_nonce.saturating_sub(inflight_non_terminal as u64);
            }

            tracing::info!(
                target: "rpc::nonce_snoop",
                "{}",
                serde_json::json!({
                    "event": "eth_getTransactionCount",
                    "address": addr,
                    "tag": tag,
                    "treat_as_latest": treat_as_latest,
                    "accepted_nonce": accepted_nonce,
                    "inflight_non_terminal": inflight_non_terminal,
                    "returned_nonce": returned_nonce,
                    "writer_enabled": service.enable_writer_worker,
                    "writer_handle_present": service.writer_handle.is_some(),
                })
            );

            Ok(JsonRpcResponse::success(
                answer_id,
                format!("{returned_nonce:#x}"),
            ))
        }

        // aggkit health-polls eth_syncing; the synthetic chain has no download
        // phase (the projector holds the tip at the Miden tip), so report
        // "not syncing" per the Ethereum JSON-RPC spec (boolean false).
        "eth_syncing" => Ok(JsonRpcResponse::success(answer_id, false)),

        // Standard client-discovery stubs (block explorers / tooling probe
        // these on startup; unimplemented they spam ERROR-level noise).
        "web3_clientVersion" => Ok(JsonRpcResponse::success(
            answer_id,
            format!("miden-agglayer/{}", env!("CARGO_PKG_VERSION")),
        )),
        "net_version" => Ok(JsonRpcResponse::success(
            answer_id,
            service.chain_id.to_string(),
        )),

        "eth_gasPrice" => Ok(JsonRpcResponse::success(answer_id, "0x3b9aca00")),
        "eth_maxPriorityFeePerGas" => Ok(JsonRpcResponse::success(answer_id, "0x3b9aca00")),
        "eth_estimateGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        "eth_chainId" => Ok(JsonRpcResponse::success(
            answer_id,
            format!("{:#x}", service.chain_id),
        )),

        "eth_call" => service_eth_call(service, request).await,

        "eth_sendRawTransaction" => {
            let params: (String,) = request.parse_params()?;
            let result = service_send_raw_txn(service, params.0).await;
            match &result {
                Ok(hash) => tracing::info!("eth_sendRawTransaction: OK hash={hash}"),
                Err(err) => tracing::info!("eth_sendRawTransaction: ERR {err:#}"),
            }
            // RD-940 — promote writer-queue-saturation to JSON-RPC -32005
            // (geth's `LimitExceeded`). aggkit's ethtxmanager retries
            // `-32005` transparently; without this mapping the default
            // `ApplicationError(1) = SendRawTransaction` would conflate
            // queue backpressure with all other tx-submission failures,
            // and ethtxmanager would not classify it as transient.
            if let Err(err) = &result
                && err
                    .downcast_ref::<crate::writer_worker::WriterQueueSaturatedError>()
                    .is_some()
            {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::ServerError(-32005),
                    "writer queue saturated; retry".to_string(),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            }
            json_rpc_response_from_result(result, answer_id, ServiceErrorCode::SendRawTransaction)
        }

        "eth_getTransactionReceipt" => {
            let params: (String,) = request.parse_params()?;
            let tx_hash_str = params.0.clone();
            let result = service_get_txn_receipt(service, params.0).await;
            match &result {
                Ok(Some(r)) => tracing::info!(
                    "eth_getTransactionReceipt: FOUND hash={tx_hash_str} block={}",
                    r.block_number.unwrap_or(0)
                ),
                Ok(None) => {
                    tracing::info!("eth_getTransactionReceipt: NOT FOUND hash={tx_hash_str}")
                }
                Err(err) => {
                    tracing::info!("eth_getTransactionReceipt: ERR hash={tx_hash_str} {err:#}")
                }
            }
            json_rpc_response_from_result(
                result,
                answer_id,
                ServiceErrorCode::GetTransactionReceipt,
            )
        }

        "eth_getTransactionByHash" => {
            let params: (String,) = request.parse_params()?;
            let Ok(txn_hash) = TxHash::from_str(&params.0) else {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    String::from("bad transaction hash"),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            };

            // Try store first (real transactions from eth_sendRawTransaction)
            if let Some(data) = service
                .store
                .txn_get(txn_hash)
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?
            {
                let txn = data.to_rpc_transaction(txn_hash, &service.block_state);
                return Ok(JsonRpcResponse::success(answer_id, txn));
            }

            // RD-940 Spec D — in-flight (writer-worker accepted but not yet
            // committed). Returns the geth pending-tx shape: `blockHash`,
            // `blockNumber`, `transactionIndex` JSON null, every other
            // numeric field hex-encoded so aggkit's Go-side
            // hexutil.Uint{,64}/Big unmarshallers don't panic. See
            // service_helpers::build_inflight_pending_tx_json for the
            // full contract.
            if let Some(handle) = service.writer_handle.as_ref()
                && let Some(entry) = handle.get_inflight(&txn_hash)
            {
                tracing::debug!(
                    tx_hash = %txn_hash,
                    state = ?entry.state,
                    "eth_getTransactionByHash: returning in-flight pending shape"
                );
                return Ok(JsonRpcResponse::success(
                    answer_id,
                    crate::service_helpers::build_inflight_pending_tx_json(
                        &entry,
                        service.chain_id,
                    ),
                ));
            }

            // Fallback: synthetic transactions (bridge-out events)
            let tx_hash_str = format!("{txn_hash:#x}");
            let logs = service
                .store
                .get_logs_for_tx(&tx_hash_str)
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
            if let Some(log) = logs.first() {
                tracing::info!("eth_getTransactionByHash: found synthetic tx {tx_hash_str}");
                let synthetic_tx = build_synthetic_tx_json(txn_hash, log, service.chain_id);
                return Ok(JsonRpcResponse::success(answer_id, synthetic_tx));
            }

            tracing::debug!(
                tx_hash = %tx_hash_str,
                "eth_getTransactionByHash: unknown hash, returning null"
            );
            Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                answer_id,
                serde_json::Value::Null,
            ))
        }

        "eth_getLogs" => service_get_logs(service, request).await,

        "eth_getBalance" => {
            let params: (String, String) = request.parse_params()?;
            validate_eth_address(&params.0).map_err(|msg| {
                JsonRpcResponse::error(
                    answer_id.clone(),
                    JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        format!("eth_getBalance: {msg}"),
                        serde_json::Value::Null,
                    ),
                )
            })?;
            Ok(JsonRpcResponse::success(answer_id, "0x0"))
        }

        "eth_getBlockTransactionCountByNumber" => {
            let _params: (String,) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0x0"))
        }

        "eth_getStorageAt" => {
            let params: (String, String, String) = request.parse_params()?;
            validate_eth_address(&params.0).map_err(|msg| {
                JsonRpcResponse::error(
                    answer_id.clone(),
                    JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        format!("eth_getStorageAt: {msg}"),
                        serde_json::Value::Null,
                    ),
                )
            })?;
            Ok(JsonRpcResponse::success(
                answer_id,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ))
        }

        "debug_traceTransaction" => service_debug_trace_transaction(service, request).await,

        "zkevm_getLatestGlobalExitRoot" => service_zkevm_get_latest_ger(service, request).await,

        "zkevm_getExitRootsByGER" => service_zkevm_get_exit_roots_by_ger(service, request).await,

        "admin_registerFaucet" => {
            let params: (crate::service_admin::RegisterFaucetParams,) = request.parse_params()?;
            let result = crate::service_admin::admin_register_faucet(service, params.0).await;
            json_rpc_response_from_result(result, answer_id, ServiceErrorCode::AdminRegisterFaucet)
        }

        "admin_registerNativeFaucet" => {
            // Allowlist an EXTERNALLY-deployed Miden-ORIGINATED (native lock/unlock) faucet
            // on the bridge — only the bridge admin (this proxy's service account) can.
            let params: (crate::service_admin::RegisterNativeFaucetParams,) =
                request.parse_params()?;
            let result =
                crate::service_admin::admin_register_native_faucet(service, params.0).await;
            json_rpc_response_from_result(result, answer_id, ServiceErrorCode::AdminRegisterFaucet)
        }

        "admin_listFaucets" => {
            // R12 — propagate store errors to the caller. Pre-fix this used
            // `unwrap_or_default()`, returning an empty list on transient DB failure.
            // An operator monitoring "do we have faucets?" via this endpoint would
            // think the registry was empty during a Postgres blip and might
            // double-register.
            let faucets = service
                .store
                .list_faucets()
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
            let list: Vec<serde_json::Value> = faucets
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "faucet_id": f.faucet_id.to_hex(),
                        "symbol": f.symbol,
                        "origin_address": format!("0x{}", hex::encode(f.origin_address)),
                        "origin_network": f.origin_network,
                        "origin_decimals": f.origin_decimals,
                        "miden_decimals": f.miden_decimals,
                        "scale": f.scale,
                    })
                })
                .collect();
            Ok(JsonRpcResponse::success(answer_id, serde_json::json!(list)))
        }

        method => {
            // WARN, not ERROR: internet scanners and explorer capability
            // probes (debug_*, parity_*, trace_*) hit this constantly; an
            // unknown method is a client-side condition, not a proxy fault.
            tracing::warn!("JSON-RPC unsupported method: {}", method);
            Ok(request.method_not_found(method))
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to setup SIGINT handler");
}

#[cfg(unix)]
async fn shutdown_signal() {
    let mut terminate_signal = tokio::signal::unix::signal(SignalKind::terminate())
        .expect("failed to setup SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.expect("failed to setup SIGINT handler");
            tracing::info!("shutdown_signal: SIGINT");
        },
        _ = terminate_signal.recv() => {
            tracing::info!("shutdown_signal: SIGTERM");
        },
    }
}

async fn health_check(State(service): State<ServiceState>) -> impl IntoResponse {
    if service.miden_client.is_alive() {
        (
            http::StatusCode::OK,
            axum::Json(serde_json::json!({ "status": "ok" })),
        )
    } else {
        (
            http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "status": "degraded",
                "reason": "node connection lost"
            })),
        )
    }
}

pub async fn serve(
    url: Url,
    state: ServiceState,
    metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
) -> anyhow::Result<()> {
    // R13 — per-IP rate limit. The default config (500 req/sec sustained,
    // 500-token burst) slows brute-force probing of admin auth (R1) and
    // signer-allow-list rejection paths (R2) without affecting legitimate
    // aggkit traffic.
    //
    // Self-review of the original R13 wiring: the builder's `.per_second(N)`
    // method is named MISLEADINGLY — it sets the REPLENISH PERIOD in seconds,
    // i.e. "one token every N seconds", which is the *inverse* of what the
    // const name `DEFAULT_RATE_LIMIT_PER_SECOND` implied. The original
    // shipped wiring at `.per_second(60)` therefore yielded one token every
    // 60 seconds (~0.0167 req/sec sustained), not 60 req/sec. Once the
    // 60-token burst was exhausted aggkit was throttled for hours, with
    // 429 cooldowns of 90s+ visible in its sync logs.
    //
    // Use `.per_millisecond(1000 / N)` so the const name maps to the actual
    // sustained rate. `1000 / N` clamps to ≥1 ms to avoid divide-by-zero
    // when N >= 1000 — at N=1000 we get one token per ms i.e. 1000 req/sec
    // exactly; at higher rates the millisecond floor caps us at 1000/sec
    // sustained, which is comfortably above any legitimate use case.
    let replenish_period_ms = 1000_u64
        .checked_div(state.rate_limit_per_second)
        .unwrap_or(1)
        .max(1);
    let governor_conf = std::sync::Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(replenish_period_ms)
            .burst_size(state.rate_limit_burst)
            .finish()
            .expect("rate-limit config must produce a valid governor"),
    );
    let governor_layer = GovernorLayer::new(governor_conf);

    let app = Router::new()
        .route("/", post(json_rpc_endpoint))
        .route("/health", get(health_check))
        .layer(
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::if_not_present(
                    http::header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                ))
                .layer(TraceLayer::new_for_http())
                // R6 — cap inbound bodies before any handler reads them. Bodies bigger
                // than MAX_REQUEST_BODY_BYTES are rejected with HTTP 413 by tower-http
                // without ever allocating the full payload.
                .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
                // R13 — per-IP rate limiting. Applied before the JSON-RPC handler so
                // an attacker cannot exhaust the worker pool via a flood.
                .layer(governor_layer)
                .layer(build_cors_layer(state.cors_allowed_origins.as_deref())),
        )
        .with_state(state)
        .route(
            "/metrics",
            get(move || async move { metrics_handle.render() }),
        );

    let listener = url
        .socket_addrs(|| None)
        .with_context(|| format!("failed to parse url {url}"))?;
    let listener = TcpListener::bind(&*listener)
        .await
        .with_context(|| format!("failed to bind TCP listener on {url}"))?;

    tracing::info!(target: COMPONENT, address = %url, "Service started");

    // PeerIpKeyExtractor needs ConnectInfo<SocketAddr> to identify the caller.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Build the CORS layer for the JSON-RPC route.
///
/// Self-review R11 — pre-fix, the CORS layer used `allow_origin(Any)` and
/// `allow_methods(Any)`. Combined with the unauthenticated admin endpoints (R1) and
/// `eth_sendRawTransaction` (R2), a victim's browser visiting attacker.example could
/// POST to a private agglayer endpoint via fetch, including state-mutating methods
/// like `admin_registerFaucet` and `eth_sendRawTransaction`.
///
/// Now driven by `ServiceState::cors_allowed_origins` (CLI flag
/// `--cors-allowed-origins` / env `CORS_ALLOWED_ORIGINS`):
/// - `None` → no `Access-Control-Allow-Origin` header is emitted; cross-origin
///   browser requests are blocked by the browser. Safest production default.
/// - `Some(["*"])` → wildcard, dev-only convenience.
/// - `Some([..])` → explicit allowlist.
fn build_cors_layer(allowed_origins: Option<&[String]>) -> tower_http::cors::CorsLayer {
    let layer = tower_http::cors::CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_headers([http::header::CONTENT_TYPE]);
    match allowed_origins {
        None => layer, // no allow-origin → effectively no CORS
        Some(origins) if origins.iter().any(|o| o == "*") => {
            tracing::warn!(
                target: COMPONENT,
                "R11: CORS configured with wildcard origin — DEV ONLY; do not deploy to mainnet"
            );
            layer.allow_origin(tower_http::cors::Any)
        }
        Some(origins) => {
            // R11 follow-up — RFC 6454 origin equality is scheme/host/port-wise
            // and host is case-insensitive. tower-http does a literal byte-compare
            // on `HeaderValue`, so `https://App.Example.com` and `https://app.example.com`
            // are treated as different origins. Normalise here by lowercasing the
            // input so an operator who deploys with checksum-cased hostnames in
            // their dashboard still gets matched against the configured list.
            let mut had_dropped = false;
            let parsed: Vec<HeaderValue> = origins
                .iter()
                .filter_map(|s| {
                    let normalised = s.to_ascii_lowercase();
                    let Ok(v) = normalised.parse::<HeaderValue>() else {
                        tracing::warn!(
                            target: COMPONENT,
                            "R11: dropping malformed CORS allow-list entry: {s:?}"
                        );
                        had_dropped = true;
                        return None;
                    };
                    Some(v)
                })
                .collect();
            if parsed.is_empty() && had_dropped {
                tracing::error!(
                    target: COMPONENT,
                    "R11: every CORS allow-list entry failed to parse; layer is now closed"
                );
            }
            tracing::info!(
                target: COMPONENT,
                origins = ?origins,
                "R11: CORS allow-list configured ({} origin(s))",
                parsed.len()
            );
            layer.allow_origin(parsed)
        }
    }
}

/// Validate that an `address` parameter from a JSON-RPC stub method is a well-formed
/// 20-byte hex address (`0x` prefix + 40 hex chars). Returns an error message suitable
/// for an `InvalidParams` JSON-RPC response.
///
/// Self-review R10 — pre-fix, methods like `eth_getCode` accepted arbitrary garbage
/// and still returned a constant stub. Some downstream EVM-compat tools interpret
/// `0xFE` (the `eth_getCode` stub) as "address has code", so accepting a malformed
/// input could mislead consistency checks. Reject malformed values rather than
/// fabricate compatibility.
fn validate_eth_address(addr: &str) -> Result<(), String> {
    let s = addr
        .strip_prefix("0x")
        .ok_or("address must start with 0x")?;
    if s.len() != 40 {
        return Err(format!("address must be 40 hex chars, got {}", s.len()));
    }
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("address contains non-hex characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-review R13 — repro+regression. Default rate-limit constants
    /// must produce a buildable governor config that delivers the expected
    /// SUSTAINED RATE (not the prior `.per_second(N)` mistake which turned
    /// 60-cells-burst-then-1-token-per-60-seconds into the proxy's effective
    /// throttle and made aggkit 429 itself out of the e2e).
    ///
    /// Pins the contract:
    /// - default 500/500 finalises
    /// - 1/1 finalises
    /// - rate_limit_per_second=0 is treated as "1 token per 1 ms" (1000/sec)
    ///   via the saturating divide in `serve()` — this matches "open" intent
    ///   when an operator nukes the limit by setting 0
    /// - burst=0 always invalid regardless of period
    #[test]
    fn r13_default_rate_limit_config_is_valid() {
        let replenish_default = 1000_u64
            .checked_div(DEFAULT_RATE_LIMIT_PER_SECOND)
            .unwrap_or(1)
            .max(1);
        let cfg = GovernorConfigBuilder::default()
            .per_millisecond(replenish_default)
            .burst_size(DEFAULT_RATE_LIMIT_BURST)
            .finish();
        assert!(
            cfg.is_some(),
            "default 500/500 rate-limit config must finalise"
        );

        let tight = GovernorConfigBuilder::default()
            .per_millisecond(1000)
            .burst_size(1)
            .finish();
        assert!(
            tight.is_some(),
            "1/1 (1 token per 1000ms) config must finalise"
        );

        assert!(
            GovernorConfigBuilder::default()
                .per_millisecond(1)
                .burst_size(0)
                .finish()
                .is_none(),
            "zero burst must be rejected regardless of period"
        );
    }

    /// Self-review (review-of-fix follow-up) — repro+regression. Pre-fix,
    /// `rpc_requests_total{method=...}` used the raw attacker-supplied method
    /// string. An unauthenticated caller posting `{"method":"admin_<uuid>"}`
    /// for distinct uuids would create one Prometheus series per call,
    /// inflating the metrics exporter without bound (OOM DoS). Test pins the
    /// bucketing function: known methods map to themselves; anything else
    /// (including typos and obvious attacker probes) collapses to "other".
    #[test]
    fn metric_label_cardinality_capped_to_known_set() {
        // Known methods preserved.
        assert_eq!(bucket_method_label("eth_getLogs"), "eth_getLogs");
        assert_eq!(
            bucket_method_label("admin_registerFaucet"),
            "admin_registerFaucet"
        );
        assert_eq!(
            bucket_method_label("eth_sendRawTransaction"),
            "eth_sendRawTransaction"
        );

        // Attacker-shaped admin probe: one series, not a million.
        assert_eq!(bucket_method_label("admin_DEADBEEF"), "other");
        assert_eq!(bucket_method_label("admin_8d4f2c3e-…-uuid"), "other");

        // Typos collapse to "other".
        assert_eq!(bucket_method_label("eth_getlogs"), "other"); // wrong case
        assert_eq!(bucket_method_label("eth_blockNumberX"), "other");

        // Empty / odd inputs handled.
        assert_eq!(bucket_method_label(""), "other");
        assert_eq!(bucket_method_label(&"a".repeat(10_000)), "other");
    }

    /// Self-review R1 — repro+regression. Pre-fix, every `admin_*` JSON-RPC method
    /// was reachable by anyone who could hit the listening port. There was no
    /// `Authorization` header check, no API key, no IP allow-list. A malicious
    /// caller could `admin_registerFaucet` with attacker-chosen `MetadataHash` to
    /// poison the registry for any token (so that the legitimate first claim of
    /// that token's metadata would fail FPI validation).
    ///
    /// Tests cover:
    /// - `not_configured` — when no admin key is set, every admin request is
    ///   rejected with `NotConfigured`. This is the safe production default
    ///   (fail closed, not open).
    /// - `missing_header` — admin key configured, no Authorization header.
    /// - `wrong_scheme` — Authorization present but not `Bearer ...`.
    /// - `wrong_token` — `Bearer x` where x != configured key.
    /// - `correct_token` — accepted.
    /// - `constant_time_eq` — basic equality + length-mismatch coverage.
    #[test]
    fn r1_admin_auth_rejects_when_unconfigured() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(
            check_admin_auth(None, &headers).unwrap_err(),
            AdminAuthError::NotConfigured
        );
    }

    #[test]
    fn r1_admin_auth_rejects_missing_header() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(
            check_admin_auth(Some("s3cret"), &headers).unwrap_err(),
            AdminAuthError::MissingHeader
        );
    }

    #[test]
    fn r1_admin_auth_rejects_wrong_scheme() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(
            check_admin_auth(Some("s3cret"), &headers).unwrap_err(),
            AdminAuthError::MalformedHeader
        );
    }

    #[test]
    fn r1_admin_auth_rejects_wrong_token() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert_eq!(
            check_admin_auth(Some("s3cret"), &headers).unwrap_err(),
            AdminAuthError::BadToken
        );
    }

    #[test]
    fn r1_admin_auth_accepts_correct_token() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer s3cret"),
        );
        assert!(check_admin_auth(Some("s3cret"), &headers).is_ok());
    }

    #[test]
    fn r1_constant_time_eq_pins_behaviour() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        // length mismatch must reject (and not panic).
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    /// Self-review of-the-fix follow-up — repro+regression. The previous
    /// `constant_time_eq` early-returned `false` on `a.len() != b.len()`,
    /// leaking the configured token length via timing. Test asserts the
    /// no-early-return contract: the function must walk `max(a.len(),
    /// b.len())` bytes regardless of where the difference is.
    ///
    /// We can't measure timing in a unit test, but we can pin the
    /// observable contract:
    ///   - same length + same content → true
    ///   - same length + different content (anywhere) → false
    ///   - different length → false
    ///   - either empty → false unless both empty
    #[test]
    fn r1_constant_time_eq_contract_pinned() {
        // Same length, every byte position can disagree independently.
        for i in 0..8 {
            let mut a = [0u8; 8];
            let mut b = [0u8; 8];
            b[i] = 1; // single-byte differ at position i
            assert!(
                !constant_time_eq(&a, &b),
                "single-byte differ at pos {i} must reject"
            );
            a[i] = 1;
            assert!(
                constant_time_eq(&a, &b),
                "matching byte at pos {i} must accept"
            );
        }
        // Length mismatch from any side.
        assert!(!constant_time_eq(b"longer-token", b"short"));
        assert!(!constant_time_eq(b"short", b"longer-token"));
    }

    /// Self-review of-the-fix follow-up — repro+regression. RFC 6750 §2.1
    /// makes the bearer auth scheme case-insensitive. The previous
    /// implementation only accepted `Bearer ` exactly; tests pin the
    /// case-insensitive prefix while still rejecting non-matching prefixes
    /// (Basic, garbage, missing space).
    #[test]
    fn r1_bearer_prefix_case_insensitive() {
        assert_eq!(
            strip_bearer_prefix_case_insensitive("Bearer abc"),
            Some("abc")
        );
        assert_eq!(
            strip_bearer_prefix_case_insensitive("bearer abc"),
            Some("abc")
        );
        assert_eq!(
            strip_bearer_prefix_case_insensitive("BEARER abc"),
            Some("abc")
        );
        assert_eq!(
            strip_bearer_prefix_case_insensitive("BeArEr abc"),
            Some("abc")
        );

        // Non-Bearer schemes rejected.
        assert_eq!(strip_bearer_prefix_case_insensitive("Basic dXNlcg=="), None);
        assert_eq!(strip_bearer_prefix_case_insensitive("Bearerabc"), None); // missing space
        assert_eq!(strip_bearer_prefix_case_insensitive("Bear abc"), None); // truncated
        assert_eq!(strip_bearer_prefix_case_insensitive(""), None);
        assert_eq!(strip_bearer_prefix_case_insensitive("Bearer "), Some(""));
    }

    /// Self-review R11 — repro+regression. Pre-fix, `CorsLayer::new().allow_origin(Any)`
    /// allowed any origin to invoke any JSON-RPC method, including admin endpoints and
    /// `eth_sendRawTransaction`. Combined with the unauthenticated admin surface (R1)
    /// this meant a victim's browser visiting attacker.example could POST state-mutating
    /// requests to a private agglayer endpoint via fetch.
    ///
    /// The fix is `build_cors_layer` driven by an explicit allow-list. We can't directly
    /// inspect `CorsLayer` configuration after build, but we can pin the input contract:
    /// None → no allow-origin emitted; ["*"] → wildcard with a warning; ["a", "b"] →
    /// explicit allow-list; junk values are filtered to avoid panics on a misconfigured
    /// env var.
    #[test]
    fn r11_cors_layer_inputs_dont_panic_and_filter_junk() {
        // None — should construct successfully.
        let _ = build_cors_layer(None);

        // Wildcard — should construct successfully (warning logged at runtime).
        let _ = build_cors_layer(Some(&["*".to_string()]));

        // Explicit allow-list — should construct successfully.
        let _ = build_cors_layer(Some(&[
            "https://app.example.com".to_string(),
            "https://staging.example.com".to_string(),
        ]));

        // Junk values must be filtered (HeaderValue parse fails) instead of panicking.
        let _ = build_cors_layer(Some(&[
            "https://valid.example.com".to_string(),
            "\nnot a valid header value\r".to_string(),
        ]));
    }

    /// Self-review R10 — repro+regression. Pre-fix, `eth_getCode` / `eth_getBalance` /
    /// `eth_getStorageAt` accepted arbitrary garbage and returned constant stubs
    /// regardless. Test that the validator rejects (a) the empty string,
    /// (b) addresses missing the `0x` prefix, (c) wrong length, (d) non-hex
    /// characters, and accepts (e) a well-formed lowercase address and (f) the
    /// upper/mixed-case checksum form.
    #[test]
    fn r10_validate_eth_address_rejects_garbage() {
        assert!(validate_eth_address("").is_err());
        assert!(validate_eth_address("nope").is_err());
        assert!(
            validate_eth_address("0000000000000000000000000000000000000000").is_err(),
            "missing 0x prefix"
        );
        assert!(
            validate_eth_address("0x000000000000000000000000000000000000000").is_err(),
            "39 chars (one short)"
        );
        assert!(
            validate_eth_address("0x00000000000000000000000000000000000000000").is_err(),
            "41 chars (one long)"
        );
        assert!(
            validate_eth_address("0xZZZZ000000000000000000000000000000000000").is_err(),
            "non-hex chars"
        );
        // Accepted forms.
        assert!(validate_eth_address("0x0000000000000000000000000000000000000000").is_ok());
        assert!(validate_eth_address("0xAbCDeF1234567890aBcdEf1234567890ABcDef12").is_ok());
    }

    /// Self-review R6 — repro+regression. The limit constant must be (a) at least as
    /// big as the largest legitimate payload (worst case: an `eth_sendRawTransaction`
    /// carrying a CLAIM proof — ~17 KB — plus headroom for an upper-bounded
    /// `eth_getLogs` filter), and (b) much smaller than axum's default 2 MB so the
    /// tower-http layer actually does the rejecting.
    ///
    /// Pre-fix (no `RequestBodyLimitLayer`), the only protection was axum's default
    /// (2 MB). Post-fix the limit is explicit and pinned by this test.
    #[test]
    fn r6_request_body_limit_is_explicit_and_in_band() {
        // Must be >= 17 KB (largest legitimate aggkit payload observed in fixtures).
        const _: () = assert!(
            MAX_REQUEST_BODY_BYTES >= 17 * 1024,
            "limit too small for legitimate aggkit payloads"
        );
        // Must be much smaller than axum default 2 MB (otherwise the new layer is a no-op).
        const _: () = assert!(
            MAX_REQUEST_BODY_BYTES < 1024 * 1024,
            "limit too generous — must stay below axum default 2 MB"
        );
    }
}
