use crate::hex::hex_decode_prefixed;
use crate::service_helpers::{
    ServiceErrorCode, json_rpc_response_from_result, networkIDCall, store_error,
};
use crate::service_state::ServiceState;
use alloy_core::sol_types::SolCall;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use serde::Deserialize;

pub(crate) async fn service_eth_call(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();

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

        const GLOBAL_EXIT_ROOT_MAP_SELECTOR: [u8; 4] = [0x25, 0x7b, 0x36, 0x32];
        if data.starts_with(&GLOBAL_EXIT_ROOT_MAP_SELECTOR) && data.len() >= 36 {
            let mut ger = [0u8; 32];
            ger.copy_from_slice(&data[4..36]);
            if service
                .store
                .is_ger_injected(&ger)
                .await
                .map_err(|e| store_error(answer_id.clone(), e))?
            {
                return Ok(JsonRpcResponse::success(
                    answer_id,
                    "0x0000000000000000000000000000000000000000000000000000000000000001",
                ));
            }
        }

        if let (Some(l1_client), Some(to)) = (&service.l1_client, &to_addr) {
            let to_lower = to.to_lowercase();
            if to_lower == service.rollup_manager_address.to_lowercase()
                || to_lower == service.rollup_address.to_lowercase()
            {
                tracing::debug!(to = %to, "forwarding eth_call to L1");
                return json_rpc_response_from_result(
                    forward_eth_call_to_l1(l1_client.as_ref(), &data_hex, to).await,
                    answer_id,
                    ServiceErrorCode::EthCall,
                );
            }
        }
    }

    Ok(JsonRpcResponse::success(
        answer_id,
        "0x0000000000000000000000000000000000000000000000000000000000000000",
    ))
}

/// Forward an eth_call to L1 for reading rollup contract state.
async fn forward_eth_call_to_l1(
    l1_client: &dyn crate::l1_client::L1Client,
    data_hex: &str,
    to_addr: &str,
) -> anyhow::Result<String> {
    let to: alloy::primitives::Address = to_addr.parse()?;
    let data = crate::hex::hex_decode_prefixed(data_hex)?;
    let result = l1_client.eth_call(to, data.into()).await?;
    Ok(format!("0x{}", alloy::hex::encode(&result)))
}
