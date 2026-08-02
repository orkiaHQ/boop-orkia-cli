//! Terminal composition root for Orkia.

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use clap::{Parser, Subcommand, ValueEnum};
use orkia_agents::{
    Agent as SupportedAgent, TranscriptReconciliation, all_statuses, install as install_agent,
    normalize_hook, parse_hook_payload, reconcile_transcript, status as agent_status,
    transcript_files, transcript_files_at, transcript_snapshot, uninstall as uninstall_agent,
};
use orkia_capture::{ClaudeAdapter, CodexAdapter, ProviderAdapter, WorkspaceWatcher};
use orkia_git::LibGit2Repository;
use orkia_github::{GitHubApp, GitHubAppCredentials};
use orkia_identity::Identity;
use orkia_ledger::{Ledger, SystemClock, verify_chain};
use orkia_model::{
    Actor, AgentSnapshotPhase, CaptureEvent, CaptureOrigin, ChangeSetId, ChangeSetStack, Intent,
    OrkiaError, RepositoryId, RepositoryPolicy, Result, ReviewPlan, SessionId, StackId,
};
use orkia_ports::{Forge, GitRepository, LedgerStore, SecretStore, SemanticDocumentStore};
use orkia_review::{PlanningInput, plan};
use orkia_semantic::{
    ChangedFile, changed_line_ranges, extract_atoms_in_ranges, infer_dependencies,
};
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
    /// Initialize Orkia metadata for an existing Git repository.
    Init {
        /// Display name used only when creating a new local identity.
        #[arg(long)]
        name: Option<String>,
        /// Create the Git repository when the target directory is not already a repository.
        #[arg(long)]
        create_git: bool,
        /// Install measured provider hooks as part of repository bootstrap.
        #[arg(long = "agent")]
        agent: Vec<String>,
    },
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
    /// Coordinate one or more repository-local stacks without importing their
    /// Git contents into a central store.
    Changeset {
        #[command(subcommand)]
        command: ChangeSetCommand,
    },
    Integrate {
        /// Integrate one repository-local signed review plan.
        #[arg(
            long,
            conflicts_with = "changeset",
            required_unless_present = "changeset"
        )]
        plan: Option<String>,
        /// Integrate a signed multi-repository ChangeSet in its topological order.
        #[arg(long, conflicts_with = "plan", required_unless_present = "plan")]
        changeset: Option<String>,
        /// Repository locations used by `--changeset`, each formatted as
        /// `<repository-uuid>=<absolute-path>`.
        #[arg(long = "repository-path", requires = "changeset")]
        repository_path: Vec<String>,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value_t = 0)]
        approvals: u8,
        /// GitHub owner. Supply with --github-repository to publish the
        /// policy check to projected commits.
        #[arg(long)]
        github_owner: Option<String>,
        #[arg(long)]
        github_repository: Option<String>,
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
    /// Internal long-lived watcher spawned by `session start` for humans.
    #[command(hide = true)]
    Watch,
}
#[derive(Subcommand)]
enum LedgerCommand {
    Verify,
    /// Fetch the signed Orkia namespace through the configured Git remote.
    Fetch {
        #[arg(long, default_value = "origin")]
        remote: String,
    },
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
    Approve {
        #[arg(long)]
        plan: String,
    },
    RequestChanges {
        #[arg(long)]
        plan: String,
        #[arg(long)]
        reason: String,
    },
    Merge {
        #[arg(long)]
        plan: String,
        #[arg(long, required = true, value_delimiter = ',')]
        units: Vec<String>,
    },
    Split {
        #[arg(long)]
        plan: String,
        #[arg(long)]
        unit: String,
        /// Atom IDs per output unit, separated by `;` (for example
        /// `atom-a,atom-b;atom-c`).
        #[arg(long, required = true, value_delimiter = ';')]
        groups: Vec<String>,
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
#[derive(Subcommand)]
enum ChangeSetCommand {
    /// Discover the latest causally-related stack in each repository and
    /// compose one signed multi-repository ChangeSet. Authors provide only
    /// repository roots; stack and PR identities are derived from signed
    /// session evidence and Git refs.
    Auto {
        /// Repository roots participating in the coordinated work. Every
        /// repository must expose the same normalized captured objective.
        #[arg(long = "repository-path", required = true)]
        repository_path: Vec<PathBuf>,
        /// IDs of ChangeSets that must be integrated before this one.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
    },
    /// Create and sign a multi-repository delivery ChangeSet. Each `--stack`
    /// is `<repository-uuid>:<stack-uuid>`.
    Create {
        #[arg(long, required = true)]
        stack: Vec<String>,
        /// Repository locations used to verify each referenced Stack before
        /// publication. Each value is `<repository-uuid>=<absolute-path>`.
        #[arg(long = "repository-path", required = true)]
        repository_path: Vec<String>,
        /// IDs of ChangeSets that must be integrated before this one.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
    },
    /// Display the latest signed revision reconstructed from Git refs.
    Show {
        #[arg(long)]
        id: String,
    },
    /// Resolve every exact StackPullRequest/projection selected by this
    /// ChangeSet and report whether it is ready for coordinated integration.
    Status {
        #[arg(long)]
        id: String,
        /// Repository locations used to reconstruct every referenced Stack.
        /// Each value is `<repository-uuid>=<absolute-path>`.
        #[arg(long = "repository-path", required = true)]
        repository_path: Vec<String>,
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

#[derive(Serialize)]
struct ChangeSetReadiness {
    id: ChangeSetId,
    revision: u32,
    ready_for_integration: bool,
    stacks: Vec<ChangeSetStackReadiness>,
    execution_order: Vec<ChangeSetExecutionStep>,
}

#[derive(Serialize)]
struct ChangeSetStackReadiness {
    repository: RepositoryId,
    stack: StackId,
    revision: u32,
    pull_request_count: usize,
    published: bool,
}

#[derive(Serialize)]
struct ChangeSetExecutionStep {
    repository: RepositoryId,
    pull_request: orkia_model::StackPullRequestId,
    revision: u32,
    published: bool,
}

#[derive(Serialize)]
struct WireChangeSetStack {
    repository_id: RepositoryId,
    stack_id: StackId,
    revision: u32,
}

#[derive(Serialize)]
struct WireChangeSetProof {
    repository_id: RepositoryId,
    stack_id: StackId,
    revision: u32,
    /// The causal session that produced the exact Stack revision.  Keeping
    /// this in the signed wire payload lets the backend/UI show provenance
    /// without pretending that a ChangeSet itself is a session.
    session_id: SessionId,
    validation_count: usize,
    refs: Vec<String>,
}

type ChangeSetProofKey = (RepositoryId, StackId, u32);
type ChangeSetProofMetadata = (SessionId, usize);

#[derive(Serialize)]
struct WireChangeSetPayload {
    wire_version: u16,
    signer_id: uuid::Uuid,
    changeset_id: ChangeSetId,
    revision: u32,
    coordinator_repository_id: RepositoryId,
    status: String,
    stacks: Vec<WireChangeSetStack>,
    depends_on: Vec<ChangeSetId>,
    proofs: Vec<WireChangeSetProof>,
}

#[derive(Serialize)]
struct WireChangeSetSigner {
    id: uuid::Uuid,
    display_name: String,
    public_key: String,
}

#[derive(Serialize)]
struct WireChangeSetSubmission {
    wire_version: u16,
    submission_id: uuid::Uuid,
    signer: WireChangeSetSigner,
    payload_base64: String,
    signature: String,
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

/// Creates the forge adapter from either a credential-brokered installation
/// token or the complete GitHub App credential set.  The latter is the normal
/// self-hosted mode: Orkia signs a short-lived JWT locally and asks GitHub for
/// an installation-scoped token, so a long-lived user token is never needed.
fn github_app_from_environment(owner: String, repository: String) -> Result<GitHubApp> {
    if let Ok(token) = std::env::var("ORKIA_GITHUB_INSTALLATION_TOKEN")
        && !token.is_empty()
    {
        return GitHubApp::new(owner, repository, token);
    }
    let app_id = std::env::var("ORKIA_GITHUB_APP_ID")
        .map_err(|_| OrkiaError::NotFound("ORKIA_GITHUB_APP_ID".into()))?
        .parse::<u64>()
        .map_err(|_| OrkiaError::Invalid("ORKIA_GITHUB_APP_ID must be an integer".into()))?;
    let installation_id = std::env::var("ORKIA_GITHUB_INSTALLATION_ID")
        .map_err(|_| OrkiaError::NotFound("ORKIA_GITHUB_INSTALLATION_ID".into()))?
        .parse::<u64>()
        .map_err(|_| {
            OrkiaError::Invalid("ORKIA_GITHUB_INSTALLATION_ID must be an integer".into())
        })?;
    let private_key = match std::env::var("ORKIA_GITHUB_PRIVATE_KEY_PATH") {
        Ok(path) => fs::read(&path).map_err(|error| {
            OrkiaError::External(format!("read ORKIA_GITHUB_PRIVATE_KEY_PATH {path}: {error}"))
        })?,
        Err(_) => std::env::var("ORKIA_GITHUB_PRIVATE_KEY")
            .map_err(|_| {
                OrkiaError::NotFound(
                    "set ORKIA_GITHUB_INSTALLATION_TOKEN or GitHub App credentials (ORKIA_GITHUB_APP_ID, ORKIA_GITHUB_INSTALLATION_ID and ORKIA_GITHUB_PRIVATE_KEY_PATH)".into(),
                )
            })?
            // GitHub Actions and dotenv stores commonly preserve PEM line
            // breaks as literal `\\n`; normalize only that encoding.
            .replace("\\n", "\n")
            .into_bytes(),
    };
    GitHubApp::from_app_credentials(
        owner,
        repository,
        GitHubAppCredentials {
            app_id,
            installation_id,
            private_key_pem: &private_key,
        },
    )
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
    let git = match &cli.command {
        Command::Init {
            create_git: true, ..
        } => match LibGit2Repository::open(&cli.repository) {
            Ok(git) => git,
            Err(_) => {
                git2::Repository::init(&cli.repository)
                    .map_err(|e| OrkiaError::External(format!("create Git repository: {e}")))?;
                LibGit2Repository::open(&cli.repository)?
            }
        },
        _ => LibGit2Repository::open(&cli.repository)?,
    };
    let root = git_dir(&cli.repository)?;
    let secrets = FileSecrets {
        root: root.join("orkia/keys"),
    };
    fs::create_dir_all(root.join("orkia/plans"))
        .map_err(|e| OrkiaError::External(e.to_string()))?;
    match cli.command {
        Command::Init {
            name,
            create_git: _,
            agent,
        } => {
            let (actor, repository_id, identity_created) =
                ensure_repository_initialized(&root, &secrets, name.as_deref())?;
            let policy = load_repository_policy(&cli.repository)?;
            git.store_actor(&actor)?;
            let events = git.ledger_store().read_all()?;
            let mut actors = git.actors()?;
            actors.insert(actor.id.clone(), actor.clone());
            verify_chain(&events, &actors)?;
            let verified = git.verify_orkia_refs(&policy)?.verified_states;
            let executable = std::env::current_exe()
                .map_err(|error| OrkiaError::External(format!("current executable: {error}")))?;
            let mut installed = Vec::new();
            for name in agent {
                let provider = supported_agent(&name)?;
                let change = install_agent(provider, &executable).map_err(OrkiaError::External)?;
                installed.push(format!(
                    "{} ({} hook event(s))",
                    provider.name(),
                    change.added.len()
                ));
            }
            println!(
                "orkia initialized repository={} actor={} identity={} agents={} ledger_events={} semantic_states={} policy={} refs={} backend={}",
                repository_id.0,
                actor.id.0,
                if identity_created {
                    "created"
                } else {
                    "existing"
                },
                if installed.is_empty() {
                    "none".to_owned()
                } else {
                    installed.join(",")
                },
                events.len(),
                verified,
                repository_policy_path(&root).display(),
                root.join("refs/orkia/ledger").display(),
                std::env::var("ORKIA_BACKEND_URL").unwrap_or_else(|_| "offline".into())
            );
        }
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
        Command::Ledger { command } => match command {
            LedgerCommand::Verify => {
                let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
                let events = git.ledger_store().read_all()?;
                let mut actors = git.actors()?;
                actors.insert(actor.id.clone(), actor);
                verify_chain(&events, &actors)?;
                println!("verified {} signed ledger events", events.len());
            }
            LedgerCommand::Fetch { remote } => {
                let policy = load_repository_policy(&cli.repository)?;
                let verification = git.fetch_verified_orkia_refs(&remote, &policy)?;
                println!(
                    "fetched and verified {} semantic states, {} ledger events",
                    verification.verified_states,
                    git.ledger_store().read_all()?.len()
                );
            }
        },
        Command::Review { command } => {
            handle_review(command, &git, &root, &cli.repository, &secrets)?
        }
        Command::Changeset { command } => {
            handle_changeset(command, &git, &root, &cli.repository, &secrets)?
        }
        Command::Integrate {
            plan,
            changeset,
            repository_path,
            branch,
            approvals,
            github_owner,
            github_repository,
        } => {
            if let Some(changeset) = changeset {
                if github_owner.is_some() || github_repository.is_some() {
                    return Err(OrkiaError::Invalid(
                        "GitHub check publication for a ChangeSet requires one forge credential set per repository; run integrate for each repository plan".into(),
                    ));
                }
                handle_changeset_integration(
                    &changeset,
                    &repository_path,
                    &branch,
                    approvals,
                    &git,
                    &cli.repository,
                )?;
                return Ok(());
            }
            let plan = plan.expect("clap requires --plan when --changeset is absent");
            // Integration is a security boundary: never trust the convenient
            // worktree JSON cache when the signed plan can be reconstructed
            // directly from refs/orkia/plans in this clone or a bare mirror.
            let review = signed_review_plan(&root, &git, &cli.repository, &plan)?;
            let policy_path = cli.repository.join("orkia.toml");
            let policy = if policy_path.exists() {
                let content = fs::read_to_string(&policy_path).map_err(|error| {
                    OrkiaError::NotFound(format!("{}: {error}", policy_path.display()))
                })?;
                orkia_policy::parse(&content)?
            } else {
                orkia_model::RepositoryPolicy::default()
            };
            let ledger = open_repository_ledger(&git, &root, &secrets)?;
            let validations = run_validations(&cli.repository, &policy, &ledger)?;
            let integration =
                orkia_policy::evaluate(&policy, &review, &validations, approvals, &branch);
            match (github_owner, github_repository) {
                (Some(owner), Some(repository)) => {
                    let github = github_app_from_environment(owner, repository)?;
                    let summary = match &integration {
                        Ok(()) => format!(
                            "Signed plan {} revision {} passed Orkia integration policy for {branch}.",
                            review.id.0, review.revision
                        ),
                        Err(error) => format!(
                            "Signed plan {} revision {} failed Orkia integration policy for {branch}: {error}",
                            review.id.0, review.revision
                        ),
                    };
                    for pull_request in git
                        .semantic_store()
                        .stack_pull_requests_for_plan(&review, &policy)?
                    {
                        let (_, projection) = git
                            .semantic_store()
                            .latest_projection_for_stack_pull_request_revision(
                                &pull_request.id,
                                pull_request.revision,
                                &policy,
                            )?
                            .ok_or_else(|| {
                                OrkiaError::NotFound(format!(
                                    "projection for stack pull request {}",
                                    pull_request.id.0
                                ))
                            })?;
                        let commit = projection.commit.as_deref().ok_or_else(|| {
                            OrkiaError::Integrity(format!(
                                "projection {} has no immutable projected commit",
                                projection.id.0
                            ))
                        })?;
                        for check in &policy.required_checks {
                            github.publish_check(commit, check, integration.is_ok(), &summary)?;
                        }
                    }
                }
                (None, None) => {}
                _ => {
                    return Err(OrkiaError::Invalid(
                        "--github-owner and --github-repository must be supplied together".into(),
                    ));
                }
            }
            let passed = integration.is_ok();
            let reason = integration
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "policy passed".into());
            ledger.append(CaptureEvent::IntegrationEvaluated {
                plan: Some(review.id.clone()),
                changeset: None,
                branch: branch.clone(),
                approvals,
                passed,
                reason,
            })?;
            integration?;
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
            repository,
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
            let changes = git.changes_since(&session_base(&events, &session)?)?;
            let repository_id = session_repository(&events, &session)?;
            persist_review_plan(
                root,
                &ledger,
                git,
                secrets,
                &plan,
                &checkpoint,
                &session,
                &repository_id,
                &session_base(&events, &session)?,
                &changes,
                repository,
            )?;
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
    repository: &Path,
) -> Result<orkia_model::LedgerEvent> {
    let events = git.ledger_store().read_all()?;
    let base_commit = session_base(&events, session)?;
    let changed_paths = changed_paths(git, &base_commit)?;
    let observed_paths = observed_agent_paths(&events, session, repository);
    let unknown_write = changed_paths.iter().any(|path| {
        !observed_paths
            .iter()
            .any(|observed| repository_path_matches(observed, path))
    });
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

fn session_repository(
    events: &[orkia_model::LedgerEvent],
    session: &SessionId,
) -> Result<RepositoryId> {
    events
        .iter()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::SessionStarted {
                session: recorded_session,
                ..
            } if recorded_session == session => Some(event.unsigned.repository.clone()),
            _ => None,
        })
        .ok_or_else(|| OrkiaError::NotFound(format!("session repository {}", session.0)))
}

fn observed_agent_paths(
    events: &[orkia_model::LedgerEvent],
    session: &SessionId,
    repository: &Path,
) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|event| match &event.unsigned.event {
            CaptureEvent::AgentAction {
                session: Some(recorded_session),
                action: orkia_model::AgentActionKind::FileWrite { path, .. },
                ..
            } if recorded_session == session => Some(relative_repository_path(repository, path)),
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
            let repository_id: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
            let state = SessionState {
                id: SessionId::new(),
                repository: repository_id.clone(),
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
            if matches!(origin, Origin::Human) && !cfg!(test) {
                spawn_workspace_watcher(repository)?;
            }
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
            // Keep the provider's exact stream in the signed ledger as well as
            // the normalized actions. This is the live-session counterpart to
            // `AgentCommand::Import` and lets a later adapter version replay
            // fields that were not understood at capture time.
            ledger.append(CaptureEvent::AgentTranscript {
                agent: adapter.provider_name().into(),
                path: format!("agent://{}/{}", adapter.provider_name(), state.id.0),
                encoding: "utf-8".into(),
                content: stdout.clone(),
            })?;
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
                    persist_review_plan(
                        root,
                        &ledger,
                        git,
                        secrets,
                        &plan,
                        &checkpoint,
                        &state.id,
                        &state.repository,
                        &state.base_commit,
                        &changes,
                        repository,
                    )?;
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
        SessionCommand::Watch => {
            let (_, ledger) = open_ledger(git, root, secrets)?;
            let watcher = WorkspaceWatcher::start(repository)?;
            let session_path = root.join("orkia/session.json");
            while session_path.exists() {
                let modified = watcher
                    .drain()
                    .into_iter()
                    .filter_map(|path| path.strip_prefix(repository).ok().map(Path::to_path_buf))
                    .filter(|path| !path.starts_with(".git"))
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .collect::<BTreeSet<_>>();
                if !modified.is_empty() {
                    ledger.append(CaptureEvent::FilesObserved {
                        read: BTreeSet::new(),
                        modified,
                        unknown_write: true,
                    })?;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    Ok(())
}

fn spawn_workspace_watcher(repository: &Path) -> Result<()> {
    let executable = std::env::current_exe()
        .map_err(|error| OrkiaError::External(format!("locate Orkia executable: {error}")))?;
    std::process::Command::new(executable)
        .arg("--repository")
        .arg(repository)
        .args(["session", "watch"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| OrkiaError::External(format!("start workspace watcher: {error}")))
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
            let changes = git.changes_since(&base)?;
            let repository_id = session_repository(&events, &session)?;
            persist_review_plan(
                root,
                &ledger,
                git,
                secrets,
                &plan,
                &checkpoint,
                &session,
                &repository_id,
                &base,
                &changes,
                repository,
            )?;
            println!(
                "review plan {}: {} unit(s), {} atom(s), coverage {}‰",
                plan.id.0,
                plan.units.len(),
                plan.atoms.len(),
                plan.coverage_milli
            );
        }
        ReviewCommand::Show { plan } => {
            let value = signed_review_plan(root, git, repository, &plan)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .map_err(|e| OrkiaError::Invalid(e.to_string()))?
            );
        }
        ReviewCommand::Approve { plan: plan_id } => {
            let current = signed_review_plan(root, git, repository, &plan_id)?;
            let revised = orkia_review::set_status(&current, orkia_model::PlanStatus::Approved)?;
            persist_revised_plan(
                root,
                git,
                repository,
                secrets,
                &revised,
                "reviewer approval via CLI",
            )?;
            println!(
                "review plan {} revision {} approved",
                revised.id.0, revised.revision
            );
        }
        ReviewCommand::RequestChanges {
            plan: plan_id,
            reason,
        } => {
            let current = signed_review_plan(root, git, repository, &plan_id)?;
            let revised =
                orkia_review::set_status(&current, orkia_model::PlanStatus::ChangesRequested)?;
            persist_revised_plan(root, git, repository, secrets, &revised, &reason)?;
            println!(
                "review plan {} revision {} changes requested",
                revised.id.0, revised.revision
            );
        }
        ReviewCommand::Merge {
            plan: plan_id,
            units,
        } => {
            let current = signed_review_plan(root, git, repository, &plan_id)?;
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
            persist_revised_plan(
                root,
                git,
                repository,
                secrets,
                &revised,
                "reviewer merge via CLI",
            )?;
            println!("review plan revision {} created", revised.id.0);
        }
        ReviewCommand::Split {
            plan: plan_id,
            unit,
            groups,
        } => {
            let current = signed_review_plan(root, git, repository, &plan_id)?;
            let unit = unit
                .parse::<uuid::Uuid>()
                .map(orkia_model::ReviewUnitId)
                .map_err(|error| OrkiaError::Invalid(format!("invalid review unit id: {error}")))?;
            let groups = groups
                .into_iter()
                .map(|group| {
                    group
                        .split(',')
                        .filter(|atom| !atom.trim().is_empty())
                        .map(|atom| {
                            atom.trim()
                                .parse::<uuid::Uuid>()
                                .map(orkia_model::AtomId)
                                .map_err(|error| {
                                    OrkiaError::Invalid(format!("invalid atom id: {error}"))
                                })
                        })
                        .collect::<Result<BTreeSet<_>>>()
                })
                .collect::<Result<Vec<_>>>()?;
            let revised = orkia_review::apply_correction(
                &current,
                orkia_review::ReviewerCorrection::Split {
                    unit,
                    groups,
                    reason: "reviewer split via CLI".into(),
                },
            )?;
            persist_revised_plan(
                root,
                git,
                repository,
                secrets,
                &revised,
                "reviewer split via CLI",
            )?;
            println!("review plan revision {} created", revised.id.0);
        }
        ReviewCommand::Project { plan: plan_id } => {
            let policy = load_repository_policy(repository)?;
            let plan = signed_review_plan(root, git, repository, &plan_id)?;
            let events = git.ledger_store().read_all()?;
            let (_, base, _) = plan_session_context(&events, &plan)?;
            let pull_requests = git
                .semantic_store()
                .stack_pull_requests_for_plan(&plan, &policy)?;
            let by_id = pull_requests
                .iter()
                .map(|pull_request| (pull_request.id.clone(), pull_request))
                .collect::<BTreeMap<_, _>>();
            let identity = load_identity(root, secrets)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            for materialized in
                orkia_projection::restack_mono_repository(git, &pull_requests, "main", &base)?
            {
                let pull_request = by_id[&materialized.step.pull_request];
                let previous = git
                    .semantic_store()
                    .latest_projection_for_stack_pull_request(&pull_request.id, &policy)?;
                let projection = orkia_model::Projection {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    id: orkia_model::ProjectionId::from_stack_pull_request(&pull_request.id),
                    revision: previous
                        .as_ref()
                        .map_or(0, |(_, projection)| projection.revision + 1),
                    stack_pull_request: pull_request.id.clone(),
                    stack_pull_request_revision: pull_request.revision,
                    repository: pull_request.repository.clone(),
                    branch: materialized.step.branch.clone(),
                    base_branch: materialized.step.parent_branch.clone(),
                    base_commit: materialized.parent_commit.clone(),
                    commit: Some(materialized.commit.clone()),
                    forge_pull_request: None,
                    status: orkia_model::ProjectionStatus::Projected,
                    supersedes: previous.map(|(object, _)| object),
                };
                git.semantic_store()
                    .store_projection(&projection, &identity)?;
                ledger.append(CaptureEvent::ProjectionUpdated {
                    projection: projection.id.clone(),
                    pull_request: pull_request.id.clone(),
                    revision: projection.revision,
                    commit: projection.commit.clone(),
                })?;
                println!("{} -> {}", materialized.step.branch, materialized.commit);
            }
        }
        ReviewCommand::Publish {
            plan: plan_id,
            github_owner,
            github_repository,
            base,
            remote,
        } => {
            let policy = load_repository_policy(repository)?;
            let plan = signed_review_plan(root, git, repository, &plan_id)?;
            let pull_requests = git
                .semantic_store()
                .stack_pull_requests_for_plan(&plan, &policy)?;
            let github = github_app_from_environment(github_owner, github_repository)?;
            if policy.protected_branches.contains(&base) {
                github.set_required_checks(
                    &base,
                    &policy.required_checks.iter().cloned().collect::<Vec<_>>(),
                    policy.required_approvals,
                )?;
            }
            let identity = load_identity(root, secrets)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            // Publish the immutable causal evidence before creating any forge
            // review. A GitHub PR must never point at a branch whose signed
            // plan/StackPullRequest cannot be fetched by a reviewer.
            git.push_orkia_refs(&remote)?;
            for review in orkia_forge::stack_pull_request_projections(&pull_requests, &base)? {
                let pull_request = review.pull_request.as_ref().ok_or_else(|| {
                    OrkiaError::Integrity(
                        "forge projection has no stack pull request identity".into(),
                    )
                })?;
                let stack_pull_request = pull_requests
                    .iter()
                    .find(|candidate| &candidate.id == pull_request)
                    .ok_or_else(|| {
                        OrkiaError::Integrity(format!(
                            "forge projection references an absent stack pull request {}",
                            pull_request.0
                        ))
                    })?;
                git.push_branch(&remote, &review.branch)?;
                let url = github.publish(&review)?;
                let previous = git
                    .semantic_store()
                    .latest_projection_for_stack_pull_request_revision(
                        pull_request,
                        stack_pull_request.revision,
                        &policy,
                    )?
                    .ok_or_else(|| {
                        OrkiaError::NotFound(format!(
                            "projection for stack pull request {}",
                            pull_request.0
                        ))
                    })?;
                let mut published = previous.1.clone();
                published.revision += 1;
                published.forge_pull_request = Some(url.clone());
                published.status = orkia_model::ProjectionStatus::Published;
                published.supersedes = Some(previous.0);
                git.semantic_store()
                    .store_projection(&published, &identity)?;
                ledger.append(CaptureEvent::ProjectionUpdated {
                    projection: published.id.clone(),
                    pull_request: published.stack_pull_request.clone(),
                    revision: published.revision,
                    commit: published.commit.clone(),
                })?;
                println!("{}", url);
            }
            // The forge URL and publication ledger events were created above;
            // synchronize their signed revisions as the final publication
            // step as well.
            git.push_orkia_refs(&remote)?;
        }
    }
    Ok(())
}

fn handle_changeset(
    command: ChangeSetCommand,
    git: &LibGit2Repository,
    root: &Path,
    repository: &Path,
    secrets: &FileSecrets,
) -> Result<()> {
    let policy = load_repository_policy(repository)?;
    match command {
        ChangeSetCommand::Auto {
            repository_path,
            depends_on,
        } => {
            let (discovered, objective) = discover_causally_related_stacks(&repository_path)?;
            let references = verify_referenced_stacks(
                &discovered,
                &repository_path
                    .iter()
                    .map(|path| {
                        let root = git_dir(path)?;
                        let id: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
                        Ok((id, path.clone()))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?,
            )?;
            let mut change_set = orkia_changesets::change_set_from_stack_references(references)?;
            for dependency in depends_on {
                let id = dependency
                    .trim()
                    .parse::<uuid::Uuid>()
                    .map(ChangeSetId)
                    .map_err(|error| {
                        OrkiaError::Invalid(format!("invalid ChangeSet dependency: {error}"))
                    })?;
                orkia_changesets::add_changeset_dependency(&mut change_set, id)?;
            }
            for dependency in &change_set.depends_on {
                let Some((_, dependency_changeset)) =
                    git.semantic_store().latest_changeset(dependency, &policy)?
                else {
                    return Err(OrkiaError::NotFound(format!(
                        "integrated ChangeSet dependency {}",
                        dependency.0
                    )));
                };
                if !matches!(
                    dependency_changeset.status,
                    orkia_model::StackPullRequestStatus::Integrated
                ) {
                    return Err(OrkiaError::Policy(format!(
                        "ChangeSet dependency {} is not integrated",
                        dependency.0
                    )));
                }
            }
            let identity = load_identity(root, secrets)?;
            if let Some((_, current)) = git
                .semantic_store()
                .latest_changeset(&change_set.id, &policy)?
                && current.stacks == change_set.stacks
                && current.depends_on == change_set.depends_on
                && current.status == change_set.status
            {
                println!(
                    "changeset {} revision {} already automatically coordinated objective=`{}` repositories={}",
                    current.id.0,
                    current.revision,
                    objective,
                    current.stacks.len()
                );
                maybe_submit_changeset(&current, &identity, &repository_path)?;
                return Ok(());
            }
            let object = git
                .semantic_store()
                .store_changeset(&change_set, &identity)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            ledger.append(CaptureEvent::ChangeSetPublished {
                changeset: change_set.id.clone(),
                revision: change_set.revision,
                object,
            })?;
            maybe_submit_changeset(&change_set, &identity, &repository_path)?;
            println!(
                "changeset {} revision {} automatically coordinated objective=`{}` repositories={}",
                change_set.id.0,
                change_set.revision,
                objective,
                change_set.stacks.len()
            );
        }
        ChangeSetCommand::Create {
            stack,
            repository_path,
            depends_on,
        } => {
            let references = stack
                .iter()
                .map(|value| parse_changeset_stack_reference(value))
                .collect::<Result<BTreeSet<_>>>()?;
            let repository_paths = repository_path
                .iter()
                .map(|value| parse_changeset_repository_path(value))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let references = verify_referenced_stacks(&references, &repository_paths)?;
            let mut change_set = orkia_changesets::change_set_from_stack_references(references)?;
            for dependency in depends_on {
                let id = dependency
                    .trim()
                    .parse::<uuid::Uuid>()
                    .map(ChangeSetId)
                    .map_err(|error| {
                        OrkiaError::Invalid(format!("invalid ChangeSet dependency: {error}"))
                    })?;
                orkia_changesets::add_changeset_dependency(&mut change_set, id)?;
            }
            // A delivery dependency is meaningful only when its signed
            // ChangeSet has already completed integration in this
            // coordinator. Do not leave an implicit promise to an external,
            // absent or merely published stack.
            for dependency in &change_set.depends_on {
                let Some((_, dependency_changeset)) =
                    git.semantic_store().latest_changeset(dependency, &policy)?
                else {
                    return Err(OrkiaError::NotFound(format!(
                        "integrated ChangeSet dependency {}",
                        dependency.0
                    )));
                };
                if !matches!(
                    dependency_changeset.status,
                    orkia_model::StackPullRequestStatus::Integrated
                ) {
                    return Err(OrkiaError::Policy(format!(
                        "ChangeSet dependency {} is not integrated",
                        dependency.0
                    )));
                }
            }
            let identity = load_identity(root, secrets)?;
            if let Some((previous, current)) = git
                .semantic_store()
                .latest_changeset(&change_set.id, &policy)?
            {
                if current.stacks == change_set.stacks
                    && current.depends_on == change_set.depends_on
                    && current.status == change_set.status
                {
                    println!(
                        "changeset {} revision {} already published",
                        current.id.0, current.revision
                    );
                    return Ok(());
                }
                change_set.revision = current.revision + 1;
                change_set.supersedes = Some(previous);
            }
            let object = git
                .semantic_store()
                .store_changeset(&change_set, &identity)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            ledger.append(CaptureEvent::ChangeSetPublished {
                changeset: change_set.id.clone(),
                revision: change_set.revision,
                object,
            })?;
            let repository_paths = repository_paths.values().cloned().collect::<Vec<_>>();
            maybe_submit_changeset(&change_set, &identity, &repository_paths)?;
            println!(
                "changeset {} revision {} published",
                change_set.id.0, change_set.revision
            );
        }
        ChangeSetCommand::Show { id } => {
            let id = id
                .parse::<uuid::Uuid>()
                .map(ChangeSetId)
                .map_err(|error| OrkiaError::Invalid(format!("invalid ChangeSet ID: {error}")))?;
            let (_, change_set) = git
                .semantic_store()
                .latest_changeset(&id, &policy)?
                .ok_or_else(|| OrkiaError::NotFound(format!("ChangeSet {}", id.0)))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&change_set)
                    .map_err(|error| OrkiaError::Invalid(error.to_string()))?
            );
        }
        ChangeSetCommand::Status {
            id,
            repository_path,
        } => {
            let id = id
                .parse::<uuid::Uuid>()
                .map(ChangeSetId)
                .map_err(|error| OrkiaError::Invalid(format!("invalid ChangeSet ID: {error}")))?;
            let (_, change_set) = git
                .semantic_store()
                .latest_changeset(&id, &policy)?
                .ok_or_else(|| OrkiaError::NotFound(format!("ChangeSet {}", id.0)))?;
            let repository_paths = repository_path
                .iter()
                .map(|value| parse_changeset_repository_path(value))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let mut dependencies = BTreeMap::new();
            let mut pending = change_set.depends_on.iter().cloned().collect::<Vec<_>>();
            while let Some(dependency) = pending.pop() {
                if dependencies.contains_key(&dependency) {
                    continue;
                }
                let Some((_, dependency_changeset)) = git
                    .semantic_store()
                    .latest_changeset(&dependency, &policy)?
                else {
                    return Err(OrkiaError::NotFound(format!(
                        "published ChangeSet dependency {}",
                        dependency.0
                    )));
                };
                if !matches!(
                    dependency_changeset.status,
                    orkia_model::StackPullRequestStatus::Integrated
                ) {
                    return Err(OrkiaError::Policy(format!(
                        "ChangeSet dependency {} is not integrated",
                        dependency.0
                    )));
                }
                if !change_set_readiness(&dependency_changeset, &repository_paths)?
                    .ready_for_integration
                {
                    return Err(OrkiaError::Conflict(format!(
                        "ChangeSet dependency {} is not forge-published",
                        dependency.0
                    )));
                }
                pending.extend(dependency_changeset.depends_on.iter().cloned());
                dependencies.insert(dependency, dependency_changeset);
            }
            let mut groups = vec![change_set.clone()];
            groups.extend(dependencies.into_values());
            orkia_changesets::changeset_execution_order(&groups)?;
            let readiness = change_set_readiness(&change_set, &repository_paths)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&readiness)
                    .map_err(|error| OrkiaError::Invalid(error.to_string()))?
            );
        }
    }
    Ok(())
}

/// Evaluates every repository-local review selected by a ChangeSet.  A
/// ChangeSet is a coordinator object, so it never gets treated as a Git
/// commit: each repository's signed plan, policy, validations and exact
/// projection are checked independently before the global execution order is
/// accepted.
#[allow(clippy::too_many_arguments)]
fn handle_changeset_integration(
    changeset_id: &str,
    repository_paths: &[String],
    branch: &str,
    approvals: u8,
    coordinator_git: &LibGit2Repository,
    coordinator_repository: &Path,
) -> Result<()> {
    let id = changeset_id
        .parse::<uuid::Uuid>()
        .map(ChangeSetId)
        .map_err(|error| OrkiaError::Invalid(format!("invalid ChangeSet ID: {error}")))?;
    let coordinator_policy = load_repository_policy(coordinator_repository)?;
    let (changeset_object, changeset) = coordinator_git
        .semantic_store()
        .latest_changeset(&id, &coordinator_policy)?
        .ok_or_else(|| OrkiaError::NotFound(format!("ChangeSet {}", id.0)))?;
    if matches!(
        changeset.status,
        orkia_model::StackPullRequestStatus::Integrated
    ) {
        return Err(OrkiaError::Conflict(format!(
            "ChangeSet {} is already integrated at revision {}",
            changeset.id.0, changeset.revision
        )));
    }
    let coordinator_root = git_dir(coordinator_repository)?;
    let coordinator_secrets = FileSecrets {
        root: coordinator_root.join("orkia/keys"),
    };
    let coordinator_ledger =
        open_repository_ledger(coordinator_git, &coordinator_root, &coordinator_secrets)?;

    let paths = repository_paths
        .iter()
        .map(|value| parse_changeset_repository_path(value))
        .collect::<Result<BTreeMap<_, _>>>()?;

    // Validate the complete ChangeSet dependency closure, not just its direct
    // edges. This prevents a cycle or a vanished prerequisite from being
    // hidden behind an otherwise valid signed coordinator object.
    let mut all_changesets = BTreeMap::from([(changeset.id.clone(), changeset.clone())]);
    let mut pending = vec![changeset.clone()];
    while let Some(current) = pending.pop() {
        for dependency in &current.depends_on {
            if all_changesets.contains_key(dependency) {
                continue;
            }
            let (_, dependency_changeset) = coordinator_git
                .semantic_store()
                .latest_changeset(dependency, &coordinator_policy)?
                .ok_or_else(|| {
                    OrkiaError::NotFound(format!("published ChangeSet dependency {}", dependency.0))
                })?;
            all_changesets.insert(dependency.clone(), dependency_changeset.clone());
            pending.push(dependency_changeset);
        }
    }
    orkia_changesets::changeset_execution_order(
        &all_changesets.values().cloned().collect::<Vec<_>>(),
    )?;

    // A signed dependency ref is not equivalent to a delivered dependency.
    // Reconstruct every prerequisite's exact stack revisions and require its
    // projections to be forge-published before allowing the dependent group
    // to integrate.
    for dependency in all_changesets
        .values()
        .filter(|candidate| candidate.id != changeset.id)
    {
        if !matches!(
            dependency.status,
            orkia_model::StackPullRequestStatus::Integrated
        ) {
            return Err(OrkiaError::Policy(format!(
                "ChangeSet {} depends on ChangeSet {} which is not integrated",
                changeset.id.0, dependency.id.0
            )));
        }
        let readiness = change_set_readiness(dependency, &paths)?;
        if !readiness.ready_for_integration {
            return Err(OrkiaError::Policy(format!(
                "ChangeSet {} depends on unpublished ChangeSet {}",
                changeset.id.0, dependency.id.0
            )));
        }
    }
    let readiness = change_set_readiness(&changeset, &paths)?;
    if !readiness.ready_for_integration {
        return Err(OrkiaError::Policy(format!(
            "ChangeSet {} is not ready: every selected StackPullRequest revision must have a published projection",
            changeset.id.0
        )));
    }

    let mut evaluated = BTreeSet::new();
    for reference in &changeset.stacks {
        let path = paths.get(&reference.repository).ok_or_else(|| {
            OrkiaError::Invalid(format!(
                "missing --repository-path for referenced repository {}",
                reference.repository.0
            ))
        })?;
        let foreign_git = LibGit2Repository::open(path)?;
        let foreign_root = git_dir(path)?;
        let foreign_secrets = FileSecrets {
            root: foreign_root.join("orkia/keys"),
        };
        let policy = load_repository_policy(path)?;
        let (_, stack) = foreign_git
            .semantic_store()
            .stack_at_revision(&reference.stack, reference.revision, &policy)?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!(
                    "Stack {} revision {} in repository {}",
                    reference.stack.0, reference.revision, reference.repository.0
                ))
            })?;
        let pull_request_revisions = if stack.pull_request_revisions.is_empty() {
            stack
                .pull_requests
                .iter()
                .map(|pull_request| (pull_request, 0))
                .collect::<Vec<_>>()
        } else {
            stack
                .pull_request_revisions
                .iter()
                .map(|(pull_request, revision)| (pull_request, *revision))
                .collect::<Vec<_>>()
        };
        for (pull_request_id, pull_request_revision) in pull_request_revisions {
            let (_, pull_request) = foreign_git
                .semantic_store()
                .stack_pull_request_at_revision(pull_request_id, pull_request_revision, &policy)?
                .ok_or_else(|| {
                    OrkiaError::NotFound(format!(
                        "StackPullRequest {} revision {}",
                        pull_request_id.0, pull_request_revision
                    ))
                })?;
            let source_plan = pull_request.source_plan.clone().ok_or_else(|| {
                OrkiaError::Integrity(format!(
                    "StackPullRequest {} has no signed source review plan",
                    pull_request.id.0
                ))
            })?;
            let review = signed_review_plan(
                &foreign_root,
                &foreign_git,
                path,
                &source_plan.0.to_string(),
            )?;
            if review.revision != pull_request.source_plan_revision {
                return Err(OrkiaError::Integrity(format!(
                    "StackPullRequest {} selects review plan revision {}, but the signed plan is revision {}",
                    pull_request.id.0, pull_request.source_plan_revision, review.revision
                )));
            }
            let ledger = open_repository_ledger(&foreign_git, &foreign_root, &foreign_secrets)?;
            let validations = run_validations(path, &policy, &ledger)?;
            orkia_policy::evaluate(&policy, &review, &validations, approvals, branch)?;
            evaluated.insert((reference.repository.clone(), pull_request.id.clone()));
        }
    }
    let identity = load_identity(&coordinator_root, &coordinator_secrets)?;
    let mut integrated = changeset.clone();
    integrated.revision += 1;
    integrated.status = orkia_model::StackPullRequestStatus::Integrated;
    integrated.supersedes = Some(changeset_object);
    coordinator_git
        .semantic_store()
        .store_changeset(&integrated, &identity)?;
    coordinator_ledger.append(CaptureEvent::IntegrationEvaluated {
        plan: None,
        changeset: Some(integrated.id.clone()),
        branch: branch.into(),
        approvals,
        passed: true,
        reason: "all dependency and repository policies passed".into(),
    })?;
    coordinator_ledger.append(CaptureEvent::ChangeSetIntegrated {
        changeset: integrated.id.clone(),
        revision: integrated.revision,
    })?;
    println!(
        "ChangeSet {} revision {} integration policy passed for {} selected StackPullRequest(s) in topological order",
        integrated.id.0,
        integrated.revision,
        evaluated.len()
    );
    for step in readiness.execution_order {
        println!(
            "{}:{} revision {} published={}",
            step.repository.0, step.pull_request.0, step.revision, step.published
        );
    }
    Ok(())
}

fn parse_changeset_stack_reference(value: &str) -> Result<ChangeSetStack> {
    let (repository, stack) = value.trim().split_once(':').ok_or_else(|| {
        OrkiaError::Invalid("a stack reference must be <repository-uuid>:<stack-uuid>".into())
    })?;
    let repository = repository
        .parse::<uuid::Uuid>()
        .map(RepositoryId)
        .map_err(|error| OrkiaError::Invalid(format!("invalid stack repository ID: {error}")))?;
    let stack = stack
        .parse::<uuid::Uuid>()
        .map(StackId)
        .map_err(|error| OrkiaError::Invalid(format!("invalid stack ID: {error}")))?;
    Ok(ChangeSetStack {
        repository,
        stack,
        revision: 0,
    })
}

fn parse_changeset_repository_path(value: &str) -> Result<(RepositoryId, PathBuf)> {
    let (repository, path) = value.trim().split_once('=').ok_or_else(|| {
        OrkiaError::Invalid("a repository path must be <repository-uuid>=<absolute-path>".into())
    })?;
    let repository = repository
        .parse::<uuid::Uuid>()
        .map(RepositoryId)
        .map_err(|error| OrkiaError::Invalid(format!("invalid repository path ID: {error}")))?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(OrkiaError::Invalid(
            "a changeset repository path must be absolute".into(),
        ));
    }
    Ok((repository, path))
}

/// Discovers one repository-local Stack per repository from signed refs and
/// captured session objectives. This is the automatic composition boundary:
/// callers never provide Stack IDs or PR order, and Orkia fails closed when
/// the repositories do not share a causal objective.
fn discover_causally_related_stacks(
    repositories: &[PathBuf],
) -> Result<(BTreeSet<ChangeSetStack>, String)> {
    if repositories.len() < 2 {
        return Err(OrkiaError::Invalid(
            "automatic ChangeSet coordination needs at least two repositories".into(),
        ));
    }
    #[derive(Clone)]
    struct Candidate {
        repository: RepositoryId,
        stack: StackId,
        revision: u32,
        objective: String,
        freshness: usize,
    }
    let mut candidates = Vec::new();
    let mut seen_repositories = BTreeSet::new();
    for path in repositories {
        let root = git_dir(path)?;
        let repository: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
        if !seen_repositories.insert(repository.clone()) {
            return Err(OrkiaError::Invalid(format!(
                "automatic ChangeSet coordination received repository {} twice",
                repository.0
            )));
        }
        let git = LibGit2Repository::open(path)?;
        let policy = load_repository_policy(path)?;
        let events = git.ledger_store().read_all()?;
        let mut objectives = BTreeMap::new();
        let mut plan_freshness = BTreeMap::new();
        for (ordinal, event) in events.iter().enumerate() {
            match &event.unsigned.event {
                CaptureEvent::SessionStarted {
                    session, objective, ..
                } => {
                    objectives.insert(session.clone(), normalize_coordination_objective(objective));
                }
                CaptureEvent::AgentAction {
                    session: Some(session),
                    action: orkia_model::AgentActionKind::Prompt { content },
                    ..
                } => {
                    // The provider prompt is the durable intent signal. The
                    // synthetic SessionStarted objective contains only the
                    // external session ID and is therefore not useful for
                    // correlating independent repositories.
                    objectives.insert(session.clone(), normalize_coordination_objective(content));
                }
                CaptureEvent::ReviewPlanCreated { plan, .. } => {
                    plan_freshness.insert(plan.clone(), ordinal);
                }
                _ => {}
            }
        }
        let mut repository_candidates = Vec::new();
        for stack in git.semantic_store().latest_stacks(&policy)? {
            let Some((pull_request_id, pull_request_revision)) = stack
                .pull_request_revisions
                .iter()
                .max_by_key(|(id, revision)| (**revision, (*id).clone()))
            else {
                continue;
            };
            let Some((_, pull_request)) = git.semantic_store().stack_pull_request_at_revision(
                pull_request_id,
                *pull_request_revision,
                &policy,
            )?
            else {
                continue;
            };
            let Some(objective) = objectives.get(&pull_request.session) else {
                continue;
            };
            if objective.is_empty() {
                continue;
            }
            repository_candidates.push(Candidate {
                repository: repository.clone(),
                stack: stack.id,
                revision: stack.revision,
                objective: objective.clone(),
                freshness: pull_request
                    .source_plan
                    .as_ref()
                    .and_then(|plan| plan_freshness.get(plan).copied())
                    .unwrap_or_default(),
            });
        }
        let Some(candidate) = repository_candidates.into_iter().max_by(|left, right| {
            (left.freshness, left.revision, &left.stack).cmp(&(
                right.freshness,
                right.revision,
                &right.stack,
            ))
        }) else {
            return Err(OrkiaError::NotFound(format!(
                "no causally captured Stack found in {}",
                path.display()
            )));
        };
        candidates.push(candidate);
    }
    let mut groups = BTreeMap::<String, Vec<Candidate>>::new();
    for candidate in candidates {
        groups
            .entry(candidate.objective.clone())
            .or_default()
            .push(candidate);
    }
    let Some((objective, group)) =
        groups
            .into_iter()
            .max_by(|(left_key, left), (right_key, right)| {
                (left.len(), right_key).cmp(&(right.len(), left_key))
            })
    else {
        return Err(OrkiaError::NotFound(
            "no captured objectives available for automatic ChangeSet coordination".into(),
        ));
    };
    if group.len() != repositories.len() {
        return Err(OrkiaError::Policy(format!(
            "automatic ChangeSet coordination requires one shared captured objective; found {}/{} repositories matching `{objective}`",
            group.len(),
            repositories.len()
        )));
    }
    let references = group
        .into_iter()
        .map(|candidate| ChangeSetStack {
            repository: candidate.repository,
            stack: candidate.stack,
            revision: candidate.revision,
        })
        .collect();
    Ok((references, objective))
}

fn normalize_coordination_objective(value: &str) -> String {
    value
        .split_whitespace()
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn verify_referenced_stacks(
    references: &BTreeSet<ChangeSetStack>,
    repository_paths: &BTreeMap<RepositoryId, PathBuf>,
) -> Result<BTreeSet<ChangeSetStack>> {
    let mut verified = BTreeSet::new();
    for reference in references {
        let path = repository_paths.get(&reference.repository).ok_or_else(|| {
            OrkiaError::Invalid(format!(
                "missing --repository-path for referenced repository {}",
                reference.repository.0
            ))
        })?;
        let git_root = git_dir(path)?;
        let declared: RepositoryId = read_json(&git_root.join("orkia/repository.json"))?;
        if declared != reference.repository {
            return Err(OrkiaError::Integrity(format!(
                "repository path {} declares {}, not {}",
                path.display(),
                declared.0,
                reference.repository.0
            )));
        }
        let git = LibGit2Repository::open(path)?;
        let policy = load_repository_policy(path)?;
        let (_, stack) = git
            .semantic_store()
            .latest_stack(&reference.stack, &policy)?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!(
                    "referenced stack {} in repository {}",
                    reference.stack.0, reference.repository.0
                ))
            })?;
        if stack.repository != reference.repository || stack.id != reference.stack {
            return Err(OrkiaError::Integrity(
                "a referenced stack does not match its declared repository identity".into(),
            ));
        }
        verified.insert(ChangeSetStack {
            repository: reference.repository.clone(),
            stack: reference.stack.clone(),
            revision: stack.revision,
        });
    }
    Ok(verified)
}

