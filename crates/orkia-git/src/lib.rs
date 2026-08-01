//! The only crate permitted to access Git/libgit2.

use git2::{ObjectType, Repository};
use orkia_model::{LedgerEvent, OrkiaError, Result};
use orkia_ports::{GitRepository, LedgerStore};
use std::path::{Path, PathBuf};

pub const LEDGER_REF: &str = "refs/orkia/ledger";

#[derive(Clone, Debug)]
pub struct LibGit2Repository { path: PathBuf }

impl LibGit2Repository {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into(); Repository::open(&path).map_err(git_error)?; Ok(Self { path })
    }
    fn repo(&self) -> Result<Repository> { Repository::open(&self.path).map_err(git_error) }
    pub fn ledger_store(&self) -> GitLedgerStore { GitLedgerStore { repository: self.clone() } }
    pub fn project_branch(&self, branch: &str, target: &str) -> Result<()> {
        let repo = self.repo()?; let object = repo.revparse_single(target).map_err(git_error)?;
        repo.reference(&format!("refs/heads/{branch}"), object.id(), true, "Orkia review projection").map_err(git_error)?; Ok(())
    }
}

fn git_error(error: git2::Error) -> OrkiaError { OrkiaError::External(format!("git: {error}")) }

impl GitRepository for LibGit2Repository {
    fn head_commit(&self) -> Result<String> { Ok(self.repo()?.head().map_err(git_error)?.peel_to_commit().map_err(git_error)?.id().to_string()) }
    fn create_isolated_worktree(&self, name: &str, path: &Path) -> Result<()> { self.repo()?.worktree(name, path, None).map_err(git_error)?; Ok(()) }
    fn write_ledger(&self, bytes: &[u8]) -> Result<()> {
        let repo = self.repo()?; let oid = repo.blob(bytes).map_err(git_error)?;
        repo.reference(LEDGER_REF, oid, true, "Orkia signed ledger").map_err(git_error)?; Ok(())
    }
    fn read_ledger(&self) -> Result<Option<Vec<u8>>> {
        let repo = self.repo()?; let Ok(reference) = repo.find_reference(LEDGER_REF) else { return Ok(None) };
        let object = reference.peel(ObjectType::Blob).map_err(git_error)?;
        Ok(Some(object.as_blob().ok_or_else(|| OrkiaError::Integrity("ledger ref is not a blob".into()))?.content().to_vec()))
    }
}

#[derive(Clone, Debug)]
pub struct GitLedgerStore { repository: LibGit2Repository }
impl LedgerStore for GitLedgerStore {
    fn append(&self, event: &LedgerEvent) -> Result<()> { let mut events = self.read_all()?; events.push(event.clone()); let bytes = serde_json::to_vec(&events).map_err(|e| OrkiaError::Invalid(e.to_string()))?; self.repository.write_ledger(&bytes) }
    fn read_all(&self) -> Result<Vec<LedgerEvent>> { match self.repository.read_ledger()? { Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| OrkiaError::Integrity(format!("invalid ledger blob: {e}"))), None => Ok(Vec::new()) } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    #[test] fn ledger_lives_in_a_dedicated_ref() {
        let dir = tempfile::tempdir().unwrap(); let repo = Repository::init(dir.path()).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap(); let tree = repo.treebuilder(None).unwrap().write().unwrap(); let tree = repo.find_tree(tree).unwrap(); repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).unwrap(); drop(tree); drop(repo);
        let git = LibGit2Repository::open(dir.path()).unwrap(); git.write_ledger(b"[]").unwrap(); assert_eq!(git.read_ledger().unwrap(), Some(b"[]".to_vec()));
    }
}
