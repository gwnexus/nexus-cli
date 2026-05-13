// nexusctl/src/cmd/git.rs
//
// `nexus git verify` — compare local git config with project git_config
// `nexus git apply`  — set local git config from project git_config

use anyhow::{Context, Result};
use console::style;
use std::path::Path;
use std::process::Command;

use nexus_core::api::GitConfig;

/// Apply git config from the platform to the local repository.
pub fn apply_git_config(dir: &Path, cfg: &GitConfig) -> Result<u32> {
    let mut applied = 0u32;

    if let Some(ref name) = cfg.user_name {
        run_git_config(dir, "user.name", name)?;
        applied += 1;
    }
    if let Some(ref email) = cfg.user_email {
        run_git_config(dir, "user.email", email)?;
        applied += 1;
    }
    if let Some(ref key) = cfg.signing_key {
        run_git_config(dir, "user.signingkey", key)?;
        applied += 1;
    }
    if let Some(sign) = cfg.commit_gpgsign {
        run_git_config(dir, "commit.gpgsign", if sign { "true" } else { "false" })?;
        applied += 1;
    }

    Ok(applied)
}

/// Run `nexus git verify` — show local vs expected git identity.
pub fn run_verify(dir: &Path, cfg: &GitConfig) {
    println!("{}", style("Git Identity Verification").bold());
    println!();

    let checks: [(&str, &Option<String>); 3] = [
        ("user.name", &cfg.user_name),
        ("user.email", &cfg.user_email),
        ("user.signingkey", &cfg.signing_key),
    ];

    let mut all_ok = true;

    for (key, expected) in &checks {
        if let Some(exp) = expected {
            let local = get_git_config(dir, key).unwrap_or_default();
            let ok = local.trim() == exp.trim();
            let icon = if ok {
                style("OK").green().to_string()
            } else {
                style("MISMATCH").red().to_string()
            };
            let local_display = if local.is_empty() {
                style("(not set)").dim().to_string()
            } else {
                local
            };
            println!(
                "  {:<20} local={:<30} expected={:<30} [{}]",
                key, local_display, exp, icon
            );
            if !ok {
                all_ok = false;
            }
        }
    }

    // Handle bool separately
    if let Some(sign) = cfg.commit_gpgsign {
        let key = "commit.gpgsign";
        let local = get_git_config(dir, key).unwrap_or_default();
        let exp = if sign { "true" } else { "false" };
        let ok = local.trim() == exp;
        let icon = if ok {
            style("OK").green().to_string()
        } else {
            style("MISMATCH").red().to_string()
        };
        let local_display = if local.is_empty() {
            style("(not set)").dim().to_string()
        } else {
            local
        };
        println!(
            "  {:<20} local={:<30} expected={:<30} [{}]",
            key, local_display, exp, icon
        );
        if !ok {
            all_ok = false;
        }
    }

    println!();
    if all_ok {
        println!("{}", style("All git identity settings match.").green());
    } else {
        println!(
            "{}",
            style("Run `nexus git apply` to fix mismatches.").yellow()
        );
    }
}

/// Run `nexus git apply` — set local git config from platform.
pub fn run_apply(dir: &Path, cfg: &GitConfig) {
    match apply_git_config(dir, cfg) {
        Ok(0) => println!("{}", style("No git identity settings to apply.").dim()),
        Ok(n) => println!(
            "{}",
            style(format!("Applied {} git config setting(s).", n)).green()
        ),
        Err(e) => eprintln!("{} {}", style("Failed to apply git config:").red(), e),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn run_git_config(dir: &Path, key: &str, value: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--local", key, value])
        .current_dir(dir)
        .status()
        .with_context(|| format!("Failed to run git config --local {} {}", key, value))?;

    if !status.success() {
        anyhow::bail!("git config --local {} {} failed", key, value);
    }
    Ok(())
}

fn get_git_config(dir: &Path, key: &str) -> Option<String> {
    Command::new("git")
        .args(["config", "--local", key])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}
