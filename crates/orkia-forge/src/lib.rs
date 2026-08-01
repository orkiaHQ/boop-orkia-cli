//! Forge-neutral review projection planning.

use orkia_model::{ForgeReview, OrkiaError, Result, ReviewPlan, StackPullRequest};
use std::collections::{BTreeMap, BTreeSet};

pub fn projections(plan: &ReviewPlan, base_branch: &str) -> Result<Vec<ForgeReview>> {
    let by_id: BTreeMap<_, _> = plan
        .units
        .iter()
        .map(|unit| (unit.id.clone(), unit))
        .collect();
    let mut remaining: BTreeSet<_> = by_id.keys().cloned().collect();
    let mut projected = BTreeSet::new();
    let mut output = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<_> = remaining
            .iter()
            .filter(|id| {
                by_id[*id]
                    .depends_on
                    .iter()
                    .all(|dependency| projected.contains(dependency))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            return Err(OrkiaError::Conflict(
                "review plan has a dependency cycle".into(),
            ));
        }
        for id in ready {
            let unit = by_id[&id];
            if unit.depends_on.len() > 1 {
                return Err(OrkiaError::Conflict(format!(
                    "review unit {} has multiple parents; create an explicit closure unit before projecting a forge review",
                    unit.id.0
                )));
            }
            let parent = unit
                .depends_on
                .iter()
                .next()
                .and_then(|dependency| {
                    output
                        .iter()
                        .find(|review: &&ForgeReview| review.unit.as_ref() == Some(dependency))
                })
                .map(|review| review.branch.clone())
                .unwrap_or_else(|| base_branch.into());
            let branch = format!("orkia/{}/r{}", plan.id.0.simple(), unit.id.0.simple());
            output.push(ForgeReview {
                unit: Some(id.clone()),
                pull_request: None,
                branch,
                base: parent,
                title: unit.title.clone(),
                body: format!(
                    "Orkia review unit {} from signed plan {} revision {}.",
                    id.0, plan.id.0, plan.revision
                ),
            });
            remaining.remove(&id);
            projected.insert(id);
        }
    }
    Ok(output)
}

/// Builds forge reviews from durable StackPullRequests, never from mutable Git
/// commits or path-level plan units. A review can have one forge base only;
/// a multi-parent causal DAG must contain an explicit closure StackPullRequest.
pub fn stack_pull_request_projections(
    pull_requests: &[StackPullRequest],
    base_branch: &str,
) -> Result<Vec<ForgeReview>> {
    if base_branch.is_empty() {
        return Err(OrkiaError::Invalid(
            "forge base branch cannot be empty".into(),
        ));
    }
    let by_id = pull_requests
        .iter()
        .map(|pull_request| (pull_request.id.clone(), pull_request))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = by_id.keys().cloned().collect::<BTreeSet<_>>();
    let mut projected = BTreeSet::new();
    let mut branches = BTreeMap::new();
    let mut output = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|id| {
                by_id[*id]
                    .parents
                    .iter()
                    .all(|parent| projected.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(OrkiaError::Conflict(
                "stack has a dependency cycle or a missing local pull request parent".into(),
            ));
        }
        for id in ready {
            let pull_request = by_id[&id];
            if pull_request.parents.len() > 1 {
                return Err(OrkiaError::Conflict(format!(
                    "stack pull request {} has multiple parents; create an explicit closure pull request before publishing",
                    pull_request.id.0
                )));
            }
            let base = pull_request
                .parents
                .iter()
                .next()
                .and_then(|parent| branches.get(parent).cloned())
                .unwrap_or_else(|| base_branch.into());
            let branch = format!("orkia/stack-pr/{}", pull_request.id.0.simple());
            branches.insert(pull_request.id.clone(), branch.clone());
            output.push(ForgeReview {
                unit: None,
                pull_request: Some(pull_request.id.clone()),
                branch,
                base,
                title: format!("Orkia StackPullRequest {}", pull_request.id.0.simple()),
                body: format!(
                    "Causal StackPullRequest {} revision {}. This review is projected from signed captured evidence, not from a commit boundary.",
                    pull_request.id.0, pull_request.revision
                ),
            });
            remaining.remove(&id);
            projected.insert(id);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::{
        AtomId, AtomKind, ChangeAtom, EventId, RepositoryId, SEMANTIC_SCHEMA_VERSION, SessionId,
        StackPullRequestId, StackPullRequestStatus,
    };

    fn stack_pull_request(parent: Option<StackPullRequestId>) -> StackPullRequest {
        let event = EventId::new();
        StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: StackPullRequestId::new(),
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: RepositoryId::new(),
            base_commit: "base".into(),
            parents: parent.into_iter().collect(),
            dependencies: BTreeSet::new(),
            atoms: vec![ChangeAtom {
                id: AtomId::new(),
                kind: AtomKind::Symbol,
                path: "src/lib.rs".into(),
                symbol: None,
                start_line: 1,
                end_line: 1,
                content_hash: "content".into(),
                source_events: BTreeSet::from([event.clone()]),
            }],
            patches: Vec::new(),
            evidence: BTreeSet::from([event]),
            validations: Vec::new(),
            status: StackPullRequestStatus::Proposed,
            supersedes: None,
        }
    }

    #[test]
    fn stack_pull_requests_produce_a_true_forge_stack() {
        let first = stack_pull_request(None);
        let second = stack_pull_request(Some(first.id.clone()));
        let projections =
            stack_pull_request_projections(&[first.clone(), second.clone()], "main").unwrap();
        assert_eq!(projections[0].pull_request.as_ref(), Some(&first.id));
        assert_eq!(projections[1].base, projections[0].branch);
        assert_eq!(projections[1].pull_request.as_ref(), Some(&second.id));
    }
}
