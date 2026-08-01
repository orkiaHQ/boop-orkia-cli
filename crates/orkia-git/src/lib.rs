//! The only crate permitted to access Git/libgit2.

use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use git2::build::CheckoutBuilder;
use git2::{
    Delta, DiffOptions, IndexAddOption, IndexEntry, IndexTime, ObjectType, Oid, Reference,
    Repository, Signature, StashFlags, StatusOptions, TreeWalkMode, TreeWalkResult,
    WorktreeAddOptions,
};
use orkia_identity::{Identity, verify};
use orkia_model::{
    AccessGrant, Attestation, GrantRevocation, GrantRole, Intent, KeyRotation, LedgerEvent, Memory,
    MergeOutcome, MergeResolution, Organization, OrkiaError, PlanId, RepositoryPolicy, Result,
    ReviewPlan, SemanticDocument, SemanticObjectKind, SemanticObjectRef, SemanticOperation,
    SemanticOperationAction, SemanticSignature, SemanticState, SessionId, Team, TrunkState,
    VaultEntry, ViewMetadata, valid_vault_name,
};
use orkia_ports::{GitRepository, LedgerStore, SemanticDocumentStore, SemanticObjectStore};
use orkia_semantic::{ChangedFile, delete_trunk, extract_trunk, merge_token_text};
use rand_core::{OsRng, RngCore};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// Legacy aggregate ledger location, retained only for automatic migration.
pub const LEGACY_LEDGER_ROOT_REF: &str = "refs/orkia/ledger";
/// Compatibility location for the former aggregate ledger blob.
pub const LEDGER_REF: &str = "refs/orkia/ledger/legacy";
/// Append-only event namespace. Every event owns one immutable Git blob.
pub const LEDGER_EVENT_REF_PREFIX: &str = "refs/orkia/ledger/events";
pub const SEMANTIC_OBJECT_REF_PREFIX: &str = "refs/orkia/objects";
pub const SEMANTIC_STATE_REF_PREFIX: &str = "refs/orkia/state";
/// Binding from a human-facing view name to its immutable metadata object.
pub const VIEW_REF_PREFIX: &str = "refs/orkia/views";
pub const ORKIA_REF_PREFIX: &str = "refs/orkia/";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingFileChange {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
    pub changed_start: u32,
    pub changed_end: u32,
}

/// A non-authoritative diagnostic projection for a semantic view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewStatus {
    pub name: String,
    pub branch: String,
    pub branch_tip: String,
    pub metadata_matches_branch: bool,
    pub working_tree_changes: usize,
    pub active_state: Option<SemanticObjectRef>,
    pub semantic_verified: bool,
    pub unpublished_operations: usize,
    pub semantic_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticMergeResult {
    pub resolution: SemanticObjectRef,
    pub result_commit: Option<String>,
    pub outcome: MergeOutcome,
}

/// Deterministic, offline-readable summary of two verified semantic states.
/// Git remains the byte-level source of truth; this adds stable Trunk and
/// operation identities so callers can explain a change without guessing from
/// line numbers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticDiff {
    pub base_commit: String,
    pub target_commit: String,
    pub changed_paths: Vec<String>,
    pub added_trunks: Vec<orkia_model::SemanticNodeId>,
    pub removed_trunks: Vec<orkia_model::SemanticNodeId>,
    pub changed_trunks: Vec<orkia_model::SemanticNodeId>,
    pub added_operations: Vec<SemanticObjectRef>,
    pub removed_operations: Vec<SemanticObjectRef>,
}

/// Provenance available for a path in one verified semantic state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticBlame {
    pub commit: String,
    pub path: String,
    pub trunks: Vec<orkia_model::SemanticTrunk>,
    pub operations: Vec<SemanticObjectRef>,
}

/// One Git history entry, annotated only when an active semantic state can be
/// verified for the commit. `state: None` is an explicit Git fallback, never
/// an inferred semantic history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticHistoryEntry {
    pub commit: String,
    pub state: Option<SemanticObjectRef>,
}

/// Content-addressed OCI layout produced from a verified Git commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxSeal {
    pub commit: String,
    pub state: SemanticObjectRef,
    pub manifest_digest: String,
    pub layer_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrkiaRefVerification {
    pub verified_states: usize,
}

#[derive(Clone, Debug)]
pub struct LibGit2Repository {
    path: PathBuf,
}

