use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::eips::Decodable2718;
use axum::Json;
use axum::extract::State;
use hex::FromHexError;
use serde::{Deserialize, Serialize};

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

pub async fn claim_endpoint_dry_run(
    State(_service): State<ServiceState>,
    Json(request): Json<ClaimRequest>,
) -> Json<ClaimResponse> {
    tracing::debug!("chain_id: {:?}", request.chain_id);
    tracing::debug!("to: {:?}", request.to);
    tracing::debug!("input: {:?}", request.input);
    Json(ClaimResponse {})
}

fn hex_decode_prefixed(input: &str) -> Result<Vec<u8>, FromHexError> {
    hex::decode(input.strip_prefix("0x").unwrap_or(input))
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
        },
        _ => {
            panic!("unhandled txn type {:?}", txn_envelope.tx_type());
        },
    }

    let txn_hash = "0xe670ec64341771606e55d6b4ca35a1a6b75ee3d5145a99d05921026d1527331";
    Ok(txn_hash.to_string())
}
