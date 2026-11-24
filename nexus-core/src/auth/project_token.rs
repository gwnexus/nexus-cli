//! Project inference token storage (`nxs_proj_*`).
//!
//! Project tokens authenticate CLIENT inference traffic against the Nexus
//! Model Gateway. They are distinct from the user PAT
//! (`nxs_pat_*`), which remains the CLI/MCP control-plane credential.
//!
//! Tokens are stored per project in `~/.config/nexus/project-tokens.toml`
//! with restrictive `0600` permissions, mirroring the PAT credential store.
//! They are NEVER written to a committed file. The value is exposed to
//! OpenCode via the gitignored workspace file `.env.nexus.local` as
//! `NEXUS_PROJECT_TOKEN`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::Error;

/// Expected prefix for Nexus project inference tokens.
pub const PROJECT_TOKEN_PREFIX: &str = "nxs_proj_";

/// Environment variable OpenCode / the gateway client reads the token from.
pub const PROJECT_TOKEN_ENV: &str = "NEXUS_PROJECT_TOKEN";

/// A single stored project inference token record.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProjectTokenEntry {
    /// The raw `nxs_proj_*` token value.
    pub token: String,

    /// Stable identifier for the token record (used for rotate/revoke).
    pub token_id: String,

    /// Non-secret display prefix (safe to print).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_prefix: Option<String>,

    /// Logical runtime name the token was issued for.
    pub runtime_id: String,

    /// Optional ISO 8601 expiry timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// ISO 8601 timestamp of when this entry was stored locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// During a rotation overlap window, the `token_id` of the superseded
    /// token that is still valid until it is finalized (revoked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_token_id: Option<String>,
}

impl std::fmt::Debug for ProjectTokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectTokenEntry")
            .field("token", &"[REDACTED]")
            .field("token_id", &self.token_id)
            .field("token_prefix", &self.token_prefix)
            .field("runtime_id", &self.runtime_id)
            .field("expires_at", &self.expires_at)
            .field("created_at", &self.created_at)
            .field("previous_token_id", &self.previous_token_id)
            .finish()
    }
}

/// The full `project-tokens.toml` file: a map of project UUID to token entry.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProjectTokenStore {
    #[serde(default)]
    pub projects: HashMap<String, ProjectTokenEntry>,
    /// Token ids per project whose revocation the hub did not confirm
    /// (`nexus project unlink` retries them). Ids only, never token values.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pending_revocations: HashMap<String, Vec<String>>,
}

impl std::fmt::Debug for ProjectTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectTokenStore")
            .field("projects", &self.projects.keys().collect::<Vec<_>>())
            .field("pending_revocations", &self.pending_revocations)
            .finish()
    }
}

impl ProjectTokenStore {
    /// Returns the store path: `~/.config/nexus/project-tokens.toml`.
    pub fn path() -> Result<PathBuf, Error> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Auth("unable to determine home directory".to_string()))?;
        Ok(home
            .join(".config")
            .join("nexus")
            .join("project-tokens.toml"))
    }

    /// Load the store from disk. Returns an empty store if the file is absent.
    pub fn load() -> Result<Self, Error> {
        Self::load_from(&Self::path()?)
    }

    /// Load the store at `path`; only a missing file is an empty store.
    fn load_from(path: &std::path::Path) -> Result<Self, Error> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let store: ProjectTokenStore = toml::from_str(&content)?;
        Ok(store)
    }

    /// Change the store in one locked step: read the current file, apply
    /// `change` and save, all under an exclusive lock, so a concurrent
    /// `nexus project` command cannot overwrite another project's entry
    /// with an older snapshot. The lock is waited for at most a few
    /// seconds, so an async caller is never blocked indefinitely; on a
    /// multi-thread runtime the wait runs in `block_in_place`, so the
    /// worker's other tasks move on meanwhile.
    pub fn update<R>(change: impl FnOnce(&mut Self) -> R) -> Result<R, Error> {
        let path = Self::path()?;
        off_worker(|| Self::update_at(&path, change))
    }

    fn update_at<R>(
        path: &std::path::Path,
        change: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, Error> {
        let _lock = StoreLock::acquire(path)?;
        let mut store = Self::load_from(path)?;
        let out = change(&mut store);
        let content = toml::to_string_pretty(&store)?;
        crate::fsutil::write_private_atomic(path, content.as_bytes())?;
        Ok(out)
    }

    /// Save the store to disk with restrictive file permissions (0600 on Unix).
    pub fn save(&self) -> Result<(), Error> {
        let path = Self::path()?;
        let content = toml::to_string_pretty(self)?;
        // Atomic and owner-only: a failed save keeps the previous store, and
        // an existing file with wider permissions is replaced, not reused.
        crate::fsutil::write_private_atomic(&path, content.as_bytes())?;
        Ok(())
    }

    /// Get the stored token entry for a project, if any.
    pub fn get(&self, project_id: &str) -> Option<&ProjectTokenEntry> {
        self.projects.get(project_id)
    }

    /// Insert or replace the token entry for a project.
    pub fn set(&mut self, project_id: &str, entry: ProjectTokenEntry) {
        self.projects.insert(project_id.to_string(), entry);
    }

    /// Remove the token entry for a project. Returns the removed entry.
    pub fn remove(&mut self, project_id: &str) -> Option<ProjectTokenEntry> {
        self.projects.remove(project_id)
    }
}

