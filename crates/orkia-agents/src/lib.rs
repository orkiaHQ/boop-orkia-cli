//! Native integrations for the coding agents supported by Riftr CLI.
//!
//! The support matrix deliberately mirrors Riftr: transcript discovery for
//! Claude Code, Codex, Kimi, OpenCode and Cursor; live hooks for Claude Code,
//! Codex, Gemini, Factory Droid and Qwen Code.  An adapter is explicit about
//! which side it supports: no unmeasured hook is installed for an import-only
//! agent, and no transcript reader is fabricated for a hook-only agent.

use orkia_model::{AgentActionKind, CaptureEvent};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub const MARKER: &str = "# orkia hook";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    Gemini,
    Kimi,
    #[serde(rename = "opencode", alias = "open-code")]
    OpenCode,
    Cursor,
    Droid,
    Qwen,
}

impl Agent {
    pub const ALL: [Self; 8] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Gemini,
        Self::Kimi,
        Self::OpenCode,
        Self::Cursor,
        Self::Droid,
        Self::Qwen,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Kimi => "kimi",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
            Self::Droid => "droid",
            Self::Qwen => "qwen",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        serde_json::from_value(Value::String(name.to_owned())).ok()
    }

    pub fn supports_transcripts(self) -> bool {
        matches!(
            self,
            Self::ClaudeCode | Self::Codex | Self::Kimi | Self::OpenCode | Self::Cursor
        )
    }

    pub fn supports_hooks(self) -> bool {
        matches!(
            self,
            Self::ClaudeCode | Self::Codex | Self::Gemini | Self::Droid | Self::Qwen
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Change {
    pub paths: Vec<PathBuf>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Status {
    pub agent: Agent,
    pub present: bool,
    pub transcript_root: Option<PathBuf>,
    pub hooks_path: Option<PathBuf>,
    pub wired_to: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub event: String,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    pub raw: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptFile {
    pub path: PathBuf,
    pub binary: bool,
}

/// Immutable source revision used for ledger storage and normalization. Some
/// providers split one session over several files; `content` is the complete
/// captured source manifest while `normalization_content` remains the native
/// primary document expected by its parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptSnapshot {
    pub encoding: String,
    pub content: String,
    pub normalization_content: String,
}

/// Result of comparing a new on-disk transcript revision with the revision
/// already preserved in the ledger.  A changed source is never replayed in
/// full: Orkia either appends the proven suffix or records the raw revision
/// without inventing a causal delta.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptReconciliation {
    Unchanged,
    Append(Vec<CaptureEvent>),
    Unreconciled,
}

/// Locate precisely the transcript file shapes Riftr imports. Parents are
/// visited before descendants, preventing a Claude subagent transcript from
/// claiming its parent session first.
pub fn transcript_files(agent: Agent) -> Result<Vec<TranscriptFile>, String> {
    if !agent.supports_transcripts() {
        return Err(format!("{} has live hooks only", agent.name()));
    }
    transcript_files_at(agent, &transcript_root(agent)?)
}

/// Discover transcript documents under an explicit source root.  The CLI uses
/// this for deterministic imports and tests; normal operation keeps using the
/// provider's conventional transcript root.
pub fn transcript_files_at(agent: Agent, root: &Path) -> Result<Vec<TranscriptFile>, String> {
    if !agent.supports_transcripts() {
        return Err(format!("{} has live hooks only", agent.name()));
    }
    let mut found = Vec::new();
    collect_transcript_files(root, agent, 0, &mut found);
    Ok(found)
}

/// Read a source revision without silently omitting OpenCode's message and
/// part sidecars. Cursor's SQLite database is an opaque binary revision, so it
/// is kept as base64 and later reconciled fail-closed.
pub fn transcript_snapshot(
    agent: Agent,
    file: &TranscriptFile,
) -> Result<TranscriptSnapshot, String> {
    let bytes =
        std::fs::read(&file.path).map_err(|error| format!("{}: {error}", file.path.display()))?;
    if file.binary {
        return Ok(TranscriptSnapshot {
            encoding: "base64".into(),
            content: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            normalization_content: String::new(),
        });
    }
    let primary = String::from_utf8_lossy(&bytes).into_owned();
    let content = if agent == Agent::OpenCode {
        opencode_source_manifest(&file.path, &primary)?
    } else {
        primary.clone()
    };
    Ok(TranscriptSnapshot {
        encoding: "utf-8".into(),
        content,
        normalization_content: primary,
    })
}

fn opencode_source_manifest(path: &Path, primary: &str) -> Result<String, String> {
    let Some(storage) = path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return Ok(primary.to_owned());
    };
    let Some(session) = serde_json::from_str::<Value>(primary)
        .ok()
        .and_then(|document| {
            document
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    else {
        return Ok(primary.to_owned());
    };
    let mut paths = Vec::new();
    collect_json_files(&storage.join("message").join(&session), &mut paths);
    let mut message_ids = paths
        .iter()
        .filter_map(|path| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    message_ids.sort();
    message_ids.dedup();
    for message in message_ids {
        collect_json_files(&storage.join("part").join(message), &mut paths);
    }
    paths.sort();
    let sidecars = paths
        .into_iter()
        .filter_map(|path| {
            std::fs::read_to_string(&path).ok().map(|content| {
                json!({
                    "path": path.strip_prefix(storage).unwrap_or(&path).to_string_lossy(),
                    "content": content,
                })
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({"session": primary, "sidecars": sidecars}))
        .map_err(|error| error.to_string())
}

fn collect_json_files(root: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            found.push(path);
        }
    }
}

fn collect_transcript_files(
    root: &Path,
    agent: Agent,
    depth: usize,
    found: &mut Vec<TranscriptFile>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let (mut directories, mut files) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        } else {
            files.push(path);
        }
    }
    files.sort();
    directories.sort();
    for path in files {
        if transcript_file(agent, &path) {
            found.push(TranscriptFile {
                binary: agent == Agent::Cursor,
                path,
            });
        }
    }
    let maximum_depth = if agent == Agent::ClaudeCode {
        Some(2)
    } else {
        None
    };
    for directory in directories {
        if maximum_depth.is_none_or(|maximum| depth + 1 < maximum) {
            collect_transcript_files(&directory, agent, depth + 1, found);
        }
    }
}

fn transcript_file(agent: Agent, path: &Path) -> bool {
    match agent {
        Agent::ClaudeCode => path
            .extension()
            .is_some_and(|extension| extension == "jsonl"),
        Agent::Codex => {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
        }
        Agent::Kimi => path.file_name().is_some_and(|name| name == "wire.jsonl"),
        Agent::OpenCode => {
            path.extension()
                .is_some_and(|extension| extension == "json")
                && path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "session")
        }
        Agent::Cursor => path.file_name().is_some_and(|name| name == "state.vscdb"),
        _ => false,
    }
}

/// Parse only the invariant envelope of a measured hook payload.  The entire
/// input stays in `raw`; this never loses fields when an agent extends it.
pub fn parse_hook_payload(agent: Agent, raw: &str) -> Result<HookPayload, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("invalid {} hook payload: {error}", agent.name()))?;
    let event = value
        .get("hook_event_name")
        .or_else(|| value.get("hookEventName"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let event = match (agent, event) {
        (Agent::Gemini, "BeforeAgent") => "UserPromptSubmit",
        (Agent::Gemini, "AfterTool") => "PostToolUse",
        (Agent::Gemini, "AfterAgent") => "Stop",
        _ => event,
    }
    .to_owned();
    Ok(HookPayload {
        session_id: value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: value.get("cwd").and_then(Value::as_str).map(PathBuf::from),
        prompt: value
            .get("prompt")
            .or_else(|| value.get("user_prompt"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        event,
        raw: value,
    })
}

/// Interpret the facts a live hook exposes without discarding the unmodified
/// payload.  All adapters use the same action vocabulary; fields an agent does
/// not expose remain `None` rather than being inferred.
pub fn normalize_hook(agent: Agent, payload: &HookPayload) -> Vec<CaptureEvent> {
    let session = payload.session_id.clone();
    let mut events = Vec::new();
    if payload.event == "UserPromptSubmit" {
        if let Some(content) = payload.prompt.clone() {
            events.push(agent_action(
                agent,
                session.clone(),
                AgentActionKind::Prompt { content },
            ));
        }
    }
    if payload.event == "PostToolUse" || payload.event == "PostToolUseFailure" {
        let patch_actions = codex_apply_patch_actions(agent, &payload.raw);
        if !patch_actions.is_empty() {
            events.extend(
                patch_actions
                    .into_iter()
                    .map(|action| agent_action(agent, session.clone(), action)),
            );
        } else if let Some(action) = tool_action(&payload.raw) {
            events.push(agent_action(agent, session, action));
        }
    }
    events
}

/// Codex 0.146 delivers `apply_patch` as `tool_input.command`.  It is not a
/// shell command: it is the textual apply_patch protocol.  Each file header is
/// an independently causally useful write, so one tool call can yield several
/// actions without pretending to know whole-file before/after hashes.
fn codex_apply_patch_actions(agent: Agent, value: &Value) -> Vec<AgentActionKind> {
    if agent != Agent::Codex
        || value.get("tool_name").and_then(Value::as_str) != Some("apply_patch")
    {
        return Vec::new();
    }
    let Some(patch) = value
        .pointer("/tool_input/command")
        .or_else(|| value.pointer("/tool_input/input"))
        .or_else(|| value.pointer("/tool_input/patch"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    patch_file_actions(patch)
}

fn patch_file_actions(patch: &str) -> Vec<AgentActionKind> {
    let mut files = Vec::<(String, u32, u32)>::new();
    let mut current: Option<(String, u32, u32)> = None;
    for line in patch.lines() {
        let header = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(path) = header {
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some((path.to_owned(), 0, 0));
            continue;
        }
        let Some((_, added, removed)) = current.as_mut() else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            *added = added.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            *removed = removed.saturating_add(1);
        }
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
        .into_iter()
        .map(|(path, added, removed)| AgentActionKind::FileWrite {
            path,
            before_hash: None,
            after_hash: None,
            added_lines: (added > 0).then_some(added),
            removed_lines: (removed > 0).then_some(removed),
        })
        .collect()
}

/// Decode a transcript source into provider-neutral actions.  The caller must
/// append the original document too: a foreign format can evolve, while these
/// actions are the deliberately conservative facts Orkia can use for review.
pub fn normalize_transcript(
    agent: Agent,
    path: &Path,
    encoding: &str,
    content: &str,
) -> Result<Vec<CaptureEvent>, String> {
    if agent == Agent::Cursor {
        return normalize_cursor(path);
    }
    if encoding != "utf-8" {
        return Ok(Vec::new());
    }
    match agent {
        Agent::Kimi => normalize_kimi(path, content),
        Agent::OpenCode => normalize_opencode(path, content),
        Agent::ClaudeCode | Agent::Codex => Ok(normalize_jsonl(agent, path, content)),
        Agent::Gemini | Agent::Cursor | Agent::Droid | Agent::Qwen => Ok(Vec::new()),
    }
}

/// Reconcile an append-only transcript revision without duplicating its prior
/// causal actions.  Claude Code, Codex, and Kimi expose line-oriented source
/// documents for which a byte-prefix plus a normalized-action prefix is
/// required.  The remaining formats have companion records or database state,
/// so a changed primary document deliberately fails closed for now.
pub fn reconcile_transcript(
    agent: Agent,
    path: &Path,
    encoding: &str,
    previous: Option<&str>,
    revision: &str,
    normalization_content: &str,
) -> Result<TranscriptReconciliation, String> {
    let Some(previous) = previous else {
        return normalize_transcript(agent, path, encoding, normalization_content)
            .map(TranscriptReconciliation::Append);
    };
    if previous == revision {
        return Ok(TranscriptReconciliation::Unchanged);
    }
    if !matches!(agent, Agent::ClaudeCode | Agent::Codex | Agent::Kimi)
        || encoding != "utf-8"
        || !revision.starts_with(previous)
    {
        return Ok(TranscriptReconciliation::Unreconciled);
    }
    let prior_actions = normalize_transcript(agent, path, encoding, previous)?;
    let current_actions = normalize_transcript(agent, path, encoding, normalization_content)?;
    if current_actions.len() < prior_actions.len()
        || current_actions[..prior_actions.len()] != prior_actions
    {
        return Ok(TranscriptReconciliation::Unreconciled);
    }
    Ok(TranscriptReconciliation::Append(
        current_actions[prior_actions.len()..].to_vec(),
    ))
}

fn normalize_jsonl(agent: Agent, path: &Path, content: &str) -> Vec<CaptureEvent> {
    let mut session = path_session(path);
    let mut events = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(found) = session_of(&value) {
            session = Some(found);
        }
        if let Some(prompt) = prompt_of(&value) {
            events.push(agent_action(
                agent,
                session.clone(),
                AgentActionKind::Prompt { content: prompt },
            ));
        }
        events.extend(content_tool_actions(agent, session.clone(), &value));
        if let Some(action) = direct_tool_action(&value) {
            events.push(agent_action(agent, session.clone(), action));
        }
        if let Some(turn) = turn_of(&value) {
            events.push(agent_action(agent, session.clone(), turn));
        }
    }
    events
}

fn normalize_kimi(path: &Path, content: &str) -> Result<Vec<CaptureEvent>, String> {
    let session = path_session(path);
    let mut events = Vec::new();
    let mut pending = HashMap::<String, Value>::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("turn.prompt") => {
                if let Some(content) = blocks_text(value.get("input"), "text") {
                    events.push(agent_action(
                        Agent::Kimi,
                        session.clone(),
                        AgentActionKind::Prompt { content },
                    ));
                }
            }
            Some("context.append_loop_event") => {
                let Some(event) = value.get("event") else {
                    continue;
                };
                match event.get("type").and_then(Value::as_str) {
                    Some("tool.call") => {
                        if let Some(id) = event.get("toolCallId").and_then(Value::as_str) {
                            pending.insert(id.to_owned(), event.clone());
                        }
                    }
                    Some("tool.result") => {
                        let Some(id) = event.get("toolCallId").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(mut call) = pending.remove(id) else {
                            continue;
                        };
                        let result = event.pointer("/result/output").cloned();
                        if let Some(object) = call.as_object_mut() {
                            object.insert("result".into(), result.unwrap_or(Value::Null));
                        }
                        if let Some(action) = kimi_tool_action(&call) {
                            events.push(agent_action(Agent::Kimi, session.clone(), action));
                        }
                    }
                    Some("step.end") => {
                        if let Some(turn) =
                            kimi_turn(event, value.get("model").and_then(Value::as_str))
                        {
                            events.push(agent_action(Agent::Kimi, session.clone(), turn));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for call in pending.into_values() {
        if let Some(action) = kimi_tool_action(&call) {
            events.push(agent_action(Agent::Kimi, session.clone(), action));
        }
    }
    Ok(events)
}

fn normalize_opencode(path: &Path, content: &str) -> Result<Vec<CaptureEvent>, String> {
    let session_doc: Value =
        serde_json::from_str(content).map_err(|error| format!("{}: {error}", path.display()))?;
    let session = session_doc
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| path_session(path));
    let Some(storage) = path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return Ok(Vec::new());
    };
    let Some(id) = session.clone() else {
        return Ok(Vec::new());
    };
    let mut message_paths = entries(&storage.join("message").join(&id));
    message_paths.sort();
    let mut events = Vec::new();
    for message_path in message_paths {
        let Ok(message_text) = std::fs::read_to_string(&message_path) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_text) else {
            continue;
        };
        let message_id = message_path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let mut part_paths = entries(&storage.join("part").join(message_id));
        part_paths.sort();
        let parts: Vec<Value> = part_paths
            .into_iter()
            .filter_map(|part| std::fs::read_to_string(part).ok())
            .filter_map(|part| serde_json::from_str(&part).ok())
            .collect();
        if message.get("role").and_then(Value::as_str) == Some("user") {
            let text = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                events.push(agent_action(
                    Agent::OpenCode,
                    Some(id.clone()),
                    AgentActionKind::Prompt { content: text },
                ));
            }
        }
        for part in &parts {
            if part.get("type").and_then(Value::as_str) == Some("tool") {
                if let Some(action) = opencode_tool_action(part) {
                    events.push(agent_action(Agent::OpenCode, Some(id.clone()), action));
                }
            }
        }
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(turn) = opencode_turn(&message, &parts) {
                events.push(agent_action(Agent::OpenCode, Some(id.clone()), turn));
            }
        }
    }
    Ok(events)
}

fn normalize_cursor(path: &Path) -> Result<Vec<CaptureEvent>, String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let composers = cursor_rows(&connection, "composerData:%")?;
    let mut events = Vec::new();
    for composer in composers {
        let Some(id) = composer.get("composerId").and_then(Value::as_str) else {
            continue;
        };
        let id = id.to_owned();
        for bubble in cursor_rows(&connection, &format!("bubbleId:{id}:%"))? {
            if let Some(action) = cursor_action(&bubble) {
                events.push(agent_action(Agent::Cursor, Some(id.clone()), action));
            }
        }
    }
    Ok(events)
}

fn cursor_rows(connection: &Connection, prefix: &str) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare("SELECT value FROM cursorDiskKV WHERE key LIKE ?1 ORDER BY key")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![prefix], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    Ok(rows
        .filter_map(|row| row.ok())
        .filter_map(|value| serde_json::from_str(&value).ok())
        .collect())
}

fn agent_action(
    agent: Agent,
    external_session: Option<String>,
    action: AgentActionKind,
) -> CaptureEvent {
    CaptureEvent::AgentAction {
        agent: agent.name().into(),
        external_session,
        session: None,
        action,
    }
}

fn content_tool_actions(agent: Agent, session: Option<String>, value: &Value) -> Vec<CaptureEvent> {
    let source = value.get("payload").unwrap_or(value);
    source
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(direct_tool_action)
        .map(|action| agent_action(agent, session.clone(), action))
        .collect()
}

fn direct_tool_action(value: &Value) -> Option<AgentActionKind> {
    let nested = value.get("payload").unwrap_or(value);
    let kind = nested.get("type").and_then(Value::as_str);
    let candidate = match kind {
        Some("function_call" | "custom_tool_call" | "tool_use") => nested,
        _ if nested.get("tool_name").is_some() || nested.get("toolName").is_some() => nested,
        _ => return None,
    };
    tool_action(candidate)
}

fn tool_action(value: &Value) -> Option<AgentActionKind> {
    let name = value
        .get("tool_name")
        .or_else(|| value.get("toolName"))
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)?
        .to_owned();
    let arguments = value
        .get("tool_input")
        .or_else(|| value.get("input"))
        .or_else(|| value.get("args"))
        .or_else(|| value.get("arguments"))
        .cloned()
        .map(parse_json_string)
        .unwrap_or(Value::Null);
    let result = value
        .get("tool_response")
        .or_else(|| value.get("result"))
        .or_else(|| value.get("output"))
        .cloned();
    action_for_tool(
        &name,
        value
            .get("tool_use_id")
            .or_else(|| value.get("toolUseId"))
            .or_else(|| value.get("call_id"))
            .or_else(|| value.get("callID"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        arguments,
        result,
        value.get("duration_ms").and_then(Value::as_u64),
    )
}

fn kimi_tool_action(value: &Value) -> Option<AgentActionKind> {
    let name = value.get("name").and_then(Value::as_str)?;
    let arguments = value.get("args").cloned().unwrap_or(Value::Null);
    let result = value
        .get("result")
        .cloned()
        .filter(|value| !value.is_null());
    action_for_tool(
        name,
        value
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        arguments,
        result,
        None,
    )
}

fn opencode_tool_action(value: &Value) -> Option<AgentActionKind> {
    let name = value.get("tool").and_then(Value::as_str)?;
    let state = value.get("state").unwrap_or(&Value::Null);
    action_for_tool(
        name,
        value
            .get("callID")
            .and_then(Value::as_str)
            .map(str::to_owned),
        state.get("input").cloned().unwrap_or(Value::Null),
        state.get("output").cloned(),
        None,
    )
}

fn action_for_tool(
    name: &str,
    id: Option<String>,
    arguments: Value,
    result: Option<Value>,
    duration_millis: Option<u64>,
) -> Option<AgentActionKind> {
    let normalized = name.to_ascii_lowercase();
    let path = path_argument(&arguments);
    if matches!(normalized.as_str(), "read" | "read_file") {
        return path.map(|path| AgentActionKind::FileRead {
            path,
            content_hash: result.as_ref().and_then(value_text).map(|text| hash(&text)),
        });
    }
    if matches!(
        normalized.as_str(),
        "write" | "edit" | "edit_file" | "code_edit"
    ) {
        return path.map(|path| {
            let before = string_argument(&arguments, &["old_string", "oldString"]);
            let after = string_argument(
                &arguments,
                &["content", "new_string", "newString", "code_edit"],
            );
            AgentActionKind::FileWrite {
                path,
                before_hash: before.as_deref().map(hash),
                after_hash: after.as_deref().map(hash),
                added_lines: after.as_deref().map(lines),
                removed_lines: before.as_deref().map(lines),
            }
        });
    }
    if matches!(
        normalized.as_str(),
        "bash" | "exec" | "exec_command" | "shell_command" | "run_terminal_cmd"
    ) {
        let command = string_argument(&arguments, &["command", "cmd"])?;
        let response = result.as_ref();
        return Some(AgentActionKind::Command {
            command,
            exit_code: response
                .and_then(|value| value.get("exit_code"))
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
            stdout: response
                .and_then(|value| value.get("stdout"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            stderr: response
                .and_then(|value| value.get("stderr"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            duration_millis,
        });
    }
    Some(AgentActionKind::Tool {
        name: name.to_owned(),
        id,
        arguments,
        result,
        duration_millis,
    })
}

fn prompt_of(value: &Value) -> Option<String> {
    let (kind, payload) = if let Some(payload) = value.get("payload") {
        (payload.get("type").and_then(Value::as_str), payload)
    } else {
        (value.get("type").and_then(Value::as_str), value)
    };
    let role = payload.get("role").and_then(Value::as_str);
    let message = payload.get("message").unwrap_or(payload);
    let message_role = message.get("role").and_then(Value::as_str);
    if kind == Some("user") || role == Some("user") || message_role == Some("user") {
        return message.get("content").and_then(value_text);
    }
    None
}

fn turn_of(value: &Value) -> Option<AgentActionKind> {
    let usage = value
        .pointer("/payload/info/total_token_usage")
        .or_else(|| value.get("usage"))?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(Value::as_u64);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("outputTokens"))
        .and_then(Value::as_u64);
    let cache_read = usage
        .get("cached_input_tokens")
        .or_else(|| usage.get("cache_read"))
        .and_then(Value::as_u64);
    if input.is_none() && output.is_none() && cache_read.is_none() {
        return None;
    }
    Some(AgentActionKind::Turn {
        model: value
            .pointer("/payload/model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cost_micros: None,
        text: None,
        thinking: None,
    })
}

fn kimi_turn(value: &Value, model: Option<&str>) -> Option<AgentActionKind> {
    let usage = value.get("usage")?;
    Some(AgentActionKind::Turn {
        model: model.map(str::to_owned),
        input_tokens: usage.get("inputOther").and_then(Value::as_u64),
        output_tokens: usage.get("output").and_then(Value::as_u64),
        cache_read_tokens: usage.get("inputCacheRead").and_then(Value::as_u64),
        cache_write_tokens: usage.get("inputCacheCreation").and_then(Value::as_u64),
        cost_micros: None,
        text: None,
        thinking: None,
    })
}

fn opencode_turn(message: &Value, parts: &[Value]) -> Option<AgentActionKind> {
    let tokens = message.get("tokens")?;
    let input = tokens.get("input").and_then(Value::as_u64);
    let output = tokens.get("output").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    let joined = |kind| {
        let text = parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some(kind))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    };
    Some(AgentActionKind::Turn {
        model: message
            .get("modelID")
            .and_then(Value::as_str)
            .map(str::to_owned),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: tokens.pointer("/cache/read").and_then(Value::as_u64),
        cache_write_tokens: tokens.pointer("/cache/write").and_then(Value::as_u64),
        cost_micros: None,
        text: joined("text"),
        thinking: joined("reasoning"),
    })
}

fn cursor_action(value: &Value) -> Option<AgentActionKind> {
    if let Some(tool) = value.get("toolFormerData") {
        let name = tool.get("name").and_then(Value::as_str)?;
        let arguments = tool
            .get("rawArgs")
            .cloned()
            .map(parse_json_string)
            .unwrap_or(Value::Null);
        return action_for_tool(
            name,
            tool.get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            arguments,
            tool.get("result").cloned(),
            None,
        );
    }
    let text = value.get("text").and_then(Value::as_str)?.trim();
    if text.is_empty() {
        return None;
    }
    match value.get("type").and_then(Value::as_u64) {
        Some(1) => Some(AgentActionKind::Prompt {
            content: text.to_owned(),
        }),
        Some(2) => {
            let tokens = value.get("tokenCount");
            Some(AgentActionKind::Turn {
                model: None,
                input_tokens: tokens
                    .and_then(|tokens| tokens.get("inputTokens"))
                    .and_then(Value::as_u64),
                output_tokens: tokens
                    .and_then(|tokens| tokens.get("outputTokens"))
                    .and_then(Value::as_u64),
                cache_read_tokens: None,
                cache_write_tokens: None,
                cost_micros: None,
                text: (!value
                    .get("isThought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false))
                .then_some(text.to_owned()),
                thinking: value
                    .get("isThought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then_some(text.to_owned()),
            })
        }
        _ => None,
    }
}

fn session_of(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            (value.get("type").and_then(Value::as_str) == Some("session_meta"))
                .then(|| {
                    value
                        .pointer("/payload/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        })
}

fn path_session(path: &Path) -> Option<String> {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn blocks_text(value: Option<&Value>, kind: &str) -> Option<String> {
    let text = value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(values) => {
            let text = values
                .iter()
                .filter_map(value_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(_) => value
            .get("text")
            .or_else(|| value.get("input_text"))
            .or_else(|| value.get("output_text"))
            .and_then(value_text),
        _ => None,
    }
}

fn parse_json_string(value: Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(value)
}

fn path_argument(arguments: &Value) -> Option<String> {
    string_argument(arguments, &["path", "file_path", "filePath", "target_file"])
}

fn string_argument(arguments: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn lines(text: &str) -> u32 {
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

fn entries(path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect()
}

pub fn all_statuses() -> Vec<Status> {
    Agent::ALL.into_iter().map(status).collect()
}

pub fn status(agent: Agent) -> Status {
    let transcript_root = transcript_root(agent).ok().filter(|path| path.exists());
    let hooks_path = hook_settings_path(agent).ok();
    let present = transcript_root.is_some()
        || hooks_path
            .as_ref()
            .is_some_and(|path| path.exists() || path.parent().is_some_and(Path::exists));
    let wired_to = hooks_path
        .as_deref()
        .and_then(|path| wired_to(path).ok().flatten());
    Status {
        agent,
        present,
        transcript_root,
        hooks_path,
        wired_to,
    }
}

/// Install only the hooks Riftr CLI has measured for this agent.  The operation
/// merges with other tools' entries and is idempotent.
pub fn install(agent: Agent, executable: &Path) -> Result<Change, String> {
    if !agent.supports_hooks() {
        return Err(format!(
            "{} supports transcript import but has no measured hook protocol",
            agent.name()
        ));
    }
    let path = hook_settings_path(agent)?;
    let events = hook_events(agent);
    let entry = hook_entry(agent, executable);
    let before = read_json(&path)?;
    let (mut after, added) = with_hooks(before.clone(), events, &entry);
    let mut paths = vec![path.clone()];
    let mut notes = Vec::new();
    if agent == Agent::ClaudeCode {
        let (updated, note) = keep_claude_transcripts(after);
        after = updated;
        notes.push(note);
    }
    if after != before {
        write_json(&path, &after)?;
    }
    if agent == Agent::Codex {
        let config = codex_config_path()?;
        let trust = codex_trust_entries(&path, &after)?;
        let change = configure_codex_hooks(&config, &trust)?;
        paths.push(config);
        notes.push(if change.hooks_enabled {
            "[features] hooks · off → true · Codex does not run hooks without it".into()
        } else {
            "[features] hooks · already true · unchanged".into()
        });
        notes.push(format!(
            "Codex hook trust · {} owned hook(s) persisted",
            change.trusted
        ));
    }
    Ok(Change {
        paths,
        added,
        removed: Vec::new(),
        notes,
    })
}

pub fn uninstall(agent: Agent) -> Result<Change, String> {
    if !agent.supports_hooks() {
        return Err(format!("{} has no measured hook protocol", agent.name()));
    }
    let path = hook_settings_path(agent)?;
    let before = read_json(&path)?;
    let codex_trust = (agent == Agent::Codex)
        .then(|| codex_trust_entries(&path, &before))
        .transpose()?;
    let (after, removed) = without_hooks(before.clone(), hook_events(agent));
    if after != before {
        write_json(&path, &after)?;
    }
    let mut paths = vec![path];
    let mut notes = Vec::new();
    if agent == Agent::Codex {
        let config = codex_config_path()?;
        let removed_trust = remove_codex_hook_trust(&config, &codex_trust.unwrap_or_default())?;
        paths.push(config);
        notes.push("[features] hooks · left as is · other tools may rely on it".into());
        notes.push(format!(
            "Codex hook trust · {removed_trust} owned entry/entries removed"
        ));
    }
    if agent == Agent::ClaudeCode && !removed.is_empty() {
        notes.push(
            "cleanupPeriodDays · left as is · uninstall never shortens transcript retention".into(),
        );
    }
    Ok(Change {
        paths,
        added: Vec::new(),
        removed,
        notes,
    })
}

pub fn transcript_root(agent: Agent) -> Result<PathBuf, String> {
    let home = || home(agent.name());
    match agent {
        Agent::ClaudeCode => std::env::var_os("ORKIA_CLAUDE_PROJECTS")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RIFTR_CLAUDE_PROJECTS").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| Ok(home()?.join(".claude/projects"))),
        Agent::Codex => std::env::var_os("ORKIA_CODEX_SESSIONS")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RIFTR_CODEX_SESSIONS").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| {
                Ok(std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or(home()?.join(".codex"))
                    .join("sessions"))
            }),
        Agent::Kimi => std::env::var_os("ORKIA_KIMI_SESSIONS")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RIFTR_KIMI_SESSIONS").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| Ok(home()?.join(".kimi-code/sessions"))),
        Agent::OpenCode => std::env::var_os("ORKIA_OPENCODE_STORAGE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RIFTR_OPENCODE_STORAGE").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| Ok(home()?.join(".local/share/opencode/storage"))),
        Agent::Cursor => std::env::var_os("ORKIA_CURSOR_STORAGE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("RIFTR_CURSOR_STORAGE").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| {
                Ok(home()?.join("Library/Application Support/Cursor/User/globalStorage"))
            }),
        other => Err(format!(
            "{} has live hooks only; its transcript format is not measured",
            other.name()
        )),
    }
}

fn hook_settings_path(agent: Agent) -> Result<PathBuf, String> {
    let home = || home(agent.name());
    match agent {
        Agent::ClaudeCode => Ok(std::env::var_os("ORKIA_CLAUDE_SETTINGS")
            .or_else(|| std::env::var_os("RIFTR_CLAUDE_SETTINGS"))
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".claude/settings.json"))),
        Agent::Codex => Ok(codex_home()?.join("hooks.json")),
        Agent::Gemini => Ok(std::env::var_os("ORKIA_GEMINI_SETTINGS")
            .or_else(|| std::env::var_os("RIFTR_GEMINI_SETTINGS"))
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".gemini/settings.json"))),
        Agent::Droid => Ok(std::env::var_os("ORKIA_DROID_SETTINGS")
            .or_else(|| std::env::var_os("RIFTR_DROID_SETTINGS"))
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".factory/settings.json"))),
        Agent::Qwen => Ok(std::env::var_os("ORKIA_QWEN_SETTINGS")
            .or_else(|| std::env::var_os("RIFTR_QWEN_SETTINGS"))
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".qwen/settings.json"))),
        other => Err(format!("{} has no measured hook protocol", other.name())),
    }
}

fn hook_events(agent: Agent) -> &'static [&'static str] {
    match agent {
        Agent::ClaudeCode => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "Stop",
            "SessionEnd",
        ],
        Agent::Codex => &["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop"],
        Agent::Gemini => &[
            "SessionStart",
            "BeforeAgent",
            "AfterTool",
            "AfterAgent",
            "SessionEnd",
        ],
        Agent::Droid | Agent::Qwen => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "Stop",
            "SessionEnd",
        ],
        _ => &[],
    }
}

fn hook_entry(agent: Agent, executable: &Path) -> Value {
    let async_hook = agent == Agent::ClaudeCode;
    let matcher = if agent == Agent::ClaudeCode || matches!(agent, Agent::Droid | Agent::Qwen) {
        ""
    } else {
        "*"
    };
    let command = command(executable, &format!("agent hook --agent {}", agent.name()));
    let mut hook = json!({"type":"command", "command":command});
    if async_hook {
        hook["async"] = json!(true);
    }
    json!({"matcher": matcher, "hooks":[hook]})
}

fn home(name: &str) -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| format!("cannot locate home directory for {name}"))
}
fn codex_home() -> Result<PathBuf, String> {
    Ok(std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(home("codex")?.join(".codex")))
}
fn codex_config_path() -> Result<PathBuf, String> {
    Ok(codex_home()?.join("config.toml"))
}

