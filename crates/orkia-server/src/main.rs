//! Self-hosted HTTP composition root for the Orkia control plane.

use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use orkia_git::LibGit2Repository;
use orkia_github::{parse_webhook, verify_webhook};
use orkia_identity::Identity;
use orkia_index_postgres::PostgresIndex;
use orkia_ledger::{Ledger, SystemClock};
use orkia_model::{
    ActorId, ChangeSetId, GrantRole, RepositoryId, RepositoryPolicy, SemanticObjectKind,
    SemanticObjectRef,
};
use orkia_ports::{LedgerStore, ReviewIndex, SecretStore};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct AppState {
    webhook_secret: Vec<u8>,
    service_token: Option<String>,
    /// Registry is process configuration only; every authorization decision is
    /// subsequently reconstructed from the selected repository's signed Git
    /// refs. Format: `UUID=/path/to/repo;UUID=/path/to/another`.
    repositories: BTreeMap<RepositoryId, std::path::PathBuf>,
    postgres_index: bool,
    /// A process-local delivery ledger is deliberately non-authoritative:
    /// Git remains the durable source of content. It prevents duplicate
    /// GitHub retries while a control-plane process is alive.
    github_deliveries: Arc<Mutex<BTreeSet<String>>>,
}

#[derive(Clone)]
struct FileSecrets {
    root: std::path::PathBuf,
}

impl SecretStore for FileSecrets {
    fn get(&self, key: &str) -> orkia_model::Result<Option<Vec<u8>>> {
        match std::fs::read(self.root.join(key)) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(orkia_model::OrkiaError::External(error.to_string())),
        }
    }

    fn put(&self, key: &str, value: &[u8]) -> orkia_model::Result<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| orkia_model::OrkiaError::External(error.to_string()))?;
        let path = self.root.join(key);
        std::fs::write(&path, value)
            .map_err(|error| orkia_model::OrkiaError::External(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| orkia_model::OrkiaError::External(error.to_string()))?;
        }
        Ok(())
    }
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    protocol: &'static str,
    git_authorization: bool,
    postgres_index: bool,
}
#[derive(Serialize)]
struct ChangeSetStatus {
    id: ChangeSetId,
    revision: u32,
    status: String,
    ready_for_integration: bool,
    stacks: Vec<VerifiedStack>,
    execution_order: Vec<ExecutionStep>,
}

#[derive(Serialize)]
struct VerifiedStack {
    repository: RepositoryId,
    stack: orkia_model::StackId,
    revision: u32,
    pull_request_count: usize,
    published: bool,
}

