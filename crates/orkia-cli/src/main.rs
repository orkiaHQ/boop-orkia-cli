//! Terminal composition root for Orkia.

use clap::{Parser, Subcommand, ValueEnum};
use orkia_agents::{
    Agent as SupportedAgent, TranscriptReconciliation, all_statuses, install as install_agent,
    normalize_hook, parse_hook_payload, reconcile_transcript, status as agent_status,
    transcript_files, transcript_files_at, transcript_snapshot, uninstall as uninstall_agent,
};
use orkia_capture::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use orkia_git::LibGit2Repository;
use orkia_github::GitHubApp;
use orkia_identity::Identity;
use orkia_ledger::{Ledger, SystemClock, verify_chain};
use orkia_model::{
    Actor, AgentSnapshotPhase, CaptureEvent, CaptureOrigin, OrkiaError, RepositoryId, Result,
    ReviewPlan, SessionId,
};
use orkia_ports::{Forge, GitRepository, LedgerStore, SecretStore};
use orkia_review::{PlanningInput, plan};
use orkia_semantic::{ChangedFile, extract_atoms, infer_dependencies};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "orkia", about = "Git-native semantic review engine")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    repository: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Native coding-agent support matching Riftr CLI's measured matrix.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Ledger {
        #[command(subcommand)]
        command: LedgerCommand,
    },
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    Integrate {
        #[arg(long)]
        plan: String,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value_t = 0)]
        approvals: u8,
    },
}
#[derive(Subcommand)]
enum AgentCommand {
    List,
    /// Import native transcript documents for a Riftr-supported agent.
    Import {
        #[arg(long)]
        agent: String,
        /// Override the agent's conventional transcript directory.
        #[arg(long)]
        source: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        agent: String,
    },
    Install {
        #[arg(long)]
        agent: String,
    },
    Uninstall {
        #[arg(long)]
        agent: String,
    },
    /// Entry point invoked by native agent hooks. Errors are never allowed to
    /// block the agent process that invoked it.
    Hook {
        #[arg(long)]
        agent: String,
    },
}
#[derive(Subcommand)]
enum IdentityCommand {
    Init {
        #[arg(long)]
        name: String,
    },
}
#[derive(Subcommand)]
enum SessionCommand {
    Start {
        #[arg(long)]
        objective: String,
        #[arg(long, value_enum, default_value_t = Origin::Human)]
        origin: Origin,
    },
    Capture {
        #[arg(long, value_enum)]
        provider: Provider,
        #[arg(long)]
        transcript: PathBuf,
    },
    Agent {
        #[arg(long, value_enum)]
        provider: Provider,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Run {
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    Checkpoint,
    Close,
}
#[derive(Subcommand)]
enum LedgerCommand {
    Verify,
}
#[derive(Subcommand)]
enum ReviewCommand {
    Plan {
        #[arg(long)]
        checkpoint: Option<String>,
    },
    Show {
        #[arg(long)]
        plan: String,
    },
    Merge {
        #[arg(long)]
        plan: String,
        #[arg(long, required = true, value_delimiter = ',')]
        units: Vec<String>,
    },
    Project {
        #[arg(long)]
        plan: String,
    },
    Publish {
        #[arg(long)]
        plan: String,
        #[arg(long)]
        github_owner: String,
        #[arg(long)]
        github_repository: String,
        #[arg(long, default_value = "main")]
        base: String,
        #[arg(long, default_value = "origin")]
        remote: String,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Origin {
    Human,
    Codex,
    Claude,
}
#[derive(Clone, Copy, ValueEnum)]
enum Provider {
    Codex,
    Claude,
}
#[derive(Clone, Serialize, Deserialize)]
struct SessionState {
    id: SessionId,
    repository: RepositoryId,
    actor: Actor,
    base_commit: String,
    observed_paths: BTreeSet<String>,
}
#[derive(Clone)]
struct FileSecrets {
    root: PathBuf,
}
impl SecretStore for FileSecrets {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.root.join(key);
        match fs::read(path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OrkiaError::External(error.to_string())),
        }
    }
    fn put(&self, key: &str, value: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|e| OrkiaError::External(e.to_string()))?;
        let path = self.root.join(key);
        fs::write(&path, value).map_err(|e| OrkiaError::External(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|e| OrkiaError::External(e.to_string()))?;
        }
        Ok(())
    }
}

fn main() {
    let cli = Cli::parse();
    let fail_safe_hook = matches!(
        &cli.command,
        Command::Agent {
            command: AgentCommand::Hook { .. }
        }
    );
    if let Err(error) = run(cli) {
        if fail_safe_hook {
            return;
        }
        eprintln!("orkia: {error}");
        std::process::exit(1);
    }
}
fn run(cli: Cli) -> Result<()> {
    if let Command::Agent {
        command: AgentCommand::Hook { agent },
    } = &cli.command
    {
        return run_agent_hook(agent, &cli.repository);
    }
    let git = LibGit2Repository::open(&cli.repository)?;
    let root = git_dir(&cli.repository)?;
    let secrets = FileSecrets {
        root: root.join("orkia/keys"),
    };
    fs::create_dir_all(root.join("orkia/plans"))
        .map_err(|e| OrkiaError::External(e.to_string()))?;
    match cli.command {
        Command::Identity {
            command: IdentityCommand::Init { name },
        } => {
            let identity = Identity::generate(name);
            identity.save(&secrets, "identity")?;
            write_json(&root.join("orkia/actor.json"), identity.actor())?;
            write_json(&root.join("orkia/repository.json"), &RepositoryId::new())?;
            println!("identity {} initialized", identity.actor().id.0);
        }
        Command::Session { command } => {
            handle_session(command, &git, &root, &secrets, &cli.repository)?
        }
        Command::Agent { command } => handle_agent(command, &git, &root, &secrets)?,
        Command::Ledger {
            command: LedgerCommand::Verify,
        } => {
            let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
            let events = git.ledger_store().read_all()?;
            let actors = BTreeMap::from([(actor.id.clone(), actor)]);
            verify_chain(&events, &actors)?;
            println!("verified {} signed ledger events", events.len());
        }
        Command::Review { command } => {
            handle_review(command, &git, &root, &cli.repository, &secrets)?
        }
        Command::Integrate {
            plan,
            branch,
            approvals,
        } => {
            let review: ReviewPlan =
                read_json(&root.join("orkia/plans").join(format!("{plan}.json")))?;
            let policy_path = cli.repository.join("orkia.toml");
            let policy = if policy_path.exists() {
                orkia_policy::load(&policy_path)?
            } else {
                orkia_model::RepositoryPolicy::default()
            };
            let ledger = open_repository_ledger(&git, &root, &secrets)?;
            let validations = run_validations(&cli.repository, &policy, &ledger)?;
            orkia_policy::evaluate(&policy, &review, &validations, approvals, &branch)?;
            println!("integration policy passed for {branch}");
        }
    }
    Ok(())
}

fn supported_agent(name: &str) -> Result<SupportedAgent> {
    SupportedAgent::parse(name).ok_or_else(|| OrkiaError::Invalid(format!("unknown agent {name}")))
}

fn handle_agent(
    command: AgentCommand,
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
) -> Result<()> {
    match command {
        AgentCommand::List => {
            for status in all_statuses() {
                println!(
                    "{}\ttranscripts={}\thooks={}\tpresent={}",
                    status.agent.name(),
                    status.agent.supports_transcripts(),
                    status.agent.supports_hooks(),
                    status.present
                );
            }
        }
        AgentCommand::Import { agent, source } => {
            let agent = supported_agent(&agent)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            let known_events = git.ledger_store().read_all()?;
            let files = match source {
                Some(source) => transcript_files_at(agent, &source),
                None => transcript_files(agent),
            }
            .map_err(OrkiaError::External)?;
            let mut actions = 0;
            let mut already_imported = 0;
            let mut unreconciled = 0;
            for file in &files {
                let snapshot = transcript_snapshot(agent, file).map_err(OrkiaError::External)?;
                let encoding = snapshot.encoding;
                let content = snapshot.content;
                let transcript_path = file.path.to_string_lossy().into_owned();
                let previous = latest_transcript_revision(
                    &known_events,
                    agent.name(),
                    &transcript_path,
                    &encoding,
                );
                let reconciliation = reconcile_transcript(
                    agent,
                    &file.path,
                    &encoding,
                    previous,
                    &content,
                    &snapshot.normalization_content,
                )
                .map_err(OrkiaError::External)?;
                let decoded = match reconciliation {
                    TranscriptReconciliation::Unchanged => {
                        already_imported += 1;
                        continue;
                    }
                    TranscriptReconciliation::Append(events) => events,
                    TranscriptReconciliation::Unreconciled => {
                        unreconciled += 1;
                        Vec::new()
                    }
                };
                ledger.append(CaptureEvent::AgentTranscript {
                    agent: agent.name().into(),
                    path: transcript_path,
                    encoding,
                    content,
                })?;
                actions += decoded.len();
                for event in decoded {
                    ledger.append(bind_known_agent_session(event, &known_events))?;
                }
            }
            println!(
                "imported {} {} transcript document(s), normalized {} new action(s), skipped {} unchanged document(s), recorded {} unreconciled revision(s)",
                files.len().saturating_sub(already_imported),
                agent.name(),
                actions,
                already_imported,
                unreconciled,
            );
        }
        AgentCommand::Status { agent } => {
            let status = agent_status(supported_agent(&agent)?);
            println!(
                "{}\tpresent={}\ttranscript_root={}\thooks={}\twired_to={}",
                status.agent.name(),
                status.present,
                status
                    .transcript_root
                    .as_deref()
                    .map(Path::display)
                    .map(|path| path.to_string())
                    .unwrap_or_else(|| "-".into()),
                status
                    .hooks_path
                    .as_deref()
                    .map(Path::display)
                    .map(|path| path.to_string())
                    .unwrap_or_else(|| "-".into()),
                status.wired_to.unwrap_or_else(|| "-".into()),
            );
        }
        AgentCommand::Install { agent } => {
            let executable = std::env::current_exe()
                .map_err(|error| OrkiaError::External(format!("current executable: {error}")))?;
            let change = install_agent(supported_agent(&agent)?, &executable)
                .map_err(OrkiaError::External)?;
            println!(
                "installed {} hook event(s): {}",
                change.added.len(),
                change.added.join(",")
            );
            for note in change.notes {
                println!("{note}");
            }
        }
        AgentCommand::Uninstall { agent } => {
            let change = uninstall_agent(supported_agent(&agent)?).map_err(OrkiaError::External)?;
            println!(
                "removed {} hook event(s): {}",
                change.removed.len(),
                change.removed.join(",")
            );
            for note in change.notes {
                println!("{note}");
            }
        }
        AgentCommand::Hook { .. } => unreachable!("hook commands are routed before composition"),
    }
    Ok(())
}

/// Hook processes are not guaranteed to inherit the agent workspace as their
/// current directory.  Codex supplies `cwd` in its native payload, so it is
/// the authoritative repository selector; the CLI argument remains a safe
/// fallback for providers that do not expose it.
fn run_agent_hook(agent_name: &str, fallback_repository: &Path) -> Result<()> {
    let agent = supported_agent(agent_name)?;
    let mut raw = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|error| OrkiaError::External(error.to_string()))?;
    let payload = parse_hook_payload(agent, &raw).map_err(OrkiaError::Invalid)?;
    let repository = payload
        .cwd
        .clone()
        .unwrap_or_else(|| fallback_repository.to_path_buf());
    let git = LibGit2Repository::open(&repository)?;
    let root = git_dir(&repository)?;
    let secrets = FileSecrets {
        root: root.join("orkia/keys"),
    };
    fs::create_dir_all(root.join("orkia/plans"))
        .map_err(|error| OrkiaError::External(error.to_string()))?;
    record_agent_hook(agent, payload, &git, &root, &secrets, &repository)
}

