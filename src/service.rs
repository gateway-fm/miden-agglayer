use crate::claim_endpoint::claim_endpoint_dry_run;
use crate::claim_endpoint::claim_endpoint_raw_txn;
use crate::claim_endpoint::claim_endpoint_txn_receipt;
use crate::service_state::ServiceState;
use alloy::consensus::Header;
use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use http::HeaderValue;
use miden_agglayer_service::COMPONENT;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use url::Url;

async fn json_rpc_endpoint(
    State(service): State<ServiceState>,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let method = request.method.as_str();
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

        method => Ok(request.method_not_found(method)),
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

    axum::serve(listener, app).await.map_err(Into::into)
}
