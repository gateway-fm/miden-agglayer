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
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

/// Maximum size in bytes for an inbound JSON-RPC request body.
///
/// Self-review R6 — without an explicit cap, axum's default body limit (2 MB) is the
/// only protection. JSON-RPC requests are typically tiny; an unauthenticated caller
/// posting megabytes of garbage is purely DoS. 256 KB is comfortable headroom for
/// legitimate payloads (a typical `eth_sendRawTransaction` carrying a CLAIM proof is
/// ~17 KB; an `eth_getLogs` with the maximum allowed filter arrays is well below 200 KB).
pub const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
use url::Url;

async fn json_rpc_endpoint(
    State(service): State<ServiceState>,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let start = std::time::Instant::now();
    let method_name = request.method.clone();

    let result = json_rpc_handler(service, request).await;

    metrics::counter!("rpc_requests_total", "method" => method_name.to_string()).increment(1);
    metrics::histogram!("rpc_request_duration_seconds", "method" => method_name)
        .record(start.elapsed().as_secs_f64());

    result
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
            let block_num = service
                .store
                .get_latest_block_number()
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
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
            let nonce = service
                .store
                .nonce_get(&params.0)
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?;
            Ok(JsonRpcResponse::success(answer_id, format!("{nonce:#x}")))
        }

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

        "net_version" => Ok(JsonRpcResponse::success(
            answer_id,
            format!("{}", service.chain_id),
        )),

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

        "admin_listFaucets" => {
            let faucets = service.store.list_faucets().await.unwrap_or_default();
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
            tracing::error!("JSON-RPC unsupported method: {}", method);
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
                .layer(
                    CorsLayer::new()
                        .allow_origin(tower_http::cors::Any)
                        .allow_methods(tower_http::cors::Any)
                        .allow_headers([http::header::CONTENT_TYPE]),
                ),
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
    let s = addr.strip_prefix("0x").ok_or("address must start with 0x")?;
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
        assert!(
            MAX_REQUEST_BODY_BYTES >= 17 * 1024,
            "limit too small: {MAX_REQUEST_BODY_BYTES}"
        );
        // Must be much smaller than axum default 2 MB (otherwise the new layer is a no-op).
        assert!(
            MAX_REQUEST_BODY_BYTES < 1024 * 1024,
            "limit too generous: {MAX_REQUEST_BODY_BYTES}"
        );
    }
}
