//! Configuration management commands: show, set, path.

use console::style;
use nexus_core::config::Config;

/// Display the current effective configuration.
pub fn show() -> anyhow::Result<()> {
    let config = Config::load()?;
    let path = Config::path()?;

    println!(
        "{} Configuration ({})",
        style(">>").bold().cyan(),
        path.display()
    );
    println!();
    println!("  api_url        = {}", config.api_url);
    println!("  default_output = {}", config.default_output);
    println!("  no_color       = {}", config.no_color);

    Ok(())
}

/// Set a configuration value from a KEY=VALUE pair.
pub fn set(pair: &str) -> anyhow::Result<()> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected KEY=VALUE format, got: '{}'", pair))?;

    let mut config = Config::load()?;
    config.set(key.trim(), value.trim())?;
    config.save()?;

    println!(
        "{} Set {} = {}",
        style("OK").bold().green(),
        key.trim(),
        value.trim()
    );

    Ok(())
}

/// Display the configuration file path.
pub fn path() -> anyhow::Result<()> {
    let path = Config::path()?;
    println!("{}", path.display());
    Ok(())
}