fn record_agent_hook(
    agent: SupportedAgent,
    payload: orkia_agents::HookPayload,
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
    repository: &Path,
) -> Result<()> {
    let hook_event = payload.event.clone();
    let external_session = payload.session_id.clone();
    let ledger = open_repository_ledger(git, root, secrets)?;
    let existing = git.ledger_store().read_all()?;
    let mut session = payload
        .session_id
        .as_deref()
        .and_then(|external| linked_agent_session(&existing, agent.name(), external));
    if payload.event == "SessionStart" && session.is_none() {
        let created = SessionId::new();
        let base_commit = git.head_commit()?;
        ledger.append(CaptureEvent::SessionStarted {
            session: created.clone(),
            origin: agent_origin(agent),
            base_commit: base_commit.clone(),
            objective: format!(
                "{} agent session {}",
                agent.name(),
                payload.session_id.as_deref().unwrap_or("unknown")
            ),
        })?;
        if let Some(external_session) = payload.session_id.clone() {
            ledger.append(CaptureEvent::AgentSessionLinked {
                agent: agent.name().into(),
                external_session: external_session.clone(),
                session: created.clone(),
            })?;
            ledger.append(CaptureEvent::AgentSessionSnapshot {
                agent: agent.name().into(),
                external_session,
                session: created.clone(),
                phase: AgentSnapshotPhase::Started,
                head_commit: base_commit,
                changed_paths: BTreeSet::new(),
                observed_paths: BTreeSet::new(),
                unknown_write: false,
            })?;
        }
        session = Some(created);
    }
    for event in normalize_hook(agent, &payload) {
        ledger.append(bind_agent_session(event, session.clone()))?;
    }
    ledger.append(CaptureEvent::AgentHook {
        agent: agent.name().into(),
        external_session: external_session.clone(),
        hook_event: hook_event.clone(),
        cwd: payload.cwd.map(|path| path.to_string_lossy().into_owned()),
        payload: payload.raw,
    })?;
    if matches!(hook_event.as_str(), "Stop" | "SessionEnd")
        && let (Some(session), Some(external_session)) = (session, external_session)
    {
        let checkpoint_event = record_agent_snapshot(
            git,
            &ledger,
            agent,
            &external_session,
            &session,
            &hook_event,
        )?;
        let events = git.ledger_store().read_all()?;
        let policy = load_repository_policy(repository)?;
        let checkpoint = format!("{}#{}", git.head_commit()?, checkpoint_event.unsigned.id.0);
        if let Some(plan) = derive_review_plan(
            git,
            &events,
            &session_base(&events, &session)?,
            &session,
            &policy,
            checkpoint.clone(),
        )? && plan.coverage_milli >= policy.minimum_coverage_milli
        {
            persist_review_plan(root, &ledger, &plan, &checkpoint)?;
        }
    }
    Ok(())
}

