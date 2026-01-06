use crate::service_state::ServiceState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;

#[derive(Serialize)]
pub struct ClaimResponse {}

pub async fn claim_endpoint(State(_service): State<ServiceState>) -> Json<ClaimResponse> {
    Json(ClaimResponse {})
}
