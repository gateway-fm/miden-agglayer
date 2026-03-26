use crate::service_helpers::store_error;
use crate::service_state::ServiceState;
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
    let ger =
        crate::service_helpers::validate_hex_hash_param(&params.0, "GER hash", answer_id.clone())?;

    match service
        .store
        .get_ger_entry(&ger)
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?
    {
        Some(entry) => {
            // If either root is still unresolved, return null so bridge-service
            // retries on the next sync cycle instead of permanently storing
            // fabricated zero roots. See docs/ger-decomposition.md.
            match (entry.mainnet_exit_root, entry.rollup_exit_root) {
                (Some(mainnet), Some(rollup)) => Ok(JsonRpcResponse::success(
                    answer_id,
                    serde_json::json!({
                        "blockNumber": format!("0x{:x}", entry.block_number),
                        "timestamp": format!("0x{:x}", entry.timestamp),
                        "mainnetExitRoot": format!("0x{}", hex::encode(mainnet)),
                        "rollupExitRoot": format!("0x{}", hex::encode(rollup)),
                    }),
                )),
                _ => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
                    answer_id,
                    serde_json::Value::Null,
                )),
            }
        }
        None => Ok(JsonRpcResponse::success::<serde_json::Value, _>(
            answer_id,
            serde_json::Value::Null,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_jrpc::{Id, JsonRpcAnswer};

    fn make_request(ger_hex: &str) -> JsonRpcExtractor {
        JsonRpcExtractor {
            parsed: serde_json::json!([ger_hex]),
            method: "zkevm_getExitRootsByGER".to_string(),
            id: Id::Num(1),
        }
    }

    fn make_service() -> crate::service_state::ServiceState {
        crate::test_helpers::create_test_service()
    }

    fn extract_result(resp: JsonRpcResponse) -> serde_json::Value {
        match resp.result {
            JsonRpcAnswer::Result(v) => v,
            JsonRpcAnswer::Error(e) => panic!("unexpected error: {e:?}"),
        }
    }

    // ── Test: unknown GER returns null ─────────────────────────────────

    #[tokio::test]
    async fn test_exit_roots_returns_null_for_unknown_ger() {
        let service = make_service();
        let ger_hex = format!("0x{}", hex::encode([0xFFu8; 32]));
        let request = make_request(&ger_hex);

        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        assert_eq!(extract_result(resp), serde_json::Value::Null);
    }

    // ── Test: GER exists but roots unresolved → must return null ───────

    #[tokio::test]
    async fn test_exit_roots_returns_null_when_roots_unresolved() {
        let service = make_service();
        let store = service.store.clone();
        let ger = [0xAAu8; 32];

        store
            .add_ger_update_event(1, [0u8; 32], "0xdead", &ger, None, None, 1000)
            .await
            .unwrap();

        let request = make_request(&format!("0x{}", hex::encode(ger)));
        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        assert_eq!(
            extract_result(resp),
            serde_json::Value::Null,
            "unresolved GER must return null, not fabricated zero roots"
        );
    }

    // ── Test: GER with resolved roots returns them correctly ──────────

    #[tokio::test]
    async fn test_exit_roots_returns_roots_when_resolved() {
        let service = make_service();
        let store = service.store.clone();

        let mainnet = [0x11u8; 32];
        let rollup = [0x22u8; 32];
        let ger = crate::ger::combined_ger(&mainnet, &rollup);

        store
            .add_ger_update_event(
                5,
                [0u8; 32],
                "0xbeef",
                &ger,
                Some(mainnet),
                Some(rollup),
                2000,
            )
            .await
            .unwrap();

        let request = make_request(&format!("0x{}", hex::encode(ger)));
        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        let result = extract_result(resp);
        assert_eq!(
            result["mainnetExitRoot"],
            format!("0x{}", hex::encode(mainnet))
        );
        assert_eq!(
            result["rollupExitRoot"],
            format!("0x{}", hex::encode(rollup))
        );
        assert_eq!(result["blockNumber"], "0x5");
    }
}