fn agent_origin(agent: SupportedAgent) -> CaptureOrigin {
    match agent {
        SupportedAgent::Codex => CaptureOrigin::Codex,
        SupportedAgent::ClaudeCode => CaptureOrigin::Claude,
        SupportedAgent::Gemini => CaptureOrigin::Gemini,
        SupportedAgent::Kimi => CaptureOrigin::Kimi,
        SupportedAgent::OpenCode => CaptureOrigin::OpenCode,
        SupportedAgent::Cursor => CaptureOrigin::Cursor,
        SupportedAgent::Droid => CaptureOrigin::Droid,
        SupportedAgent::Qwen => CaptureOrigin::Qwen,
    }
}

fn linked_agent_session(
    events: &[orkia_model::LedgerEvent],
    agent: &str,
    external_session: &str,
) -> Option<SessionId> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::AgentSessionLinked {
                agent: recorded_agent,
                external_session: recorded_external,
                session,
            } if recorded_agent == agent && recorded_external == external_session => {
                Some(session.clone())
            }
            _ => None,
        })
}

fn bind_agent_session(mut event: CaptureEvent, session: Option<SessionId>) -> CaptureEvent {
    if let CaptureEvent::AgentAction {
        session: bound_session,
        ..
    } = &mut event
    {
        *bound_session = session;
    }
    event
}

fn bind_known_agent_session(
    mut event: CaptureEvent,
    events: &[orkia_model::LedgerEvent],
) -> CaptureEvent {
    if let CaptureEvent::AgentAction {
        agent,
        external_session: Some(external_session),
        session,
        ..
    } = &mut event
    {
        *session = linked_agent_session(events, agent, external_session);
    }
    event
}

fn latest_transcript_revision<'a>(
    events: &'a [orkia_model::LedgerEvent],
    agent: &str,
    path: &str,
    encoding: &str,
) -> Option<&'a str> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::AgentTranscript {
                agent: recorded_agent,
                path: recorded_path,
                encoding: recorded_encoding,
                content: recorded_content,
            } if recorded_agent == agent
                && recorded_path == path
                && recorded_encoding == encoding =>
            {
                Some(recorded_content.as_str())
            }
            _ => None,
        })
}

fn record_agent_snapshot(
    git: &LibGit2Repository,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
    agent: SupportedAgent,
    external_session: &str,
    session: &SessionId,
    hook_event: &str,
) -> Result<orkia_model::LedgerEvent> {
    let events = git.ledger_store().read_all()?;
    let base_commit = session_base(&events, session)?;
    let changed_paths = git
        .changes_since(&base_commit)?
        .into_iter()
        .map(|change| change.path)
        .collect::<BTreeSet<_>>();
    let observed_paths = observed_agent_paths(&events, session);
    let unknown_write = changed_paths
        .iter()
        .any(|path| !observed_paths.contains(path));
    ledger.append(CaptureEvent::FilesObserved {
        read: BTreeSet::new(),
        modified: changed_paths.clone(),
        unknown_write,
    })?;
    ledger.append(CaptureEvent::AgentSessionSnapshot {
        agent: agent.name().into(),
        external_session: external_session.into(),
        session: session.clone(),
        phase: if hook_event == "SessionEnd" {
            AgentSnapshotPhase::Stopped
        } else {
            AgentSnapshotPhase::Checkpoint
        },
        head_commit: git.head_commit()?,
        changed_paths,
        observed_paths,
        unknown_write,
    })?;
    ledger.append(CaptureEvent::Checkpoint {
        commit: git.head_commit()?,
    })
}

fn session_base(events: &[orkia_model::LedgerEvent], session: &SessionId) -> Result<String> {
    events
        .iter()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::SessionStarted {
                session: recorded_session,
                base_commit,
                ..
            } if recorded_session == session => Some(base_commit.clone()),
            _ => None,
        })
        .ok_or_else(|| OrkiaError::NotFound(format!("agent session {}", session.0)))
}