#[derive(Serialize)]
struct ExecutionStep {
    repository: RepositoryId,
    pull_request: orkia_model::StackPullRequestId,
    revision: u32,
    published: bool,
}
#[tokio::main]
async fn main() {
    let repositories = configured_repositories();
    let postgres_index = rebuild_postgres_index(&repositories)
        .expect("configured Postgres index must be reconstructible from Git ledgers");
    let state = Arc::new(AppState {
        webhook_secret: std::env::var("ORKIA_GITHUB_WEBHOOK_SECRET")
            .unwrap_or_default()
            .into_bytes(),
        service_token: std::env::var("ORKIA_SERVICE_TOKEN").ok(),
        repositories,
        postgres_index,
        github_deliveries: Arc::new(Mutex::new(BTreeSet::new())),
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/webhooks/github", post(github_webhook))
        .route("/v1/status", get(status))
        .route("/v1/changesets/{id}", get(change_set_status))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
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
async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(Health {
        status: "ok",
        service: "orkia",
        protocol: "orkia/v1",
        git_authorization: !state.repositories.is_empty(),
        postgres_index: state.postgres_index,
    })
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let service_authorized = service_token_authorized(&state, &headers);
    if state.service_token.is_some() && !service_authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !state.repositories.is_empty() && !service_authorized {
        let repository = if state.repositories.len() == 1 {
            state
                .repositories
                .values()
                .next()
                .expect("non-empty registry")
        } else {
            let Some(repository_id) = headers
                .get("x-orkia-repository")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<uuid::Uuid>().ok())
                .map(RepositoryId)
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Some(repository) = state.repositories.get(&repository_id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            repository
        };
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
            let content = match std::fs::read_to_string(&policy_path) {
                Ok(content) => content,
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            match orkia_policy::parse(&content) {
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
    axum::Json(Health {
        status: "ok",
        service: "orkia",
        protocol: "orkia/v1",
        git_authorization: !state.repositories.is_empty(),
        postgres_index: state.postgres_index,
    })
    .into_response()
}

/// Reconstructs a signed ChangeSet from its coordinator repository and proves
/// that each `{ repository, stack }` edge resolves to a signed stack in the
/// registered repository. The response deliberately exposes no mutable index:
/// this is a direct Git-ref verification suitable for control-plane polling.
async fn change_set_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let service_authorized = service_token_authorized(&state, &headers);
    if state.service_token.is_some() && !service_authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(uuid) = id.parse::<uuid::Uuid>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some((coordinator_id, coordinator_path)) = selected_repository(&state, &headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let actor = headers
        .get("x-orkia-actor")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<uuid::Uuid>().ok())
        .map(ActorId);
    if !service_authorized && actor.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let grants = grant_refs(&headers).collect::<Vec<_>>();
    let coordinator_policy = match policy_for(coordinator_path) {
        Ok(policy) => policy,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Ok(coordinator) = LibGit2Repository::open(coordinator_path) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !service_authorized
        && coordinator
            .semantic_store()
            .require_role(
                actor.as_ref().expect("actor checked above"),
                GrantRole::Reviewer,
                grants.iter().cloned(),
                &coordinator_policy,
            )
            .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let id = ChangeSetId(uuid);
    let Ok(Some((_, change_set))) = coordinator
        .semantic_store()
        .latest_changeset(&id, &coordinator_policy)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // ChangeSet edges coordinate delivery, rather than Git parentage. Every
    // dependency must nevertheless be a signed, forge-published delivery
    // group before this group can be reported as ready. Walk the complete
    // closure so a stale intermediate dependency cannot be hidden behind a
    // valid direct ref.
    let mut all_dependencies = BTreeMap::new();
    let mut pending = change_set.depends_on.iter().cloned().collect::<Vec<_>>();
    while let Some(dependency) = pending.pop() {
        if all_dependencies.contains_key(&dependency) {
            continue;
        }
        let Ok(Some((_, dependency_changeset))) = coordinator
            .semantic_store()
            .latest_changeset(&dependency, &coordinator_policy)
        else {
            return StatusCode::CONFLICT.into_response();
        };
        if !matches!(
            dependency_changeset.status,
            orkia_model::StackPullRequestStatus::Integrated
        ) {
            return StatusCode::CONFLICT.into_response();
        }
        pending.extend(dependency_changeset.depends_on.iter().cloned());
        all_dependencies.insert(dependency, dependency_changeset);
    }
    let mut delivery_groups = vec![change_set.clone()];
    delivery_groups.extend(all_dependencies.values().cloned());
    if orkia_changesets::changeset_execution_order(&delivery_groups).is_err() {
        return StatusCode::CONFLICT.into_response();
    }
    for dependency in all_dependencies.values() {
        match changeset_is_published(dependency, &state.repositories) {
            Ok(true) => {}
            Ok(false) => return StatusCode::CONFLICT.into_response(),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    let mut stacks = Vec::new();
    let mut ready_for_integration = true;
    let mut selected_pull_requests = Vec::new();
    let mut selected_metadata = BTreeMap::new();
    for reference in &change_set.stacks {
        let Some(repository_path) = state.repositories.get(&reference.repository) else {
            return StatusCode::CONFLICT.into_response();
        };
        let policy = match policy_for(repository_path) {
            Ok(policy) => policy,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
        let Ok(repository) = LibGit2Repository::open(repository_path) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if !service_authorized
            && repository
                .semantic_store()
                .require_role(
                    actor.as_ref().expect("actor checked above"),
                    GrantRole::Reviewer,
                    grants.iter().cloned(),
                    &policy,
                )
                .is_err()
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let Ok(Some((_, stack))) = repository.semantic_store().stack_at_revision(
            &reference.stack,
            reference.revision,
            &policy,
        ) else {
            return StatusCode::CONFLICT.into_response();
        };
        if stack.repository != reference.repository || stack.id != reference.stack {
            return StatusCode::CONFLICT.into_response();
        }
        let pull_request_revisions = if stack.pull_request_revisions.is_empty() {
            stack
                .pull_requests
                .iter()
                .map(|pull_request| (pull_request, 0))
                .collect::<Vec<_>>()
        } else {
            stack
                .pull_request_revisions
                .iter()
                .map(|(pull_request, revision)| (pull_request, *revision))
                .collect::<Vec<_>>()
        };
        let mut stack_published = true;
        for (pull_request, revision) in pull_request_revisions {
            let Ok(Some((_, selected_pull_request))) = repository
                .semantic_store()
                .stack_pull_request_at_revision(pull_request, revision, &policy)
            else {
                return StatusCode::CONFLICT.into_response();
            };
            let projection = match repository
                .semantic_store()
                .latest_projection_for_stack_pull_request_revision(pull_request, revision, &policy)
            {
                Ok(projection) => projection,
                Err(_) => return StatusCode::CONFLICT.into_response(),
            };
            let published = projection.as_ref().is_some_and(|(_, projection)| {
                orkia_changesets::projection_is_published_for(
                    projection,
                    &reference.repository,
                    pull_request,
                    revision,
                )
            });
            stack_published &= published;
            selected_metadata.insert(
                (reference.repository.clone(), pull_request.clone()),
                (revision, published),
            );
            selected_pull_requests.push(selected_pull_request);
        }
        ready_for_integration &= stack_published;
        stacks.push(VerifiedStack {
            repository: reference.repository.clone(),
            stack: stack.id,
            revision: stack.revision,
            pull_request_count: stack.pull_requests.len(),
            published: stack_published,
        });
    }
    let execution_order =
        match orkia_changesets::stack_pull_request_execution_order(&selected_pull_requests) {
            Ok(order) => order,
            Err(_) => return StatusCode::CONFLICT.into_response(),
        }
        .into_iter()
        .map(|(repository, pull_request)| {
            let Some((revision, published)) = selected_metadata
                .get(&(repository.clone(), pull_request.clone()))
                .copied()
            else {
                return Err(StatusCode::CONFLICT);
            };
            Ok(ExecutionStep {
                repository,
                pull_request,
                revision,
                published,
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>();
    let execution_order = match execution_order {
        Ok(order) => order,
        Err(status) => return status.into_response(),
    };
    let _ = coordinator_id;
    axum::Json(ChangeSetStatus {
        id: change_set.id,
        revision: change_set.revision,
        status: format!("{:?}", change_set.status).to_lowercase(),
        ready_for_integration,
        stacks,
        execution_order,
    })
    .into_response()
}

fn selected_repository<'a>(
    state: &'a AppState,
    headers: &HeaderMap,
) -> Option<(&'a RepositoryId, &'a std::path::PathBuf)> {
    if state.repositories.len() == 1 {
        return state.repositories.iter().next();
    }
    let id = headers
        .get("x-orkia-repository")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<uuid::Uuid>().ok())
        .map(RepositoryId)?;
    state.repositories.get_key_value(&id)
}

fn service_token_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    state.service_token.as_ref().is_some_and(|token| {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {token}"))
    })
}

fn grant_refs(headers: &HeaderMap) -> impl Iterator<Item = SemanticObjectRef> + '_ {
    headers
        .get("x-orkia-grants")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .filter(|value| !value.is_empty())
        .map(|hash| SemanticObjectRef {
            kind: SemanticObjectKind::Grant,
            hash: hash.into(),
        })
}

fn policy_for(repository: &std::path::Path) -> Result<RepositoryPolicy, orkia_model::OrkiaError> {
    let policy_path = repository.join("orkia.toml");
    if policy_path.exists() {
        let content = std::fs::read_to_string(&policy_path).map_err(|error| {
            orkia_model::OrkiaError::NotFound(format!("{}: {error}", policy_path.display()))
        })?;
        orkia_policy::parse(&content)
    } else {
        Ok(RepositoryPolicy::default())
    }
}

fn configured_repositories() -> BTreeMap<RepositoryId, std::path::PathBuf> {
    let mut repositories = BTreeMap::new();
    if let Ok(entries) = std::env::var("ORKIA_REPOSITORIES") {
        for entry in entries.split(';').filter(|entry| !entry.is_empty()) {
            let Some((id, path)) = entry.split_once('=') else {
                eprintln!("ignoring malformed ORKIA_REPOSITORIES entry");
                continue;
            };
            let Ok(id) = id.parse::<uuid::Uuid>() else {
                eprintln!("ignoring ORKIA_REPOSITORIES entry with invalid repository ID");
                continue;
            };
            repositories.insert(RepositoryId(id), path.into());
        }
    }
    // Compatibility for a single-repository deployment. Its durable ID is
    // read from the repository metadata generated by `orkia identity init`.
    if repositories.is_empty()
        && let Ok(path) = std::env::var("ORKIA_REPOSITORY")
    {
        let path = std::path::PathBuf::from(path);
        if let Ok(git) = git2::Repository::open(&path)
            && let Ok(bytes) = std::fs::read(git.path().join("orkia/repository.json"))
            && let Ok(id) = serde_json::from_slice::<RepositoryId>(&bytes)
        {
            repositories.insert(id, path);
        } else {
            eprintln!("ORKIA_REPOSITORY has no readable Orkia repository ID");
        }
    }
    repositories
}

/// Rebuilds the optional operational index from the signed Git ledger of every
/// registered repository.  No API handler reads this index for authorization
/// or semantic decisions; a failed/missing index is therefore recoverable by
/// restarting after the database is available.
fn rebuild_postgres_index(
    repositories: &BTreeMap<RepositoryId, std::path::PathBuf>,
) -> Result<bool, orkia_model::OrkiaError> {
    let Some(url) = std::env::var("ORKIA_POSTGRES_URL").ok() else {
        return Ok(false);
    };
    let mut events = Vec::new();
    for repository in repositories.values() {
        let git = LibGit2Repository::open(repository)?;
        events.extend(git.ledger_store().read_all()?);
    }
    let index = PostgresIndex::connect(&url)?;
    index.rebuild(&events)?;
    Ok(true)
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
    let event_name = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let event = match parse_webhook(event_name, delivery, &body) {
        Ok(event) => event,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let Some((repository_id, repository_path)) = selected_repository(&state, &headers) else {
        return StatusCode::BAD_REQUEST;
    };
    let mut deliveries = match state.github_deliveries.lock() {
        Ok(deliveries) => deliveries,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    if !deliveries.insert(delivery.to_owned()) {
        return StatusCode::NO_CONTENT;
    }
    let durable_duplicate = match record_webhook(repository_id, repository_path, &event) {
        Ok(duplicate) => duplicate,
        Err(_) => {
            deliveries.remove(delivery);
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };
    if durable_duplicate {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::ACCEPTED
    }
}

/// Persists an authenticated forge delivery in the selected repository's
/// signed ledger. The delivery ID is checked against the durable ledger as
/// well as the process-local retry set, so a server restart cannot replay the
/// same webhook into a second causal event.
fn record_webhook(
    repository_id: &RepositoryId,
    repository_path: &std::path::Path,
    event: &orkia_github::WebhookEvent,
) -> Result<bool, orkia_model::OrkiaError> {
    let git = LibGit2Repository::open(repository_path)?;
    let existing = git.ledger_store().read_all()?;
    if existing.iter().any(|record| {
        matches!(
            &record.unsigned.event,
            orkia_model::CaptureEvent::ForgeWebhook { delivery_id, .. }
                if delivery_id == &event.delivery_id
        )
    }) {
        return Ok(true);
    }
    let git_root = git2::Repository::open(repository_path)
        .map_err(|error| orkia_model::OrkiaError::External(error.to_string()))?
        .path()
        .to_path_buf();
    let actor: orkia_model::Actor =
        serde_json::from_slice(&std::fs::read(git_root.join("orkia/actor.json")).map_err(
            |error| orkia_model::OrkiaError::NotFound(format!("webhook actor: {error}")),
        )?)
        .map_err(|error| orkia_model::OrkiaError::Integrity(error.to_string()))?;
    let secrets = FileSecrets {
        root: git_root.join("orkia/keys"),
    };
    let identity = Identity::load(&secrets, "identity", actor)?
        .ok_or_else(|| orkia_model::OrkiaError::NotFound("webhook signing identity".into()))?;
    let ledger = Ledger::new(
        git.ledger_store(),
        SystemClock,
        repository_id.clone(),
        identity,
    );
    ledger.append(orkia_model::CaptureEvent::ForgeWebhook {
        forge: "github".into(),
        event_name: event.event_name.clone(),
        delivery_id: event.delivery_id.clone(),
        payload: event.payload.clone(),
    })?;
    Ok(false)
}

/// Reconstructs the exact Stack/StackPullRequest revisions selected by a
/// dependency ChangeSet and checks their projection publication state. This
/// helper deliberately returns a readiness fact only; the handler remains the
/// authority for HTTP status and reviewer authorization.
fn changeset_is_published(
    changeset: &orkia_model::ChangeSet,
    repositories: &BTreeMap<RepositoryId, std::path::PathBuf>,
) -> Result<bool, orkia_model::OrkiaError> {
    for reference in &changeset.stacks {
        let Some(path) = repositories.get(&reference.repository) else {
            return Ok(false);
        };
        let policy = policy_for(path)?;
        let repository = LibGit2Repository::open(path)?;
        let Some((_, stack)) = repository.semantic_store().stack_at_revision(
            &reference.stack,
            reference.revision,
            &policy,
        )?
        else {
            return Ok(false);
        };
        if stack.repository != reference.repository || stack.id != reference.stack {
            return Ok(false);
        }
        let revisions = if stack.pull_request_revisions.is_empty() {
            stack
                .pull_requests
                .iter()
                .map(|pull_request| (pull_request, 0))
                .collect::<Vec<_>>()
        } else {
            stack
                .pull_request_revisions
                .iter()
                .map(|(pull_request, revision)| (pull_request, *revision))
                .collect::<Vec<_>>()
        };
        for (pull_request, revision) in revisions {
            if repository
                .semantic_store()
                .stack_pull_request_at_revision(pull_request, revision, &policy)?
                .is_none()
            {
                return Ok(false);
            }
            let published = repository
                .semantic_store()
                .latest_projection_for_stack_pull_request_revision(pull_request, revision, &policy)?
                .is_some_and(|(_, projection)| {
                    orkia_changesets::projection_is_published_for(
                        &projection,
                        &reference.repository,
                        pull_request,
                        revision,
                    )
                });
            if !published {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_ledger::verify_chain;
    use std::collections::BTreeMap;

    #[test]
    fn webhook_is_signed_and_durably_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        git2::Repository::init(temporary.path()).unwrap();
        let git_root = git2::Repository::open(temporary.path())
            .unwrap()
            .path()
            .to_path_buf();
        let identity = Identity::generate("Orkia webhook");
        let secrets = FileSecrets {
            root: git_root.join("orkia/keys"),
        };
        // Save through the production identity API and provide the actor
        // document consumed by the server composition root.
        identity.save(&secrets, "identity").unwrap();
        std::fs::write(
            git_root.join("orkia/actor.json"),
            serde_json::to_vec(identity.actor()).unwrap(),
        )
        .unwrap();
        let repository = RepositoryId::new();
        let event = orkia_github::WebhookEvent {
            event_name: "pull_request".into(),
            delivery_id: "delivery-1".into(),
            payload: serde_json::json!({"action": "opened"}),
        };
        assert!(!record_webhook(&repository, temporary.path(), &event).unwrap());
        // A fresh call reconstructs the durable ledger instead of relying on
        // the process-local delivery cache.
        assert!(record_webhook(&repository, temporary.path(), &event).unwrap());
        let git = LibGit2Repository::open(temporary.path()).unwrap();
        let events = git.ledger_store().read_all().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].unsigned.event,
            orkia_model::CaptureEvent::ForgeWebhook { .. }
        ));
        verify_chain(
            &events,
            &BTreeMap::from([(identity.actor().id.clone(), identity.actor().clone())]),
        )
        .unwrap();
    }
}