impl LibGit2Repository {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        Repository::open(&path).map_err(git_error)?;
        Ok(Self { path })
    }
    fn repo(&self) -> Result<Repository> {
        Repository::open(&self.path).map_err(git_error)
    }
    pub fn ledger_store(&self) -> GitLedgerStore {
        GitLedgerStore {
            repository: self.clone(),
        }
    }
    pub fn semantic_store(&self) -> GitSemanticStore {
        GitSemanticStore {
            repository: self.clone(),
        }
    }
    /// Extracts a complete semantic manifest from a committed Git tree.
    ///
    /// Git determines rename continuations through its similarity algorithm;
    /// Orkia then carries the prior Trunk identity into the new path.
    pub fn extract_semantic_state(
        &self,
        commit: &str,
        previous: Option<&SemanticState>,
    ) -> Result<SemanticState> {
        let repo = self.repo()?;
        let commit = repo
            .revparse_single(commit)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let tree = commit.tree().map_err(git_error)?;
        let parent = commit.parent(0).ok();
        match (previous, parent.as_ref()) {
            (Some(previous), Some(parent)) if previous.commit == parent.id().to_string() => {}
            (Some(previous), Some(_)) => {
                let previous_commit = repo
                    .find_commit(GitSemanticStore::oid(
                        &previous.commit,
                        "previous semantic state commit",
                    )?)
                    .map_err(git_error)?;
                if previous_commit.tree_id() != tree.id() {
                    return Err(OrkiaError::Invalid(
                        "previous semantic state must belong to the first Git parent or have an identical tree after rebase".into(),
                    ));
                }
            }
            (Some(_), None) => {}
            (None, _) => {}
        }
        let renamed_paths = match (previous, parent.as_ref()) {
            (Some(_), Some(parent)) => {
                renamed_paths(&repo, &parent.tree().map_err(git_error)?, &tree)?
            }
            _ => BTreeMap::new(),
        };
        let repository_anchor = repository_anchor(&repo, &commit)?;
        let mut files = BTreeMap::new();
        let mut entries = Vec::new();
        tree.walk(TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(ObjectType::Blob) {
                if let Some(name) = entry.name() {
                    entries.push((format!("{root}{name}"), entry.id()));
                }
            }
            TreeWalkResult::Ok
        })
        .map_err(git_error)?;
        let mut trunks = BTreeMap::new();
        for (path, oid) in entries {
            let blob = repo.find_blob(oid).map_err(git_error)?;
            let blob_id = oid.to_string();
            files.insert(path.clone(), blob_id.clone());
            let Ok(content) = std::str::from_utf8(blob.content()) else {
                continue;
            };
            let prior_path = renamed_paths.get(&path).map(String::as_str);
            let previous_trunk = previous.and_then(|state| {
                state.trunks.values().find(|trunk| {
                    trunk.paths.contains(&path)
                        || prior_path.is_some_and(|old_path| trunk.paths.contains(old_path))
                })
            });
            let changed_end = content.lines().count().max(1) as u32;
            let trunk = extract_trunk(
                &repository_anchor,
                &blob_id,
                &ChangedFile {
                    path: path.clone(),
                    content: content.into(),
                    changed_start: 1,
                    changed_end,
                    source_events: BTreeSet::new(),
                },
                previous_trunk,
            );
            trunks.insert(trunk.id.clone(), trunk);
        }
        if let Some(previous) = previous {
            for trunk in previous.trunks.values() {
                trunks
                    .entry(trunk.id.clone())
                    .or_insert_with(|| delete_trunk(trunk, false));
            }
        }
        let state = SemanticState {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            commit: commit.id().to_string(),
            parent_commits: commit.parent_ids().map(|id| id.to_string()).collect(),
            files,
            operations: BTreeSet::new(),
            trunks,
        };
        state.validate()?;
        Ok(state)
    }
    /// Materializes, signs and activates the semantic state of a Git commit.
    ///
    /// The first-parent state is reused when present, which lets Git rename
    /// detection preserve Trunk identities across consecutive commits.
    pub fn materialize_semantic_state(
        &self,
        commit: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let repo = self.repo()?;
        let commit_object = repo
            .revparse_single(commit)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let store = self.semantic_store();
        let mut parent_states = Vec::new();
        for parent_id in commit_object.parent_ids() {
            if let Some(reference) = store.state_for_commit(&parent_id.to_string())? {
                parent_states.push(store.get_state(&reference)?);
            }
        }
        let parent_previous = commit_object.parent_id(0).ok().and_then(|first_parent| {
            parent_states
                .iter()
                .find(|state| state.commit == first_parent.to_string())
                .cloned()
        });
        // After a Git rebase, the new first parent often has no semantic state
        // yet. A commit with the same Git tree is the deterministic, Git-native
        // continuation of the pre-rebase manifest; it preserves Trunk IDs
        // without inventing a second history graph.
        let rebase_previous = if parent_previous.is_none() {
            self.semantic_state_with_same_tree(&commit_object)?
        } else {
            None
        };
        let previous = parent_previous.or(rebase_previous);
        let mut state =
            self.extract_semantic_state(&commit_object.id().to_string(), previous.as_ref())?;
        if let Some(previous) = &previous {
            state.operations.extend(previous.operations.iter().cloned());
        }
        for parent_state in &parent_states {
            state
                .operations
                .extend(parent_state.operations.iter().cloned());
            for trunk in parent_state.trunks.values() {
                state
                    .trunks
                    .entry(trunk.id.clone())
                    .or_insert_with(|| delete_trunk(trunk, true));
            }
        }
        let first_parent_trunks = previous
            .as_ref()
            .map(|state| state.trunks.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let inherited_from_other_parent = parent_states
            .iter()
            .flat_map(|state| state.trunks.keys().cloned())
            .filter(|id| !first_parent_trunks.contains(id))
            .collect::<BTreeSet<_>>();
        for operation in transition_operations(previous.as_ref(), &state) {
            if operation_target(&operation)
                .is_some_and(|id| inherited_from_other_parent.contains(id))
            {
                continue;
            }
            let reference = store.put_operation(&operation)?;
            store.sign_document(&reference, identity)?;
            state.operations.insert(reference);
        }
        let state_ref = store.put_state(&state)?;
        store.sign_document(&state_ref, identity)?;
        store.activate_state(&state.commit, &state_ref, policy)?;
        Ok(state_ref)
    }

    /// Imports an existing Git history into the semantic overlay in
    /// deterministic parent-before-child order. Existing active states are
    /// retained so this is safe to resume after interruption.
    pub fn import_semantic_history(
        &self,
        target: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<Vec<SemanticObjectRef>> {
        let repo = self.repo()?;
        let target = repo
            .revparse_single(target)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut walk = repo.revwalk().map_err(git_error)?;
        walk.push(target.id()).map_err(git_error)?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(git_error)?;
        let commits: Vec<_> = walk
            .map(|id| id.map_err(git_error).map(|id| id.to_string()))
            .collect::<Result<_>>()?;
        drop(target);
        drop(repo);
        let store = self.semantic_store();
        let mut imported = Vec::new();
        for commit in commits {
            if let Some(state) = store.state_for_commit(&commit)? {
                store.verify_active_state(&state, policy)?;
                imported.push(state);
            } else {
                imported.push(self.materialize_semantic_state(&commit, identity, policy)?);
            }
        }
        Ok(imported)
    }

    /// Performs a verified Git three-way merge and records its semantic
    /// decision. Git remains authoritative for the resulting tree; Orkia
    /// refuses to merge semantically when a participating manifest is absent
    /// or fails its signature quorum.
    pub fn semantic_merge(
        &self,
        left: &str,
        right: &str,
        branch: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticMergeResult> {
        let repo = self.repo()?;
        let left = repo
            .revparse_single(left)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let right = repo
            .revparse_single(right)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let base_id = repo.merge_base(left.id(), right.id()).map_err(git_error)?;
        let base = repo.find_commit(base_id).map_err(git_error)?;
        let left_id = left.id().to_string();
        let right_id = right.id().to_string();
        let store = self.semantic_store();
        let mut operations = BTreeSet::new();
        for commit in [&base, &left, &right] {
            let state_ref = store
                .state_for_commit(&commit.id().to_string())?
                .ok_or_else(|| {
                    OrkiaError::NotFound(format!(
                        "active semantic state required for merge commit {}",
                        commit.id()
                    ))
                })?;
            operations.extend(store.verify_active_state(&state_ref, policy)?.operations);
        }
        let mut index = repo
            .merge_trees(
                &base.tree().map_err(git_error)?,
                &left.tree().map_err(git_error)?,
                &right.tree().map_err(git_error)?,
                None,
            )
            .map_err(git_error)?;
        if index.has_conflicts() && !resolve_token_conflicts(&repo, &mut index)? {
            let resolution = MergeResolution {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                base_commit: base.id().to_string(),
                left_commit: left.id().to_string(),
                right_commit: right.id().to_string(),
                result_commit: None,
                outcome: MergeOutcome::Conflict,
                operations,
                supersedes: None,
            };
            let resolution = store.put_merge_resolution(&resolution)?;
            store.sign_document(&resolution, identity)?;
            return Ok(SemanticMergeResult {
                resolution,
                result_commit: None,
                outcome: MergeOutcome::Conflict,
            });
        }
        let tree_id = index.write_tree_to(&repo).map_err(git_error)?;
        let tree = repo.find_tree(tree_id).map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now(&identity.actor().display_name, "orkia@local"))
            .map_err(git_error)?;
        let commit = repo
            .commit(
                None,
                &signature,
                &signature,
                "orkia: semantic merge",
                &tree,
                &[&left, &right],
            )
            .map_err(git_error)?;
        repo.reference(
            &format!("refs/heads/{branch}"),
            commit,
            false,
            "Create Orkia semantic merge branch",
        )
        .map_err(git_error)?;
        drop(tree);
        drop(index);
        drop(base);
        drop(left);
        drop(right);
        drop(repo);
        self.materialize_semantic_state(&commit.to_string(), identity, policy)?;
        let resolution = store.put_merge_resolution(&MergeResolution {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            base_commit: base_id.to_string(),
            left_commit: left_id,
            right_commit: right_id,
            result_commit: Some(commit.to_string()),
            outcome: MergeOutcome::Merged,
            operations,
            supersedes: None,
        })?;
        store.sign_document(&resolution, identity)?;
        Ok(SemanticMergeResult {
            resolution,
            result_commit: Some(commit.to_string()),
            outcome: MergeOutcome::Merged,
        })
    }

    /// Records a human-resolved Git merge as the signed successor of a
    /// previously recorded semantic conflict. The caller performs the actual
    /// edit in normal Git tooling; Orkia proves that the resulting commit has
    /// the exact conflicting parents and a verified semantic state.
    pub fn finalize_merge_resolution(
        &self,
        conflict: &SemanticObjectRef,
        result_commit: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        if conflict.kind != SemanticObjectKind::Resolution {
            return Err(OrkiaError::Invalid(
                "only a merge resolution can be finalized".into(),
            ));
        }
        let store = self.semantic_store();
        store.require_signature_quorum(conflict, policy)?;
        let original = store.get_merge_resolution(conflict)?;
        if !matches!(original.outcome, MergeOutcome::Conflict) {
            return Err(OrkiaError::Policy(
                "only an unresolved merge conflict can be finalized".into(),
            ));
        }
        let repo = self.repo()?;
        let commit = repo
            .revparse_single(result_commit)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let parents = commit
            .parents()
            .map(|parent| parent.id().to_string())
            .collect::<BTreeSet<_>>();
        if parents != BTreeSet::from([original.left_commit.clone(), original.right_commit.clone()])
        {
            return Err(OrkiaError::Integrity(
                "resolved commit must have exactly the conflicting left and right parents".into(),
            ));
        }
        let result = commit.id().to_string();
        drop(commit);
        drop(repo);
        let state = store.state_for_commit(&result)?.ok_or_else(|| {
            OrkiaError::NotFound(format!(
                "active semantic state for resolved commit {result}"
            ))
        })?;
        store.verify_active_state(&state, policy)?;
        let resolution = store.put_merge_resolution(&MergeResolution {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            base_commit: original.base_commit,
            left_commit: original.left_commit,
            right_commit: original.right_commit,
            result_commit: Some(result),
            outcome: MergeOutcome::Merged,
            operations: original.operations,
            supersedes: Some(conflict.clone()),
        })?;
        store.sign_document(&resolution, identity)?;
        Ok(resolution)
    }

    fn semantic_state_with_same_tree(
        &self,
        commit: &git2::Commit<'_>,
    ) -> Result<Option<SemanticState>> {
        let repo = self.repo()?;
        let tree = commit.tree().map_err(git_error)?.id();
        let store = self.semantic_store();
        let mut candidates = Vec::new();
        for reference in repo
            .references_glob(&format!("{SEMANTIC_STATE_REF_PREFIX}/*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let Some(target) = reference.target() else {
                continue;
            };
            let state = match store.get_state(&SemanticObjectRef {
                kind: SemanticObjectKind::State,
                hash: target.to_string(),
            }) {
                Ok(state) => state,
                Err(_) => continue,
            };
            if state.commit == commit.id().to_string() {
                continue;
            }
            let Ok(previous_commit) = repo.find_commit(GitSemanticStore::oid(
                &state.commit,
                "semantic state commit",
            )?) else {
                continue;
            };
            if previous_commit.tree_id() == tree {
                candidates.push(state);
            }
        }
        candidates.sort_by(|left, right| left.commit.cmp(&right.commit));
        Ok(candidates.into_iter().next())
    }
    pub fn project_branch(&self, branch: &str, target: &str) -> Result<()> {
        let repo = self.repo()?;
        let object = repo.revparse_single(target).map_err(git_error)?;
        repo.reference(
            &format!("refs/heads/{branch}"),
            object.id(),
            true,
            "Orkia review projection",
        )
        .map_err(git_error)?;
        Ok(())
    }

    /// Creates a Git branch for a semantic view and publishes its metadata in
    /// the Orkia namespace. Git remains the authority for checkout and commit
    /// history; the metadata only describes the semantic projection.
    pub fn create_view(&self, view: &ViewMetadata) -> Result<SemanticObjectRef> {
        view.validate()?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        let view_ref = view_ref_name(&view.name)?;
        if !Reference::is_valid_name(&branch_ref) {
            return Err(OrkiaError::Invalid(format!(
                "view branch is not a valid Git ref: {}",
                view.branch
            )));
        }
        let repo = self.repo()?;
        let base = GitSemanticStore::oid(&view.base_commit, "view base commit")?;
        repo.find_commit(base).map_err(git_error)?;
        repo.reference(&branch_ref, base, false, "Create Orkia semantic view")
            .map_err(git_error)?;

        let store = self.semantic_store();
        let object = store.put_view(view)?;
        let oid = GitSemanticStore::oid(&object.hash, "view metadata hash")?;
        if let Err(error) = repo.reference(&view_ref, oid, false, "Bind Orkia semantic view") {
            // A view has no useful meaning if its metadata cannot be bound.
            // We intentionally leave the just-created Git branch intact: it
            // is a normal Git artifact and can be recovered or reused.
            return Err(git_error(error));
        }
        Ok(object)
    }

    /// Creates a child view pinned to the current immutable revision of its
    /// parent. The child's Git base cannot precede or diverge from that parent.
    pub fn create_child_view(
        &self,
        view: &ViewMetadata,
        parent_name: &str,
    ) -> Result<SemanticObjectRef> {
        if view.parent.is_some() {
            return Err(OrkiaError::Invalid(
                "child view parent must be selected by name, not supplied as an object ref".into(),
            ));
        }
        let (parent_ref, parent) = self.view_with_object(parent_name)?;
        let repo = self.repo()?;
        let child_base = GitSemanticStore::oid(&view.base_commit, "view base commit")?;
        let parent_base = GitSemanticStore::oid(&parent.base_commit, "parent view base commit")?;
        if child_base != parent_base
            && !repo
                .graph_descendant_of(child_base, parent_base)
                .map_err(git_error)?
        {
            return Err(OrkiaError::Policy(format!(
                "child view base {} must descend from parent view {} base {}",
                view.base_commit, parent.name, parent.base_commit
            )));
        }
        let child = ViewMetadata {
            parent: Some(parent_ref),
            ..view.clone()
        };
        self.create_view(&child)
    }

    /// Resolves a view name to its validated immutable metadata document.
    pub fn view(&self, name: &str) -> Result<ViewMetadata> {
        Ok(self.view_with_object(name)?.1)
    }

    fn view_with_object(&self, name: &str) -> Result<(SemanticObjectRef, ViewMetadata)> {
        let repo = self.repo()?;
        let reference = repo
            .find_reference(&view_ref_name(name)?)
            .map_err(|error| {
                if error.code() == git2::ErrorCode::NotFound {
                    OrkiaError::NotFound(format!("semantic view {name}"))
                } else {
                    git_error(error)
                }
            })?;
        let target = reference.target().ok_or_else(|| {
            OrkiaError::Integrity(format!("semantic view {name} is a symbolic ref"))
        })?;
        let object = SemanticObjectRef {
            kind: SemanticObjectKind::View,
            hash: target.to_string(),
        };
        let view = self.semantic_store().get_view(&object)?;
        Ok((object, view))
    }

    /// Advances a view only to a descendant commit whose semantic state and
    /// complete operation closure satisfy the repository signature policy.
    pub fn advance_view(
        &self,
        name: &str,
        target: &str,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        self.advance_view_with_scope(name, target, policy, true)
    }

    /// The single mutation path for a view ref. `require_draft` is kept
    /// private so only `publish_shared_view` can advance a Shared view after
    /// it has checked the actor's signed grant.
    fn advance_view_with_scope(
        &self,
        name: &str,
        target: &str,
        policy: &RepositoryPolicy,
        require_draft: bool,
    ) -> Result<SemanticObjectRef> {
        let (_, current) = self.view_with_object(name)?;
        if require_draft && matches!(current.scope, orkia_model::ViewScope::Shared) {
            return Err(OrkiaError::Policy(format!(
                "Shared view {name} must be updated through publish_shared_view with a verified grant"
            )));
        }
        let repo = self.repo()?;
        let target = repo
            .revparse_single(target)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let branch_ref = format!("refs/heads/{}", current.branch);
        let branch = repo.find_reference(&branch_ref).map_err(|error| {
            OrkiaError::Integrity(format!(
                "semantic view {} references missing branch {}: {error}",
                current.name, current.branch
            ))
        })?;
        let branch_tip = branch.target().ok_or_else(|| {
            OrkiaError::Integrity(format!("semantic view {name} branch is symbolic"))
        })?;
        if target.id() != branch_tip
            && !repo
                .graph_descendant_of(target.id(), branch_tip)
                .map_err(git_error)?
        {
            return Err(OrkiaError::Policy(format!(
                "view {name} may only advance to a descendant of {branch_tip}"
            )));
        }

        let store = self.semantic_store();
        let state_ref = store
            .state_for_commit(&target.id().to_string())?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!("active semantic state for commit {}", target.id()))
            })?;
        let state = store.verify_active_state(&state_ref, policy)?;

        let next = ViewMetadata {
            base_commit: target.id().to_string(),
            visible_operations: state.operations,
            ..current
        };
        let next_ref = store.put_view(&next)?;
        let view_ref = view_ref_name(name)?;
        let metadata_oid = GitSemanticStore::oid(&next_ref.hash, "view metadata hash")?;
        // The branch is moved only after all semantic validation and metadata
        // persistence have succeeded. A subsequent Git ref error leaves an
        // immutable metadata blob that can be safely retried or audited.
        repo.reference(&view_ref, metadata_oid, true, "Advance Orkia semantic view")
            .map_err(git_error)?;
        repo.reference(
            &branch_ref,
            target.id(),
            true,
            "Advance Orkia semantic view branch",
        )
        .map_err(git_error)?;
        Ok(next_ref)
    }

    /// Explicit publication path for Shared views. Unlike Draft updates, this
    /// requires a policy-trusted grant for the actor making the publication.
    pub fn publish_shared_view(
        &self,
        name: &str,
        target: &str,
        actor: &orkia_model::ActorId,
        grants: BTreeSet<SemanticObjectRef>,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let view = self.view(name)?;
        if !matches!(view.scope, orkia_model::ViewScope::Shared) {
            return Err(OrkiaError::Invalid(
                "publish is only valid for Shared views; use update for a Draft view".into(),
            ));
        }
        self.semantic_store().require_role(
            actor,
            GrantRole::SharedViewMaintainer,
            grants,
            policy,
        )?;
        self.advance_view_with_scope(name, target, policy, false)
    }

    /// Repository-scoped publication path. Callers that know their stable
    /// repository identifier must use this form so a grant limited to that
    /// repository is neither ignored nor usable elsewhere.
    pub fn publish_shared_view_for_repository(
        &self,
        name: &str,
        target: &str,
        actor: &orkia_model::ActorId,
        grants: BTreeSet<SemanticObjectRef>,
        repository: &str,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let view = self.view(name)?;
        if !matches!(view.scope, orkia_model::ViewScope::Shared) {
            return Err(OrkiaError::Invalid(
                "publish is only valid for Shared views; use update for a Draft view".into(),
            ));
        }
        self.semantic_store().require_role_for_repository(
            actor,
            GrantRole::SharedViewMaintainer,
            grants,
            repository,
            policy,
        )?;
        self.advance_view_with_scope(name, target, policy, false)
    }

    /// Deletes the Git refs that name a view, without deleting commits or
    /// semantic blobs. Draft views may be removed directly; Shared views need
    /// an explicit caller opt-in. A checked-out branch is always protected.
    pub fn delete_view(&self, name: &str, allow_shared: bool) -> Result<()> {
        let (_, view) = self.view_with_object(name)?;
        if matches!(view.scope, orkia_model::ViewScope::Shared) && !allow_shared {
            return Err(OrkiaError::Policy(format!(
                "refusing to delete Shared view {name} without explicit force"
            )));
        }
        let repo = self.repo()?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        self.ensure_view_branch_is_not_checked_out(&repo, &branch_ref)?;

        let mut branch = repo.find_reference(&branch_ref).map_err(|error| {
            OrkiaError::Integrity(format!(
                "semantic view {} references missing branch {}: {error}",
                view.name, view.branch
            ))
        })?;
        let mut metadata = repo
            .find_reference(&view_ref_name(name)?)
            .map_err(git_error)?;
        // Git content remains recoverable from existing commits/reflogs and
        // semantic blobs remain under refs/orkia/objects. Only the two names
        // that make this view active are removed.
        branch.delete().map_err(git_error)?;
        metadata.delete().map_err(git_error)
    }

    /// Reports Git and Orkia state independently so callers can tell a normal
    /// dirty worktree from a missing, stale or unverified semantic state.
    pub fn view_status(&self, name: &str, policy: &RepositoryPolicy) -> Result<ViewStatus> {
        let (_, view) = self.view_with_object(name)?;
        let repo = self.repo()?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        let branch_tip = repo
            .find_reference(&branch_ref)
            .map_err(|error| {
                OrkiaError::Integrity(format!(
                    "semantic view {} references missing branch {}: {error}",
                    view.name, view.branch
                ))
            })?
            .target()
            .ok_or_else(|| {
                OrkiaError::Integrity(format!("semantic view {name} branch is symbolic"))
            })?;
        let mut options = StatusOptions::new();
        options.include_untracked(true).recurse_untracked_dirs(true);
        let working_tree_changes = repo.statuses(Some(&mut options)).map_err(git_error)?.len();

        let store = self.semantic_store();
        let mut status = ViewStatus {
            name: view.name.clone(),
            branch: view.branch.clone(),
            branch_tip: branch_tip.to_string(),
            metadata_matches_branch: view.base_commit == branch_tip.to_string(),
            working_tree_changes,
            active_state: None,
            semantic_verified: false,
            unpublished_operations: 0,
            semantic_error: None,
        };
        match store.state_for_commit(&branch_tip.to_string()) {
            Ok(Some(state_ref)) => {
                status.active_state = Some(state_ref.clone());
                match store.verify_active_state(&state_ref, policy) {
                    Ok(state) => {
                        status.semantic_verified = true;
                        status.unpublished_operations = state
                            .operations
                            .difference(&view.visible_operations)
                            .count();
                    }
                    Err(error) => status.semantic_error = Some(error.to_string()),
                }
            }
            Ok(None) => {
                status.semantic_error = Some("no active semantic state for branch tip".into())
            }
            Err(error) => status.semantic_error = Some(error.to_string()),
        }
        Ok(status)
    }

    /// Replaces the visible root operations of a view without moving its Git
    /// branch. The filter is valid only for the branch tip's verified state.
    pub fn set_view_visible_operations(
        &self,
        name: &str,
        operations: BTreeSet<SemanticObjectRef>,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        if operations
            .iter()
            .any(|operation| operation.kind != SemanticObjectKind::Operation)
        {
            return Err(OrkiaError::Invalid(
                "a view filter may contain only semantic operations".into(),
            ));
        }
        let (_, current) = self.view_with_object(name)?;
        let repo = self.repo()?;
        let branch_tip = repo
            .find_reference(&format!("refs/heads/{}", current.branch))
            .map_err(git_error)?
            .target()
            .ok_or_else(|| OrkiaError::Integrity(format!("view {name} branch is symbolic")))?;
        if current.base_commit != branch_tip.to_string() {
            return Err(OrkiaError::Policy(format!(
                "view {name} metadata is stale; advance it before changing its operation filter"
            )));
        }
        let store = self.semantic_store();
        let state_ref = store
            .state_for_commit(&branch_tip.to_string())?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!("active semantic state for commit {branch_tip}"))
            })?;
        let state = store.verify_active_state(&state_ref, policy)?;
        if !operations.is_subset(&state.operations) {
            return Err(OrkiaError::Policy(
                "view filter contains operations outside the active semantic state".into(),
            ));
        }
        let next = ViewMetadata {
            visible_operations: operations,
            ..current
        };
        let next_ref = store.put_view(&next)?;
        repo.reference(
            &view_ref_name(name)?,
            GitSemanticStore::oid(&next_ref.hash, "view metadata hash")?,
            true,
            "Set Orkia view operation filter",
        )
        .map_err(git_error)?;
        Ok(next_ref)
    }

    /// Saves uncommitted work in Git's normal stash namespace, after proving
    /// that the requested view is the branch checked out in this worktree.
    pub fn stash_view(&self, name: &str, message: &str, include_untracked: bool) -> Result<String> {
        let view = self.view(name)?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        let mut repo = self.repo()?;
        if repo.head().map_err(git_error)?.name() != Some(branch_ref.as_str()) {
            return Err(OrkiaError::Policy(format!(
                "view {name} must be checked out before stashing its work"
            )));
        }
        let signature = repo
            .signature()
            .or_else(|_| Signature::now("Orkia", "orkia@local"))
            .map_err(git_error)?;
        let flags = include_untracked.then_some(StashFlags::INCLUDE_UNTRACKED);
        repo.stash_save(&signature, message, flags)
            .map(|oid| oid.to_string())
            .map_err(git_error)
    }

    /// Records all tracked and untracked working-tree changes as an ordinary
    /// Git commit on a checked-out view, then materializes and verifies its
    /// semantic state before advancing the view metadata.
    pub fn record_view(
        &self,
        name: &str,
        message: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let view = self.view(name)?;
        if matches!(view.scope, orkia_model::ViewScope::Shared) {
            return Err(OrkiaError::Policy(format!(
                "Shared view {name} cannot be recorded directly; record on a Draft view then publish it with a verified grant"
            )));
        }
        let branch_ref = format!("refs/heads/{}", view.branch);
        let repo = self.repo()?;
        if repo.head().map_err(git_error)?.name() != Some(branch_ref.as_str()) {
            return Err(OrkiaError::Policy(format!(
                "view {name} must be checked out before recording"
            )));
        }
        let parent = repo
            .find_reference(&branch_ref)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(git_error)?;
        index.update_all(["*"].iter(), None).map_err(git_error)?;
        index.write().map_err(git_error)?;
        let tree_id = index.write_tree_to(&repo).map_err(git_error)?;
        let tree = repo.find_tree(tree_id).map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now(&identity.actor().display_name, "orkia@local"))
            .map_err(git_error)?;
        let commit = repo
            .commit(
                Some(&branch_ref),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )
            .map_err(git_error)?;
        drop(tree);
        drop(parent);
        drop(index);
        drop(repo);
        let state = self.materialize_semantic_state(&commit.to_string(), identity, policy)?;
        self.advance_view(name, &commit.to_string(), policy)?;
        Ok(state)
    }

    /// Replaces a single-parent view tip with a new ordinary Git commit built
    /// from the current worktree. The old tip is retained by Git's reflog.
    pub fn revise_view(
        &self,
        name: &str,
        message: &str,
        identity: &Identity,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let view = self.view(name)?;
        if matches!(view.scope, orkia_model::ViewScope::Shared) {
            return Err(OrkiaError::Policy(format!(
                "Shared view {name} cannot be revised directly; revise a Draft view then publish it with a verified grant"
            )));
        }
        let branch_ref = format!("refs/heads/{}", view.branch);
        let repo = self.repo()?;
        if repo.head().map_err(git_error)?.name() != Some(branch_ref.as_str()) {
            return Err(OrkiaError::Policy(format!(
                "view {name} must be checked out before revising"
            )));
        }
        let current = repo
            .find_reference(&branch_ref)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        if current.parent_count() != 1 {
            return Err(OrkiaError::Policy(
                "revising a root or merge commit is not supported; create a new record instead"
                    .into(),
            ));
        }
        let parent = current.parent(0).map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(git_error)?;
        index.update_all(["*"].iter(), None).map_err(git_error)?;
        index.write().map_err(git_error)?;
        let tree_id = index.write_tree_to(&repo).map_err(git_error)?;
        let tree = repo.find_tree(tree_id).map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now(&identity.actor().display_name, "orkia@local"))
            .map_err(git_error)?;
        let commit = repo
            .commit(None, &signature, &signature, message, &tree, &[&parent])
            .map_err(git_error)?;
        repo.reference(&branch_ref, commit, true, "Revise Orkia view commit")
            .map_err(git_error)?;
        drop(tree);
        drop(parent);
        drop(current);
        drop(index);
        drop(repo);
        let state = self.materialize_semantic_state(&commit.to_string(), identity, policy)?;
        self.advance_view(name, &commit.to_string(), policy)?;
        Ok(state)
    }

    /// Restores tracked files and the Git index to a checked-out view tip.
    /// Safe checkout leaves untracked files alone and refuses overwrite
    /// conflicts rather than discarding user data.
    pub fn restore_view(&self, name: &str) -> Result<()> {
        let view = self.view(name)?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        let repo = self.repo()?;
        if repo.head().map_err(git_error)?.name() != Some(branch_ref.as_str()) {
            return Err(OrkiaError::Policy(format!(
                "view {name} must be checked out before restoring"
            )));
        }
        let target = repo.revparse_single(&branch_ref).map_err(git_error)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        repo.checkout_tree(&target, Some(&mut checkout))
            .map_err(git_error)?;
        let commit = target.peel_to_commit().map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .read_tree(&commit.tree().map_err(git_error)?)
            .map_err(git_error)?;
        index.write().map_err(git_error)
    }

    /// Rewinds a clean, checked-out single-parent view by one Git commit. The
    /// target parent must already have a verified semantic state; the removed
    /// commit remains reachable through Git's reflog.
    pub fn unrecord_view(
        &self,
        name: &str,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticObjectRef> {
        let (_, current_view) = self.view_with_object(name)?;
        let branch_ref = format!("refs/heads/{}", current_view.branch);
        let repo = self.repo()?;
        if repo.head().map_err(git_error)?.name() != Some(branch_ref.as_str()) {
            return Err(OrkiaError::Policy(format!(
                "view {name} must be checked out before unrecording"
            )));
        }
        let mut status_options = StatusOptions::new();
        status_options
            .include_untracked(true)
            .recurse_untracked_dirs(true);
        if !repo
            .statuses(Some(&mut status_options))
            .map_err(git_error)?
            .is_empty()
        {
            return Err(OrkiaError::Policy(
                "cannot unrecord a view with local Git changes; stash or restore first".into(),
            ));
        }
        let current = repo
            .find_reference(&branch_ref)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        if current_view.base_commit != current.id().to_string() {
            return Err(OrkiaError::Policy(format!(
                "view {name} metadata is stale; it cannot be unrecorded safely"
            )));
        }
        if current.parent_count() != 1 {
            return Err(OrkiaError::Policy(
                "unrecording a root or merge commit is not supported".into(),
            ));
        }
        let parent = current.parent(0).map_err(git_error)?;
        let store = self.semantic_store();
        let state_ref = store
            .state_for_commit(&parent.id().to_string())?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!(
                    "active semantic state for unrecord target {}",
                    parent.id()
                ))
            })?;
        let state = store.verify_active_state(&state_ref, policy)?;
        let next = ViewMetadata {
            base_commit: parent.id().to_string(),
            visible_operations: state.operations,
            ..current_view
        };
        let next_ref = store.put_view(&next)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        repo.checkout_tree(parent.as_object(), Some(&mut checkout))
            .map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .read_tree(&parent.tree().map_err(git_error)?)
            .map_err(git_error)?;
        index.write().map_err(git_error)?;
        repo.reference(&branch_ref, parent.id(), true, "Unrecord Orkia view commit")
            .map_err(git_error)?;
        repo.reference(
            &view_ref_name(name)?,
            GitSemanticStore::oid(&next_ref.hash, "view metadata hash")?,
            true,
            "Unrecord Orkia semantic view",
        )
        .map_err(git_error)?;
        Ok(next_ref)
    }

    /// Tags the current Git tip of a view. The tag is an ordinary annotated
    /// Git tag, so every Git client can transport and inspect it.
    pub fn tag_view(&self, name: &str, tag: &str, message: &str) -> Result<String> {
        let view = self.view(name)?;
        let repo = self.repo()?;
        let target = repo
            .revparse_single(&format!("refs/heads/{}", view.branch))
            .map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now("Orkia", "orkia@local"))
            .map_err(git_error)?;
        repo.tag(tag, &target, &signature, message, false)
            .map(|oid| oid.to_string())
            .map_err(git_error)
    }

    fn ensure_view_branch_is_not_checked_out(
        &self,
        repo: &Repository,
        branch_ref: &str,
    ) -> Result<()> {
        let primary_head = repo.head().ok();
        if primary_head.as_ref().and_then(|head| head.name()) == Some(branch_ref) {
            return Err(OrkiaError::Policy(format!(
                "cannot delete view branch {branch_ref}: it is checked out in the primary worktree"
            )));
        }
        for worktree_name in repo.worktrees().map_err(git_error)?.iter().flatten() {
            let worktree = repo.find_worktree(worktree_name).map_err(git_error)?;
            let checkout = Repository::open(worktree.path()).map_err(|error| {
                OrkiaError::Integrity(format!(
                    "cannot inspect worktree {} while deleting view: {error}",
                    worktree.path().display()
                ))
            })?;
            if checkout
                .head()
                .ok()
                .and_then(|head| head.name().map(str::to_owned))
                == Some(branch_ref.to_owned())
            {
                return Err(OrkiaError::Policy(format!(
                    "cannot delete view branch {branch_ref}: it is checked out at {}",
                    worktree.path().display()
                )));
            }
        }
        Ok(())
    }

    /// Materializes a named view as an independent Git worktree on its branch.
    pub fn create_view_worktree(&self, name: &str, path: &Path) -> Result<()> {
        let view = self.view(name)?;
        let repo = self.repo()?;
        let branch = repo
            .find_reference(&format!("refs/heads/{}", view.branch))
            .map_err(|error| {
                OrkiaError::Integrity(format!(
                    "semantic view {} references missing branch {}: {error}",
                    view.name, view.branch
                ))
            })?;
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&branch));
        repo.worktree(&worktree_name(&view.name), path, Some(&options))
            .map_err(git_error)?;
        Ok(())
    }

    /// Safely checks out a named view in the repository's primary worktree.
    /// libgit2 refuses conflicts through the normal Git checkout machinery, so
    /// uncommitted user work is never overwritten by Orkia.
    pub fn switch_view(&self, name: &str) -> Result<()> {
        let view = self.view(name)?;
        let branch_ref = format!("refs/heads/{}", view.branch);
        let repo = self.repo()?;
        repo.find_reference(&branch_ref).map_err(|error| {
            OrkiaError::Integrity(format!(
                "semantic view {} references missing branch {}: {error}",
                view.name, view.branch
            ))
        })?;
        let target = repo.revparse_single(&branch_ref).map_err(git_error)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        // Check first and move HEAD only after libgit2 has accepted the work
        // tree transition. This avoids a partial branch switch on conflict.
        repo.checkout_tree(&target, Some(&mut checkout))
            .map_err(git_error)?;
        repo.set_head(&branch_ref).map_err(git_error)
    }
    pub fn changes_since(&self, base: &str) -> Result<Vec<WorkingFileChange>> {
        let repo = self.repo()?;
        let commit = repo
            .revparse_single(base)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut options = DiffOptions::new();
        options
            .include_untracked(true)
            .show_untracked_content(true)
            .recurse_untracked_dirs(true);
        let diff = repo
            .diff_tree_to_workdir_with_index(
                Some(&commit.tree().map_err(git_error)?),
                Some(&mut options),
            )
            .map_err(git_error)?;
        let mut changes = Vec::new();
        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .ok_or_else(|| OrkiaError::Integrity("diff delta has no path".into()))?
                .to_string_lossy()
                .into_owned();
            let old_content = if delta.old_file().id().is_zero() {
                String::new()
            } else {
                repo.find_blob(delta.old_file().id())
                    .ok()
                    .and_then(|blob| std::str::from_utf8(blob.content()).ok().map(str::to_owned))
                    .unwrap_or_default()
            };
            let new_content = std::fs::read_to_string(self.path.join(&path)).unwrap_or_default();
            let (changed_start, changed_end) = changed_lines(&old_content, &new_content);
            changes.push(WorkingFileChange {
                path,
                old_content,
                new_content,
                changed_start,
                changed_end,
            });
        }
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(changes)
    }
    pub fn project_paths(&self, branch: &str, base: &str, paths: &[String]) -> Result<String> {
        let repo = self.repo()?;
        let parent = repo
            .revparse_single(base)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .read_tree(&parent.tree().map_err(git_error)?)
            .map_err(git_error)?;
        for path in paths {
            if self.path.join(path).exists() {
                index.add_path(Path::new(path)).map_err(git_error)?;
            } else {
                index.remove_path(Path::new(path)).map_err(git_error)?;
            }
        }
        let tree_id = index.write_tree_to(&repo).map_err(git_error)?;
        let tree = repo.find_tree(tree_id).map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now("Orkia", "orkia@local"))
            .map_err(git_error)?;
        let message = format!("orkia: project review {branch}");
        let commit = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent],
            )
            .map_err(git_error)?;
        Ok(commit.to_string())
    }
    pub fn push_branch(&self, remote: &str, branch: &str) -> Result<()> {
        let repo = self.repo()?;
        let mut remote = repo.find_remote(remote).map_err(git_error)?;
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        remote.push(&[&refspec], None).map_err(git_error)
    }

    /// Push every Orkia ref using normal Git object negotiation and packs.
    ///
    /// This transports the ledger, semantic objects, state manifests, views,
    /// plans and attestations that live below `refs/orkia/` without creating a
    /// second synchronization protocol.
    pub fn push_orkia_refs(&self, remote: &str) -> Result<()> {
        let repo = self.repo()?;
        let refspecs = local_orkia_refspecs(&repo)?;
        if refspecs.is_empty() {
            return Ok(());
        }
        let mut remote = repo.find_remote(remote).map_err(git_error)?;
        let refspecs: Vec<_> = refspecs.iter().map(String::as_str).collect();
        remote.push(&refspecs, None).map_err(git_error)
    }

    /// Fetch every Orkia ref into the same namespace.
    ///
    /// Callers should validate object signatures and schema versions before
    /// making a fetched semantic state active for a view.
    pub fn fetch_orkia_refs(&self, remote: &str) -> Result<()> {
        let repo = self.repo()?;
        let mut remote = repo.find_remote(remote).map_err(git_error)?;
        remote.connect(git2::Direction::Fetch).map_err(git_error)?;
        let refspecs = remote
            .list()
            .map_err(git_error)?
            .iter()
            .map(|head| head.name())
            .filter(|name| name.starts_with(ORKIA_REF_PREFIX))
            .map(|name| format!("+{name}:{name}"))
            .collect::<Vec<_>>();
        remote.disconnect().map_err(git_error)?;
        if refspecs.is_empty() {
            return Ok(());
        }
        let refspecs: Vec<_> = refspecs.iter().map(String::as_str).collect();
        remote.fetch(&refspecs, None, None).map_err(git_error)
    }

    /// Fetches Orkia refs through Git, then validates every fetched semantic
    /// state before returning control to callers. Transport alone never makes
    /// semantic data trusted.
    pub fn fetch_verified_orkia_refs(
        &self,
        remote: &str,
        policy: &RepositoryPolicy,
    ) -> Result<OrkiaRefVerification> {
        self.fetch_orkia_refs(remote)?;
        self.verify_orkia_refs(policy)
    }

    /// Fail-closed verification pass for semantic states received through a
    /// normal Git fetch. This is deliberately separate from transport: a ref
    /// is not trusted merely because a remote advertised it.
    pub fn verify_orkia_refs(&self, policy: &RepositoryPolicy) -> Result<OrkiaRefVerification> {
        let repo = self.repo()?;
        let store = self.semantic_store();
        let mut verified_states = 0;
        for reference in repo
            .references_glob(&format!("{SEMANTIC_STATE_REF_PREFIX}/*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let state = reference
                .target()
                .ok_or_else(|| OrkiaError::Integrity("semantic state ref is symbolic".into()))?;
            store.verify_active_state(
                &SemanticObjectRef {
                    kind: SemanticObjectKind::State,
                    hash: state.to_string(),
                },
                policy,
            )?;
            verified_states += 1;
        }
        Ok(OrkiaRefVerification { verified_states })
    }

    /// Queries the canonical semantic manifest directly from Git. Any external
    /// search index is therefore optional and reconstructible.
    pub fn query_trunks(
        &self,
        commit: &str,
        path_prefix: Option<&str>,
        policy: &RepositoryPolicy,
    ) -> Result<Vec<orkia_model::SemanticTrunk>> {
        let store = self.semantic_store();
        let state = store.state_for_commit(commit)?.ok_or_else(|| {
            OrkiaError::NotFound(format!("active semantic state for commit {commit}"))
        })?;
        let mut trunks: Vec<_> = store
            .verify_active_state(&state, policy)?
            .trunks
            .into_values()
            .filter(|trunk| {
                path_prefix
                    .is_none_or(|prefix| trunk.paths.iter().any(|path| path.starts_with(prefix)))
            })
            .collect();
        trunks.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(trunks)
    }

    /// Compares two active, signature-verified state manifests. The result is
    /// deliberately a summary, not a replacement for `git diff`: paths and
    /// blobs still come from Git, while Trunk and operation identities supply
    /// durable semantic provenance across ordinary edits and recognized moves.
    pub fn semantic_diff(
        &self,
        base_commit: &str,
        target_commit: &str,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticDiff> {
        let store = self.semantic_store();
        let state = |commit: &str| -> Result<SemanticState> {
            let object = store.state_for_commit(commit)?.ok_or_else(|| {
                OrkiaError::NotFound(format!("active semantic state for commit {commit}"))
            })?;
            store.verify_active_state(&object, policy)
        };
        let base = state(base_commit)?;
        let target = state(target_commit)?;

        let changed_paths = base
            .files
            .keys()
            .chain(target.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| base.files.get(*path) != target.files.get(*path))
            .cloned()
            .collect();
        let added_trunks = target
            .trunks
            .keys()
            .filter(|id| !base.trunks.contains_key(*id))
            .cloned()
            .collect();
        let removed_trunks = base
            .trunks
            .keys()
            .filter(|id| !target.trunks.contains_key(*id))
            .cloned()
            .collect();
        let changed_trunks = target
            .trunks
            .iter()
            .filter(|(id, trunk)| base.trunks.get(*id).is_some_and(|base| base != *trunk))
            .map(|(id, _)| id.clone())
            .collect();
        let added_operations = target
            .operations
            .difference(&base.operations)
            .cloned()
            .collect();
        let removed_operations = base
            .operations
            .difference(&target.operations)
            .cloned()
            .collect();
        Ok(SemanticDiff {
            base_commit: base.commit,
            target_commit: target.commit,
            changed_paths,
            added_trunks,
            removed_trunks,
            changed_trunks,
            added_operations,
            removed_operations,
        })
    }

    /// Returns only provenance that can be proved from the signed state and
    /// operation closure. Callers should use normal `git blame` for byte- or
    /// line-level history outside a captured semantic manifest.
    pub fn semantic_blame(
        &self,
        commit: &str,
        path: &str,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticBlame> {
        let store = self.semantic_store();
        let state_ref = store.state_for_commit(commit)?.ok_or_else(|| {
            OrkiaError::NotFound(format!("active semantic state for commit {commit}"))
        })?;
        let state = store.verify_active_state(&state_ref, policy)?;
        let trunks = state
            .trunks
            .values()
            .filter(|trunk| trunk.paths.contains(path))
            .cloned()
            .collect::<Vec<_>>();
        if trunks.is_empty() {
            return Err(OrkiaError::NotFound(format!(
                "no captured semantic trunk for {path} at {commit}"
            )));
        }
        let trunk_ids = trunks
            .iter()
            .map(|trunk| trunk.id.clone())
            .collect::<BTreeSet<_>>();
        let mut operations = Vec::new();
        for operation in &state.operations {
            let operation_document = store.get_operation(operation)?;
            let touches_path = match operation_document.action {
                SemanticOperationAction::Insert { trunk, .. } => trunk_ids.contains(&trunk),
                SemanticOperationAction::Delete { target }
                | SemanticOperationAction::Move { target, .. }
                | SemanticOperationAction::Replace { target, .. } => trunk_ids.contains(&target),
                SemanticOperationAction::Resolve { .. } => false,
            };
            if touches_path {
                operations.push(operation.clone());
            }
        }
        operations.sort();
        Ok(SemanticBlame {
            commit: state.commit,
            path: path.into(),
            trunks,
            operations,
        })
    }

    /// Walks ordinary Git history and annotates captured commits with their
    /// verified state ref. This keeps Git history authoritative and makes
    /// uncaptured commits visible as deliberate fallback entries.
    pub fn semantic_log(
        &self,
        start: &str,
        limit: usize,
        policy: &RepositoryPolicy,
    ) -> Result<Vec<SemanticHistoryEntry>> {
        let repo = self.repo()?;
        let start = repo
            .revparse_single(start)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut walk = repo.revwalk().map_err(git_error)?;
        walk.push(start.id()).map_err(git_error)?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
            .map_err(git_error)?;
        let store = self.semantic_store();
        walk.take(limit)
            .map(|item| {
                let commit = item.map_err(git_error)?.to_string();
                let state = store.state_for_commit(&commit)?;
                if let Some(state) = &state {
                    store.verify_active_state(state, policy)?;
                }
                Ok(SemanticHistoryEntry { commit, state })
            })
            .collect()
    }

    /// Writes a reproducible OCI image layout. The layer is an uncompressed,
    /// deterministic tar of the committed Git tree (no worktree files), and
    /// the OCI annotations bind it to the verified semantic state.
    pub fn seal_sandbox(
        &self,
        commit: &str,
        output: &Path,
        policy: &RepositoryPolicy,
    ) -> Result<SandboxSeal> {
        if output.exists() {
            return Err(OrkiaError::Conflict(format!(
                "refusing to overwrite existing OCI layout {}",
                output.display()
            )));
        }
        let store = self.semantic_store();
        let state = store.state_for_commit(commit)?.ok_or_else(|| {
            OrkiaError::NotFound(format!("active semantic state for commit {commit}"))
        })?;
        store.verify_active_state(&state, policy)?;
        let repo = self.repo()?;
        let commit = repo
            .revparse_single(commit)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut entries = Vec::new();
        commit
            .tree()
            .map_err(git_error)?
            .walk(TreeWalkMode::PreOrder, |root, entry| {
                if entry.kind() == Some(ObjectType::Blob) {
                    entries.push((
                        format!("{root}{}", entry.name().unwrap_or_default()),
                        entry.id(),
                        entry.filemode() as u32,
                    ));
                }
                TreeWalkResult::Ok
            })
            .map_err(git_error)?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut layer = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut layer);
            for (path, oid, mode) in entries {
                let blob = repo.find_blob(oid).map_err(git_error)?;
                let mut header = tar::Header::new_gnu();
                header.set_size(blob.size() as u64);
                header.set_mode(mode & 0o777);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_cksum();
                archive
                    .append_data(&mut header, path, Cursor::new(blob.content()))
                    .map_err(|e| OrkiaError::External(format!("cannot write OCI layer: {e}")))?;
            }
            archive
                .finish()
                .map_err(|e| OrkiaError::External(format!("cannot finish OCI layer: {e}")))?;
        }
        fs::create_dir_all(output.join("blobs/sha256"))
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        fs::write(
            output.join("oci-layout"),
            b"{\"imageLayoutVersion\":\"1.0.0\"}\n",
        )
        .map_err(|e| OrkiaError::External(e.to_string()))?;
        let layer_digest = write_oci_blob(output, &layer)?;
        let config = orkia_model::canonical_json(
            &serde_json::json!({"architecture":"unknown","os":"unknown","rootfs":{"type":"layers","diff_ids":[format!("sha256:{layer_digest}")]}}),
        )?;
        let config_digest = write_oci_blob(output, &config)?;
        let manifest = orkia_model::canonical_json(
            &serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{config_digest}"),"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":format!("sha256:{layer_digest}"),"size":layer.len()}]}),
        )?;
        let manifest_digest = write_oci_blob(output, &manifest)?;
        let index = orkia_model::canonical_json(
            &serde_json::json!({"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":format!("sha256:{manifest_digest}"),"size":manifest.len(),"annotations":{"org.opencontainers.image.revision":commit.id().to_string(),"org.orkia.semantic-state":state.hash}}]}),
        )?;
        fs::write(output.join("index.json"), index)
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        Ok(SandboxSeal {
            commit: commit.id().to_string(),
            state,
            manifest_digest,
            layer_digest,
        })
    }

    /// Verifies every OCI descriptor and blob digest without needing a Git
    /// checkout. This establishes transport integrity; callers that also have
    /// the repository may independently verify the annotated semantic state.
    pub fn verify_sandbox(output: &Path) -> Result<SandboxSeal> {
        let layout: serde_json::Value = read_oci_json(&output.join("oci-layout"))?;
        if layout
            .get("imageLayoutVersion")
            .and_then(serde_json::Value::as_str)
            != Some("1.0.0")
        {
            return Err(OrkiaError::Integrity("unsupported OCI image layout".into()));
        }
        let index: serde_json::Value = read_oci_json(&output.join("index.json"))?;
        let descriptor = index
            .get("manifests")
            .and_then(serde_json::Value::as_array)
            .and_then(|v| v.first())
            .ok_or_else(|| OrkiaError::Integrity("OCI index has no manifest".into()))?;
        let manifest_digest = verify_oci_descriptor(output, descriptor)?;
        let manifest: serde_json::Value =
            read_oci_json(&output.join("blobs/sha256").join(&manifest_digest))?;
        verify_oci_descriptor(
            output,
            manifest
                .get("config")
                .ok_or_else(|| OrkiaError::Integrity("OCI manifest has no config".into()))?,
        )?;
        let layer = manifest
            .get("layers")
            .and_then(serde_json::Value::as_array)
            .and_then(|v| v.first())
            .ok_or_else(|| OrkiaError::Integrity("OCI manifest has no layer".into()))?;
        let layer_digest = verify_oci_descriptor(output, layer)?;
        let annotations = descriptor
            .get("annotations")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| OrkiaError::Integrity("OCI manifest lacks Orkia annotations".into()))?;
        let commit = annotations
            .get("org.opencontainers.image.revision")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| OrkiaError::Integrity("OCI manifest lacks commit annotation".into()))?
            .into();
        let hash = annotations
            .get("org.orkia.semantic-state")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                OrkiaError::Integrity("OCI manifest lacks semantic state annotation".into())
            })?
            .into();
        Ok(SandboxSeal {
            commit,
            state: SemanticObjectRef {
                kind: SemanticObjectKind::State,
                hash,
            },
            manifest_digest,
            layer_digest,
        })
    }
}

