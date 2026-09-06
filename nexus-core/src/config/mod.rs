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
const DEFAULT_API_URL: &str = "https://nexus.gatewarden.eu";

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
/// published npm package (`npx @gwdn/nexus-mcp`) or a local checkout
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

    /// Check for CLI updates on startup (default: true).
    /// Uses a local cache to avoid network calls on every invocation.
    #[serde(default = "default_check_updates")]
    pub check_updates: bool,

    /// Configuration for `nexus run`.
    #[serde(default)]
    pub run: RunConfig,
}

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

fn default_check_updates() -> bool {
    true
}

fn default_run_tool() -> String {
    "opencode".to_string()
}

fn default_launch_countdown_secs() -> u64 {
    5
}

/// Configuration for `nexus run` stored in `[run]` section of `~/.config/nexus/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    /// Default tool binary to launch (default: "opencode").
    #[serde(default = "default_run_tool")]
    pub default_tool: String,

    /// Seconds to count down after pre-launch checks before starting the tool (default: 5).
    /// Set to 0 to skip the countdown and launch immediately.
    #[serde(default = "default_launch_countdown_secs")]
    pub launch_countdown_secs: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            default_tool: default_run_tool(),
            launch_countdown_secs: default_launch_countdown_secs(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_API_URL.to_string(),
            default_output: OutputPreference::default(),
            no_color: false,
            mcp_source: McpSource::default(),
            check_updates: true,
            run: RunConfig::default(),
        }
    }
}

/// Which configuration layer supplied a resolved value.
///
/// Mirrors git's `--local` / `--global` provenance so `nexus config show`
/// can indicate at a glance which layer is in effect for each key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Supplied by the project-local `.nexus/config.toml` `[config]` section.
    Local,
    /// Supplied by the global `~/.config/nexus/config.toml`.
    Global,
    /// Neither layer set this key; compiled-in default is in effect.
    Default,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Global => write!(f, "global"),
            Self::Default => write!(f, "default"),
        }
    }
}

/// The effective configuration merged from the project-local and global
/// layers, plus per-key provenance for display purposes.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// The merged configuration (local overrides applied on top of global).
    pub config: Config,
    /// Which layer supplied `api_url`.
    pub api_url_source: ConfigSource,
    /// Which layer supplied `default_output`.
    pub default_output_source: ConfigSource,
    /// Which layer supplied `no_color`.
    pub no_color_source: ConfigSource,
    /// Path to the project-local config file, if it exists.
    pub local_path: Option<PathBuf>,
    /// Path to the global config file (may not exist yet).
    pub global_path: PathBuf,
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, &content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
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
            "check_updates" => {
                self.check_updates = value
                    .parse::<bool>()
                    .map_err(|_| Error::Config(format!("invalid bool value: '{}'", value)))?;
                Ok(())
            }
            "run.default_tool" => {
                self.run.default_tool = value.to_string();
                Ok(())
            }
            "run.launch_countdown_secs" => {
                self.run.launch_countdown_secs = value
                    .parse::<u64>()
                    .map_err(|_| Error::Config(format!("invalid u64 value: '{}'", value)))?;
                Ok(())
            }
            other => Err(Error::Config(format!(
                "unknown config key '{}', valid keys: api_url, default_output, no_color, mcp_source, check_updates, run.default_tool, run.launch_countdown_secs",
                other
            ))),
        }
    }

    /// Load the effective configuration for a working directory.
    ///
    /// Precedence (highest first, mirrors git's local -> global -> system model):
    /// 1. Project-local `.nexus/config.toml` `[config]` section
    /// 2. Global `~/.config/nexus/config.toml`
    /// 3. Compiled-in defaults
    ///
    /// `workspace` defaults to the current working directory when `None`.
    pub fn load_effective(workspace: Option<&std::path::Path>) -> Result<Config, Error> {
        Ok(Self::load_effective_with_provenance(workspace)?.config)
    }

    /// Same as [`load_effective`](Self::load_effective), but also reports which
    /// layer supplied each overridable key. Used by `nexus config show`.
    pub fn load_effective_with_provenance(
        workspace: Option<&std::path::Path>,
    ) -> Result<EffectiveConfig, Error> {
        let global_path = Self::path()?;
        let global_exists = global_path.exists();
        let mut config = Self::load()?;

        let base_source = if global_exists {
            ConfigSource::Global
        } else {
            ConfigSource::Default
        };
        let mut api_url_source = base_source;
        let mut default_output_source = base_source;
        let mut no_color_source = base_source;

        let local_overrides = load_project_config(workspace)?.and_then(|pc| pc.config);
        let local_path = project_config_path(workspace).ok().filter(|p| p.exists());

        if let Some(overrides) = local_overrides {
            if let Some(url) = overrides.api_url {
                config.api_url = url;
                api_url_source = ConfigSource::Local;
            }
            if let Some(fmt) = overrides.default_output {
                config.default_output = fmt;
                default_output_source = ConfigSource::Local;
            }
            if let Some(nc) = overrides.no_color {
                config.no_color = nc;
                no_color_source = ConfigSource::Local;
            }
        }

        Ok(EffectiveConfig {
            config,
            api_url_source,
            default_output_source,
            no_color_source,
            local_path,
            global_path,
        })
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

/// Extra MCP server definition for `[mcp_extra.<name>]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraMcpServer {
    /// Command array, e.g. ["npx", "-y", "task-master-ai@latest"]
    pub command: Vec<String>,

    /// Optional environment variables for this MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<std::collections::HashMap<String, String>>,
}

/// Plugin definition for `[plugins.<name>]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDef {
    /// Source type: "github-raw", "local"
    #[serde(default = "default_plugin_source")]
    pub source: String,

    /// URL to download the plugin from (for github-raw source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Local path to copy from (for local source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Target filename inside .opencode/plugins/ (derived from URL if not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

fn default_plugin_source() -> String {
    "github-raw".to_string()
}

/// Project-local overrides for a subset of global config keys, stored under
/// the `[config]` section of `.nexus/config.toml`.
///
/// Written by `nexus config set --local KEY=VALUE`. Any key left unset here
/// falls back to the global config, then to compiled-in defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalConfigOverrides {
    /// Project-scoped override for the Nexus API base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,

    /// Project-scoped override for the default output format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_output: Option<OutputPreference>,

    /// Project-scoped override for disabling colored output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_color: Option<bool>,
}

impl LocalConfigOverrides {
    /// Update a single local override key by name.
    /// Returns an error if the key is not supported for `--local` scope.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        match key {
            "api_url" => {
                self.api_url = Some(value.to_string());
                Ok(())
            }
            "default_output" => {
                self.default_output = Some(value.parse()?);
                Ok(())
            }
            "no_color" => {
                self.no_color = Some(
                    value
                        .parse::<bool>()
                        .map_err(|_| Error::Config(format!("invalid bool value: '{}'", value)))?,
                );
                Ok(())
            }
            other => Err(Error::Config(format!(
                "unknown local config key '{}', valid keys for --local: api_url, default_output, no_color",
                other
            ))),
        }
    }
}

/// The full `.nexus/config.toml` file structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<toml::Value>,

    /// Extra MCP servers to merge into opencode.json on init.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_extra: Option<std::collections::HashMap<String, ExtraMcpServer>>,

    /// Plugins to install into .opencode/plugins/ on init.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<std::collections::HashMap<String, PluginDef>>,

    /// Project-local overrides for a subset of global config keys
    /// (`api_url`, `default_output`, `no_color`). Written by
    /// `nexus config set --local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<LocalConfigOverrides>,
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
