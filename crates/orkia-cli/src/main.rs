//! Terminal composition root for Orkia.

use base64::Engine;
use clap::{Parser, Subcommand, ValueEnum};
use orkia_agents::{
    Agent as SupportedAgent, all_statuses, install as install_agent, parse_hook_payload,
    status as agent_status, transcript_files, uninstall as uninstall_agent,
};
use orkia_capture::{ClaudeAdapter, CodexAdapter, ProviderAdapter};
use orkia_git::LibGit2Repository;
use orkia_github::GitHubApp;
use orkia_identity::Identity;
use orkia_ledger::{Ledger, SystemClock, verify_chain};
use orkia_model::{
    Actor, CaptureEvent, CaptureOrigin, OrkiaError, RepositoryId, Result, ReviewPlan, SessionId,
    ViewMetadata, ViewScope,
};
use orkia_ports::{
    Forge, GitRepository, LedgerStore, ReviewIndex, SecretStore, SemanticDocumentStore,
};
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
    /// Materialize Git-backed Trunk/Branch/Leaf semantic manifests.
    Semantic {
        #[command(subcommand)]
        command: SemanticCommand,
    },
    /// Store and retrieve encrypted secrets in Git-backed Orkia vault objects.
    Vault {
        #[command(subcommand)]
        command: VaultCommand,
    },
    /// Seal a verified Git commit as a reproducible OCI image layout.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Manage signed Git-backed organization authorities.
    Organization {
        #[command(subcommand)]
        command: OrganizationCommand,
    },
    /// Manage signed Git-backed team membership snapshots.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Issue immutable signed access grants to actors and/or teams.
    Access {
        #[command(subcommand)]
        command: AccessCommand,
    },
    Intent {
        #[command(subcommand)]
        command: IntentCommand,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Manage Git-backed semantic views and their isolated worktrees.
    View {
        #[command(subcommand)]
        command: ViewCommand,
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
    /// Rotate the local key after publishing a dual-signed continuity proof.
    Rotate,
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
    /// Sign a portable Git attestation for the current session.
    Attest {
        #[arg(long)]
        commit: Option<String>,
    },
    Close,
}
#[derive(Subcommand)]
enum LedgerCommand {
    Verify,
}
#[derive(Subcommand)]
enum SemanticCommand {
    /// Extract, sign and activate the semantic state of a commit.
    Record {
        #[arg(long)]
        commit: Option<String>,
    },
    /// Verify every fetched semantic state and its signed closure.
    Verify,
    /// Create a verified Git three-way merge and a signed semantic resolution.
    Merge {
        #[arg(long)]
        left: String,
        #[arg(long)]
        right: String,
        #[arg(long)]
        branch: String,
    },
    /// Record a manually resolved two-parent Git merge as a signed successor.
    Resolve {
        #[arg(long)]
        resolution: String,
        #[arg(long)]
        commit: String,
    },
    /// Import a Git history into verified semantic manifests.
    Import {
        #[arg(long)]
        commit: Option<String>,
    },
    /// Push all semantic refs through an ordinary configured Git remote.
    Export {
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Fetch semantic refs and fail if their signed states do not verify.
    Fetch {
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Fail-closed integrity check; optionally rebuild the non-authoritative Postgres index.
    Doctor {
        /// Rebuild the ledger projection using ORKIA_POSTGRES_URL after verification.
        #[arg(long)]
        rebuild_index: bool,
    },
    /// Query canonical semantic Trunks from a commit manifest.
    Query {
        #[arg(long)]
        commit: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// Compare two verified semantic states by paths, Trunks and operations.
    Diff {
        #[arg(long)]
        base: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// Show signed Trunk and operation provenance for one captured path.
    Blame {
        #[arg(long)]
        path: String,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Walk Git history and mark only commits with verified semantic capture.
    Log {
        #[arg(long)]
        commit: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}
#[derive(Subcommand)]
enum VaultCommand {
    /// Encrypt bytes from a file and publish the signed ciphertext under a name.
    Put {
        #[arg(long)]
        name: String,
        #[arg(long)]
        value_file: PathBuf,
        #[arg(long)]
        password_file: PathBuf,
    },
    /// Decrypt a named entry into a new private file without printing it.
    Get {
        #[arg(long)]
        name: String,
        #[arg(long)]
        password_file: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Subcommand)]
enum SandboxCommand {
    /// Create a new OCI image-layout directory from a verified commit tree.
    Seal {
        #[arg(long)]
        commit: Option<String>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify OCI descriptor digests and Orkia provenance annotations offline.
    Verify {
        #[arg(long)]
        input: PathBuf,
    },
}
#[derive(Subcommand)]
enum OrganizationCommand {
    Create {
        #[arg(long)]
        slug: String,
    },
}
#[derive(Subcommand)]
enum TeamCommand {
    Create {
        /// Hash of the signed Organization object.
        #[arg(long)]
        organization: String,
        #[arg(long)]
        name: String,
        /// UUID of a member. May be supplied more than once.
        #[arg(long = "member", required = true)]
        members: Vec<String>,
    },
}
#[derive(Subcommand)]
enum AccessCommand {
    Grant {
        #[arg(long, value_enum)]
        role: GrantRoleArg,
        /// Optional UUID of an individual subject.
        #[arg(long)]
        actor: Option<String>,
        /// Hash of a Team object. May be supplied more than once.
        #[arg(long = "team")]
        teams: Vec<String>,
        /// RepositoryId scope; omit for all repositories.
        #[arg(long = "scope")]
        repositories: Vec<String>,
        /// RFC 3339 expiration instant; omit for a non-expiring grant.
        #[arg(long)]
        expires_at: Option<String>,
    },
    Revoke {
        #[arg(long)]
        grant: String,
        #[arg(long)]
        reason: String,
    },
}
#[derive(Subcommand)]
enum IntentCommand {
    Put {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body_file: PathBuf,
    },
    Show {
        #[arg(long)]
        hash: String,
    },
}
#[derive(Subcommand)]
enum MemoryCommand {
    Put {
        #[arg(long)]
        topic: String,
        #[arg(long)]
        content_file: PathBuf,
    },
    Show {
        #[arg(long)]
        hash: String,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum GrantRoleArg {
    Administrator,
    SharedViewMaintainer,
    Reviewer,
}

impl From<GrantRoleArg> for orkia_model::GrantRole {
    fn from(value: GrantRoleArg) -> Self {
        match value {
            GrantRoleArg::Administrator => Self::Administrator,
            GrantRoleArg::SharedViewMaintainer => Self::SharedViewMaintainer,
            GrantRoleArg::Reviewer => Self::Reviewer,
        }
    }
}
#[derive(Subcommand)]
enum ViewCommand {
    /// Create a Git branch and bind immutable semantic view metadata to it.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// Name of the view whose current metadata revision becomes the parent.
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        shared: bool,
        /// Optionally check the new view out into this independent worktree.
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
    Show {
        #[arg(long)]
        name: String,
    },
    /// Show Git and semantic readiness for a view.
    Status {
        #[arg(long)]
        name: String,
    },
    /// Safely check a view branch out in the primary repository worktree.
    Switch {
        #[arg(long)]
        name: String,
    },
    /// Advance a view to a verified descendant semantic state.
    Update {
        #[arg(long)]
        name: String,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Publish a verified commit to a Shared view using a signed maintainer grant.
    Publish {
        #[arg(long)]
        name: String,
        #[arg(long)]
        commit: Option<String>,
        /// Hash of a signed AccessGrant. May be supplied more than once.
        #[arg(long = "grant", required = true)]
        grants: Vec<String>,
    },
    /// Remove a Draft view's refs while retaining its Git commits and blobs.
    Delete {
        #[arg(long)]
        name: String,
        /// Required to remove a Shared view.
        #[arg(long)]
        force_shared: bool,
    },
    /// Restrict a view to root operations of its verified active state.
    Filter {
        #[arg(long)]
        name: String,
        #[arg(long = "operation", required = true)]
        operations: Vec<String>,
    },
    /// Save current uncommitted work using Git's stash namespace.
    Stash {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "orkia view stash")]
        message: String,
        #[arg(long)]
        include_untracked: bool,
    },
    /// Create an annotated Git tag at a view's current tip.
    Tag {
        #[arg(long)]
        name: String,
        #[arg(long)]
        tag: String,
        #[arg(long, default_value = "Orkia view tag")]
        message: String,
    },
    /// Commit current work, materialize its semantic state and advance the view.
    Record {
        #[arg(long)]
        name: String,
        #[arg(long)]
        message: String,
    },
    /// Replace the current single-parent view tip from the working tree.
    Revise {
        #[arg(long)]
        name: String,
        #[arg(long)]
        message: String,
    },
    /// Restore tracked files and index to the checked-out view tip.
    Restore {
        #[arg(long)]
        name: String,
    },
    /// Safely rewind a clean checked-out view by one verified commit.
    Unrecord {
        #[arg(long)]
        name: String,
    },
    /// Check an existing view out in a new isolated Git worktree.
    Worktree {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: PathBuf,
    },
}
#[derive(Subcommand)]
enum ReviewCommand {
    /// Sign and migrate a legacy local review-plan JSON into Git refs.
    Import {
        #[arg(long)]
        path: PathBuf,
    },
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
        Command::Identity {
            command: IdentityCommand::Rotate,
        } => {
            let current = load_identity(&root, &secrets)?;
            let next = current.successor();
            let rotation = git
                .semantic_store()
                .put_key_rotation(&orkia_model::KeyRotation {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    actor: current.actor().id.clone(),
                    previous_public_key: current.actor().public_key.clone(),
                    next_public_key: next.actor().public_key.clone(),
                })?;
            git.semantic_store().sign_document(&rotation, &current)?;
            git.semantic_store().sign_document(&rotation, &next)?;
            git.semantic_store().verify_key_rotation(&rotation)?;
            next.save(&secrets, "identity")?;
            write_json(&root.join("orkia/actor.json"), next.actor())?;
            println!("identity key rotated as {}", rotation.hash);
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
        Command::Semantic { command } => match command {
            SemanticCommand::Record { commit } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
                let identity = Identity::load(&secrets, "identity", actor)?
                    .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let state = git.materialize_semantic_state(&commit, &identity, &policy)?;
                println!("semantic state {} activated for {commit}", state.hash);
            }
            SemanticCommand::Verify => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let report = git.verify_orkia_refs(&policy)?;
                println!("verified {} semantic state ref(s)", report.verified_states);
            }
            SemanticCommand::Merge {
                left,
                right,
                branch,
            } => {
                let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
                let identity = Identity::load(&secrets, "identity", actor)?
                    .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let result = git.semantic_merge(&left, &right, &branch, &identity, &policy)?;
                println!(
                    "semantic merge {:?}; resolution={}; result={}",
                    result.outcome,
                    result.resolution.hash,
                    result.result_commit.unwrap_or_else(|| "conflict".into())
                );
            }
            SemanticCommand::Resolve { resolution, commit } => {
                let identity = load_identity(&root, &secrets)?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let resolution = git.finalize_merge_resolution(
                    &orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Resolution,
                        hash: resolution,
                    },
                    &commit,
                    &identity,
                    &policy,
                )?;
                println!(
                    "semantic conflict finalized for {commit} as {}",
                    resolution.hash
                );
            }
            SemanticCommand::Import { commit } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let actor: Actor = read_json(&root.join("orkia/actor.json"))?;
                let identity = Identity::load(&secrets, "identity", actor)?
                    .ok_or_else(|| OrkiaError::NotFound("Orkia identity".into()))?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let states = git.import_semantic_history(&commit, &identity, &policy)?;
                println!("imported or verified {} semantic state(s)", states.len());
            }
            SemanticCommand::Export { remote } => {
                git.push_orkia_refs(&remote)?;
                println!("exported Orkia refs to {remote} through Git");
            }
            SemanticCommand::Fetch { remote } => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let report = git.fetch_verified_orkia_refs(&remote, &policy)?;
                println!(
                    "fetched and verified {} semantic state ref(s) from {remote}",
                    report.verified_states
                );
            }
            SemanticCommand::Doctor { rebuild_index } => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let report = git.verify_orkia_refs(&policy)?;
                if rebuild_index {
                    let url = std::env::var("ORKIA_POSTGRES_URL").map_err(|_| {
                        OrkiaError::NotFound(
                            "ORKIA_POSTGRES_URL required for --rebuild-index".into(),
                        )
                    })?;
                    let index = orkia_index_postgres::PostgresIndex::connect(&url)?;
                    let events = git.ledger_store().read_all()?;
                    index.rebuild(&events)?;
                    println!(
                        "reconstructed Postgres ledger index from {} Git event(s)",
                        events.len()
                    );
                }
                println!(
                    "doctor passed: {} semantic state ref(s) are rebuildable from Git",
                    report.verified_states
                );
            }
            SemanticCommand::Query {
                commit,
                path_prefix,
            } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                for trunk in git.query_trunks(&commit, path_prefix.as_deref(), &policy)? {
                    println!(
                        "{}\t{:?}\t{}",
                        trunk.id.0,
                        trunk.state,
                        trunk.paths.iter().cloned().collect::<Vec<_>>().join(",")
                    );
                }
            }
            SemanticCommand::Diff { base, target } => {
                let target = target.unwrap_or(git.head_commit()?);
                let policy = repository_policy(&cli.repository)?;
                let diff = git.semantic_diff(&base, &target, &policy)?;
                println!("base={}\ttarget={}", diff.base_commit, diff.target_commit);
                for path in diff.changed_paths {
                    println!("path\t{path}");
                }
                for trunk in diff.added_trunks {
                    println!("trunk-added\t{}", trunk.0);
                }
                for trunk in diff.removed_trunks {
                    println!("trunk-removed\t{}", trunk.0);
                }
                for trunk in diff.changed_trunks {
                    println!("trunk-changed\t{}", trunk.0);
                }
                for operation in diff.added_operations {
                    println!("operation-added\t{}", operation.hash);
                }
                for operation in diff.removed_operations {
                    println!("operation-removed\t{}", operation.hash);
                }
            }
            SemanticCommand::Blame { path, commit } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy = repository_policy(&cli.repository)?;
                let blame = git.semantic_blame(&commit, &path, &policy)?;
                println!("commit={}\tpath={}", blame.commit, blame.path);
                for trunk in blame.trunks {
                    println!("trunk\t{}\t{:?}", trunk.id.0, trunk.state);
                }
                for operation in blame.operations {
                    println!("operation\t{}", operation.hash);
                }
            }
            SemanticCommand::Log { commit, limit } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy = repository_policy(&cli.repository)?;
                for entry in git.semantic_log(&commit, limit, &policy)? {
                    println!(
                        "{}\t{}",
                        entry.commit,
                        entry
                            .state
                            .map(|state| format!("verified:{}", state.hash))
                            .unwrap_or_else(|| "git-fallback".into())
                    );
                }
            }
        },
        Command::Vault { command } => match command {
            VaultCommand::Put {
                name,
                value_file,
                password_file,
            } => {
                let value = fs::read(&value_file).map_err(|error| {
                    OrkiaError::External(format!("cannot read vault value: {error}"))
                })?;
                let password = fs::read(&password_file).map_err(|error| {
                    OrkiaError::External(format!("cannot read vault password: {error}"))
                })?;
                let identity = load_identity(&root, &secrets)?;
                let entry = git
                    .semantic_store()
                    .store_vault_secret(&name, &value, &password, &identity)?;
                println!("encrypted vault entry {name} stored as {}", entry.hash);
            }
            VaultCommand::Get {
                name,
                password_file,
                output,
            } => {
                if output.exists() {
                    return Err(OrkiaError::Conflict(format!(
                        "refusing to overwrite existing vault output {}",
                        output.display()
                    )));
                }
                let password = fs::read(&password_file).map_err(|error| {
                    OrkiaError::External(format!("cannot read vault password: {error}"))
                })?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let value = git
                    .semantic_store()
                    .read_vault_secret(&name, &password, &policy)?;
                write_private_file(&output, &value)?;
                println!("vault entry {name} decrypted to {}", output.display());
            }
        },
        Command::Sandbox { command } => match command {
            SandboxCommand::Seal { commit, output } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy = repository_policy(&cli.repository)?;
                let seal = git.seal_sandbox(&commit, &output, &policy)?;
                println!(
                    "sealed {} as OCI layout {} (manifest sha256:{})",
                    seal.commit,
                    output.display(),
                    seal.manifest_digest
                );
            }
            SandboxCommand::Verify { input } => {
                let seal = LibGit2Repository::verify_sandbox(&input)?;
                println!(
                    "verified OCI layout: commit={} state={} manifest=sha256:{}",
                    seal.commit, seal.state.hash, seal.manifest_digest
                );
            }
        },
        Command::Organization { command } => match command {
            OrganizationCommand::Create { slug } => {
                let identity = load_identity(&root, &secrets)?;
                let store = git.semantic_store();
                let organization = store.put_organization(&orkia_model::Organization {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    slug: slug.clone(),
                    issuer: identity.actor().id.clone(),
                })?;
                store.sign_document(&organization, &identity)?;
                println!("organization {slug} created as {}", organization.hash);
            }
        },
        Command::Team { command } => match command {
            TeamCommand::Create {
                organization,
                name,
                members,
            } => {
                let identity = load_identity(&root, &secrets)?;
                let store = git.semantic_store();
                let organization = orkia_model::SemanticObjectRef {
                    kind: orkia_model::SemanticObjectKind::Organization,
                    hash: organization,
                };
                let policy = repository_policy(&cli.repository)?;
                let authority = store.verify_organization(&organization, &policy)?;
                if authority.issuer != identity.actor().id {
                    return Err(OrkiaError::Policy(
                        "only the organization issuer may publish a team snapshot".into(),
                    ));
                }
                let members = members
                    .into_iter()
                    .map(|member| {
                        member
                            .parse::<uuid::Uuid>()
                            .map(orkia_model::ActorId)
                            .map_err(|error| {
                                OrkiaError::Invalid(format!("invalid team member UUID: {error}"))
                            })
                    })
                    .collect::<Result<BTreeSet<_>>>()?;
                let team = store.put_team(&orkia_model::Team {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    organization,
                    name: name.clone(),
                    issuer: identity.actor().id.clone(),
                    members,
                })?;
                store.sign_document(&team, &identity)?;
                println!("team {name} created as {}", team.hash);
            }
        },
        Command::Access { command } => match command {
            AccessCommand::Grant {
                role,
                actor,
                teams,
                repositories,
                expires_at,
            } => {
                let identity = load_identity(&root, &secrets)?;
                let actor = actor
                    .map(|actor| {
                        actor
                            .parse::<uuid::Uuid>()
                            .map(orkia_model::ActorId)
                            .map_err(|error| {
                                OrkiaError::Invalid(format!("invalid grant actor UUID: {error}"))
                            })
                    })
                    .transpose()?;
                let teams = teams
                    .into_iter()
                    .map(|hash| orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Team,
                        hash,
                    })
                    .collect::<BTreeSet<_>>();
                let store = git.semantic_store();
                let policy = repository_policy(&cli.repository)?;
                for team in &teams {
                    store.verify_team(team, &policy)?;
                }
                let grant = store.put_access_grant(&orkia_model::AccessGrant {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    issuer: identity.actor().id.clone(),
                    actor,
                    role: role.into(),
                    repositories: repositories.into_iter().collect(),
                    teams,
                    expires_at,
                })?;
                store.sign_document(&grant, &identity)?;
                println!("signed access grant created as {}", grant.hash);
            }
            AccessCommand::Revoke { grant, reason } => {
                let identity = load_identity(&root, &secrets)?;
                let grant = orkia_model::SemanticObjectRef {
                    kind: orkia_model::SemanticObjectKind::Grant,
                    hash: grant,
                };
                let document = git.semantic_store().get_access_grant(&grant)?;
                if document.issuer != identity.actor().id {
                    return Err(OrkiaError::Policy(
                        "only the grant issuer may revoke it".into(),
                    ));
                }
                let revocation =
                    git.semantic_store()
                        .put_grant_revocation(&orkia_model::GrantRevocation {
                            schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                            grant,
                            issuer: identity.actor().id.clone(),
                            reason,
                        })?;
                git.semantic_store().sign_document(&revocation, &identity)?;
                println!("signed grant revocation {}", revocation.hash);
            }
        },
        Command::Intent { command } => match command {
            IntentCommand::Put { title, body_file } => {
                let identity = load_identity(&root, &secrets)?;
                let body = fs::read_to_string(body_file)
                    .map_err(|e| OrkiaError::External(e.to_string()))?;
                let object = git.semantic_store().put_intent(&orkia_model::Intent {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    title,
                    body,
                    session: None,
                    evidence: BTreeSet::new(),
                })?;
                git.semantic_store().sign_document(&object, &identity)?;
                println!("signed intent {}", object.hash);
            }
            IntentCommand::Show { hash } => {
                let object = git
                    .semantic_store()
                    .get_intent(&orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Intent,
                        hash,
                    })?;
                println!("{}\n\n{}", object.title, object.body);
            }
        },
        Command::Memory { command } => match command {
            MemoryCommand::Put {
                topic,
                content_file,
            } => {
                let identity = load_identity(&root, &secrets)?;
                let content = fs::read_to_string(content_file)
                    .map_err(|e| OrkiaError::External(e.to_string()))?;
                let object = git.semantic_store().put_memory(&orkia_model::Memory {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    topic,
                    content,
                    evidence: BTreeSet::new(),
                })?;
                git.semantic_store().sign_document(&object, &identity)?;
                println!("signed memory {}", object.hash);
            }
            MemoryCommand::Show { hash } => {
                let object = git
                    .semantic_store()
                    .get_memory(&orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Memory,
                        hash,
                    })?;
                println!("{}\n\n{}", object.topic, object.content);
            }
        },
        Command::View { command } => match command {
            ViewCommand::Create {
                name,
                branch,
                base,
                parent,
                shared,
                worktree,
            } => {
                let base_commit = base.unwrap_or(git.head_commit()?);
                let branch = branch.unwrap_or_else(|| format!("orkia/views/{name}"));
                let view = ViewMetadata {
                    schema_version: orkia_model::SEMANTIC_SCHEMA_VERSION,
                    name: name.clone(),
                    branch: branch.clone(),
                    base_commit,
                    scope: if shared {
                        ViewScope::Shared
                    } else {
                        ViewScope::Draft
                    },
                    parent: None,
                    visible_operations: BTreeSet::new(),
                };
                let object = if let Some(parent) = parent {
                    git.create_child_view(&view, &parent)?
                } else {
                    git.create_view(&view)?
                };
                if let Some(path) = worktree {
                    git.create_view_worktree(&name, &path)?;
                    println!(
                        "view {name} ({branch}) created as {} with worktree {}",
                        object.hash,
                        path.display()
                    );
                } else {
                    println!("view {name} ({branch}) created as {}", object.hash);
                }
            }
            ViewCommand::Show { name } => {
                let view = git.view(&name)?;
                println!(
                    "{}\tbranch={}\tbase={}\tscope={:?}",
                    view.name, view.branch, view.base_commit, view.scope
                );
            }
            ViewCommand::Status { name } => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let status = git.view_status(&name, &policy)?;
                println!(
                    "{}\tbranch={}\ttip={}\tmetadata_matches_branch={}\tgit_changes={}\tsemantic_verified={}\tunpublished_operations={}\tsemantic_error={}",
                    status.name,
                    status.branch,
                    status.branch_tip,
                    status.metadata_matches_branch,
                    status.working_tree_changes,
                    status.semantic_verified,
                    status.unpublished_operations,
                    status.semantic_error.unwrap_or_else(|| "-".into()),
                );
            }
            ViewCommand::Switch { name } => {
                git.switch_view(&name)?;
                println!("switched primary worktree to view {name}");
            }
            ViewCommand::Update { name, commit } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let metadata = git.advance_view(&name, &commit, &policy)?;
                println!("view {name} advanced to {commit} as {}", metadata.hash);
            }
            ViewCommand::Publish {
                name,
                commit,
                grants,
            } => {
                let commit = commit.unwrap_or(git.head_commit()?);
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let identity = load_identity(&root, &secrets)?;
                let grants = grants
                    .into_iter()
                    .map(|hash| orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Grant,
                        hash,
                    })
                    .collect();
                let repository: RepositoryId = read_json(&root.join("orkia/repository.json"))?;
                let metadata = git.publish_shared_view_for_repository(
                    &name,
                    &commit,
                    &identity.actor().id,
                    grants,
                    &repository.0.to_string(),
                    &policy,
                )?;
                println!(
                    "Shared view {name} published at {commit} as {}",
                    metadata.hash
                );
            }
            ViewCommand::Delete { name, force_shared } => {
                git.delete_view(&name, force_shared)?;
                println!(
                    "view {name} deleted; its Git commits and semantic blobs remain recoverable"
                );
            }
            ViewCommand::Filter { name, operations } => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let operations = operations
                    .into_iter()
                    .map(|hash| orkia_model::SemanticObjectRef {
                        kind: orkia_model::SemanticObjectKind::Operation,
                        hash,
                    })
                    .collect();
                let metadata = git.set_view_visible_operations(&name, operations, &policy)?;
                println!("view {name} filter updated as {}", metadata.hash);
            }
            ViewCommand::Stash {
                name,
                message,
                include_untracked,
            } => {
                let stash = git.stash_view(&name, &message, include_untracked)?;
                println!("view {name} stashed as {stash}");
            }
            ViewCommand::Tag { name, tag, message } => {
                let object = git.tag_view(&name, &tag, &message)?;
                println!("view {name} tagged {tag} as {object}");
            }
            ViewCommand::Record { name, message } => {
                let identity = load_identity(&root, &secrets)?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let state = git.record_view(&name, &message, &identity, &policy)?;
                println!("view {name} recorded and advanced as {}", state.hash);
            }
            ViewCommand::Revise { name, message } => {
                let identity = load_identity(&root, &secrets)?;
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let state = git.revise_view(&name, &message, &identity, &policy)?;
                println!("view {name} revised and advanced as {}", state.hash);
            }
            ViewCommand::Restore { name } => {
                git.restore_view(&name)?;
                println!("view {name} restored to its Git tip");
            }
            ViewCommand::Unrecord { name } => {
                let policy_path = cli.repository.join("orkia.toml");
                let policy = if policy_path.exists() {
                    orkia_policy::load(&policy_path)?
                } else {
                    orkia_model::RepositoryPolicy::default()
                };
                let state = git.unrecord_view(&name, &policy)?;
                println!("view {name} unrecorded to semantic state {}", state.hash);
            }
            ViewCommand::Worktree { name, path } => {
                git.create_view_worktree(&name, &path)?;
                println!("view {name} checked out at {}", path.display());
            }
        },
        Command::Review { command } => {
            handle_review(command, &git, &root, &cli.repository, &secrets)?
        }
        Command::Integrate {
            plan,
            branch,
            approvals,
        } => {
            let policy_path = cli.repository.join("orkia.toml");
            let policy = if policy_path.exists() {
                orkia_policy::load(&policy_path)?
            } else {
                orkia_model::RepositoryPolicy::default()
            };
            let review = load_review_plan(&git, &plan, &policy)?;
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
        AgentCommand::Import { agent } => {
            let agent = supported_agent(&agent)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            let files = transcript_files(agent).map_err(OrkiaError::External)?;
            for file in &files {
                let bytes = fs::read(&file.path).map_err(|error| {
                    OrkiaError::External(format!("{}: {error}", file.path.display()))
                })?;
                let (encoding, content) = if file.binary {
                    (
                        "base64".into(),
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    )
                } else {
                    ("utf-8".into(), String::from_utf8_lossy(&bytes).into_owned())
                };
                ledger.append(CaptureEvent::AgentTranscript {
                    agent: agent.name().into(),
                    path: file.path.to_string_lossy().into_owned(),
                    encoding,
                    content,
                })?;
            }
            println!(
                "imported {} {} transcript document(s)",
                files.len(),
                agent.name()
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
        AgentCommand::Hook { agent } => {
            let agent = supported_agent(&agent)?;
            let mut raw = String::new();
            use std::io::Read;
            std::io::stdin()
                .read_to_string(&mut raw)
                .map_err(|error| OrkiaError::External(error.to_string()))?;
            let payload = parse_hook_payload(agent, &raw).map_err(OrkiaError::Invalid)?;
            let ledger = open_repository_ledger(git, root, secrets)?;
            if payload.event == "SessionStart" {
                ledger.append(CaptureEvent::SessionStarted {
                    session: SessionId::new(),
                    origin: match agent {
                        SupportedAgent::Codex => CaptureOrigin::Codex,
                        SupportedAgent::ClaudeCode => CaptureOrigin::Claude,
                        _ => CaptureOrigin::Unknown,
                    },
                    base_commit: git.head_commit()?,
                    objective: format!(
                        "{} agent session {}",
                        agent.name(),
                        payload.session_id.as_deref().unwrap_or("unknown")
                    ),
                })?;
            }
            if payload.event == "UserPromptSubmit" {
                if let Some(content) = payload.prompt.clone() {
                    ledger.append(CaptureEvent::Prompt {
                        provider: agent.name().into(),
                        content,
                    })?;
                }
            }
            ledger.append(CaptureEvent::AgentHook {
                agent: agent.name().into(),
                external_session: payload.session_id,
                hook_event: payload.event,
                cwd: payload.cwd.map(|path| path.to_string_lossy().into_owned()),
                payload: payload.raw,
            })?;
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
                ledger.append(bind_turn_to_session(event, &state))?;
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
                ledger.append(bind_turn_to_session(event, &state))?;
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
        SessionCommand::Attest { commit } => {
            let state: SessionState = read_json(&root.join("orkia/session.json"))?;
            let identity = load_identity(root, secrets)?;
            let result = commit.unwrap_or(git.head_commit()?);
            let attestation = git.semantic_store().attest_session(
                state.id,
                state.base_commit,
                Some(result),
                BTreeSet::new(),
                &identity,
            )?;
            println!("session attested as {}", attestation.hash);
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

fn bind_turn_to_session(event: CaptureEvent, state: &SessionState) -> CaptureEvent {
    match event {
        CaptureEvent::AgentTurn {
            provider,
            turn_id,
            model,
            input_tokens,
            output_tokens,
            cost_micros,
            ..
        } => CaptureEvent::AgentTurn {
            provider,
            session: Some(state.id.clone()),
            base_commit: Some(state.base_commit.clone()),
            turn_id,
            model,
            input_tokens,
            output_tokens,
            cost_micros,
        },
        CaptureEvent::AgentAction {
            provider,
            turn_id,
            action_id,
            kind,
            paths,
            command,
            exit_code,
            ..
        } => CaptureEvent::AgentAction {
            provider,
            session: Some(state.id.clone()),
            base_commit: Some(state.base_commit.clone()),
            turn_id,
            action_id,
            kind,
            paths,
            command,
            exit_code,
        },
        event => event,
    }
}

fn handle_review(
    command: ReviewCommand,
    git: &LibGit2Repository,
    root: &Path,
    repository: &Path,
    secrets: &FileSecrets,
) -> Result<()> {
    match command {
        ReviewCommand::Import { path } => {
            let plan: ReviewPlan = read_json(&path)?;
            let identity = load_identity(root, secrets)?;
            let object = git.semantic_store().store_review_plan(&plan, &identity)?;
            println!(
                "legacy review plan {} revision {} imported as {}",
                plan.id.0, plan.revision, object.hash
            );
        }
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
            let identity = load_identity(root, secrets)?;
            git.semantic_store().store_review_plan(&plan, &identity)?;
            println!(
                "review plan {}: {} unit(s), coverage {}‰",
                plan.id.0,
                plan.units.len(),
                plan.coverage_milli
            );
        }
        ReviewCommand::Show { plan } => {
            let value = load_review_plan(git, &plan, &repository_policy(repository)?)?;
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
            let current = load_review_plan(git, &plan_id, &repository_policy(repository)?)?;
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
            let identity = load_identity(root, secrets)?;
            git.semantic_store()
                .store_review_plan(&revised, &identity)?;
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
            let plan = load_review_plan(git, &plan_id, &repository_policy(repository)?)?;
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
            let plan = load_review_plan(git, &plan_id, &repository_policy(repository)?)?;
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

fn repository_policy(repository: &Path) -> Result<orkia_model::RepositoryPolicy> {
    let path = repository.join("orkia.toml");
    if path.exists() {
        orkia_policy::load(&path)
    } else {
        Ok(orkia_model::RepositoryPolicy::default())
    }
}

fn load_review_plan(
    git: &LibGit2Repository,
    value: &str,
    policy: &orkia_model::RepositoryPolicy,
) -> Result<ReviewPlan> {
    let id = value
        .parse::<uuid::Uuid>()
        .map(orkia_model::PlanId)
        .map_err(|error| OrkiaError::Invalid(format!("invalid review plan ID: {error}")))?;
    git.semantic_store().latest_review_plan(&id, policy)
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
fn observed_paths(events: &[orkia_model::LedgerEvent]) -> BTreeSet<String> {
    events
        .iter()
        .flat_map(|event| match &event.unsigned.event {
            CaptureEvent::FilesObserved { modified, .. } => modified.clone(),
            _ => BTreeSet::new(),
        })
        .collect()
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
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|error| OrkiaError::External(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| OrkiaError::External(error.to_string()))?;
    }
    Ok(())
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
