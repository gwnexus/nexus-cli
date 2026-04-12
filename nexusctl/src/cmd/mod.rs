//! Command dispatcher and implementations.

mod auth;
mod config_cmd;
mod init;

use crate::{Cli, Command, ConfigAction};

/// Dispatch the parsed CLI command to the appropriate handler.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init { path, name, force } => {
            init::run(&path, name.as_deref(), force).await?;
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
        Command::Config { action } => match action {
            ConfigAction::Show => config_cmd::show()?,
            ConfigAction::Set { pair } => config_cmd::set(&pair)?,
            ConfigAction::Path => config_cmd::path()?,
        },
    }
    Ok(())
}
