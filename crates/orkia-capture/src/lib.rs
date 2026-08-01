//! Causal capture for human sessions and coding-agent transcripts.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use orkia_ledger::Ledger;
use orkia_model::{AgentActionKind, CaptureEvent, CaptureOrigin, OrkiaError, Result, SessionId};
use orkia_ports::{Clock, GitRepository, LedgerStore};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub trait ProviderAdapter: Send + Sync {
    fn provider_name(&self) -> &'static str;
    fn capture(&self, transcript: &str) -> Vec<CaptureEvent>;
}

pub struct CodexAdapter;
impl ProviderAdapter for CodexAdapter {
    fn provider_name(&self) -> &'static str {
        "codex"
    }
    fn capture(&self, transcript: &str) -> Vec<CaptureEvent> {
        parse_jsonl_provider(self.provider_name(), transcript)
    }
}
pub struct ClaudeAdapter;
impl ProviderAdapter for ClaudeAdapter {
    fn provider_name(&self) -> &'static str {
        "claude-code"
    }
    fn capture(&self, transcript: &str) -> Vec<CaptureEvent> {
        parse_jsonl_provider(self.provider_name(), transcript)
    }
}

fn parse_jsonl_provider(provider: &str, transcript: &str) -> Vec<CaptureEvent> {
    transcript
        .lines()
        .filter(|line| !line.trim().is_empty())
        .flat_map(
            |line| match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    let mut events = Vec::new();
                    if let Some(turn) = normalized_turn(provider, &value) {
                        events.push(turn);
                    }
                    if value.get("tool").is_some() {
                        events.push(CaptureEvent::ToolCall {
                            tool: value
                                .get("tool")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_owned(),
                            arguments: value
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            result: value
                                .get("result")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        });
                        if let Some(action) = normalized_action(provider, &value) {
                            events.push(action);
                        }
                    } else {
                        events.push(CaptureEvent::Transcript {
                            provider: provider.to_owned(),
                            content: value.to_string(),
                        });
                    }
                    events
                }
                Err(_) => vec![CaptureEvent::Transcript {
                    provider: provider.to_owned(),
                    content: line.to_owned(),
                }],
            },
        )
        .collect()
}

