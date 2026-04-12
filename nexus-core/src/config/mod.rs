//! Nexus Core configuration module.
//!
//! Manages the CLI configuration file at `~/.config/nexus/config.toml`.
//! Provides defaults and cascading resolution for CLI flags.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::Error;

/// Default Nexus API base URL.
const DEFAULT_API_URL: &str = "https://nexus.mpowr.tech";

/// Output format preference, stored in config and resolved from CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputPreference {
    Table,
    Json,
    Plain,
}

impl Default for OutputPreference {
    fn default() -> Self {
        Self::Table
    }
}

impl fmt::Display for OutputPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => write!(f, "table"),
            Self::Json => write!(f, "json"),
            Self::Plain => write!(f, "plain"),
        }
    }
}

impl FromStr for OutputPreference {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "plain" => Ok(Self::Plain),
            other => Err(Error::Config(format!(
                "unknown output format '{}', expected: table, json, plain",
                other
            ))),
        }
    }
}

/// CLI configuration loaded from `~/.config/nexus/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Nexus API base URL.
    #[serde(default = "default_api_url")]
    pub api_url: String,

    /// Default output format.
    #[serde(default)]
    pub default_output: OutputPreference,

    /// Disable colored output.
    #[serde(default)]
    pub no_color: bool,
}

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_API_URL.to_string(),
            default_output: OutputPreference::default(),
            no_color: false,
        }
    }
}

impl Config {
    /// Returns the configuration directory path: `~/.config/nexus/`.
    pub fn dir() -> Result<PathBuf, Error> {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::Config("unable to determine home directory".to_string())
        })?;
        Ok(home.join(".config").join("nexus"))
    }

    /// Returns the configuration file path: `~/.config/nexus/config.toml`.
    pub fn path() -> Result<PathBuf, Error> {
        Ok(Self::dir()?.join("config.toml"))
    }

    /// Load configuration from disk.
    /// Returns `Default` if the file does not exist.
    pub fn load() -> Result<Self, Error> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save the current configuration to disk.
    /// Creates the parent directory if it does not exist.
    pub fn save(&self) -> Result<(), Error> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Update a single configuration key by name.
    /// Returns an error if the key is not recognized.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        match key {
            "api_url" => {
                self.api_url = value.to_string();
                Ok(())
            }
            "default_output" => {
                self.default_output = value.parse()?;
                Ok(())
            }
            "no_color" => {
                self.no_color = value
                    .parse::<bool>()
                    .map_err(|_| Error::Config(format!("invalid bool value: '{}'", value)))?;
                Ok(())
            }
            other => Err(Error::Config(format!(
                "unknown config key '{}', valid keys: api_url, default_output, no_color",
                other
            ))),
        }
    }
}
