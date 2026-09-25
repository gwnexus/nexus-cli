//! The `nexus upgrade` command.
//!
//! Downloads and runs the official install script to upgrade the CLI binary
//! to the latest release version.
//!
//! # Security note
//!
//! The install script is taken from the latest GitHub release together with
//! its `install.sh.sha256` asset and verified in-process before it runs; a
//! missing or mismatching checksum aborts the upgrade (NEXUS-APP dispatch
//! 4820e584). Only when the latest release predates these assets does the
//! command fall back to `nexus.gatewarden.eu`, where a checksum is verified
//! if published. The script itself verifies the SHA-256 of the binary
//! tarball it downloads.

use console::style;
use nexus_core::update_check::mark_as_current;
use std::process::Command;

/// Installer script from the latest GitHub release (preferred source).
const RELEASE_INSTALL_URL: &str =
    "https://github.com/gwnexus/nexus-cli/releases/latest/download/install.sh";

/// SHA-256 checksum of [`RELEASE_INSTALL_URL`], published with the release.
const RELEASE_INSTALL_SHA256_URL: &str =
    "https://github.com/gwnexus/nexus-cli/releases/latest/download/install.sh.sha256";

/// CDN URL of the installer script (fallback for releases without the asset).
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
    // Download the install script to a temp file and verify integrity
    let tmp_dir = std::env::temp_dir();
    let script_path = tmp_dir.join("nexus-install.sh");
    let script_path_str = script_path.display().to_string();

    // Prefer the script published with the latest GitHub release, where a
    // checksum is mandatory; fall back to the CDN for older releases.
    let from_release = curl_to_file(RELEASE_INSTALL_URL, &script_path_str);
    let (source, sha_url, checksum_required) = if from_release {
        ("GitHub release", RELEASE_INSTALL_SHA256_URL, true)
    } else if curl_to_file(INSTALL_URL, &script_path_str) {
        ("nexus.gatewarden.eu", INSTALL_SHA256_URL, false)
    } else {
        anyhow::bail!(
            "Failed to download install script from {} or {}. Check your network connection.",
            RELEASE_INSTALL_URL,
            INSTALL_URL
        );
    };
    println!(
        "   {} Fetched install script over HTTPS from {}",
        style("i").bold().blue(),
        style(source).dim()
    );

    let script = std::fs::read(&script_path)?;
    match fetch_text(sha_url).map(|sha| checksum_matches(&script, &sha)) {
        Some(Some(true)) => {
            println!(
                "   {} Checksum verified (SHA-256)",
                style("✓").bold().green()
            );
        }
        Some(Some(false)) => {
            println!(
                "   {} Checksum mismatch — aborting upgrade for safety",
                style("✗").bold().red()
            );
            let _ = std::fs::remove_file(&script_path);
            anyhow::bail!(
                "Install script integrity check failed. The file may have been tampered with."
            );
        }
        _ if checksum_required => {
            let _ = std::fs::remove_file(&script_path);
            anyhow::bail!(
                "No valid checksum published for the install script ({}); aborting upgrade.",
                sha_url
            );
        }
        _ => {
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

/// Download `url` to `path` with curl; `true` on success.
fn curl_to_file(url: &str, path: &str) -> bool {
    Command::new("curl")
        .args(["-fsSL", "-o", path, url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Fetch a small text resource with curl; `None` if unavailable.
fn fetch_text(url: &str) -> Option<String> {
    let output = Command::new("curl").args(["-fsSL", url]).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compare `script` against a `sha256sum`-style checksum file (`<hex>  name`
/// or just `<hex>`). `None` if the checksum file is malformed.
fn checksum_matches(script: &[u8], checksum_file: &str) -> Option<bool> {
    let expected = checksum_file.split_whitespace().next()?.to_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(nexus_core::hash::sha256_hex_bytes(script) == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checksum_matches() {
        let hash = nexus_core::hash::sha256_hex("echo hi\n");
        assert_eq!(
            checksum_matches(b"echo hi\n", &format!("{hash}  install.sh\n")),
            Some(true)
        );
        assert_eq!(
            checksum_matches(b"echo hi\n", &hash.to_uppercase()),
            Some(true)
        );
        assert_eq!(checksum_matches(b"tampered\n", &hash), Some(false));
        assert_eq!(checksum_matches(b"x", "not-a-hash"), None);
        assert_eq!(checksum_matches(b"x", ""), None);
    }
}
