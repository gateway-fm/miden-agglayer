use crate::hex::hex_decode_prefixed;
use crate::service_helpers::{networkIDCall, store_error};
use crate::service_state::ServiceState;
use alloy_core::sol_types::SolCall;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use serde::Deserialize;

const ABI_FALSE: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const ABI_TRUE: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const GLOBAL_EXIT_ROOT_MAP_SELECTOR: [u8; 4] = [0x25, 0x7b, 0x36, 0x32];
const IS_CLAIMED_SELECTOR: [u8; 4] = [0xcc, 0x46, 0x16, 0x32];

fn abi_u32(word: &[u8]) -> Option<u32> {
    if word.len() != 32 || word[..28].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u32::from_be_bytes(word[28..32].try_into().ok()?))
}

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
            return Ok(JsonRpcResponse::success(
                answer_id,
                format!("{:#066x}", service.network_id),
            ));
        }

        if data.starts_with(&GLOBAL_EXIT_ROOT_MAP_SELECTOR) && data.len() >= 36 {
            let mut ger = [0u8; 32];
            ger.copy_from_slice(&data[4..36]);
            let applied = crate::applied_state::ger_applied(&service, &ger)
                .await
                .map_err(|error| store_error(answer_id.clone(), error))?;
            return Ok(JsonRpcResponse::success(
                answer_id,
                if applied { ABI_TRUE } else { ABI_FALSE },
            ));
        }

        if data.starts_with(&IS_CLAIMED_SELECTOR) {
            if data.len() < 68 {
                return Err(JsonRpcResponse::error(
                    answer_id,
                    JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        "isClaimed requires two ABI-encoded uint32 arguments".to_string(),
                        serde_json::Value::Null,
                    ),
                ));
            }
            let (Some(leaf_index), Some(source)) = (abi_u32(&data[4..36]), abi_u32(&data[36..68]))
            else {
                return Err(JsonRpcResponse::error(
                    answer_id,
                    JsonRpcError::new(
                        JsonRpcErrorReason::InvalidParams,
                        "isClaimed arguments exceed uint32".to_string(),
                        serde_json::Value::Null,
                    ),
                ));
            };
            let global_index = crate::applied_state::global_index_for_claim(leaf_index, source);
            let applied = crate::applied_state::claim_applied(&service, global_index)
                .await
                .map_err(|error| store_error(answer_id.clone(), error))?;
            return Ok(JsonRpcResponse::success(
                answer_id,
                if applied { ABI_TRUE } else { ABI_FALSE },
            ));
        }
    }

    Ok(JsonRpcResponse::success(answer_id, ABI_FALSE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_service;
    use axum_jrpc::Id;

    fn eth_call_request(data: Vec<u8>) -> JsonRpcExtractor {
        JsonRpcExtractor {
            parsed: serde_json::json!([{
                "to": "0x0000000000000000000000000000000000000001",
                "data": format!("0x{}", alloy::hex::encode(data))
            }, "latest"]),
            method: "eth_call".to_string(),
            id: Id::Num(1),
        }
    }

    fn is_claimed_calldata(leaf: u32, source: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity(68);
        data.extend(IS_CLAIMED_SELECTOR);
        data.extend([0u8; 28]);
        data.extend(leaf.to_be_bytes());
        data.extend([0u8; 28]);
        data.extend(source.to_be_bytes());
        data
    }

    #[tokio::test]
    async fn is_claimed_uses_required_mainnet_and_rollup_global_indices() {
        for (leaf, source) in [(17u32, 0u32), (23u32, 9u32)] {
            let service = create_test_service();
            let gi = crate::applied_state::global_index_for_claim(leaf, source);
            service
                .store
                .mark_claim_note_processed(
                    format!("landed-{leaf}-{source}"),
                    gi.to_be_bytes::<32>(),
                    1,
                )
                .await
                .unwrap();

            let response =
                service_eth_call(service, eth_call_request(is_claimed_calldata(leaf, source)))
                    .await
                    .unwrap();
            let json = serde_json::to_value(response).unwrap();
            assert_eq!(json["result"], ABI_TRUE);
        }
    }

    #[tokio::test]
    async fn is_claimed_ignores_in_flight_submission_locks() {
        let service = create_test_service();
        let leaf = 31u32;
        let source = 4u32;
        let gi = crate::applied_state::global_index_for_claim(leaf, source);
        service.store.try_claim(gi).await.unwrap();

        let response =
            service_eth_call(service, eth_call_request(is_claimed_calldata(leaf, source)))
                .await
                .unwrap();
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["result"], ABI_FALSE);
    }

    #[tokio::test]
    async fn global_exit_root_map_reads_applied_state() {
        let service = create_test_service();
        let ger = [0xabu8; 32];
        service
            .store
            .commit_ger_event_atomic(1, [0u8; 32], "0xger-applied", &ger, None, None, 0)
            .await
            .unwrap();
        let mut calldata = Vec::with_capacity(36);
        calldata.extend(GLOBAL_EXIT_ROOT_MAP_SELECTOR);
        calldata.extend(ger);

        let response = service_eth_call(service, eth_call_request(calldata))
            .await
            .unwrap();
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["result"], ABI_TRUE);
    }

    #[test]
    fn selector_is_pinned_to_aggkit_contract() {
        assert_eq!(IS_CLAIMED_SELECTOR, [0xcc, 0x46, 0x16, 0x32]);
    }
}
