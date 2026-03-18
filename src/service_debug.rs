use crate::service_helpers::encode_bridge_asset_from_log;
use crate::service_state::ServiceState;
use alloy::primitives::TxHash;
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use std::str::FromStr;

pub(crate) async fn service_debug_trace_transaction(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let params: (String, serde_json::Value) = request.parse_params()?;
    let bridge_addr = crate::bridge_address::get_bridge_address();

    // Try store for real transactions (has actual calldata)
    if let Ok(hash) = TxHash::from_str(&params.0)
        && let Some(data) = service.store.txn_get(hash).await.unwrap_or(None)
    {
        use alloy::consensus::Transaction;
        let from = format!("{:#x}", data.signer);
        let to = data
            .envelope
            .to()
            .map(|a| format!("{a:#x}"))
            .unwrap_or_default();
        let input = format!("0x{}", hex::encode(data.envelope.input()));
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

    // Fallback for synthetic bridge-out txs
    let input_data = if let Ok(hash) = TxHash::from_str(&params.0) {
        let tx_key = format!("{hash:#x}");
        let logs = service
            .store
            .get_logs_for_tx(&tx_key)
            .await
            .unwrap_or_default();
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
