//! Terminal composition root for Orkia.

use clap::{Parser, Subcommand, ValueEnum};
use orkia_capture::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use orkia_git::LibGit2Repository;
use orkia_github::GitHubApp;
use orkia_identity::Identity;
use orkia_ledger::{Ledger, SystemClock, verify_chain};
use orkia_model::{
    Actor, CaptureEvent, CaptureOrigin, OrkiaError, RepositoryId, Result, ReviewPlan, SessionId,
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
    Ledger {
        #[command(subcommand)]
        command: LedgerCommand,
    },
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    Integrate {
        #[arg(long, default_value = "main")]
        branch: String,
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
    if let Err(error) = run(Cli::parse()) {
        eprintln!("orkia: {error}");
        std::process::exit(1);
    }
}
fn run(cli: Cli) -> Result<()> {
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
            println!("identity {} initialized", identity.actor().id.0);
        }
        Command::Session { command } => {
            handle_session(command, &git, &root, &secrets, &cli.repository)?
        }
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
        Command::Integrate { branch } => {
            println!(
                "integration proposal for {branch} is ready for configured policy and forge checks"
            );
        }
    }
    Ok(())
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
            let state = SessionState {
                id: SessionId::new(),
                repository: RepositoryId::new(),
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
            ledger.append(CaptureEvent::Checkpoint {
                commit: git.head_commit()?,
            })?;
            println!("checkpoint captured");
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
    _repository: &Path,
    secrets: &FileSecrets,
) -> Result<()> {
    match command {
        ReviewCommand::Plan { checkpoint } => {
            let events = git.ledger_store().read_all()?;
            let base = latest_session_base(&events)?;
            let changes = git.changes_since(&base)?;
            let source_events: BTreeSet<orkia_model::EventId> = events
                .iter()
                .map(|event| event.unsigned.id.clone())
                .collect();
            let observed = observed_paths(&events);
            let coverage_milli = if changes.iter().all(|change| observed.contains(&change.path))
                && !events.iter().any(|event| {
                    matches!(
                        event.unsigned.event,
                        CaptureEvent::FilesObserved {
                            unknown_write: true,
                            ..
                        }
                    )
                }) {
                1000
            } else {
                0
            };
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
            let plan = plan(PlanningInput {
                checkpoint: checkpoint.unwrap_or(git.head_commit()?),
                dependencies: infer_dependencies(&atoms),
                atoms,
                coverage_milli,
                minimum_coverage_milli: 950,
                minimum_confidence_milli: 800,
                source_events,
            });
            let destination = root.join("orkia/plans").join(format!("{}.json", plan.id.0));
            write_json(&destination, &plan)?;
            println!(
                "review plan {}: {} unit(s), coverage {}‰",
                plan.id.0,
                plan.units.len(),
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
        .ok_or_else(|| OrkiaError::NotFound("ledger events".into()))?
        .unsigned
        .repository
        .clone();
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
fn observed_paths(events: &[orkia_model::LedgerEvent]) -> BTreeSet<String> {
    events
        .iter()
        .flat_map(|event| match &event.unsigned.event {
            CaptureEvent::FilesObserved { modified, .. } => modified.clone(),
            _ => BTreeSet::new(),
        })
        .collect()
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
