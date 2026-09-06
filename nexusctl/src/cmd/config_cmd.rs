//! Configuration management commands: show, set, path.
//!
//! Supports two layers, mirroring git's `--local` / `--global` model:
//! - Global: `~/.config/nexus/config.toml` (machine-wide default)
//! - Local: `.nexus/config.toml` `[config]` section (this project only)
//!
//! Local overrides take precedence over global for `api_url`,
//! `default_output`, and `no_color`. See `Config::load_effective`.

use console::style;
use nexus_core::config::{self, Config, ProjectConfig};

/// Display the effective configuration, with per-key provenance
/// (local / global / default).
pub fn show() -> anyhow::Result<()> {
    let workspace = std::env::current_dir().ok();
    let effective = Config::load_effective_with_provenance(workspace.as_deref())?;

    println!("{} Configuration", style(">>").bold().cyan());
    println!(
        "   global: {}",
        style(effective.global_path.display()).dim()
    );
    match &effective.local_path {
        Some(p) => println!("   local:  {}", style(p.display()).dim()),
        None => println!("   local:  {}", style("(not set for this project)").dim()),
    }
    println!();
    println!(
        "  api_url        = {} ({})",
        effective.config.api_url, effective.api_url_source
    );
    println!(
        "  default_output = {} ({})",
        effective.config.default_output, effective.default_output_source
    );
    println!(
        "  no_color       = {} ({})",
        effective.config.no_color, effective.no_color_source
    );

    Ok(())
}

/// Set a configuration value from a KEY=VALUE pair.
///
/// When `local` is true, writes to the project-local `.nexus/config.toml`
/// `[config]` section instead of the global config file.
pub fn set(pair: &str, local: bool) -> anyhow::Result<()> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected KEY=VALUE format, got: '{}'", pair))?;
    let key = key.trim();
    let value = value.trim();

    if local {
        let workspace = std::env::current_dir()?;
        let mut project_config =
            config::load_project_config(Some(&workspace))?.unwrap_or_else(ProjectConfig::default);
        let mut overrides = project_config.config.take().unwrap_or_default();
        overrides.set(key, value)?;
        project_config.config = Some(overrides);
        config::save_project_config(Some(&workspace), &project_config)?;

        println!(
            "{} Set {} = {} (local)",
            style("OK").bold().green(),
            key,
            value
        );
    } else {
        let mut cfg = Config::load()?;
        cfg.set(key, value)?;
        cfg.save()?;

        println!(
            "{} Set {} = {} (global)",
            style("OK").bold().green(),
            key,
            value
        );
    }

    Ok(())
}

/// Display the configuration file path.
///
/// Without flags, prints the global config path (unchanged behavior).
/// Pass `local: true` for the project-local `.nexus/config.toml` path.
pub fn path(local: bool) -> anyhow::Result<()> {
    if local {
        let workspace = std::env::current_dir()?;
        let path = config::project_config_path(Some(&workspace))?;
        println!("{}", path.display());
    } else {
        let path = Config::path()?;
        println!("{}", path.display());
    }
    Ok(())
}
