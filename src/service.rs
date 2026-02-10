use crate::COMPONENT;
use crate::ger::latest_ger_update_event;
use crate::hex::hex_decode_prefixed;
use crate::hex::hex_decode_u64;
use crate::service_get_txn_receipt::service_get_txn_receipt;
use crate::service_send_raw_txn::service_send_raw_txn;
use crate::service_state::ServiceState;
use alloy::primitives::LogData;
use alloy::sol_types::SolEvent;
use alloy_core::sol_types::SolCall;
use alloy_rpc_types_eth::Filter;
use alloy_rpc_types_eth::{Header, Log};
use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use http::HeaderValue;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use url::Url;

// https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L71C19-L71C28
alloy_core::sol! {
    uint32 public networkID;
}

async fn json_rpc_endpoint(
    State(service): State<ServiceState>,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let method = request.method.as_str();
    match method {
        "eth_getBlockByNumber" => tracing::trace!("JSON-RPC {method}"),
        "eth_call" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_gasPrice" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_estimateGas" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_getLogs" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_getTransactionCount" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_getTransactionByHash" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        "eth_getTransactionReceipt" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        },
        _ => tracing::debug!("JSON-RPC {method}"),
    }

    match method {
        // polycli checks if the contract code exists
        // return a non-empty string to satisfy the check
        "eth_getCode" => {
            let _params: (String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0x00"))
        },

        "eth_blockNumber" => {
            let block_num = service.block_num_tracker.latest();
            let block_num_str = format!("{:#x}", block_num);
            Ok(JsonRpcResponse::success(answer_id, block_num_str))
        },

        // polycli estimates GasFeeCap using the latest header baseFeePerGas
        // return a dummy header with zero baseFeePerGas to satisfy the client logic
        "eth_getBlockByNumber" => {
            let params: (String, bool) = request.parse_params()?;
            let block_num = match params.0.as_str() {
                "latest" => service.block_num_tracker.latest(),
                any => {
                    let Ok(num) = hex_decode_u64(any) else {
                        let error = JsonRpcError::new(
                            JsonRpcErrorReason::ApplicationError(4),
                            String::from("bad block number"),
                            serde_json::Value::Null,
                        );
                        return Err(JsonRpcResponse::error(answer_id, error));
                    };
                    num
                },
            };
            let header = alloy::consensus::Header {
                number: block_num,
                base_fee_per_gas: Some(0),
                ..Default::default()
            };
            let header = Header::new(header);
            Ok(JsonRpcResponse::success(answer_id, header))
        },

        // polycli sets a txn.Nonce from this method result
        // TODO: for replay protection and ordering this should be a monotonic counter per "from" account
        "eth_getTransactionCount" => {
            let _params: (String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0x0"))
        },

        "eth_gasPrice" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        // polycli estimates GasTipCap (priority fee cap), return zero
        "eth_maxPriorityFeePerGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        // polycli estimates how much gas will be spent on a transaction, return zero
        "eth_estimateGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        "eth_chainId" => {
            Ok(JsonRpcResponse::success(answer_id, format!("{:#x}", service.chain_id)))
        },

        // AggLayer requests current state of the bridge contract using eth_call,
        // but currently everything is stubbed with zero except networkID
        "eth_call" => {
            #[derive(Debug, Deserialize)]
            struct TransactionParam {
                data: Option<String>,
                input: Option<String>,
            }
            let params: (TransactionParam, String) = request.parse_params()?;
            let txn_param = params.0;

            if let Some(data_hex) = txn_param.data.or(txn_param.input) {
                let Ok(data) = hex_decode_prefixed(&data_hex) else {
                    let error = JsonRpcError::new(
                        JsonRpcErrorReason::ApplicationError(3),
                        String::from("bad transaction.data"),
                        serde_json::Value::Null,
                    );
                    return Err(JsonRpcResponse::error(answer_id, error));
                };

                if data.starts_with(&networkIDCall::SELECTOR) {
                    let chain_id = service.chain_id;
                    let chain_id_hex = format!("{:#066x}", chain_id);
                    return Ok(JsonRpcResponse::success(answer_id, chain_id_hex));
                }
            }

            Ok(JsonRpcResponse::success(
                answer_id,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ))
        },

        "eth_sendRawTransaction" => {
            let params: (String,) = request.parse_params()?;
            let result = service_send_raw_txn(service, params.0).await;
            match result {
                Ok(value) => Ok(JsonRpcResponse::success(answer_id, value)),
                Err(error) => {
                    let error = JsonRpcError::new(
                        JsonRpcErrorReason::ApplicationError(1),
                        error.to_string(),
                        serde_json::Value::Null,
                    );
                    Err(JsonRpcResponse::error(answer_id, error))
                },
            }
        },

        // polycli polls receipts to get the eth_sendRawTransaction status
        "eth_getTransactionReceipt" => {
            let params: (String,) = request.parse_params()?;
            let result = service_get_txn_receipt(service, params.0).await;
            match result {
                Ok(value) => Ok(JsonRpcResponse::success(answer_id, value)),
                Err(error) => {
                    let error = JsonRpcError::new(
                        JsonRpcErrorReason::ApplicationError(2),
                        error.to_string(),
                        serde_json::Value::Null,
                    );
                    Err(JsonRpcResponse::error(answer_id, error))
                },
            }
        },

        "eth_getTransactionByHash" => {
            let _txn_hash_str: (String,) = request.parse_params()?;
            // TODO: implement eth_getTransactionByHash
            Ok(JsonRpcResponse::success(answer_id, serde_json::Value::Null))
        },

        "eth_getLogs" => {
            let _filter: (Filter,) = request.parse_params()?;
            let mut logs = Vec::<Log>::new();
            if let Some((event, txn_hash, block_num)) = latest_ger_update_event() {
                let mut log: Log<LogData> = Log::<LogData>::default();
                log.inner.data = event.encode_log_data();
                log.transaction_hash = Some(txn_hash);
                log.block_number = Some(block_num);
                logs.push(log);
            }
            Ok(JsonRpcResponse::success(answer_id, logs))
        },

        method => {
            tracing::error!("JSON-RPC unsupported method: {}", method);
            Ok(request.method_not_found(method))
        },
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("failed to setup SIGINT handler");
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

pub async fn serve(url: Url, state: ServiceState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", post(json_rpc_endpoint))
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
        .with_state(state);

    let listener = url
        .socket_addrs(|| None)
        .with_context(|| format!("failed to parse url {url}"))?;
    let listener = TcpListener::bind(&*listener)
        .await
        .with_context(|| format!("failed to bind TCP listener on {url}"))?;

    tracing::info!(target: COMPONENT, address = %url, "Service started");

    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}
