//! Pure, serializable domain types for Orkia.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub type Hash = String;
pub type CanonicalJson = Vec<u8>;
/// Domain-owned timestamp type used by ports so infrastructure contracts do
/// not need to depend directly on the serialization/runtime crate.
pub type Timestamp = OffsetDateTime;
pub const SEMANTIC_SCHEMA_VERSION: u16 = 1;

/// RFC-8785-style deterministic JSON for Orkia's signed documents. Object
/// keys are recursively sorted and scalar rendering is delegated to
/// `serde_json`, preventing a serializer's struct/map insertion order from
/// changing an object ID or signature.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<CanonicalJson> {
    let value = serde_json::to_value(value).map_err(|error| {
        OrkiaError::Invalid(format!("cannot serialize canonical JSON: {error}"))
    })?;
    let mut output = Vec::new();
    write_canonical_json(&value, &mut output)?;
    Ok(output)
}

fn write_canonical_json(value: &serde_json::Value, output: &mut Vec<u8>) -> Result<()> {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .map_err(|error| {
                    OrkiaError::Invalid(format!("cannot encode JSON string: {error}"))
                })?
                .as_bytes(),
        ),
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        serde_json::Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .map_err(|error| {
                            OrkiaError::Invalid(format!("cannot encode JSON object key: {error}"))
                        })?
                        .as_bytes(),
                );
                output.push(b':');
                write_canonical_json(&values[key], output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

/// Categories of immutable semantic objects stored alongside Git content.
///
/// The category is part of the Git ref namespace and is deliberately small:
/// it lets a clone discover which Orkia objects accompany a commit without
/// introducing an Orkia-owned object database.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticObjectKind {
    Operation,
    State,
    View,
    Plan,
    Resolution,
    Attestation,
    Signature,
    Vault,
    Grant,
    Organization,
    Team,
    Intent,
    Memory,
    KeyRotation,
    GrantRevocation,
    StackPullRequest,
    ChangeSet,
    Stack,
    Projection,
}

impl SemanticObjectKind {
    /// Stable, path-safe segment used below `refs/orkia/objects/`.
    pub const fn ref_segment(self) -> &'static str {
        match self {
            Self::Operation => "operation",
            Self::State => "state",
            Self::View => "view",
            Self::Plan => "plan",
            Self::Resolution => "resolution",
            Self::Attestation => "attestation",
            Self::Signature => "signature",
            Self::Vault => "vault",
            Self::Grant => "grant",
            Self::Organization => "organization",
            Self::Team => "team",
            Self::Intent => "intent",
            Self::Memory => "memory",
            Self::KeyRotation => "key_rotation",
            Self::GrantRevocation => "grant_revocation",
            Self::StackPullRequest => "stack-pr",
            Self::ChangeSet => "changeset",
            Self::Stack => "stack",
            Self::Projection => "projection",
        }
    }
}

/// A signed, portable statement of desired work. Intents deliberately carry
/// no mutable local state: revisions are new Git objects linked by callers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub schema_version: u16,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub session: Option<SessionId>,
    #[serde(default)]
    pub evidence: BTreeSet<SemanticObjectRef>,
}

impl SemanticDocument for Intent {
    const KIND: SemanticObjectKind = SemanticObjectKind::Intent;
    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.title.trim().is_empty() || self.body.trim().is_empty() {
            return Err(OrkiaError::Invalid(
                "intent title and body cannot be empty".into(),
            ));
        }
        for evidence in &self.evidence {
            validate_object_ref(evidence, "intent evidence")?;
        }
        Ok(())
    }
}

/// Durable, signed repository knowledge with explicit semantic evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub schema_version: u16,
    pub topic: String,
    pub content: String,
    #[serde(default)]
    pub evidence: BTreeSet<SemanticObjectRef>,
}

/// A continuity proof from one public key to a replacement key for the same
/// actor. Both keys must sign the canonical document before consumers accept
/// the rotation, preventing unilateral key substitution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyRotation {
    pub schema_version: u16,
    pub actor: ActorId,
    pub previous_public_key: String,
    pub next_public_key: String,
}

impl SemanticDocument for KeyRotation {
    const KIND: SemanticObjectKind = SemanticObjectKind::KeyRotation;
    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.previous_public_key.is_empty()
            || self.next_public_key.is_empty()
            || self.previous_public_key == self.next_public_key
        {
            return Err(OrkiaError::Invalid(
                "key rotation must contain two distinct public keys".into(),
            ));
        }
        Ok(())
    }
}

impl SemanticDocument for Memory {
    const KIND: SemanticObjectKind = SemanticObjectKind::Memory;
    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.topic.trim().is_empty() || self.content.trim().is_empty() {
            return Err(OrkiaError::Invalid(
                "memory topic and content cannot be empty".into(),
            ));
        }
        for evidence in &self.evidence {
            validate_object_ref(evidence, "memory evidence")?;
        }
        Ok(())
    }
}

/// An immutable Orkia object addressed by the Git object ID of its blob.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SemanticObjectRef {
    pub kind: SemanticObjectKind,
    pub hash: Hash,
}

/// Versioned documents that comprise Orkia's Git-resident semantic overlay.
///
/// A document is valid independently of any local database. Its referenced
/// objects are checked by the Git adapter before the document becomes active.
pub trait SemanticDocument {
    const KIND: SemanticObjectKind;

    fn validate(&self) -> Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SemanticNodeId(pub String);

impl SemanticNodeId {
    pub fn validate(&self, field: &str) -> Result<()> {
        if self.0.is_empty() {
            return Err(OrkiaError::Invalid(format!("{field} cannot be empty")));
        }
        Ok(())
    }

