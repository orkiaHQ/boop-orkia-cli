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
    pub id: PlanId,
    pub revision: u32,
    pub source_checkpoint: String,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub protected_branches: BTreeSet<String>,
    pub validation_commands: Vec<String>,
    pub minimum_coverage_milli: u16,
    pub minimum_confidence_milli: u16,
    pub required_approvals: u8,
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self {
            protected_branches: BTreeSet::from(["main".into()]),
            validation_commands: Vec::new(),
            minimum_coverage_milli: 950,
            minimum_confidence_milli: 800,
            required_approvals: 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationResult {
    pub command: String,
    pub passed: bool,
    pub output: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForgeReview {
    pub unit: ReviewUnitId,
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
        CaptureEvent::ToolCall { .. } => "tool_call",
        CaptureEvent::AgentHook { .. } => "agent_hook",
        CaptureEvent::AgentTranscript { .. } => "agent_transcript",
        CaptureEvent::AgentSessionLinked { .. } => "agent_session_linked",
        CaptureEvent::AgentSessionSnapshot { .. } => "agent_session_snapshot",
        CaptureEvent::AgentAction { .. } => "agent_action",
        CaptureEvent::FilesObserved { .. } => "files_observed",
        CaptureEvent::Command { .. } => "command",
        CaptureEvent::Validation { .. } => "validation",
        CaptureEvent::Checkpoint { .. } => "checkpoint",
        CaptureEvent::ReviewPlanCreated { .. } => "review_plan_created",
        CaptureEvent::ReviewPlanRevised { .. } => "review_plan_revised",
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
}