fn command(executable: &Path, args: &str) -> String {
    let exe = quoted(&executable.to_string_lossy());
    format!(
        "sh -c {}",
        quoted(&format!(
            "[ -x {exe} ] || exit 0; exec {exe} {args} {MARKER}"
        ))
    )
}
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn read_json(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "parse {} — Orkia will not overwrite it: {error}",
            path.display()
        )
    })
}
fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let temporary = path.with_extension("orkia-tmp");
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
}

fn with_hooks(mut settings: Value, events: &[&str], entry: &Value) -> (Value, Vec<String>) {
    let mut added = Vec::new();
    for event in events {
        let Some(entries) = event_entries(&mut settings, event) else {
            continue;
        };
        if entries.iter().any(is_orkias) {
            continue;
        }
        entries.push(entry.clone());
        added.push((*event).into());
    }
    (settings, added)
}
fn without_hooks(mut settings: Value, events: &[&str]) -> (Value, Vec<String>) {
    let mut removed = Vec::new();
    for event in events {
        let Some(entries) = event_entries(&mut settings, event) else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| !is_orkias(entry));
        if entries.len() != before {
            removed.push((*event).into());
        }
    }
    prune(&mut settings);
    (settings, removed)
}
fn event_entries<'a>(settings: &'a mut Value, event: &str) -> Option<&'a mut Vec<Value>> {
    let hooks = settings
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    if hooks.contains_key(event) && !hooks[event].is_array() {
        return None;
    }
    hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
}
fn is_orkias(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.contains(MARKER))
        })
    })
}
fn prune(settings: &mut Value) {
    if let Some(map) = settings.as_object_mut() {
        if let Some(hooks) = map.get_mut("hooks").and_then(Value::as_object_mut) {
            hooks.retain(|_, entries| !entries.as_array().is_some_and(Vec::is_empty));
            if hooks.is_empty() {
                map.remove("hooks");
            }
        }
    }
}
fn wired_to(path: &Path) -> Result<Option<String>, String> {
    let value = read_json(path)?;
    let Some(command) = find_command(&value) else {
        return Ok(None);
    };
    guarded_path(&command)
        .ok_or_else(|| {
            format!(
                "cannot read guarded executable path from {}",
                path.display()
            )
        })
        .map(Some)
}
fn find_command(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| command.contains(MARKER))
            .map(str::to_owned)
            .or_else(|| map.values().find_map(find_command)),
        Value::Array(values) => values.iter().find_map(find_command),
        _ => None,
    }
}
fn guarded_path(command: &str) -> Option<String> {
    let script = unquoted(command.strip_prefix("sh -c ")?)?.0;
    unquoted(script.split("[ -x ").nth(1)?).map(|(path, _)| path)
}
fn unquoted(text: &str) -> Option<(String, &str)> {
    let mut rest = text.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.strip_prefix(r"\''") {
            Some(more) => {
                word.push('\'');
                rest = more;
            }
            None => return Some((word, rest)),
        }
    }
}
fn keep_claude_transcripts(mut settings: Value) -> (Value, String) {
    let Some(map) = settings.as_object_mut() else {
        return (
            settings,
            "cleanupPeriodDays · unchanged · settings root is not an object".into(),
        );
    };
    let old = map.get("cleanupPeriodDays").and_then(Value::as_u64);
    if old >= Some(3650) {
        return (
            settings,
            "cleanupPeriodDays · already long enough · unchanged".into(),
        );
    }
    map.insert("cleanupPeriodDays".into(), json!(3650));
    (
        settings,
        format!(
            "cleanupPeriodDays · {} → 3650 · transcripts retained",
            old.map(|n| n.to_string()).unwrap_or_else(|| "unset".into())
        ),
    )
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexTrustEntry {
    key: String,
    trusted_hash: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CodexConfigChange {
    hooks_enabled: bool,
    trusted: usize,
}

fn codex_trust_entries(path: &Path, settings: &Value) -> Result<Vec<CodexTrustEntry>, String> {
    let source = absolute_path(path)?;
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for event in hook_events(Agent::Codex) {
        let Some(groups) = hooks.get(*event).and_then(Value::as_array) else {
            continue;
        };
        for (group_index, group) in groups.iter().enumerate() {
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (handler_index, handler) in handlers.iter().enumerate() {
                if !handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(MARKER))
                {
                    continue;
                }
                let event_name = codex_event_key(event)
                    .ok_or_else(|| format!("unsupported Codex hook event {event}"))?;
                let matcher = if matches!(*event, "UserPromptSubmit" | "Stop") {
                    None
                } else {
                    group.get("matcher").and_then(Value::as_str)
                };
                entries.push(CodexTrustEntry {
                    key: format!(
                        "{}:{event_name}:{group_index}:{handler_index}",
                        source.display()
                    ),
                    trusted_hash: codex_command_hook_hash(event_name, matcher, handler)?,
                });
            }
        }
    }
    Ok(entries)
}

fn codex_event_key(event: &str) -> Option<&'static str> {
    match event {
        "SessionStart" => Some("session_start"),
        "UserPromptSubmit" => Some("user_prompt_submit"),
        "PostToolUse" => Some("post_tool_use"),
        "Stop" => Some("stop"),
        _ => None,
    }
}

