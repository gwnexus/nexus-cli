//! Nexus CLI entry point.
//!
//! The `nexus` binary provides project scaffolding, authentication,
//! and configuration management for the Nexus platform.

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod cmd;

/// Nexus CLI -- project scaffolding and platform tools for Nexus.
#[derive(Debug, Parser)]
#[command(
    name = "nexus",
    version = env!("CARGO_PKG_VERSION"),
    about = "Nexus CLI — Nexus platform operations",
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

        /// [DEPRECATED] Shadow flag is no longer needed; .nexus/ is always excluded via .git/info/exclude.
        #[arg(long, hide = true)]
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

        /// Limit pull to specific scope(s): skills, directives, agents, workspace, tasks.
        /// Can be specified multiple times. If omitted, pulls everything.
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,

        /// Download actor avatar SVGs into .nexus/actors/assets/.
        #[arg(long)]
        with_actor_assets: bool,

        /// Skip actor asset download (metadata only). This is the default.
        #[arg(long)]
        skip_actor_assets: bool,
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

    /// Upgrade the Nexus CLI to the latest release version.
    Upgrade,

    /// Shadow (hide) AI/agentic scaffold files from Git tracking.
    Shadow {
        #[command(subcommand)]
        action: ShadowAction,
    },

    /// Manage workspace shadow mode (exclude devbox files from Git tracking).
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },

    /// Import existing agentic files (CLAUDE.md, AGENTS.md, .cursorrules, etc.) into the linked Nexus project.
    Import {
        /// Only detect files, don't import.
        #[arg(long)]
        dry_run: bool,
    },

    /// Sync agent files between the local workspace and the Nexus platform.
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },

    /// Manage per-project git identity settings (user.name, user.email, GPG signing).
    Git {
        #[command(subcommand)]
        action: GitAction,
    },

    /// Manage actors assigned to the linked project.
    Actors {
        #[command(subcommand)]
        action: ActorsAction,
    },

    /// Launch a tool (default: opencode) with platform-managed env vars injected.
    ///
    /// Resolves env vars from .nexus/env (plugin defaults) and .env.nexus.local
    /// (secrets), runs a pre-launch check, then spawns the tool. After the tool
    /// exits, prints a session summary with duration and git activity.
    /// Shell vars already set are never overwritten.
    Run {
        /// Tool binary to launch (default: opencode, or config run.default_tool).
        #[arg(short, long)]
        tool: Option<String>,

        /// Print resolved env block and exit without launching the tool.
        #[arg(long)]
        dry_run: bool,

        /// Print resolved env block, then confirm before launching.
        #[arg(long)]
        show_env: bool,

        /// Skip af_export API call; use only .nexus/env from disk.
        #[arg(long)]
        no_db: bool,

        /// Use exec() instead of spawn+wait (no post-session summary, Unix only).
        #[arg(long)]
        exec: bool,

        /// Skip pre-launch checks.
        #[arg(long)]
        skip_checks: bool,

        /// Extra arguments forwarded verbatim to the tool.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

/// Shadow mode subcommands.
#[derive(Debug, Subcommand)]
pub enum ShadowAction {
    /// Enable shadow mode: exclude agentic files from Git tracking.
    On,
    /// Disable shadow mode: remove exclusions and re-track agentic files.
    Off,
    /// Show current shadow mode status.
    Status,
}

/// Workspace shadow subcommands.
#[derive(Debug, Subcommand)]
pub enum WorkspaceAction {
    /// Enable workspace shadow: exclude devbox.json, devbox.lock, .devbox/, scripts/devbox/ from Git.
    #[command(name = "shadow")]
    Shadow {
        #[command(subcommand)]
        action: WorkspaceShadowAction,
    },
}

/// Workspace shadow on/off/status.
#[derive(Debug, Subcommand)]
pub enum WorkspaceShadowAction {
    /// Enable workspace shadow mode.
    On,
    /// Disable workspace shadow mode.
    Off,
    /// Show workspace shadow status.
    Status,
}

/// Sync subcommands for agent file synchronization.
#[derive(Debug, Subcommand)]
pub enum SyncAction {
    /// Show sync status of all agent files (local vs platform).
    Status {
        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Push local agent file changes to the platform.
    Push {
        /// The file key to push (e.g. "AGENTS.md", "CLAUDE.md").
        file_key: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Discard local changes and reset to the platform version.
    Reset {
        /// The file key to reset (e.g. "AGENTS.md", "CLAUDE.md").
        file_key: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },
}

/// Git identity subcommands.
#[derive(Debug, Subcommand)]
pub enum GitAction {
    /// Verify local git config matches the project's expected identity.
    Verify,
    /// Apply the project's git identity settings to local .git/config.
    Apply,
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

/// Actors management subcommands.
#[derive(Debug, Subcommand)]
pub enum ActorsAction {
    /// List actors assigned to the linked project.
    List {
        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Show full actor profile.
    Show {
        /// Actor slug or UUID.
        slug: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Normalize actor markdown to YAML frontmatter format.
    Normalize {
        /// Path to actor markdown file.
        path: String,
    },

    /// Validate actor profile(s) against schema and model route catalog.
    Validate {
        /// Path to actor markdown file.
        path: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Import validated actor profiles into the Actor Registry.
    Import {
        /// Path to actor markdown file or directory.
        path: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Export actor configuration for a target format.
    Export {
        /// Export target format.
        #[arg(long, default_value = "opencode")]
        target: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Manage actor avatars.
    Avatar {
        #[command(subcommand)]
        action: ActorAvatarAction,
    },
}

/// Actor avatar subcommands.
#[derive(Debug, Subcommand)]
pub enum ActorAvatarAction {
    /// Trigger avatar regeneration via API.
    Generate {
        /// Actor slug.
        slug: String,

        /// Override the linked project ID.
        #[arg(long)]
        project_id: Option<String>,
    },

    /// Reset avatar to DiceBear default.
    Reset {
        /// Actor slug.
        slug: String,

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
    // Build dynamic about string with version + copyright
    let version = env!("CARGO_PKG_VERSION");
    let about = format!(
        "Nexus CLI v{version} \u{2014} Nexus platform operations\n(C) 2025-present RelicFrog Holding UG | Nexus Product Group"
    );

    let cli = Cli::from_arg_matches(&Cli::command().about(about).get_matches())?;

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

    // Load config for update check
    let config = nexus_core::config::Config::load().unwrap_or_default();

    // Spawn update check in background (non-blocking)
    let update_handle = if config.check_updates {
        Some(tokio::spawn(async move {
            nexus_core::update_check::check_for_update(&config).await
        }))
    } else {
        None
    };

    let result = cmd::dispatch(cli).await;

    // Show update notice after command output (if available)
    if let Some(handle) = update_handle {
        if let Ok(Some(info)) = handle.await {
            use console::style;
            eprintln!();
            eprintln!(
                "{} A new version of Nexus CLI is available: {} -> {}",
                style("UPDATE").bold().yellow(),
                style(&info.current).dim(),
                style(&info.latest).bold().green(),
            );
            eprintln!("   Run {} to upgrade.", style("nexus upgrade").bold());
        }
    }

    result
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
                ..
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
                ..
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

    #[test]
    fn test_parse_upgrade() {
        let cli = Cli::try_parse_from(["nexus", "upgrade"]).unwrap();
        assert!(matches!(cli.command, Command::Upgrade));
    }

    #[test]
    fn test_parse_shadow_on() {
        let cli = Cli::try_parse_from(["nexus", "shadow", "on"]).unwrap();
        match cli.command {
            Command::Shadow {
                action: ShadowAction::On,
            } => {}
            _ => panic!("expected Shadow On"),
        }
    }

    #[test]
    fn test_parse_shadow_off() {
        let cli = Cli::try_parse_from(["nexus", "shadow", "off"]).unwrap();
        match cli.command {
            Command::Shadow {
                action: ShadowAction::Off,
            } => {}
            _ => panic!("expected Shadow Off"),
        }
    }

    #[test]
    fn test_parse_shadow_status() {
        let cli = Cli::try_parse_from(["nexus", "shadow", "status"]).unwrap();
        match cli.command {
            Command::Shadow {
                action: ShadowAction::Status,
            } => {}
            _ => panic!("expected Shadow Status"),
        }
    }

    #[test]
    fn test_parse_shadow_no_subcommand_fails() {
        let result = Cli::try_parse_from(["nexus", "shadow"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_shadow_invalid_subcommand_fails() {
        let result = Cli::try_parse_from(["nexus", "shadow", "toggle"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_git_verify() {
        let cli = Cli::try_parse_from(["nexus", "git", "verify"]).unwrap();
        match cli.command {
            Command::Git {
                action: GitAction::Verify,
            } => {}
            _ => panic!("expected Git Verify"),
        }
    }

    #[test]
    fn test_parse_git_apply() {
        let cli = Cli::try_parse_from(["nexus", "git", "apply"]).unwrap();
        match cli.command {
            Command::Git {
                action: GitAction::Apply,
            } => {}
            _ => panic!("expected Git Apply"),
        }
    }

    #[test]
    fn test_parse_git_no_subcommand_fails() {
        let result = Cli::try_parse_from(["nexus", "git"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_pull_with_actor_assets() {
        let cli = Cli::try_parse_from(["nexus", "pull", "--with-actor-assets"]).unwrap();
        match cli.command {
            Command::Pull {
                with_actor_assets, ..
            } => assert!(with_actor_assets),
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_pull_default_no_actor_assets() {
        let cli = Cli::try_parse_from(["nexus", "pull"]).unwrap();
        match cli.command {
            Command::Pull {
                with_actor_assets, ..
            } => assert!(!with_actor_assets),
            _ => panic!("expected Pull command"),
        }
    }

    #[test]
    fn test_parse_actors_list() {
        let cli = Cli::try_parse_from(["nexus", "actors", "list"]).unwrap();
        match cli.command {
            Command::Actors {
                action: ActorsAction::List { ref project_id },
            } => {
                assert!(project_id.is_none());
            }
            _ => panic!("expected Actors List command"),
        }
    }

    #[test]
    fn test_parse_actors_show() {
        let cli = Cli::try_parse_from(["nexus", "actors", "show", "my-agent"]).unwrap();
        match cli.command {
            Command::Actors {
                action: ActorsAction::Show { ref slug, .. },
            } => {
                assert_eq!(slug, "my-agent");
            }
            _ => panic!("expected Actors Show command"),
        }
    }

    #[test]
    fn test_parse_actors_avatar_generate() {
        let cli =
            Cli::try_parse_from(["nexus", "actors", "avatar", "generate", "my-agent"]).unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Avatar {
                        action: ActorAvatarAction::Generate { ref slug, .. },
                    },
            } => {
                assert_eq!(slug, "my-agent");
            }
            _ => panic!("expected Actors Avatar Generate command"),
        }
    }

    #[test]
    fn test_parse_actors_avatar_reset() {
        let cli = Cli::try_parse_from(["nexus", "actors", "avatar", "reset", "my-agent"]).unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Avatar {
                        action: ActorAvatarAction::Reset { ref slug, .. },
                    },
            } => {
                assert_eq!(slug, "my-agent");
            }
            _ => panic!("expected Actors Avatar Reset command"),
        }
    }

    #[test]
    fn test_parse_actors_no_subcommand_fails() {
        let result = Cli::try_parse_from(["nexus", "actors"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_actors_normalize() {
        let cli = Cli::try_parse_from(["nexus", "actors", "normalize", "/tmp/actor.md"]).unwrap();
        match cli.command {
            Command::Actors {
                action: ActorsAction::Normalize { ref path },
            } => {
                assert_eq!(path, "/tmp/actor.md");
            }
            _ => panic!("expected Actors Normalize command"),
        }
    }

    #[test]
    fn test_parse_actors_validate() {
        let cli = Cli::try_parse_from(["nexus", "actors", "validate", "/tmp/actor.md"]).unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Validate {
                        ref path,
                        ref project_id,
                    },
            } => {
                assert_eq!(path, "/tmp/actor.md");
                assert!(project_id.is_none());
            }
            _ => panic!("expected Actors Validate command"),
        }
    }

    #[test]
    fn test_parse_actors_validate_with_project_id() {
        let cli = Cli::try_parse_from([
            "nexus",
            "actors",
            "validate",
            "/tmp/actor.md",
            "--project-id",
            "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        ])
        .unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Validate {
                        ref path,
                        ref project_id,
                    },
            } => {
                assert_eq!(path, "/tmp/actor.md");
                assert_eq!(
                    project_id.as_deref(),
                    Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")
                );
            }
            _ => panic!("expected Actors Validate command"),
        }
    }

    #[test]
    fn test_parse_actors_import() {
        let cli = Cli::try_parse_from(["nexus", "actors", "import", "./actors/"]).unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Import {
                        ref path,
                        ref project_id,
                    },
            } => {
                assert_eq!(path, "./actors/");
                assert!(project_id.is_none());
            }
            _ => panic!("expected Actors Import command"),
        }
    }

    #[test]
    fn test_parse_actors_export_default_target() {
        let cli = Cli::try_parse_from(["nexus", "actors", "export"]).unwrap();
        match cli.command {
            Command::Actors {
                action:
                    ActorsAction::Export {
                        ref target,
                        ref project_id,
                    },
            } => {
                assert_eq!(target, "opencode");
                assert!(project_id.is_none());
            }
            _ => panic!("expected Actors Export command"),
        }
    }

    #[test]
    fn test_parse_actors_export_custom_target() {
        let cli =
            Cli::try_parse_from(["nexus", "actors", "export", "--target", "opencode"]).unwrap();
        match cli.command {
            Command::Actors {
                action: ActorsAction::Export { ref target, .. },
            } => {
                assert_eq!(target, "opencode");
            }
            _ => panic!("expected Actors Export command"),
        }
    }

    #[test]
    fn test_parse_pull_skip_actor_assets() {
        let cli = Cli::try_parse_from(["nexus", "pull", "--skip-actor-assets"]).unwrap();
        match cli.command {
            Command::Pull {
                skip_actor_assets,
                with_actor_assets,
                ..
            } => {
                assert!(skip_actor_assets);
                assert!(!with_actor_assets);
            }
            _ => panic!("expected Pull command"),
        }
    }
}