fn write_oci_blob(output: &Path, bytes: &[u8]) -> Result<String> {
    let digest = hex::encode(Sha256::digest(bytes));
    fs::write(output.join("blobs/sha256").join(&digest), bytes)
        .map_err(|e| OrkiaError::External(format!("cannot write OCI blob: {e}")))?;
    Ok(digest)
}

fn read_oci_json(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_slice(
        &fs::read(path).map_err(|e| OrkiaError::NotFound(format!("{}: {e}", path.display())))?,
    )
    .map_err(|e| OrkiaError::Integrity(format!("invalid OCI JSON: {e}")))
}

fn verify_oci_descriptor(output: &Path, descriptor: &serde_json::Value) -> Result<String> {
    let digest = descriptor
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .and_then(|v| v.strip_prefix("sha256:"))
        .ok_or_else(|| OrkiaError::Integrity("invalid OCI descriptor digest".into()))?;
    let bytes = fs::read(output.join("blobs/sha256").join(digest))
        .map_err(|e| OrkiaError::Integrity(format!("missing OCI blob {digest}: {e}")))?;
    if hex::encode(Sha256::digest(&bytes)) != digest
        || descriptor.get("size").and_then(serde_json::Value::as_u64) != Some(bytes.len() as u64)
    {
        return Err(OrkiaError::Integrity(
            "OCI descriptor digest or size mismatch".into(),
        ));
    }
    Ok(digest.into())
}