fn observed_agent_paths(
    events: &[orkia_model::LedgerEvent],
    session: &SessionId,
) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|event| match &event.unsigned.event {
            CaptureEvent::AgentAction {
                session: Some(recorded_session),
                action: orkia_model::AgentActionKind::FileWrite { path, .. },
                ..
            } if recorded_session == session => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn handle_session(
    command: SessionCommand,
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
    repository: &Path,
) -> Result<()> {
    match command {
        SessionCommand::Start { objective, origin } => {
            let identity = load_identity(root, secrets)?;
            let base_commit = git.head_commit()?;
            let repository: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
            let state = SessionState {
                id: SessionId::new(),
                repository: repository.clone(),
                actor: identity.actor().clone(),
                base_commit: base_commit.clone(),
                observed_paths: BTreeSet::new(),
            };
            let ledger = Ledger::new(
                git.ledger_store(),
                SystemClock,
                state.repository.clone(),
                identity,
            );
            ledger.append(CaptureEvent::SessionStarted {
                session: state.id.clone(),
                origin: match origin {
                    Origin::Human => CaptureOrigin::Human,
                    Origin::Codex => CaptureOrigin::Codex,
                    Origin::Claude => CaptureOrigin::Claude,
                },
                base_commit,
                objective,
            })?;
            write_json(&root.join("orkia/session.json"), &state)?;
            println!("session {} started", state.id.0);
        }
        SessionCommand::Capture {
            provider,
            transcript,
        } => {
            let (state, ledger) = open_ledger(git, root, secrets)?;
            let body =
                fs::read_to_string(transcript).map_err(|e| OrkiaError::External(e.to_string()))?;
            let adapter: Box<dyn ProviderAdapter> = match provider {
                Provider::Codex => Box::new(CodexAdapter),
                Provider::Claude => Box::new(ClaudeAdapter),
            };
            for event in adapter.capture(&body) {
                ledger.append(event)?;
            }
            println!(
                "captured {} transcript bytes into session {}",
                body.len(),
                state.id.0
            );
        }
        SessionCommand::Agent { provider, args } => {
            let (mut state, ledger) = open_ledger(git, root, secrets)?;
            let before = changed_paths(git, &state.base_commit)?;
            let program = match provider {
                Provider::Codex => "codex",
                Provider::Claude => "claude",
            };
            let provider_args: Vec<String> = match provider {
                Provider::Codex => std::iter::once("exec".to_owned()).chain(args).collect(),
                Provider::Claude => args,
            };
            let output = std::process::Command::new(program)
                .current_dir(repository)
                .args(&provider_args)
                .output()
                .map_err(|e| OrkiaError::External(format!("cannot run {program}: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            ledger.append(CaptureEvent::Command {
                command: std::iter::once(program)
                    .chain(provider_args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
                exit_code: output.status.code(),
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            })?;
            let adapter: Box<dyn ProviderAdapter> = match provider {
                Provider::Codex => Box::new(CodexAdapter),
                Provider::Claude => Box::new(ClaudeAdapter),
            };
            for event in adapter.capture(&stdout) {
                ledger.append(event)?;
            }
            record_known_changes(git, root, &mut state, &ledger, &before)?;
            if !output.status.success() {
                return Err(OrkiaError::External(format!(
                    "{program} session failed: {stderr}"
                )));
            }
            println!("{} agent session captured", adapter.provider_name());
        }
        SessionCommand::Run { command } => {
            if command.is_empty() {
                return Err(OrkiaError::Invalid("command is empty".into()));
            }
            let (mut state, ledger) = open_ledger(git, root, secrets)?;
            let before = changed_paths(git, &state.base_commit)?;
            let output = std::process::Command::new(&command[0])
                .current_dir(repository)
                .args(&command[1..])
                .output()
                .map_err(|e| OrkiaError::External(e.to_string()))?;
            let passed = output.status.success();
            ledger.append(CaptureEvent::Command {
                command: command.join(" "),
                exit_code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })?;
            record_known_changes(git, root, &mut state, &ledger, &before)?;
            println!("command {}", if passed { "passed" } else { "failed" });
            if !passed {
                return Err(OrkiaError::External("captured command failed".into()));
            }
        }
        SessionCommand::Checkpoint => {
            let (state, ledger) = open_ledger(git, root, secrets)?;
            let changes = git.changes_since(&state.base_commit)?;
            let modified = changes
                .iter()
                .map(|change| change.path.clone())
                .collect::<BTreeSet<_>>();
            let unknown_write = modified
                .iter()
                .any(|path| !state.observed_paths.contains(path));
            ledger.append(CaptureEvent::FilesObserved {
                read: BTreeSet::new(),
                modified,
                unknown_write,
            })?;
            let commit = git.head_commit()?;
            let checkpoint_event = ledger.append(CaptureEvent::Checkpoint {
                commit: commit.clone(),
            })?;
            let policy = load_repository_policy(repository)?;
            let events = git.ledger_store().read_all()?;
            let checkpoint = format!("{commit}#{}", checkpoint_event.unsigned.id.0);
            match derive_review_plan(
                git,
                &events,
                &state.base_commit,
                &state.id,
                &policy,
                checkpoint.clone(),
            )? {
                Some(plan) if plan.coverage_milli >= policy.minimum_coverage_milli => {
                    persist_review_plan(root, &ledger, &plan, &checkpoint)?;
                    println!(
                        "checkpoint captured; automatic review plan {}: {} unit(s), {} atom(s), coverage {}‰",
                        plan.id.0,
                        plan.units.len(),
                        plan.atoms.len(),
                        plan.coverage_milli
                    );
                }
                Some(plan) => println!(
                    "checkpoint captured; automatic review withheld: causal coverage {}‰ is below required {}‰",
                    plan.coverage_milli, policy.minimum_coverage_milli
                ),
                None => println!("checkpoint captured; no changed atoms to review"),
            }
        }
        SessionCommand::Close => {
            let (state, ledger) = open_ledger(git, root, secrets)?;
            ledger.append(CaptureEvent::SessionClosed { session: state.id })?;
            let _ = fs::remove_file(root.join("orkia/session.json"));
            println!("session closed");
        }
    }
    Ok(())
}
fn handle_review(
    command: ReviewCommand,
    git: &LibGit2Repository,
    root: &Path,
    repository: &Path,
    secrets: &FileSecrets,
) -> Result<()> {
    match command {
        ReviewCommand::Plan { checkpoint } => {
            let events = git.ledger_store().read_all()?;
            let base = latest_session_base(&events)?;
            let session = latest_session_id(&events)?;
            let policy = load_repository_policy(repository)?;
            let checkpoint = checkpoint.unwrap_or(git.head_commit()?);
            let plan =
                derive_review_plan(git, &events, &base, &session, &policy, checkpoint.clone())?
                    .ok_or_else(|| OrkiaError::Invalid("no changed atoms to review".into()))?;
            if plan.coverage_milli < policy.minimum_coverage_milli {
                return Err(OrkiaError::Policy(format!(
                    "causal coverage {} is below required {}; no automatic stack was created",
                    plan.coverage_milli, policy.minimum_coverage_milli
                )));
            }
            let ledger = open_repository_ledger(git, root, secrets)?;
            persist_review_plan(root, &ledger, &plan, &checkpoint)?;
            println!(
                "review plan {}: {} unit(s), {} atom(s), coverage {}‰",
                plan.id.0,
                plan.units.len(),
                plan.atoms.len(),
                plan.coverage_milli
            );
        }
        ReviewCommand::Show { plan } => {
            let value: ReviewPlan =
                read_json(&root.join("orkia/plans").join(format!("{plan}.json")))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .map_err(|e| OrkiaError::Invalid(e.to_string()))?
            );
        }
        ReviewCommand::Merge {
            plan: plan_id,
            units,
        } => {
            let path = root.join("orkia/plans").join(format!("{plan_id}.json"));
            let current: ReviewPlan = read_json(&path)?;
            let units = units
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    value
                        .trim()
                        .parse::<uuid::Uuid>()
                        .map(orkia_model::ReviewUnitId)
                        .map_err(|error| {
                            OrkiaError::Invalid(format!("invalid review unit id: {error}"))
                        })
                })
                .collect::<Result<BTreeSet<_>>>()?;
            let revised = orkia_review::apply_correction(
                &current,
                orkia_review::ReviewerCorrection::Merge {
                    units,
                    reason: "reviewer merge via CLI".into(),
                },
            )?;
            let destination = root
                .join("orkia/plans")
                .join(format!("{}.json", revised.id.0));
            write_json(&destination, &revised)?;
            open_repository_ledger(git, root, secrets)?.append(
                CaptureEvent::ReviewPlanRevised {
                    plan: revised.id.clone(),
                    revision: revised.revision,
                    reason: "reviewer merge via CLI".into(),
                },
            )?;
            println!("review plan revision {} created", revised.id.0);
        }
        ReviewCommand::Project { plan: plan_id } => {
            let plan: ReviewPlan =
                read_json(&root.join("orkia/plans").join(format!("{plan_id}.json")))?;
            let base = latest_session_base(&git.ledger_store().read_all()?)?;
            let mut commits = BTreeMap::new();
            for projection in orkia_forge::projections(&plan, &base)? {
                let parent = commits
                    .get(&projection.base)
                    .cloned()
                    .unwrap_or(projection.base.clone());
                let unit = plan
                    .units
                    .iter()
                    .find(|unit| unit.id == projection.unit)
                    .ok_or_else(|| OrkiaError::NotFound("projected review unit".into()))?;
                let paths = unit
                    .atoms
                    .iter()
                    .filter_map(|atom| plan.atom_paths.get(atom).cloned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let commit = git.project_paths(&projection.branch, &parent, &paths)?;
                commits.insert(projection.branch.clone(), commit.clone());
                println!("{} -> {}", projection.branch, commit);
            }
        }
        ReviewCommand::Publish {
            plan: plan_id,
            github_owner,
            github_repository,
            base,
            remote,
        } => {
            let token = std::env::var("ORKIA_GITHUB_INSTALLATION_TOKEN")
                .map_err(|_| OrkiaError::NotFound("ORKIA_GITHUB_INSTALLATION_TOKEN".into()))?;
            let plan: ReviewPlan =
                read_json(&root.join("orkia/plans").join(format!("{plan_id}.json")))?;
            let github = GitHubApp::new(github_owner, github_repository, token)?;
            for projection in orkia_forge::projections(&plan, &base)? {
                git.push_branch(&remote, &projection.branch)?;
                let url = github.publish(&projection)?;
                println!("{}", url);
            }
        }
    }
    Ok(())
}

/// Derive a review only from evidence captured for one session.  A historical
/// unknown write in another session must never contaminate a new checkpoint.
/// Returning `None` means that the checkpoint has no semantic change atoms.
fn derive_review_plan(
    git: &LibGit2Repository,
    events: &[orkia_model::LedgerEvent],
    base_commit: &str,
    session: &SessionId,
    policy: &orkia_model::RepositoryPolicy,
    checkpoint: String,
) -> Result<Option<ReviewPlan>> {
    let changes = git.changes_since(base_commit)?;
    let scoped_events = session_events(events, session)?;
    let source_events = scoped_events
        .iter()
        .map(|event| event.unsigned.id.clone())
        .collect::<BTreeSet<_>>();
    let atoms = changes
        .iter()
        .flat_map(|change| {
            extract_atoms(&ChangedFile {
                path: change.path.clone(),
                changed_start: change.changed_start,
                changed_end: change.changed_end,
                content: change.new_content.clone(),
                source_events: source_events.clone(),
            })
        })
        .collect::<Vec<_>>();
    if atoms.is_empty() {
        return Ok(None);
    }
    let coverage_milli = causal_coverage_milli(&changes, &scoped_events);
    Ok(Some(plan(PlanningInput {
        checkpoint,
        dependencies: infer_dependencies(&atoms),
        atoms,
        coverage_milli,
        minimum_coverage_milli: policy.minimum_coverage_milli,
        minimum_confidence_milli: policy.minimum_confidence_milli,
        source_events,
    })))
}

fn persist_review_plan(
    root: &Path,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
    plan: &ReviewPlan,
    checkpoint: &str,
) -> Result<()> {
    let destination = root.join("orkia/plans").join(format!("{}.json", plan.id.0));
    write_json(&destination, plan)?;
    ledger.append(CaptureEvent::ReviewPlanCreated {
        plan: plan.id.clone(),
        checkpoint: checkpoint.into(),
        atom_count: plan.atoms.len() as u32,
        coverage_milli: plan.coverage_milli,
    })?;
    Ok(())
}

fn load_repository_policy(repository: &Path) -> Result<orkia_model::RepositoryPolicy> {
    let path = repository.join("orkia.toml");
    if path.exists() {
        orkia_policy::load(&path)
    } else {
        Ok(orkia_model::RepositoryPolicy::default())
    }
}
fn open_ledger(
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
) -> Result<(SessionState, Ledger<orkia_git::GitLedgerStore, SystemClock>)> {
    let state = read_state(root)?;
    let identity = Identity::load(secrets, "identity", state.actor.clone())?
        .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
    Ok((
        state.clone(),
        Ledger::new(git.ledger_store(), SystemClock, state.repository, identity),
    ))
}
fn open_repository_ledger(
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
) -> Result<Ledger<orkia_git::GitLedgerStore, SystemClock>> {
    let events = git.ledger_store().read_all()?;
    let repository = events
        .first()
        .map(|event| event.unsigned.repository.clone())
        .unwrap_or(read_json(&root.join("orkia/repository.json"))?);
    let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
    let identity = Identity::load(secrets, "identity", actor)?
        .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
    Ok(Ledger::new(
        git.ledger_store(),
        SystemClock,
        repository,
        identity,
    ))
}
fn changed_paths(git: &LibGit2Repository, base: &str) -> Result<BTreeSet<String>> {
    Ok(git
        .changes_since(base)?
        .into_iter()
        .map(|change| change.path)
        .collect())
}
fn record_known_changes(
    git: &LibGit2Repository,
    root: &Path,
    state: &mut SessionState,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
    before: &BTreeSet<String>,
) -> Result<()> {
    let modified = changed_paths(git, &state.base_commit)?
        .difference(before)
        .cloned()
        .collect::<BTreeSet<_>>();
    state.observed_paths.extend(modified.iter().cloned());
    ledger.append(CaptureEvent::FilesObserved {
        read: BTreeSet::new(),
        modified,
        unknown_write: false,
    })?;
    write_json(&root.join("orkia/session.json"), state)
}
fn latest_session_base(events: &[orkia_model::LedgerEvent]) -> Result<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::SessionStarted { base_commit, .. } => Some(base_commit.clone()),
            _ => None,
        })
        .ok_or_else(|| OrkiaError::NotFound("session start event".into()))
}

