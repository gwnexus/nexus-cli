//! Project inference token storage (`nxs_proj_*`).
//!
//! Project tokens authenticate CLIENT inference traffic against the Nexus
//! Model Gateway (gateway ADR-0005). They are distinct from the user PAT
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
}

impl std::fmt::Debug for ProjectTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectTokenStore")
            .field("projects", &self.projects.keys().collect::<Vec<_>>())
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
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let store: ProjectTokenStore = toml::from_str(&content)?;
        Ok(store)
    }

    /// Save the store to disk with restrictive file permissions (0600 on Unix).
    pub fn save(&self) -> Result<(), Error> {
        let path = Self::path()?;
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
                .open(&path)?;
            file.write_all(content.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&path, &content)?;
        }

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
    if let Ok(token) = std::env::var(PROJECT_TOKEN_ENV) {
        if !token.is_empty() {
            return Some(token);
        }
    }
    match ProjectTokenStore::load() {
        Ok(store) => store.get(project_id).map(|e| e.token.clone()),
        Err(_) => None,
    }
}