/// Mirrors Codex's `command_hook_hash`: hash the normalized, configuration
/// derived identity, rather than the JSON source text.  The field set and
/// default timeout (600 seconds) come from Codex 0.146's hook discovery code.
fn codex_command_hook_hash(
    event_name: &str,
    matcher: Option<&str>,
    handler: &Value,
) -> Result<String, String> {
    let command = handler
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex hook has no command".to_owned())?;
    let timeout = handler
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .max(1);
    let asynchronous = handler
        .get("async")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut normalized_handler = serde_json::Map::new();
    normalized_handler.insert("type".into(), Value::String("command".into()));
    normalized_handler.insert("command".into(), Value::String(command.into()));
    normalized_handler.insert("timeout".into(), Value::Number(timeout.into()));
    normalized_handler.insert("async".into(), Value::Bool(asynchronous));
    if let Some(status) = handler.get("statusMessage").and_then(Value::as_str) {
        normalized_handler.insert("statusMessage".into(), Value::String(status.into()));
    }
    if let Some(limit) = handler
        .get("additionalContextLimit")
        .and_then(Value::as_u64)
        .filter(|limit| *limit != 2_500)
    {
        normalized_handler.insert("additionalContextLimit".into(), Value::Number(limit.into()));
    }
    let mut identity = serde_json::Map::new();
    identity.insert("event_name".into(), Value::String(event_name.into()));
    identity.insert(
        "hooks".into(),
        Value::Array(vec![Value::Object(normalized_handler)]),
    );
    if let Some(matcher) = matcher {
        identity.insert("matcher".into(), Value::String(matcher.into()));
    }
    let canonical = canonical_json(&Value::Object(identity));
    let bytes = serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
    Ok(format!("sha256:{}", hash_bytes(&bytes)))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        other => other.clone(),
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| error.to_string())
    }
}

