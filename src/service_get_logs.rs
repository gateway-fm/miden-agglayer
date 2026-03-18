use crate::log_synthesis::LogFilter;
use crate::service_helpers::store_error;
use crate::service_state::ServiceState;
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};

pub(crate) async fn service_get_logs(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let raw_params: (serde_json::Value,) = request.parse_params()?;
    let log_filter: LogFilter = serde_json::from_value(raw_params.0).unwrap_or_default();
    let current_block = service
        .store
        .get_latest_block_number()
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?;
    let synthetic_logs = service
        .store
        .get_logs(&log_filter, current_block)
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?;
    let json_logs: Vec<serde_json::Value> = synthetic_logs
        .iter()
        .map(|l: &crate::log_synthesis::SyntheticLog| l.to_json())
        .collect();

    Ok(JsonRpcResponse::success::<Vec<serde_json::Value>, _>(
        answer_id, json_logs,
    ))
}