/// Computes publication readiness without consulting any local index or
/// mutable worktree plan. A ChangeSet selects exact Stack and
/// StackPullRequest revisions, and only a `Published` projection of that same
/// revision can make it ready.
fn change_set_readiness(
    change_set: &orkia_model::ChangeSet,
    repository_paths: &BTreeMap<RepositoryId, PathBuf>,
) -> Result<ChangeSetReadiness> {
    let mut stacks = Vec::new();
    let mut ready_for_integration = true;
    let mut selected_pull_requests = Vec::new();
    let mut selected_metadata = BTreeMap::new();
    for reference in &change_set.stacks {
        let path = repository_paths.get(&reference.repository).ok_or_else(|| {
            OrkiaError::Invalid(format!(
                "missing --repository-path for referenced repository {}",
                reference.repository.0
            ))
        })?;
        let git_root = git_dir(path)?;
        let declared: RepositoryId = read_json(&git_root.join("orkia/repository.json"))?;
        if declared != reference.repository {
            return Err(OrkiaError::Integrity(format!(
                "repository path {} declares {}, not {}",
                path.display(),
                declared.0,
                reference.repository.0
            )));
        }
        let git = LibGit2Repository::open(path)?;
        let policy = load_repository_policy(path)?;
        let (_, stack) = git
            .semantic_store()
            .stack_at_revision(&reference.stack, reference.revision, &policy)?
            .ok_or_else(|| {
                OrkiaError::NotFound(format!(
                    "Stack {} revision {} in repository {}",
                    reference.stack.0, reference.revision, reference.repository.0
                ))
            })?;
        if stack.repository != reference.repository || stack.id != reference.stack {
            return Err(OrkiaError::Integrity(
                "referenced Stack does not match its declared repository identity".into(),
            ));
        }
        let pull_request_revisions = if stack.pull_request_revisions.is_empty() {
            stack
                .pull_requests
                .iter()
                .map(|pull_request| (pull_request, 0))
                .collect::<Vec<_>>()
        } else {
            stack
                .pull_request_revisions
                .iter()
                .map(|(pull_request, revision)| (pull_request, *revision))
                .collect::<Vec<_>>()
        };
        let mut published = true;
        for (pull_request, revision) in pull_request_revisions {
            let (_, selected_pull_request) = git
                .semantic_store()
                .stack_pull_request_at_revision(pull_request, revision, &policy)?
                .ok_or_else(|| {
                    OrkiaError::NotFound(format!(
                        "StackPullRequest {} revision {}",
                        pull_request.0, revision
                    ))
                })?;
            let projection = git
                .semantic_store()
                .latest_projection_for_stack_pull_request_revision(
                    pull_request,
                    revision,
                    &policy,
                )?;
            let projection_published = projection.as_ref().is_some_and(|(_, projection)| {
                orkia_changesets::projection_is_published_for(
                    projection,
                    &reference.repository,
                    pull_request,
                    revision,
                )
            });
            published &= projection_published;
            selected_metadata.insert(
                (reference.repository.clone(), pull_request.clone()),
                (revision, projection_published),
            );
            selected_pull_requests.push(selected_pull_request);
        }
        ready_for_integration &= published;
        stacks.push(ChangeSetStackReadiness {
            repository: reference.repository.clone(),
            stack: stack.id,
            revision: stack.revision,
            pull_request_count: stack.pull_requests.len(),
            published,
        });
    }
    let execution_order =
        orkia_changesets::stack_pull_request_execution_order(&selected_pull_requests)?
            .into_iter()
            .map(|(repository, pull_request)| {
                let (revision, published) = selected_metadata
                    .get(&(repository.clone(), pull_request.clone()))
                    .copied()
                    .ok_or_else(|| {
                        OrkiaError::Integrity(
                    "topological stack order contains a pull request not selected by the ChangeSet"
                        .into(),
                )
                    })?;
                Ok(ChangeSetExecutionStep {
                    repository,
                    pull_request,
                    revision,
                    published,
                })
            })
            .collect::<Result<Vec<_>>>()?;
    Ok(ChangeSetReadiness {
        id: change_set.id.clone(),
        revision: change_set.revision,
        ready_for_integration,
        stacks,
        execution_order,
    })
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
    let policy_digest = orkia_model::repository_policy_digest(policy)?;
    let changes = git
        .changes_since(base_commit)?
        .into_iter()
        .filter(|change| !is_orkia_owned_path(&change.path))
        .collect::<Vec<_>>();
    let scoped_events = session_events(events, session)?;
    let source_events = scoped_events
        .iter()
        .map(|event| event.unsigned.id.clone())
        .collect::<BTreeSet<_>>();
    let atoms = changes
        .iter()
        .flat_map(|change| {
            let path_events = scoped_events
                .iter()
                .filter(|event| event_covers_path(&event.unsigned.event, &change.path))
                .map(|event| event.unsigned.id.clone())
                .collect::<BTreeSet<_>>();
            let source_events = if path_events.is_empty() {
                source_events.clone()
            } else {
                path_events
            };
            let ranges = changed_line_ranges(&change.old_content, &change.new_content);
            let ranges = if ranges.is_empty() {
                vec![(change.changed_start, change.changed_end)]
            } else {
                ranges
            };
            extract_atoms_in_ranges(
                &ChangedFile {
                    path: change.path.clone(),
                    changed_start: change.changed_start,
                    changed_end: change.changed_end,
                    content: change.new_content.clone(),
                    source_events: source_events.clone(),
                },
                &ranges,
            )
        })
        .collect::<Vec<_>>();
    if atoms.is_empty() {
        return Ok(None);
    }
    let coverage_milli = causal_coverage_milli(&changes, &scoped_events);
    Ok(Some(plan(PlanningInput {
        checkpoint,
        policy_digest: Some(policy_digest),
        dependencies: infer_dependencies(&atoms),
        atoms,
        coverage_milli,
        minimum_coverage_milli: policy.minimum_coverage_milli,
        minimum_confidence_milli: policy.minimum_confidence_milli,
        source_events,
    })))
}

