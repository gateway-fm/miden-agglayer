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
        Some(mut entry) => {
            if (entry.mainnet_exit_root.is_none() || entry.rollup_exit_root.is_none())
                && let Some(l1_client) = &service.l1_client
            {
                match l1_client.fetch_exit_roots().await {
                    Ok((mainnet, rollup)) if crate::ger::combined_ger(&mainnet, &rollup) == ger => {
                        service
                            .store
                            .set_ger_exit_roots(&ger, mainnet, rollup)
                            .await
                            .map_err(|e| store_error(answer_id.clone(), e))?;
                        entry.mainnet_exit_root = Some(mainnet);
                        entry.rollup_exit_root = Some(rollup);
                    }
                    _ => { /* L1 stale or unreachable, return zeros */ }
                }
            }

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
    use crate::block_state::BlockState;
    use crate::l1_client::L1Client;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::test_accounts_config;
    use alloy::primitives::{Address, Bytes, TxHash};
    use alloy::rpc::types::Filter;
    use alloy_rpc_types_eth::{Log, ReceiptEnvelope, TransactionReceipt};
    use axum_jrpc::{Id, JsonRpcAnswer};
    use std::sync::Arc;

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

    fn make_service_with_l1(l1: Arc<dyn L1Client>) -> crate::service_state::ServiceState {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let block_state = Arc::new(BlockState::new());
        crate::service_state::ServiceState::new(
            crate::MidenClient::new_test(),
            test_accounts_config(),
            1,
            1,
            store,
            block_state,
            Some(l1),
            String::new(),
            String::new(),
        )
    }

    fn extract_result(resp: JsonRpcResponse) -> serde_json::Value {
        match resp.result {
            JsonRpcAnswer::Result(v) => v,
            JsonRpcAnswer::Error(e) => panic!("unexpected error: {e:?}"),
        }
    }

    // -- L1 client stub for lazy resolution tests --

    struct StubL1Client {
        exit_roots: ([u8; 32], [u8; 32]),
    }

    #[async_trait::async_trait]
    impl L1Client for StubL1Client {
        async fn eth_call(&self, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
            anyhow::bail!("unused")
        }
        async fn send_raw_transaction(&self, _raw: &str) -> anyhow::Result<String> {
            anyhow::bail!("unused")
        }
        async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])> {
            Ok(self.exit_roots)
        }
        async fn get_block_number(&self) -> anyhow::Result<u64> {
            anyhow::bail!("unused")
        }
        async fn get_logs(&self, _f: &Filter) -> anyhow::Result<Vec<Log>> {
            anyhow::bail!("unused")
        }
        async fn get_transaction_receipt(
            &self,
            _tx: TxHash,
        ) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope<Log>>>> {
            Ok(None)
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
    //
    // This is the core bug: before the fix, this returned zero roots
    // which poisoned bridge-service's database permanently.

    #[tokio::test]
    async fn test_exit_roots_returns_null_when_roots_unresolved() {
        let service = make_service();
        let store = service.store.clone();
        let ger = [0xAAu8; 32];

        // Simulate a GER that was injected without resolved exit roots
        // (the L1 race condition scenario).
        store
            .add_ger_update_event(1, [0u8; 32], "0xdead", &ger, None, None, 1000)
            .await
            .unwrap();

        let request = make_request(&format!("0x{}", hex::encode(ger)));
        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        // MUST be null — not a response with zero roots.
        // Bridge-service treats null as "retry later" but treats zero roots
        // as valid data and stores them permanently via ON CONFLICT DO NOTHING.
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

    // ── Test: lazy resolution from L1 when roots are missing ──────────

    #[tokio::test]
    async fn test_exit_roots_lazy_resolves_from_l1() {
        let mainnet = [0x33u8; 32];
        let rollup = [0x44u8; 32];
        let ger = crate::ger::combined_ger(&mainnet, &rollup);

        let l1 = Arc::new(StubL1Client {
            exit_roots: (mainnet, rollup),
        });
        let service = make_service_with_l1(l1);
        let store = service.store.clone();

        // GER exists but roots are None (race condition at injection time)
        store
            .add_ger_update_event(3, [0u8; 32], "0xcafe", &ger, None, None, 3000)
            .await
            .unwrap();

        let request = make_request(&format!("0x{}", hex::encode(ger)));
        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        // Lazy resolution should have fetched and verified the roots from L1
        let result = extract_result(resp);
        assert_eq!(
            result["mainnetExitRoot"],
            format!("0x{}", hex::encode(mainnet)),
            "lazy resolution should fill in mainnet root from L1"
        );
        assert_eq!(
            result["rollupExitRoot"],
            format!("0x{}", hex::encode(rollup)),
            "lazy resolution should fill in rollup root from L1"
        );

        // Roots should also be persisted in the store for future queries
        let entry = store.get_ger_entry(&ger).await.unwrap().unwrap();
        assert_eq!(entry.mainnet_exit_root, Some(mainnet));
        assert_eq!(entry.rollup_exit_root, Some(rollup));
    }

    // ── Test: lazy resolution returns null when L1 roots are stale ────

    #[tokio::test]
    async fn test_exit_roots_returns_null_when_l1_stale() {
        // L1 has moved on to different roots that don't match our GER
        let stale_mainnet = [0x55u8; 32];
        let stale_rollup = [0x66u8; 32];
        let l1 = Arc::new(StubL1Client {
            exit_roots: (stale_mainnet, stale_rollup),
        });
        let service = make_service_with_l1(l1);
        let store = service.store.clone();

        // Our GER was computed from different roots
        let ger = [0xBBu8; 32]; // won't match combined_ger(stale_mainnet, stale_rollup)

        store
            .add_ger_update_event(4, [0u8; 32], "0xface", &ger, None, None, 4000)
            .await
            .unwrap();

        let request = make_request(&format!("0x{}", hex::encode(ger)));
        let resp = service_zkevm_get_exit_roots_by_ger(service, request)
            .await
            .expect("should not be a JSON-RPC error");

        // L1 roots don't match → lazy resolution fails → must return null
        assert_eq!(
            extract_result(resp),
            serde_json::Value::Null,
            "stale L1 roots must not be returned; response must be null"
        );
    }
}
