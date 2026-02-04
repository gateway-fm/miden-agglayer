use crate::claim_endpoint::claim_endpoint_dry_run;
use crate::claim_endpoint::claim_endpoint_raw_txn;
use crate::claim_endpoint::claim_endpoint_txn_receipt;
use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::Header;
use alloy_core::sol_types::SolCall;
use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use http::HeaderValue;
use miden_agglayer_service::COMPONENT;
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
    tracing::debug!("JSON-RPC request: {}", method);

    match method {
        // polycli checks if the contract code exists
        // return a non-empty string to satisfy the check
        "eth_getCode" => {
            let _params: (String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0x00"))
        },

        // polycli estimates GasFeeCap using the latest header baseFeePerGas
        // return a dummy header with zero baseFeePerGas to satisfy the client logic
        "eth_getBlockByNumber" => {
            let _params: (String, bool) = request.parse_params()?;
            let header = Header {
                base_fee_per_gas: Some(0),
                ..Default::default()
            };
            Ok(JsonRpcResponse::success(answer_id, header))
        },

        // polycli estimates GasTipCap (priority fee cap), return zero
        "eth_maxPriorityFeePerGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        // polycli sets a txn.Nonce from this method result
        // TODO: for replay protection and ordering this should be a monotonic counter per "from" account
        "eth_getTransactionCount" => {
            let _params: (String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0x0"))
        },

        // polycli estimates how much gas will be spent on a transaction, return zero
        "eth_estimateGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        "eth_chainId" => {
            Ok(JsonRpcResponse::success(answer_id, format!("{:#x}", service.chain_id)))
        },
        "net_version" => Ok(JsonRpcResponse::success(answer_id, format!("{}", service.chain_id))),

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
            let result = claim_endpoint_raw_txn(service, params.0).await;
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
            let result = claim_endpoint_txn_receipt(service, params.0).await;
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
        .route("/claim", post(claim_endpoint_dry_run))
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