/// Materializes the provider's first captured prompt as a signed semantic
/// Intent. SessionStarted is intentionally only a fallback: provider prompts
/// are the authoritative user intent and are what the automatic ChangeSet
/// coordinator correlates across repositories.
fn automatic_intent(
    git: &LibGit2Repository,
    identity: &Identity,
    events: &[orkia_model::LedgerEvent],
    session: &SessionId,
) -> Result<orkia_model::SemanticObjectRef> {
    let scoped = session_events(events, session)?;
    let body = scoped
        .iter()
        .find_map(|event| match &event.unsigned.event {
            CaptureEvent::AgentAction {
                session: Some(recorded),
                action: orkia_model::AgentActionKind::Prompt { content },
                ..
            } if recorded == session && !content.trim().is_empty() => Some(content.trim()),
            _ => None,
        })
        .or_else(|| {
            scoped.iter().find_map(|event| match &event.unsigned.event {
                CaptureEvent::SessionStarted {
                    session: recorded,
                    objective,
                    ..
                } if recorded == session && !objective.trim().is_empty() => Some(objective.trim()),
                _ => None,
            })
        })
        .ok_or_else(|| OrkiaError::Invalid("automatic review has no captured intent".into()))?;
    let title = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Captured work")
        .chars()
        .take(120)
        .collect::<String>();
    let intent = Intent {
        schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
        title,
        body: body.to_owned(),
        session: Some(session.clone()),
        evidence: BTreeSet::new(),
    };
    let object = git.semantic_store().put_intent(&intent)?;
    git.semantic_store().sign_document(&object, identity)?;
    Ok(object)
}

