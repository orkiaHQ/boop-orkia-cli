//! Native integrations for the coding agents supported by Riftr CLI.
//!
//! The support matrix deliberately mirrors Riftr: transcript discovery for
//! Claude Code, Codex, Kimi, OpenCode and Cursor; live hooks for Claude Code,
//! Codex, Gemini, Factory Droid and Qwen Code.  An adapter is explicit about
//! which side it supports: no unmeasured hook is installed for an import-only
//! agent, and no transcript reader is fabricated for a hook-only agent.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

/// Locate precisely the transcript file shapes Riftr imports. Parents are
/// visited before descendants, preventing a Claude subagent transcript from
/// claiming its parent session first.
pub fn transcript_files(agent: Agent) -> Result<Vec<TranscriptFile>, String> {
    if !agent.supports_transcripts() {
        return Err(format!("{} has live hooks only", agent.name()));
    }
    let mut found = Vec::new();
    collect_transcript_files(&transcript_root(agent)?, agent, 0, &mut found);
    Ok(found)
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
        let changed = enable_codex_hooks(&config)?;
        paths.push(config);
        notes.push(if changed {
            "[features] hooks · off → true · Codex does not run hooks without it".into()
        } else {
            "[features] hooks · already true · unchanged".into()
        });
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
    let (after, removed) = without_hooks(before.clone(), hook_events(agent));
    if after != before {
        write_json(&path, &after)?;
    }
    let mut paths = vec![path];
    let mut notes = Vec::new();
    if agent == Agent::Codex {
        paths.push(codex_config_path()?);
        notes.push("[features] hooks · left as is · other tools may rely on it".into());
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
fn enable_codex_hooks(path: &Path) -> Result<bool, String> {
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
    let features = doc
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    let table = features.as_table_mut().ok_or_else(|| {
        format!(
            "{} has a `features` value that is not a table",
            path.display()
        )
    })?;
    if table.get("hooks").and_then(Item::as_bool) == Some(true) {
        return Ok(false);
    }
    table.insert("hooks", value(true));
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("orkia-tmp");
    std::fs::write(&temporary, doc.to_string()).map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())?;
    Ok(true)
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
        assert!(enable_codex_hooks(&config).unwrap());
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("hooks = true")
        );
        assert!(!enable_codex_hooks(&config).unwrap());
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
}
