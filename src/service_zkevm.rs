use crate::service_helpers::store_error;
use crate::service_state::ServiceState;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};

pub(crate) async fn service_zkevm_get_latest_ger(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let ger = service
        .store
        .get_latest_ger()
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?
        .unwrap_or([0u8; 32]);
    Ok(JsonRpcResponse::success(
        answer_id,
        format!("0x{}", hex::encode(ger)),
    ))
}

pub(crate) async fn service_zkevm_get_exit_roots_by_ger(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
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

    match service
        .store
        .get_ger_entry(&ger)
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?
    {
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
