//! Claim-aware `eth_estimateGas` (Cantina #21, PR #127 review).
//!
//! The official agglayer ClaimTxManager calls `eth_estimateGas` on the exact
//! `claimAsset` transaction BEFORE allocating a nonce or persisting a
//! monitored tx. On a real EVM chain, `AgglayerBridge._verifyLeaf` reads
//! `globalExitRootMap[combinedGER]` once and immediately reverts
//! `GlobalExitRootInvalid()` when it is zero — so a claim whose GER has not
//! propagated normally creates no transaction at all, and the manager simply
//! retries the estimate later.
//!
//! This is a compatibility shim, not a simulator: it only reads the landed
//! projection and the local synchronized Miden bridge account. A spent claim
//! returns `AlreadyClaimed()` before the GER check; otherwise an absent GER
//! returns `GlobalExitRootInvalid()`. No transaction is executed or proved.
//! The literal `execution reverted` prefix is load-bearing for ClaimTxManager.

use crate::claim::claimAssetCall;
use crate::hex::hex_decode_prefixed;
use crate::service_helpers::store_error;
use crate::service_state::ServiceState;
use alloy_core::sol_types::{SolCall, SolError};
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use serde::Deserialize;

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts — AgglayerBridge
    // (PolygonZkEVMBridgeV2) reverts with this custom error from
    // `_verifyLeaf` when `globalExitRootMap[combinedGER]` is unset.
    // Selector: 0x002f6fad.
    #[derive(Debug)]
    error GlobalExitRootInvalid();
    // Selector: 0x646cf558.
    #[derive(Debug)]
    error AlreadyClaimed();
}

/// The JSON-RPC error code geth uses for `execution reverted` responses
/// carrying revert data (EIP-1474 leaves this to the client; geth picked 3
/// and the ecosystem — including bridge-service — followed).
const EXECUTION_REVERTED_CODE: i32 = 3;

/// Build the geth-shaped `execution reverted: GlobalExitRootInvalid()` error:
/// message retains the load-bearing `execution reverted` prefix and `data`
/// carries the 4-byte Solidity custom-error selector (`0x002f6fad`).
pub(crate) fn global_exit_root_invalid_error() -> JsonRpcError {
    JsonRpcError::new(
        JsonRpcErrorReason::ApplicationError(EXECUTION_REVERTED_CODE),
        "execution reverted: GlobalExitRootInvalid()".to_string(),
        serde_json::Value::String(format!(
            "0x{}",
            alloy::hex::encode(GlobalExitRootInvalid::SELECTOR)
        )),
    )
}

pub(crate) fn already_claimed_error() -> JsonRpcError {
    JsonRpcError::new(
        JsonRpcErrorReason::ApplicationError(EXECUTION_REVERTED_CODE),
        "execution reverted: AlreadyClaimed()".to_string(),
        serde_json::Value::String(format!(
            "0x{}",
            alloy::hex::encode(AlreadyClaimed::SELECTOR)
        )),
    )
}

