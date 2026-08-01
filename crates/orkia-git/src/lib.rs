//! The only crate permitted to access Git/libgit2.

use git2::{DiffOptions, ObjectType, Repository, Signature};
use orkia_model::{LedgerEvent, OrkiaError, Result};
use orkia_ports::{GitRepository, LedgerStore};
use std::path::{Path, PathBuf};

pub const LEDGER_REF: &str = "refs/orkia/ledger";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingFileChange {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
    pub changed_start: u32,
    pub changed_end: u32,
}

#[derive(Clone, Debug)]
pub struct LibGit2Repository {
    path: PathBuf,
}

impl LibGit2Repository {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        Repository::open(&path).map_err(git_error)?;
        Ok(Self { path })
    }
    fn repo(&self) -> Result<Repository> {
        Repository::open(&self.path).map_err(git_error)
    }
    pub fn ledger_store(&self) -> GitLedgerStore {
        GitLedgerStore {
            repository: self.clone(),
        }
    }
    pub fn project_branch(&self, branch: &str, target: &str) -> Result<()> {
        let repo = self.repo()?;
        let object = repo.revparse_single(target).map_err(git_error)?;
        repo.reference(
            &format!("refs/heads/{branch}"),
            object.id(),
            true,
            "Orkia review projection",
        )
        .map_err(git_error)?;
        Ok(())
    }
    pub fn changes_since(&self, base: &str) -> Result<Vec<WorkingFileChange>> {
        let repo = self.repo()?;
        let commit = repo
            .revparse_single(base)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut options = DiffOptions::new();
        options
            .include_untracked(true)
            .show_untracked_content(true)
            .recurse_untracked_dirs(true);
        let diff = repo
            .diff_tree_to_workdir_with_index(
                Some(&commit.tree().map_err(git_error)?),
                Some(&mut options),
            )
            .map_err(git_error)?;
        let mut changes = Vec::new();
        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .ok_or_else(|| OrkiaError::Integrity("diff delta has no path".into()))?
                .to_string_lossy()
                .into_owned();
            let old_content = if delta.old_file().id().is_zero() {
                String::new()
            } else {
                repo.find_blob(delta.old_file().id())
                    .ok()
                    .and_then(|blob| std::str::from_utf8(blob.content()).ok().map(str::to_owned))
                    .unwrap_or_default()
            };
            let new_content = std::fs::read_to_string(self.path.join(&path)).unwrap_or_default();
            let (changed_start, changed_end) = changed_lines(&old_content, &new_content);
            changes.push(WorkingFileChange {
                path,
                old_content,
                new_content,
                changed_start,
                changed_end,
            });
        }
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(changes)
    }
    pub fn project_paths(&self, branch: &str, base: &str, paths: &[String]) -> Result<String> {
        let repo = self.repo()?;
        let parent = repo
            .revparse_single(base)
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?;
        let mut index = repo.index().map_err(git_error)?;
        index
            .read_tree(&parent.tree().map_err(git_error)?)
            .map_err(git_error)?;
        for path in paths {
            if self.path.join(path).exists() {
                index.add_path(Path::new(path)).map_err(git_error)?;
            } else {
                index.remove_path(Path::new(path)).map_err(git_error)?;
            }
        }
        let tree_id = index.write_tree_to(&repo).map_err(git_error)?;
        let tree = repo.find_tree(tree_id).map_err(git_error)?;
        let signature = repo
            .signature()
            .or_else(|_| Signature::now("Orkia", "orkia@local"))
            .map_err(git_error)?;
        let message = format!("orkia: project review {branch}");
        let commit = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent],
            )
            .map_err(git_error)?;
        Ok(commit.to_string())
    }
}

fn changed_lines(old: &str, new: &str) -> (u32, u32) {
    let old_lines: Vec<_> = old.lines().collect();
    let new_lines: Vec<_> = new.lines().collect();
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let start = prefix as u32 + 1;
    let end = (new_lines.len().saturating_sub(suffix).max(prefix + 1)) as u32;
    (start, end)
}

fn git_error(error: git2::Error) -> OrkiaError {
    OrkiaError::External(format!("git: {error}"))
}

impl GitRepository for LibGit2Repository {
    fn head_commit(&self) -> Result<String> {
        Ok(self
            .repo()?
            .head()
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?
            .id()
            .to_string())
    }
    fn create_isolated_worktree(&self, name: &str, path: &Path) -> Result<()> {
        self.repo()?.worktree(name, path, None).map_err(git_error)?;
        Ok(())
    }
    fn write_ledger(&self, bytes: &[u8]) -> Result<()> {
        let repo = self.repo()?;
        let oid = repo.blob(bytes).map_err(git_error)?;
        repo.reference(LEDGER_REF, oid, true, "Orkia signed ledger")
            .map_err(git_error)?;
        Ok(())
    }
    fn read_ledger(&self) -> Result<Option<Vec<u8>>> {
        let repo = self.repo()?;
        let Ok(reference) = repo.find_reference(LEDGER_REF) else {
            return Ok(None);
        };
        let object = reference.peel(ObjectType::Blob).map_err(git_error)?;
        Ok(Some(
            object
                .as_blob()
                .ok_or_else(|| OrkiaError::Integrity("ledger ref is not a blob".into()))?
                .content()
                .to_vec(),
        ))
    }
}

#[derive(Clone, Debug)]
pub struct GitLedgerStore {
    repository: LibGit2Repository,
}
impl LedgerStore for GitLedgerStore {
    fn append(&self, event: &LedgerEvent) -> Result<()> {
        let mut events = self.read_all()?;
        events.push(event.clone());
        let bytes = serde_json::to_vec(&events).map_err(|e| OrkiaError::Invalid(e.to_string()))?;
        self.repository.write_ledger(&bytes)
    }
    fn read_all(&self) -> Result<Vec<LedgerEvent>> {
        match self.repository.read_ledger()? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| OrkiaError::Integrity(format!("invalid ledger blob: {e}"))),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    #[test]
    fn ledger_lives_in_a_dedicated_ref() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        let git = LibGit2Repository::open(dir.path()).unwrap();
        git.write_ledger(b"[]").unwrap();
        assert_eq!(git.read_ledger().unwrap(), Some(b"[]".to_vec()));
    }
}