/// An exclusive lock on `<store>.lock`, released on drop (Unix; elsewhere
/// a no-op).
struct StoreLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

/// Run blocking `work` without holding up an async worker: on a
/// multi-thread runtime through `block_in_place` (the worker's other tasks
/// move on), otherwise directly (`block_in_place` is not available there).
fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

impl StoreLock {
    fn acquire(store: &std::path::Path) -> Result<Self, Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::io::AsRawFd;
            let mut name = store.as_os_str().to_owned();
            name.push(".lock");
            if let Some(dir) = store.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(std::path::PathBuf::from(name))?;
            // Nonblocking with a bounded wait (about 3 s): callers run on
            // async threads, which must not block on another process.
            for _ in 0..60 {
                // SAFETY: the descriptor belongs to `file`, which the lock owns.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Self { _file: file });
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(err.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(Error::Other(
                "the project token store is locked by another nexus project command; try again"
                    .to_string(),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = store;
            Ok(Self {})
        }
    }
}

/// Validate that a token string has the correct project-token prefix.
pub fn validate_project_token_format(token: &str) -> Result<(), Error> {
    if !token.starts_with(PROJECT_TOKEN_PREFIX) {
        return Err(Error::Auth(format!(
            "invalid project token format: must start with '{}'",
            PROJECT_TOKEN_PREFIX
        )));
    }
    if token.len() < PROJECT_TOKEN_PREFIX.len() + 10 {
        return Err(Error::Auth(
            "project token is too short to be valid".to_string(),
        ));
    }
    Ok(())
}

/// Resolve a project inference token for the given project.
///
/// Checks in order:
/// 1. `NEXUS_PROJECT_TOKEN` environment variable (CI / explicit override)
/// 2. The stored token in `~/.config/nexus/project-tokens.toml`
///
/// Returns `None` if no token is available.
pub fn resolve_project_token(project_id: &str) -> Option<String> {
    try_resolve_project_token(project_id).ok().flatten()
}

/// [`resolve_project_token`], with a store that cannot be read reported
/// as an error instead of "no token" (which would ask for a new link).
pub fn try_resolve_project_token(project_id: &str) -> Result<Option<String>, Error> {
    if let Ok(token) = std::env::var(PROJECT_TOKEN_ENV) {
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    Ok(ProjectTokenStore::load()?
        .get(project_id)
        .map(|e| e.token.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ProjectTokenEntry {
        ProjectTokenEntry {
            token: format!("{PROJECT_TOKEN_PREFIX}synthetic-{id}"),
            token_id: id.to_string(),
            token_prefix: None,
            runtime_id: "rt".into(),
            expires_at: None,
            created_at: None,
            previous_token_id: None,
        }
    }

    /// The locked update runs from any context: no runtime, a
    /// current-thread runtime and a multi-thread runtime.
    #[test]
    fn test_off_worker_runs_in_every_runtime_flavor() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("project-tokens.toml");
        let update = |id: &'static str| {
            let path = path.clone();
            move || off_worker(|| ProjectTokenStore::update_at(&path, |s| s.set(id, entry(id))))
        };
        update("a")().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { update("b")() })
            .unwrap();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap()
            .block_on(async {
                let work = update("c");
                tokio::spawn(async move { work() }).await.unwrap()
            })
            .unwrap();
        let store = ProjectTokenStore::load_from(&path).unwrap();
        for id in ["a", "b", "c"] {
            assert!(store.get(id).is_some(), "{id} missing");
        }
    }

    #[test]
    fn test_update_rereads_the_store_and_keeps_other_projects() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("project-tokens.toml");
        ProjectTokenStore::update_at(&path, |s| s.set("a", entry("a1"))).unwrap();
        // A stale snapshot taken before another command linked project b.
        let stale = ProjectTokenStore::load_from(&path).unwrap();
        ProjectTokenStore::update_at(&path, |s| s.set("b", entry("b1"))).unwrap();
        // Changing a through update keeps b; saving the stale snapshot
        // would have dropped it.
        drop(stale);
        ProjectTokenStore::update_at(&path, |s| {
            s.remove("a");
        })
        .unwrap();
        let store = ProjectTokenStore::load_from(&path).unwrap();
        assert!(store.get("a").is_none());
        assert_eq!(store.get("b").map(|e| e.token_id.as_str()), Some("b1"));
    }

    #[test]
    fn test_concurrent_updates_keep_every_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("project-tokens.toml");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    ProjectTokenStore::update_at(&path, |s| {
                        s.set(&format!("p{i}"), entry(&format!("t{i}")))
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let store = ProjectTokenStore::load_from(&path).unwrap();
        assert_eq!(store.projects.len(), 8);
    }

    #[test]
    fn test_a_broken_store_is_an_error_not_an_empty_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("project-tokens.toml");
        assert!(ProjectTokenStore::load_from(&path)
            .unwrap()
            .projects
            .is_empty());
        std::fs::write(&path, "[projects\nbroken").unwrap();
        assert!(ProjectTokenStore::load_from(&path).is_err());
        // An update does not replace a store it cannot read.
        assert!(ProjectTokenStore::update_at(&path, |s| s.set("a", entry("a1"))).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[projects\nbroken");
    }
}
