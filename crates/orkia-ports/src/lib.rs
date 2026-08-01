//! Inward-facing contracts. Implementations belong to infrastructure crates.

use orkia_model::{ForgeReview, LedgerEvent, RepositoryPolicy, Result, ReviewPlan, ValidationResult};

pub trait LedgerStore: Send + Sync {
    fn append(&self, event: &LedgerEvent) -> Result<()>;
    fn read_all(&self) -> Result<Vec<LedgerEvent>>;
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

pub trait Clock: Send + Sync { fn now(&self) -> time::OffsetDateTime; }

pub trait ValidationExecutor: Send + Sync {
    fn execute(&self, policy: &RepositoryPolicy) -> Result<Vec<ValidationResult>>;
}

pub trait PlanStore: Send + Sync {
    fn save(&self, plan: &ReviewPlan) -> Result<()>;
    fn load(&self, id: &orkia_model::PlanId) -> Result<Option<ReviewPlan>>;
}
