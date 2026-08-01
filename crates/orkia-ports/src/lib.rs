//! Inward-facing contracts. Implementations belong to infrastructure crates.

use orkia_model::{
    AccessGrant, Attestation, ForgeReview, GrantRevocation, Intent, KeyRotation, LedgerEvent,
    Memory, MergeResolution, Organization, RepositoryPolicy, Result, ReviewPlan,
    SemanticObjectKind, SemanticObjectRef, SemanticOperation, SemanticSignature, SemanticState,
    Team, ValidationResult, VaultEntry, ViewMetadata,
};

pub trait LedgerStore: Send + Sync {
    fn append(&self, event: &LedgerEvent) -> Result<()>;
    fn read_all(&self) -> Result<Vec<LedgerEvent>>;
}

/// Immutable semantic objects and their association with Git commits.
///
/// Git remains responsible for object storage and transport. Implementations
/// expose only the small semantic namespace that Orkia adds on top of it.
pub trait SemanticObjectStore: Send + Sync {
    fn put(&self, kind: SemanticObjectKind, bytes: &[u8]) -> Result<SemanticObjectRef>;
    fn get(&self, object: &SemanticObjectRef) -> Result<Vec<u8>>;
    fn state_for_commit(&self, commit: &str) -> Result<Option<SemanticObjectRef>>;
}

/// Typed access to the versioned documents in Orkia's Git semantic overlay.
///
/// Implementations must validate a document and all references required to
/// activate it before returning it to callers.
pub trait SemanticDocumentStore: SemanticObjectStore {
    fn put_operation(&self, operation: &SemanticOperation) -> Result<SemanticObjectRef>;
    fn get_operation(&self, object: &SemanticObjectRef) -> Result<SemanticOperation>;
    fn put_state(&self, state: &SemanticState) -> Result<SemanticObjectRef>;
    fn get_state(&self, object: &SemanticObjectRef) -> Result<SemanticState>;
    fn put_view(&self, view: &ViewMetadata) -> Result<SemanticObjectRef>;
    fn get_view(&self, object: &SemanticObjectRef) -> Result<ViewMetadata>;
    fn put_merge_resolution(&self, resolution: &MergeResolution) -> Result<SemanticObjectRef>;
    fn get_merge_resolution(&self, object: &SemanticObjectRef) -> Result<MergeResolution>;
    fn put_attestation(&self, attestation: &Attestation) -> Result<SemanticObjectRef>;
    fn get_attestation(&self, object: &SemanticObjectRef) -> Result<Attestation>;
    fn put_access_grant(&self, grant: &AccessGrant) -> Result<SemanticObjectRef>;
    fn get_access_grant(&self, object: &SemanticObjectRef) -> Result<AccessGrant>;
    fn put_grant_revocation(&self, revocation: &GrantRevocation) -> Result<SemanticObjectRef>;
    fn get_grant_revocation(&self, object: &SemanticObjectRef) -> Result<GrantRevocation>;
    fn put_vault_entry(&self, entry: &VaultEntry) -> Result<SemanticObjectRef>;
    fn get_vault_entry(&self, object: &SemanticObjectRef) -> Result<VaultEntry>;
    fn put_review_plan(&self, plan: &ReviewPlan) -> Result<SemanticObjectRef>;
    fn get_review_plan(&self, object: &SemanticObjectRef) -> Result<ReviewPlan>;
    fn put_organization(&self, organization: &Organization) -> Result<SemanticObjectRef>;
    fn get_organization(&self, object: &SemanticObjectRef) -> Result<Organization>;
    fn put_team(&self, team: &Team) -> Result<SemanticObjectRef>;
    fn get_team(&self, object: &SemanticObjectRef) -> Result<Team>;
    fn put_intent(&self, intent: &Intent) -> Result<SemanticObjectRef>;
    fn get_intent(&self, object: &SemanticObjectRef) -> Result<Intent>;
    fn put_memory(&self, memory: &Memory) -> Result<SemanticObjectRef>;
    fn get_memory(&self, object: &SemanticObjectRef) -> Result<Memory>;
    fn put_key_rotation(&self, rotation: &KeyRotation) -> Result<SemanticObjectRef>;
    fn get_key_rotation(&self, object: &SemanticObjectRef) -> Result<KeyRotation>;
    fn put_signature(&self, signature: &SemanticSignature) -> Result<SemanticObjectRef>;
    fn get_signature(&self, object: &SemanticObjectRef) -> Result<SemanticSignature>;
    fn activate_state(
        &self,
        commit: &str,
        state: &SemanticObjectRef,
        policy: &RepositoryPolicy,
    ) -> Result<()>;
}

pub trait GitRepository: Send + Sync {
    fn head_commit(&self) -> Result<String>;
    fn create_isolated_worktree(&self, name: &str, path: &std::path::Path) -> Result<()>;
    fn write_ledger(&self, bytes: &[u8]) -> Result<()>;
    fn read_ledger(&self) -> Result<Option<Vec<u8>>>;
}

pub trait Forge: Send + Sync {
    fn publish(&self, review: &ForgeReview) -> Result<String>;
    fn set_required_checks(&self, branch: &str, checks: &[String]) -> Result<()>;
}

pub trait ReviewIndex: Send + Sync {
    fn rebuild(&self, events: &[LedgerEvent]) -> Result<()>;
    fn search(&self, query: &str) -> Result<Vec<orkia_model::IndexRecord>>;
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &str, value: &[u8]) -> Result<()>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> time::OffsetDateTime;
}

pub trait ValidationExecutor: Send + Sync {
    fn execute(&self, policy: &RepositoryPolicy) -> Result<Vec<ValidationResult>>;
}

pub trait PlanStore: Send + Sync {
    fn save(&self, plan: &ReviewPlan) -> Result<()>;
    fn load(&self, id: &orkia_model::PlanId) -> Result<Option<ReviewPlan>>;
}