fn configure_codex_hooks(
    path: &Path,
    trust: &[CodexTrustEntry],
) -> Result<CodexConfigChange, String> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        format!(
            "parse {} — Orkia will not overwrite it: {error}",
            path.display()
        )
    })?;
    let original = doc.to_string();
    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let table = features.as_table_mut().ok_or_else(|| {
        format!(
            "{} has a `features` value that is not a table",
            path.display()
        )
    })?;
    let hooks_enabled = table.get("hooks").and_then(Item::as_bool) != Some(true);
    if hooks_enabled {
        table.insert("hooks", value(true));
    }
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("{} has a `hooks` value that is not a table", path.display()))?;
    let state = hooks
        .entry("state")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            format!(
                "{} has a `hooks.state` value that is not a table",
                path.display()
            )
        })?;
    let mut trusted = 0;
    for entry in trust {
        let item = state
            .entry(&entry.key)
            .or_insert_with(|| Item::Table(Table::new()));
        let item = item.as_table_mut().ok_or_else(|| {
            format!(
                "{} has a non-table hooks.state entry for {}",
                path.display(),
                entry.key
            )
        })?;
        if item.get("trusted_hash").and_then(Item::as_str) != Some(&entry.trusted_hash) {
            item.insert("trusted_hash", value(&entry.trusted_hash));
            trusted += 1;
        }
    }
    if doc.to_string() != original {
        write_toml(path, &doc)?;
    }
    Ok(CodexConfigChange {
        hooks_enabled,
        trusted,
    })
}

