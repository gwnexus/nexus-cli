//! Nexus Core authentication module.
//!
//! Manages credential storage. Two scopes are supported, mirroring the
//! `nexus config set --local/--global` model:
//!
//! - **Local** (default): `.nexus/credentials.toml` in the current project
//!   workspace. Only affects the current project; never touches other
//!   projects' stored tokens.
//! - **Global** (`--global`, opt-in): `~/.config/nexus/credentials.toml`,
//!   shared across every repo on the machine.
//!
//! Tokens use the `nxs_pat_` prefix convention.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::Error;

mod project_token;

pub use project_token::{
    resolve_project_token, validate_project_token_format, ProjectTokenEntry, ProjectTokenStore,
    PROJECT_TOKEN_ENV, PROJECT_TOKEN_PREFIX,
};

/// Expected prefix for Nexus personal access tokens.
pub const TOKEN_PREFIX: &str = "nxs_pat_";

/// Stored credentials for authenticating against the Nexus API.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// The personal access token (starts with `nxs_pat_`).
    pub token: String,

    /// Optional ISO 8601 expiry timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    /// Returns the global credentials file path: `~/.config/nexus/credentials.toml`.
    ///
    /// Shared across every repo on the machine. Only written to when the
    /// caller explicitly opts in via `--global`.
    pub fn global_path() -> Result<PathBuf, Error> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Auth("unable to determine home directory".to_string()))?;
        Ok(home.join(".config").join("nexus").join("credentials.toml"))
    }

    /// Returns the project-local credentials file path:
    /// `<workspace>/.nexus/credentials.toml`.
    ///
    /// This is the default storage location for `nexus login`/`logout`.
    /// It is scoped to a single project workspace and never affects
    /// credentials stored for other projects. `workspace` defaults to the
    /// current working directory when `None`.
    pub fn local_path(workspace: Option<&Path>) -> Result<PathBuf, Error> {
        let base = match workspace {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir()?,
        };
        Ok(base.join(".nexus").join("credentials.toml"))
    }

    /// Backwards-compatible alias for [`global_path`](Self::global_path).
    #[deprecated(note = "use global_path() or local_path() explicitly")]
    pub fn path() -> Result<PathBuf, Error> {
        Self::global_path()
    }

    /// Load credentials from an explicit file path.
    /// Returns `None` if the file does not exist.
    pub fn load_from(path: &Path) -> Result<Option<Self>, Error> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let creds: Credentials = toml::from_str(&content)?;
        Ok(Some(creds))
    }

    /// Load credentials from the global store.
    /// Returns `None` if the file does not exist.
    pub fn load() -> Result<Option<Self>, Error> {
        Self::load_from(&Self::global_path()?)
    }

    /// Save credentials to an explicit file path with restrictive file
    /// permissions (0600 on Unix). Creates the parent directory if needed.
    pub fn save_to(&self, path: &Path) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;

        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(content.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(path, &content)?;
        }

        Ok(())
    }

    /// Save credentials to the global store.
    pub fn save(&self) -> Result<(), Error> {
        self.save_to(&Self::global_path()?)
    }

    /// Remove a credentials file at an explicit path, if it exists.
    pub fn remove_at(path: &Path) -> Result<(), Error> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Remove the global credentials file from disk.
    pub fn remove() -> Result<(), Error> {
        Self::remove_at(&Self::global_path()?)
    }

    /// Validate that a token string has the correct prefix.
    pub fn validate_token_format(token: &str) -> Result<(), Error> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(Error::Auth(format!(
                "invalid token format: must start with '{}'",
                TOKEN_PREFIX
            )));
        }
        if token.len() < TOKEN_PREFIX.len() + 10 {
            return Err(Error::Auth("token is too short to be valid".to_string()));
        }
        Ok(())
    }
}

/// Where a resolved token came from. Used to render provenance in
/// `nexus status` and to decide what `nexus logout` should touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `NEXUS_PRIVATE_TOKEN` environment variable.
    Env,
    /// Project-local `.nexus/credentials.toml`.
    Local,
    /// Global `~/.config/nexus/credentials.toml`.
    Global,
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env => write!(f, "env"),
            Self::Local => write!(f, "local"),
            Self::Global => write!(f, "global"),
        }
    }
}