/// Resolves only ordinary three-way text conflicts that the semantic token
/// merger can prove disjoint. No index mutation is made until every conflict
/// has a deterministic answer, so one unresolved file leaves Git's original
/// conflict intact for the user.
fn resolve_token_conflicts(repo: &Repository, index: &mut git2::Index) -> Result<bool> {
    struct ResolvedConflict {
        path: String,
        mode: u32,
        contents: String,
    }

    let conflicts = index
        .conflicts()
        .map_err(git_error)?
        .map(|conflict| conflict.map_err(git_error))
        .collect::<Result<Vec<_>>>()?;
    let mut resolved = Vec::with_capacity(conflicts.len());
    for conflict in conflicts {
        let (Some(base_entry), Some(left_entry), Some(right_entry)) =
            (conflict.ancestor, conflict.our, conflict.their)
        else {
            return Ok(false);
        };
        let base = repo.find_blob(base_entry.id).map_err(git_error)?;
        let left = repo.find_blob(left_entry.id).map_err(git_error)?;
        let right = repo.find_blob(right_entry.id).map_err(git_error)?;
        let (Ok(base), Ok(left), Ok(right)) = (
            std::str::from_utf8(base.content()),
            std::str::from_utf8(left.content()),
            std::str::from_utf8(right.content()),
        ) else {
            return Ok(false);
        };
        let Some(contents) = merge_token_text(base, left, right) else {
            return Ok(false);
        };
        let Ok(path) = String::from_utf8(left_entry.path) else {
            return Ok(false);
        };
        resolved.push(ResolvedConflict {
            path,
            mode: left_entry.mode,
            contents,
        });
    }
    for conflict in resolved {
        let blob = repo.blob(conflict.contents.as_bytes()).map_err(git_error)?;
        index
            .conflict_remove(Path::new(&conflict.path))
            .map_err(git_error)?;
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: conflict.mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: blob,
                flags: 0,
                flags_extended: 0,
                path: conflict.path.into_bytes(),
            })
            .map_err(git_error)?;
    }
    Ok(!index.has_conflicts())
}

fn local_orkia_refspecs(repo: &Repository) -> Result<Vec<String>> {
    let references = repo.references_glob("refs/orkia/*").map_err(git_error)?;
    references
        .map(|reference| {
            let reference = reference.map_err(git_error)?;
            let name = reference.name().ok_or_else(|| {
                OrkiaError::Integrity("an Orkia reference has no UTF-8 name".into())
            })?;
            Ok(format!("{name}:{name}"))
        })
        .collect()
}

fn view_ref_name(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('/') {
        return Err(OrkiaError::Invalid(
            "view name must be a non-empty single Git-ref path segment".into(),
        ));
    }
    let reference = format!("{VIEW_REF_PREFIX}/{name}");
    if !Reference::is_valid_name(&reference) {
        return Err(OrkiaError::Invalid(format!(
            "view name is not valid in a Git ref: {name}"
        )));
    }
    Ok(reference)
}

fn vault_ref_name(name: &str) -> Result<String> {
    if !valid_vault_name(name) {
        return Err(OrkiaError::Invalid(
            "vault entry name must contain only ASCII letters, digits, '.', '_' or '-'".into(),
        ));
    }
    Ok(format!("{ORKIA_REF_PREFIX}vault/{name}"))
}

fn plan_ref_name(id: &PlanId, revision: u32) -> String {
    format!("{ORKIA_REF_PREFIX}plans/{}/{}", id.0, revision)
}

fn vault_key(password: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password, salt, &mut key)
        .map_err(|error| OrkiaError::External(format!("cannot derive vault key: {error}")))?;
    Ok(key)
}

fn worktree_name(view_name: &str) -> String {
    // `git_worktree_add` treats the name as an administrative directory
    // segment. Keep it deterministic and independent from user-controlled
    // separator characters accepted by normal Git branch names.
    let encoded = view_name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("orkia-{encoded}")
}

fn renamed_paths(
    repo: &Repository,
    parent: &git2::Tree<'_>,
    current: &git2::Tree<'_>,
) -> Result<BTreeMap<String, String>> {
    let mut diff = repo
        .diff_tree_to_tree(Some(parent), Some(current), None)
        .map_err(git_error)?;
    diff.find_similar(None).map_err(git_error)?;
    let mut renamed = BTreeMap::new();
    for delta in diff.deltas() {
        if !matches!(delta.status(), Delta::Renamed | Delta::Copied) {
            continue;
        }
        let Some(old_path) = delta.old_file().path() else {
            continue;
        };
        let Some(new_path) = delta.new_file().path() else {
            continue;
        };
        renamed.insert(
            new_path.to_string_lossy().into_owned(),
            old_path.to_string_lossy().into_owned(),
        );
    }
    Ok(renamed)
}

fn repository_anchor(repo: &Repository, commit: &git2::Commit<'_>) -> Result<String> {
    let mut walk = repo.revwalk().map_err(git_error)?;
    walk.push(commit.id()).map_err(git_error)?;
    let mut roots = BTreeSet::new();
    for oid in walk {
        let oid = oid.map_err(git_error)?;
        let ancestor = repo.find_commit(oid).map_err(git_error)?;
        if ancestor.parent_count() == 0 {
            roots.insert(ancestor.id().to_string());
        }
    }
    roots
        .into_iter()
        .next()
        .ok_or_else(|| OrkiaError::Integrity("Git history has no root commit".into()))
}

fn transition_operations(
    previous: Option<&SemanticState>,
    current: &SemanticState,
) -> Vec<SemanticOperation> {
    let base_commit = previous
        .map(|state| state.commit.clone())
        .unwrap_or_else(|| current.commit.clone());
    let mut operations = Vec::new();
    for (id, trunk) in &current.trunks {
        let prior = previous.and_then(|state| state.trunks.get(id));
        let action = match (prior, &trunk.state) {
            (None, TrunkState::Alive) => {
                trunk_blob(current, trunk).map(|content_hash| SemanticOperationAction::Insert {
                    trunk: id.clone(),
                    after: None,
                    content_hash,
                })
            }
            (Some(prior), TrunkState::Deleted | TrunkState::Zombie)
                if prior.state == TrunkState::Alive =>
            {
                Some(SemanticOperationAction::Delete { target: id.clone() })
            }
            (Some(prior), TrunkState::Alive) => match prior.state {
                TrunkState::Alive => {
                    let old_blob = previous.and_then(|state| trunk_blob(state, prior));
                    let new_blob = trunk_blob(current, trunk);
                    (old_blob != new_blob).then(|| SemanticOperationAction::Replace {
                        target: id.clone(),
                        content_hash: new_blob.unwrap_or_else(|| current.commit.clone()),
                    })
                }
                TrunkState::Deleted | TrunkState::Zombie => {
                    trunk_blob(current, trunk).map(|content_hash| SemanticOperationAction::Insert {
                        trunk: id.clone(),
                        after: None,
                        content_hash,
                    })
                }
            },
            _ => None,
        };
        if let Some(action) = action {
            operations.push(SemanticOperation {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                base_commit: base_commit.clone(),
                dependencies: BTreeSet::new(),
                action,
            });
        }
    }
    operations.sort_by_key(|operation| orkia_model::canonical_json(operation).unwrap_or_default());
    operations
}

fn trunk_blob(state: &SemanticState, trunk: &orkia_model::SemanticTrunk) -> Option<String> {
    trunk
        .paths
        .iter()
        .filter_map(|path| state.files.get(path))
        .next_back()
        .cloned()
}

fn operation_target(operation: &SemanticOperation) -> Option<&orkia_model::SemanticNodeId> {
    match &operation.action {
        SemanticOperationAction::Insert { trunk, .. } => Some(trunk),
        SemanticOperationAction::Delete { target }
        | SemanticOperationAction::Move { target, .. }
        | SemanticOperationAction::Replace { target, .. } => Some(target),
        SemanticOperationAction::Resolve { .. } => None,
    }
}

fn ledger_event_ref_name(event: &LedgerEvent) -> Result<String> {
    if event.hash.is_empty() || !event.hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(OrkiaError::Invalid(
            "ledger event hash must be a non-empty hexadecimal string".into(),
        ));
    }
    Ok(format!(
        "{}/{}/{}",
        LEDGER_EVENT_REF_PREFIX, event.unsigned.actor.0, event.hash
    ))
}

fn migrate_legacy_ledger_ref(repo: &Repository) -> Result<()> {
    let mut legacy = match repo.find_reference(LEGACY_LEDGER_ROOT_REF) {
        Ok(reference) => reference,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(()),
        Err(error) => return Err(git_error(error)),
    };
    legacy
        .rename(LEDGER_REF, true, "Migrate aggregate Orkia ledger")
        .map_err(git_error)?;
    Ok(())
}

fn changed_lines(old: &str, new: &str) -> (u32, u32) {
    let old_lines: Vec<_> = old.lines().collect();
    let new_lines: Vec<_> = new.lines().collect();
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let start = prefix as u32 + 1;
    let end = (new_lines.len().saturating_sub(suffix).max(prefix + 1)) as u32;
    (start, end)
}

fn git_error(error: git2::Error) -> OrkiaError {
    OrkiaError::External(format!("git: {error}"))
}

impl GitRepository for LibGit2Repository {
    fn head_commit(&self) -> Result<String> {
        Ok(self
            .repo()?
            .head()
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?
            .id()
            .to_string())
    }
    fn create_isolated_worktree(&self, name: &str, path: &Path) -> Result<()> {
        self.repo()?.worktree(name, path, None).map_err(git_error)?;
        Ok(())
    }
    fn write_ledger(&self, bytes: &[u8]) -> Result<()> {
        let repo = self.repo()?;
        let oid = repo.blob(bytes).map_err(git_error)?;
        repo.reference(LEDGER_REF, oid, true, "Orkia signed ledger")
            .map_err(git_error)?;
        Ok(())
    }
    fn read_ledger(&self) -> Result<Option<Vec<u8>>> {
        let repo = self.repo()?;
        let Ok(reference) = repo.find_reference(LEDGER_REF) else {
            return Ok(None);
        };
        let object = reference.peel(ObjectType::Blob).map_err(git_error)?;
        Ok(Some(
            object
                .as_blob()
                .ok_or_else(|| OrkiaError::Integrity("ledger ref is not a blob".into()))?
                .content()
                .to_vec(),
        ))
    }
}

#[derive(Clone, Debug)]
pub struct GitLedgerStore {
    repository: LibGit2Repository,
}

/// Git-backed persistence for the semantic overlay.
///
/// Each value is a normal immutable Git blob. A ref makes it discoverable by
/// kind, and a second ref optionally binds a state manifest to a commit. Both
/// refs travel through ordinary Git refspecs; no parallel object transport is
/// required.
#[derive(Clone, Debug)]
pub struct GitSemanticStore {
    repository: LibGit2Repository,
}

impl GitSemanticStore {
    fn object_ref_name(object: &SemanticObjectRef) -> String {
        format!(
            "{}/{}/{}",
            SEMANTIC_OBJECT_REF_PREFIX,
            object.kind.ref_segment(),
            object.hash
        )
    }

    fn state_ref_name(commit: &Oid) -> String {
        format!("{}/{}", SEMANTIC_STATE_REF_PREFIX, commit)
    }

    fn oid(value: &str, field: &str) -> Result<Oid> {
        Oid::from_str(value)
            .map_err(|_| OrkiaError::Invalid(format!("{field} is not a Git object ID: {value}")))
    }

    fn put_document<T: SemanticDocument + Serialize>(
        &self,
        document: &T,
    ) -> Result<SemanticObjectRef> {
        document.validate()?;
        let bytes = orkia_model::canonical_json(document)?;
        self.put(T::KIND, &bytes)
    }

    fn get_document<T: SemanticDocument + DeserializeOwned>(
        &self,
        object: &SemanticObjectRef,
    ) -> Result<T> {
        if object.kind != T::KIND {
            return Err(OrkiaError::Invalid(format!(
                "expected a {} object, found {}",
                T::KIND.ref_segment(),
                object.kind.ref_segment()
            )));
        }
        let bytes = self.get(object)?;
        let document: T = serde_json::from_slice(&bytes).map_err(|error| {
            OrkiaError::Integrity(format!("invalid semantic document: {error}"))
        })?;
        document.validate()?;
        Ok(document)
    }

    fn operation_closure(
        &self,
        roots: impl IntoIterator<Item = SemanticObjectRef>,
    ) -> Result<BTreeSet<SemanticObjectRef>> {
        let mut pending: Vec<_> = roots.into_iter().collect();
        let mut visited = std::collections::BTreeSet::new();
        while let Some(operation) = pending.pop() {
            if operation.kind != SemanticObjectKind::Operation {
                return Err(OrkiaError::Integrity(
                    "semantic operation closure contains a non-operation object".into(),
                ));
            }
            if !visited.insert(operation.clone()) {
                continue;
            }
            pending.extend(self.get_operation(&operation)?.dependencies);
        }
        Ok(visited)
    }

    fn validate_operation_closure(
        &self,
        roots: impl IntoIterator<Item = SemanticObjectRef>,
    ) -> Result<()> {
        self.operation_closure(roots).map(|_| ())
    }

