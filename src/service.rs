use crate::COMPONENT;
use crate::hex::hex_decode_prefixed;
use crate::hex::hex_decode_u64;
use crate::log_synthesis::LogFilter;
use crate::service_get_txn_receipt::service_get_txn_receipt;
use crate::service_send_raw_txn::service_send_raw_txn;
use crate::service_state::ServiceState;
use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use http::HeaderValue;
use serde::Deserialize;
use std::str::FromStr;
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

// https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L196
alloy_core::sol! {
    #[derive(Debug)]
    function bridgeAsset(
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        address token,
        bool forceUpdateGlobalExitRoot,
        bytes permitData
    );
}

/// Encode `bridgeAsset(...)` calldata from a BridgeEvent synthetic log.
///
/// The aggkit L2BridgeSyncer calls `debug_traceTransaction`, finds the subcall
/// to the bridge address, then ABI-decodes the `input` as `bridgeAsset(...)`.
/// Without proper calldata, it errors with "failed to extract bridge event data".
fn encode_bridge_asset_from_log(log: &crate::log_synthesis::SyntheticLog) -> String {
    let data_hex = log.data.strip_prefix("0x").unwrap_or(&log.data);
    let Ok(data_bytes) = hex::decode(data_hex) else {
        return "0x".to_string();
    };

    // BridgeEvent data layout (32 bytes each):
    //   [0]  leafType (uint8)
    //   [1]  originNetwork (uint32)
    //   [2]  originAddress (address)
    //   [3]  destinationNetwork (uint32)
    //   [4]  destinationAddress (address)
    //   [5]  amount (uint256)
    //   [6]  metadata offset
    //   [7]  depositCount (uint32)
    if data_bytes.len() < 8 * 32 {
        return "0x".to_string();
    }

    // Extract fields from 32-byte ABI words
    let dest_net = u32::from_be_bytes(
        data_bytes[3 * 32 + 28..3 * 32 + 32].try_into().unwrap_or([0; 4]),
    );
    let dest_addr: [u8; 20] = data_bytes[4 * 32 + 12..4 * 32 + 32]
        .try_into()
        .unwrap_or([0; 20]);
    let amount = alloy::primitives::U256::from_be_slice(&data_bytes[5 * 32..6 * 32]);

    // Encode as bridgeAsset(destNet, destAddr, amount, token=0, false, "")
    let call = bridgeAssetCall {
        destinationNetwork: dest_net,
        destinationAddress: alloy::primitives::Address::from(dest_addr),
        amount,
        token: alloy::primitives::Address::ZERO,
        forceUpdateGlobalExitRoot: false,
        permitData: alloy::primitives::Bytes::new(),
    };

    format!("0x{}", hex::encode(SolCall::abi_encode(&call)))
}

#[repr(i32)]
enum ServiceErrorCode {
    SendRawTransaction = 1,
    GetTransactionReceipt,
}

impl From<ServiceErrorCode> for JsonRpcErrorReason {
    fn from(value: ServiceErrorCode) -> Self {
        Self::ApplicationError(value as i32)
    }
}

/// Build a synthetic transaction JSON for bridge-out events that have no TxnManager entry.
///
/// AggSender's L2BridgeSyncer calls `eth_getTransactionByHash` for every log it receives,
/// then extracts the sender via Go's `ethclient.TransactionByHash`. Go checks:
///
///   if json.From != nil && json.BlockHash != nil { setSenderFromServer(tx, from, blockHash) }
///
/// This function constructs a minimal Legacy transaction that Go can unmarshal and
/// extract the sender from without falling back to RLP-based signature recovery.
fn build_synthetic_tx_json(
    txn_hash: TxHash,
    log: &crate::log_synthesis::SyntheticLog,
    chain_id: u64,
) -> serde_json::Value {
    serde_json::json!({
        "type": "0x0",
        "nonce": "0x0",
        "gasPrice": "0x0",
        "gas": "0x0",
        "to": &log.address,
        "value": "0x0",
        "input": "0x",
        "v": "0x1b",
        "r": "0x1",
        "s": "0x1",
        "hash": format!("{txn_hash:#x}"),
        "from": &log.address,
        "blockHash": format!("0x{}", hex::encode(log.block_hash)),
        "blockNumber": format!("0x{:x}", log.block_number),
        "transactionIndex": "0x0",
        "chainId": format!("0x{:x}", chain_id),
    })
}

