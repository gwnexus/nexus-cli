//! Nexus Core authentication module.
//!
//! Manages credential storage at `~/.config/nexus/credentials.toml`.
//! Tokens use the `nxs_pat_` prefix convention.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::Error;

/// Expected prefix for Nexus personal access tokens.
pub const TOKEN_PREFIX: &str = "nxs_pat_";

/// Stored credentials for authenticating against the Nexus API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// The personal access token (starts with `nxs_pat_`).
    pub token: String,

    /// Optional ISO 8601 expiry timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl Credentials {
    /// Returns the credentials file path: `~/.config/nexus/credentials.toml`.
    pub fn path() -> Result<PathBuf, Error> {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::Auth("unable to determine home directory".to_string())
        })?;
        Ok(home.join(".config").join("nexus").join("credentials.toml"))
    }

    /// Load credentials from disk.
    /// Returns `None` if the file does not exist.
    pub fn load() -> Result<Option<Self>, Error> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let creds: Credentials = toml::from_str(&content)?;
        Ok(Some(creds))
    }

    /// Save credentials to disk with restrictive file permissions (0600 on Unix).
    /// Creates the parent directory if it does not exist.
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

    /// Remove the credentials file from disk.
    pub fn remove() -> Result<(), Error> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
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
            return Err(Error::Auth(
                "token is too short to be valid".to_string(),
            ));
        }
        Ok(())
    }
}
