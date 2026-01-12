use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::eips::Decodable2718;
use alloy_core::sol_types::SolCall;
use axum::Json;
use axum::extract::State;
use hex::FromHexError;
use serde::{Deserialize, Serialize};

fn hex_decode_prefixed(input: &str) -> Result<Vec<u8>, FromHexError> {
    hex::decode(input.strip_prefix("0x").unwrap_or(input))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    chain_id: String,
    input: String,
    to: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimResponse {}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L556
    #[derive(Debug)]
    function claimAsset(
        bytes32[32] calldata smtProofLocalExitRoot,
        bytes32[32] calldata smtProofRollupExitRoot,
        uint256 globalIndex,
        bytes32 mainnetExitRoot,
        bytes32 rollupExitRoot,
        uint32 originNetwork,
        address originTokenAddress,
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        bytes calldata metadata
    );
}

pub async fn claim_endpoint_dry_run(
    State(_service): State<ServiceState>,
    Json(request): Json<ClaimRequest>,
) -> Json<ClaimResponse> {
    tracing::debug!("chain_id: {:?}", request.chain_id);
    tracing::debug!("to: {:?}", request.to);

    let Ok(params_encoded) = hex_decode_prefixed(&request.input) else {
        return Json(ClaimResponse {});
    };
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        let params = claimAssetCall::abi_decode(&params_encoded);
        tracing::debug!("claimAsset call params: {:?}", params);
    } else {
        panic!("unhandled txn method {:?}", params_encoded);
    }

    Json(ClaimResponse {})
}

pub async fn claim_endpoint_raw_txn(
    _service: ServiceState,
    input: String,
) -> anyhow::Result<String> {
    tracing::debug!("input: {:?}", input);
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;

    match txn_envelope {
        TxEnvelope::Eip1559(txn_signed) => {
            let txn = txn_signed.tx();
            tracing::debug!("chain_id: {:?}", txn.chain_id);
            tracing::debug!("to: {:?}", txn.to);

            let params_encoded = &txn.input;
            if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
                let params = claimAssetCall::abi_decode(params_encoded)?;
                tracing::debug!("claimAsset call params: {:?}", params);
            } else {
                panic!("unhandled txn method {:?}", params_encoded);
            }
        },
        _ => {
            panic!("unhandled txn type {:?}", txn_envelope.tx_type());
        },
    }

    let txn_hash = "0xe670ec64341771606e55d6b4ca35a1a6b75ee3d5145a99d05921026d1527331";
    Ok(txn_hash.to_string())
}
