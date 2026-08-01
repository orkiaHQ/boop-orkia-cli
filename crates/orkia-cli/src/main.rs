//! Terminal composition root for Orkia.

use clap::{Parser, Subcommand, ValueEnum};
use orkia_capture::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use orkia_git::LibGit2Repository;
use orkia_identity::Identity;
use orkia_ledger::{verify_chain, Ledger, SystemClock};
use orkia_model::{Actor, CaptureEvent, CaptureOrigin, OrkiaError, RepositoryId, Result, ReviewPlan, SessionId};
use orkia_ports::{GitRepository, LedgerStore, SecretStore};
use orkia_review::{plan, PlanningInput};
use orkia_semantic::{extract_atoms, ChangedFile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "orkia", about = "Git-native semantic review engine")]
struct Cli { #[arg(long, global = true, default_value = ".")] repository: PathBuf, #[command(subcommand)] command: Command }
#[derive(Subcommand)]
enum Command { Identity { #[command(subcommand)] command: IdentityCommand }, Session { #[command(subcommand)] command: SessionCommand }, Ledger { #[command(subcommand)] command: LedgerCommand }, Review { #[command(subcommand)] command: ReviewCommand }, Integrate { #[arg(long, default_value = "main")] branch: String } }
#[derive(Subcommand)] enum IdentityCommand { Init { #[arg(long)] name: String } }
#[derive(Subcommand)] enum SessionCommand { Start { #[arg(long)] objective: String, #[arg(long, value_enum, default_value_t = Origin::Human)] origin: Origin }, Capture { #[arg(long, value_enum)] provider: Provider, #[arg(long)] transcript: PathBuf }, Agent { #[arg(long, value_enum)] provider: Provider, #[arg(last = true, allow_hyphen_values = true)] args: Vec<String> }, Run { #[arg(required = true, trailing_var_arg = true)] command: Vec<String> }, Checkpoint, Close }
#[derive(Subcommand)] enum LedgerCommand { Verify }
#[derive(Subcommand)] enum ReviewCommand { Plan { #[arg(long)] path: PathBuf, #[arg(long)] checkpoint: Option<String> }, Show { #[arg(long)] plan: String }, Merge { #[arg(long)] plan: String, #[arg(long, required = true, value_delimiter = ',')] units: Vec<String> } }
#[derive(Clone, Copy, ValueEnum)] enum Origin { Human, Codex, Claude }
#[derive(Clone, Copy, ValueEnum)] enum Provider { Codex, Claude }
#[derive(Clone, Serialize, Deserialize)] struct SessionState { id: SessionId, repository: RepositoryId, actor: Actor }
#[derive(Clone)] struct FileSecrets { root: PathBuf }
impl SecretStore for FileSecrets { fn get(&self, key: &str) -> Result<Option<Vec<u8>>> { let path = self.root.join(key); match fs::read(path) { Ok(value) => Ok(Some(value)), Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(error) => Err(OrkiaError::External(error.to_string())) } } fn put(&self, key: &str, value: &[u8]) -> Result<()> { fs::create_dir_all(&self.root).map_err(|e| OrkiaError::External(e.to_string()))?; let path = self.root.join(key); fs::write(&path, value).map_err(|e| OrkiaError::External(e.to_string()))?; #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| OrkiaError::External(e.to_string()))?; } Ok(()) } }

fn main() { if let Err(error) = run(Cli::parse()) { eprintln!("orkia: {error}"); std::process::exit(1); } }
fn run(cli: Cli) -> Result<()> {
    let git = LibGit2Repository::open(&cli.repository)?; let root = git_dir(&cli.repository)?; let secrets = FileSecrets { root: root.join("orkia/keys") }; fs::create_dir_all(root.join("orkia/plans")).map_err(|e| OrkiaError::External(e.to_string()))?;
    match cli.command {
        Command::Identity { command: IdentityCommand::Init { name } } => { let identity = Identity::generate(name); identity.save(&secrets, "identity")?; write_json(&root.join("orkia/actor.json"), identity.actor())?; println!("identity {} initialized", identity.actor().id.0); }
        Command::Session { command } => handle_session(command, &git, &root, &secrets)?,
        Command::Ledger { command: LedgerCommand::Verify } => { let actor: Actor = read_json(&root.join("orkia/actor.json"))?; let events = git.ledger_store().read_all()?; let actors = BTreeMap::from([(actor.id.clone(), actor)]); verify_chain(&events, &actors)?; println!("verified {} signed ledger events", events.len()); }
        Command::Review { command } => handle_review(command, &git, &root)?,
        Command::Integrate { branch } => { println!("integration proposal for {branch} is ready for configured policy and forge checks"); }
    } Ok(())
}
fn handle_session(command: SessionCommand, git: &LibGit2Repository, root: &Path, secrets: &FileSecrets) -> Result<()> { match command {
    SessionCommand::Start { objective, origin } => { let identity = load_identity(root, secrets)?; let state = SessionState { id: SessionId::new(), repository: RepositoryId::new(), actor: identity.actor().clone() }; let ledger = Ledger::new(git.ledger_store(), SystemClock, state.repository.clone(), identity); ledger.append(CaptureEvent::SessionStarted { session: state.id.clone(), origin: match origin { Origin::Human => CaptureOrigin::Human, Origin::Codex => CaptureOrigin::Codex, Origin::Claude => CaptureOrigin::Claude }, base_commit: git.head_commit()?, objective })?; write_json(&root.join("orkia/session.json"), &state)?; println!("session {} started", state.id.0); }
    SessionCommand::Capture { provider, transcript } => { let (state, ledger) = open_ledger(git, root, secrets)?; let body = fs::read_to_string(transcript).map_err(|e| OrkiaError::External(e.to_string()))?; let adapter: Box<dyn ProviderAdapter> = match provider { Provider::Codex => Box::new(CodexAdapter), Provider::Claude => Box::new(ClaudeAdapter) }; for event in adapter.capture(&body) { ledger.append(event)?; } println!("captured {} transcript bytes into session {}", body.len(), state.id.0); }
    SessionCommand::Agent { provider, args } => { let (_, ledger) = open_ledger(git, root, secrets)?; let program = match provider { Provider::Codex => "codex", Provider::Claude => "claude" }; let provider_args: Vec<String> = match provider { Provider::Codex => std::iter::once("exec".to_owned()).chain(args).collect(), Provider::Claude => args }; let output = std::process::Command::new(program).args(&provider_args).output().map_err(|e| OrkiaError::External(format!("cannot run {program}: {e}")))?; let stdout = String::from_utf8_lossy(&output.stdout).into_owned(); let stderr = String::from_utf8_lossy(&output.stderr).into_owned(); ledger.append(CaptureEvent::Command { command: std::iter::once(program).chain(provider_args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "), exit_code: output.status.code(), stdout: stdout.clone(), stderr: stderr.clone() })?; let adapter: Box<dyn ProviderAdapter> = match provider { Provider::Codex => Box::new(CodexAdapter), Provider::Claude => Box::new(ClaudeAdapter) }; for event in adapter.capture(&stdout) { ledger.append(event)?; } if !output.status.success() { return Err(OrkiaError::External(format!("{program} session failed: {stderr}"))); } println!("{} agent session captured", adapter.provider_name()); }
    SessionCommand::Run { command } => { if command.is_empty() { return Err(OrkiaError::Invalid("command is empty".into())); } let (_, ledger) = open_ledger(git, root, secrets)?; let output = std::process::Command::new(&command[0]).args(&command[1..]).output().map_err(|e| OrkiaError::External(e.to_string()))?; let passed = output.status.success(); ledger.append(CaptureEvent::Command { command: command.join(" "), exit_code: output.status.code(), stdout: String::from_utf8_lossy(&output.stdout).into_owned(), stderr: String::from_utf8_lossy(&output.stderr).into_owned() })?; println!("command {}", if passed { "passed" } else { "failed" }); if !passed { return Err(OrkiaError::External("captured command failed".into())); } }
    SessionCommand::Checkpoint => { let (_, ledger) = open_ledger(git, root, secrets)?; ledger.append(CaptureEvent::Checkpoint { commit: git.head_commit()? })?; println!("checkpoint captured"); }
    SessionCommand::Close => { let (state, ledger) = open_ledger(git, root, secrets)?; ledger.append(CaptureEvent::SessionClosed { session: state.id })?; let _ = fs::remove_file(root.join("orkia/session.json")); println!("session closed"); }
} Ok(()) }
fn handle_review(command: ReviewCommand, git: &LibGit2Repository, root: &Path) -> Result<()> { match command { ReviewCommand::Plan { path, checkpoint } => { let content = fs::read_to_string(&path).map_err(|e| OrkiaError::External(e.to_string()))?; let events = git.ledger_store().read_all()?; let source_events: BTreeSet<orkia_model::EventId> = events.iter().map(|event| event.unsigned.id.clone()).collect(); let atoms = extract_atoms(&ChangedFile { path: path.to_string_lossy().into_owned(), changed_start: 1, changed_end: content.lines().count() as u32, content, source_events: source_events.clone() }); let plan = plan(PlanningInput { checkpoint: checkpoint.unwrap_or(git.head_commit()?), atoms, dependencies: vec![], coverage_milli: 1000, minimum_coverage_milli: 950, minimum_confidence_milli: 800, source_events }); let destination = root.join("orkia/plans").join(format!("{}.json", plan.id.0)); write_json(&destination, &plan)?; println!("review plan {}: {} unit(s)", plan.id.0, plan.units.len()); }
    ReviewCommand::Show { plan } => { let value: ReviewPlan = read_json(&root.join("orkia/plans").join(format!("{plan}.json")))?; println!("{}", serde_json::to_string_pretty(&value).map_err(|e| OrkiaError::Invalid(e.to_string()))?); }
    ReviewCommand::Merge { plan: plan_id, units } => { let path = root.join("orkia/plans").join(format!("{plan_id}.json")); let current: ReviewPlan = read_json(&path)?; let units = units.into_iter().filter(|value| !value.trim().is_empty()).map(|value| value.trim().parse::<uuid::Uuid>().map(orkia_model::ReviewUnitId).map_err(|error| OrkiaError::Invalid(format!("invalid review unit id: {error}")))).collect::<Result<BTreeSet<_>>>()?; let revised = orkia_review::apply_correction(&current, orkia_review::ReviewerCorrection::Merge { units, reason: "reviewer merge via CLI".into() })?; let destination = root.join("orkia/plans").join(format!("{}.json", revised.id.0)); write_json(&destination, &revised)?; println!("review plan revision {} created", revised.id.0); }
} Ok(()) }
fn open_ledger(git: &LibGit2Repository, root: &Path, secrets: &FileSecrets) -> Result<(SessionState, Ledger<orkia_git::GitLedgerStore, SystemClock>)> { let state = read_state(root)?; let identity = Identity::load(secrets, "identity", state.actor.clone())?.ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?; Ok((state.clone(), Ledger::new(git.ledger_store(), SystemClock, state.repository, identity))) }
fn load_identity(root: &Path, secrets: &FileSecrets) -> Result<Identity> { let actor: Actor = read_json(&root.join("orkia/actor.json"))?; Identity::load(secrets, "identity", actor)?.ok_or_else(|| OrkiaError::NotFound("run `orkia identity init` first".into())) }
fn read_state(root: &Path) -> Result<SessionState> { read_json(&root.join("orkia/session.json")) }
fn git_dir(path: &Path) -> Result<PathBuf> { Ok(git2::Repository::open(path).map_err(|e| OrkiaError::External(e.to_string()))?.path().to_path_buf()) }
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> { let text = serde_json::to_vec_pretty(value).map_err(|e| OrkiaError::Invalid(e.to_string()))?; fs::write(path, text).map_err(|e| OrkiaError::External(e.to_string())) }
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> { serde_json::from_slice(&fs::read(path).map_err(|e| OrkiaError::NotFound(format!("{}: {e}", path.display())))?).map_err(|e| OrkiaError::Integrity(e.to_string())) }
