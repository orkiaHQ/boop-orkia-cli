//! Pure StackPullRequest and stack domain logic.
//!
//! This crate deliberately has no Git, filesystem or forge dependency.  It
//! turns reviewed semantic atoms into durable causal work units and validates
//! their dependency closure before a projection adapter is allowed to run.

use orkia_model::{
    ChangeSet, ChangeSetId, ChangeSetStack, OrkiaError, Projection, ProjectionStatus, RepositoryId,
    Result, ReviewPlan, SEMANTIC_SCHEMA_VERSION, SessionId, Stack, StackId, StackPullRequest,
    StackPullRequestDependency, StackPullRequestId, StackPullRequestStatus,
};
use std::collections::{BTreeMap, BTreeSet};

/// Creates one StackPullRequest per approved review unit.  IDs derive only from the
/// session and the closed atom set, so they survive branch/commit rewrites.
pub fn from_review_plan(
    plan: &ReviewPlan,
    session: SessionId,
    repository: RepositoryId,
    base_commit: String,
) -> Result<Vec<StackPullRequest>> {
    if plan.atoms.is_empty() {
        return Err(OrkiaError::Invalid(
            "cannot derive stack pull requests without atoms".into(),
        ));
    }
    let atoms = plan
        .atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom))
        .collect::<BTreeMap<_, _>>();
    let unit_ids = plan
        .units
        .iter()
        .map(|unit| {
            let mut parts = vec![session.0.as_bytes().to_vec()];
            parts.extend(unit.atoms.iter().map(|atom| atom.0.as_bytes().to_vec()));
            let refs = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
            (
                unit.id.clone(),
                StackPullRequestId::from_stable_parts(&refs),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut pull_requests = Vec::new();
    for unit in &plan.units {
        let id = unit_ids[&unit.id].clone();
        let selected = unit
            .atoms
            .iter()
            .map(|atom| {
                atoms.get(atom).map(|atom| (*atom).clone()).ok_or_else(|| {
                    OrkiaError::Integrity("review unit references an absent atom".into())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let evidence = selected
            .iter()
            .flat_map(|atom| atom.source_events.iter().cloned())
            .collect::<BTreeSet<_>>();
        let parents = unit
            .depends_on
            .iter()
            .map(|dependency| {
                unit_ids.get(dependency).cloned().ok_or_else(|| {
                    OrkiaError::Integrity("review unit depends on an absent unit".into())
                })
            })
            .collect::<Result<BTreeSet<_>>>()?;
        pull_requests.push(StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id,
            revision: 0,
            source_plan: Some(plan.id.clone()),
            source_plan_revision: plan.revision,
            session: session.clone(),
            intent: None,
            repository: repository.clone(),
            base_commit: base_commit.clone(),
            parents,
            dependencies: BTreeSet::new(),
            atoms: selected,
            patches: Vec::new(),
            evidence,
            validations: Vec::new(),
            status: StackPullRequestStatus::Proposed,
            supersedes: None,
        });
    }
    validate_stack_pull_requests(&pull_requests)?;
    Ok(pull_requests)
}

/// Validates a closed, acyclic same-repository sub-DAG. Cross-repository
/// dependencies are retained as explicit blockers and do not get silently
/// projected as local parents.
pub fn validate_stack_pull_requests(pull_requests: &[StackPullRequest]) -> Result<()> {
    let by_id = pull_requests
        .iter()
        .map(|pull_request| (pull_request.id.clone(), pull_request))
        .collect::<BTreeMap<_, _>>();
    for pull_request in pull_requests {
        orkia_model::SemanticDocument::validate(pull_request)?;
        for parent in &pull_request.parents {
            let Some(parent_pull_request) = by_id.get(parent) else {
                return Err(OrkiaError::Integrity(
                    "stack omits a required local pull request parent".into(),
                ));
            };
            if parent_pull_request.repository != pull_request.repository {
                return Err(OrkiaError::Integrity(
                    "cross-repository dependencies must use the explicit dependency field".into(),
                ));
            }
        }
    }
    let _ = stack_pull_request_order(pull_requests)?;
    Ok(())
}

/// Returns the unique deterministic projection order, or rejects cycles and
/// missing local parents before any Git worktree is touched.
pub fn stack_pull_request_order(
    pull_requests: &[StackPullRequest],
) -> Result<Vec<StackPullRequestId>> {
    let by_id = pull_requests
        .iter()
        .map(|pull_request| (pull_request.id.clone(), pull_request))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = by_id.keys().cloned().collect::<BTreeSet<_>>();
    let mut complete = BTreeSet::new();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|id| {
                by_id[*id]
                    .parents
                    .iter()
                    .all(|parent| complete.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(OrkiaError::Conflict(
                "stack has a dependency cycle or a missing pull request parent".into(),
            ));
        }
        for id in ready {
            remaining.remove(&id);
            complete.insert(id.clone());
            ordered.push(id);
        }
    }
    Ok(ordered)
}

/// Computes the observable execution order across repositories. Both local
/// parent edges and explicit cross-repository dependency edges must be closed
/// by the supplied collection; absent external work is a hard error rather
/// than an implicit ordering guess.
pub fn stack_pull_request_execution_order(
    pull_requests: &[StackPullRequest],
) -> Result<Vec<(RepositoryId, StackPullRequestId)>> {
    let by_key = pull_requests
        .iter()
        .map(|pull_request| {
            (
                (pull_request.repository.clone(), pull_request.id.clone()),
                pull_request,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut remaining = by_key.keys().cloned().collect::<BTreeSet<_>>();
    let mut complete = BTreeSet::new();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|key| {
                let pull_request = by_key[*key];
                pull_request.parents.iter().all(|parent| {
                    complete.contains(&(pull_request.repository.clone(), parent.clone()))
                }) && pull_request.dependencies.iter().all(|dependency| {
                    complete.contains(&(
                        dependency.repository.clone(),
                        dependency.stack_pull_request.clone(),
                    ))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(OrkiaError::Conflict(
                "multi-repository changeset graph has a cycle or an unpublished dependency".into(),
            ));
        }
        for key in ready {
            remaining.remove(&key);
            complete.insert(key.clone());
            ordered.push(key);
        }
    }
    Ok(ordered)
}

/// Builds a reconstructible view over a validated, closed sub-DAG.
pub fn stack(pull_requests: &[StackPullRequest]) -> Result<Stack> {
    validate_stack_pull_requests(pull_requests)?;
    let repository = pull_requests
        .first()
        .map(|pull_request| pull_request.repository.clone())
        .ok_or_else(|| OrkiaError::Invalid("cannot create an empty stack".into()))?;
    if pull_requests
        .iter()
        .any(|pull_request| pull_request.repository != repository)
    {
        return Err(OrkiaError::Invalid(
            "a stack can contain stack PRs from exactly one repository".into(),
        ));
    }
    let all = pull_requests
        .iter()
        .map(|pull_request| pull_request.id.clone())
        .collect::<BTreeSet<_>>();
    let roots = pull_requests
        .iter()
        .filter(|pull_request| pull_request.parents.is_empty())
        .map(|pull_request| pull_request.id.clone())
        .collect::<BTreeSet<_>>();
    Ok(Stack {
        schema_version: SEMANTIC_SCHEMA_VERSION,
        id: StackId::from_stack_pull_requests(all.iter().cloned()),
        revision: 0,
        repository,
        pull_requests: all,
        pull_request_revisions: pull_requests
            .iter()
            .map(|pull_request| (pull_request.id.clone(), pull_request.revision))
            .collect(),
        roots,
        supersedes: None,
    })
}

/// Groups one or more repository-local stacks into a multi-repository
/// ChangeSet. A single stack is valid: it is the degenerate local case and
/// can later be extended with dependent stacks from other repositories.
pub fn change_set(stacks: &[Stack]) -> Result<ChangeSet> {
    if stacks.is_empty() {
        return Err(OrkiaError::Invalid(
            "a changeset needs at least one repository stack".into(),
        ));
    }
    let stack_refs = stacks
        .iter()
        .map(|stack| ChangeSetStack {
            repository: stack.repository.clone(),
            stack: stack.id.clone(),
            revision: stack.revision,
        })
        .collect::<BTreeSet<_>>();
    change_set_from_stack_references(stack_refs)
}

/// Creates a delivery ChangeSet from already-published stack identities. This
/// is the composition boundary for a coordinator that references stacks in
/// other repositories without importing their Git content.
pub fn change_set_from_stack_references(stack_refs: BTreeSet<ChangeSetStack>) -> Result<ChangeSet> {
    if stack_refs.is_empty() {
        return Err(OrkiaError::Invalid(
            "a changeset needs at least one repository stack".into(),
        ));
    }
    let unique = stack_refs
        .iter()
        .map(|reference| (reference.repository.clone(), reference.stack.clone()))
        .collect::<BTreeSet<_>>();
    if unique.len() != stack_refs.len() {
        return Err(OrkiaError::Invalid(
            "a changeset cannot select two revisions of the same stack".into(),
        ));
    }
    let change_set = ChangeSet {
        schema_version: SEMANTIC_SCHEMA_VERSION,
        id: ChangeSetId::from_stack_references(stack_refs.iter().cloned()),
        revision: 0,
        stacks: stack_refs,
        depends_on: BTreeSet::new(),
        status: StackPullRequestStatus::Active,
        supersedes: None,
    };
    orkia_model::SemanticDocument::validate(&change_set)?;
    Ok(change_set)
}

/// Orders multi-repository ChangeSets. Dependencies are between delivery
/// groups, not between PRs: each group can itself contain stacks from several
/// repositories.
pub fn changeset_execution_order(changesets: &[ChangeSet]) -> Result<Vec<ChangeSetId>> {
    let by_id = changesets
        .iter()
        .map(|changeset| (changeset.id.clone(), changeset))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = by_id.keys().cloned().collect::<BTreeSet<_>>();
    let mut complete = BTreeSet::new();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|id| {
                by_id[*id]
                    .depends_on
                    .iter()
                    .all(|parent| complete.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(OrkiaError::Conflict(
                "changeset graph has a cycle or a missing dependency".into(),
            ));
        }
        for id in ready {
            remaining.remove(&id);
            complete.insert(id.clone());
            ordered.push(id);
        }
    }
    Ok(ordered)
}

/// Adds an explicit dependency between two multi-repository delivery groups.
pub fn add_changeset_dependency(changeset: &mut ChangeSet, dependency: ChangeSetId) -> Result<()> {
    if dependency == changeset.id {
        return Err(OrkiaError::Invalid(
            "a changeset cannot depend on itself".into(),
        ));
    }
    changeset.depends_on.insert(dependency);
    orkia_model::SemanticDocument::validate(changeset)
}

/// Adds an explicit cross-repository dependency. It is never folded into a
/// local parent edge, which would fabricate a Git branch relationship.
pub fn add_stack_pull_request_dependency(
    pull_request: &mut StackPullRequest,
    dependency_id: StackPullRequestId,
    repository: RepositoryId,
) -> Result<()> {
    if dependency_id == pull_request.id || repository == pull_request.repository {
        return Err(OrkiaError::Invalid(
            "a cross-repository dependency must name another pull request and repository".into(),
        ));
    }
    pull_request
        .dependencies
        .insert(StackPullRequestDependency {
            stack_pull_request: dependency_id,
            repository,
        });
    orkia_model::SemanticDocument::validate(pull_request)
}

/// Returns whether a forge publication is for the exact immutable delivery
/// member selected by a Stack revision.  The lookup itself is performed by a
/// Git adapter, but the identity/revision invariant belongs to the domain so
/// every composition root applies the same fail-closed rule.
pub fn projection_is_published_for(
    projection: &Projection,
    repository: &RepositoryId,
    pull_request: &StackPullRequestId,
    pull_request_revision: u32,
) -> bool {
    projection.repository == *repository
        && projection.stack_pull_request == *pull_request
        && projection.stack_pull_request_revision == pull_request_revision
        && matches!(projection.status, ProjectionStatus::Published)
        && projection.commit.is_some()
        && projection
            .forge_pull_request
            .as_deref()
            .is_some_and(|url| !url.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::{
        AtomId, AtomKind, ChangeAtom, EventId, PlanId, PlanStatus, ReviewUnit, ReviewUnitId,
    };

    fn atom(index: u8) -> ChangeAtom {
        ChangeAtom {
            id: AtomId::from_stable_parts(&[&[index]]),
            kind: AtomKind::Symbol,
            path: format!("src/{index}.rs"),
            symbol: Some(format!("f{index}")),
            start_line: 1,
            end_line: 1,
            content_hash: format!("content-{index}"),
            source_events: BTreeSet::from([EventId::new()]),
        }
    }

    #[test]
    fn derives_stable_changesets_and_a_topological_stack() {
        let first = atom(1);
        let second = atom(2);
        let first_unit = ReviewUnit {
            id: ReviewUnitId::new(),
            title: "first".into(),
            atoms: BTreeSet::from([first.id.clone()]),
            depends_on: BTreeSet::new(),
            confidence_milli: 1000,
        };
        let second_unit = ReviewUnit {
            id: ReviewUnitId::new(),
            title: "second".into(),
            atoms: BTreeSet::from([second.id.clone()]),
            depends_on: BTreeSet::from([first_unit.id.clone()]),
            confidence_milli: 1000,
        };
        let plan = ReviewPlan {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: PlanId::new(),
            revision: 0,
            source_checkpoint: "base".into(),
            policy_digest: None,
            units: vec![first_unit, second_unit],
            atom_paths: BTreeMap::new(),
            atoms: vec![first, second],
            coverage_milli: 1000,
            status: PlanStatus::Proposed,
            created_from: BTreeSet::new(),
        };
        let session = SessionId::new();
        let repository = RepositoryId::new();
        let first_run =
            from_review_plan(&plan, session.clone(), repository.clone(), "base".into()).unwrap();
        let second_run = from_review_plan(&plan, session, repository, "base".into()).unwrap();
        assert_eq!(first_run, second_run);
        let order = stack_pull_request_order(&first_run).unwrap();
        assert_eq!(
            order,
            vec![first_run[0].id.clone(), first_run[1].id.clone()]
        );
        assert_eq!(
            stack(&first_run).unwrap().roots,
            BTreeSet::from([first_run[0].id.clone()])
        );
        assert_eq!(
            stack(&first_run).unwrap().pull_request_revisions,
            BTreeMap::from([
                (first_run[0].id.clone(), first_run[0].revision),
                (first_run[1].id.clone(), first_run[1].revision),
            ])
        );
        assert_eq!(
            stack_pull_request_execution_order(&first_run)
                .unwrap()
                .into_iter()
                .map(|(_, id)| id)
                .collect::<Vec<_>>(),
            order
        );
        let stack = stack(&first_run).unwrap();
        let change_set = change_set(std::slice::from_ref(&stack)).unwrap();
        assert_eq!(
            change_set.stacks,
            BTreeSet::from([ChangeSetStack {
                repository: stack.repository.clone(),
                stack: stack.id,
                revision: stack.revision,
            }])
        );
    }

    #[test]
    fn changeset_groups_stacks_from_different_repositories() {
        let first_pr = StackPullRequestId::new();
        let first = Stack {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: StackId::new(),
            revision: 0,
            repository: RepositoryId::new(),
            pull_requests: BTreeSet::from([first_pr.clone()]),
            pull_request_revisions: BTreeMap::from([(first_pr.clone(), 0)]),
            roots: BTreeSet::from([first_pr]),
            supersedes: None,
        };
        let second_pr = StackPullRequestId::new();
        let second = Stack {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: StackId::new(),
            revision: 0,
            repository: RepositoryId::new(),
            pull_requests: BTreeSet::from([second_pr.clone()]),
            pull_request_revisions: BTreeMap::from([(second_pr.clone(), 0)]),
            roots: BTreeSet::from([second_pr]),
            supersedes: None,
        };
        let change_set = change_set(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(
            change_set.stacks,
            BTreeSet::from([
                ChangeSetStack {
                    repository: first.repository.clone(),
                    stack: first.id,
                    revision: first.revision,
                },
                ChangeSetStack {
                    repository: second.repository.clone(),
                    stack: second.id,
                    revision: second.revision,
                },
            ])
        );
        assert_ne!(first.repository, second.repository);
    }

    #[test]
    fn changeset_identity_includes_the_repository_of_each_stack() {
        let shared_stack = StackId::new();
        let left = ChangeSetStack {
            repository: RepositoryId::new(),
            stack: shared_stack.clone(),
            revision: 0,
        };
        let right = ChangeSetStack {
            repository: RepositoryId::new(),
            stack: shared_stack,
            revision: 0,
        };
        let left_change_set = change_set_from_stack_references(BTreeSet::from([left])).unwrap();
        let right_change_set = change_set_from_stack_references(BTreeSet::from([right])).unwrap();
        assert_ne!(left_change_set.id, right_change_set.id);
    }

    #[test]
    fn cross_repository_pull_requests_have_explicit_topological_order() {
        let first_repository = RepositoryId::new();
        let second_repository = RepositoryId::new();
        let first_id = StackPullRequestId::new();
        let second_id = StackPullRequestId::new();
        let atom = || orkia_model::ChangeAtom {
            id: orkia_model::AtomId::new(),
            kind: orkia_model::AtomKind::Hunk,
            path: "change.txt".into(),
            symbol: None,
            start_line: 1,
            end_line: 1,
            content_hash: "hash".into(),
            source_events: BTreeSet::from([EventId::new()]),
        };
        let evidence = EventId::new();
        let mut upstream = StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: first_id.clone(),
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: first_repository.clone(),
            base_commit: "base".into(),
            parents: BTreeSet::new(),
            dependencies: BTreeSet::new(),
            atoms: vec![atom()],
            patches: Vec::new(),
            evidence: BTreeSet::from([evidence.clone()]),
            validations: Vec::new(),
            status: StackPullRequestStatus::Active,
            supersedes: None,
        };
        // The atom's source event must close over the PR evidence for model
        // validation; use the same event in the test fixture.
        upstream.atoms[0].source_events = BTreeSet::from([evidence]);
        let mut downstream = StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: second_id.clone(),
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: second_repository.clone(),
            base_commit: "base".into(),
            parents: BTreeSet::new(),
            dependencies: BTreeSet::from([StackPullRequestDependency {
                stack_pull_request: first_id.clone(),
                repository: first_repository.clone(),
            }]),
            atoms: vec![atom()],
            patches: Vec::new(),
            evidence: BTreeSet::new(),
            validations: Vec::new(),
            status: StackPullRequestStatus::Active,
            supersedes: None,
        };
        let downstream_event = EventId::new();
        downstream.evidence.insert(downstream_event.clone());
        downstream.atoms[0].source_events = BTreeSet::from([downstream_event]);
        let order = stack_pull_request_execution_order(&[downstream, upstream]).unwrap();
        assert_eq!(
            order,
            vec![(first_repository, first_id), (second_repository, second_id)]
        );
    }

    #[test]
    fn published_projection_must_match_repository_and_exact_pull_request_revision() {
        let repository = RepositoryId::new();
        let pull_request = StackPullRequestId::new();
        let projection = Projection {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: orkia_model::ProjectionId::new(),
            revision: 1,
            stack_pull_request: pull_request.clone(),
            stack_pull_request_revision: 3,
            repository: repository.clone(),
            branch: "orkia/stack-pr/example".into(),
            base_branch: "main".into(),
            base_commit: "0123456789012345678901234567890123456789".into(),
            commit: Some("1123456789012345678901234567890123456789".into()),
            forge_pull_request: Some("https://forge.test/pr/1".into()),
            status: ProjectionStatus::Published,
            supersedes: None,
        };
        assert!(projection_is_published_for(
            &projection,
            &repository,
            &pull_request,
            3
        ));
        assert!(!projection_is_published_for(
            &projection,
            &RepositoryId::new(),
            &pull_request,
            3
        ));
        assert!(!projection_is_published_for(
            &projection,
            &repository,
            &pull_request,
            2
        ));
    }
}
