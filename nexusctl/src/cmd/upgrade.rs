//! The `nexus upgrade` command.
//!
//! Downloads and runs the official install script to upgrade the CLI binary
//! to the latest release version.

use console::style;
use std::process::Command;

/// CDN URL of the installer script.
const INSTALL_URL: &str = "https://d1187p3nik605m.cloudfront.net/cli/install.sh";

/// Run the upgrade command.
pub fn run() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "{} Upgrading Nexus CLI (current: v{})",
        style(">>").bold().cyan(),
        current
    );
    println!();

    // Execute: curl -fsSL <URL> | bash
    let status = Command::new("bash")
        .arg("-c")
        .arg(format!("curl -fsSL {} | bash", INSTALL_URL))
        .status()?;

    println!();
    if status.success() {
        println!(
            "{} Upgrade complete. Run {} to verify.",
            style("OK").bold().green(),
            style("nexus --version").bold()
        );
    } else {
        println!(
            "{} Upgrade failed (exit code: {}).",
            style("ERR").bold().red(),
            status.code().unwrap_or(-1)
        );
        println!("   You can try manually: curl -fsSL {} | bash", INSTALL_URL);
    }

    Ok(())
}