fn latest_session_id(events: &[orkia_model::LedgerEvent]) -> Result<SessionId> {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::SessionStarted { session, .. } => Some(session.clone()),
            _ => None,
        })
        .ok_or_else(|| OrkiaError::NotFound("session start event".into()))
}

fn session_events<'a>(
    events: &'a [orkia_model::LedgerEvent],
    session: &SessionId,
) -> Result<Vec<&'a orkia_model::LedgerEvent>> {
    let start = events
        .iter()
        .rposition(|event| {
            matches!(
                &event.unsigned.event,
                CaptureEvent::SessionStarted { session: recorded, .. } if recorded == session
            )
        })
        .ok_or_else(|| OrkiaError::NotFound(format!("session {}", session.0)))?;
    Ok(events[start..].iter().collect())
}

fn causal_coverage_milli(
    changes: &[orkia_git::WorkingFileChange],
    events: &[&orkia_model::LedgerEvent],
) -> u16 {
    let observed = events
        .iter()
        .flat_map(|event| match &event.unsigned.event {
            CaptureEvent::FilesObserved { modified, .. } => modified.clone(),
            CaptureEvent::AgentAction {
                session: Some(_),
                action: orkia_model::AgentActionKind::FileWrite { path, .. },
                ..
            } => BTreeSet::from([path.clone()]),
            _ => BTreeSet::new(),
        })
        .collect::<BTreeSet<_>>();
    let every_change_observed = changes.iter().all(|change| observed.contains(&change.path));
    let unknown_write = events.iter().any(|event| {
        matches!(
            event.unsigned.event,
            CaptureEvent::FilesObserved {
                unknown_write: true,
                ..
            }
        )
    });
    let unbound_agent_action = events.iter().any(|event| {
        matches!(
            event.unsigned.event,
            CaptureEvent::AgentAction { session: None, .. }
        )
    });
    if every_change_observed && !unknown_write && !unbound_agent_action {
        1000
    } else {
        0
    }
}