fn json_rpc_response_from_result<T: serde::Serialize>(
    result: anyhow::Result<T>,
    answer_id: axum_jrpc::Id,
    error_code: ServiceErrorCode,
) -> JrpcResult {
    match result {
        Ok(value) => Ok(JsonRpcResponse::success(answer_id, value)),
        Err(error) => {
            let error = JsonRpcError::new(
                error_code.into(),
                error.to_string(),
                serde_json::Value::Null,
            );
            Err(JsonRpcResponse::error(answer_id, error))
        }
    }
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
        }
        "eth_gasPrice" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_estimateGas" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_getLogs" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "net_version" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_getBlockTransactionCountByNumber" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_getTransactionCount" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_getTransactionByHash" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "eth_getTransactionReceipt" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "zkevm_getLatestGlobalExitRoot" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        "zkevm_getExitRootsByGER" => {
            tracing::debug!(target: concat!(module_path!(), "::debug"), "JSON-RPC {method}")
        }
        _ => tracing::debug!("JSON-RPC {method}"),
    }

    match method {
        // Return non-empty code for all addresses.
        // Aggsender checks eth_getCode before calling eth_call on the rollup/bridge contracts.
        // Returns 0xFE (INVALID opcode) to indicate contract exists but prevents execution.
        "eth_getCode" => {
            let _params: (String, String) = request.parse_params()?;
            Ok(JsonRpcResponse::success(answer_id, "0xFE"))
        }

        "eth_blockNumber" => {
            let block_num = service.block_num_tracker.latest();
            let block_num_str = format!("{:#x}", block_num);
            Ok(JsonRpcResponse::success(answer_id, block_num_str))
        }

        // Return synthetic block with deterministic hash (prevents false reorg detection)
        "eth_getBlockByNumber" => {
            let params: (String, bool) = request.parse_params()?;
            let block_num = match params.0.as_str() {
                "latest" | "pending" | "finalized" | "safe" => service.block_num_tracker.latest(),
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
            let nonce = service.nonce_tracker.get(&params.0);
            Ok(JsonRpcResponse::success(answer_id, format!("{nonce:#x}")))
        }

        "eth_gasPrice" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        // polycli estimates GasTipCap (priority fee cap), return zero
        "eth_maxPriorityFeePerGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        // polycli estimates how much gas will be spent on a transaction, return zero
        "eth_estimateGas" => Ok(JsonRpcResponse::success(answer_id, "0x0")),

        "eth_chainId" => Ok(JsonRpcResponse::success(
            answer_id,
            format!("{:#x}", service.chain_id),
        )),

        // AggLayer requests current state of the bridge contract using eth_call,
        // but currently everything is stubbed with zero except networkID
        "eth_call" => {
            #[derive(Debug, Deserialize)]
            struct TransactionParam {
                to: Option<String>,
                data: Option<String>,
                input: Option<String>,
            }
            let params: (TransactionParam, String) = request.parse_params()?;
            let txn_param = params.0;
            let to_addr = txn_param.to.clone();

            if let Some(data_hex) = txn_param.data.or(txn_param.input) {
                let Ok(data) = hex_decode_prefixed(&data_hex) else {
                    let error = JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        String::from("bad transaction.data"),
                        serde_json::Value::Null,
                    );
                    return Err(JsonRpcResponse::error(answer_id, error));
                };

                if data.len() >= 4 {
                    tracing::debug!(
                        to = ?to_addr,
                        selector = %format!("0x{}", alloy::hex::encode(&data[..4])),
                        data_len = data.len(),
                        "eth_call"
                    );
                }

                if data.starts_with(&networkIDCall::SELECTOR) {
                    let network_id = service.network_id;
                    let network_id_hex = format!("{:#066x}", network_id);
                    return Ok(JsonRpcResponse::success(answer_id, network_id_hex));
                }

                // Forward eth_call to L1 for rollup manager / rollup contract queries.
                // The aggsender queries these L1 contracts via the L2 RPC to build certificates.
                if let (Some(l1_url), Some(to)) = (&service.l1_rpc_url, &to_addr) {
                    let to_lower = to.to_lowercase();
                    let rollup_mgr = "0x6c6c009cc348976db4a908c92b24433d4f6eda43";
                    let rollup_addr = "0x414e9e227e4b589af92200508af5399576530e4e";
                    if to_lower == rollup_mgr || to_lower == rollup_addr {
                        tracing::debug!(to = %to, "forwarding eth_call to L1");
                        match forward_eth_call_to_l1(l1_url, &data_hex, to).await {
                            Ok(result) => return Ok(JsonRpcResponse::success(answer_id, result)),
                            Err(e) => tracing::warn!("L1 forward failed: {e:#}"),
                        }
                    }
                }
            }

            Ok(JsonRpcResponse::success(
                answer_id,
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ))
        }

        "eth_sendRawTransaction" => {
            let params: (String,) = request.parse_params()?;
            let result = service_send_raw_txn(service, params.0).await;
            match &result {
                Ok(hash) => tracing::info!("eth_sendRawTransaction: OK hash={hash}"),
                Err(err) => tracing::info!("eth_sendRawTransaction: ERR {err:#}"),
            }
            json_rpc_response_from_result(result, answer_id, ServiceErrorCode::SendRawTransaction)
        }

        // polycli polls receipts to get the eth_sendRawTransaction status
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

            // Try TxnManager first (real transactions from eth_sendRawTransaction)
            if let Some(txn) = service.txn_manager.txn(txn_hash) {
                return Ok(JsonRpcResponse::success(answer_id, txn));
            }

            // Fallback: check LogStore for synthetic transactions (bridge-out events).
            // BridgeOutScanner creates synthetic tx hashes for BridgeEvent logs that
            // have no corresponding TxnManager entry. AggSender's L2BridgeSyncer
            // queries eth_getTransactionByHash for each log to extract the sender.
            let tx_hash_str = format!("{txn_hash:#x}");
            let logs = service.log_store.get_logs_for_tx(&tx_hash_str);
            if let Some(log) = logs.first() {
                tracing::info!("eth_getTransactionByHash: found synthetic tx {tx_hash_str}");
                let synthetic_tx = build_synthetic_tx_json(txn_hash, log, service.chain_id);
                return Ok(JsonRpcResponse::success(answer_id, synthetic_tx));
            }

            // Unknown hash: return null.
            // The EthTxManager checks TransactionByHash BEFORE sending to see if the
            // tx is already "in the network". If we return a synthetic tx here, the
            // EthTxManager skips SendTransaction entirely, and the receipt is never
            // created — causing perpetual "not mined yet" polling.
            tracing::debug!(
                tx_hash = %tx_hash_str,
                "eth_getTransactionByHash: unknown hash, returning null"
            );
            Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                answer_id,
                serde_json::Value::Null,
            ))
        }

        "eth_getLogs" => {
            // Return synthetic logs from LogStore (GER/claim events with proper formatting).
            // TxnManager logs are intentionally excluded: they duplicate LogStore entries
            // but use alloy's Log type which serializes Optional fields as JSON null,
            // causing Go's hexutil.Uint unmarshaling to fail in the bridge-service.
            let raw_params: (serde_json::Value,) = request.parse_params()?;

            let log_filter: LogFilter = serde_json::from_value(raw_params.0).unwrap_or_default();
            let current_block = service.block_num_tracker.latest();
            let synthetic_logs = service.log_store.get_logs(&log_filter, current_block);
            let json_logs: Vec<serde_json::Value> = synthetic_logs
                .iter()
                .map(|l: &crate::log_synthesis::SyntheticLog| l.to_json())
                .collect();

            Ok(JsonRpcResponse::success::<Vec<serde_json::Value>, _>(
                answer_id, json_logs,
            ))
        }

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

        // Aggkit's L2BridgeSyncer calls debug_traceTransaction with callTracer config
        // to extract the sender of claim/bridge transactions. Returns a call trace
        // with the correct sender and input data so aggkit can build certificates.
        //
        // Aggkit's findCall() does DFS on the "calls" array looking for a subcall
        // where "to" matches the bridge address. It does NOT check the root call.
        // So we must include a subcall with to=bridge_address.
        "debug_traceTransaction" => {
            // Accept 2 params: [txHash, {"tracer": "callTracer"}]
            let params: (String, serde_json::Value) = request.parse_params()?;
            let bridge_addr = crate::bridge_address::get_bridge_address();

            // Try TxnManager for real transactions (has actual calldata)
            if let Ok(hash) = TxHash::from_str(&params.0)
                && let Some((from, to, input)) = service.txn_manager.txn_trace_info(hash)
            {
                let call_to = if to.is_empty() {
                    bridge_addr.to_string()
                } else {
                    to
                };
                return Ok(JsonRpcResponse::success(
                    answer_id,
                    serde_json::json!({
                        "type": "CALL",
                        "from": &from,
                        "to": &call_to,
                        "value": "0x0",
                        "input": &input,
                        "calls": [{
                            "type": "DELEGATECALL",
                            "from": &call_to,
                            "to": &call_to,
                            "value": "0x0",
                            "input": &input,
                            "calls": []
                        }]
                    }),
                ));
            }

            // Fallback for synthetic bridge-out txs: look up the BridgeEvent log
            // and encode proper bridgeAsset calldata so aggkit's L2BridgeSyncer
            // can extract the bridge event data via findCall() + ABI decode.
            let input_data = if let Ok(hash) = TxHash::from_str(&params.0) {
                let tx_key = format!("{hash:#x}");
                let logs = service.log_store.get_logs_for_tx(&tx_key);
                if let Some(log) = logs.first() {
                    encode_bridge_asset_from_log(log)
                } else {
                    "0x".to_string()
                }
            } else {
                "0x".to_string()
            };

            Ok(JsonRpcResponse::success(
                answer_id,
                serde_json::json!({
                    "type": "CALL",
                    "from": bridge_addr,
                    "to": bridge_addr,
                    "value": "0x0",
                    "input": &input_data,
                    "calls": [{
                        "type": "DELEGATECALL",
                        "from": bridge_addr,
                        "to": bridge_addr,
                        "value": "0x0",
                        "input": &input_data,
                        "calls": []
                    }]
                }),
            ))
        }

        // Bridge-service's L2 synchronizer calls syncTrustedState() which queries
        // the latest GER to track trusted exit roots. Returns the most recently
        // inserted GER hash, or zero hash if none.
        "zkevm_getLatestGlobalExitRoot" => {
            let ger = service
                .log_store
                .get_latest_ger()
                .unwrap_or([0u8; 32]);
            Ok(JsonRpcResponse::success(
                answer_id,
                format!("0x{}", hex::encode(ger)),
            ))
        }

        // Bridge-service calls ExitRootsByGER to get the exit roots and block info
        // for a given GER. Returns null if the GER is not found (causes syncTrustedState
        // to skip gracefully).
        "zkevm_getExitRootsByGER" => {
            let params: (String,) = request.parse_params()?;
            let hash_hex = params.0.strip_prefix("0x").unwrap_or(&params.0);
            let Ok(hash_bytes) = hex::decode(hash_hex) else {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    String::from("bad GER hash"),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            };
            let Ok(ger): Result<[u8; 32], _> = hash_bytes.try_into() else {
                let error = JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    String::from("GER hash must be 32 bytes"),
                    serde_json::Value::Null,
                );
                return Err(JsonRpcResponse::error(answer_id, error));
            };

            match service.log_store.get_ger_entry(&ger) {
                Some(entry) => {
                    let mainnet = entry.mainnet_exit_root.unwrap_or([0u8; 32]);
                    let rollup = entry.rollup_exit_root.unwrap_or([0u8; 32]);
                    Ok(JsonRpcResponse::success(
                        answer_id,
                        serde_json::json!({
                            "blockNumber": format!("0x{:x}", entry.block_number),
                            "timestamp": format!("0x{:x}", entry.timestamp),
                            "mainnetExitRoot": format!("0x{}", hex::encode(mainnet)),
                            "rollupExitRoot": format!("0x{}", hex::encode(rollup)),
                        }),
                    ))
                }
                None => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    serde_json::Value::Null,
                )),
            }
        }

        method => {
            tracing::error!("JSON-RPC unsupported method: {}", method);
            Ok(request.method_not_found(method))
        }
    }
}

