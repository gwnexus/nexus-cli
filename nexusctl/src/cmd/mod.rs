//! Command dispatcher and implementations.

mod actors;
mod auth;
pub(crate) mod ccx;
mod claude_cmd;
pub(crate) mod claude_render;
mod config_cmd;
mod deinit;
pub(crate) mod display;
pub(crate) mod git;
pub(crate) mod import;
mod init;
mod link;
pub(crate) mod mcp_local;
pub(crate) mod preflight;
pub(crate) mod project;
pub(crate) mod pull;
pub(crate) mod push;
pub(crate) mod run;
pub(crate) mod shadow;
mod skills_cmd;
pub(crate) mod stash;
pub(crate) mod sync;
mod upgrade;

use crate::{
    ActorAvatarAction, ActorsAction, ClaudeAction, Cli, Command, ConfigAction, GitAction,
    ProjectAction, ShadowAction, SkillsAction, StashAction, SyncAction, WorkspaceAction,
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
            let config = nexus_core::config::Config::load_effective(None)?;
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
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            link::link(&api_url, project_id.as_deref()).await?;
        }
        Command::Unlink => {
            link::unlink()?;
        }
        Command::Project { ref action } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            match action {
                ProjectAction::Link {
                    ref project_id,
                    ref runtime_id,
                    ref restrict_profiles,
                    ref expires,
                    rotate,
                    status,
                } => {
                    if *rotate {
                        project::rotate(&api_url, project_id.as_deref(), false).await?;
                    } else if *status {
                        project::status(&api_url, project_id.as_deref()).await?;
                    } else {
                        project::link(
                            &api_url,
                            project_id.as_deref(),
                            runtime_id.as_deref(),
                            restrict_profiles,
                            expires.as_deref(),
                        )
                        .await?;
                    }
                }
                ProjectAction::Rotate {
                    ref project_id,
                    finalize,
                } => {
                    project::rotate(&api_url, project_id.as_deref(), *finalize).await?;
                }
                ProjectAction::Unlink { ref project_id } => {
                    project::unlink(&api_url, project_id.as_deref()).await?;
                }
                ProjectAction::Status { ref project_id } => {
                    project::status(&api_url, project_id.as_deref()).await?;
                }
            }
        }
        Command::Deinit { force } => {
            deinit::run(force || cli.yes)?;
        }
        Command::Login { global, .. } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            auth::login(&api_url, global).await?;
        }
        Command::Logout { global, .. } => {
            auth::logout(global)?;
        }
        Command::Status => {
            let workspace = std::env::current_dir()?;
            let effective =
                nexus_core::config::Config::load_effective_with_provenance(Some(&workspace))?;
            let api_url = cli.resolve_api_url(&effective.config);
            let api_url_source = cli.resolve_api_url_source(&effective);
            auth::status(&api_url, api_url_source).await?;
        }
        Command::Pull {
            ref project_id,
            force,
            force_unmanaged,
            ref scope,
            with_actor_assets,
            skip_actor_assets,
        } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            // --skip-actor-assets takes precedence over --with-actor-assets
            let effective_with_assets = with_actor_assets && !skip_actor_assets;
            pull::run(
                &api_url,
                project_id.as_deref(),
                force || cli.yes,
                config.mcp_source,
                scope,
                effective_with_assets,
                // Only the explicit flags, never -y: accepting prompts must
                // not discard local edits to CCX files.
                ccx::ForceMode::from_flags(force, force_unmanaged),
            )
            .await?;
        }
        Command::Claude { ref action } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            let code = match action {
                ClaudeAction::Status { ref project_id } => {
                    let json = matches!(
                        cli.resolve_output(&config),
                        nexus_core::OutputPreference::Json
                    );
                    claude_cmd::status(&api_url, project_id.as_deref(), json).await?
                }
                ClaudeAction::Diff { ref project_id } => {
                    claude_cmd::diff(&api_url, project_id.as_deref()).await?
                }
                ClaudeAction::Launch {
                    skip_checks,
                    force,
                    ref account,
                } => {
                    claude_cmd::launch(
                        &api_url,
                        *skip_checks,
                        *force,
                        config.run.default_tool.as_deref(),
                        config.run.launch_countdown_secs,
                        account.as_deref(),
                        cli.yes,
                    )
                    .await?;
                    0
                }
            };
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::Skills { ref action } => match action {
            SkillsAction::List { ref status, limit } => {
                let config = nexus_core::config::Config::load_effective(None)?;
                let api_url = cli.resolve_api_url(&config);
                let output = cli.resolve_output(&config);
                skills_cmd::list(&api_url, status.as_deref(), *limit, output).await?;
            }
            SkillsAction::Export { ref project_id } => {
                let config = nexus_core::config::Config::load_effective(None)?;
                let api_url = cli.resolve_api_url(&config);
                skills_cmd::export(&api_url, project_id.as_deref()).await?;
            }
        },
        Command::Preflight => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            preflight::run(&api_url).await?;
        }
        Command::Config { action } => match action {
            ConfigAction::Show => config_cmd::show()?,
            ConfigAction::Set { pair, local, .. } => config_cmd::set(&pair, local)?,
            ConfigAction::Path { local, .. } => config_cmd::path(local)?,
        },
        Command::Upgrade => {
            upgrade::run()?;
        }
        Command::McpLocal => {
            mcp_local::run().await?;
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
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            import::run(&api_url, dry_run, cli.yes).await?;
        }
        Command::Sync { ref action } => {
            let config = nexus_core::config::Config::load_effective(None)?;
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
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            // Resolve project ID from .nexus/config.toml
            let workspace = std::env::current_dir()?;
            let project_id = nexus_core::config::resolve_project_id(None, Some(&workspace))?;
            let token = nexus_core::auth::resolve_token().ok_or_else(|| {
                anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
            })?;
            let client = nexus_core::api::NexusClient::new(&api_url, Some(token))?;
            let detail = client.get_project(&project_id).await?;

            let git_config = detail.project.git_config.as_ref();
            let gh_effective = detail.project.gh_effective.as_ref();

            if git_config.is_none() && gh_effective.is_none() {
                println!(
                    "No git_config or gh profile set for this project. Configure it in the Nexus dashboard."
                );
            } else {
                match action {
                    GitAction::Verify => git::run_verify(&workspace, git_config, gh_effective),
                    GitAction::Apply => git::run_apply(&workspace, git_config),
                }
            }
        }
        Command::Actors { ref action } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            match action {
                ActorsAction::List { ref project_id } => {
                    actors::list(&api_url, project_id.as_deref()).await?;
                }
                ActorsAction::Show {
                    ref slug,
                    ref project_id,
                } => {
                    actors::show(&api_url, slug, project_id.as_deref()).await?;
                }
                ActorsAction::Normalize { ref path } => {
                    actors::normalize(path)?;
                }
                ActorsAction::Validate {
                    ref path,
                    ref project_id,
                } => {
                    actors::validate(&api_url, path, project_id.as_deref()).await?;
                }
                ActorsAction::Import {
                    ref path,
                    ref project_id,
                } => {
                    actors::import(&api_url, path, project_id.as_deref()).await?;
                }
                ActorsAction::Export {
                    ref target,
                    ref project_id,
                } => {
                    actors::export(&api_url, target, project_id.as_deref()).await?;
                }
                ActorsAction::Avatar { ref action } => match action {
                    ActorAvatarAction::Generate {
                        ref slug,
                        ref project_id,
                    } => {
                        actors::avatar_generate(&api_url, slug, project_id.as_deref()).await?;
                    }
                    ActorAvatarAction::Reset {
                        ref slug,
                        ref project_id,
                    } => {
                        actors::avatar_reset(&api_url, slug, project_id.as_deref()).await?;
                    }
                },
            }
        }
        Command::Push {
            ref project_id,
            ref name,
            dry_run,
            workspace: _,
            adopt_local,
        } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            push::run(
                &api_url,
                project_id.as_deref(),
                name.as_deref(),
                dry_run,
                adopt_local,
            )
            .await?;
        }
        Command::Stash { ref action } => {
            let workspace = std::env::current_dir()?;
            match action.as_ref().unwrap_or(&StashAction::Save) {
                StashAction::Save => stash::save(&workspace)?,
                StashAction::Pop => stash::pop(&workspace)?,
                StashAction::List => stash::list(&workspace)?,
            }
        }
        Command::Run {
            ref tool,
            dry_run,
            show_env,
            no_db,
            exec,
            skip_checks,
            force,
            ref account,
            ref args,
        } => {
            let config = nexus_core::config::Config::load_effective(None)?;
            let api_url = cli.resolve_api_url(&config);
            let default_tool = config.run.default_tool.clone();
            let countdown_secs = config.run.launch_countdown_secs;
            run::run(
                &api_url,
                tool.as_deref(),
                dry_run,
                show_env,
                no_db,
                exec,
                should_skip_prelaunch_checks(skip_checks, force),
                force,
                args,
                default_tool.as_deref(),
                countdown_secs,
                account.as_deref(),
                cli.yes,
            )
            .await?;
        }
    }
    Ok(())
}