// This is the CLI composition boundary. It deliberately carries the complete
// capture context needed to atomically publish the derived semantic objects.
#[allow(clippy::too_many_arguments)]
fn persist_review_plan(
    root: &Path,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
    git: &LibGit2Repository,
    secrets: &FileSecrets,
    plan: &ReviewPlan,
    checkpoint: &str,
    session: &SessionId,
    repository: &RepositoryId,
    base_commit: &str,
    changes: &[orkia_model::FileChange],
    repository_path: &Path,
) -> Result<()> {
    let identity = load_identity(root, secrets)?;
    let policy = load_repository_policy(repository_path)?;
    // The policy is input to confidence, projection and integration. Bind its
    // canonical semantic digest into the signed plan before any derived object
    // is published; a later policy edit must produce a fresh review instead of
    // silently changing the decision represented by this plan.
    let mut signed_plan = plan.clone();
    signed_plan.policy_digest = Some(orkia_model::repository_policy_digest(&policy)?);
    let plan = &signed_plan;
    let destination = root.join("orkia/plans").join(format!("{}.json", plan.id.0));
    write_json(&destination, plan)?;
    match git.semantic_store().latest_review_plan(&plan.id, &policy) {
        Ok(existing) if existing.revision == plan.revision && existing == *plan => {
            // Hook delivery is at-least-once. The same causal plan has already
            // been signed and published, so replaying it must not manufacture
            // StackPullRequest or stack revisions.
            return Ok(());
        }
        Ok(existing) if existing.revision == plan.revision => {
            return Err(OrkiaError::Integrity(format!(
                "review plan {} revision {} is already bound to different content",
                existing.id.0, existing.revision
            )));
        }
        Ok(_) | Err(OrkiaError::NotFound(_)) => {}
        Err(error) => return Err(error),
    }
    git.semantic_store().store_review_plan(plan, &identity)?;
    let events = git.ledger_store().read_all()?;
    let intent = automatic_intent(git, &identity, &events, session)?;
    // Automatic publication is itself a review checkpoint.  Capture the
    // policy validations before deriving StackPullRequests so every durable
    // unit carries the exact validation results that were available when it
    // was published.  Integration re-runs them later as a separate gate.
    let validations = run_validations(repository_path, &policy, ledger)?;
    let mut pull_requests = orkia_changesets::from_review_plan(
        plan,
        session.clone(),
        repository.clone(),
        base_commit.into(),
    )?;
    for pull_request in &mut pull_requests {
        pull_request.intent = Some(intent.clone());
        pull_request.validations = validations.clone();
        orkia_projection::bind_patches(pull_request, changes)?;
        if let Some((previous, current)) = git
            .semantic_store()
            .latest_stack_pull_request(&pull_request.id, &policy)?
        {
            pull_request.revision = current.revision + 1;
            pull_request.status = orkia_model::StackPullRequestStatus::Active;
            pull_request.supersedes = Some(previous);
        } else {
            pull_request.status = orkia_model::StackPullRequestStatus::Active;
        }
        let object = git
            .semantic_store()
            .store_stack_pull_request(pull_request, &identity)?;
        ledger.append(CaptureEvent::StackPullRequestPublished {
            pull_request: pull_request.id.clone(),
            revision: pull_request.revision,
            object,
        })?;
    }
    let mut stack = orkia_changesets::stack(&pull_requests)?;
    if let Some((previous, current)) = git.semantic_store().latest_stack(&stack.id, &policy)? {
        if current.pull_requests != stack.pull_requests
            || current.pull_request_revisions != stack.pull_request_revisions
            || current.roots != stack.roots
        {
            stack.revision = current.revision + 1;
            stack.supersedes = Some(previous);
        } else {
            stack = current;
        }
    }
    git.semantic_store().store_stack(&stack, &identity)?;
    let mut change_set = orkia_changesets::change_set(std::slice::from_ref(&stack))?;
    if let Some((previous, current)) = git
        .semantic_store()
        .latest_changeset(&change_set.id, &policy)?
    {
        if current.stacks != change_set.stacks
            || current.depends_on != change_set.depends_on
            || current.status != change_set.status
        {
            change_set.revision = current.revision + 1;
            change_set.supersedes = Some(previous);
        } else {
            change_set = current;
        }
    }
    let object = git
        .semantic_store()
        .store_changeset(&change_set, &identity)?;
    ledger.append(CaptureEvent::ChangeSetPublished {
        changeset: change_set.id.clone(),
        revision: change_set.revision,
        object,
    })?;
    ledger.append(CaptureEvent::ReviewPlanCreated {
        plan: plan.id.clone(),
        checkpoint: checkpoint.into(),
        atom_count: plan.atoms.len() as u32,
        coverage_milli: plan.coverage_milli,
    })?;
    let repository_path_buf = repository_path.to_path_buf();
    maybe_submit_changeset(
        &change_set,
        &identity,
        std::slice::from_ref(&repository_path_buf),
    )?;
    maybe_auto_project_and_publish(git, root, repository_path, secrets, &plan.id);
    maybe_auto_coordinate_changeset(git, root, repository_path, secrets);
    Ok(())
}

