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

/// CDN URL of the installer script SHA-256 checksum.
const INSTALL_SHA256_URL: &str = "https://nexus.gatewarden.eu/install.sh.sha256";

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

    // Download the install script to a temp file and verify integrity
    let tmp_dir = std::env::temp_dir();
    let script_path = tmp_dir.join("nexus-install.sh");
    let script_path_str = script_path.display().to_string();

    // Fetch the script
    let dl_status = Command::new("curl")
        .args(["-fsSL", "-o", &script_path_str, INSTALL_URL])
        .status()?;

    if !dl_status.success() {
        anyhow::bail!(
            "Failed to download install script from {}. Check your network connection.",
            INSTALL_URL
        );
    }

    // Attempt checksum verification (non-blocking if checksum file unavailable)
    let checksum_verified = verify_script_checksum(&script_path_str);
    match checksum_verified {
        Ok(true) => {
            println!(
                "   {} Checksum verified (SHA-256)",
                style("✓").bold().green()
            );
        }
        Ok(false) => {
            println!(
                "   {} Checksum mismatch — aborting upgrade for safety",
                style("✗").bold().red()
            );
            let _ = std::fs::remove_file(&script_path);
            anyhow::bail!(
                "Install script integrity check failed. The file may have been tampered with."
            );
        }
        Err(_) => {
            println!(
                "   {} Checksum file unavailable — proceeding without verification",
                style("!").bold().yellow()
            );
            println!(
                "     {}",
                style("For stronger supply-chain guarantees, download from GitHub Releases").dim()
            );
        }
    }
    println!();

    // Execute the verified script
    let status = Command::new("bash").arg(&script_path_str).status()?;

    // Clean up
    let _ = std::fs::remove_file(&script_path);

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

/// Verify the install script checksum against the `.sha256` sidecar file.
///
/// Returns:
/// - `Ok(true)` if checksum matches
/// - `Ok(false)` if checksum does NOT match (tampered)
/// - `Err(_)` if the checksum file could not be fetched (unavailable)
fn verify_script_checksum(script_path: &str) -> anyhow::Result<bool> {
    // Fetch expected checksum
    let output = Command::new("curl")
        .args(["-fsSL", INSTALL_SHA256_URL])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("checksum file unavailable");
    }

    let expected = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if expected.is_empty() || expected.len() != 64 {
        anyhow::bail!("invalid checksum format");
    }

    // Compute local SHA-256
    let local_output = Command::new("shasum")
        .args(["-a", "256", script_path])
        .output()?;

    if !local_output.status.success() {
        anyhow::bail!("shasum failed");
    }

    let local_hash = String::from_utf8_lossy(&local_output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    Ok(local_hash == expected)
}
