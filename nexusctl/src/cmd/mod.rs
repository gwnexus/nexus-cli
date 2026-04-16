//! Command dispatcher and implementations.

mod auth;
mod config_cmd;
mod deinit;
mod init;
mod link;
mod preflight;
mod pull;
mod skills_cmd;

use crate::{Cli, Command, ConfigAction, SkillsAction};

/// Dispatch the parsed CLI command to the appropriate handler.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init {
            ref path,
            ref name,
            ref project_id,
            force,
            shadowed_ai,
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
                shadowed_ai,
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
        } => {
            let config = nexus_core::config::Config::load()?;
            let api_url = cli.resolve_api_url(&config);
            pull::run(
                &api_url,
                project_id.as_deref(),
                force || cli.yes,
                config.mcp_source,
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
    }
    Ok(())
}
