//! Nexus CLI entry point.
//!
//! The `nexus` binary provides project scaffolding, authentication,
//! and configuration management for the Nexus platform.

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod cmd;

/// Nexus CLI -- project scaffolding and platform tools for mpowr-nexus.
#[derive(Debug, Parser)]
#[command(
    name = "nexus",
    version,
    about = "Nexus CLI for mpowr-nexus platform operations",
    long_about = "Project scaffolding, authentication, and configuration management\nfor the mpowr-nexus multi-agent collaboration platform."
)]
pub struct Cli {
    /// Nexus API base URL (overrides config file).
    #[arg(long, global = true)]
    pub api_url: Option<String>,

    /// Output format: table, json, plain (overrides config file).
    #[arg(long, global = true)]
    pub output: Option<String>,

    /// Enable verbose/debug output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Accept all defaults without prompting (non-interactive mode).
    #[arg(short = 'y', long = "yes", global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Available CLI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new Nexus project workspace.
    Init {
        /// Target directory (defaults to current directory).
        #[arg(default_value = ".")]
        path: String,

        /// Project name (defaults to directory name).
        #[arg(short, long)]
        name: Option<String>,

        /// Nexus project UUID (enables server-aware init: pulls skills, commands, MCP config).
        #[arg(long)]
        project_id: Option<String>,

        /// Skip interactive prompts and use defaults.
        #[arg(short, long)]
        force: bool,

        /// Shadow all AI scaffold files in .gitignore (AGENTS.md, .claude/, .opencode/, opencode.json).
        #[arg(long)]
        shadowed_ai: bool,
    },

    /// Link this directory to a Nexus project.
    Link {
        /// Nexus project UUID to link directly (skips interactive selection).
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Unlink this directory from its Nexus project.
    Unlink,

    /// Remove all Nexus/AI scaffold files from this directory.
    Deinit {
        /// Delete without confirmation prompt.
        #[arg(short, long)]
        force: bool,
    },

    /// Authenticate with the Nexus platform.
    Login,

    /// Remove stored credentials.
    Logout,

    /// Show current authentication and project status.
    Status,

    /// Pull skills and configuration from the Nexus platform into this workspace.
    Pull {
        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,

        /// Overwrite existing files without confirmation.
        #[arg(short, long)]
        force: bool,
    },

    /// Skills management subcommands.
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },

    /// View or update CLI configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Run preflight checks to verify environment readiness.
    Preflight,
}

/// Configuration subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Show the current configuration.
    Show,

    /// Set a configuration value (KEY=VALUE).
    Set {
        /// Configuration key=value pair.
        pair: String,
    },

    /// Show the configuration file path.
    Path,
}

/// Skills management subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// List all skills for the current tenant.
    List {
        /// Filter by status: draft, active, archived (comma-separated, default: draft,active).
        #[arg(long)]
        status: Option<String>,

        /// Maximum number of skills to return (default: 50, max: 100).
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Export enabled skills for the linked project as JSON.
    Export {
        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },
}

impl Cli {
    /// Resolve the effective output format from CLI flag -> config -> default.
    pub fn resolve_output(
        &self,
        config: &nexus_core::config::Config,
    ) -> nexus_core::OutputPreference {
        if let Some(ref fmt) = self.output {
            fmt.parse().unwrap_or(config.default_output)
        } else {
            config.default_output
        }
    }