fn run_validations(
    repository: &Path,
    policy: &orkia_model::RepositoryPolicy,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
) -> Result<Vec<orkia_model::ValidationResult>> {
    let mut results = Vec::new();
    for command in &policy.validation_commands {
        let output = std::process::Command::new("sh")
            .arg("-lc")
            .arg(command)
            .current_dir(repository)
            .output()
            .map_err(|error| OrkiaError::External(format!("validation {command}: {error}")))?;
        let result = orkia_model::ValidationResult {
            command: command.clone(),
            passed: output.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        };
        ledger.append(CaptureEvent::Validation {
            command: result.command.clone(),
            passed: result.passed,
            output: result.output.clone(),
        })?;
        results.push(result);
    }
    Ok(results)
}
fn load_identity(root: &Path, secrets: &FileSecrets) -> Result<Identity> {
    let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
    Identity::load(secrets, "identity", actor)?
        .ok_or_else(|| OrkiaError::NotFound("run `orkia identity init` first".into()))
}
fn read_state(root: &Path) -> Result<SessionState> {
    read_json(&root.join("orkia/session.json"))
}
fn git_dir(path: &Path) -> Result<PathBuf> {
    Ok(git2::Repository::open(path)
        .map_err(|e| OrkiaError::External(e.to_string()))?
        .path()
        .to_path_buf())
}
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_vec_pretty(value).map_err(|e| OrkiaError::Invalid(e.to_string()))?;
    fs::write(path, text).map_err(|e| OrkiaError::External(e.to_string()))
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &fs::read(path).map_err(|e| OrkiaError::NotFound(format!("{}: {e}", path.display())))?,
    )
    .map_err(|e| OrkiaError::Integrity(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_model::{ActorId, EventId, LedgerEvent, UnsignedEvent};
    use time::OffsetDateTime;

    fn ledger_event(event: CaptureEvent) -> LedgerEvent {
        LedgerEvent {
            unsigned: UnsignedEvent {
                id: EventId::new(),
                repository: RepositoryId::new(),
                actor: ActorId::new(),
                occurred_at: OffsetDateTime::UNIX_EPOCH,
                previous_hash: None,
                event,
            },
            hash: String::new(),
            signature: String::new(),
        }
    }

    #[test]
    fn external_agent_session_resolves_to_the_original_orkia_session() {
        let session = SessionId::new();
        let events = vec![ledger_event(CaptureEvent::AgentSessionLinked {
            agent: "codex".into(),
            external_session: "provider-session".into(),
            session: session.clone(),
        })];
        assert_eq!(
            linked_agent_session(&events, "codex", "provider-session"),
            Some(session)
        );
        assert_eq!(linked_agent_session(&events, "codex", "other"), None);
    }

    #[test]
    fn binding_an_agent_action_carries_the_resolved_session() {
        let session = SessionId::new();
        let bound = bind_agent_session(
            CaptureEvent::AgentAction {
                agent: "codex".into(),
                external_session: Some("provider-session".into()),
                session: None,
                action: orkia_model::AgentActionKind::Prompt {
                    content: "implement the parser".into(),
                },
            },
            Some(session.clone()),
        );
        assert!(matches!(
            bound,
            CaptureEvent::AgentAction { session: Some(found), .. } if found == session
        ));
    }

    #[test]
    fn transcript_action_reuses_a_known_live_session() {
        let session = SessionId::new();
        let known = vec![ledger_event(CaptureEvent::AgentSessionLinked {
            agent: "codex".into(),
            external_session: "provider-session".into(),
            session: session.clone(),
        })];
        let bound = bind_known_agent_session(
            CaptureEvent::AgentAction {
                agent: "codex".into(),
                external_session: Some("provider-session".into()),
                session: None,
                action: orkia_model::AgentActionKind::Turn {
                    model: None,
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cost_micros: None,
                    text: None,
                    thinking: None,
                },
            },
            &known,
        );
        assert!(matches!(
            bound,
            CaptureEvent::AgentAction { session: Some(found), .. } if found == session
        ));
    }

    #[test]
    fn latest_transcript_revision_tracks_the_most_recent_document() {
        let events = vec![ledger_event(CaptureEvent::AgentTranscript {
            agent: "codex".into(),
            path: "/history/rollout.jsonl".into(),
            encoding: "utf-8".into(),
            content: "source".into(),
        })];
        assert_eq!(
            latest_transcript_revision(&events, "codex", "/history/rollout.jsonl", "utf-8"),
            Some("source")
        );
    }

    fn initialized_session_repository(
        temp: &tempfile::TempDir,
    ) -> (PathBuf, LibGit2Repository, PathBuf, FileSecrets, Identity) {
        let repository = temp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let repo = git2::Repository::init(&repository).unwrap();
        std::fs::create_dir_all(repository.join("src")).unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("src/lib.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Orkia test", "orkia@example.test").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);

        let git = LibGit2Repository::open(&repository).unwrap();
        let root = git_dir(&repository).unwrap();
        std::fs::create_dir_all(root.join("orkia/plans")).unwrap();
        let secrets = FileSecrets {
            root: root.join("orkia/keys"),
        };
        let identity = Identity::generate("automatic review integration test");
        identity.save(&secrets, "identity").unwrap();
        write_json(&root.join("orkia/actor.json"), identity.actor()).unwrap();
        write_json(&root.join("orkia/repository.json"), &RepositoryId::new()).unwrap();
        (repository, git, root, secrets, identity)
    }

    #[test]
    fn checkpoint_persists_atoms_and_a_signed_automatic_review_plan() {
        let temp = tempfile::tempdir().unwrap();
        let (repository, git, root, secrets, identity) = initialized_session_repository(&temp);
        handle_session(
            SessionCommand::Start {
                objective: "add a feature".into(),
                origin: Origin::Human,
            },
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();
        handle_session(
            SessionCommand::Run {
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf 'pub fn feature() {}\\n' > src/lib.rs".into(),
                ],
            },
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();
        handle_session(
            SessionCommand::Checkpoint,
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();

        let plans = std::fs::read_dir(root.join("orkia/plans"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(plans.len(), 1);
        let plan: ReviewPlan = read_json(&plans[0]).unwrap();
        assert_eq!(plan.coverage_milli, 1000);
        assert!(
            !plan.atoms.is_empty(),
            "semantic atoms are persisted with the plan"
        );
        assert!(git.ledger_store().read_all().unwrap().iter().any(|event| {
            matches!(event.unsigned.event, CaptureEvent::ReviewPlanCreated { .. })
        }));
        let events = git.ledger_store().read_all().unwrap();
        let actors = BTreeMap::from([(identity.actor().id.clone(), identity.actor().clone())]);
        verify_chain(&events, &actors).unwrap();
    }

    #[test]
    fn checkpoint_withholds_automatic_review_when_a_write_was_not_captured() {
        let temp = tempfile::tempdir().unwrap();
        let (repository, git, root, secrets, _) = initialized_session_repository(&temp);
        handle_session(
            SessionCommand::Start {
                objective: "change without capture".into(),
                origin: Origin::Human,
            },
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn unobserved() {}\n").unwrap();
        handle_session(
            SessionCommand::Checkpoint,
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_dir(root.join("orkia/plans")).unwrap().count(),
            0
        );
        assert!(!git.ledger_store().read_all().unwrap().iter().any(|event| {
            matches!(event.unsigned.event, CaptureEvent::ReviewPlanCreated { .. })
        }));
    }

    #[test]
    fn completed_codex_hook_session_creates_a_review_from_its_captured_write() {
        let temp = tempfile::tempdir().unwrap();
        let (repository, git, root, secrets, _) = initialized_session_repository(&temp);
        let start = orkia_agents::HookPayload {
            session_id: Some("codex-automatic-plan".into()),
            event: "SessionStart".into(),
            cwd: Some(repository.clone()),
            prompt: None,
            raw: serde_json::json!({"hook_event_name":"SessionStart"}),
        };
        record_agent_hook(
            SupportedAgent::Codex,
            start,
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();
        std::fs::write(repository.join("src/lib.rs"), "pub fn captured() {}\n").unwrap();
        let write = orkia_agents::HookPayload {
            session_id: Some("codex-automatic-plan".into()),
            event: "PostToolUse".into(),
            cwd: Some(repository.clone()),
            prompt: None,
            raw: serde_json::json!({
                "hook_event_name":"PostToolUse",
                "tool_name":"apply_patch",
                "tool_input":{"command":"*** Begin Patch\n*** Update File: src/lib.rs\n-old\n+new\n*** End Patch"}
            }),
        };
        record_agent_hook(
            SupportedAgent::Codex,
            write,
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();
        let stop = orkia_agents::HookPayload {
            session_id: Some("codex-automatic-plan".into()),
            event: "Stop".into(),
            cwd: Some(repository.clone()),
            prompt: None,
            raw: serde_json::json!({"hook_event_name":"Stop"}),
        };
        record_agent_hook(
            SupportedAgent::Codex,
            stop,
            &git,
            &root,
            &secrets,
            &repository,
        )
        .unwrap();

        let plans = std::fs::read_dir(root.join("orkia/plans")).unwrap().count();
        assert_eq!(
            plans, 1,
            "the completed agent session is planned automatically"
        );
        assert!(git.ledger_store().read_all().unwrap().iter().any(|event| {
            matches!(event.unsigned.event, CaptureEvent::ReviewPlanCreated { .. })
        }));
    }

    #[test]
    fn growing_codex_transcript_imports_only_the_new_action_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git2::Repository::init(&repository).unwrap();
        let git = LibGit2Repository::open(&repository).unwrap();
        let root = git_dir(&repository).unwrap();
        let secrets = FileSecrets {
            root: root.join("orkia/keys"),
        };
        let identity = Identity::generate("transcript integration test");
        identity.save(&secrets, "identity").unwrap();
        write_json(&root.join("orkia/actor.json"), identity.actor()).unwrap();
        write_json(&root.join("orkia/repository.json"), &RepositoryId::new()).unwrap();

        let sessions = temp.path().join("codex-sessions");
        std::fs::create_dir(&sessions).unwrap();
        let transcript = sessions.join("rollout-growing.jsonl");
        let initial = concat!(
            r#"{"type":"session_meta","payload":{"id":"growing-session"}}"#,
            "\n",
            r#"{"type":"message","payload":{"role":"user","content":[{"type":"input_text","text":"first action"}]}}"#,
            "\n"
        );
        std::fs::write(&transcript, initial).unwrap();

        handle_agent(
            AgentCommand::Import {
                agent: "codex".into(),
                source: Some(sessions.clone()),
            },
            &git,
            &root,
            &secrets,
        )
        .unwrap();
        let after_initial = git.ledger_store().read_all().unwrap();
        assert_eq!(after_initial.len(), 2, "raw transcript plus first prompt");

        std::fs::write(
            transcript,
            format!(
                "{initial}{}\n",
                r#"{"type":"message","payload":{"role":"user","content":[{"type":"input_text","text":"second action"}]}}"#
            ),
        )
        .unwrap();
        handle_agent(
            AgentCommand::Import {
                agent: "codex".into(),
                source: Some(sessions),
            },
            &git,
            &root,
            &secrets,
        )
        .unwrap();

        let events = git.ledger_store().read_all().unwrap();
        let prompts: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.unsigned.event {
                CaptureEvent::AgentAction {
                    action: orkia_model::AgentActionKind::Prompt { content },
                    ..
                } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, ["first action", "second action"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.unsigned.event,
                    CaptureEvent::AgentTranscript { .. }
                ))
                .count(),
            2,
            "both immutable transcript revisions are retained"
        );
        let actors = BTreeMap::from([(identity.actor().id.clone(), identity.actor().clone())]);
        verify_chain(&events, &actors).unwrap();
    }

    #[test]
    fn changed_opencode_sidecar_is_versioned_without_replaying_its_actions() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git2::Repository::init(&repository).unwrap();
        let git = LibGit2Repository::open(&repository).unwrap();
        let root = git_dir(&repository).unwrap();
        let secrets = FileSecrets {
            root: root.join("orkia/keys"),
        };
        let identity = Identity::generate("opencode transcript integration test");
        identity.save(&secrets, "identity").unwrap();
        write_json(&root.join("orkia/actor.json"), identity.actor()).unwrap();
        write_json(&root.join("orkia/repository.json"), &RepositoryId::new()).unwrap();

        let storage = temp.path().join("opencode-storage");
        let session = storage.join("session/global/ses_1.json");
        let message = storage.join("message/ses_1/msg_1.json");
        let part = storage.join("part/msg_1/prt_1.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::create_dir_all(message.parent().unwrap()).unwrap();
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&session, r#"{"id":"ses_1"}"#).unwrap();
        std::fs::write(&message, r#"{"role":"user"}"#).unwrap();
        std::fs::write(&part, r#"{"type":"text","text":"first action"}"#).unwrap();

        handle_agent(
            AgentCommand::Import {
                agent: "opencode".into(),
                source: Some(storage.clone()),
            },
            &git,
            &root,
            &secrets,
        )
        .unwrap();
        std::fs::write(&part, r#"{"type":"text","text":"rewritten action"}"#).unwrap();
        handle_agent(
            AgentCommand::Import {
                agent: "opencode".into(),
                source: Some(storage),
            },
            &git,
            &root,
            &secrets,
        )
        .unwrap();

        let events = git.ledger_store().read_all().unwrap();
        let prompts: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.unsigned.event {
                CaptureEvent::AgentAction {
                    action: orkia_model::AgentActionKind::Prompt { content },
                    ..
                } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, ["first action"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.unsigned.event,
                    CaptureEvent::AgentTranscript { .. }
                ))
                .count(),
            2
        );
        let actors = BTreeMap::from([(identity.actor().id.clone(), identity.actor().clone())]);
        verify_chain(&events, &actors).unwrap();
    }
}