/// Forward an eth_call to L1 for reading rollup contract state.
async fn forward_eth_call_to_l1(
    l1_rpc_url: &str,
    data_hex: &str,
    to_addr: &str,
) -> anyhow::Result<String> {
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy_rpc_types_eth::TransactionRequest;

    let provider = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let to: alloy::primitives::Address = to_addr.parse()?;
    let data = crate::hex::hex_decode_prefixed(data_hex)?;

    let result = provider
        .call(
            TransactionRequest::default()
                .to(to)
                .input(data.into()),
        )
        .await?;

    Ok(format!("0x{}", alloy::hex::encode(&result)))
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_synthesis::SyntheticLog;

    #[test]
    fn test_build_synthetic_tx_json_format() {
        let txn_hash = TxHash::from([5u8; 32]);
        let log = SyntheticLog {
            address: "0xc8cbebf950b9df44d987c8619f092bea980ff038".to_string(),
            topics: vec![],
            data: "0x".to_string(),
            block_number: 100,
            block_hash: [0xAA; 32],
            transaction_hash: format!("{txn_hash:#x}"),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };

        let json = build_synthetic_tx_json(txn_hash, &log, 2);

        // Required by Go's types.Transaction.UnmarshalJSON
        assert_eq!(json["type"], "0x0");
        assert_eq!(json["nonce"], "0x0");
        assert_eq!(json["gasPrice"], "0x0");
        assert_eq!(json["gas"], "0x0");
        assert_eq!(json["value"], "0x0");
        assert_eq!(json["input"], "0x");

        // Required by Go's ethclient for sender extraction
        assert_eq!(json["from"], log.address);
        assert!(
            !json["blockHash"].is_null(),
            "blockHash must not be null for Go setSenderFromServer"
        );
        assert_eq!(json["blockNumber"], "0x64");
        assert_eq!(json["transactionIndex"], "0x0");

        // Go checks RawSignatureValues: r must be non-nil
        assert_eq!(json["v"], "0x1b");
        assert_eq!(json["r"], "0x1");
        assert_eq!(json["s"], "0x1");

        // hash and chainId
        assert_eq!(json["hash"], format!("{txn_hash:#x}"));
        assert_eq!(json["chainId"], "0x2");
    }

    #[test]
    fn test_build_synthetic_tx_json_different_blocks() {
        let txn_hash = TxHash::from([6u8; 32]);
        let log = SyntheticLog {
            address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
            topics: vec![],
            data: "0x".to_string(),
            block_number: 255,
            block_hash: [0xBB; 32],
            transaction_hash: "0xabc".to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };

        let json = build_synthetic_tx_json(txn_hash, &log, 1337);

        assert_eq!(json["blockNumber"], "0xff");
        assert_eq!(json["chainId"], "0x539");
        assert_eq!(json["from"], log.address);
        assert_eq!(json["to"], log.address);
    }
}