/// Whether `nexus run` should skip pre-launch checks (the "Nexus Pre-launch
/// Check" panel, `run_prelaunch_checks`) entirely.
///
/// `--force` only skips the interactive "N checks failed, continue anyway?"
/// confirmation prompt after checks have been shown (documented as
/// "non-interactive/CI mode"); it must NOT imply `--skip-checks`. Some
/// checks (e.g. Billing Auth, NEXUS-APP dispatch 8de19c71) are deliberately
/// not bypassable by `--force` and rely on `run_prelaunch_checks` actually
/// running to enforce that. A prior version of this call site computed
/// `skip_checks || force`, which skipped the whole check function --
/// including the force-proof checks inside it -- whenever `--force` was
/// passed alone, silently defeating the guarantee those checks exist to
/// provide (regression reported as a follow-up to 8de19c71).
fn should_skip_prelaunch_checks(skip_checks: bool, _force: bool) -> bool {
    skip_checks
}

#[cfg(test)]
mod tests {
    use super::should_skip_prelaunch_checks;

    #[test]
    fn test_force_alone_does_not_skip_prelaunch_checks() {
        // The exact regression: --force without --skip-checks must still
        // run run_prelaunch_checks, so force-proof checks (e.g. Billing
        // Auth) get a chance to fire.
        assert!(!should_skip_prelaunch_checks(false, true));
    }

    #[test]
    fn test_skip_checks_flag_skips_prelaunch_checks_regardless_of_force() {
        assert!(should_skip_prelaunch_checks(true, false));
        assert!(should_skip_prelaunch_checks(true, true));
    }

    #[test]
    fn test_neither_flag_runs_prelaunch_checks() {
        assert!(!should_skip_prelaunch_checks(false, false));
    }
}