    /// Content-addressed, deterministic node identity for the semantic tree.
    pub fn from_stable_parts(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        Self(hex::encode(hasher.finalize()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrunkState {
    Alive,
    Deleted,
    /// A deleted file identity retained because a live operation or
    /// resolution still names it.
    Zombie,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticNodeState {
    Alive,
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticLeaf {
    pub id: SemanticNodeId,
    pub state: SemanticNodeState,
    pub text_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticBranch {
    pub id: SemanticNodeId,
    pub state: SemanticNodeState,
    pub source_hash: Hash,
    #[serde(default)]
    pub leaves: Vec<SemanticLeaf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticTrunk {
    pub id: SemanticNodeId,
    pub state: TrunkState,
    /// All observed Git paths for this logical file identity. A rename adds a
    /// path while preserving `id`; historical paths stay auditable.
    #[serde(default)]
    pub paths: BTreeSet<String>,
    #[serde(default)]
    pub branches: Vec<SemanticBranch>,
}

impl SemanticTrunk {
    pub fn validate(&self) -> Result<()> {
        self.id.validate("trunk id")?;
        if self.paths.is_empty() {
            return Err(OrkiaError::Invalid("a trunk needs an observed path".into()));
        }
        let mut branch_ids = BTreeSet::new();
        for branch in &self.branches {
            branch.id.validate("branch id")?;
            validate_hash(&branch.source_hash, "branch source hash")?;
            if !branch_ids.insert(branch.id.clone()) {
                return Err(OrkiaError::Invalid(
                    "duplicate branch identity in trunk".into(),
                ));
            }
            let mut leaf_ids = BTreeSet::new();
            for leaf in &branch.leaves {
                leaf.id.validate("leaf id")?;
                validate_hash(&leaf.text_hash, "leaf text hash")?;
                if !leaf_ids.insert(leaf.id.clone()) {
                    return Err(OrkiaError::Invalid(
                        "duplicate leaf identity in branch".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticOperationAction {
    Insert {
        trunk: SemanticNodeId,
        after: Option<SemanticNodeId>,
        content_hash: Hash,
    },
    Delete {
        target: SemanticNodeId,
    },
    Move {
        target: SemanticNodeId,
        after: Option<SemanticNodeId>,
    },
    Replace {
        target: SemanticNodeId,
        content_hash: Hash,
    },
    Resolve {
        targets: BTreeSet<SemanticObjectRef>,
        content_hash: Hash,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticOperation {
    pub schema_version: u16,
    pub base_commit: Hash,
    #[serde(default)]
    pub dependencies: BTreeSet<SemanticObjectRef>,
    pub action: SemanticOperationAction,
}

impl SemanticDocument for SemanticOperation {
    const KIND: SemanticObjectKind = SemanticObjectKind::Operation;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.base_commit, "operation base commit")?;
        for dependency in &self.dependencies {
            validate_object_ref(dependency, "operation dependency")?;
        }
        match &self.action {
            SemanticOperationAction::Insert {
                trunk,
                after,
                content_hash,
            } => {
                trunk.validate("insert trunk")?;
                if let Some(after) = after {
                    after.validate("insert anchor")?;
                }
                validate_hash(content_hash, "insert content hash")
            }
            SemanticOperationAction::Delete { target } => target.validate("delete target"),
            SemanticOperationAction::Move { target, after } => {
                target.validate("move target")?;
                if let Some(after) = after {
                    after.validate("move anchor")?;
                }
                Ok(())
            }
            SemanticOperationAction::Replace {
                target,
                content_hash,
            } => {
                target.validate("replace target")?;
                validate_hash(content_hash, "replace content hash")
            }
            SemanticOperationAction::Resolve {
                targets,
                content_hash,
            } => {
                if targets.is_empty() {
                    return Err(OrkiaError::Invalid(
                        "a resolution must reference at least one alternative".into(),
                    ));
                }
                for target in targets {
                    validate_object_ref(target, "resolution target")?;
                }
                validate_hash(content_hash, "resolution content hash")
            }
        }
    }
}

/// Exact semantic coverage of a Git commit. File paths map to the Git blob
/// that was analysed; P2 will enrich each entry with Trunk/Branch/Leaf data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticState {
    pub schema_version: u16,
    pub commit: Hash,
    /// The Git commits whose semantic states were inherited to produce this
    /// state. A merge normally records more than one parent.
    #[serde(default)]
    pub parent_commits: BTreeSet<Hash>,
    #[serde(default)]
    pub files: BTreeMap<String, Hash>,
    #[serde(default)]
    pub operations: BTreeSet<SemanticObjectRef>,
    #[serde(default)]
    pub trunks: BTreeMap<SemanticNodeId, SemanticTrunk>,
}

impl SemanticDocument for SemanticState {
    const KIND: SemanticObjectKind = SemanticObjectKind::State;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.commit, "state commit")?;
        for parent in &self.parent_commits {
            validate_hash(parent, "state parent commit")?;
        }
        for (path, blob) in &self.files {
            if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..")
            {
                return Err(OrkiaError::Invalid(format!(
                    "invalid semantic state path: {path}"
                )));
            }
            validate_hash(blob, "state file blob")?;
        }
        for operation in &self.operations {
            if operation.kind != SemanticObjectKind::Operation {
                return Err(OrkiaError::Invalid(
                    "a state closure may contain only operation objects".into(),
                ));
            }
            validate_object_ref(operation, "state operation")?;
        }
        for (id, trunk) in &self.trunks {
            if id != &trunk.id {
                return Err(OrkiaError::Invalid(
                    "semantic state trunk map key does not match trunk id".into(),
                ));
            }
            trunk.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewScope {
    Draft,
    Shared,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewMetadata {
    pub schema_version: u16,
    pub name: String,
    pub branch: String,
    pub base_commit: Hash,
    pub scope: ViewScope,
    pub parent: Option<SemanticObjectRef>,
    #[serde(default)]
    pub visible_operations: BTreeSet<SemanticObjectRef>,
}

impl SemanticDocument for ViewMetadata {
    const KIND: SemanticObjectKind = SemanticObjectKind::View;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.name.is_empty() || self.branch.is_empty() {
            return Err(OrkiaError::Invalid(
                "view name and branch cannot be empty".into(),
            ));
        }
        validate_hash(&self.base_commit, "view base commit")?;
        if let Some(parent) = &self.parent {
            validate_object_ref(parent, "view parent")?;
            if parent.kind != SemanticObjectKind::View {
                return Err(OrkiaError::Invalid(
                    "view parent must reference a view object".into(),
                ));
            }
        }
        for operation in &self.visible_operations {
            if operation.kind != SemanticObjectKind::Operation {
                return Err(OrkiaError::Invalid(
                    "a view may expose only operation objects".into(),
                ));
            }
            validate_object_ref(operation, "view operation")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeOutcome {
    Merged,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeResolution {
    pub schema_version: u16,
    pub base_commit: Hash,
    pub left_commit: Hash,
    pub right_commit: Hash,
    pub result_commit: Option<Hash>,
    pub outcome: MergeOutcome,
    #[serde(default)]
    pub operations: BTreeSet<SemanticObjectRef>,
    /// Conflict resolutions are immutable follow-up documents. Keeping this
    /// edge lets a clone trace a manual resolution back to the conflict that
    /// required human judgment.
    #[serde(default)]
    pub supersedes: Option<SemanticObjectRef>,
}

impl SemanticDocument for MergeResolution {
    const KIND: SemanticObjectKind = SemanticObjectKind::Resolution;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.base_commit, "merge base commit")?;
        validate_hash(&self.left_commit, "merge left commit")?;
        validate_hash(&self.right_commit, "merge right commit")?;
        match (&self.outcome, &self.result_commit) {
            (MergeOutcome::Merged, Some(result)) => validate_hash(result, "merge result commit")?,
            (MergeOutcome::Merged, None) => {
                return Err(OrkiaError::Invalid(
                    "a successful merge must have a result commit".into(),
                ));
            }
            (MergeOutcome::Conflict, Some(result)) => validate_hash(result, "merge result commit")?,
            (MergeOutcome::Conflict, None) => {}
        }
        for operation in &self.operations {
            if operation.kind != SemanticObjectKind::Operation {
                return Err(OrkiaError::Invalid(
                    "a merge resolution may contain only operation objects".into(),
                ));
            }
            validate_object_ref(operation, "merge operation")?;
        }
        if let Some(previous) = &self.supersedes {
            if previous.kind != SemanticObjectKind::Resolution {
                return Err(OrkiaError::Invalid(
                    "a merge resolution may supersede only another resolution".into(),
                ));
            }
            validate_object_ref(previous, "superseded merge resolution")?;
        }
        Ok(())
    }
}

/// Immutable, signed authorization delegated to one actor. Grants are kept in
/// Git so every clone can evaluate the same policy offline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantRole {
    Administrator,
    SharedViewMaintainer,
    Reviewer,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccessGrant {
    pub schema_version: u16,
    pub issuer: ActorId,
    #[serde(default)]
    pub actor: Option<ActorId>,
    pub role: GrantRole,
    #[serde(default)]
    pub repositories: BTreeSet<String>,
    /// Teams whose verified members receive this grant in addition to the
    /// direct actor subject. Team objects remain immutable and signed.
    #[serde(default)]
    pub teams: BTreeSet<SemanticObjectRef>,
    pub expires_at: Option<String>,
}

/// Immutable revocation of a grant, signed by that grant's trusted issuer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GrantRevocation {
    pub schema_version: u16,
    pub grant: SemanticObjectRef,
    pub issuer: ActorId,
    pub reason: String,
}

impl SemanticDocument for GrantRevocation {
    const KIND: SemanticObjectKind = SemanticObjectKind::GrantRevocation;
    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.grant.kind != SemanticObjectKind::Grant || self.reason.trim().is_empty() {
            return Err(OrkiaError::Invalid(
                "revocation needs a grant and a reason".into(),
            ));
        }
        validate_object_ref(&self.grant, "revoked grant")
    }
}

impl SemanticDocument for AccessGrant {
    const KIND: SemanticObjectKind = SemanticObjectKind::Grant;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self
            .repositories
            .iter()
            .any(|repository| repository.is_empty())
        {
            return Err(OrkiaError::Invalid(
                "grant repository names cannot be empty".into(),
            ));
        }
        if let Some(expires_at) = &self.expires_at {
            time::OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
                .map_err(|error| {
                    OrkiaError::Invalid(format!("grant expiration must be RFC 3339: {error}"))
                })?;
        }
        for team in &self.teams {
            if team.kind != SemanticObjectKind::Team {
                return Err(OrkiaError::Invalid(
                    "grant teams must reference team objects".into(),
                ));
            }
            validate_object_ref(team, "grant team")?;
        }
        if self.actor.is_none() && self.teams.is_empty() {
            return Err(OrkiaError::Invalid(
                "grant must target an actor, a team, or both".into(),
            ));
        }
        Ok(())
    }
}

/// Immutable organization authority. Its issuer is checked against repository
/// policy before a team or grant can rely on it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Organization {
    pub schema_version: u16,
    pub slug: String,
    pub issuer: ActorId,
}

impl SemanticDocument for Organization {
    const KIND: SemanticObjectKind = SemanticObjectKind::Organization;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if !valid_vault_name(&self.slug) {
            return Err(OrkiaError::Invalid(
                "organization slug must contain only ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        Ok(())
    }
}

/// Immutable membership snapshot delegated by a trusted organization issuer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Team {
    pub schema_version: u16,
    pub organization: SemanticObjectRef,
    pub name: String,
    pub issuer: ActorId,
    #[serde(default)]
    pub members: BTreeSet<ActorId>,
}

impl SemanticDocument for Team {
    const KIND: SemanticObjectKind = SemanticObjectKind::Team;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.organization.kind != SemanticObjectKind::Organization {
            return Err(OrkiaError::Invalid(
                "team organization must reference an organization object".into(),
            ));
        }
        validate_object_ref(&self.organization, "team organization")?;
        if !valid_vault_name(&self.name) || self.members.is_empty() {
            return Err(OrkiaError::Invalid(
                "team name must be safe and team membership cannot be empty".into(),
            ));
        }
        Ok(())
    }
}

/// An encrypted, portable secret payload. The plaintext and the password are
/// never represented in this document or in a Git object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub schema_version: u16,
    pub name: String,
    pub algorithm: String,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

impl SemanticDocument for VaultEntry {
    const KIND: SemanticObjectKind = SemanticObjectKind::Vault;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if !valid_vault_name(&self.name) {
            return Err(OrkiaError::Invalid(
                "vault entry name must contain only ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        if self.algorithm != "argon2id+xchacha20poly1305" {
            return Err(OrkiaError::Invalid(format!(
                "unsupported vault encryption algorithm {}",
                self.algorithm
            )));
        }
        if self.salt.is_empty() || self.nonce.is_empty() || self.ciphertext.is_empty() {
            return Err(OrkiaError::Invalid(
                "vault salt, nonce and ciphertext cannot be empty".into(),
            ));
        }
        Ok(())
    }
}

pub fn valid_vault_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attestation {
    pub schema_version: u16,
    pub actor: ActorId,
    /// Public key used for the actor's direct proof over the attestation
    /// statement. The detached semantic-object proof is retained separately
    /// so repository policy can require additional co-signers.
    #[serde(default)]
    pub actor_public_key: Option<String>,
    pub session: SessionId,
    pub base_commit: Hash,
    pub result_commit: Option<Hash>,
    #[serde(default)]
    pub evidence: BTreeSet<SemanticObjectRef>,
    pub signature: String,
}

impl SemanticDocument for Attestation {
    const KIND: SemanticObjectKind = SemanticObjectKind::Attestation;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.actor_public_key.as_deref().is_some_and(str::is_empty) {
            return Err(OrkiaError::Invalid(
                "attestation actor public key cannot be empty".into(),
            ));
        }
        validate_hash(&self.base_commit, "attestation base commit")?;
        if let Some(result) = &self.result_commit {
            validate_hash(result, "attestation result commit")?;
        }
        if self.signature.is_empty() {
            return Err(OrkiaError::Invalid(
                "attestation signature cannot be empty".into(),
            ));
        }
        for evidence in &self.evidence {
            validate_object_ref(evidence, "attestation evidence")?;
        }
        Ok(())
    }
}

/// Detached Ed25519 proof over the exact Git blob bytes of a semantic object.
///
/// The signer travels with the proof for offline verification. Authorization
/// of that actor belongs to repository policy and is intentionally separate
/// from cryptographic validity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticSignature {
    pub schema_version: u16,
    pub subject: SemanticObjectRef,
    pub signer: Actor,
    pub signature: String,
}

impl SemanticDocument for SemanticSignature {
    const KIND: SemanticObjectKind = SemanticObjectKind::Signature;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_object_ref(&self.subject, "signature subject")?;
        if self.signer.public_key.is_empty() || self.signature.is_empty() {
            return Err(OrkiaError::Invalid(
                "a semantic signature needs a public key and a signature".into(),
            ));
        }
        Ok(())
    }
}

fn validate_schema_version(version: u16) -> Result<()> {
    if version != SEMANTIC_SCHEMA_VERSION {
        return Err(OrkiaError::Invalid(format!(
            "unsupported semantic schema version {version}; expected {SEMANTIC_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn validate_hash(hash: &str, field: &str) -> Result<()> {
    if hash.is_empty() {
        return Err(OrkiaError::Invalid(format!("{field} cannot be empty")));
    }
    Ok(())
}

fn validate_object_ref(object: &SemanticObjectRef, field: &str) -> Result<()> {
    validate_hash(&object.hash, field)
}

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id!(ActorId);
id!(SessionId);
id!(EventId);
id!(ReviewUnitId);
id!(PlanId);
id!(RepositoryId);
id!(StackPullRequestId);
id!(ChangeSetId);
id!(StackId);
id!(ProjectionId);

impl StackPullRequestId {
    /// Deterministic identity for an automatically derived causal work unit.
    /// Session identity and its closed atom set are durable across projection
    /// rewrites, unlike a Git commit hash.
    pub fn from_stable_parts(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl StackId {
    /// Stable identity for the stack represented by a closed set of
    /// stack PRs. Revisions then model changes in its projection or policy,
    /// rather than inventing a new stack for the same causal graph.
    pub fn from_stack_pull_requests(
        pull_requests: impl IntoIterator<Item = StackPullRequestId>,
    ) -> Self {
        let mut ids = pull_requests.into_iter().collect::<Vec<_>>();
        ids.sort();
        let mut hasher = Sha256::new();
        hasher.update(b"orkia:stack:v1");
        for id in ids {
            hasher.update(id.0.as_bytes());
        }
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl ChangeSetId {
    /// A ChangeSet is the durable multi-repository delivery group. Its stable
    /// identity derives from both the repository and stack identities. A
    /// StackId alone is only locally meaningful, so omitting its repository
    /// could collide across independent repositories.
    pub fn from_stack_references(stacks: impl IntoIterator<Item = ChangeSetStack>) -> Self {
        let mut references = stacks.into_iter().collect::<Vec<_>>();
        references.sort();
        let mut hasher = Sha256::new();
        hasher.update(b"orkia:changeset:v1");
        for reference in references {
            hasher.update(reference.repository.0.as_bytes());
            hasher.update(reference.stack.0.as_bytes());
        }
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl ProjectionId {
    /// A StackPullRequest has one durable projection identity per repository. New
    /// restack attempts become revisions of this projection rather than new
    /// review objects.
    pub fn from_stack_pull_request(pull_request: &StackPullRequestId) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"orkia:projection:v1");
        hasher.update(pull_request.0.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

/// Identifier for an extracted review atom.
///
/// New user-created atoms may use [`AtomId::new`]. Extractors must derive the
/// identifier from their stable inputs with [`AtomId::from_stable_parts`] so a
/// second analysis of unchanged Git content does not invent a new atom.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AtomId(pub Uuid);

impl AtomId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Produces a deterministic UUID-shaped ID from length-delimited inputs.
    ///
    /// Length delimiters make `(ab, c)` distinct from `(a, bc)`.  The UUID
    /// version and variant bits are set solely for interoperable formatting;
    /// identity comes from the SHA-256 digest of the supplied stable inputs.
    pub fn from_stable_parts(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl Default for AtomId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub id: ActorId,
    pub display_name: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureOrigin {
    Human,
    Codex,
    Claude,
    Gemini,
    Kimi,
    OpenCode,
    Cursor,
    Droid,
    Qwen,
    Unknown,
}

/// A provider-neutral action decoded from a native agent transcript or hook.
/// The raw source remains in the ledger separately; this type only records
/// facts that the adapter can identify without guessing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentActionKind {
    Prompt {
        content: String,
    },
    Tool {
        name: String,
        id: Option<String>,
        arguments: serde_json::Value,
        result: Option<serde_json::Value>,
        duration_millis: Option<u64>,
    },
    FileRead {
        path: String,
        content_hash: Option<Hash>,
    },
    FileWrite {
        path: String,
        before_hash: Option<Hash>,
        after_hash: Option<Hash>,
        added_lines: Option<u32>,
        removed_lines: Option<u32>,
    },
    Command {
        command: String,
        exit_code: Option<i32>,
        stdout: Option<String>,
        stderr: Option<String>,
        duration_millis: Option<u64>,
    },
    Turn {
        model: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        cost_micros: Option<u64>,
        text: Option<String>,
        thinking: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentSnapshotPhase {
    Started,
    Checkpoint,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureEvent {
    SessionStarted {
        session: SessionId,
        origin: CaptureOrigin,
        base_commit: String,
        objective: String,
    },
    Prompt {
        provider: String,
        content: String,
    },
    Transcript {
        provider: String,
        content: String,
    },
    ToolCall {
        tool: String,
        arguments: serde_json::Value,
        result: serde_json::Value,
    },
    /// The unmodified payload delivered by a native coding-agent hook.  Keeping
    /// it whole makes a new upstream field durable before Orkia has learned to
    /// interpret it, and preserves agent-specific provenance for reconstruction.
    AgentHook {
        agent: String,
        external_session: Option<String>,
        hook_event: String,
        cwd: Option<String>,
        payload: serde_json::Value,
    },
    /// An unmodified local transcript document from a supported coding agent.
    /// Binary sources (Cursor's SQLite database) are base64-encoded.
    AgentTranscript {
        agent: String,
        path: String,
        encoding: String,
        content: String,
    },
    /// Durable bijection between a provider's session identifier and an Orkia
    /// session.  It is appended once and reused by subsequent hook invocations.
    AgentSessionLinked {
        agent: String,
        external_session: String,
        session: SessionId,
    },
    /// Git state observed at the boundary of a linked live agent session.
    /// `unknown_write` is true whenever the final diff has a path for which
    /// the linked session did not supply a typed file-write action.
    AgentSessionSnapshot {
        agent: String,
        external_session: String,
        session: SessionId,
        phase: AgentSnapshotPhase,
        head_commit: String,
        changed_paths: BTreeSet<String>,
        observed_paths: BTreeSet<String>,
        unknown_write: bool,
    },
    /// A lossless source is recorded as `AgentHook` or `AgentTranscript`; this
    /// is the provider-neutral, causally useful interpretation of one fact.
    AgentAction {
        agent: String,
        external_session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        action: AgentActionKind,
    },
    /// Authenticated forge delivery retained in the signed ledger before any
    /// webhook-specific orchestration consumes it. The payload is lossless so
    /// a later server version can replay new GitHub event fields.
    ForgeWebhook {
        forge: String,
        event_name: String,
        delivery_id: String,
        payload: serde_json::Value,
    },
    FilesObserved {
        read: BTreeSet<String>,
        modified: BTreeSet<String>,
        unknown_write: bool,
    },
    Command {
        command: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Validation {
        command: String,
        passed: bool,
        output: String,
    },
    Checkpoint {
        commit: String,
    },
    /// Signed publication record for an automatically derived review plan. The
    /// plan itself is a reconstructible projection written beside the ledger.
    ReviewPlanCreated {
        plan: PlanId,
        checkpoint: String,
        atom_count: u32,
        coverage_milli: u16,
    },
    ReviewPlanRevised {
        plan: PlanId,
        revision: u32,
        reason: String,
    },
    ReviewPlanApproved {
        plan: PlanId,
        revision: u32,
    },
    ReviewPlanChangesRequested {
        plan: PlanId,
        revision: u32,
        reason: String,
    },
    StackPullRequestPublished {
        pull_request: StackPullRequestId,
        revision: u32,
        object: SemanticObjectRef,
    },
    ChangeSetPublished {
        changeset: ChangeSetId,
        revision: u32,
        object: SemanticObjectRef,
    },
    ChangeSetIntegrated {
        changeset: ChangeSetId,
        revision: u32,
    },
    ProjectionUpdated {
        projection: ProjectionId,
        pull_request: StackPullRequestId,
        revision: u32,
        commit: Option<String>,
    },
    /// Signed result of the final policy gate. The causal ledger records the
    /// decision independently of mutable forge state, whether the gate
    /// passed or failed.
    IntegrationEvaluated {
        plan: Option<PlanId>,
        changeset: Option<ChangeSetId>,
        branch: String,
        approvals: u8,
        passed: bool,
        reason: String,
    },
    SessionClosed {
        session: SessionId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnsignedEvent {
    pub id: EventId,
    pub repository: RepositoryId,
    pub actor: ActorId,
    pub occurred_at: OffsetDateTime,
    pub previous_hash: Option<Hash>,
    pub event: CaptureEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerEvent {
    #[serde(flatten)]
    pub unsigned: UnsignedEvent,
    pub hash: Hash,
    pub signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AtomKind {
    Symbol,
    Block,
    Hunk,
    Import,
    Test,
    Configuration,
    Migration,
}

impl AtomKind {
    /// Stable tag used when deriving an atom identity.
    pub const fn stable_tag(&self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Block => "block",
            Self::Hunk => "hunk",
            Self::Import => "import",
            Self::Test => "test",
            Self::Configuration => "configuration",
            Self::Migration => "migration",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeAtom {
    pub id: AtomId,
    pub kind: AtomKind,
    pub path: String,
    pub symbol: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: Hash,
    pub source_events: BTreeSet<EventId>,
}

/// Pure representation of a Git tree-to-worktree change. Infrastructure
/// adapters populate it; semantic and StackPullRequest crates can consume it without
/// depending on libgit2.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
    pub changed_start: u32,
    pub changed_end: u32,
}

/// An exact, context-bound fragment projected for a StackPullRequest. `before` is
/// matched against the parent projection; empty `before` represents an
/// insertion and is anchored by the neighbouring contexts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PatchFragment {
    pub atoms: BTreeSet<AtomId>,
    pub path: String,
    pub before: String,
    pub after: String,
    pub before_context: String,
    pub after_context: String,
    pub base_content_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DependencyKind {
    Hard,
    Causal,
    Import,
    Test,
    Configuration,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AtomDependency {
    pub from: AtomId,
    pub to: AtomId,
    pub kind: DependencyKind,
    pub confidence_milli: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewUnit {
    pub id: ReviewUnitId,
    pub title: String,
    pub atoms: BTreeSet<AtomId>,
    pub depends_on: BTreeSet<ReviewUnitId>,
    pub confidence_milli: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PlanStatus {
    Proposed,
    Approved,
    ChangesRequested,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewPlan {
    #[serde(default = "default_semantic_schema_version")]
    pub schema_version: u16,
    pub id: PlanId,
    pub revision: u32,
    pub source_checkpoint: String,
    /// Digest of the exact repository policy evaluated to create this plan.
    /// It is part of the signed plan bytes, so a later policy change cannot be
    /// silently applied to an earlier causal decision.
    #[serde(default)]
    pub policy_digest: Option<Hash>,
    pub units: Vec<ReviewUnit>,
    #[serde(default)]
    pub atom_paths: BTreeMap<AtomId, String>,
    /// The concrete intra-commit units from which the plan was derived.  They
    /// are persisted with the plan so projection and review never need to
    /// rediscover semantic boundaries from a later working tree.
    #[serde(default)]
    pub atoms: Vec<ChangeAtom>,
    pub coverage_milli: u16,
    pub status: PlanStatus,
    pub created_from: BTreeSet<EventId>,
}

impl SemanticDocument for ReviewPlan {
    const KIND: SemanticObjectKind = SemanticObjectKind::Plan;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.source_checkpoint, "review plan checkpoint")?;
        if let Some(digest) = &self.policy_digest
            && (digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(OrkiaError::Invalid(
                "review plan policy digest must be a SHA-256 hexadecimal value".into(),
            ));
        }
        if self.coverage_milli > 1000 {
            return Err(OrkiaError::Invalid(
                "review plan coverage cannot exceed 1000‰".into(),
            ));
        }
        if self.units.is_empty() {
            return Err(OrkiaError::Invalid(
                "review plan must contain at least one review unit".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for unit in &self.units {
            if unit.atoms.is_empty() || !ids.insert(unit.id.clone()) {
                return Err(OrkiaError::Invalid(
                    "review plan units must be non-empty and have unique IDs".into(),
                ));
            }
        }
        // New semantic plans must be closed over their concrete atoms. Empty
        // `atoms` remains readable for the legacy plan format, but once an
        // atom manifest is present no unit may invent, duplicate or omit an
        // atom and the path index must describe that same manifest.
        if !self.atoms.is_empty() {
            let atom_ids = self
                .atoms
                .iter()
                .map(|atom| atom.id.clone())
                .collect::<BTreeSet<_>>();
            if atom_ids.len() != self.atoms.len() {
                return Err(OrkiaError::Invalid(
                    "review plan atom manifest contains duplicate IDs".into(),
                ));
            }
            let mut assigned = BTreeSet::new();
            for unit in &self.units {
                for atom in &unit.atoms {
                    if !atom_ids.contains(atom) || !assigned.insert(atom.clone()) {
                        return Err(OrkiaError::Invalid(
                            "review plan units must partition the atom manifest".into(),
                        ));
                    }
                }
            }
            if assigned != atom_ids {
                return Err(OrkiaError::Invalid(
                    "review plan units must cover every atom in the manifest".into(),
                ));
            }
            if !self.atom_paths.is_empty()
                && self.atom_paths.keys().cloned().collect::<BTreeSet<_>>() != atom_ids
            {
                return Err(OrkiaError::Invalid(
                    "review plan atom paths must match the atom manifest".into(),
                ));
            }
        }
        // Dependencies are part of the signed review decision, so reject
        // dangling edges and cycles before a projection adapter can consume
        // the plan.
        for unit in &self.units {
            if unit.depends_on.contains(&unit.id)
                || unit
                    .depends_on
                    .iter()
                    .any(|dependency| !ids.contains(dependency))
            {
                return Err(OrkiaError::Invalid(
                    "review plan dependencies must be closed and acyclic".into(),
                ));
            }
        }
        let mut remaining = ids.clone();
        let mut complete = BTreeSet::new();
        while !remaining.is_empty() {
            let ready = self
                .units
                .iter()
                .filter(|unit| {
                    remaining.contains(&unit.id)
                        && unit
                            .depends_on
                            .iter()
                            .all(|dependency| complete.contains(dependency))
                })
                .map(|unit| unit.id.clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return Err(OrkiaError::Invalid(
                    "review plan dependencies must be acyclic".into(),
                ));
            }
            for id in ready {
                remaining.remove(&id);
                complete.insert(id);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub protected_branches: BTreeSet<String>,
    pub validation_commands: Vec<String>,
    pub minimum_coverage_milli: u16,
    pub minimum_confidence_milli: u16,
    pub required_approvals: u8,
    /// GitHub check contexts required on protected branches. The default
    /// makes `orkia integrate` enforceable outside the CLI as well.
    #[serde(default = "default_required_checks")]
    pub required_checks: BTreeSet<String>,
    #[serde(default = "default_minimum_semantic_signatures")]
    pub minimum_semantic_signatures: u8,
    #[serde(default)]
    pub authorized_grant_issuers: BTreeSet<ActorId>,
    #[serde(default)]
    pub revoked_grants: BTreeSet<Hash>,
}

const fn default_minimum_semantic_signatures() -> u8 {
    1
}

fn default_required_checks() -> BTreeSet<String> {
    BTreeSet::from(["orkia/integrate".into()])
}

const fn default_semantic_schema_version() -> u16 {
    SEMANTIC_SCHEMA_VERSION
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self {
            protected_branches: BTreeSet::from(["main".into()]),
            // Every automatic checkpoint must leave an explicit, deterministic
            // validation record in the causal ledger.  Repositories can add
            // stronger commands in `orkia.toml`; this Git-native check is the
            // safe baseline that works for every language and does not require
            // a toolchain to be installed.
            validation_commands: vec!["git diff --check".into()],
            minimum_coverage_milli: 950,
            minimum_confidence_milli: 800,
            required_approvals: 1,
            required_checks: default_required_checks(),
            minimum_semantic_signatures: default_minimum_semantic_signatures(),
            authorized_grant_issuers: BTreeSet::new(),
            revoked_grants: BTreeSet::new(),
        }
    }
}

/// Stable SHA-256 digest of the policy semantics, not its TOML formatting.
/// The resulting value is embedded in a signed `ReviewPlan` by the CLI before
/// it can influence projection or integration.
pub fn repository_policy_digest(policy: &RepositoryPolicy) -> Result<Hash> {
    let bytes = canonical_json(policy)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationResult {
    pub command: String,
    pub passed: bool,
    pub output: String,
}

/// Lifecycle of a causal unit of work.  A revision creates a new immutable
/// object; this state describes the current semantic meaning, never a mutable
/// Git branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackPullRequestStatus {
    Proposed,
    Active,
    Superseded,
    Integrated,
    Abandoned,
    BlockedConflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct StackPullRequestDependency {
    pub stack_pull_request: StackPullRequestId,
    pub repository: RepositoryId,
}

/// Immutable, signed causal work unit.  Its ID never includes a projected Git
/// commit, so rebase/restack can replace the projection without losing review
/// history or agent provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StackPullRequest {
    #[serde(default = "default_semantic_schema_version")]
    pub schema_version: u16,
    pub id: StackPullRequestId,
    pub revision: u32,
    #[serde(default)]
    pub source_plan: Option<PlanId>,
    /// Immutable revision of `source_plan` that selected these exact
    /// StackPullRequest boundaries. A reviewer correction therefore cannot make a
    /// later projection accidentally mix units from an older plan revision.
    #[serde(default)]
    pub source_plan_revision: u32,
    pub session: SessionId,
    #[serde(default)]
    pub intent: Option<SemanticObjectRef>,
    pub repository: RepositoryId,
    pub base_commit: Hash,
    #[serde(default)]
    pub parents: BTreeSet<StackPullRequestId>,
    #[serde(default)]
    pub dependencies: BTreeSet<StackPullRequestDependency>,
    pub atoms: Vec<ChangeAtom>,
    #[serde(default)]
    pub patches: Vec<PatchFragment>,
    pub evidence: BTreeSet<EventId>,
    #[serde(default)]
    pub validations: Vec<ValidationResult>,
    pub status: StackPullRequestStatus,
    #[serde(default)]
    pub supersedes: Option<SemanticObjectRef>,
}

impl SemanticDocument for StackPullRequest {
    const KIND: SemanticObjectKind = SemanticObjectKind::StackPullRequest;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.base_commit, "stack pull request base commit")?;
        if self.atoms.is_empty() || self.evidence.is_empty() {
            return Err(OrkiaError::Invalid(
                "a stack pull request needs atoms and captured causal evidence".into(),
            ));
        }
        if let Some(intent) = &self.intent {
            if intent.kind != SemanticObjectKind::Intent {
                return Err(OrkiaError::Invalid(
                    "stack pull request intent must be an intent object".into(),
                ));
            }
            validate_object_ref(intent, "stack pull request intent")?;
        }
        if let Some(previous) = &self.supersedes {
            if previous.kind != SemanticObjectKind::StackPullRequest {
                return Err(OrkiaError::Invalid(
                    "stack pull request supersedes must reference a stack pull request".into(),
                ));
            }
            validate_object_ref(previous, "stack pull request supersedes")?;
        }
        let mut atom_ids = BTreeSet::new();
        for atom in &self.atoms {
            if !atom_ids.insert(atom.id.clone()) {
                return Err(OrkiaError::Invalid(
                    "stack pull request contains duplicate atoms".into(),
                ));
            }
            if atom.source_events.is_empty()
                || !atom
                    .source_events
                    .iter()
                    .all(|event| self.evidence.contains(event))
            {
                return Err(OrkiaError::Invalid(
                    "every stack pull request atom must close over captured evidence".into(),
                ));
            }
        }
        for patch in &self.patches {
            if patch.path.is_empty()
                || patch.atoms.is_empty()
                || !patch.atoms.iter().all(|atom| atom_ids.contains(atom))
                || patch.base_content_hash.is_empty()
            {
                return Err(OrkiaError::Invalid(
                    "stack pull request patch must be closed over its atoms and base content"
                        .into(),
                ));
            }
        }
        if self.parents.contains(&self.id)
            || self
                .dependencies
                .iter()
                .any(|dependency| dependency.stack_pull_request == self.id)
        {
            return Err(OrkiaError::Invalid(
                "stack pull request cannot depend on itself".into(),
            ));
        }
        if self
            .dependencies
            .iter()
            .any(|dependency| dependency.repository == self.repository)
        {
            return Err(OrkiaError::Invalid(
                "cross-repository dependencies must reference another repository".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionStatus {
    Pending,
    Projected,
    Published,
    BlockedConflict,
    Superseded,
}

/// Replaceable Git/forge manifestation of a durable StackPullRequest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    #[serde(default = "default_semantic_schema_version")]
    pub schema_version: u16,
    pub id: ProjectionId,
    pub revision: u32,
    pub stack_pull_request: StackPullRequestId,
    /// Exact immutable StackPullRequest revision materialized by this
    /// projection. The stable ID alone is insufficient after a restack.
    #[serde(default)]
    pub stack_pull_request_revision: u32,
    pub repository: RepositoryId,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: Hash,
    #[serde(default)]
    pub commit: Option<Hash>,
    #[serde(default)]
    pub forge_pull_request: Option<String>,
    pub status: ProjectionStatus,
    #[serde(default)]
    pub supersedes: Option<SemanticObjectRef>,
}

impl SemanticDocument for Projection {
    const KIND: SemanticObjectKind = SemanticObjectKind::Projection;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        validate_hash(&self.base_commit, "projection base commit")?;
        if self.branch.is_empty() || self.base_branch.is_empty() {
            return Err(OrkiaError::Invalid(
                "projection branches cannot be empty".into(),
            ));
        }
        if let Some(commit) = &self.commit {
            validate_hash(commit, "projection commit")?;
        }
        if matches!(
            self.status,
            ProjectionStatus::Projected | ProjectionStatus::Published
        ) && self.commit.is_none()
        {
            return Err(OrkiaError::Invalid(
                "a projected projection must name its immutable Git commit".into(),
            ));
        }
        if matches!(self.status, ProjectionStatus::Published)
            && self.forge_pull_request.as_deref().is_none_or(str::is_empty)
        {
            return Err(OrkiaError::Invalid(
                "a published projection must name its forge pull request".into(),
            ));
        }
        if let Some(previous) = &self.supersedes {
            if previous.kind != SemanticObjectKind::Projection {
                return Err(OrkiaError::Invalid(
                    "projection supersedes must reference a projection".into(),
                ));
            }
            validate_object_ref(previous, "projection supersedes")?;
        }
        Ok(())
    }
}

/// Ordered PR stack inside exactly one repository. The PR records carry
/// patches and projections; this object records their Git/forge order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Stack {
    #[serde(default = "default_semantic_schema_version")]
    pub schema_version: u16,
    pub id: StackId,
    pub revision: u32,
    pub repository: RepositoryId,
    pub pull_requests: BTreeSet<StackPullRequestId>,
    /// Immutable member selection for this Stack revision. Looking up the
    /// latest StackPullRequest by ID would silently rewrite historical review
    /// meaning after an amend, so every member revision is recorded here.
    #[serde(default)]
    pub pull_request_revisions: BTreeMap<StackPullRequestId, u32>,
    #[serde(default)]
    pub roots: BTreeSet<StackPullRequestId>,
    #[serde(default)]
    pub supersedes: Option<SemanticObjectRef>,
}

impl SemanticDocument for Stack {
    const KIND: SemanticObjectKind = SemanticObjectKind::Stack;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.pull_requests.is_empty() || !self.roots.is_subset(&self.pull_requests) {
            return Err(OrkiaError::Invalid(
                "a stack needs stack PRs and roots contained in that stack".into(),
            ));
        }
        if !self.pull_request_revisions.is_empty()
            && self
                .pull_request_revisions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != self.pull_requests
        {
            return Err(OrkiaError::Invalid(
                "a stack revision must select exactly one revision for every stack pull request"
                    .into(),
            ));
        }
        if let Some(previous) = &self.supersedes {
            if previous.kind != SemanticObjectKind::Stack {
                return Err(OrkiaError::Invalid(
                    "stack supersedes must reference a stack".into(),
                ));
            }
            validate_object_ref(previous, "stack supersedes")?;
        }
        Ok(())
    }
}

/// Causal delivery spanning one or more repository-local stacks. This is the
/// multi-repository unit used for coordinated publication and integration; it
/// deliberately contains no Git patch because content remains in stack PRs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    #[serde(default = "default_semantic_schema_version")]
    pub schema_version: u16,
    pub id: ChangeSetId,
    pub revision: u32,
    pub stacks: BTreeSet<ChangeSetStack>,
    #[serde(default)]
    pub depends_on: BTreeSet<ChangeSetId>,
    pub status: StackPullRequestStatus,
    #[serde(default)]
    pub supersedes: Option<SemanticObjectRef>,
}

/// A stack as seen by a multi-repository ChangeSet. The repository identity
/// is part of the edge: a StackId alone cannot be used to schedule or verify
/// a delivery across independent Git repositories.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChangeSetStack {
    pub repository: RepositoryId,
    pub stack: StackId,
    /// The exact signed Stack revision coordinated by this ChangeSet.
    #[serde(default)]
    pub revision: u32,
}

impl SemanticDocument for ChangeSet {
    const KIND: SemanticObjectKind = SemanticObjectKind::ChangeSet;

    fn validate(&self) -> Result<()> {
        validate_schema_version(self.schema_version)?;
        if self.stacks.is_empty() || self.depends_on.contains(&self.id) {
            return Err(OrkiaError::Invalid(
                "a changeset needs stacks and cannot depend on itself".into(),
            ));
        }
        let unique_stacks = self
            .stacks
            .iter()
            .map(|reference| (reference.repository.clone(), reference.stack.clone()))
            .collect::<BTreeSet<_>>();
        if unique_stacks.len() != self.stacks.len() {
            return Err(OrkiaError::Invalid(
                "a changeset cannot select more than one revision of the same repository stack"
                    .into(),
            ));
        }
        if let Some(previous) = &self.supersedes {
            if previous.kind != SemanticObjectKind::ChangeSet {
                return Err(OrkiaError::Invalid(
                    "changeset supersedes must reference a changeset".into(),
                ));
            }
            validate_object_ref(previous, "changeset supersedes")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForgeReview {
    /// Legacy review-plan unit, retained only for importing pre-stack plans.
    #[serde(default)]
    pub unit: Option<ReviewUnitId>,
    /// Stable causal unit represented by this forge review. This identity
    /// survives rebase, restack and replacement commits.
    #[serde(default)]
    pub pull_request: Option<StackPullRequestId>,
    pub branch: String,
    pub base: String,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexRecord {
    pub event_id: EventId,
    pub event_hash: Hash,
    pub occurred_at: OffsetDateTime,
    pub event_kind: String,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OrkiaError {
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("integrity error: {0}")]
    Integrity(String),
    #[error("external error: {0}")]
    External(String),
    #[error("policy denied: {0}")]
    Policy(String),
}

pub type Result<T> = std::result::Result<T, OrkiaError>;

pub fn event_kind(event: &CaptureEvent) -> &'static str {
    match event {
        CaptureEvent::SessionStarted { .. } => "session_started",
        CaptureEvent::Prompt { .. } => "prompt",
        CaptureEvent::Transcript { .. } => "transcript",
        CaptureEvent::AgentAction { .. } => "agent_action",
        CaptureEvent::ForgeWebhook { .. } => "forge_webhook",
        CaptureEvent::ToolCall { .. } => "tool_call",
        CaptureEvent::AgentHook { .. } => "agent_hook",
        CaptureEvent::AgentTranscript { .. } => "agent_transcript",
        CaptureEvent::AgentSessionLinked { .. } => "agent_session_linked",
        CaptureEvent::AgentSessionSnapshot { .. } => "agent_session_snapshot",
        CaptureEvent::FilesObserved { .. } => "files_observed",
        CaptureEvent::Command { .. } => "command",
        CaptureEvent::Validation { .. } => "validation",
        CaptureEvent::Checkpoint { .. } => "checkpoint",
        CaptureEvent::ReviewPlanCreated { .. } => "review_plan_created",
        CaptureEvent::ReviewPlanRevised { .. } => "review_plan_revised",
        CaptureEvent::ReviewPlanApproved { .. } => "review_plan_approved",
        CaptureEvent::ReviewPlanChangesRequested { .. } => "review_plan_changes_requested",
        CaptureEvent::StackPullRequestPublished { .. } => "stack_pull_request_published",
        CaptureEvent::ChangeSetPublished { .. } => "changeset_published",
        CaptureEvent::ChangeSetIntegrated { .. } => "changeset_integrated",
        CaptureEvent::ProjectionUpdated { .. } => "projection_updated",
        CaptureEvent::IntegrationEvaluated { .. } => "integration_evaluated",
        CaptureEvent::SessionClosed { .. } => "session_closed",
    }
}

pub type Metadata = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        assert_ne!(SessionId::new(), SessionId::new());
    }

    #[test]
    fn semantic_object_kinds_have_stable_ref_segments() {
        assert_eq!(SemanticObjectKind::Operation.ref_segment(), "operation");
        assert_eq!(SemanticObjectKind::Attestation.ref_segment(), "attestation");
    }

    #[test]
    fn stable_atom_ids_are_reproducible_and_unambiguous() {
        let first = AtomId::from_stable_parts(&[b"ab", b"c"]);
        let repeat = AtomId::from_stable_parts(&[b"ab", b"c"]);
        let different_boundaries = AtomId::from_stable_parts(&[b"a", b"bc"]);
        assert_eq!(first, repeat);
        assert_ne!(first, different_boundaries);
    }

    #[test]
    fn canonical_json_sorts_nested_object_keys() {
        let first = serde_json::json!({"z": {"b": 2, "a": 1}, "a": true});
        let second = serde_json::json!({"a": true, "z": {"a": 1, "b": 2}});
        assert_eq!(
            canonical_json(&first).unwrap(),
            canonical_json(&second).unwrap()
        );
        assert_eq!(
            canonical_json(&first).unwrap(),
            br#"{"a":true,"z":{"a":1,"b":2}}"#.to_vec()
        );
    }

    #[test]
    fn semantic_state_rejects_unknown_schema_and_non_operation_closure() {
        let mut state = SemanticState {
            schema_version: SEMANTIC_SCHEMA_VERSION + 1,
            commit: "commit".into(),
            parent_commits: BTreeSet::new(),
            files: BTreeMap::new(),
            operations: BTreeSet::new(),
            trunks: BTreeMap::new(),
        };
        assert!(state.validate().is_err());

        state.schema_version = SEMANTIC_SCHEMA_VERSION;
        state.operations.insert(SemanticObjectRef {
            kind: SemanticObjectKind::View,
            hash: "object".into(),
        });
        assert!(state.validate().is_err());
    }

    #[test]
    fn review_plan_rejects_unknown_schema() {
        let plan = ReviewPlan {
            schema_version: SEMANTIC_SCHEMA_VERSION + 1,
            id: PlanId::new(),
            revision: 0,
            source_checkpoint: "checkpoint".into(),
            policy_digest: None,
            units: vec![ReviewUnit {
                id: ReviewUnitId::new(),
                title: "unit".into(),
                atoms: BTreeSet::from([AtomId::new()]),
                depends_on: BTreeSet::new(),
                confidence_milli: 1000,
            }],
            atoms: Vec::new(),
            atom_paths: BTreeMap::new(),
            coverage_milli: 1000,
            status: PlanStatus::Proposed,
            created_from: BTreeSet::new(),
        };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn review_plan_rejects_atom_gaps_and_dependency_cycles() {
        let first = ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("first".into()),
            start_line: 1,
            end_line: 1,
            content_hash: "first".into(),
            source_events: BTreeSet::new(),
        };
        let second = ChangeAtom {
            id: AtomId::new(),
            kind: AtomKind::Symbol,
            path: "src/lib.rs".into(),
            symbol: Some("second".into()),
            start_line: 2,
            end_line: 2,
            content_hash: "second".into(),
            source_events: BTreeSet::new(),
        };
        let first_unit = ReviewUnitId::new();
        let second_unit = ReviewUnitId::new();
        let plan = ReviewPlan {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: PlanId::new(),
            revision: 0,
            source_checkpoint: "checkpoint".into(),
            policy_digest: None,
            units: vec![
                ReviewUnit {
                    id: first_unit.clone(),
                    title: "first".into(),
                    atoms: BTreeSet::from([first.id.clone()]),
                    depends_on: BTreeSet::from([second_unit.clone()]),
                    confidence_milli: 1000,
                },
                ReviewUnit {
                    id: second_unit,
                    title: "second".into(),
                    atoms: BTreeSet::from([second.id.clone()]),
                    depends_on: BTreeSet::from([first_unit]),
                    confidence_milli: 1000,
                },
            ],
            atoms: vec![first, second],
            atom_paths: BTreeMap::new(),
            coverage_milli: 1000,
            status: PlanStatus::Proposed,
            created_from: BTreeSet::new(),
        };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn policy_digest_is_canonical_and_changes_with_policy_semantics() {
        let policy = RepositoryPolicy::default();
        let first = repository_policy_digest(&policy).unwrap();
        assert_eq!(first, repository_policy_digest(&policy).unwrap());
        let mut changed = policy;
        changed.required_approvals = 2;
        assert_ne!(first, repository_policy_digest(&changed).unwrap());
    }

    #[test]
    fn published_projection_requires_commit_and_forge_identity() {
        let projection = Projection {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            id: ProjectionId::new(),
            revision: 0,
            stack_pull_request: StackPullRequestId::new(),
            stack_pull_request_revision: 0,
            repository: RepositoryId::new(),
            branch: "orkia/stack-pr/example".into(),
            base_branch: "main".into(),
            base_commit: "base".into(),
            commit: None,
            forge_pull_request: None,
            status: ProjectionStatus::Published,
            supersedes: None,
        };
        assert!(projection.validate().is_err());
    }
}
