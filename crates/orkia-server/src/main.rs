//! Self-hosted HTTP composition root for the Orkia control plane.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use orkia_git::LibGit2Repository;
use orkia_github::verify_webhook;
use orkia_model::{ActorId, GrantRole, RepositoryPolicy, SemanticObjectKind, SemanticObjectRef};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct AppState {
    webhook_secret: Vec<u8>,
    service_token: Option<String>,
    repository: Option<std::path::PathBuf>,
    /// A process-local delivery ledger is deliberately non-authoritative:
    /// Git remains the durable source of content. It prevents duplicate
    /// GitHub retries while a control-plane process is alive.
    github_deliveries: Arc<Mutex<BTreeSet<String>>>,
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    protocol: &'static str,
    git_authorization: bool,
}
#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        webhook_secret: std::env::var("ORKIA_GITHUB_WEBHOOK_SECRET")
            .unwrap_or_default()
            .into_bytes(),
        service_token: std::env::var("ORKIA_SERVICE_TOKEN").ok(),
        repository: std::env::var("ORKIA_REPOSITORY").ok().map(Into::into),
        github_deliveries: Arc::new(Mutex::new(BTreeSet::new())),
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
        protocol: "orkia/v1",
        git_authorization: false,
    })
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(repository) = &state.repository {
        let Some(actor) = headers
            .get("x-orkia-actor")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<uuid::Uuid>().ok())
            .map(ActorId)
        else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let grants = headers
            .get("x-orkia-grants")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .split(',')
            .filter(|v| !v.is_empty())
            .map(|hash| SemanticObjectRef {
                kind: SemanticObjectKind::Grant,
                hash: hash.into(),
            });
        let policy_path = repository.join("orkia.toml");
        let policy = if policy_path.exists() {
            match orkia_policy::load(&policy_path) {
                Ok(policy) => policy,
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        } else {
            RepositoryPolicy::default()
        };
        let Ok(git) = LibGit2Repository::open(repository) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if git
            .semantic_store()
            .require_role(&actor, GrantRole::Reviewer, grants, &policy)
            .is_err()
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
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
        protocol: "orkia/v1",
        git_authorization: state.repository.is_some(),
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
    let Some(delivery) = headers
        .get("x-github-delivery")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    else {
        return StatusCode::BAD_REQUEST;
    };
    let mut deliveries = match state.github_deliveries.lock() {
        Ok(deliveries) => deliveries,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    if !deliveries.insert(delivery.to_owned()) {
        return StatusCode::NO_CONTENT;
    }
    StatusCode::ACCEPTED
}
