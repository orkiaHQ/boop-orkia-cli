//! Pure, fail-closed planning of Git projections for StackPullRequests.
//!
//! Patch application and worktree creation are infrastructure concerns. This
//! crate only establishes the deterministic order and the only admissible Git
//! parent for a single-parent pull request.

use orkia_changesets::stack_pull_request_order;
use orkia_model::{
    FileChange, OrkiaError, PatchFragment, Result, StackPullRequest, StackPullRequestId,
};
use orkia_ports::PatchProjectionRepository;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionStep {
    pub pull_request: StackPullRequestId,
    pub branch: String,
    pub parent_branch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedProjection {
    pub step: ProjectionStep,
    /// Exact parent consumed while materialising this revision. It is kept so
    /// the immutable projection document can be reconstructed without
    /// inspecting a mutable branch ref later.
    pub parent_commit: String,
    pub commit: String,
}

/// Derives context-bound patches for the atoms already assigned to a
/// StackPullRequest.  The line alignment is deterministic and keeps independent
/// symbols in the same file separate whenever their ranges do not overlap.
pub fn bind_patches(pull_request: &mut StackPullRequest, changes: &[FileChange]) -> Result<()> {
    let mut patches = Vec::new();
    for change in changes {
        let atoms = pull_request
            .atoms
            .iter()
            .filter(|atom| atom.path == change.path)
            .collect::<Vec<_>>();
        if atoms.is_empty() {
            continue;
        }
        let mut ranges = atoms
            .iter()
            .map(|atom| {
                (
                    atom.start_line.saturating_sub(1) as usize,
                    atom.end_line as usize,
                    atom.id.clone(),
                )
            })
            .collect::<Vec<_>>();
        ranges.sort_by_key(|(start, end, _)| (*start, *end));
        let aligned = align_lines(&change.old_content, &change.new_content);
        let mut grouped = Vec::<(usize, usize, BTreeSet<_>)>::new();
        for (start, end, atom) in ranges {
            if let Some((_, group_end, ids)) = grouped.last_mut()
                && start <= *group_end
            {
                *group_end = (*group_end).max(end);
                ids.insert(atom);
            } else {
                grouped.push((start, end, BTreeSet::from([atom])));
            }
        }
        for (start, end, atom_ids) in grouped {
            patches.push(fragment_for_range(change, &aligned, start, end, atom_ids)?);
        }
    }
    let covered = patches
        .iter()
        .flat_map(|patch| patch.atoms.iter().cloned())
        .collect::<BTreeSet<_>>();
    let expected = pull_request
        .atoms
        .iter()
        .map(|atom| atom.id.clone())
        .collect::<BTreeSet<_>>();
    if covered != expected {
        return Err(OrkiaError::Invalid(
            "every stack pull request atom must have a matching changed file for projection".into(),
        ));
    }
    pull_request.patches = patches;
    orkia_model::SemanticDocument::validate(pull_request)
}

/// Applies one StackPullRequest's patches against file content supplied by a Git
/// adapter. A patch is accepted only when its original region is found with
/// its recorded neighbouring context; ambiguous matches are conflicts.
pub fn apply_patches(
    files: &mut BTreeMap<String, String>,
    patches: &[PatchFragment],
) -> Result<()> {
    for patch in patches {
        let content = files.entry(patch.path.clone()).or_default();
        *content = apply_patch(content, patch)?;
    }
    Ok(())
}

/// Materializes exactly one StackPullRequest into its own branch using an isolated
/// Git index supplied by the adapter. The primary worktree is never read as a
/// source of unassigned changes.
pub fn project_stack_pull_request(
    repository: &dyn PatchProjectionRepository,
    pull_request: &StackPullRequest,
    branch: &str,
    parent_commit: &str,
) -> Result<String> {
    if pull_request.patches.is_empty() {
        return Err(OrkiaError::Invalid(
            "cannot project a stack pull request without exact bound patches".into(),
        ));
    }
    let paths = pull_request
        .patches
        .iter()
        .map(|patch| patch.path.clone())
        .collect::<BTreeSet<_>>();
    let mut files = repository.read_files_at(parent_commit, &paths)?;
    apply_patches(&mut files, &pull_request.patches)?;
    repository.commit_file_contents(branch, parent_commit, &files)
}

/// Reprojects a complete mono-repository stack in topological order. Every
/// descendant consumes the freshly produced parent commit, making this the
/// deterministic restack primitive used after an upstream amendment.
pub fn restack_mono_repository(
    repository: &dyn PatchProjectionRepository,
    pull_requests: &[StackPullRequest],
    base_branch: &str,
    base_commit: &str,
) -> Result<Vec<MaterializedProjection>> {
    let steps = plan_mono_repository(pull_requests, base_branch)?;
    let by_id = pull_requests
        .iter()
        .map(|pull_request| (pull_request.id.clone(), pull_request))
        .collect::<BTreeMap<_, _>>();
    let mut commits = BTreeMap::<String, String>::new();
    let mut materialized = Vec::new();
    for step in steps {
        let parent = commits
            .get(&step.parent_branch)
            .cloned()
            .unwrap_or_else(|| base_commit.into());
        let pull_request = by_id[&step.pull_request];
        let commit = project_stack_pull_request(repository, pull_request, &step.branch, &parent)?;
        commits.insert(step.branch.clone(), commit.clone());
        materialized.push(MaterializedProjection {
            step,
            parent_commit: parent,
            commit,
        });
    }
    Ok(materialized)
}

fn fragment_for_range(
    change: &FileChange,
    aligned: &[(usize, usize)],
    start: usize,
    end: usize,
    atoms: BTreeSet<orkia_model::AtomId>,
) -> Result<PatchFragment> {
    let new_lines = split_lines(&change.new_content);
    if start > new_lines.len() || end > new_lines.len() || start >= end {
        return Err(OrkiaError::Invalid(
            "atom range is outside changed file content".into(),
        ));
    }
    let old_lines = split_lines(&change.old_content);
    let previous = aligned
        .iter()
        .filter(|(_, new)| *new < start)
        .map(|(old, _)| *old)
        .next_back();
    let following = aligned
        .iter()
        .find(|(_, new)| *new >= end)
        .map(|(old, _)| *old);
    let old_start = previous.map_or(0, |index| index + 1);
    let old_end = following.unwrap_or(old_lines.len());
    let before = join_lines(&old_lines[old_start..old_end]);
    let after = join_lines(&new_lines[start..end]);
    // Context must only use LCS-aligned lines. Adjacent lines changed by a
    // sibling StackPullRequest would otherwise make two valid same-file patches
    // spuriously conflict during a restack.
    let before_context = previous
        .map(|index| join_lines(&old_lines[index..old_start]))
        .unwrap_or_default();
    let after_context = following
        .map(|index| join_lines(&old_lines[old_end..=index]))
        .unwrap_or_default();
    Ok(PatchFragment {
        atoms,
        path: change.path.clone(),
        before,
        after,
        before_context,
        after_context,
        base_content_hash: digest(&change.old_content),
    })
}

fn apply_patch(content: &str, patch: &PatchFragment) -> Result<String> {
    let mut candidates = Vec::new();
    if patch.before.is_empty() {
        for boundary in insertion_positions(content, &patch.before_context, &patch.after_context) {
            candidates.push((boundary, boundary));
        }
    } else {
        let mut offset = 0;
        while let Some(relative) = content[offset..].find(&patch.before) {
            let start = offset + relative;
            let end = start + patch.before.len();
            let left = &content[..start];
            let right = &content[end..];
            if left.ends_with(&patch.before_context) && right.starts_with(&patch.after_context) {
                candidates.push((start, end));
            }
            offset = end;
        }
    }
    if candidates.len() != 1 {
        return Err(OrkiaError::Conflict(format!(
            "patch for {} is not uniquely applicable ({})",
            patch.path,
            candidates.len()
        )));
    }
    let (start, end) = candidates[0];
    Ok(format!(
        "{}{}{}",
        &content[..start],
        patch.after,
        &content[end..]
    ))
}

fn insertion_positions(content: &str, before_context: &str, after_context: &str) -> Vec<usize> {
    if before_context.is_empty() && after_context.is_empty() {
        return vec![0];
    }
    let mut positions = Vec::new();
    for boundary in 0..=content.len() {
        if !content.is_char_boundary(boundary) {
            continue;
        }
        if content[..boundary].ends_with(before_context)
            && content[boundary..].starts_with(after_context)
        {
            positions.push(boundary);
        }
    }
    positions
}

fn align_lines(old: &str, new: &str) -> Vec<(usize, usize)> {
    let old = split_lines(old);
    let new = split_lines(new);
    let mut table = vec![vec![0_usize; new.len() + 1]; old.len() + 1];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            table[old_index][new_index] = if old[old_index] == new[new_index] {
                table[old_index + 1][new_index + 1] + 1
            } else {
                table[old_index + 1][new_index].max(table[old_index][new_index + 1])
            };
        }
    }
    let (mut old_index, mut new_index) = (0, 0);
    let mut output = Vec::new();
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            output.push((old_index, new_index));
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }
    output
}

