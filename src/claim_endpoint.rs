use crate::service_state::ServiceState;
use axum::Json;
use axum::extract::State;
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

pub async fn claim_endpoint_raw_txn(
    _service: ServiceState,
    input: String,
) -> anyhow::Result<String> {
    tracing::debug!("input: {:?}", input);
    let txn_hash = "0xe670ec64341771606e55d6b4ca35a1a6b75ee3d5145a99d05921026d1527331";
    Ok(txn_hash.to_string())
}