fn remove_codex_hook_trust(path: &Path, entries: &[CodexTrustEntry]) -> Result<usize, String> {
    use toml_edit::DocumentMut;
    if entries.is_empty() || !path.exists() {
        return Ok(0);
    }
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        format!(
            "parse {} — Orkia will not overwrite it: {error}",
            path.display()
        )
    })?;
    let original = doc.to_string();
    let Some(state) = doc
        .get_mut("hooks")
        .and_then(|hooks| hooks.as_table_mut())
        .and_then(|hooks| hooks.get_mut("state"))
        .and_then(|state| state.as_table_mut())
    else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries {
        if state.remove(&entry.key).is_some() {
            removed += 1;
        }
    }
    if doc.to_string() != original {
        write_toml(path, &doc)?;
    }
    Ok(removed)
}

fn write_toml(path: &Path, document: &toml_edit::DocumentMut) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("orkia-tmp");
    std::fs::write(&temporary, document.to_string()).map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matrix_matches_riftr() {
        assert!(Agent::ClaudeCode.supports_transcripts() && Agent::ClaudeCode.supports_hooks());
        assert!(Agent::Codex.supports_transcripts() && Agent::Codex.supports_hooks());
        assert!(Agent::Gemini.supports_hooks() && !Agent::Gemini.supports_transcripts());
        for agent in [Agent::Kimi, Agent::OpenCode, Agent::Cursor] {
            assert!(agent.supports_transcripts() && !agent.supports_hooks());
        }
        for agent in [Agent::Droid, Agent::Qwen] {
            assert!(agent.supports_hooks() && !agent.supports_transcripts());
        }
    }
    #[test]
    fn merges_without_removing_other_tools_and_uninstalls_cleanly() {
        let mut value = json!({"hooks":{"Stop":[{"matcher":"","hooks":[{"command":"other"}]}]}});
        let entry = json!({"matcher":"","hooks":[{"command":"orkia agent hook # orkia hook"}]});
        let (added, events) = with_hooks(value.clone(), &["Stop", "SessionEnd"], &entry);
        assert_eq!(events, ["Stop", "SessionEnd"]);
        value = added;
        assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
        let (removed, events) = without_hooks(value, &["Stop", "SessionEnd"]);
        assert_eq!(events, ["Stop", "SessionEnd"]);
        assert_eq!(
            removed,
            json!({"hooks":{"Stop":[{"matcher":"","hooks":[{"command":"other"}]}]}})
        );
    }
    #[test]
    fn codex_alias_and_shell_quote_round_trip() {
        assert_eq!(Agent::parse("open-code"), Some(Agent::OpenCode));
        let path = Path::new("/a b/o'k");
        let command = command(path, "agent hook --agent codex");
        assert_eq!(guarded_path(&command).as_deref(), Some("/a b/o'k"));
    }

    #[test]
    fn normalizes_gemini_without_dropping_the_raw_payload() {
        let payload = parse_hook_payload(
            Agent::Gemini,
            r#"{"session_id":"s","hook_event_name":"AfterTool","cwd":"/repo","tool_input":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(payload.event, "PostToolUse");
        assert_eq!(payload.session_id.as_deref(), Some("s"));
        assert_eq!(payload.raw["tool_input"]["x"], 1);
    }

    #[test]
    fn codex_enables_but_never_disables_its_hook_feature() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        assert!(configure_codex_hooks(&config, &[]).unwrap().hooks_enabled);
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("hooks = true")
        );
        assert!(!configure_codex_hooks(&config, &[]).unwrap().hooks_enabled);
    }

    #[test]
    fn codex_trust_uses_its_normalized_hook_identity_and_is_reversible() {
        let temp = tempfile::tempdir().unwrap();
        let hooks = temp.path().join("hooks.json");
        let settings = json!({"hooks":{"PostToolUse":[{
            "matcher":"*",
            "hooks":[{"type":"command","command":"echo hook # orkia hook"}]
        }]}});
        let trust = codex_trust_entries(&hooks, &settings).unwrap();
        assert_eq!(trust.len(), 1);
        assert_eq!(
            trust[0].trusted_hash,
            "sha256:d88cc67c9ef4a8ab4e9e941b582b36f1a8ac09c979b3d8ed3749a426ea225062"
        );
        let config = temp.path().join("config.toml");
        let change = configure_codex_hooks(&config, &trust).unwrap();
        assert!(change.hooks_enabled);
        assert_eq!(change.trusted, 1);
        let persisted = std::fs::read_to_string(&config).unwrap();
        assert!(persisted.contains(&trust[0].key));
        assert!(persisted.contains(&trust[0].trusted_hash));
        assert_eq!(remove_codex_hook_trust(&config, &trust).unwrap(), 1);
        assert!(
            !std::fs::read_to_string(config)
                .unwrap()
                .contains(&trust[0].key)
        );
    }

    #[test]
    fn growing_codex_transcript_appends_only_its_new_action() {
        let path = Path::new("/history/rollout.jsonl");
        let initial = concat!(
            r#"{"type":"session_meta","payload":{"id":"s"}}"#,
            "\n",
            r#"{"type":"message","payload":{"role":"user","content":[{"type":"input_text","text":"first"}]}}"#,
            "\n"
        );
        let grown = format!(
            "{initial}{}\n",
            r#"{"type":"message","payload":{"role":"user","content":[{"type":"input_text","text":"second"}]}}"#
        );
        let delta =
            reconcile_transcript(Agent::Codex, path, "utf-8", Some(initial), &grown, &grown)
                .unwrap();
        assert!(matches!(
            delta,
            TranscriptReconciliation::Append(events)
                if events.len() == 1
                    && matches!(&events[0], CaptureEvent::AgentAction { action: AgentActionKind::Prompt { content }, .. } if content == "second")
        ));
    }

    #[test]
    fn claude_and_kimi_append_only_their_proven_suffixes() {
        let claude_path = Path::new("/history/claude.jsonl");
        let claude_initial = concat!(
            r#"{"sessionId":"claude","type":"user","message":{"role":"user","content":"first"}}"#,
            "\n"
        );
        let claude_grown = format!(
            "{claude_initial}{}\n",
            r#"{"sessionId":"claude","type":"user","message":{"role":"user","content":"second"}}"#
        );
        let claude = reconcile_transcript(
            Agent::ClaudeCode,
            claude_path,
            "utf-8",
            Some(claude_initial),
            &claude_grown,
            &claude_grown,
        )
        .unwrap();
        assert!(matches!(claude, TranscriptReconciliation::Append(events) if events.len() == 1));

        let kimi_path = Path::new("/history/wire.jsonl");
        let kimi_initial = concat!(
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"first"}]}"#,
            "\n"
        );
        let kimi_grown = format!(
            "{kimi_initial}{}\n",
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"second"}]}"#
        );
        let kimi = reconcile_transcript(
            Agent::Kimi,
            kimi_path,
            "utf-8",
            Some(kimi_initial),
            &kimi_grown,
            &kimi_grown,
        )
        .unwrap();
        assert!(matches!(kimi, TranscriptReconciliation::Append(events) if events.len() == 1));
    }

    #[test]
    fn opencode_and_cursor_revisions_fail_closed_instead_of_replaying_actions() {
        let opencode = reconcile_transcript(
            Agent::OpenCode,
            Path::new("/storage/session/global/ses_1.json"),
            "utf-8",
            Some(r#"{"session":"old"}"#),
            r#"{"session":"new"}"#,
            r#"{"id":"ses_1"}"#,
        )
        .unwrap();
        assert_eq!(opencode, TranscriptReconciliation::Unreconciled);
        let cursor = reconcile_transcript(
            Agent::Cursor,
            Path::new("/storage/state.vscdb"),
            "base64",
            Some("old"),
            "new",
            "",
        )
        .unwrap();
        assert_eq!(cursor, TranscriptReconciliation::Unreconciled);
    }

    #[test]
    fn opencode_snapshot_versions_sidecars_even_when_the_session_document_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let session = storage.join("session/global/ses_1.json");
        let message = storage.join("message/ses_1/msg_1.json");
        let part = storage.join("part/msg_1/prt_1.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::create_dir_all(message.parent().unwrap()).unwrap();
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&session, r#"{"id":"ses_1"}"#).unwrap();
        std::fs::write(&message, r#"{"role":"user"}"#).unwrap();
        std::fs::write(&part, r#"{"type":"text","text":"first"}"#).unwrap();
        let file = TranscriptFile {
            path: session.clone(),
            binary: false,
        };
        let first = transcript_snapshot(Agent::OpenCode, &file).unwrap();
        std::fs::write(&part, r#"{"type":"text","text":"second"}"#).unwrap();
        let second = transcript_snapshot(Agent::OpenCode, &file).unwrap();
        assert_ne!(first.content, second.content);
        assert_eq!(first.normalization_content, second.normalization_content);
        assert_eq!(
            reconcile_transcript(
                Agent::OpenCode,
                &session,
                &second.encoding,
                Some(&first.content),
                &second.content,
                &second.normalization_content,
            )
            .unwrap(),
            TranscriptReconciliation::Unreconciled
        );
    }

    #[test]
    fn rewritten_transcript_is_recorded_without_replaying_actions() {
        let delta = reconcile_transcript(
            Agent::Codex,
            Path::new("/history/rollout.jsonl"),
            "utf-8",
            Some("old"),
            "new",
            "new",
        )
        .unwrap();
        assert_eq!(delta, TranscriptReconciliation::Unreconciled);
    }

    #[test]
    fn claude_walk_excludes_nested_subagent_transcripts() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("project");
        std::fs::create_dir_all(parent.join("session/subagents")).unwrap();
        std::fs::write(parent.join("main.jsonl"), "main").unwrap();
        std::fs::write(parent.join("session/subagents/agent.jsonl"), "subagent").unwrap();
        let mut found = Vec::new();
        collect_transcript_files(temp.path(), Agent::ClaudeCode, 0, &mut found);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("main.jsonl"));
    }

    fn actions(events: &[CaptureEvent]) -> Vec<&AgentActionKind> {
        events
            .iter()
            .filter_map(|event| match event {
                CaptureEvent::AgentAction { action, .. } => Some(action),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_hook_agent_normalizes_its_measured_prompt_or_tool_fact() {
        for (agent, event) in [
            (Agent::ClaudeCode, "PostToolUse"),
            (Agent::Codex, "PostToolUse"),
            (Agent::Gemini, "AfterTool"),
            (Agent::Droid, "PostToolUse"),
            (Agent::Qwen, "PostToolUse"),
        ] {
            let payload = parse_hook_payload(
                agent,
                &format!(
                    r#"{{"session_id":"s-{0}","hook_event_name":"{event}","tool_name":"Read","tool_input":{{"path":"src/lib.rs"}},"tool_response":"source"}}"#,
                    agent.name()
                ),
            )
            .unwrap();
            assert!(matches!(
                actions(&normalize_hook(agent, &payload)).as_slice(),
                [AgentActionKind::FileRead { path, .. }] if path == "src/lib.rs"
            ));
        }
    }

    #[test]
    fn real_codex_apply_patch_payload_yields_one_write_per_file() {
        let payload = parse_hook_payload(
            Agent::Codex,
            r#"{"session_id":"codex-live","hook_event_name":"PostToolUse","tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** Add File: LIVE.md\n+live Codex hook captured\n*** Update File: src/lib.rs\n-old\n+new\n*** End Patch"},"tool_response":"Success"}"#,
        )
        .unwrap();
        let events = normalize_hook(Agent::Codex, &payload);
        assert!(matches!(
            actions(&events).as_slice(),
            [
                AgentActionKind::FileWrite { path: first, added_lines: Some(1), .. },
                AgentActionKind::FileWrite { path: second, added_lines: Some(1), removed_lines: Some(1), .. },
            ] if first == "LIVE.md" && second == "src/lib.rs"
        ));
    }

    #[test]
    fn codex_and_claude_jsonl_become_typed_actions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-test.jsonl");
        let codex = concat!(
            r#"{"type":"session_meta","payload":{"id":"codex-session"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"add parser"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":12,"output_tokens":7}}}}"#,
        );
        let decoded = normalize_transcript(Agent::Codex, &path, "utf-8", codex).unwrap();
        assert!(decoded.iter().all(|event| matches!(
            event,
            CaptureEvent::AgentAction { external_session: Some(session), .. }
                if session == "codex-session"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Prompt { content } if content == "add parser"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Command { command, .. } if command == "cargo test"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Turn {
                input_tokens: Some(12),
                output_tokens: Some(7),
                ..
            }
        )));

        let claude = r#"{"sessionId":"claude-session","type":"user","message":{"role":"user","content":"inspect this"}}"#;
        let decoded = normalize_transcript(Agent::ClaudeCode, &path, "utf-8", claude).unwrap();
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Prompt { content } if content == "inspect this"
        )));
    }

    #[test]
    fn kimi_wire_preserves_prompt_file_write_and_usage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("kimi-session/wire.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wire = concat!(
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"write it"}]}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t1","name":"Write","args":{"path":"src/lib.rs","content":"fn main() {}"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"t1","result":{"output":"ok"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":10,"output":3,"inputCacheRead":2,"inputCacheCreation":1}}}"#,
        );
        let decoded = normalize_transcript(Agent::Kimi, &path, "utf-8", wire).unwrap();
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::FileWrite { path, after_hash: Some(_), .. } if path == "src/lib.rs"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Turn {
                cache_read_tokens: Some(2),
                cache_write_tokens: Some(1),
                ..
            }
        )));
    }

    #[test]
    fn opencode_store_is_assembled_into_actions() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let session = storage.join("session/global/ses_1.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::create_dir_all(storage.join("message/ses_1")).unwrap();
        std::fs::create_dir_all(storage.join("part/msg_1")).unwrap();
        std::fs::write(&session, r#"{"id":"ses_1"}"#).unwrap();
        std::fs::write(
            storage.join("message/ses_1/msg_1.json"),
            r#"{"role":"user"}"#,
        )
        .unwrap();
        std::fs::write(
            storage.join("part/msg_1/prt_1.json"),
            r#"{"type":"text","text":"please edit"}"#,
        )
        .unwrap();
        std::fs::write(storage.join("part/msg_1/prt_2.json"), r#"{"type":"tool","tool":"write","callID":"call","state":{"input":{"filePath":"src/a.rs","content":"pub fn a() {}"},"output":"ok"}}"#).unwrap();
        let decoded = normalize_transcript(
            Agent::OpenCode,
            &session,
            "utf-8",
            &std::fs::read_to_string(&session).unwrap(),
        )
        .unwrap();
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Prompt { content } if content == "please edit"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::FileWrite { path, .. } if path == "src/a.rs"
        )));
    }

    #[test]
    fn cursor_database_is_read_only_and_normalized() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.vscdb");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params!["composerData:composer-1", r#"{"composerId":"composer-1"}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![
                    "bubbleId:composer-1:1",
                    r#"{"type":1,"text":"review this"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params!["bubbleId:composer-1:2", r#"{"toolFormerData":{"name":"edit_file","rawArgs":"{\"target_file\":\"src/a.rs\",\"code_edit\":\"new\"}"}}"#],
            )
            .unwrap();
        drop(connection);
        let decoded = normalize_transcript(Agent::Cursor, &path, "base64", "ignored").unwrap();
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::Prompt { content } if content == "review this"
        )));
        assert!(actions(&decoded).iter().any(|action| matches!(
            action,
            AgentActionKind::FileWrite { path, .. } if path == "src/a.rs"
        )));
    }
}
