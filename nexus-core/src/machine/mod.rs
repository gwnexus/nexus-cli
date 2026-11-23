//! Machine identity module.
//!
//! Generates and persists a unique machine identifier at
//! `~/.config/nexus/machine.toml`. The ID is a UUID v4 created on first use
//! and reused for the lifetime of the installation.
//!
//! This ID is sent with session-related API calls to correlate work across
//! sessions to a specific development machine.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::Error;

/// Stored machine identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineIdentity {
    /// Unique machine identifier (UUID v4).
    pub machine_id: String,
}

impl MachineIdentity {
    /// Returns the machine identity file path: `~/.config/nexus/machine.toml`.
    pub fn path() -> Result<PathBuf, Error> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("unable to determine home directory".to_string()))?;
        Ok(home.join(".config").join("nexus").join("machine.toml"))
    }

    /// Load or create the machine identity.
    ///
    /// If the file exists, loads and returns the stored ID.
    /// If the file does not exist, generates a new UUID v4 and persists it.
    pub fn load_or_create() -> Result<Self, Error> {
        let path = Self::path()?;
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let identity: MachineIdentity = toml::from_str(&content)?;
            return Ok(identity);
        }

        let identity = Self {
            machine_id: Uuid::new_v4().to_string(),
        };
        identity.save()?;
        Ok(identity)
    }

    /// Load the machine identity from disk.
    /// Returns `None` if the file does not exist.
    pub fn load() -> Result<Option<Self>, Error> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let identity: MachineIdentity = toml::from_str(&content)?;
        Ok(Some(identity))
    }

    /// Save the machine identity to disk with restrictive permissions (0600 on Unix).
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

    /// Returns the machine ID string.
    pub fn id(&self) -> &str {
        &self.machine_id
    }
}

/// Resolve the machine ID, creating it if necessary.
///
/// This is the primary entry point for consumers. Always returns a valid
/// machine ID string.
pub fn resolve_machine_id() -> Result<String, Error> {
    let identity = MachineIdentity::load_or_create()?;
    Ok(identity.machine_id)
}
