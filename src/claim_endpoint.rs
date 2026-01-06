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

pub async fn claim_endpoint(
    State(_service): State<ServiceState>,
    Json(request): Json<ClaimRequest>,
) -> Json<ClaimResponse> {
    tracing::debug!("chain_id: {:?}", request.chain_id);
    tracing::debug!("to: {:?}", request.to);
    tracing::debug!("input: {:?}", request.input);
    Json(ClaimResponse {})
}
