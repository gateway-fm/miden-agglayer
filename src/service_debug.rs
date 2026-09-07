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

        // OBSERVABILITY — this is the branch that answers aggkit's re-fetch of a
        // RECOVERED claim, and until now it said nothing.
        //
        // aggkit reads a claim's calldata with `debug_traceTransaction`, never with
        // `eth_getTransactionByHash`: `bridgesync/downloader.go:435` calls
        // `extractCallData` -> `extractRootCall` (`:743`), which issues
        // `debug_traceTransaction`. It uses `eth_getTransactionByHash` only in
        // `ExtractTxnAddresses` for a bridgeLeafTypeMessage BRIDGE event (`:170`).
        //
        // The sibling line in `service.rs` ("eth_getTransactionByHash: served stored
        // tx") therefore CANNOT fire for a claim re-fetch, and #148's step 4c was
        // grepping for it — so the recovered claim was being served correctly and
        // the test still measured zero. Log the exact hash here so the serve is
        // observable on the branch that actually performs it.
        let input_len = data.envelope.input().len();
        tracing::info!(
            "debug_traceTransaction: served stored tx {} (input_len={input_len})",
            format!("{hash:#x}")
        );
        ::metrics::counter!("debug_trace_stored_tx_served_total").increment(1);
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

    // Fallback for synthetic bridge-out txs. (A synthesized CLAIM tx does not reach this
    // fallback: its full authoritative claimAsset calldata is persisted under the derived
    // hash — `projection::persist_synthetic_claim_tx` — and served by the stored-envelope
    // branch above.)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TxnEntry;
    use crate::test_helpers::create_test_service;
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::Address;
    use axum_jrpc::{Id, JsonRpcAnswer};

    /// #148 step 4c — the recovered claim IS re-served, on the `debug_traceTransaction`
    /// store-first branch.
    ///
    /// aggkit reads a claim's calldata with `debug_traceTransaction`
    /// (`bridgesync/downloader.go:435` -> `extractRootCall:743`), never with
    /// `eth_getTransactionByHash` — which it uses only for a bridgeLeafTypeMessage
    /// BRIDGE event (`:170`). The e2e step was grepping the `eth_getTransactionByHash`
    /// log line and so measured zero serves while the proxy was answering correctly on
    /// this path. This pins the branch that actually answers: a stored envelope must
    /// come back with its REAL persisted calldata, not the synthetic `0x` fallback.
    #[tokio::test]
    async fn debug_trace_serves_stored_claim_calldata_not_the_empty_fallback() {
        let service = create_test_service();
        let block_state = service.block_state.clone();
        let txn_hash = TxHash::from([0xC1u8; 32]);
        let signer = Address::from([0x77u8; 20]);

        // A recovered claim's persisted claimAsset calldata.
        let calldata = alloy::primitives::Bytes::from(vec![0xAAu8; 132]);
        let tx = alloy::consensus::TxLegacy {
            input: calldata.clone(),
            ..Default::default()
        };
        let envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            tx,
            alloy::primitives::Signature::test_signature(),
            txn_hash,
        ));

        service
            .store
            .txn_begin(
                txn_hash,
                TxnEntry {
                    id: None,
                    envelope,
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        service
            .store
            .txn_commit(txn_hash, Ok(()), 77, block_state.get_block_hash(77))
            .await
            .unwrap();

        let request = JsonRpcExtractor {
            parsed: serde_json::json!([format!("{txn_hash:#x}"), {}]),
            method: "debug_traceTransaction".to_string(),
            id: Id::Num(1),
        };
        let response = service_debug_trace_transaction(service, request)
            .await
            .expect("trace must succeed");
        let JsonRpcAnswer::Result(value) = response.result else {
            panic!("debug_traceTransaction returned an error for a stored tx");
        };

        let expected = format!("0x{}", hex::encode(&calldata));
        assert_eq!(
            value["input"].as_str().unwrap(),
            expected,
            "the store-first branch must serve the RECOVERED calldata — an empty or \
             re-encoded input means aggkit gets bytes that are not the repaired claim"
        );
        assert_eq!(
            value["calls"][0]["input"].as_str().unwrap(),
            expected,
            "the inner DELEGATECALL is what aggkit's callTracer parser reads"
        );
    }
}
