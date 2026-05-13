//! Command dispatcher and implementations.

mod auth;
mod config_cmd;
mod deinit;
pub(crate) mod git;
pub(crate) mod import;
mod init;
mod link;
mod preflight;
pub(crate) mod pull;
pub(crate) mod shadow;
mod skills_cmd;
pub(crate) mod sync;
mod upgrade;

use crate::{
    Cli, Command, ConfigAction, GitAction, ShadowAction, SkillsAction, SyncAction, WorkspaceAction,
    WorkspaceShadowAction,
};

/// Dispatch the parsed CLI command to the appropriate handler.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init {
            ref path,
            ref name,
            ref project_id,
            force,
            ..
        } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            init::run(
                path,
                name.as_deref(),
                project_id.as_deref(),
                &api_url,
                force || cli.yes,
                config.mcp_source,
            )
            .await?;
        }
        Command::Link { ref project_id } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            link::link(&api_url, project_id.as_deref()).await?;
        }
        Command::Unlink => {
            link::unlink()?;
        }
        Command::Deinit { force } => {
            deinit::run(force || cli.yes)?;
        }
        Command::Login => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            auth::login(&api_url).await?;
        }
        Command::Logout => {
            auth::logout()?;
        }
        Command::Status => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            auth::status(&api_url).await?;
        }
        Command::Pull {
            ref project_id,
            force,
            ref scope,
        } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            pull::run(
                &api_url,
                project_id.as_deref(),
                force || cli.yes,
                config.mcp_source,
                scope,
            )
            .await?;
        }
        Command::Skills { ref action } => match action {
            SkillsAction::List { ref status, limit } => {
                let config = nexus_core::config::Config::load()?;
                let api_url = cli.resolve_api_url(&config);
                let output = cli.resolve_output(&config);
                skills_cmd::list(&api_url, status.as_deref(), *limit, output).await?;
            }
            SkillsAction::Export { ref project_id } => {
                let config = nexus_core::config::Config::load()?;
                let api_url = cli.resolve_api_url(&config);
                skills_cmd::export(&api_url, project_id.as_deref()).await?;
            }
        },
        Command::Preflight => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            preflight::run(&api_url).await?;
        }
        Command::Config { action } => match action {
            ConfigAction::Show => config_cmd::show()?,
            ConfigAction::Set { pair } => config_cmd::set(&pair)?,
            ConfigAction::Path => config_cmd::path()?,
        },
        Command::Upgrade => {
            upgrade::run()?;
        }
        Command::Shadow { ref action } => match action {
            ShadowAction::On => shadow::on()?,
            ShadowAction::Off => shadow::off()?,
            ShadowAction::Status => shadow::status()?,
        },
        Command::Workspace { ref action } => match action {
            WorkspaceAction::Shadow { ref action } => match action {
                WorkspaceShadowAction::On => shadow::workspace_on()?,
                WorkspaceShadowAction::Off => shadow::workspace_off()?,
                WorkspaceShadowAction::Status => shadow::status()?,
            },
        },
        Command::Import { dry_run } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            import::run(&api_url, dry_run, cli.yes).await?;
        }
        Command::Sync { ref action } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            match action {
                SyncAction::Status { ref project_id } => {
                    sync::status(&api_url, project_id.as_deref()).await?;
                }
                SyncAction::Push {
                    ref file_key,
                    ref project_id,
                } => {
                    sync::push(&api_url, project_id.as_deref(), file_key).await?;
                }
                SyncAction::Reset {
                    ref file_key,
                    ref project_id,
                } => {
                    sync::reset(&api_url, project_id.as_deref(), file_key).await?;
                }
            }
        }
        Command::Git { ref action } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            // Resolve project ID from .nexus/config.toml
            let workspace = std::env::current_dir()?;
            let project_id = nexus_core::config::resolve_project_id(None, Some(&workspace))?;
            let token = nexus_core::auth::resolve_token().ok_or_else(|| {
                anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
            })?;
            let client = nexus_core::api::NexusClient::new(&api_url, Some(token))?;
            let detail = client.get_project(&project_id).await?;

            match detail.project.git_config {
                Some(ref cfg) => match action {
                    GitAction::Verify => git::run_verify(&workspace, cfg),
                    GitAction::Apply => git::run_apply(&workspace, cfg),
                },
                None => {
                    println!(
                        "No git_config set for this project. Configure it in the Nexus dashboard."
                    );
                }
            }
        }
    }
    Ok(())
}