fn split_lines(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

fn join_lines(lines: &[&str]) -> String {
    lines.concat()
}

fn digest(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// Plans a mono-repository stack. A Git PR has one base branch: a StackPullRequest
/// with more than one local parent is deliberately blocked until an explicit
/// closure StackPullRequest has been created by the caller.
pub fn plan_mono_repository(
    pull_requests: &[StackPullRequest],
    base_branch: &str,
) -> Result<Vec<ProjectionStep>> {
    plan_mono_repository_with_published_dependencies(pull_requests, base_branch, &BTreeSet::new())
}

/// Plans a local projection only after every explicit cross-repository
/// prerequisite has been observed as published by the caller. The ordinary
/// mono-repository entry point deliberately supplies an empty set, therefore
/// it fails closed instead of pretending an external dependency is local.
pub fn plan_mono_repository_with_published_dependencies(
    pull_requests: &[StackPullRequest],
    base_branch: &str,
    published_dependencies: &BTreeSet<orkia_model::StackPullRequestDependency>,
) -> Result<Vec<ProjectionStep>> {
    if base_branch.is_empty() {
        return Err(OrkiaError::Invalid(
            "projection base branch cannot be empty".into(),
        ));
    }
    let by_id = pull_requests
        .iter()
        .map(|pull_request| (pull_request.id.clone(), pull_request))
        .collect::<BTreeMap<_, _>>();
    let order = stack_pull_request_order(pull_requests)?;
    let mut branches = BTreeMap::new();
    let mut steps = Vec::new();
    for id in order {
        let pull_request = by_id[&id];
        if !pull_request.dependencies.is_subset(published_dependencies) {
            return Err(OrkiaError::Policy(format!(
                "stack pull request {} has unpublished cross-repository dependencies",
                pull_request.id.0
            )));
        }
        if pull_request.parents.len() > 1 {
            return Err(OrkiaError::Conflict(format!(
                "stack pull request {} has multiple local parents; create an explicit closure pull request before projecting a Git PR",
                pull_request.id.0
            )));
        }
        let branch = format!("orkia/stack-pr/{}", pull_request.id.0.simple());
        let parent_branch = pull_request
            .parents
            .iter()
            .next()
            .and_then(|parent| branches.get(parent).cloned())
            .unwrap_or_else(|| base_branch.into());
        branches.insert(pull_request.id.clone(), branch.clone());
        steps.push(ProjectionStep {
            pull_request: pull_request.id.clone(),
            branch,
            parent_branch,
        });
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_changesets::from_review_plan;
    use orkia_model::{
        AtomId, AtomKind, ChangeAtom, EventId, PlanId, PlanStatus, RepositoryId, ReviewPlan,
        ReviewUnit, ReviewUnitId, SEMANTIC_SCHEMA_VERSION, SessionId,
    };
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn projects_a_linear_stack_without_choosing_arbitrary_parents() {
        let event = EventId::new();
        let atom = |name: &str| ChangeAtom {
            id: AtomId::from_stable_parts(&[name.as_bytes()]),
            kind: AtomKind::Symbol,
            path: format!("{name}.rs"),
            symbol: Some(name.into()),
            start_line: 1,
            end_line: 1,
            content_hash: name.into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let first = atom("first");
        let second = atom("second");
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
            created_from: BTreeSet::from([event]),
        };
        let changesets =
            from_review_plan(&plan, SessionId::new(), RepositoryId::new(), "base".into()).unwrap();
        let steps = plan_mono_repository(&changesets, "main").unwrap();
        assert_eq!(steps[0].parent_branch, "main");
        assert_eq!(steps[1].parent_branch, steps[0].branch);
    }

    #[test]
    fn blocks_unpublished_cross_repository_dependencies() {
        let event = EventId::new();
        let atom = ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("feature".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "feature".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let mut pull_request = StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: StackPullRequestId::new(),
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: RepositoryId::new(),
            base_commit: "base".into(),
            parents: BTreeSet::new(),
            dependencies: BTreeSet::new(),
            atoms: vec![atom],
            patches: Vec::new(),
            evidence: BTreeSet::from([event]),
            validations: Vec::new(),
            status: orkia_model::StackPullRequestStatus::Proposed,
            supersedes: None,
        };
        let dependency = orkia_model::StackPullRequestDependency {
            stack_pull_request: StackPullRequestId::new(),
            repository: RepositoryId::new(),
        };
        pull_request.dependencies.insert(dependency.clone());
        assert!(plan_mono_repository(&[pull_request.clone()], "main").is_err());
        assert!(
            plan_mono_repository_with_published_dependencies(
                &[pull_request],
                "main",
                &BTreeSet::from([dependency]),
            )
            .is_ok()
        );
    }

    #[test]
    fn binds_and_applies_two_independent_changesets_in_the_same_file() {
        let event = EventId::new();
        let old = "fn first() { 1 }\n\nfn second() { 2 }\n";
        let new = "fn first() { 10 }\n\nfn second() { 20 }\n";
        let make = |id: StackPullRequestId, atom: ChangeAtom| StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id,
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: RepositoryId::new(),
            base_commit: "base".into(),
            parents: BTreeSet::new(),
            dependencies: BTreeSet::new(),
            atoms: vec![atom],
            patches: Vec::new(),
            evidence: BTreeSet::from([event.clone()]),
            validations: Vec::new(),
            status: orkia_model::StackPullRequestStatus::Proposed,
            supersedes: None,
        };
        let first = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"first"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("first".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "first".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let second = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"second"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("second".into()),
            start_line: 3,
            end_line: 3,
            content_hash: "second".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let change = FileChange {
            path: "src/lib.rs".into(),
            old_content: old.into(),
            new_content: new.into(),
            changed_start: 1,
            changed_end: 3,
        };
        let mut first_changeset = make(StackPullRequestId::new(), first);
        let mut second_changeset = make(StackPullRequestId::new(), second);
        bind_patches(&mut first_changeset, std::slice::from_ref(&change)).unwrap();
        bind_patches(&mut second_changeset, &[change]).unwrap();
        let mut files = BTreeMap::from([("src/lib.rs".into(), old.into())]);
        apply_patches(&mut files, &first_changeset.patches).unwrap();
        apply_patches(&mut files, &second_changeset.patches).unwrap();
        assert_eq!(files["src/lib.rs"], new);
    }

    #[test]
    fn projects_same_file_changesets_as_a_real_git_stack_without_touching_main() {
        let temp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        let old = "fn first() { 1 }\n\nfn second() { 2 }\n";
        let new = "fn first() { 10 }\n\nfn second() { 20 }\n";
        std::fs::write(temp.path().join("src/lib.rs"), old).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("src/lib.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Orkia", "orkia@example.test").unwrap();
        let base = repo
            .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
            .unwrap()
            .to_string();
        drop(tree);
        drop(index);
        drop(repo);

        let event = EventId::new();
        let make = |id: StackPullRequestId, atom: ChangeAtom| StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id,
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: SessionId::new(),
            intent: None,
            repository: RepositoryId::new(),
            base_commit: base.clone(),
            parents: BTreeSet::new(),
            dependencies: BTreeSet::new(),
            atoms: vec![atom],
            patches: Vec::new(),
            evidence: BTreeSet::from([event.clone()]),
            validations: Vec::new(),
            status: orkia_model::StackPullRequestStatus::Proposed,
            supersedes: None,
        };
        let first = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"real-first"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("first".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "first".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let second = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"real-second"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("second".into()),
            start_line: 3,
            end_line: 3,
            content_hash: "second".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let change = FileChange {
            path: "src/lib.rs".into(),
            old_content: old.into(),
            new_content: new.into(),
            changed_start: 1,
            changed_end: 3,
        };
        let mut first_changeset = make(StackPullRequestId::new(), first);
        let mut second_changeset = make(StackPullRequestId::new(), second);
        bind_patches(&mut first_changeset, std::slice::from_ref(&change)).unwrap();
        bind_patches(&mut second_changeset, &[change]).unwrap();

        let git = orkia_git::LibGit2Repository::open(temp.path()).unwrap();
        let first_commit =
            project_stack_pull_request(&git, &first_changeset, "orkia/stack-pr/first", &base)
                .unwrap();
        let second_commit = project_stack_pull_request(
            &git,
            &second_changeset,
            "orkia/stack-pr/second",
            &first_commit,
        )
        .unwrap();

        let repo = git2::Repository::open(temp.path()).unwrap();
        let read = |commit: &str| {
            let commit = repo
                .revparse_single(commit)
                .unwrap()
                .peel_to_commit()
                .unwrap();
            let entry = commit
                .tree()
                .unwrap()
                .get_path(std::path::Path::new("src/lib.rs"))
                .unwrap();
            String::from_utf8(repo.find_blob(entry.id()).unwrap().content().to_vec()).unwrap()
        };
        assert_eq!(read(&base), old, "main/base remains unchanged");
        assert_eq!(
            read(&first_commit),
            "fn first() { 10 }\n\nfn second() { 2 }\n"
        );
        assert_eq!(read(&second_commit), new);
        assert_eq!(
            repo.find_reference("refs/heads/orkia/stack-pr/second")
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            second_commit
        );
    }

    #[test]
    fn amending_an_upstream_stack_pull_request_reprojects_its_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        let base_content = "fn parent() { 1 }\n\nfn child() { 2 }\n";
        std::fs::write(temp.path().join("src/lib.rs"), base_content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("src/lib.rs")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Orkia", "orkia@example.test").unwrap();
        let base = repo
            .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
            .unwrap()
            .to_string();
        drop(tree);
        drop(index);
        drop(repo);

        let event = EventId::new();
        let repository = RepositoryId::new();
        let session = SessionId::new();
        let parent_id = StackPullRequestId::new();
        let child_id = StackPullRequestId::new();
        let make = |id: StackPullRequestId,
                    parents: BTreeSet<StackPullRequestId>,
                    atom: ChangeAtom| StackPullRequest {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id,
            revision: 0,
            source_plan: None,
            source_plan_revision: 0,
            session: session.clone(),
            intent: None,
            repository: repository.clone(),
            base_commit: base.clone(),
            parents,
            dependencies: BTreeSet::new(),
            atoms: vec![atom],
            patches: Vec::new(),
            evidence: BTreeSet::from([event.clone()]),
            validations: Vec::new(),
            status: orkia_model::StackPullRequestStatus::Active,
            supersedes: None,
        };
        let parent_atom = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"amended-parent"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("parent".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "parent".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let child_atom = ChangeAtom {
            id: AtomId::from_stable_parts(&[b"amended-child"]),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("child".into()),
            start_line: 3,
            end_line: 3,
            content_hash: "child".into(),
            source_events: BTreeSet::from([event.clone()]),
        };
        let first_target = "fn parent() { 10 }\n\nfn child() { 20 }\n";
        let first_change = FileChange {
            path: "src/lib.rs".into(),
            old_content: base_content.into(),
            new_content: first_target.into(),
            changed_start: 1,
            changed_end: 3,
        };
        let mut parent = make(parent_id.clone(), BTreeSet::new(), parent_atom);
        let mut child = make(
            child_id.clone(),
            BTreeSet::from([parent_id.clone()]),
            child_atom,
        );
        bind_patches(&mut parent, std::slice::from_ref(&first_change)).unwrap();
        bind_patches(&mut child, &[first_change]).unwrap();
        let git = orkia_git::LibGit2Repository::open(temp.path()).unwrap();
        let first =
            restack_mono_repository(&git, &[parent.clone(), child.clone()], "main", &base).unwrap();
        assert_eq!(first.len(), 2);

        let amended_target = "fn parent() { 11 }\n\nfn child() { 20 }\n";
        let amended_change = FileChange {
            path: "src/lib.rs".into(),
            old_content: base_content.into(),
            new_content: amended_target.into(),
            changed_start: 1,
            changed_end: 3,
        };
        parent.revision = 1;
        parent.patches.clear();
        bind_patches(&mut parent, std::slice::from_ref(&amended_change)).unwrap();
        let second =
            restack_mono_repository(&git, &[parent.clone(), child.clone()], "main", &base).unwrap();
        assert_eq!(second[0].step.pull_request, parent_id);
        assert_eq!(second[1].step.pull_request, child_id);
        assert_ne!(second[0].commit, first[0].commit);
        assert_ne!(second[1].commit, first[1].commit);

        let repo = git2::Repository::open(temp.path()).unwrap();
        let projected = repo
            .revparse_single(&second[1].commit)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let entry = projected
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("src/lib.rs"))
            .unwrap();
        assert_eq!(
            repo.find_blob(entry.id()).unwrap().content(),
            amended_target.as_bytes()
        );
        let base_commit = repo
            .revparse_single(&base)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let base_entry = base_commit
            .tree()
            .unwrap()
            .get_path(std::path::Path::new("src/lib.rs"))
            .unwrap();
        assert_eq!(
            repo.find_blob(base_entry.id()).unwrap().content(),
            base_content.as_bytes(),
            "restack never rewrites the source branch"
        );
    }
}