/// Resolve an authentication token from available sources.
///
/// Checks in order:
/// 1. `NEXUS_PRIVATE_TOKEN` environment variable (useful for CI/CD and MCP servers)
/// 2. Project-local `.nexus/credentials.toml` (current workspace only)
/// 3. Global `~/.config/nexus/credentials.toml`
///
/// Returns `None` if no token is available.
pub fn resolve_token() -> Option<String> {
    resolve_token_with_source().map(|(token, _)| token)
}

/// Same as [`resolve_token`], but also reports which layer supplied the
/// token (env / local / global).
pub fn resolve_token_with_source() -> Option<(String, TokenSource)> {
    if let Ok(token) = std::env::var("NEXUS_PRIVATE_TOKEN") {
        if !token.is_empty() {
            return Some((token, TokenSource::Env));
        }
    }
    if let Ok(local_path) = Credentials::local_path(None) {
        if let Ok(Some(creds)) = Credentials::load_from(&local_path) {
            return Some((creds.token, TokenSource::Local));
        }
    }
    match Credentials::load() {
        Ok(Some(creds)) => Some((creds.token, TokenSource::Global)),
        _ => None,
    }
}

/// Resolve an authentication token, returning an error if none is found.
///
/// Same resolution order as [`resolve_token`], but returns a descriptive
/// error instead of `None`.
pub fn require_token() -> Result<String, Error> {
    resolve_token()
        .ok_or_else(|| Error::Auth("Not authenticated. Run 'nexus login' first.".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the PAT scope leak reported against `nexus login`:
    /// logging in to project A (local scope) must never mutate project B's
    /// locally-stored credentials, and vice versa.
    fn creds(token: &str) -> Credentials {
        Credentials {
            token: token.to_string(),
            expires_at: None,
        }
    }

    #[test]
    fn local_login_does_not_touch_other_projects_local_credentials() {
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();

        let path_a = Credentials::local_path(Some(project_a.path())).unwrap();
        let path_b = Credentials::local_path(Some(project_b.path())).unwrap();

        creds("nxs_pat_project_a_prod_token_0000")
            .save_to(&path_a)
            .unwrap();

        // Logging in to project B (local scope) must not touch project A.
        creds("nxs_pat_project_b_staging_token_0")
            .save_to(&path_b)
            .unwrap();

        let loaded_a = Credentials::load_from(&path_a).unwrap().unwrap();
        let loaded_b = Credentials::load_from(&path_b).unwrap().unwrap();

        assert_eq!(loaded_a.token, "nxs_pat_project_a_prod_token_0000");
        assert_eq!(loaded_b.token, "nxs_pat_project_b_staging_token_0");
    }

    #[test]
    fn local_logout_does_not_remove_other_projects_local_credentials() {
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();

        let path_a = Credentials::local_path(Some(project_a.path())).unwrap();
        let path_b = Credentials::local_path(Some(project_b.path())).unwrap();

        creds("nxs_pat_project_a_prod_token_0000")
            .save_to(&path_a)
            .unwrap();
        creds("nxs_pat_project_b_staging_token_0")
            .save_to(&path_b)
            .unwrap();

        // Logout in project B (local scope) must only remove B's token.
        Credentials::remove_at(&path_b).unwrap();

        assert!(Credentials::load_from(&path_a).unwrap().is_some());
        assert!(Credentials::load_from(&path_b).unwrap().is_none());
    }

    #[test]
    fn local_credentials_take_precedence_over_global_when_present() {
        // resolve_token_with_source() uses the real CWD for the "local"
        // check; here we only verify the path helper and load/save round
        // trip in isolation (full precedence is exercised by the CLI
        // integration tests, since resolve_token_with_source() is not
        // parameterizable by workspace).
        let workspace = tempfile::tempdir().unwrap();
        let local_path = Credentials::local_path(Some(workspace.path())).unwrap();
        assert!(Credentials::load_from(&local_path).unwrap().is_none());

        creds("nxs_pat_local_token_000000000000")
            .save_to(&local_path)
            .unwrap();
        let loaded = Credentials::load_from(&local_path).unwrap().unwrap();
        assert_eq!(loaded.token, "nxs_pat_local_token_000000000000");
    }
}
