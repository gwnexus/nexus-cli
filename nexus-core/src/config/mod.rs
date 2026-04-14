//! Nexus Core configuration module.
//!
//! Manages the CLI configuration file at `~/.config/nexus/config.toml`
//! and the project-local configuration at `.nexus/config.toml`.
//! Provides defaults and cascading resolution for CLI flags.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::Error;

/// Default Nexus API base URL.
const DEFAULT_API_URL: &str = "https://nexus.mpowr.tech";

/// Output format preference, stored in config and resolved from CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputPreference {
    #[default]
    Table,
    Json,
    Plain,
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

/// MCP server source preference.
///
/// Controls whether `nexus init` generates MCP configs pointing to the
/// published npm package (`npx @mpowr/nexus-mcp`) or a local checkout
/// (`node tools/nexus-mcp/dist/server.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpSource {
    /// Use the published npm package via npx (default).
    #[default]
    Npm,
    /// Use a local checkout of the MCP server.
    Local,
}

impl fmt::Display for McpSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Npm => write!(f, "npm"),
            Self::Local => write!(f, "local"),
        }
    }
}

impl FromStr for McpSource {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "npm" => Ok(Self::Npm),
            "local" => Ok(Self::Local),
            other => Err(Error::Config(format!(
                "unknown mcp_source '{}', expected: npm, local",
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

    /// MCP server source: npm (default) or local.
    #[serde(default)]
    pub mcp_source: McpSource,
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
            mcp_source: McpSource::default(),
        }
    }
}

impl Config {
    /// Returns the configuration directory path: `~/.config/nexus/`.
    pub fn dir() -> Result<PathBuf, Error> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("unable to determine home directory".to_string()))?;
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
            "mcp_source" => {
                self.mcp_source = value.parse()?;
                Ok(())
            }
            other => Err(Error::Config(format!(
                "unknown config key '{}', valid keys: api_url, default_output, no_color, mcp_source",
                other
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Project-local configuration (.nexus/config.toml)
// ---------------------------------------------------------------------------

/// Project information stored in `.nexus/config.toml` under the `[project]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Nexus project UUID.
    pub id: String,

    /// Human-readable project name.
    #[serde(default)]
    pub name: String,

    /// URL-safe project slug.
    #[serde(default)]
    pub slug: String,
}

/// The full `.nexus/config.toml` file structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<toml::Value>,
}

/// Returns the path to the project-local `.nexus/config.toml`,
/// searching from the given directory (or cwd).
pub fn project_config_path(from: Option<&std::path::Path>) -> Result<PathBuf, Error> {
    let base = match from {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    Ok(base.join(".nexus").join("config.toml"))
}

/// Load the project-local config from `.nexus/config.toml`.
/// Returns `None` if the file does not exist.
pub fn load_project_config(from: Option<&std::path::Path>) -> Result<Option<ProjectConfig>, Error> {
    let path = project_config_path(from)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let config: ProjectConfig = toml::from_str(&content)?;
    Ok(Some(config))
}

/// Save the project-local config to `.nexus/config.toml`.
/// Creates `.nexus/` if it doesn't exist.
pub fn save_project_config(
    from: Option<&std::path::Path>,
    config: &ProjectConfig,
) -> Result<(), Error> {
    let path = project_config_path(from)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Remove the `[project]` section from `.nexus/config.toml`.
/// Preserves other sections (e.g. `[mcp]`).
pub fn remove_project_section(from: Option<&std::path::Path>) -> Result<bool, Error> {
    let path = project_config_path(from)?;
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&path)?;
    let mut config: ProjectConfig = toml::from_str(&content)?;
    if config.project.is_none() {
        return Ok(false);
    }
    config.project = None;
    let new_content = toml::to_string_pretty(&config)?;
    std::fs::write(&path, new_content)?;
    Ok(true)
}

/// Load the linked project info from `.nexus/config.toml`.
/// Returns `None` if not linked.
pub fn load_linked_project(from: Option<&std::path::Path>) -> Result<Option<ProjectInfo>, Error> {
    match load_project_config(from)? {
        Some(pc) => Ok(pc.project),
        None => Ok(None),
    }
}

/// Resolve a project ID from multiple sources (highest priority first):
/// 1. Explicit CLI flag (`--project-id`)
/// 2. `.nexus/config.toml` `[project].id`
///
/// Returns an error with a helpful message if neither is set.
pub fn resolve_project_id(
    cli_project_id: Option<&str>,
    workspace: Option<&std::path::Path>,
) -> Result<String, Error> {
    // 1. CLI flag takes priority
    if let Some(id) = cli_project_id {
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }

    // 2. Check .nexus/config.toml
    if let Some(project) = load_linked_project(workspace)? {
        if !project.id.is_empty() {
            return Ok(project.id);
        }
    }

    Err(Error::Config(
        "No project ID found. Either:\n  \
         - Pass --project-id <UUID>\n  \
         - Run 'nexus link' to link this directory to a project"
            .to_string(),
    ))
}