pub(crate) async fn service_estimate_gas(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();

    #[derive(Debug, Deserialize)]
    struct TransactionParam {
        data: Option<String>,
        input: Option<String>,
    }
    // `eth_estimateGas` params are `[txObject]` or `[txObject, blockTag]`;
    // parse leniently (only the first element matters) so both shapes and
    // any trailing state-override object are accepted.
    let params: Vec<serde_json::Value> = request.parse_params()?;
    let txn_param: TransactionParam = params
        .first()
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| {
            JsonRpcResponse::error(
                answer_id.clone(),
                JsonRpcError::new(
                    JsonRpcErrorReason::InvalidParams,
                    format!("eth_estimateGas: bad transaction object: {e}"),
                    serde_json::Value::Null,
                ),
            )
        })?
        .unwrap_or(TransactionParam {
            data: None,
            input: None,
        });

    if let Some(data_hex) = txn_param.data.or(txn_param.input)
        && let Ok(data) = hex_decode_prefixed(&data_hex)
        && data.starts_with(&claimAssetCall::SELECTOR)
        && let Ok(call) = claimAssetCall::abi_decode(&data)
    {
        let combined = crate::ger::combined_ger(&call.mainnetExitRoot.0, &call.rollupExitRoot.0);
        let (claimed, ger_applied) =
            crate::applied_state::claim_and_ger_applied(&service, call.globalIndex, &combined)
                .await
                .map_err(|error| store_error(answer_id.clone(), error))?;
        if claimed {
            ::metrics::counter!("rpc_estimate_gas_already_claimed_total").increment(1);
            return Err(JsonRpcResponse::error(answer_id, already_claimed_error()));
        }
        if !ger_applied {
            ::metrics::counter!("rpc_estimate_gas_ger_not_ready_total").increment(1);
            tracing::info!(
                global_index = %call.globalIndex,
                combined_ger = %alloy::hex::encode(combined),
                "eth_estimateGas(claimAsset): GER is not applied; returning GlobalExitRootInvalid()"
            );
            return Err(JsonRpcResponse::error(
                answer_id,
                global_exit_root_invalid_error(),
            ));
        }
    }

    // Legacy stub for everything else (and for admitted claims): gas is
    // meaningless on the synthetic chain.
    Ok(JsonRpcResponse::success(answer_id, "0x0"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_service;
    use alloy::primitives::{Address, FixedBytes, U256};
    use axum_jrpc::Id;
    use sha3::{Digest, Keccak256};

    /// The Solidity custom-error selector the review pinned: the first 4
    /// bytes of keccak256("GlobalExitRootInvalid()") must be 0x002f6fad.
    #[test]
    fn global_exit_root_invalid_selector_is_002f6fad() {
        let hash = Keccak256::digest(b"GlobalExitRootInvalid()");
        assert_eq!(&hash[..4], &[0x00, 0x2f, 0x6f, 0xad]);
        assert_eq!(GlobalExitRootInvalid::SELECTOR, [0x00, 0x2f, 0x6f, 0xad]);
    }

    #[test]
    fn already_claimed_selector_is_646cf558() {
        let hash = Keccak256::digest(b"AlreadyClaimed()");
        assert_eq!(&hash[..4], &[0x64, 0x6c, 0xf5, 0x58]);
        assert_eq!(AlreadyClaimed::SELECTOR, [0x64, 0x6c, 0xf5, 0x58]);
    }

    fn claim_calldata(mainnet: [u8; 32], rollup: [u8; 32]) -> String {
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(7u64),
            mainnetExitRoot: FixedBytes::from(mainnet),
            rollupExitRoot: FixedBytes::from(rollup),
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1,
            destinationAddress: Address::ZERO,
            amount: U256::from(1u64),
            metadata: Default::default(),
        }
        .abi_encode();
        format!("0x{}", alloy::hex::encode(calldata))
    }

    fn estimate_request(data_hex: &str) -> JsonRpcExtractor {
        JsonRpcExtractor {
            parsed: serde_json::json!([{ "from": "0x0000000000000000000000000000000000000001",
                                          "to": "0x0000000000000000000000000000000000000002",
                                          "data": data_hex }]),
            method: "eth_estimateGas".to_string(),
            id: Id::Num(1),
        }
    }

    /// Applied claim wins over GER readiness and returns the exact AggKit shim shape.
    #[tokio::test]
    async fn estimate_gas_already_claimed_reverts_with_aggkit_shape() {
        let service = create_test_service();
        service
            .store
            .mark_claim_note_processed(
                "already-applied".to_string(),
                U256::from(7u64).to_be_bytes::<32>(),
                1,
            )
            .await
            .unwrap();
        let request = estimate_request(&claim_calldata([0xAA; 32], [0xBB; 32]));

        let response = service_estimate_gas(service, request)
            .await
            .expect_err("applied claim must revert during estimate");
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["error"]["code"], 3);
        assert_eq!(
            json["error"]["message"],
            "execution reverted: AlreadyClaimed()"
        );
        assert_eq!(json["error"]["data"], "0x646cf558");
    }

    /// Missing GER returns GlobalExitRootInvalid() with the pinned selector.
    #[tokio::test]
    async fn estimate_gas_claim_missing_ger_reverts_with_selector() {
        let service = create_test_service();
        let req = estimate_request(&claim_calldata([0xAA; 32], [0xBB; 32]));

        let resp = service_estimate_gas(service, req)
            .await
            .expect_err("missing GER must produce a JSON-RPC error");
        let json = serde_json::to_value(&resp).unwrap();
        let err = &json["error"];
        let msg = err["message"].as_str().unwrap();
        assert!(
            msg.contains("execution reverted"),
            "bridge-service keys on the literal string; got: {msg}"
        );
        assert!(msg.contains("GlobalExitRootInvalid()"), "got: {msg}");
        assert_eq!(err["data"].as_str().unwrap(), "0x002f6fad");
        assert_eq!(err["code"].as_i64().unwrap(), 3, "geth revert code");
    }

    /// Published GER (projector committed the event → `is_ger_injected` =
    /// true) → the estimate succeeds with the legacy stub.
    #[tokio::test]
    async fn estimate_gas_claim_published_ger_succeeds() {
        let service = create_test_service();
        let ger = crate::ger::combined_ger(&[0xAA; 32], &[0xBB; 32]);
        service
            .store
            .commit_ger_event_atomic(1, [0u8; 32], "0xger-seed", &ger, None, None, 0)
            .await
            .unwrap();

        let req = estimate_request(&claim_calldata([0xAA; 32], [0xBB; 32]));
        let resp = service_estimate_gas(service, req)
            .await
            .expect("published GER must estimate successfully");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"].as_str().unwrap(), "0x0");
    }

    /// Visibility-barrier-held GER: the L1InfoTreeIndexer has SEEN the pair
    /// (`ger_entries` row exists / `has_seen_ger` = true) but the projector
    /// has intentionally NOT published the event yet (`is_ger_injected` =
    /// false). The estimate must keep reverting until publication — the gate
    /// is the published flag, not mere L1 observation.
    #[tokio::test]
    async fn estimate_gas_claim_seen_but_unpublished_ger_still_reverts() {
        let service = create_test_service();
        let ger = crate::ger::combined_ger(&[0x11; 32], &[0x22; 32]);
        // Seen (indexer pre-populated) but NOT injected/published.
        service
            .store
            .mark_ger_seen(
                &ger,
                crate::log_synthesis::GerEntry {
                    mainnet_exit_root: Some([0x11; 32]),
                    rollup_exit_root: Some([0x22; 32]),
                    block_number: 1,
                    timestamp: 0,
                },
            )
            .await
            .unwrap();
        assert!(service.store.has_seen_ger(&ger).await.unwrap());
        assert!(!service.store.is_ger_injected(&ger).await.unwrap());

        let req = estimate_request(&claim_calldata([0x11; 32], [0x22; 32]));
        let resp = service_estimate_gas(service.clone(), req)
            .await
            .expect_err("seen-but-unpublished GER must still revert");
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("execution reverted")
        );

        // Publication flips the verdict.
        service
            .store
            .commit_ger_event_atomic(2, [0u8; 32], "0xger-pub", &ger, None, None, 0)
            .await
            .unwrap();
        let req = estimate_request(&claim_calldata([0x11; 32], [0x22; 32]));
        service_estimate_gas(service, req)
            .await
            .expect("estimate must succeed once the GER event is published");
    }

    /// Non-claim calldata (and requests without calldata) keep the legacy
    /// `0x0` stub — only `claimAsset` is GER-gated.
    #[tokio::test]
    async fn estimate_gas_non_claim_calldata_returns_stub() {
        let service = create_test_service();
        // insertGlobalExitRoot calldata — not a claim, must not be gated.
        let calldata = crate::ger::insertGlobalExitRootCall {
            root: FixedBytes::from([0xCC; 32]),
        }
        .abi_encode();
        let req = estimate_request(&format!("0x{}", alloy::hex::encode(calldata)));
        let resp = service_estimate_gas(service.clone(), req).await.unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"].as_str().unwrap(), "0x0");

        // No data field at all.
        let req = JsonRpcExtractor {
            parsed: serde_json::json!([{ "to": "0x0000000000000000000000000000000000000002" }]),
            method: "eth_estimateGas".to_string(),
            id: Id::Num(2),
        };
        let resp = service_estimate_gas(service, req).await.unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"].as_str().unwrap(), "0x0");
    }
}