    /// Resolve the effective API URL from CLI flag -> config -> default.
    pub fn resolve_api_url(&self, config: &nexus_core::config::Config) -> String {
        if let Some(ref url) = self.api_url {
            url.clone()
        } else {
            config.api_url.clone()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env().add_directive("nexus=info".parse()?)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    cmd::dispatch(cli).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_parse_init_default() {
        let cli = Cli::try_parse_from(["nexus", "init"]).unwrap();
        match cli.command {
            Command::Init {
                ref path,
                ref name,
                ref project_id,
                force,
                shadowed_ai,
            } => {
                assert_eq!(path, ".");
                assert!(name.is_none());
                assert!(project_id.is_none());
                assert!(!force);
                assert!(!shadowed_ai);
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_parse_init_with_path_and_name() {
        let cli =
            Cli::try_parse_from(["nexus", "init", "/tmp/myproject", "-n", "My Project"]).unwrap();
        match cli.command {
            Command::Init {
                ref path,
                ref name,
                ref project_id,
                force,
                ..
            } => {
                assert_eq!(path, "/tmp/myproject");
                assert_eq!(name.as_deref(), Some("My Project"));
                assert!(project_id.is_none());
                assert!(!force);
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_parse_init_force() {
        let cli = Cli::try_parse_from(["nexus", "init", "--force"]).unwrap();
        match cli.command {
            Command::Init { force, .. } => assert!(force),
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_parse_init_with_project_id() {
        let cli = Cli::try_parse_from([
            "nexus",
            "init",
            ".",
            "--project-id",
            "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Command::Init {
                ref project_id,
                force,
                ..
            } => {
                assert_eq!(
                    project_id.as_deref(),
                    Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")
                );
                assert!(force);
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_parse_link_no_args() {
        let cli = Cli::try_parse_from(["nexus", "link"]).unwrap();
        match cli.command {
            Command::Link { ref project_id } => {
                assert!(project_id.is_none());
            }
            _ => panic!("expected Link command"),
        }
    }

    #[test]
    fn test_parse_link_with_project_id() {
        let cli = Cli::try_parse_from([
            "nexus",
            "link",
            "--project-id",
            "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        ])
        .unwrap();
        match cli.command {
            Command::Link { ref project_id } => {
                assert_eq!(
                    project_id.as_deref(),
                    Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")
                );
            }
            _ => panic!("expected Link command"),
        }
    }

    #[test]
    fn test_parse_unlink() {
        let cli = Cli::try_parse_from(["nexus", "unlink"]).unwrap();
        assert!(matches!(cli.command, Command::Unlink));
    }

    #[test]
    fn test_parse_deinit_no_force() {
        let cli = Cli::try_parse_from(["nexus", "deinit"]).unwrap();
        match cli.command {
            Command::Deinit { force } => assert!(!force),
            _ => panic!("expected Deinit command"),
        }
    }

    #[test]
    fn test_parse_deinit_with_force() {
        let cli = Cli::try_parse_from(["nexus", "deinit", "--force"]).unwrap();
        match cli.command {
            Command::Deinit { force } => assert!(force),
            _ => panic!("expected Deinit command"),
        }
    }

    #[test]
    fn test_parse_login() {
        let cli = Cli::try_parse_from(["nexus", "login"]).unwrap();
        assert!(matches!(cli.command, Command::Login));
    }

    #[test]
    fn test_parse_logout() {
        let cli = Cli::try_parse_from(["nexus", "logout"]).unwrap();
        assert!(matches!(cli.command, Command::Logout));
    }

    #[test]
    fn test_parse_status() {
        let cli = Cli::try_parse_from(["nexus", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn test_parse_config_show() {
        let cli = Cli::try_parse_from(["nexus", "config", "show"]).unwrap();
        match cli.command {
            Command::Config {
                action: ConfigAction::Show,
            } => {}
            _ => panic!("expected Config Show"),
        }
    }

    #[test]
    fn test_parse_config_set() {
        let cli =
            Cli::try_parse_from(["nexus", "config", "set", "api_url=https://custom.url"]).unwrap();
        match cli.command {
            Command::Config {
                action: ConfigAction::Set { ref pair },
            } => {
                assert_eq!(pair, "api_url=https://custom.url");
            }
            _ => panic!("expected Config Set"),
        }
    }

    #[test]
    fn test_parse_config_path() {
        let cli = Cli::try_parse_from(["nexus", "config", "path"]).unwrap();
        match cli.command {
            Command::Config {
                action: ConfigAction::Path,
            } => {}
            _ => panic!("expected Config Path"),
        }
    }

    #[test]
    fn test_global_verbose_flag() {
        let cli = Cli::try_parse_from(["nexus", "--verbose", "status"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn test_global_api_url_flag() {
        let cli =
            Cli::try_parse_from(["nexus", "--api-url", "https://custom.api", "status"]).unwrap();
        assert_eq!(cli.api_url.as_deref(), Some("https://custom.api"));
    }

    #[test]
    fn test_global_output_flag() {
        let cli = Cli::try_parse_from(["nexus", "--output", "json", "status"]).unwrap();
        assert_eq!(cli.output.as_deref(), Some("json"));
    }

    #[test]
    fn test_unknown_command_fails() {
        let result = Cli::try_parse_from(["nexus", "unknown"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_pull_no_args() {
        let cli = Cli::try_parse_from(["nexus", "pull"]).unwrap();
        match cli.command {
            Command::Pull {
                ref project_id,
                force,
            } => {
                assert!(project_id.is_none());
                assert!(!force);
            }
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_pull_with_project_id() {
        let cli = Cli::try_parse_from([
            "nexus",
            "pull",
            "--project-id",
            "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        ])
        .unwrap();
        match cli.command {
            Command::Pull {
                ref project_id,
                force,
            } => {
                assert_eq!(
                    project_id.as_deref(),
                    Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")
                );
                assert!(!force);
            }
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_pull_with_force() {
        let cli = Cli::try_parse_from(["nexus", "pull", "--force"]).unwrap();
        match cli.command {
            Command::Pull { force, .. } => assert!(force),
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_pull_short_force() {
        let cli = Cli::try_parse_from(["nexus", "pull", "-f"]).unwrap();
        match cli.command {
            Command::Pull { force, .. } => assert!(force),
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_skills_export_no_args() {
        let cli = Cli::try_parse_from(["nexus", "skills", "export"]).unwrap();
        match cli.command {
            Command::Skills {
                action: SkillsAction::Export { ref project_id },
            } => {
                assert!(project_id.is_none());
            }
            _ => panic!("expected Skills Export command"),
        }
    }

    #[test]
    fn test_parse_skills_export_with_project_id() {
        let cli = Cli::try_parse_from([
            "nexus",
            "skills",
            "export",
            "--project-id",
            "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        ])
        .unwrap();
        match cli.command {
            Command::Skills {
                action: SkillsAction::Export { ref project_id },
            } => {
                assert_eq!(
                    project_id.as_deref(),
                    Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")
                );
            }
            _ => panic!("expected Skills Export command"),
        }
    }

    #[test]
    fn test_parse_preflight() {
        let cli = Cli::try_parse_from(["nexus", "preflight"]).unwrap();
        assert!(matches!(cli.command, Command::Preflight));
    }

    #[test]
    fn test_parse_init_shadowed_ai() {
        let cli = Cli::try_parse_from(["nexus", "init", "--shadowed-ai"]).unwrap();
        match cli.command {
            Command::Init { shadowed_ai, .. } => assert!(shadowed_ai),
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_global_yes_flag() {
        let cli = Cli::try_parse_from(["nexus", "-y", "init"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn test_global_yes_long_flag() {
        let cli = Cli::try_parse_from(["nexus", "--yes", "deinit"]).unwrap();
        assert!(cli.yes);
    }
}
