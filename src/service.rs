use crate::claim_endpoint::claim_endpoint;
use crate::service_state::ServiceState;
use anyhow::Context;
use axum::Router;
use axum::routing::post;
use http::HeaderValue;
use miden_agglayer::COMPONENT;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use url::Url;

pub async fn serve(url: Url, state: ServiceState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/claim", post(claim_endpoint))
        .layer(
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::if_not_present(
                    http::header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                ))
                .layer(
                    CorsLayer::new()
                        .allow_origin(tower_http::cors::Any)
                        .allow_methods(tower_http::cors::Any)
                        .allow_headers([http::header::CONTENT_TYPE]),
                ),
        )
        .with_state(state);

    let listener = url
        .socket_addrs(|| None)
        .with_context(|| format!("failed to parse url {url}"))?;
    let listener = TcpListener::bind(&*listener)
        .await
        .with_context(|| format!("failed to bind TCP listener on {url}"))?;

    tracing::info!(target: COMPONENT, address = %url, "Service started");

    axum::serve(listener, app).await.map_err(Into::into)
}