    fn require_signature_quorum(
        &self,
        subject: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<()> {
        let bytes = self.get(subject)?;
        let mut valid_signers = BTreeSet::new();
        for signature in self.signatures_for(subject)? {
            if verify(&signature.signer.public_key, &bytes, &signature.signature).is_ok() {
                valid_signers.insert(signature.signer.id);
            }
        }
        if valid_signers.len() < usize::from(policy.minimum_semantic_signatures) {
            return Err(OrkiaError::Policy(format!(
                "{} {} needs {} valid signature(s), found {}",
                subject.kind.ref_segment(),
                subject.hash,
                policy.minimum_semantic_signatures,
                valid_signers.len()
            )));
        }
        Ok(())
    }

    /// Revalidates an active state for consumers that must not trust a ref
    /// merely because it already exists locally (for example shared views).
    pub fn verify_active_state(
        &self,
        state: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<SemanticState> {
        let manifest = self.get_state(state)?;
        self.require_signature_quorum(state, policy)?;
        for operation in self.operation_closure(manifest.operations.iter().cloned())? {
            self.require_signature_quorum(&operation, policy)?;
        }
        Ok(manifest)
    }

    fn validate_state_files(&self, state: &SemanticState) -> Result<()> {
        let repo = self.repository.repo()?;
        for blob in state.files.values() {
            repo.find_blob(Self::oid(blob, "state file blob")?)
                .map_err(|_| {
                    OrkiaError::Integrity(format!("state references missing Git blob {blob}"))
                })?;
        }
        Ok(())
    }

    fn signatures_for(&self, subject: &SemanticObjectRef) -> Result<Vec<SemanticSignature>> {
        let repo = self.repository.repo()?;
        let mut signatures = Vec::new();
        for reference in repo
            .references_glob(&format!("{SEMANTIC_OBJECT_REF_PREFIX}/signature/*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let Some(target) = reference.target() else {
                continue;
            };
            let object = SemanticObjectRef {
                kind: SemanticObjectKind::Signature,
                hash: target.to_string(),
            };
            if let Ok(signature) = self.get_signature(&object)
                && signature.subject == *subject
            {
                signatures.push(signature);
            }
        }
        Ok(signatures)
    }

    /// Creates an offline-verifiable proof over the exact stored object bytes.
    pub fn sign_document(
        &self,
        object: &SemanticObjectRef,
        identity: &Identity,
    ) -> Result<SemanticObjectRef> {
        let bytes = self.get(object)?;
        self.put_signature(&SemanticSignature {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            subject: object.clone(),
            signer: identity.actor().clone(),
            signature: identity.sign(&bytes),
        })
    }

    /// Stores a session attestation as an immutable Git blob and signs the
    /// complete document. The result commit, when present, is linked to its
    /// active semantic state as evidence.
    pub fn attest_session(
        &self,
        session: SessionId,
        base_commit: String,
        result_commit: Option<String>,
        mut evidence: BTreeSet<SemanticObjectRef>,
        identity: &Identity,
    ) -> Result<SemanticObjectRef> {
        if let Some(result) = &result_commit
            && let Some(state) = self.state_for_commit(result)?
        {
            evidence.insert(state);
        }
        let statement = Self::attestation_statement(
            &identity.actor().id,
            Some(&identity.actor().public_key),
            &session,
            &base_commit,
            &result_commit,
            &evidence,
        )?;
        let attestation = self.put_attestation(&Attestation {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            actor: identity.actor().id.clone(),
            actor_public_key: Some(identity.actor().public_key.clone()),
            session,
            base_commit,
            result_commit,
            evidence,
            signature: identity.sign(&statement),
        })?;
        self.sign_document(&attestation, identity)?;
        Ok(attestation)
    }

    /// Verifies an attestation document's schema/evidence and detached
    /// signature quorum before it is used by policy or provenance consumers.
    pub fn verify_attestation(
        &self,
        attestation: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<Attestation> {
        let document = self.get_attestation(attestation)?;
        if let Some(actor_public_key) = &document.actor_public_key {
            let statement = Self::attestation_statement(
                &document.actor,
                Some(actor_public_key),
                &document.session,
                &document.base_commit,
                &document.result_commit,
                &document.evidence,
            )?;
            verify(actor_public_key, &statement, &document.signature)?;
        } else {
            // Schema-v1 attestations did not embed the author key. Preserve
            // their readability, but require their detached proof to be from
            // the declared actor rather than merely from any quorum signer.
            let bytes = self.get(attestation)?;
            let actor_signed = self
                .signatures_for(attestation)?
                .into_iter()
                .any(|signature| {
                    signature.signer.id == document.actor
                        && verify(&signature.signer.public_key, &bytes, &signature.signature)
                            .is_ok()
                });
            if !actor_signed {
                return Err(OrkiaError::Integrity(
                    "legacy attestation lacks a valid signature from its declared actor".into(),
                ));
            }
        }
        self.require_signature_quorum(attestation, policy)?;
        Ok(document)
    }

    fn attestation_statement(
        actor: &orkia_model::ActorId,
        actor_public_key: Option<&str>,
        session: &SessionId,
        base_commit: &str,
        result_commit: &Option<String>,
        evidence: &BTreeSet<SemanticObjectRef>,
    ) -> Result<Vec<u8>> {
        serde_json::to_vec(&(
            actor,
            actor_public_key,
            session,
            base_commit,
            result_commit,
            evidence,
        ))
        .map_err(|error| OrkiaError::Invalid(format!("cannot serialize attestation: {error}")))
    }

    pub fn verify_access_grant(
        &self,
        grant: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<AccessGrant> {
        if policy.revoked_grants.contains(&grant.hash) {
            return Err(OrkiaError::Policy(
                "grant is revoked by repository policy".into(),
            ));
        }
        let document = self.get_access_grant(grant)?;
        if self.is_distributed_grant_revoked(grant, &document.issuer)? {
            return Err(OrkiaError::Policy(
                "grant is revoked by a signed Git revocation".into(),
            ));
        }
        if !policy.authorized_grant_issuers.contains(&document.issuer) {
            return Err(OrkiaError::Policy(
                "grant issuer is not trusted by repository policy".into(),
            ));
        }
        if let Some(expires_at) = &document.expires_at {
            let expires_at = time::OffsetDateTime::parse(
                expires_at,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|error| {
                OrkiaError::Integrity(format!(
                    "invalid grant expiration in stored object: {error}"
                ))
            })?;
            if expires_at <= time::OffsetDateTime::now_utc() {
                return Err(OrkiaError::Policy("grant is expired".into()));
            }
        }
        self.require_signature_quorum(grant, policy)?;
        let bytes = self.get(grant)?;
        let issuer_signed = self.signatures_for(grant)?.into_iter().any(|signature| {
            signature.signer.id == document.issuer
                && verify(&signature.signer.public_key, &bytes, &signature.signature).is_ok()
        });
        if !issuer_signed {
            return Err(OrkiaError::Policy(
                "grant lacks a valid signature from its declared issuer".into(),
            ));
        }
        Ok(document)
    }

    fn is_distributed_grant_revoked(
        &self,
        grant: &SemanticObjectRef,
        issuer: &orkia_model::ActorId,
    ) -> Result<bool> {
        let repo = self.repository.repo()?;
        for reference in repo
            .references_glob(&format!("{SEMANTIC_OBJECT_REF_PREFIX}/grant_revocation/*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let Some(target) = reference.target() else {
                continue;
            };
            let revocation = SemanticObjectRef {
                kind: SemanticObjectKind::GrantRevocation,
                hash: target.to_string(),
            };
            let Ok(document) = self.get_grant_revocation(&revocation) else {
                continue;
            };
            if document.grant != *grant || document.issuer != *issuer {
                continue;
            }
            let bytes = self.get(&revocation)?;
            if self
                .signatures_for(&revocation)?
                .into_iter()
                .any(|signature| {
                    signature.signer.id == *issuer
                        && verify(&signature.signer.public_key, &bytes, &signature.signature)
                            .is_ok()
                })
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn verify_organization(
        &self,
        organization: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<Organization> {
        let document = self.get_organization(organization)?;
        self.verify_issuer_controlled_document(organization, &document.issuer, policy)?;
        Ok(document)
    }

    pub fn verify_team(&self, team: &SemanticObjectRef, policy: &RepositoryPolicy) -> Result<Team> {
        let document = self.get_team(team)?;
        let organization = self.verify_organization(&document.organization, policy)?;
        if document.issuer != organization.issuer {
            return Err(OrkiaError::Policy(
                "team issuer must be the trusted organization issuer".into(),
            ));
        }
        self.verify_issuer_controlled_document(team, &document.issuer, policy)?;
        Ok(document)
    }

    /// A rotation is accepted only when the same actor has signed the exact
    /// rotation document with both the previous and replacement public keys.
    pub fn verify_key_rotation(&self, rotation: &SemanticObjectRef) -> Result<KeyRotation> {
        let document = self.get_key_rotation(rotation)?;
        let bytes = self.get(rotation)?;
        let mut keys = BTreeSet::new();
        for signature in self.signatures_for(rotation)? {
            if signature.signer.id == document.actor
                && verify(&signature.signer.public_key, &bytes, &signature.signature).is_ok()
                && (signature.signer.public_key == document.previous_public_key
                    || signature.signer.public_key == document.next_public_key)
            {
                keys.insert(signature.signer.public_key);
            }
        }
        if !keys.contains(&document.previous_public_key)
            || !keys.contains(&document.next_public_key)
        {
            return Err(OrkiaError::Policy(
                "key rotation lacks signatures from both keys".into(),
            ));
        }
        Ok(document)
    }

    fn verify_issuer_controlled_document(
        &self,
        subject: &SemanticObjectRef,
        issuer: &orkia_model::ActorId,
        policy: &RepositoryPolicy,
    ) -> Result<()> {
        if !policy.authorized_grant_issuers.contains(issuer) {
            return Err(OrkiaError::Policy(
                "document issuer is not trusted by repository policy".into(),
            ));
        }
        self.require_signature_quorum(subject, policy)?;
        let bytes = self.get(subject)?;
        let issuer_signed = self.signatures_for(subject)?.into_iter().any(|signature| {
            signature.signer.id == *issuer
                && verify(&signature.signer.public_key, &bytes, &signature.signature).is_ok()
        });
        if !issuer_signed {
            return Err(OrkiaError::Policy(
                "document lacks a valid signature from its declared issuer".into(),
            ));
        }
        Ok(())
    }

    pub fn require_role(
        &self,
        actor: &orkia_model::ActorId,
        role: GrantRole,
        grants: impl IntoIterator<Item = SemanticObjectRef>,
        policy: &RepositoryPolicy,
    ) -> Result<()> {
        self.require_role_for_repository(actor, role, grants, "*", policy)
    }

    pub fn require_role_for_repository(
        &self,
        actor: &orkia_model::ActorId,
        role: GrantRole,
        grants: impl IntoIterator<Item = SemanticObjectRef>,
        repository: &str,
        policy: &RepositoryPolicy,
    ) -> Result<()> {
        for grant in grants {
            let document = match self.verify_access_grant(&grant, policy) {
                Ok(document) => document,
                Err(_) => continue,
            };
            let team_membership = document.teams.iter().any(|team| {
                self.verify_team(team, policy)
                    .is_ok_and(|team| team.members.contains(actor))
            });
            if (document.actor.as_ref() == Some(actor) || team_membership)
                && (document.role == GrantRole::Administrator || document.role == role)
                && (document.repositories.is_empty()
                    || document.repositories.contains("*")
                    || document.repositories.contains(repository))
            {
                return Ok(());
            }
        }
        Err(OrkiaError::Policy(
            "actor lacks a verified grant for the requested role".into(),
        ))
    }

    /// Encrypts a secret before it enters Git. Only the Argon2id salt,
    /// XChaCha20-Poly1305 nonce and ciphertext are written as an immutable
    /// semantic object; the mutable name ref merely selects the current
    /// revision. The object is signed before that ref becomes visible.
    pub fn store_vault_secret(
        &self,
        name: &str,
        plaintext: &[u8],
        password: &[u8],
        identity: &Identity,
    ) -> Result<SemanticObjectRef> {
        let reference = vault_ref_name(name)?;
        if password.is_empty() {
            return Err(OrkiaError::Invalid("vault password cannot be empty".into()));
        }
        let mut salt = [0u8; 16];
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut nonce);
        let key = vault_key(password, &salt)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|error| {
            OrkiaError::External(format!("cannot initialize vault cipher: {error}"))
        })?;
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|error| {
                OrkiaError::External(format!("cannot encrypt vault secret: {error}"))
            })?;
        let object = self.put_vault_entry(&VaultEntry {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: name.into(),
            algorithm: "argon2id+xchacha20poly1305".into(),
            salt: STANDARD_NO_PAD.encode(salt),
            nonce: STANDARD_NO_PAD.encode(nonce),
            ciphertext: STANDARD_NO_PAD.encode(ciphertext),
        })?;
        self.sign_document(&object, identity)?;
        let oid = Self::oid(&object.hash, "vault object hash")?;
        self.repository
            .repo()?
            .reference(&reference, oid, true, "Publish encrypted Orkia vault entry")
            .map_err(git_error)?;
        Ok(object)
    }

    /// Decrypts a named vault entry only after its signed Git object satisfies
    /// policy. A bad password and a tampered ciphertext both fail closed.
    pub fn read_vault_secret(
        &self,
        name: &str,
        password: &[u8],
        policy: &RepositoryPolicy,
    ) -> Result<Vec<u8>> {
        let reference = vault_ref_name(name)?;
        if password.is_empty() {
            return Err(OrkiaError::Invalid("vault password cannot be empty".into()));
        }
        let target = self
            .repository
            .repo()?
            .find_reference(&reference)
            .map_err(|_| OrkiaError::NotFound(format!("vault entry {name}")))?
            .target()
            .ok_or_else(|| OrkiaError::Integrity(format!("vault ref {reference} is symbolic")))?;
        let object = SemanticObjectRef {
            kind: SemanticObjectKind::Vault,
            hash: target.to_string(),
        };
        self.require_signature_quorum(&object, policy)?;
        let entry = self.get_vault_entry(&object)?;
        if entry.name != name {
            return Err(OrkiaError::Integrity(format!(
                "vault ref {reference} names {}, not {name}",
                entry.name
            )));
        }
        let salt = STANDARD_NO_PAD
            .decode(entry.salt)
            .map_err(|error| OrkiaError::Integrity(format!("invalid vault salt: {error}")))?;
        let nonce = STANDARD_NO_PAD
            .decode(entry.nonce)
            .map_err(|error| OrkiaError::Integrity(format!("invalid vault nonce: {error}")))?;
        let ciphertext = STANDARD_NO_PAD
            .decode(entry.ciphertext)
            .map_err(|error| OrkiaError::Integrity(format!("invalid vault ciphertext: {error}")))?;
        if salt.len() != 16 || nonce.len() != 24 {
            return Err(OrkiaError::Integrity(
                "vault salt or nonce has an invalid length".into(),
            ));
        }
        let key = vault_key(password, &salt)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|error| {
            OrkiaError::External(format!("cannot initialize vault cipher: {error}"))
        })?;
        cipher
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| OrkiaError::Integrity("cannot decrypt vault entry".into()))
    }

    /// Publishes an immutable signed review-plan revision under a Git ref so
    /// projections and PR stacks can be rebuilt by another clone.
    pub fn store_review_plan(
        &self,
        plan: &ReviewPlan,
        identity: &Identity,
    ) -> Result<SemanticObjectRef> {
        let reference = plan_ref_name(&plan.id, plan.revision);
        let object = self.put_review_plan(plan)?;
        self.sign_document(&object, identity)?;
        let oid = Self::oid(&object.hash, "review plan object hash")?;
        self.repository
            .repo()?
            .reference(
                &reference,
                oid,
                false,
                "Publish immutable Orkia review plan",
            )
            .map_err(git_error)?;
        Ok(object)
    }

    /// Loads the newest signed revision of a plan from Git, rather than a
    /// worktree-local cache.
    pub fn latest_review_plan(&self, id: &PlanId, policy: &RepositoryPolicy) -> Result<ReviewPlan> {
        let repo = self.repository.repo()?;
        let prefix = format!("{ORKIA_REF_PREFIX}plans/{}/", id.0);
        let mut candidates = Vec::new();
        for reference in repo
            .references_glob(&format!("{prefix}*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let target = reference
                .target()
                .ok_or_else(|| OrkiaError::Integrity("review plan ref is symbolic".into()))?;
            let object = SemanticObjectRef {
                kind: SemanticObjectKind::Plan,
                hash: target.to_string(),
            };
            self.require_signature_quorum(&object, policy)?;
            let plan = self.get_review_plan(&object)?;
            if plan.id != *id {
                return Err(OrkiaError::Integrity(format!(
                    "review plan ref under {} points to plan {}",
                    id.0, plan.id.0
                )));
            }
            candidates.push(plan);
        }
        candidates
            .into_iter()
            .max_by_key(|plan| plan.revision)
            .ok_or_else(|| OrkiaError::NotFound(format!("review plan {id:?}")))
    }

    fn bind_state_unchecked(&self, commit: &str, state: &SemanticObjectRef) -> Result<()> {
        if state.kind != SemanticObjectKind::State {
            return Err(OrkiaError::Invalid(
                "only a semantic state object can be bound to a commit".into(),
            ));
        }
        let manifest = self.get_state(state)?;
        let commit = Self::oid(commit, "commit")?;
        if manifest.commit != commit.to_string() {
            return Err(OrkiaError::Integrity(format!(
                "semantic state is for commit {}, not {commit}",
                manifest.commit
            )));
        }
        let state_oid = Self::oid(&state.hash, "semantic state hash")?;
        let repo = self.repository.repo()?;
        repo.find_commit(commit).map_err(git_error)?;
        repo.reference(
            &Self::state_ref_name(&commit),
            state_oid,
            true,
            "Bind verified Orkia semantic state to commit",
        )
        .map_err(git_error)?;
        Ok(())
    }
}

impl SemanticObjectStore for GitSemanticStore {
    fn put(&self, kind: SemanticObjectKind, bytes: &[u8]) -> Result<SemanticObjectRef> {
        let repo = self.repository.repo()?;
        let oid = repo.blob(bytes).map_err(git_error)?;
        let object = SemanticObjectRef {
            kind,
            hash: oid.to_string(),
        };
        repo.reference(
            &Self::object_ref_name(&object),
            oid,
            true,
            "Store Orkia semantic object",
        )
        .map_err(git_error)?;
        Ok(object)
    }

    fn get(&self, object: &SemanticObjectRef) -> Result<Vec<u8>> {
        let expected = Self::oid(&object.hash, "semantic object hash")?;
        let repo = self.repository.repo()?;
        let reference = repo
            .find_reference(&Self::object_ref_name(object))
            .map_err(|_| OrkiaError::NotFound(format!("semantic object {}", object.hash)))?;
        let actual = reference.target().ok_or_else(|| {
            OrkiaError::Integrity(format!(
                "semantic object ref {} is symbolic",
                reference.name().unwrap_or("<unnamed>")
            ))
        })?;
        if actual != expected {
            return Err(OrkiaError::Integrity(format!(
                "semantic object ref does not match requested hash: expected {expected}, found {actual}"
            )));
        }
        Ok(repo
            .find_blob(actual)
            .map_err(git_error)?
            .content()
            .to_vec())
    }

    fn state_for_commit(&self, commit: &str) -> Result<Option<SemanticObjectRef>> {
        let commit = Self::oid(commit, "commit")?;
        let repo = self.repository.repo()?;
        let reference = match repo.find_reference(&Self::state_ref_name(&commit)) {
            Ok(reference) => reference,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(git_error(error)),
        };
        let state = reference.target().ok_or_else(|| {
            OrkiaError::Integrity(format!("semantic state ref for {commit} is symbolic"))
        })?;
        let object = SemanticObjectRef {
            kind: SemanticObjectKind::State,
            hash: state.to_string(),
        };
        let manifest = self.get_state(&object)?;
        if manifest.commit != commit.to_string() {
            return Err(OrkiaError::Integrity(format!(
                "semantic state is for commit {}, not {commit}",
                manifest.commit
            )));
        }
        Ok(Some(object))
    }
}

impl SemanticDocumentStore for GitSemanticStore {
    fn put_operation(&self, operation: &SemanticOperation) -> Result<SemanticObjectRef> {
        self.put_document(operation)
    }

    fn get_operation(&self, object: &SemanticObjectRef) -> Result<SemanticOperation> {
        self.get_document(object)
    }

    fn put_state(&self, state: &SemanticState) -> Result<SemanticObjectRef> {
        self.put_document(state)
    }

    fn get_state(&self, object: &SemanticObjectRef) -> Result<SemanticState> {
        let state: SemanticState = self.get_document(object)?;
        self.validate_operation_closure(state.operations.iter().cloned())?;
        self.validate_state_files(&state)?;
        Ok(state)
    }

    fn put_view(&self, view: &ViewMetadata) -> Result<SemanticObjectRef> {
        self.put_document(view)
    }

    fn get_view(&self, object: &SemanticObjectRef) -> Result<ViewMetadata> {
        let view: ViewMetadata = self.get_document(object)?;
        if let Some(parent) = &view.parent {
            self.get(parent)?;
        }
        self.validate_operation_closure(view.visible_operations.iter().cloned())?;
        Ok(view)
    }

    fn put_merge_resolution(&self, resolution: &MergeResolution) -> Result<SemanticObjectRef> {
        self.put_document(resolution)
    }

    fn get_merge_resolution(&self, object: &SemanticObjectRef) -> Result<MergeResolution> {
        let resolution: MergeResolution = self.get_document(object)?;
        self.validate_operation_closure(resolution.operations.iter().cloned())?;
        Ok(resolution)
    }

    fn put_attestation(&self, attestation: &Attestation) -> Result<SemanticObjectRef> {
        self.put_document(attestation)
    }

    fn get_attestation(&self, object: &SemanticObjectRef) -> Result<Attestation> {
        let attestation: Attestation = self.get_document(object)?;
        for evidence in &attestation.evidence {
            self.get(evidence)?;
        }
        Ok(attestation)
    }

    fn put_access_grant(&self, grant: &AccessGrant) -> Result<SemanticObjectRef> {
        self.put_document(grant)
    }

    fn get_access_grant(&self, object: &SemanticObjectRef) -> Result<AccessGrant> {
        self.get_document(object)
    }

    fn put_grant_revocation(&self, revocation: &GrantRevocation) -> Result<SemanticObjectRef> {
        self.put_document(revocation)
    }

    fn get_grant_revocation(&self, object: &SemanticObjectRef) -> Result<GrantRevocation> {
        let revocation: GrantRevocation = self.get_document(object)?;
        self.get_access_grant(&revocation.grant)?;
        Ok(revocation)
    }

    fn put_vault_entry(&self, entry: &VaultEntry) -> Result<SemanticObjectRef> {
        self.put_document(entry)
    }

    fn get_vault_entry(&self, object: &SemanticObjectRef) -> Result<VaultEntry> {
        self.get_document(object)
    }

    fn put_review_plan(&self, plan: &ReviewPlan) -> Result<SemanticObjectRef> {
        self.put_document(plan)
    }

    fn get_review_plan(&self, object: &SemanticObjectRef) -> Result<ReviewPlan> {
        self.get_document(object)
    }

    fn put_organization(&self, organization: &Organization) -> Result<SemanticObjectRef> {
        self.put_document(organization)
    }

    fn get_organization(&self, object: &SemanticObjectRef) -> Result<Organization> {
        self.get_document(object)
    }

    fn put_team(&self, team: &Team) -> Result<SemanticObjectRef> {
        self.put_document(team)
    }

    fn get_team(&self, object: &SemanticObjectRef) -> Result<Team> {
        self.get_document(object)
    }

    fn put_intent(&self, intent: &Intent) -> Result<SemanticObjectRef> {
        self.put_document(intent)
    }

    fn get_intent(&self, object: &SemanticObjectRef) -> Result<Intent> {
        let intent: Intent = self.get_document(object)?;
        for evidence in &intent.evidence {
            self.get(evidence)?;
        }
        Ok(intent)
    }

    fn put_memory(&self, memory: &Memory) -> Result<SemanticObjectRef> {
        self.put_document(memory)
    }

    fn get_memory(&self, object: &SemanticObjectRef) -> Result<Memory> {
        let memory: Memory = self.get_document(object)?;
        for evidence in &memory.evidence {
            self.get(evidence)?;
        }
        Ok(memory)
    }

    fn put_key_rotation(&self, rotation: &KeyRotation) -> Result<SemanticObjectRef> {
        self.put_document(rotation)
    }

    fn get_key_rotation(&self, object: &SemanticObjectRef) -> Result<KeyRotation> {
        self.get_document(object)
    }

    fn put_signature(&self, signature: &SemanticSignature) -> Result<SemanticObjectRef> {
        self.put_document(signature)
    }

    fn get_signature(&self, object: &SemanticObjectRef) -> Result<SemanticSignature> {
        self.get_document(object)
    }

    fn activate_state(
        &self,
        commit: &str,
        state: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<()> {
        // Check the state itself before signatures so an invalid document can
        // never be made active merely by accumulating valid proofs.
        self.verify_active_state(state, policy)?;
        self.bind_state_unchecked(commit, state)
    }
}
impl LedgerStore for GitLedgerStore {
    fn append(&self, event: &LedgerEvent) -> Result<()> {
        let repo = self.repository.repo()?;
        migrate_legacy_ledger_ref(&repo)?;
        let bytes = orkia_model::canonical_json(event)?;
        let oid = repo.blob(&bytes).map_err(git_error)?;
        repo.reference(
            &ledger_event_ref_name(event)?,
            oid,
            false,
            "Append immutable Orkia ledger event",
        )
        .map_err(git_error)?;
        Ok(())
    }
    fn read_all(&self) -> Result<Vec<LedgerEvent>> {
        let repo = self.repository.repo()?;
        let mut events: Vec<LedgerEvent> = Vec::new();
        for reference in repo
            .references_glob(&format!("{LEDGER_EVENT_REF_PREFIX}/*"))
            .map_err(git_error)?
        {
            let reference = reference.map_err(git_error)?;
            let object = reference.peel(ObjectType::Blob).map_err(git_error)?;
            let blob = object.as_blob().ok_or_else(|| {
                OrkiaError::Integrity("ledger event ref does not point to a blob".into())
            })?;
            events.push(
                serde_json::from_slice(blob.content())
                    .map_err(|e| OrkiaError::Integrity(format!("invalid ledger event: {e}")))?,
            );
        }
        let legacy_ref = match self.repository.read_ledger() {
            Ok(Some(bytes)) => Some(bytes),
            Ok(None) => read_legacy_ledger_root(&repo)?,
            Err(error) => return Err(error),
        };
        if let Some(bytes) = legacy_ref {
            let legacy_events: Vec<LedgerEvent> = serde_json::from_slice(&bytes)
                .map_err(|e| OrkiaError::Integrity(format!("invalid ledger blob: {e}")))?;
            events.extend(legacy_events);
        }
        events.sort_by(|left, right| {
            (
                left.unsigned.occurred_at,
                &left.unsigned.actor,
                &left.unsigned.id,
                &left.hash,
            )
                .cmp(&(
                    right.unsigned.occurred_at,
                    &right.unsigned.actor,
                    &right.unsigned.id,
                    &right.hash,
                ))
        });
        Ok(events)
    }
}

fn read_legacy_ledger_root(repo: &Repository) -> Result<Option<Vec<u8>>> {
    let reference = match repo.find_reference(LEGACY_LEDGER_ROOT_REF) {
        Ok(reference) => reference,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(git_error(error)),
    };
    let object = reference.peel(ObjectType::Blob).map_err(git_error)?;
    Ok(Some(
        object
            .as_blob()
            .ok_or_else(|| OrkiaError::Integrity("legacy ledger ref is not a blob".into()))?
            .content()
            .to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use orkia_ports::{GitRepository, LedgerStore, SemanticDocumentStore, SemanticObjectStore};
    #[test]
    fn ledger_lives_in_a_dedicated_ref() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        let git = LibGit2Repository::open(dir.path()).unwrap();
        git.write_ledger(b"[]").unwrap();
        assert_eq!(git.read_ledger().unwrap(), Some(b"[]".to_vec()));
    }

    #[test]
    fn semantic_objects_are_immutable_git_blobs_bound_to_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let store = git.semantic_store();
        let commit = git.head_commit().unwrap();
        let operation = store
            .put_operation(&SemanticOperation {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                base_commit: commit.clone(),
                dependencies: Default::default(),
                action: orkia_model::SemanticOperationAction::Delete {
                    target: orkia_model::SemanticNodeId("leaf-1".into()),
                },
            })
            .unwrap();
        assert_eq!(store.get_operation(&operation).unwrap().base_commit, commit);

        let state = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: commit.clone(),
                parent_commits: Default::default(),
                files: Default::default(),
                operations: std::collections::BTreeSet::from([operation.clone()]),
                trunks: Default::default(),
            })
            .unwrap();
        let policy = RepositoryPolicy::default();
        assert_eq!(store.state_for_commit(&commit).unwrap(), None);
        assert!(store.activate_state(&commit, &state, &policy).is_err());
        let signer = Identity::generate("Ada");
        let signature = store.sign_document(&state, &signer).unwrap();
        assert_eq!(store.get_signature(&signature).unwrap().subject, state);
        assert!(store.activate_state(&commit, &state, &policy).is_err());
        store.sign_document(&operation, &signer).unwrap();
        store.activate_state(&commit, &state, &policy).unwrap();
        assert_eq!(
            store.state_for_commit(&commit).unwrap(),
            Some(state.clone())
        );

        let repo = Repository::open(dir.path()).unwrap();
        assert!(
            repo.find_reference(&GitSemanticStore::object_ref_name(&operation))
                .is_ok()
        );
        assert!(
            repo.find_reference(&GitSemanticStore::state_ref_name(
                &Oid::from_str(&commit).unwrap()
            ))
            .is_ok()
        );
        let no_signature_policy = RepositoryPolicy {
            minimum_semantic_signatures: 0,
            ..RepositoryPolicy::default()
        };
        assert!(
            store
                .activate_state(&commit, &operation, &no_signature_policy)
                .is_err()
        );

        let wrong_commit = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: "not-the-bound-commit".into(),
                parent_commits: Default::default(),
                files: Default::default(),
                operations: Default::default(),
                trunks: Default::default(),
            })
            .unwrap();
        assert!(
            store
                .activate_state(&commit, &wrong_commit, &no_signature_policy)
                .is_err()
        );

        let incomplete_closure = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: commit.clone(),
                parent_commits: Default::default(),
                files: Default::default(),
                operations: std::collections::BTreeSet::from([SemanticObjectRef {
                    kind: SemanticObjectKind::Operation,
                    hash: "0000000000000000000000000000000000000000".into(),
                }]),
                trunks: Default::default(),
            })
            .unwrap();
        assert!(
            store
                .activate_state(&commit, &incomplete_closure, &no_signature_policy)
                .is_err()
        );
    }

    #[test]
    fn sandbox_seal_is_reproducible_and_uses_only_the_committed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let blob = repo.blob(b"tracked\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("tracked.txt", blob, 0o100644).unwrap();
        let tree_id = builder.write().unwrap();
        drop(builder);
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        std::fs::write(dir.path().join("untracked-secret.txt"), "never sealed").unwrap();

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        git.materialize_semantic_state(&commit.to_string(), &identity, &policy)
            .unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_output = first.path().join("image");
        let second_output = second.path().join("image");
        let first_seal = git
            .seal_sandbox(&commit.to_string(), &first_output, &policy)
            .unwrap();
        let second_seal = git
            .seal_sandbox(&commit.to_string(), &second_output, &policy)
            .unwrap();
        assert_eq!(first_seal.manifest_digest, second_seal.manifest_digest);
        assert_eq!(first_seal.layer_digest, second_seal.layer_digest);
        let layer = std::fs::read(
            first_output
                .join("blobs/sha256")
                .join(&first_seal.layer_digest),
        )
        .unwrap();
        let mut archive = tar::Archive::new(Cursor::new(layer));
        let paths = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![PathBuf::from("tracked.txt")]);
        assert_eq!(
            LibGit2Repository::verify_sandbox(&first_output)
                .unwrap()
                .manifest_digest,
            first_seal.manifest_digest
        );
        std::fs::write(
            first_output
                .join("blobs/sha256")
                .join(first_seal.layer_digest),
            b"tampered",
        )
        .unwrap();
        assert!(LibGit2Repository::verify_sandbox(&first_output).is_err());
    }

    #[test]
    fn ledger_events_are_immutable_git_refs() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let event = LedgerEvent {
            unsigned: orkia_model::UnsignedEvent {
                id: orkia_model::EventId::new(),
                repository: orkia_model::RepositoryId::new(),
                actor: orkia_model::ActorId::new(),
                occurred_at: time::OffsetDateTime::UNIX_EPOCH,
                previous_hash: None,
                event: orkia_model::CaptureEvent::SessionClosed {
                    session: orkia_model::SessionId::new(),
                },
            },
            hash: "a1b2c3".into(),
            signature: "test-signature".into(),
        };

        git.ledger_store().append(&event).unwrap();
        assert_eq!(git.ledger_store().read_all().unwrap(), vec![event.clone()]);
        let repo = Repository::open(dir.path()).unwrap();
        assert!(
            repo.find_reference(&ledger_event_ref_name(&event).unwrap())
                .is_ok()
        );
        assert!(git.ledger_store().append(&event).is_err());
    }

    #[test]
    fn session_attestation_is_signed_and_verifiable_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let attestation = git
            .semantic_store()
            .attest_session(
                SessionId::new(),
                commit.to_string(),
                None,
                BTreeSet::new(),
                &identity,
            )
            .unwrap();
        assert_eq!(
            git.semantic_store()
                .verify_attestation(&attestation, &RepositoryPolicy::default())
                .unwrap()
                .actor,
            identity.actor().id
        );
        // A co-signed blob is insufficient if the attestation's own actor
        // proof was forged or rebound to a different public key.
        let mut forged = git.semantic_store().get_attestation(&attestation).unwrap();
        forged.actor_public_key = Some(Identity::generate("Mallory").actor().public_key.clone());
        let forged = git.semantic_store().put_attestation(&forged).unwrap();
        git.semantic_store()
            .sign_document(&forged, &identity)
            .unwrap();
        assert!(
            git.semantic_store()
                .verify_attestation(&forged, &RepositoryPolicy::default())
                .is_err()
        );
        // Existing schema-v1 blobs remain readable when their detached proof
        // is from the actor named by the attestation.
        let mut legacy = git.semantic_store().get_attestation(&attestation).unwrap();
        legacy.actor_public_key = None;
        let legacy = git.semantic_store().put_attestation(&legacy).unwrap();
        git.semantic_store()
            .sign_document(&legacy, &identity)
            .unwrap();
        assert!(
            git.semantic_store()
                .verify_attestation(&legacy, &RepositoryPolicy::default())
                .is_ok()
        );
    }

    #[test]
    fn trusted_signed_grant_authorizes_only_its_subject_and_role() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let issuer = Identity::generate("Issuer");
        let subject = Identity::generate("Maintainer");
        let grant = git
            .semantic_store()
            .put_access_grant(&AccessGrant {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                issuer: issuer.actor().id.clone(),
                actor: Some(subject.actor().id.clone()),
                role: GrantRole::SharedViewMaintainer,
                repositories: BTreeSet::from(["*".into()]),
                teams: BTreeSet::new(),
                expires_at: None,
            })
            .unwrap();
        git.semantic_store().sign_document(&grant, &issuer).unwrap();
        let policy = RepositoryPolicy {
            authorized_grant_issuers: BTreeSet::from([issuer.actor().id.clone()]),
            ..RepositoryPolicy::default()
        };
        git.semantic_store()
            .require_role(
                &subject.actor().id,
                GrantRole::SharedViewMaintainer,
                BTreeSet::from([grant.clone()]),
                &policy,
            )
            .unwrap();
        let revocation = git
            .semantic_store()
            .put_grant_revocation(&GrantRevocation {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                grant: grant.clone(),
                issuer: issuer.actor().id.clone(),
                reason: "access removed".into(),
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&revocation, &issuer)
            .unwrap();
        assert!(
            git.semantic_store()
                .require_role(
                    &subject.actor().id,
                    GrantRole::SharedViewMaintainer,
                    BTreeSet::from([grant.clone()]),
                    &policy,
                )
                .is_err()
        );
        let expired = git
            .semantic_store()
            .put_access_grant(&AccessGrant {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                issuer: issuer.actor().id.clone(),
                actor: Some(subject.actor().id.clone()),
                role: GrantRole::SharedViewMaintainer,
                repositories: BTreeSet::from(["*".into()]),
                teams: BTreeSet::new(),
                expires_at: Some("1970-01-01T00:00:00Z".into()),
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&expired, &issuer)
            .unwrap();
        assert!(
            git.semantic_store()
                .require_role(
                    &subject.actor().id,
                    GrantRole::SharedViewMaintainer,
                    BTreeSet::from([expired]),
                    &policy,
                )
                .is_err()
        );
        let scoped = git
            .semantic_store()
            .put_access_grant(&AccessGrant {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                issuer: issuer.actor().id.clone(),
                actor: Some(subject.actor().id.clone()),
                role: GrantRole::SharedViewMaintainer,
                repositories: BTreeSet::from(["repository-a".into()]),
                teams: BTreeSet::new(),
                expires_at: None,
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&scoped, &issuer)
            .unwrap();
        git.semantic_store()
            .require_role_for_repository(
                &subject.actor().id,
                GrantRole::SharedViewMaintainer,
                BTreeSet::from([scoped.clone()]),
                "repository-a",
                &policy,
            )
            .unwrap();
        assert!(
            git.semantic_store()
                .require_role_for_repository(
                    &subject.actor().id,
                    GrantRole::SharedViewMaintainer,
                    BTreeSet::from([scoped]),
                    "repository-b",
                    &policy,
                )
                .is_err()
        );
        let organization = git
            .semantic_store()
            .put_organization(&Organization {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                slug: "engineering".into(),
                issuer: issuer.actor().id.clone(),
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&organization, &issuer)
            .unwrap();
        let team = git
            .semantic_store()
            .put_team(&Team {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                organization,
                name: "maintainers".into(),
                issuer: issuer.actor().id.clone(),
                members: BTreeSet::from([subject.actor().id.clone()]),
            })
            .unwrap();
        git.semantic_store().sign_document(&team, &issuer).unwrap();
        let team_grant = git
            .semantic_store()
            .put_access_grant(&AccessGrant {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                issuer: issuer.actor().id.clone(),
                actor: None,
                role: GrantRole::SharedViewMaintainer,
                repositories: BTreeSet::from(["*".into()]),
                teams: BTreeSet::from([team]),
                expires_at: None,
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&team_grant, &issuer)
            .unwrap();
        git.semantic_store()
            .require_role(
                &subject.actor().id,
                GrantRole::SharedViewMaintainer,
                BTreeSet::from([team_grant]),
                &policy,
            )
            .unwrap();
        assert!(
            git.semantic_store()
                .require_role(
                    &subject.actor().id,
                    GrantRole::Reviewer,
                    BTreeSet::from([grant]),
                    &policy,
                )
                .is_err()
        );
    }

    #[test]
    fn intents_and_memory_are_immutable_git_documents() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let intent = git
            .semantic_store()
            .put_intent(&Intent {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                title: "Harden parser".into(),
                body: "Keep compatibility".into(),
                session: None,
                evidence: BTreeSet::new(),
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&intent, &identity)
            .unwrap();
        assert_eq!(
            git.semantic_store().get_intent(&intent).unwrap().title,
            "Harden parser"
        );
        let memory = git
            .semantic_store()
            .put_memory(&Memory {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                topic: "parser".into(),
                content: "Use a fail-closed grammar".into(),
                evidence: BTreeSet::from([intent]),
            })
            .unwrap();
        git.semantic_store()
            .sign_document(&memory, &identity)
            .unwrap();
        assert_eq!(
            git.semantic_store().get_memory(&memory).unwrap().topic,
            "parser"
        );
    }

    #[test]
    fn vault_secret_is_encrypted_signed_and_recoverable_from_git() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let secret = b"token-that-must-not-appear-in-git";
        let entry = git
            .semantic_store()
            .store_vault_secret("api-token", secret, b"correct horse", &identity)
            .unwrap();
        let stored = git.semantic_store().get_vault_entry(&entry).unwrap();
        assert_ne!(stored.ciphertext.as_bytes(), secret);
        assert_eq!(
            git.semantic_store()
                .read_vault_secret("api-token", b"correct horse", &RepositoryPolicy::default())
                .unwrap(),
            secret
        );
        assert!(
            git.semantic_store()
                .read_vault_secret("api-token", b"wrong password", &RepositoryPolicy::default())
                .is_err()
        );
        assert!(
            Repository::open(dir.path())
                .unwrap()
                .find_reference("refs/orkia/vault/api-token")
                .is_ok()
        );
    }

    #[test]
    fn review_plan_revisions_are_signed_and_loaded_from_git_refs() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let id = PlanId::new();
        let plan = ReviewPlan {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            id: id.clone(),
            revision: 0,
            source_checkpoint: "checkpoint".into(),
            units: vec![orkia_model::ReviewUnit {
                id: orkia_model::ReviewUnitId::new(),
                title: "One unit".into(),
                atoms: BTreeSet::from([orkia_model::AtomId::new()]),
                depends_on: BTreeSet::new(),
                confidence_milli: 1000,
            }],
            atoms: Vec::new(),
            atom_paths: BTreeMap::new(),
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Proposed,
            created_from: BTreeSet::new(),
        };
        git.semantic_store()
            .store_review_plan(&plan, &identity)
            .unwrap();
        let mut revision = plan.clone();
        revision.revision = 1;
        git.semantic_store()
            .store_review_plan(&revision, &identity)
            .unwrap();
        assert_eq!(
            git.semantic_store()
                .latest_review_plan(&id, &RepositoryPolicy::default())
                .unwrap()
                .revision,
            1
        );
        assert!(
            Repository::open(dir.path())
                .unwrap()
                .find_reference(&plan_ref_name(&id, 1))
                .is_ok()
        );
    }

    #[test]
    fn appending_migrates_the_legacy_ledger_ref_without_losing_events() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let legacy_event = LedgerEvent {
            unsigned: orkia_model::UnsignedEvent {
                id: orkia_model::EventId::new(),
                repository: orkia_model::RepositoryId::new(),
                actor: orkia_model::ActorId::new(),
                occurred_at: time::OffsetDateTime::UNIX_EPOCH,
                previous_hash: None,
                event: orkia_model::CaptureEvent::SessionClosed {
                    session: orkia_model::SessionId::new(),
                },
            },
            hash: "010203".into(),
            signature: "legacy-signature".into(),
        };
        let legacy_blob = repo
            .blob(&serde_json::to_vec(&vec![legacy_event.clone()]).unwrap())
            .unwrap();
        repo.reference(
            LEGACY_LEDGER_ROOT_REF,
            legacy_blob,
            false,
            "Old aggregate Orkia ledger",
        )
        .unwrap();
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let new_event = LedgerEvent {
            unsigned: orkia_model::UnsignedEvent {
                id: orkia_model::EventId::new(),
                repository: orkia_model::RepositoryId::new(),
                actor: orkia_model::ActorId::new(),
                occurred_at: time::OffsetDateTime::UNIX_EPOCH,
                previous_hash: Some(legacy_event.hash.clone()),
                event: orkia_model::CaptureEvent::SessionClosed {
                    session: orkia_model::SessionId::new(),
                },
            },
            hash: "040506".into(),
            signature: "new-signature".into(),
        };
        git.ledger_store().append(&new_event).unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        assert!(repo.find_reference(LEGACY_LEDGER_ROOT_REF).is_err());
        assert!(repo.find_reference(LEDGER_REF).is_ok());
        assert!(
            repo.find_reference(&ledger_event_ref_name(&new_event).unwrap())
                .is_ok()
        );
        let events = git.ledger_store().read_all().unwrap();
        assert!(events.contains(&legacy_event));
        assert!(events.contains(&new_event));
    }

    #[test]
    fn semantic_refs_round_trip_through_a_normal_git_remote() {
        let source_dir = tempfile::tempdir().unwrap();
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver_two_dir = tempfile::tempdir().unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();

        let source = Repository::init(source_dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = source.treebuilder(None).unwrap().write().unwrap();
        let tree = source.find_tree(tree_id).unwrap();
        source
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        source
            .remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        drop(tree);
        drop(source);

        let source = LibGit2Repository::open(source_dir.path()).unwrap();
        let source_commit = source.head_commit().unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        let state = source
            .materialize_semantic_state(&source_commit, &identity, &policy)
            .unwrap();
        let plan_id = PlanId::new();
        let plan = ReviewPlan {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            id: plan_id.clone(),
            revision: 0,
            source_checkpoint: source_commit,
            units: vec![orkia_model::ReviewUnit {
                id: orkia_model::ReviewUnitId::new(),
                title: "Remote plan".into(),
                atoms: BTreeSet::from([orkia_model::AtomId::new()]),
                depends_on: BTreeSet::new(),
                confidence_milli: 1000,
            }],
            atoms: Vec::new(),
            atom_paths: BTreeMap::new(),
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Proposed,
            created_from: BTreeSet::new(),
        };
        source
            .semantic_store()
            .store_review_plan(&plan, &identity)
            .unwrap();
        let ledger_event = LedgerEvent {
            unsigned: orkia_model::UnsignedEvent {
                id: orkia_model::EventId::new(),
                repository: orkia_model::RepositoryId::new(),
                actor: orkia_model::ActorId::new(),
                occurred_at: time::OffsetDateTime::UNIX_EPOCH,
                previous_hash: None,
                event: orkia_model::CaptureEvent::SessionClosed {
                    session: orkia_model::SessionId::new(),
                },
            },
            hash: "d4e5f6".into(),
            signature: "test-signature".into(),
        };
        source.ledger_store().append(&ledger_event).unwrap();
        source.push_orkia_refs("origin").unwrap();

        let receiver = Repository::init(receiver_dir.path()).unwrap();
        receiver
            .remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        drop(receiver);
        let receiver_two = Repository::init(receiver_two_dir.path()).unwrap();
        receiver_two
            .remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        drop(receiver_two);

        let receiver = LibGit2Repository::open(receiver_dir.path()).unwrap();
        assert_eq!(
            receiver
                .fetch_verified_orkia_refs("origin", &policy)
                .unwrap()
                .verified_states,
            1
        );
        assert_eq!(
            receiver
                .semantic_store()
                .get_state(&state)
                .unwrap()
                .schema_version,
            1
        );
        assert_eq!(
            receiver.ledger_store().read_all().unwrap(),
            vec![ledger_event]
        );
        assert_eq!(
            receiver
                .semantic_store()
                .latest_review_plan(&plan_id, &policy)
                .unwrap()
                .id,
            plan_id
        );
        // A second clone transports first and verifies afterwards; the same
        // Git refs must yield the identical immutable state and review plan.
        let receiver_two = LibGit2Repository::open(receiver_two_dir.path()).unwrap();
        receiver_two.fetch_orkia_refs("origin").unwrap();
        assert_eq!(
            receiver_two
                .verify_orkia_refs(&policy)
                .unwrap()
                .verified_states,
            1
        );
        assert_eq!(
            receiver_two.semantic_store().get_state(&state).unwrap(),
            receiver.semantic_store().get_state(&state).unwrap()
        );
        assert_eq!(
            receiver_two
                .semantic_store()
                .latest_review_plan(&plan_id, &policy)
                .unwrap(),
            receiver
                .semantic_store()
                .latest_review_plan(&plan_id, &policy)
                .unwrap()
        );
    }

    #[test]
    fn switch_view_checks_out_its_git_branch_in_the_primary_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        git.create_view(&ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "switchable".into(),
            branch: "views/switchable".into(),
            base_commit: commit.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        })
        .unwrap();
        git.switch_view("switchable").unwrap();
        assert_eq!(
            Repository::open(dir.path()).unwrap().head().unwrap().name(),
            Some("refs/heads/views/switchable")
        );
    }

    #[test]
    fn view_is_a_git_branch_with_a_portable_metadata_binding_and_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let metadata = ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "draft-1".into(),
            branch: "views/draft-1".into(),
            base_commit: commit.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        };
        let object = git.create_view(&metadata).unwrap();
        assert_eq!(git.view("draft-1").unwrap(), metadata);
        let status = git
            .view_status("draft-1", &RepositoryPolicy::default())
            .unwrap();
        assert_eq!(status.working_tree_changes, 0);
        assert!(status.metadata_matches_branch);
        assert!(!status.semantic_verified);
        assert_eq!(
            status.semantic_error.as_deref(),
            Some("no active semantic state for branch tip")
        );
        git.tag_view("draft-1", "draft-1-v1", "first draft view")
            .unwrap();

        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(
            repo.find_reference("refs/heads/views/draft-1")
                .unwrap()
                .target(),
            Some(commit)
        );
        assert_eq!(
            repo.find_reference(&view_ref_name("draft-1").unwrap())
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            object.hash
        );
        assert!(repo.find_reference("refs/tags/draft-1-v1").is_ok());
        drop(repo);

        let worktrees = tempfile::tempdir().unwrap();
        let checkout = worktrees.path().join("draft-1");
        git.create_view_worktree("draft-1", &checkout).unwrap();
        let checkout_repo = Repository::open(&checkout).unwrap();
        assert_eq!(
            checkout_repo.head().unwrap().name(),
            Some("refs/heads/views/draft-1")
        );
        assert_eq!(checkout_repo.head().unwrap().target(), Some(commit));
    }

    #[test]
    fn advancing_a_view_requires_a_verified_active_semantic_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first = repo
            .commit(Some("HEAD"), &signature, &signature, "first", &tree, &[])
            .unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let second = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "second",
                &tree,
                &[&first_commit],
            )
            .unwrap();
        drop(first_commit);
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        git.create_view(&ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "shared".into(),
            branch: "views/shared".into(),
            base_commit: first.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        })
        .unwrap();
        let store = git.semantic_store();
        let state = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: second.to_string(),
                parent_commits: BTreeSet::from([first.to_string()]),
                files: BTreeMap::new(),
                operations: BTreeSet::new(),
                trunks: BTreeMap::new(),
            })
            .unwrap();
        let unsigned_policy = RepositoryPolicy {
            minimum_semantic_signatures: 0,
            ..RepositoryPolicy::default()
        };
        store
            .activate_state(&second.to_string(), &state, &unsigned_policy)
            .unwrap();
        assert!(
            git.advance_view("shared", &second.to_string(), &RepositoryPolicy::default())
                .is_err()
        );

        store
            .sign_document(&state, &Identity::generate("Ada"))
            .unwrap();
        git.advance_view("shared", &second.to_string(), &RepositoryPolicy::default())
            .unwrap();
        let view = git.view("shared").unwrap();
        assert_eq!(view.base_commit, second.to_string());
        assert_eq!(
            Repository::open(dir.path())
                .unwrap()
                .find_reference("refs/heads/views/shared")
                .unwrap()
                .target(),
            Some(second)
        );
    }

    #[test]
    fn shared_view_publication_requires_a_trusted_signed_maintainer_grant() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let first = repo
            .commit(Some("HEAD"), &signature, &signature, "first", &tree, &[])
            .unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let second = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "second",
                &tree,
                &[&first_commit],
            )
            .unwrap();
        drop(first_commit);
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        git.create_view(&ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "shared".into(),
            branch: "views/shared".into(),
            base_commit: first.to_string(),
            scope: orkia_model::ViewScope::Shared,
            parent: None,
            visible_operations: BTreeSet::new(),
        })
        .unwrap();
        let publisher = Identity::generate("Publisher");
        let issuer = Identity::generate("Issuer");
        let store = git.semantic_store();
        let state = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: second.to_string(),
                parent_commits: BTreeSet::from([first.to_string()]),
                files: BTreeMap::new(),
                operations: BTreeSet::new(),
                trunks: BTreeMap::new(),
            })
            .unwrap();
        store
            .activate_state(
                &second.to_string(),
                &state,
                &RepositoryPolicy {
                    minimum_semantic_signatures: 0,
                    ..RepositoryPolicy::default()
                },
            )
            .unwrap();
        store.sign_document(&state, &publisher).unwrap();
        let grant = store
            .put_access_grant(&AccessGrant {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                issuer: issuer.actor().id.clone(),
                actor: Some(publisher.actor().id.clone()),
                role: GrantRole::SharedViewMaintainer,
                repositories: BTreeSet::from(["*".into()]),
                teams: BTreeSet::new(),
                expires_at: None,
            })
            .unwrap();
        store.sign_document(&grant, &issuer).unwrap();
        let policy = RepositoryPolicy {
            authorized_grant_issuers: BTreeSet::from([issuer.actor().id.clone()]),
            ..RepositoryPolicy::default()
        };

        assert!(
            git.advance_view("shared", &second.to_string(), &policy)
                .is_err()
        );
        assert!(
            git.publish_shared_view(
                "shared",
                &second.to_string(),
                &publisher.actor().id,
                BTreeSet::new(),
                &policy,
            )
            .is_err()
        );
        git.publish_shared_view(
            "shared",
            &second.to_string(),
            &publisher.actor().id,
            BTreeSet::from([grant]),
            &policy,
        )
        .unwrap();
        assert_eq!(git.view("shared").unwrap().base_commit, second.to_string());
    }

    #[test]
    fn deleting_a_view_removes_only_its_refs_and_protects_shared_views() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        for (name, scope) in [
            ("draft", orkia_model::ViewScope::Draft),
            ("shared", orkia_model::ViewScope::Shared),
        ] {
            git.create_view(&ViewMetadata {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                name: name.into(),
                branch: format!("views/{name}"),
                base_commit: commit.to_string(),
                scope,
                parent: None,
                visible_operations: BTreeSet::new(),
            })
            .unwrap();
        }

        assert!(git.delete_view("shared", false).is_err());
        git.delete_view("draft", false).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        assert!(repo.find_reference("refs/heads/views/draft").is_err());
        assert!(
            repo.find_reference(&view_ref_name("draft").unwrap())
                .is_err()
        );
        assert!(repo.find_commit(commit).is_ok());
        drop(repo);

        git.delete_view("shared", true).unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        assert!(repo.find_reference("refs/heads/views/shared").is_err());
        assert!(
            repo.find_reference(&view_ref_name("shared").unwrap())
                .is_err()
        );
    }

    #[test]
    fn child_view_pins_the_immutable_revision_of_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let parent = git
            .create_view(&ViewMetadata {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                name: "parent".into(),
                branch: "views/parent".into(),
                base_commit: commit.to_string(),
                scope: orkia_model::ViewScope::Shared,
                parent: None,
                visible_operations: BTreeSet::new(),
            })
            .unwrap();
        let child = ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "child".into(),
            branch: "views/child".into(),
            base_commit: commit.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        };
        git.create_child_view(&child, "parent").unwrap();
        assert_eq!(git.view("child").unwrap().parent, Some(parent));
    }

    #[test]
    fn view_filter_accepts_only_operations_from_a_verified_active_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let store = git.semantic_store();
        let operation = store
            .put_operation(&SemanticOperation {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                base_commit: commit.to_string(),
                dependencies: BTreeSet::new(),
                action: SemanticOperationAction::Delete {
                    target: orkia_model::SemanticNodeId("trunk-1".into()),
                },
            })
            .unwrap();
        let state = store
            .put_state(&SemanticState {
                schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                commit: commit.to_string(),
                parent_commits: BTreeSet::new(),
                files: BTreeMap::new(),
                operations: BTreeSet::from([operation.clone()]),
                trunks: BTreeMap::new(),
            })
            .unwrap();
        let signer = Identity::generate("Ada");
        store.sign_document(&operation, &signer).unwrap();
        store.sign_document(&state, &signer).unwrap();
        let policy = RepositoryPolicy::default();
        store
            .activate_state(&commit.to_string(), &state, &policy)
            .unwrap();
        git.create_view(&ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "filtered".into(),
            branch: "views/filtered".into(),
            base_commit: commit.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        })
        .unwrap();

        git.set_view_visible_operations("filtered", BTreeSet::from([operation.clone()]), &policy)
            .unwrap();
        assert_eq!(
            git.view("filtered").unwrap().visible_operations,
            BTreeSet::from([operation])
        );
        assert!(
            git.set_view_visible_operations(
                "filtered",
                BTreeSet::from([SemanticObjectRef {
                    kind: SemanticObjectKind::Operation,
                    hash: "0000000000000000000000000000000000000000".into(),
                }]),
                &policy,
            )
            .is_err()
        );
    }

    #[test]
    fn recording_a_checked_out_view_creates_git_commit_and_verified_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let base = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        git.materialize_semantic_state(&base.to_string(), &identity, &policy)
            .unwrap();
        git.create_view(&ViewMetadata {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            name: "recording".into(),
            branch: "views/recording".into(),
            base_commit: base.to_string(),
            scope: orkia_model::ViewScope::Draft,
            parent: None,
            visible_operations: BTreeSet::new(),
        })
        .unwrap();
        git.switch_view("recording").unwrap();
        std::fs::write(dir.path().join("recorded.rs"), "fn recorded() {}\n").unwrap();
        let state = git
            .record_view("recording", "record semantic change", &identity, &policy)
            .unwrap();
        let state = git.semantic_store().get_state(&state).unwrap();
        assert!(state.files.contains_key("recorded.rs"));
        assert_eq!(git.view("recording").unwrap().base_commit, state.commit);
        assert_eq!(
            git.query_trunks(&state.commit, Some("recorded"), &policy)
                .unwrap()
                .len(),
            1
        );
        let diff = git
            .semantic_diff(&base.to_string(), &state.commit, &policy)
            .unwrap();
        assert_eq!(diff.changed_paths, vec!["recorded.rs"]);
        assert_eq!(diff.added_trunks.len(), 1);
        assert_eq!(diff.added_operations.len(), 1);
        let blame = git
            .semantic_blame(&state.commit, "recorded.rs", &policy)
            .unwrap();
        assert_eq!(blame.trunks.len(), 1);
        assert_eq!(blame.operations.len(), 1);
        let history = git.semantic_log(&state.commit, 10, &policy).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|entry| entry.state.is_some()));
        let first_commit = state.commit;
        std::fs::write(dir.path().join("recorded.rs"), "fn revised() {}\n").unwrap();
        let revised = git
            .revise_view("recording", "revise semantic change", &identity, &policy)
            .unwrap();
        let revised = git.semantic_store().get_state(&revised).unwrap();
        assert_ne!(revised.commit, first_commit);
        assert_eq!(revised.parent_commits, BTreeSet::from([base.to_string()]));
        git.unrecord_view("recording", &policy).unwrap();
        assert_eq!(git.view("recording").unwrap().base_commit, base.to_string());
    }

    #[test]
    fn commit_extraction_uses_git_rename_detection_to_continue_a_trunk() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        std::fs::write(dir.path().join("old.rs"), "fn stable() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("old.rs")).unwrap();
        index.write().unwrap();
        let first_tree_id = index.write_tree().unwrap();
        let first_tree = repo.find_tree(first_tree_id).unwrap();
        let first = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "add old path",
                &first_tree,
                &[],
            )
            .unwrap();
        drop(first_tree);

        std::fs::rename(dir.path().join("old.rs"), dir.path().join("new.rs")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("old.rs")).unwrap();
        index.add_path(Path::new("new.rs")).unwrap();
        index.write().unwrap();
        let second_tree_id = index.write_tree().unwrap();
        let second_tree = repo.find_tree(second_tree_id).unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let second = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "rename path",
                &second_tree,
                &[&first_commit],
            )
            .unwrap();
        drop(second_tree);
        drop(first_commit);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        let first_ref = git
            .materialize_semantic_state(&first.to_string(), &identity, &policy)
            .unwrap();
        let first_state = git.semantic_store().get_state(&first_ref).unwrap();
        let second_ref = git
            .materialize_semantic_state(&second.to_string(), &identity, &policy)
            .unwrap();
        let second_state = git.semantic_store().get_state(&second_ref).unwrap();
        assert_eq!(first_state.trunks.len(), 1);
        assert_eq!(second_state.trunks.len(), 1);
        assert_eq!(first_state.operations.len(), 1);
        assert_eq!(second_state.operations, first_state.operations);
        let first_trunk = first_state.trunks.values().next().unwrap();
        let second_trunk = second_state.trunks.values().next().unwrap();
        assert_eq!(first_trunk.id, second_trunk.id);
        assert!(second_trunk.paths.contains("old.rs"));
        assert!(second_trunk.paths.contains("new.rs"));

        let repo = Repository::open(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("new.rs")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("new.rs")).unwrap();
        index.write().unwrap();
        let third_tree_id = index.write_tree().unwrap();
        let third_tree = repo.find_tree(third_tree_id).unwrap();
        let second_commit = repo.find_commit(second).unwrap();
        let third = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "delete path",
                &third_tree,
                &[&second_commit],
            )
            .unwrap();
        drop(third_tree);
        drop(second_commit);
        drop(repo);

        let third_state = git
            .materialize_semantic_state(&third.to_string(), &identity, &policy)
            .unwrap();
        let third_state = git.semantic_store().get_state(&third_state).unwrap();
        assert_eq!(third_state.trunks.len(), 1);
        assert_eq!(
            third_state.trunks.values().next().unwrap().state,
            orkia_model::TrunkState::Deleted
        );
        assert_eq!(third_state.operations.len(), 2);
    }

    #[test]
    fn rebase_with_an_identical_tree_reuses_trunks_and_operation_closure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let empty_id = repo.treebuilder(None).unwrap().write().unwrap();
        let empty = repo.find_tree(empty_id).unwrap();
        let root = repo
            .commit(Some("HEAD"), &signature, &signature, "root", &empty, &[])
            .unwrap();
        drop(empty);

        let blob = repo.blob(b"fn stable() {}\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("stable.rs", blob, 0o100644).unwrap();
        let changed_id = builder.write().unwrap();
        drop(builder);
        let changed = repo.find_tree(changed_id).unwrap();
        let root_commit = repo.find_commit(root).unwrap();
        let original = repo
            .commit(
                Some("refs/heads/original"),
                &signature,
                &signature,
                "original patch",
                &changed,
                &[&root_commit],
            )
            .unwrap();
        drop(changed);
        drop(root_commit);

        let empty = repo.find_tree(empty_id).unwrap();
        let root_commit = repo.find_commit(root).unwrap();
        let rebased_parent = repo
            .commit(
                Some("refs/heads/rebased-base"),
                &signature,
                &signature,
                "new parent with same tree",
                &empty,
                &[&root_commit],
            )
            .unwrap();
        drop(empty);
        drop(root_commit);
        let changed = repo.find_tree(changed_id).unwrap();
        let rebased_parent_commit = repo.find_commit(rebased_parent).unwrap();
        let rebased = repo
            .commit(
                Some("refs/heads/rebased"),
                &signature,
                &signature,
                "rebased patch",
                &changed,
                &[&rebased_parent_commit],
            )
            .unwrap();
        drop(changed);
        drop(rebased_parent_commit);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        let original_state = git
            .materialize_semantic_state(&original.to_string(), &identity, &policy)
            .unwrap();
        let original_state = git.semantic_store().get_state(&original_state).unwrap();
        let rebased_state = git
            .materialize_semantic_state(&rebased.to_string(), &identity, &policy)
            .unwrap();
        let rebased_state = git.semantic_store().get_state(&rebased_state).unwrap();
        assert_eq!(rebased_state.trunks, original_state.trunks);
        assert_eq!(rebased_state.operations, original_state.operations);
        assert_eq!(
            rebased_state.parent_commits,
            BTreeSet::from([rebased_parent.to_string()])
        );
    }

    #[test]
    fn history_import_materializes_parent_before_child_and_is_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let empty_id = repo.treebuilder(None).unwrap().write().unwrap();
        let empty = repo.find_tree(empty_id).unwrap();
        let base = repo
            .commit(Some("HEAD"), &signature, &signature, "base", &empty, &[])
            .unwrap();
        drop(empty);
        let blob = repo.blob(b"fn imported() {}\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("imported.rs", blob, 0o100644).unwrap();
        let tree_id = builder.write().unwrap();
        drop(builder);
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(base).unwrap();
        let tip = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "tip",
                &tree,
                &[&parent],
            )
            .unwrap();
        drop(tree);
        drop(parent);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        assert_eq!(
            git.import_semantic_history(&tip.to_string(), &identity, &policy)
                .unwrap()
                .len(),
            2
        );
        assert!(
            git.semantic_store()
                .state_for_commit(&base.to_string())
                .unwrap()
                .is_some()
        );
        assert!(
            git.semantic_store()
                .state_for_commit(&tip.to_string())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            git.import_semantic_history(&tip.to_string(), &identity, &policy)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn merge_inherits_semantic_operations_from_every_parent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let empty = repo.find_tree(empty).unwrap();
        let base = repo
            .commit(Some("HEAD"), &signature, &signature, "base", &empty, &[])
            .unwrap();
        drop(empty);
        let base_commit = repo.find_commit(base).unwrap();

        let left_blob = repo.blob(b"fn left() {}\n").unwrap();
        let mut left_builder = repo.treebuilder(None).unwrap();
        left_builder.insert("left.rs", left_blob, 0o100644).unwrap();
        let left_tree_id = left_builder.write().unwrap();
        drop(left_builder);
        let left_tree = repo.find_tree(left_tree_id).unwrap();
        let left = repo
            .commit(
                Some("refs/heads/left"),
                &signature,
                &signature,
                "left",
                &left_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(left_tree);

        let right_blob = repo.blob(b"fn right() {}\n").unwrap();
        let mut right_builder = repo.treebuilder(None).unwrap();
        right_builder
            .insert("right.rs", right_blob, 0o100644)
            .unwrap();
        let right_tree_id = right_builder.write().unwrap();
        drop(right_builder);
        let right_tree = repo.find_tree(right_tree_id).unwrap();
        let right = repo
            .commit(
                Some("refs/heads/right"),
                &signature,
                &signature,
                "right",
                &right_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(right_tree);
        drop(base_commit);

        let mut merge_builder = repo.treebuilder(None).unwrap();
        merge_builder
            .insert("left.rs", left_blob, 0o100644)
            .unwrap();
        merge_builder
            .insert("right.rs", right_blob, 0o100644)
            .unwrap();
        let merge_tree_id = merge_builder.write().unwrap();
        drop(merge_builder);
        let merge_tree = repo.find_tree(merge_tree_id).unwrap();
        let left_commit = repo.find_commit(left).unwrap();
        let right_commit = repo.find_commit(right).unwrap();
        let merge = repo
            .commit(
                Some("refs/heads/merge"),
                &signature,
                &signature,
                "merge",
                &merge_tree,
                &[&left_commit, &right_commit],
            )
            .unwrap();
        drop(merge_tree);
        drop(left_commit);
        drop(right_commit);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        git.materialize_semantic_state(&base.to_string(), &identity, &policy)
            .unwrap();
        git.materialize_semantic_state(&left.to_string(), &identity, &policy)
            .unwrap();
        git.materialize_semantic_state(&right.to_string(), &identity, &policy)
            .unwrap();
        let merge_ref = git
            .materialize_semantic_state(&merge.to_string(), &identity, &policy)
            .unwrap();
        let merge_state = git.semantic_store().get_state(&merge_ref).unwrap();
        assert_eq!(merge_state.parent_commits.len(), 2);
        assert_eq!(merge_state.operations.len(), 2);
        assert_eq!(merge_state.trunks.len(), 2);

        let semantic = git
            .semantic_merge(
                &left.to_string(),
                &right.to_string(),
                "semantic-result",
                &identity,
                &policy,
            )
            .unwrap();
        assert_eq!(semantic.outcome, MergeOutcome::Merged);
        assert!(semantic.result_commit.is_some());
        assert_eq!(
            git.semantic_store()
                .get_merge_resolution(&semantic.resolution)
                .unwrap()
                .outcome,
            MergeOutcome::Merged
        );
    }

    #[test]
    fn semantic_merge_resolves_disjoint_same_line_token_edits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let make_tree = |repo: &Repository, content: &[u8]| {
            let blob = repo.blob(content).unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("merge.rs", blob, 0o100644).unwrap();
            builder.write().unwrap()
        };
        let base_tree = repo
            .find_tree(make_tree(&repo, b"let left = 1; let right = 2;\n"))
            .unwrap();
        let base = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "base",
                &base_tree,
                &[],
            )
            .unwrap();
        drop(base_tree);
        let base_commit = repo.find_commit(base).unwrap();
        let left_tree = repo
            .find_tree(make_tree(&repo, b"let left = 10; let right = 2;\n"))
            .unwrap();
        let left = repo
            .commit(
                Some("refs/heads/left"),
                &signature,
                &signature,
                "left",
                &left_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(left_tree);
        let right_tree = repo
            .find_tree(make_tree(&repo, b"let left = 1; let right = 20;\n"))
            .unwrap();
        let right = repo
            .commit(
                Some("refs/heads/right"),
                &signature,
                &signature,
                "right",
                &right_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(right_tree);
        drop(base_commit);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        for commit in [base, left, right] {
            git.materialize_semantic_state(&commit.to_string(), &identity, &policy)
                .unwrap();
        }
        let result = git
            .semantic_merge(
                &left.to_string(),
                &right.to_string(),
                "semantic-token-result",
                &identity,
                &policy,
            )
            .unwrap();
        assert_eq!(result.outcome, MergeOutcome::Merged);
        let reversed = git
            .semantic_merge(
                &right.to_string(),
                &left.to_string(),
                "semantic-token-result-reversed",
                &identity,
                &policy,
            )
            .unwrap();
        assert_eq!(reversed.outcome, MergeOutcome::Merged);
        let result_repo = Repository::open(dir.path()).unwrap();
        let result = result_repo
            .find_commit(Oid::from_str(result.result_commit.as_deref().unwrap()).unwrap())
            .unwrap();
        let reversed = result_repo
            .find_commit(Oid::from_str(reversed.result_commit.as_deref().unwrap()).unwrap())
            .unwrap();
        assert_eq!(result.tree_id(), reversed.tree_id());
        let tree = result.tree().unwrap();
        let entry = tree.get_path(Path::new("merge.rs")).unwrap();
        let blob = result_repo.find_blob(entry.id()).unwrap();
        assert_eq!(
            std::str::from_utf8(blob.content()).unwrap(),
            "let left = 10; let right = 20;\n"
        );
    }

    #[test]
    fn manual_git_resolution_becomes_a_signed_successor_of_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree = |repo: &Repository, contents: &[u8]| {
            let blob = repo.blob(contents).unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("conflict.rs", blob, 0o100644).unwrap();
            builder.write().unwrap()
        };
        let base_tree = repo.find_tree(tree(&repo, b"let value = 1;\n")).unwrap();
        let base = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "base",
                &base_tree,
                &[],
            )
            .unwrap();
        drop(base_tree);
        let base_commit = repo.find_commit(base).unwrap();
        let left_tree = repo.find_tree(tree(&repo, b"let value = 2;\n")).unwrap();
        let left = repo
            .commit(
                Some("refs/heads/left"),
                &signature,
                &signature,
                "left",
                &left_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(left_tree);
        let right_tree = repo.find_tree(tree(&repo, b"let value = 3;\n")).unwrap();
        let right = repo
            .commit(
                Some("refs/heads/right"),
                &signature,
                &signature,
                "right",
                &right_tree,
                &[&base_commit],
            )
            .unwrap();
        drop(right_tree);
        let left_commit = repo.find_commit(left).unwrap();
        let right_commit = repo.find_commit(right).unwrap();
        let resolved_tree = repo.find_tree(tree(&repo, b"let value = 4;\n")).unwrap();
        let resolved = repo
            .commit(
                Some("refs/heads/manual-resolution"),
                &signature,
                &signature,
                "manual resolution",
                &resolved_tree,
                &[&left_commit, &right_commit],
            )
            .unwrap();
        drop(resolved_tree);
        drop(right_commit);
        drop(left_commit);
        drop(base_commit);
        drop(repo);

        let git = LibGit2Repository::open(dir.path()).unwrap();
        let identity = Identity::generate("Ada");
        let policy = RepositoryPolicy::default();
        for commit in [base, left, right, resolved] {
            git.materialize_semantic_state(&commit.to_string(), &identity, &policy)
                .unwrap();
        }
        let conflict = git
            .semantic_merge(
                &left.to_string(),
                &right.to_string(),
                "semantic-conflict",
                &identity,
                &policy,
            )
            .unwrap();
        assert_eq!(conflict.outcome, MergeOutcome::Conflict);
        let final_resolution = git
            .finalize_merge_resolution(
                &conflict.resolution,
                &resolved.to_string(),
                &identity,
                &policy,
            )
            .unwrap();
        let final_resolution = git
            .semantic_store()
            .get_merge_resolution(&final_resolution)
            .unwrap();
        assert_eq!(final_resolution.result_commit, Some(resolved.to_string()));
        assert_eq!(final_resolution.supersedes, Some(conflict.resolution));
    }

    #[test]
    fn pushes_projected_branch_to_a_local_remote() {
        let source_dir = tempfile::tempdir().unwrap();
        let remote_dir = tempfile::tempdir().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        repo.remote("origin", remote_dir.path().to_str().unwrap())
            .unwrap();
        drop(tree);
        drop(repo);
        let git = LibGit2Repository::open(source_dir.path()).unwrap();
        git.project_branch("orkia/test", "HEAD").unwrap();
        git.push_branch("origin", "orkia/test").unwrap();
        let remote = Repository::open_bare(remote_dir.path()).unwrap();
        assert!(remote.find_reference("refs/heads/orkia/test").is_ok());
    }
}