/// Turns an automatically-created plan into a projected branch and, when the
/// forge target is configured, publishes its PR without a second author
/// command. Failures are retained as a deferred operation: capture and the
/// signed plan remain durable and the next checkpoint retries the same plan.
fn maybe_auto_project_and_publish(
    git: &LibGit2Repository,
    root: &Path,
    repository: &Path,
    secrets: &FileSecrets,
    plan: &orkia_model::PlanId,
) {
    if std::env::var("ORKIA_AUTO_PROJECT").as_deref() != Ok("1") {
        return;
    }
    let plan_id = plan.0.to_string();
    if let Err(error) = handle_review(
        ReviewCommand::Project {
            plan: plan_id.clone(),
        },
        git,
        root,
        repository,
        secrets,
    ) {
        eprintln!("automatic review projection deferred: {error}");
        return;
    }
    let Ok(target) = std::env::var("ORKIA_AUTO_PUBLISH_GITHUB") else {
        return;
    };
    let Some((github_owner, github_repository)) = target.split_once('/') else {
        eprintln!(
            "automatic forge publication deferred: ORKIA_AUTO_PUBLISH_GITHUB must be owner/repository"
        );
        return;
    };
    let base = std::env::var("ORKIA_AUTO_PUBLISH_BASE").unwrap_or_else(|_| "main".into());
    let remote = std::env::var("ORKIA_AUTO_PUBLISH_REMOTE").unwrap_or_else(|_| "origin".into());
    if let Err(error) = handle_review(
        ReviewCommand::Publish {
            plan: plan_id,
            github_owner: github_owner.into(),
            github_repository: github_repository.into(),
            base,
            remote,
        },
        git,
        root,
        repository,
        secrets,
    ) {
        eprintln!("automatic forge publication deferred: {error}");
    }
}

