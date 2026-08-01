//! Self-hosted HTTP composition root for the Orkia control plane.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use orkia_github::verify_webhook;
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};

#[derive(Clone)]
struct AppState {
    webhook_secret: Vec<u8>,
    service_token: Option<String>,
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
}
#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        webhook_secret: std::env::var("ORKIA_GITHUB_WEBHOOK_SECRET")
            .unwrap_or_default()
            .into_bytes(),
        service_token: std::env::var("ORKIA_SERVICE_TOKEN").ok(),
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/webhooks/github", post(github_webhook))
        .route("/v1/status", get(status))
        .with_state(state);
    let address: SocketAddr = std::env::var("ORKIA_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8787".into())
        .parse()
        .expect("valid ORKIA_BIND");
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("bind server");
    axum::serve(listener, app).await.expect("serve server");
}
async fn health() -> impl IntoResponse {
    axum::Json(Health {
        status: "ok",
        service: "orkia",
    })
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = &state.service_token {
        let authorized = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {token}"));
        if !authorized {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    axum::Json(Health {
        status: "ok",
        service: "orkia",
    })
    .into_response()
}
async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if state.webhook_secret.is_empty() || !verify_webhook(&state.webhook_secret, signature, &body) {
        return StatusCode::UNAUTHORIZED;
    }
    StatusCode::ACCEPTED
}
