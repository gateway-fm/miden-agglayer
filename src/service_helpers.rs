use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcResponse};

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

#[repr(i32)]
pub(crate) enum ServiceErrorCode {
    SendRawTransaction = 1,
    GetTransactionReceipt,
    AdminRegisterFaucet,
}

impl From<ServiceErrorCode> for JsonRpcErrorReason {
    fn from(value: ServiceErrorCode) -> Self {
        Self::ApplicationError(value as i32)
    }
}

/// Validate a hex-encoded 32-byte hash parameter from JSON-RPC requests.
///
/// Strips optional "0x" prefix, decodes hex, and verifies the result is exactly 32 bytes.
/// Returns a `JsonRpcResponse` error on failure (suitable for `?` in JrpcResult functions).
pub(crate) fn validate_hex_hash_param(
    hex_str: &str,
    field_name: &str,
    answer_id: axum_jrpc::Id,
) -> Result<[u8; 32], JsonRpcResponse> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(stripped).map_err(|_| {
        JsonRpcResponse::error(
            answer_id.clone(),
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                format!("bad {field_name}"),
                serde_json::Value::Null,
            ),
        )
    })?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        JsonRpcResponse::error(
            answer_id,
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                format!("{field_name} must be 32 bytes"),
                serde_json::Value::Null,
            ),
        )
    })?;
    Ok(arr)
}

pub(crate) fn store_error(answer_id: axum_jrpc::Id, e: anyhow::Error) -> JsonRpcResponse {
    JsonRpcResponse::error(
        answer_id,
        JsonRpcError::new(
            JsonRpcErrorReason::InternalError,
            format!("store error: {e}"),
            serde_json::Value::Null,
        ),
    )
}

pub(crate) fn json_rpc_response_from_result<T: serde::Serialize>(
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

pub(crate) fn build_synthetic_tx_json(
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

/// Encode `bridgeAsset(...)` calldata from a BridgeEvent synthetic log.
pub(crate) fn encode_bridge_asset_from_log(log: &crate::log_synthesis::SyntheticLog) -> String {
    let data_hex = log.data.strip_prefix("0x").unwrap_or(&log.data);
    let Ok(data_bytes) = hex::decode(data_hex) else {
        return "0x".to_string();
    };

    // BridgeEvent ABI field offsets (each field is padded to 32 bytes)
    const DEST_NET_OFFSET: usize = 3 * 32; // destinationNetwork
    const DEST_ADDR_OFFSET: usize = 4 * 32; // destinationAddress
    const AMOUNT_OFFSET: usize = 5 * 32; // amount
    const MIN_DATA_LEN: usize = 8 * 32;

    if data_bytes.len() < MIN_DATA_LEN {
        return "0x".to_string();
    }

    let dest_net = u32::from_be_bytes(
        data_bytes[DEST_NET_OFFSET + 28..DEST_NET_OFFSET + 32]
            .try_into()
            .unwrap_or([0; 4]),
    );
    let dest_addr: [u8; 20] = data_bytes[DEST_ADDR_OFFSET + 12..DEST_ADDR_OFFSET + 32]
        .try_into()
        .unwrap_or([0; 20]);
    let amount =
        alloy::primitives::U256::from_be_slice(&data_bytes[AMOUNT_OFFSET..AMOUNT_OFFSET + 32]);

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

        assert_eq!(json["type"], "0x0");
        assert_eq!(json["nonce"], "0x0");
        assert_eq!(json["gasPrice"], "0x0");
        assert_eq!(json["gas"], "0x0");
        assert_eq!(json["value"], "0x0");
        assert_eq!(json["input"], "0x");

        assert_eq!(json["from"], log.address);
        assert!(
            !json["blockHash"].is_null(),
            "blockHash must not be null for Go setSenderFromServer"
        );
        assert_eq!(json["blockNumber"], "0x64");
        assert_eq!(json["transactionIndex"], "0x0");

        assert_eq!(json["v"], "0x1b");
        assert_eq!(json["r"], "0x1");
        assert_eq!(json["s"], "0x1");

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