/// If a repository registry is configured, every automatic plan publication
/// attempts to compose the latest causally-related stacks without requiring an
/// author to name stacks or PRs. A repository that is not ready yet simply
/// defers coordination; its local signed plan remains valid and the next
/// checkpoint retries the deterministic composition.
fn maybe_auto_coordinate_changeset(
    git: &LibGit2Repository,
    root: &Path,
    repository: &Path,
    secrets: &FileSecrets,
) {
    let Ok(raw_paths) = std::env::var("ORKIA_AUTO_COORDINATE_REPOSITORIES") else {
        return;
    };
    let paths = raw_paths
        .split(if cfg!(windows) { ';' } else { ':' })
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.len() < 2 || !paths.iter().any(|path| path == repository) {
        return;
    }
    if let Err(error) = handle_changeset(
        ChangeSetCommand::Auto {
            repository_path: paths,
            depends_on: Vec::new(),
        },
        git,
        root,
        repository,
        secrets,
    ) {
        eprintln!("automatic ChangeSet coordination deferred: {error}");
    }
}

/// Submit the signed, content-free ChangeSet envelope when a backend is
/// configured. Offline repositories remain fully functional; publication is
/// retried by the same deterministic payload on the next checkpoint.
fn maybe_submit_changeset(
    change_set: &orkia_model::ChangeSet,
    identity: &Identity,
    repository_paths: &[PathBuf],
) -> Result<()> {
    let Some(base_url) = std::env::var_os("ORKIA_BACKEND_URL") else {
        return Ok(());
    };
    let base_url = base_url.to_string_lossy().trim_end_matches('/').to_owned();
    let stacks = change_set
        .stacks
        .iter()
        .map(|stack| WireChangeSetStack {
            repository_id: stack.repository.clone(),
            stack_id: stack.stack.clone(),
            revision: stack.revision,
        })
        .collect::<Vec<_>>();
    let proof_metadata = changeset_proof_metadata(change_set, repository_paths)?;
    let proofs = stacks
        .iter()
        .map(|stack| {
            let metadata = proof_metadata
                .get(&(
                    stack.repository_id.clone(),
                    stack.stack_id.clone(),
                    stack.revision,
                ))
                .ok_or_else(|| {
                    OrkiaError::Integrity(format!(
                        "missing causal metadata for ChangeSet stack {} revision {}",
                        stack.stack_id.0, stack.revision
                    ))
                })?;
            Ok(WireChangeSetProof {
                repository_id: stack.repository_id.clone(),
                stack_id: stack.stack_id.clone(),
                revision: stack.revision,
                session_id: metadata.0.clone(),
                validation_count: metadata.1,
                refs: vec![
                    format!("refs/orkia/stacks/{}/{}", stack.stack_id.0, stack.revision),
                    format!(
                        "refs/orkia/changesets/{}/{}",
                        change_set.id.0, change_set.revision
                    ),
                ],
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let payload = WireChangeSetPayload {
        wire_version: 1,
        signer_id: identity.actor().id.0,
        changeset_id: change_set.id.clone(),
        revision: change_set.revision,
        coordinator_repository_id: stacks
            .first()
            .map(|stack| stack.repository_id.clone())
            .ok_or_else(|| OrkiaError::Invalid("ChangeSet has no stacks".into()))?,
        status: format!("{:?}", change_set.status).to_lowercase(),
        stacks,
        depends_on: change_set.depends_on.iter().cloned().collect(),
        proofs,
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|error| {
        OrkiaError::Invalid(format!("serialize ChangeSet wire payload: {error}"))
    })?;
    let envelope = WireChangeSetSubmission {
        wire_version: 1,
        submission_id: uuid::Uuid::new_v4(),
        signer: WireChangeSetSigner {
            id: identity.actor().id.0,
            display_name: identity.actor().display_name.clone(),
            public_key: identity.actor().public_key.clone(),
        },
        payload_base64: STANDARD_NO_PAD.encode(&payload_bytes),
        signature: identity.sign(&payload_bytes),
    };
    let client = reqwest::blocking::Client::new();
    let mut request = client.post(format!("{base_url}/api/v1/changesets"));
    if let Some(token) =
        std::env::var_os("ORKIA_BACKEND_TOKEN").or_else(|| std::env::var_os("ORKIA_SERVICE_TOKEN"))
    {
        request = request.bearer_auth(token.to_string_lossy());
    }
    let response = request
        .json(&envelope)
        .send()
        .map_err(|error| OrkiaError::External(format!("ChangeSet backend submission: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        return Err(OrkiaError::External(format!(
            "ChangeSet backend submission returned {status}: {detail}"
        )));
    }
    eprintln!(
        "ChangeSet {} revision {} submitted to backend",
        change_set.id.0, change_set.revision
    );
    Ok(())
}

/// Reconstruct the provenance of each exact Stack revision before submitting
/// a ChangeSet.  The backend receives only signed metadata; the authoritative
/// session and validation values are read from the repository's signed Git
/// objects, never inferred from a branch name or a client-side cache.
fn changeset_proof_metadata(
    change_set: &orkia_model::ChangeSet,
    repository_paths: &[PathBuf],
) -> Result<BTreeMap<ChangeSetProofKey, ChangeSetProofMetadata>> {
    let mut repositories = BTreeMap::<RepositoryId, (LibGit2Repository, RepositoryPolicy)>::new();
    for path in repository_paths {
        let root = git_dir(path)?;
        let repository: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
        if repositories.contains_key(&repository) {
            continue;
        }
        let git = LibGit2Repository::open(path)?;
        let policy = load_repository_policy(path)?;
        repositories.insert(repository, (git, policy));
    }
    let mut metadata = BTreeMap::new();
    for stack in &change_set.stacks {
        let (git, policy) = repositories.get(&stack.repository).ok_or_else(|| {
            OrkiaError::NotFound(format!(
                "repository {} is not available for ChangeSet provenance",
                stack.repository.0
            ))
        })?;
        let Some((_, stored_stack)) =
            git.semantic_store()
                .stack_at_revision(&stack.stack, stack.revision, policy)?
        else {
            return Err(OrkiaError::NotFound(format!(
                "stack {} revision {} for ChangeSet provenance",
                stack.stack.0, stack.revision
            )));
        };
        let mut sessions = BTreeSet::new();
        let mut validation_count = 0;
        for (pull_request_id, pull_request_revision) in &stored_stack.pull_request_revisions {
            let Some((_, pull_request)) = git.semantic_store().stack_pull_request_at_revision(
                pull_request_id,
                *pull_request_revision,
                policy,
            )?
            else {
                return Err(OrkiaError::NotFound(format!(
                    "stack pull request {} revision {} for ChangeSet provenance",
                    pull_request_id.0, pull_request_revision
                )));
            };
            sessions.insert(pull_request.session);
            validation_count += pull_request.validations.len();
        }
        let Some(session) = sessions.into_iter().next() else {
            return Err(OrkiaError::Integrity(format!(
                "stack {} revision {} has no causal session",
                stack.stack.0, stack.revision
            )));
        };
        metadata.insert(
            (
                stack.repository.clone(),
                stack.stack.clone(),
                stack.revision,
            ),
            (session, validation_count),
        );
    }
    Ok(metadata)
}

fn load_repository_policy(repository: &Path) -> Result<orkia_model::RepositoryPolicy> {
    let path = repository.join("orkia.toml");
    if path.exists() {
        let content = fs::read_to_string(&path)
            .map_err(|error| OrkiaError::NotFound(format!("{}: {error}", path.display())))?;
        orkia_policy::parse(&content)
    } else {
        Ok(orkia_model::RepositoryPolicy::default())
    }
}

/// The worktree copy is only a user-facing lookup by ID. Decisions always use
/// the newest revision whose bytes and signature are present in Git refs.
fn signed_review_plan(
    root: &Path,
    git: &LibGit2Repository,
    repository: &Path,
    plan_id: &str,
) -> Result<ReviewPlan> {
    let local = root.join("orkia/plans").join(format!("{plan_id}.json"));
    let id = if local.exists() {
        read_json::<ReviewPlan>(&local)?.id
    } else {
        plan_id
            .parse::<uuid::Uuid>()
            .map(orkia_model::PlanId)
            .map_err(|error| OrkiaError::Invalid(format!("invalid review plan id: {error}")))?
    };
    let policy = load_repository_policy(repository)?;
    let plan = git.semantic_store().latest_review_plan(&id, &policy)?;
    let expected_policy = orkia_model::repository_policy_digest(&policy)?;
    if plan.policy_digest.as_deref() != Some(expected_policy.as_str()) {
        return Err(OrkiaError::Policy(format!(
            "signed review plan {} was created under a different repository policy; create a new review plan",
            plan.id.0
        )));
    }
    Ok(plan)
}

fn persist_revised_plan(
    root: &Path,
    git: &LibGit2Repository,
    repository: &Path,
    secrets: &FileSecrets,
    revised: &ReviewPlan,
    reason: &str,
) -> Result<()> {
    let events = git.ledger_store().read_all()?;
    let (session, base, repository_id) = plan_session_context(&events, revised)?;
    let changes = git.changes_since(&base)?;
    let ledger = open_repository_ledger(git, root, secrets)?;
    persist_review_plan(
        root,
        &ledger,
        git,
        secrets,
        revised,
        &revised.source_checkpoint,
        &session,
        &repository_id,
        &base,
        &changes,
        repository,
    )?;
    match revised.status {
        orkia_model::PlanStatus::Approved => ledger.append(CaptureEvent::ReviewPlanApproved {
            plan: revised.id.clone(),
            revision: revised.revision,
        })?,
        orkia_model::PlanStatus::ChangesRequested => {
            ledger.append(CaptureEvent::ReviewPlanChangesRequested {
                plan: revised.id.clone(),
                revision: revised.revision,
                reason: reason.into(),
            })?
        }
        _ => ledger.append(CaptureEvent::ReviewPlanRevised {
            plan: revised.id.clone(),
            revision: revised.revision,
            reason: reason.into(),
        })?,
    };
    Ok(())
}

/// Resolve the immutable causal session that produced a plan.  A reviewer may
/// correct a plan after another session has started; using the latest global
/// session here would rebind the correction to unrelated files and evidence.
fn plan_session_context(
    events: &[orkia_model::LedgerEvent],
    plan: &ReviewPlan,
) -> Result<(SessionId, String, RepositoryId)> {
    if let Some((session, base_commit, repository)) = events.iter().find_map(|event| {
        if !plan.created_from.contains(&event.unsigned.id) {
            return None;
        }
        match &event.unsigned.event {
            CaptureEvent::SessionStarted {
                session,
                base_commit,
                ..
            } => Some((
                session.clone(),
                base_commit.clone(),
                event.unsigned.repository.clone(),
            )),
            _ => None,
        }
    }) {
        return Ok((session, base_commit, repository));
    }

    // Plans imported from an older schema may not carry `created_from`.  Their
    // checkpoint embeds the signed Checkpoint event id (`<commit>#<uuid>`),
    // which still lets us recover the session by walking backward in the
    // immutable ledger.  If that evidence is absent, fail closed instead of
    // guessing from the newest session.
    let checkpoint_event = plan
        .source_checkpoint
        .rsplit_once('#')
        .and_then(|(_, id)| id.parse::<uuid::Uuid>().ok())
        .map(orkia_model::EventId);
    let checkpoint_index =
        checkpoint_event.and_then(|id| events.iter().position(|event| event.unsigned.id == id));
    if let Some(index) = checkpoint_index {
        for event in events[..=index].iter().rev() {
            if let CaptureEvent::SessionStarted {
                session,
                base_commit,
                ..
            } = &event.unsigned.event
            {
                return Ok((
                    session.clone(),
                    base_commit.clone(),
                    event.unsigned.repository.clone(),
                ));
            }
            if matches!(event.unsigned.event, CaptureEvent::SessionClosed { .. }) {
                break;
            }
        }
    }
    Err(OrkiaError::Integrity(format!(
        "review plan {} has no recoverable source session evidence",
        plan.id.0
    )))
}

fn open_ledger(
    git: &LibGit2Repository,
    root: &Path,
    secrets: &FileSecrets,
) -> Result<(SessionState, Ledger<orkia_git::GitLedgerStore, SystemClock>)> {
    let state = read_state(root)?;
    let identity = Identity::load(secrets, "identity", state.actor.clone())?
        .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
    git.store_actor(identity.actor())?;
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
    git.store_actor(identity.actor())?;
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
        .filter(|path| !is_orkia_owned_path(path))
        .collect())
}

fn is_orkia_owned_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized == "orkia.toml"
        || normalized.starts_with(".orkia/")
        || normalized.starts_with("orkia/")
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
    let end = events
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, event)| match &event.unsigned.event {
            CaptureEvent::SessionClosed { session: closed } if closed == session => Some(index),
            CaptureEvent::SessionStarted {
                session: started, ..
            } if started != session => Some(index),
            _ => None,
        })
        .unwrap_or(events.len());
    Ok(events[start..end].iter().collect())
}

