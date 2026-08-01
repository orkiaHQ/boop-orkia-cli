//! Pure, serializable domain types for Orkia.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub type Hash = String;
pub type CanonicalJson = Vec<u8>;

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
    };
}

id!(ActorId);
id!(SessionId);
id!(EventId);
id!(AtomId);
id!(ReviewUnitId);
id!(PlanId);
id!(RepositoryId);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub id: ActorId,
    pub display_name: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureOrigin { Human, Codex, Claude, Unknown }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureEvent {
    SessionStarted { session: SessionId, origin: CaptureOrigin, base_commit: String, objective: String },
    Prompt { provider: String, content: String },
    Transcript { provider: String, content: String },
    ToolCall { tool: String, arguments: serde_json::Value, result: serde_json::Value },
    FilesObserved { read: BTreeSet<String>, modified: BTreeSet<String>, unknown_write: bool },
    Command { command: String, exit_code: Option<i32>, stdout: String, stderr: String },
    Validation { command: String, passed: bool, output: String },
    Checkpoint { commit: String },
    ReviewPlanRevised { plan: PlanId, revision: u32, reason: String },
    SessionClosed { session: SessionId },
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
pub enum AtomKind { Symbol, Block, Hunk, Import, Test, Configuration, Migration }

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DependencyKind { Hard, Causal, Import, Test, Configuration }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AtomDependency { pub from: AtomId, pub to: AtomId, pub kind: DependencyKind, pub confidence_milli: u16 }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewUnit {
    pub id: ReviewUnitId,
    pub title: String,
    pub atoms: BTreeSet<AtomId>,
    pub depends_on: BTreeSet<ReviewUnitId>,
    pub confidence_milli: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PlanStatus { Proposed, Approved, ChangesRequested, Superseded }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReviewPlan {
    pub id: PlanId,
    pub revision: u32,
    pub source_checkpoint: String,
    pub units: Vec<ReviewUnit>,
    pub coverage_milli: u16,
    pub status: PlanStatus,
    pub created_from: BTreeSet<EventId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub protected_branches: BTreeSet<String>,
    pub validation_commands: Vec<String>,
    pub minimum_coverage_milli: u16,
    pub minimum_confidence_milli: u16,
    pub required_approvals: u8,
}

impl Default for RepositoryPolicy {
    fn default() -> Self { Self { protected_branches: BTreeSet::from(["main".into()]), validation_commands: Vec::new(), minimum_coverage_milli: 950, minimum_confidence_milli: 800, required_approvals: 1 } }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationResult { pub command: String, pub passed: bool, pub output: String }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForgeReview { pub unit: ReviewUnitId, pub branch: String, pub base: String, pub title: String, pub body: String }

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexRecord { pub event_id: EventId, pub event_hash: Hash, pub occurred_at: OffsetDateTime, pub event_kind: String }

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OrkiaError {
    #[error("invalid data: {0}")] Invalid(String),
    #[error("not found: {0}")] NotFound(String),
    #[error("conflict: {0}")] Conflict(String),
    #[error("integrity error: {0}")] Integrity(String),
    #[error("external error: {0}")] External(String),
    #[error("policy denied: {0}")] Policy(String),
}

pub type Result<T> = std::result::Result<T, OrkiaError>;

pub fn event_kind(event: &CaptureEvent) -> &'static str {
    match event {
        CaptureEvent::SessionStarted { .. } => "session_started", CaptureEvent::Prompt { .. } => "prompt",
        CaptureEvent::Transcript { .. } => "transcript", CaptureEvent::ToolCall { .. } => "tool_call",
        CaptureEvent::FilesObserved { .. } => "files_observed", CaptureEvent::Command { .. } => "command",
        CaptureEvent::Validation { .. } => "validation", CaptureEvent::Checkpoint { .. } => "checkpoint",
        CaptureEvent::ReviewPlanRevised { .. } => "review_plan_revised", CaptureEvent::SessionClosed { .. } => "session_closed",
    }
}

pub type Metadata = BTreeMap<String, String>;

#[cfg(test)]
mod tests { use super::*; #[test] fn ids_are_unique() { assert_ne!(SessionId::new(), SessionId::new()); } }
