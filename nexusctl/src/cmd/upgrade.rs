//! The `nexus upgrade` command.
//!
//! Downloads and runs the official install script to upgrade the CLI binary
//! to the latest release version.
//!
//! # Security note
//!
//! This command executes `curl -fsSL <URL> | bash` which downloads and
//! immediately runs a shell script from `nexus.gatewarden.eu` over HTTPS.
//! The script is fetched without an additional checksum or signature
//! verification step. The connection is encrypted (TLS) and the domain is
//! pinned at compile time. If you require stronger supply-chain guarantees,
//! download the binary directly from GitHub Releases and verify the SHA-256
//! checksum published alongside each release.

use console::style;
use nexus_core::update_check::mark_as_current;
use std::process::Command;

/// CDN URL of the installer script.
const INSTALL_URL: &str = "https://nexus.gatewarden.eu/install.sh";

/// Run the upgrade command.
pub fn run() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "{} Upgrading Nexus CLI (current: v{})",
        style(">>").bold().cyan(),
        current
    );
    println!(
        "   {} Fetching install script over HTTPS from {}",
        style("i").bold().blue(),
        style("nexus.gatewarden.eu").dim()
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

        // Detect the newly installed version by running the upgraded binary.
        // Suppress the update-check banner for the current process and the
        // next 24 h cache window by stamping the cache with the new version.
        let installed_version = detect_installed_version().unwrap_or_else(|| current.to_string());
        mark_as_current(&installed_version);
    } else {
        println!(
            "{} Upgrade failed (exit code: {}).",
            style("ERR").bold().red(),
            status.code().unwrap_or(-1)
        );
        println!("   You can try manually: curl -fsSL {} | bash", INSTALL_URL);
        println!(
            "   Or download a binary directly from: {}",
            style("https://github.com/gwnexus/nexus-cli/releases").dim()
        );
    }

    Ok(())
}

/// Try to determine the version of the newly installed binary by running
/// `nexus --version` and parsing the output (e.g. "nexus 0.6.13").
/// Returns `None` if the binary cannot be found or the output cannot be parsed.
fn detect_installed_version() -> Option<String> {
    // Resolve the binary path: prefer the same executable that is currently
    // running so we pick up the freshly replaced binary in-place.
    let bin = std::env::current_exe().ok()?;
    let output = Command::new(&bin).arg("--version").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: "nexus 0.6.13" or "nexus v0.6.13"
    stdout
        .split_whitespace()
        .last()
        .map(|v| v.trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
}
