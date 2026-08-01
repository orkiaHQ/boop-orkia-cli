//! Causal capture for human sessions and coding-agent transcripts.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use orkia_model::{CaptureEvent, CaptureOrigin, OrkiaError, Result, SessionId};
use orkia_ports::GitRepository;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub trait ProviderAdapter: Send + Sync {
    fn provider_name(&self) -> &'static str;
    fn capture(&self, transcript: &str) -> Vec<CaptureEvent>;
}

/// Capture's only persistence dependency. The composition root binds this
/// sink to the signed Git ledger (or a contract double); capture itself never
/// imports a ledger implementation.
pub trait EventSink: Send + Sync {
    fn append(&self, event: CaptureEvent) -> Result<()>;
}

/// Small composition helper for binding a concrete signed ledger without
/// making this crate depend on that implementation.
pub struct CallbackSink<F>(pub F);

impl<F> EventSink for CallbackSink<F>
where
    F: Fn(CaptureEvent) -> Result<()> + Send + Sync,
{
    fn append(&self, event: CaptureEvent) -> Result<()> {
        (self.0)(event)
    }
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
        .map(
            |line| match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) if value.get("tool").is_some() => CaptureEvent::ToolCall {
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
                },
                Ok(value) => CaptureEvent::Transcript {
                    provider: provider.to_owned(),
                    content: value.to_string(),
                },
                Err(_) => CaptureEvent::Transcript {
                    provider: provider.to_owned(),
                    content: line.to_owned(),
                },
            },
        )
        .collect()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Coverage {
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
    pub unknown_write: bool,
}
impl Coverage {
    pub fn milli(&self) -> u16 {
        if self.unknown_write { 0 } else { 1000 }
    }
}

pub struct Session<L> {
    pub id: SessionId,
    ledger: L,
    coverage: Coverage,
}
impl<L: EventSink> Session<L> {
    pub fn start(
        ledger: L,
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
                if let Ok(event) = result
                    && let Ok(mut paths) = target.lock()
                {
                    paths.extend(event.paths);
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
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], CaptureEvent::ToolCall { .. }));
    }
}
