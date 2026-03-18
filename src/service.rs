use crate::COMPONENT;
use crate::hex::hex_decode_u64;
use crate::service_debug::service_debug_trace_transaction;
use crate::service_eth_call::service_eth_call;
use crate::service_get_logs::service_get_logs;
use crate::service_get_txn_receipt::service_get_txn_receipt;
use crate::service_helpers::{
    build_synthetic_tx_json, json_rpc_response_from_result, store_error, ServiceErrorCode,
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
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use url::Url;

async fn json_rpc_endpoint(
    State(service): State<ServiceState>,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let start = std::time::Instant::now();
    let method_name = request.method.clone();

    let result = json_rpc_handler(service, request).await;

    metrics::counter!("rpc_requests_total", "method" => method_name.clone()).increment(1);
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
        "eth_call" | "eth_gasPrice" | "eth_estimateGas" | "eth_getLogs" | "net_version"
        | "eth_getBlockTransactionCountByNumber" | "eth_getTransactionCount"
        | "eth_getTransactionByHash" | "eth_getTransactionReceipt"
        | "zkevm_getLatestGlobalExitRoot" | "zkevm_getExitRootsByGER" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        _ => tracing::debug!("JSON-RPC {method}"),
    }

    match method {
        "eth_getCode" => {
            let _params: (String, String) = request.parse_params()?;
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
            let hash_hex = params.0.strip_prefix("0x").unwrap_or(&params.0);
            let Ok(hash_bytes) = hex::decode(hash_hex) else {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    String::from("bad block hash"),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            };
            let Ok(hash): Result<[u8; 32], _> = hash_bytes.try_into() else {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    String::from("block hash must be 32 bytes"),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            };
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
            let _params: (String, String) = request.parse_params()?;
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
            let _params: (String, String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(
                answer_id,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ))
        }

        "debug_traceTransaction" => service_debug_trace_transaction(service, request).await,

        "zkevm_getLatestGlobalExitRoot" => service_zkevm_get_latest_ger(service, request).await,

        "zkevm_getExitRootsByGER" => {
            service_zkevm_get_exit_roots_by_ger(service, request).await
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

async fn health_check() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "ok" }))
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