fn causal_coverage_milli(
    changes: &[orkia_model::FileChange],
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
    let every_change_observed = changes.iter().all(|change| {
        observed
            .iter()
            .any(|candidate| repository_path_matches(candidate, &change.path))
    });
    // A watcher may report a write before `orkia run` records the command
    // that mediated it. Treat the path as unknown until a later typed
    // observation closes it; an unrelated human/editor write remains a hard
    // coverage failure at checkpoint time.
    let mut unknown_paths = BTreeSet::new();
    for event in events {
        if let CaptureEvent::FilesObserved {
            modified,
            unknown_write,
            ..
        } = &event.unsigned.event
        {
            if *unknown_write {
                unknown_paths.extend(modified.iter().cloned());
            } else {
                unknown_paths.retain(|path| !modified.contains(path));
            }
        }
    }
    let unknown_write = !unknown_paths.is_empty();
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

/// Returns whether a captured event names one concrete repository path.  The
/// causal evidence attached to an atom is intentionally path-scoped; falling
/// back to the whole session remains the conservative behavior only when no
/// provider exposed a path for that file.
fn event_covers_path(event: &CaptureEvent, path: &str) -> bool {
    match event {
        CaptureEvent::AgentAction {
            action:
                orkia_model::AgentActionKind::FileRead {
                    path: candidate, ..
                }
                | orkia_model::AgentActionKind::FileWrite {
                    path: candidate, ..
                },
            ..
        } => repository_path_matches(candidate, path),
        CaptureEvent::FilesObserved { read, modified, .. } => read
            .iter()
            .chain(modified.iter())
            .any(|candidate| repository_path_matches(candidate, path)),
        CaptureEvent::AgentSessionSnapshot {
            changed_paths,
            observed_paths,
            ..
        } => changed_paths
            .iter()
            .chain(observed_paths.iter())
            .any(|candidate| repository_path_matches(candidate, path)),
        _ => false,
    }
}

fn relative_repository_path(repository: &Path, candidate: &str) -> String {
    let candidate_path = Path::new(candidate);
    candidate_path
        .strip_prefix(repository)
        .unwrap_or(candidate_path)
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_owned()
}

fn repository_path_matches(candidate: &str, path: &str) -> bool {
    let candidate = candidate.replace('\\', "/");
    let path = path.replace('\\', "/");
    candidate == path
        || candidate.trim_start_matches("./") == path.trim_start_matches("./")
        || candidate.ends_with(&format!("/{path}"))
}

fn run_validations(
    repository: &Path,
    policy: &orkia_model::RepositoryPolicy,
    ledger: &Ledger<orkia_git::GitLedgerStore, SystemClock>,
) -> Result<Vec<orkia_model::ValidationResult>> {
    let mut results = Vec::new();
    for command in &policy.validation_commands {
        let output = std::process::Command::new("sh")
            // Validation commands must not source an interactive/login shell:
            // that would make signed results depend on a user's profile and
            // can add unrelated diagnostics to an otherwise passing check.
            .arg("-c")
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

/// Initializes repository-local Orkia metadata without replacing an existing
/// identity or repository ID. Hook installation remains an explicit option on
/// the CLI command because provider settings are commonly user-global.
fn ensure_repository_initialized(
    root: &Path,
    secrets: &FileSecrets,
    requested_name: Option<&str>,
) -> Result<(Actor, RepositoryId, bool)> {
    let actor_path = root.join("orkia/actor.json");
    let key_path = secrets.root.join("identity");
    let (actor, identity_created) = if actor_path.exists() || key_path.exists() {
        let actor: Actor = read_json(&actor_path)?;
        if let Some(requested_name) = requested_name
            && requested_name != actor.display_name
        {
            return Err(OrkiaError::Invalid(format!(
                "repository already has actor `{}`; refusing to replace it with `{requested_name}`",
                actor.display_name
            )));
        }
        let _ = Identity::load(secrets, "identity", actor.clone())?
            .ok_or_else(|| OrkiaError::NotFound("repository identity key is missing".into()))?;
        (actor, false)
    } else {
        let name = requested_name.ok_or_else(|| {
            OrkiaError::Invalid(
                "new repository initialization requires --name for the local identity".into(),
            )
        })?;
        let identity = Identity::generate(name);
        let actor = identity.actor().clone();
        identity.save(secrets, "identity")?;
        write_json(&actor_path, &actor)?;
        (actor, true)
    };

    let repository_path = root.join("orkia/repository.json");
    let repository_id = if repository_path.exists() {
        read_json(&repository_path)?
    } else {
        let repository_id = RepositoryId::new();
        write_json(&repository_path, &repository_id)?;
        repository_id
    };
    fs::create_dir_all(root.join("orkia/plans"))
        .map_err(|error| OrkiaError::External(error.to_string()))?;
    let policy_path = repository_policy_path(root);
    if !policy_path.exists() {
        let policy = toml::to_string_pretty(&RepositoryPolicy::default())
            .map_err(|error| OrkiaError::Invalid(format!("serialize default policy: {error}")))?;
        fs::write(&policy_path, policy).map_err(|error| {
            OrkiaError::External(format!("write {}: {error}", policy_path.display()))
        })?;
    } else {
        let policy = fs::read_to_string(&policy_path).map_err(|error| {
            OrkiaError::External(format!("read {}: {error}", policy_path.display()))
        })?;
        toml::from_str::<RepositoryPolicy>(&policy).map_err(|error| {
            OrkiaError::Invalid(format!("invalid {}: {error}", policy_path.display()))
        })?;
    }
    Ok((actor, repository_id, identity_created))
}

fn repository_policy_path(git_dir: &Path) -> PathBuf {
    git_dir.parent().unwrap_or(git_dir).join("orkia.toml")
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

    #[test]
    fn parses_a_cross_repository_stack_reference() {
        let repository = RepositoryId::new();
        let stack = StackId::new();
        let parsed =
            parse_changeset_stack_reference(&format!("{}:{}", repository.0, stack.0)).unwrap();
        assert_eq!(parsed.repository, repository);
        assert_eq!(parsed.stack, stack);
        assert!(parse_changeset_stack_reference("not-a-reference").is_err());
    }

    #[test]
    fn parses_only_absolute_repository_locations_for_changesets() {
        let repository = RepositoryId::new();
        let (parsed, path) =
            parse_changeset_repository_path(&format!("{}=/tmp/orkia-stack", repository.0)).unwrap();
        assert_eq!(parsed, repository);
        assert_eq!(path, PathBuf::from("/tmp/orkia-stack"));
        assert!(parse_changeset_repository_path(&format!("{}=relative", repository.0)).is_err());
    }

    #[test]
    fn excludes_orkia_owned_metadata_from_causal_changes() {
        assert!(is_orkia_owned_path("orkia.toml"));
        assert!(is_orkia_owned_path("orkia/plans/example.json"));
        assert!(is_orkia_owned_path(".orkia/cache"));
        assert!(!is_orkia_owned_path("src/lib.rs"));
    }

    #[test]
    fn watcher_unknown_write_is_closed_by_a_later_mediated_observation() {
        let changes = vec![orkia_model::FileChange {
            path: "src/lib.rs".into(),
            old_content: "old".into(),
            new_content: "new".into(),
            changed_start: 1,
            changed_end: 1,
        }];
        let events = [
            ledger_event(CaptureEvent::FilesObserved {
                read: BTreeSet::new(),
                modified: BTreeSet::from(["src/lib.rs".into()]),
                unknown_write: true,
            }),
            ledger_event(CaptureEvent::FilesObserved {
                read: BTreeSet::new(),
                modified: BTreeSet::from(["src/lib.rs".into()]),
                unknown_write: false,
            }),
        ];
        let references = events.iter().collect::<Vec<_>>();
        assert_eq!(causal_coverage_milli(&changes, &references), 1000);
    }

    #[test]
    fn session_event_scope_stops_at_close_or_the_next_concurrent_session() {
        let first = SessionId::new();
        let second = SessionId::new();
        let events = vec![
            ledger_event(CaptureEvent::SessionStarted {
                session: first.clone(),
                origin: CaptureOrigin::Human,
                base_commit: "a".repeat(40),
                objective: "first".into(),
            }),
            ledger_event(CaptureEvent::FilesObserved {
                read: BTreeSet::new(),
                modified: BTreeSet::from(["first.rs".into()]),
                unknown_write: false,
            }),
            ledger_event(CaptureEvent::SessionStarted {
                session: second.clone(),
                origin: CaptureOrigin::Human,
                base_commit: "b".repeat(40),
                objective: "second".into(),
            }),
            ledger_event(CaptureEvent::FilesObserved {
                read: BTreeSet::new(),
                modified: BTreeSet::from(["second.rs".into()]),
                unknown_write: false,
            }),
        ];
        let scoped = session_events(&events, &first).unwrap();
        assert_eq!(scoped.len(), 2);
        assert!(matches!(
            scoped[1].unsigned.event,
            CaptureEvent::FilesObserved { .. }
        ));
    }

    #[test]
    fn plan_correction_uses_its_source_session_after_a_new_session_starts() {
        let first = SessionId::new();
        let second = SessionId::new();
        let first_started = ledger_event(CaptureEvent::SessionStarted {
            session: first.clone(),
            origin: CaptureOrigin::Human,
            base_commit: "a".repeat(40),
            objective: "first".into(),
        });
        let second_started = ledger_event(CaptureEvent::SessionStarted {
            session: second,
            origin: CaptureOrigin::Human,
            base_commit: "b".repeat(40),
            objective: "second".into(),
        });
        let plan = ReviewPlan {
            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
            id: orkia_model::PlanId::new(),
            revision: 0,
            source_checkpoint: format!("{}#{}", "a".repeat(40), first_started.unsigned.id.0),
            policy_digest: None,
            units: Vec::new(),
            atom_paths: BTreeMap::new(),
            atoms: Vec::new(),
            coverage_milli: 1000,
            status: orkia_model::PlanStatus::Proposed,
            created_from: BTreeSet::from([first_started.unsigned.id.clone()]),
        };
        let events = vec![first_started.clone(), second_started];
        let (session, base, repository) = plan_session_context(&events, &plan).unwrap();
        assert_eq!(session, first);
        assert_eq!(base, "a".repeat(40));
        assert_eq!(repository, first_started.unsigned.repository);
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
    fn init_is_idempotent_and_refuses_identity_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        git2::Repository::init(&repository).unwrap();
        let git_root = git_dir(&repository).unwrap();
        let secrets = FileSecrets {
            root: git_root.join("orkia/keys"),
        };
        let (first_actor, first_repository, first_created) =
            ensure_repository_initialized(&git_root, &secrets, Some("Phase 0")).unwrap();
        assert!(first_created);
        let key_before = fs::read(secrets.root.join("identity")).unwrap();
        let (second_actor, second_repository, second_created) =
            ensure_repository_initialized(&git_root, &secrets, Some("Phase 0")).unwrap();
        assert!(!second_created);
        assert_eq!(first_actor, second_actor);
        assert_eq!(first_repository, second_repository);
        assert_eq!(key_before, fs::read(secrets.root.join("identity")).unwrap());
        assert!(ensure_repository_initialized(&git_root, &secrets, Some("replacement")).is_err());
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
        let plan_path = plans[0].clone();
        assert_eq!(plan.coverage_milli, 1000);
        assert!(
            !plan.atoms.is_empty(),
            "semantic atoms are persisted with the plan"
        );
        let pull_requests = git
            .semantic_store()
            .stack_pull_requests_for_plan(&plan, &orkia_model::RepositoryPolicy::default())
            .unwrap();
        assert_eq!(pull_requests.len(), 1);
        assert!(
            !pull_requests[0].patches.is_empty(),
            "the authoritative stack pull request carries exact projection patches"
        );
        assert_eq!(
            pull_requests[0]
                .validations
                .iter()
                .map(|validation| validation.command.as_str())
                .collect::<Vec<_>>(),
            vec!["git diff --check"],
            "automatic StackPullRequests retain the checkpoint validation"
        );
        let intent_ref = pull_requests[0]
            .intent
            .clone()
            .expect("automatic StackPullRequests retain a signed intent");
        let intent = git.semantic_store().get_intent(&intent_ref).unwrap();
        assert!(intent.session.is_some());
        assert_eq!(intent.body, "add a feature");
        assert!(git.ledger_store().read_all().unwrap().iter().any(|event| {
            matches!(
                event.unsigned.event,
                CaptureEvent::Validation {
                    ref command,
                    passed: true,
                    ..
                } if command == "git diff --check"
            )
        }));
        handle_review(
            ReviewCommand::Project {
                plan: plan.id.0.to_string(),
            },
            &git,
            &root,
            &repository,
            &secrets,
        )
        .unwrap();
        let git_repository = git2::Repository::open(&repository).unwrap();
        let projected = git_repository
            .find_reference(&format!(
                "refs/heads/orkia/stack-pr/{}",
                pull_requests[0].id.0.simple()
            ))
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let entry = projected
            .tree()
            .unwrap()
            .get_path(Path::new("src/lib.rs"))
            .unwrap();
        let body = git_repository
            .find_blob(entry.id())
            .unwrap()
            .content()
            .to_vec();
        assert_eq!(body, b"pub fn feature() {}\n");
        let repository_id: RepositoryId = read_json(&root.join("orkia/repository.json")).unwrap();
        let (projected_object, projected) = git
            .semantic_store()
            .latest_projection_for_stack_pull_request_revision(
                &pull_requests[0].id,
                pull_requests[0].revision,
                &orkia_model::RepositoryPolicy::default(),
            )
            .unwrap()
            .unwrap();
        let mut published = projected;
        published.revision += 1;
        published.forge_pull_request = Some("https://forge.test/pr/1".into());
        published.status = orkia_model::ProjectionStatus::Published;
        published.supersedes = Some(projected_object);
        git.semantic_store()
            .store_projection(&published, &identity)
            .unwrap();
        let stack_id =
            orkia_model::StackId::from_stack_pull_requests([pull_requests[0].id.clone()]);
        let (_, stack) = git
            .semantic_store()
            .latest_stack(&stack_id, &orkia_model::RepositoryPolicy::default())
            .unwrap()
            .unwrap();
        let changeset_id =
            orkia_model::ChangeSetId::from_stack_references([orkia_model::ChangeSetStack {
                repository: repository_id.clone(),
                stack: stack.id,
                revision: stack.revision,
            }]);
        let (_, changeset) = git
            .semantic_store()
            .latest_changeset(&changeset_id, &orkia_model::RepositoryPolicy::default())
            .unwrap()
            .unwrap();
        handle_changeset_integration(
            &changeset.id.0.to_string(),
            &[format!("{}={}", repository_id.0, repository.display())],
            "feature",
            0,
            &git,
            &repository,
        )
        .unwrap();
        assert!(matches!(
            git.semantic_store()
                .latest_changeset(&changeset.id, &orkia_model::RepositoryPolicy::default())
                .unwrap()
                .unwrap()
                .1
                .status,
            orkia_model::StackPullRequestStatus::Integrated
        ));
        assert!(git.ledger_store().read_all().unwrap().iter().any(|event| {
            matches!(event.unsigned.event, CaptureEvent::ReviewPlanCreated { .. })
        }));
        std::fs::remove_file(plan_path).unwrap();
        let reconstructed =
            signed_review_plan(&root, &git, &repository, &plan.id.0.to_string()).unwrap();
        assert_eq!(
            reconstructed, plan,
            "signed refs replace the worktree cache"
        );
        std::fs::write(
            repository.join("orkia.toml"),
            r#"protected_branches = ["main"]
validation_commands = []
minimum_coverage_milli = 950
minimum_confidence_milli = 800
required_approvals = 2
required_checks = ["orkia/integrate"]
minimum_semantic_signatures = 1
authorized_grant_issuers = []
revoked_grants = []
"#,
        )
        .unwrap();
        assert!(
            signed_review_plan(&root, &git, &repository, &plan.id.0.to_string()).is_err(),
            "a changed policy must not reinterpret a previously signed plan"
        );
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

    #[test]
    fn absolute_agent_paths_match_git_relative_paths() {
        let repository = Path::new("/tmp/orkia-repository");
        let absolute = "/tmp/orkia-repository/src/lib.rs";

        assert_eq!(relative_repository_path(repository, absolute), "src/lib.rs");
        assert!(repository_path_matches(absolute, "src/lib.rs"));
        assert!(repository_path_matches("./src/lib.rs", "src/lib.rs"));
        assert!(!repository_path_matches(absolute, "src/main.rs"));
    }
}