fn normalized_action(provider: &str, value: &serde_json::Value) -> Option<CaptureEvent> {
    let tool = value.get("tool")?.as_str()?.to_ascii_lowercase();
    let arguments = value
        .get("arguments")
        .and_then(serde_json::Value::as_object);
    let mut paths = BTreeSet::new();
    if let Some(arguments) = arguments {
        if let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) {
            paths.insert(path.into());
        }
        if let Some(paths_value) = arguments.get("paths").and_then(serde_json::Value::as_array) {
            paths.extend(
                paths_value
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    let kind = if tool.contains("read") || tool == "cat" {
        AgentActionKind::Read
    } else if tool.contains("write") || tool.contains("edit") || tool.contains("patch") {
        AgentActionKind::Write
    } else if tool.contains("shell") || tool.contains("command") || tool == "exec" {
        AgentActionKind::Command
    } else if tool.is_empty() {
        AgentActionKind::Unknown
    } else {
        AgentActionKind::Tool
    };
    let command = arguments.and_then(|arguments| {
        arguments
            .get("command")
            .or_else(|| arguments.get("cmd"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    let exit_code = value
        .get("result")
        .and_then(serde_json::Value::as_object)
        .and_then(|result| result.get("exit_code"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    Some(CaptureEvent::AgentAction {
        provider: provider.to_owned(),
        session: None,
        base_commit: None,
        turn_id: value
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        action_id: value
            .get("action_id")
            .or_else(|| value.get("tool_call_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        kind,
        paths,
        command,
        exit_code,
    })
}

fn normalized_turn(provider: &str, value: &serde_json::Value) -> Option<CaptureEvent> {
    let usage = value.get("usage").and_then(serde_json::Value::as_object);
    let string = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let number = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .or_else(|| usage.and_then(|usage| usage.get(name)?.as_u64()))
    };
    let turn_id = string("turn_id").or_else(|| string("id"));
    let model = string("model");
    let input_tokens = number("input_tokens").or_else(|| number("prompt_tokens"));
    let output_tokens = number("output_tokens").or_else(|| number("completion_tokens"));
    let cost_micros = number("cost_micros");
    (turn_id.is_some()
        || model.is_some()
        || input_tokens.is_some()
        || output_tokens.is_some()
        || cost_micros.is_some())
    .then(|| CaptureEvent::AgentTurn {
        provider: provider.to_owned(),
        session: None,
        base_commit: None,
        turn_id,
        model,
        input_tokens,
        output_tokens,
        cost_micros,
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Coverage {
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
    pub unknown_write: bool,
}
impl Coverage {
    pub fn milli(&self) -> u16 {
        if self.unknown_write {
            0
        } else if self.modified.is_empty() {
            1000
        } else {
            1000
        }
    }
}

pub struct Session<L, C> {
    pub id: SessionId,
    ledger: Ledger<L, C>,
    coverage: Coverage,
}
impl<L: LedgerStore, C: Clock> Session<L, C> {
    pub fn start(
        ledger: Ledger<L, C>,
        git: &dyn GitRepository,
        origin: CaptureOrigin,
        objective: impl Into<String>,
    ) -> Result<Self> {
        let id = SessionId::new();
        ledger.append(CaptureEvent::SessionStarted {
            session: id.clone(),
            origin,
            base_commit: git.head_commit()?,
            objective: objective.into(),
        })?;
        Ok(Self {
            id,
            ledger,
            coverage: Coverage::default(),
        })
    }
    pub fn record_provider(
        &mut self,
        adapter: &dyn ProviderAdapter,
        transcript: &str,
    ) -> Result<()> {
        for event in adapter.capture(transcript) {
            self.ledger.append(event)?;
        }
        Ok(())
    }
    pub fn observe(
        &mut self,
        read: BTreeSet<String>,
        modified: BTreeSet<String>,
        unknown_write: bool,
    ) -> Result<()> {
        self.coverage.read.extend(read.iter().cloned());
        self.coverage.modified.extend(modified.iter().cloned());
        self.coverage.unknown_write |= unknown_write;
        self.ledger.append(CaptureEvent::FilesObserved {
            read,
            modified,
            unknown_write,
        })?;
        Ok(())
    }
    pub fn run(&mut self, program: &str, args: &[String]) -> Result<i32> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| OrkiaError::External(format!("cannot run {program}: {e}")))?;
        let code = output.status.code();
        self.ledger.append(CaptureEvent::Command {
            command: std::iter::once(program)
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            exit_code: code,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })?;
        Ok(code.unwrap_or(1))
    }
    pub fn checkpoint(&mut self, commit: String) -> Result<()> {
        self.ledger.append(CaptureEvent::Checkpoint { commit })?;
        Ok(())
    }
    pub fn close(self) -> Result<()> {
        self.ledger
            .append(CaptureEvent::SessionClosed { session: self.id })?;
        Ok(())
    }
    pub fn coverage(&self) -> &Coverage {
        &self.coverage
    }
}

/// Cross-platform recursive watcher. The caller decides whether a received
/// write was mediated by `orkia run`; unclassified writes mark coverage low.
pub struct WorkspaceWatcher {
    _watcher: RecommendedWatcher,
    changed: Arc<Mutex<BTreeSet<PathBuf>>>,
}
impl WorkspaceWatcher {
    pub fn start(root: &Path) -> Result<Self> {
        let changed = Arc::new(Mutex::new(BTreeSet::new()));
        let target = changed.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result {
                    if let Ok(mut paths) = target.lock() {
                        paths.extend(event.paths);
                    }
                }
            })
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| OrkiaError::External(e.to_string()))?;
        Ok(Self {
            _watcher: watcher,
            changed,
        })
    }
    pub fn drain(&self) -> BTreeSet<PathBuf> {
        std::mem::take(
            &mut *self
                .changed
                .lock()
                .expect("workspace watcher lock poisoned"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_provider_records_are_retained() {
        let events =
            CodexAdapter.capture("{\"tool\":\"shell\",\"arguments\":{\"cmd\":\"ls\"}}\nanswer");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], CaptureEvent::ToolCall { .. }));
        assert!(matches!(events[1], CaptureEvent::AgentAction { .. }));
    }

    #[test]
    fn provider_usage_is_normalized_without_losing_raw_transcript() {
        let events = CodexAdapter.capture(
            r#"{"id":"turn-7","model":"gpt-test","usage":{"input_tokens":12,"output_tokens":34},"cost_micros":56}"#,
        );
        assert!(matches!(
            events[0],
            CaptureEvent::AgentTurn {
                ref turn_id,
                ref model,
                input_tokens: Some(12),
                output_tokens: Some(34),
                cost_micros: Some(56),
                ..
            } if turn_id.as_deref() == Some("turn-7") && model.as_deref() == Some("gpt-test")
        ));
        assert!(matches!(events[1], CaptureEvent::Transcript { .. }));
    }

    #[test]
    fn tool_payload_is_normalized_to_action_with_paths_and_command() {
        let events = CodexAdapter.capture(
            r#"{"tool":"shell","turn_id":"turn-1","tool_call_id":"call-1","arguments":{"cmd":"cargo test","paths":["src/lib.rs"]},"result":{"exit_code":0}}"#,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            CaptureEvent::AgentAction {
                turn_id,
                action_id,
                kind: AgentActionKind::Command,
                paths,
                command: Some(command),
                exit_code: Some(0),
                ..
            } if turn_id.as_deref() == Some("turn-1")
                && action_id.as_deref() == Some("call-1")
                && paths.contains("src/lib.rs")
                && command == "cargo test"
        )));
    }
}
