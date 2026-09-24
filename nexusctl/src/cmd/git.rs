// nexusctl/src/cmd/git.rs
//
// `nexus git verify` — compare local git config with project git_config
// `nexus git apply`  — set local git config from project git_config

use anyhow::{Context, Result};
use console::style;
use std::path::Path;
use std::process::Command;

use nexus_core::api::{GhConfig, GitConfig};
use nexus_core::config;

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

    // Per-project gh CLI profile (NEXUS-APP dispatch 8776d208).
    if let Some(ref gh) = cfg.gh {
        match config::Config::dir() {
            Ok(config_dir) => {
                let profile_dir = gh_profile_dir(&config_dir, &gh.profile);
                let status = gh_verify_login(&profile_dir, gh);
                let (icon, detail) = match &status {
                    GhVerifyStatus::Ok { active } => {
                        (style("OK").green().to_string(), format!("active={active}"))
                    }
                    GhVerifyStatus::Mismatch { active, expected } => {
                        all_ok = false;
                        (
                            style("MISMATCH").red().to_string(),
                            format!("active={active} expected={expected}"),
                        )
                    }
                    GhVerifyStatus::NotLoggedIn => {
                        all_ok = false;
                        (
                            style("NOT LOGGED IN").red().to_string(),
                            format!(
                                "run: GH_CONFIG_DIR={} gh auth login --hostname {}",
                                profile_dir.display(),
                                gh.host
                            ),
                        )
                    }
                    GhVerifyStatus::GhNotFound => {
                        all_ok = false;
                        (
                            style("NOT FOUND").red().to_string(),
                            "gh binary not found -- install from https://cli.github.com".into(),
                        )
                    }
                };
                println!(
                    "  {:<20} profile={:<20} host={:<15} [{}] {}",
                    "gh", gh.profile, gh.host, icon, detail
                );
            }
            Err(e) => {
                all_ok = false;
                println!(
                    "  {:<20} {}",
                    "gh",
                    style(format!("could not resolve gh profile directory: {e}")).red()
                );
            }
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
    // Nothing to apply for `gh`: login is interactive by design (NEXUS-APP
    // dispatch 8776d208). `nexus run`'s pre-launch check and `nexus git
    // verify` above tell the operator the exact one-time command to run.
}

// ── gh profile helpers (NEXUS-APP dispatch 8776d208) ────────────────────────

/// Directory holding the local `gh` CLI profile for `profile`:
/// `<config_dir>/gh-profiles/<profile>`. The GitHub token itself lives only
/// inside this directory (managed by `gh` itself); Nexus never sees it.
pub fn gh_profile_dir(config_dir: &Path, profile: &str) -> std::path::PathBuf {
    config_dir.join("gh-profiles").join(profile)
}

/// Ensure the gh profile directory exists with owner-only permissions
/// (`0700` on Unix, since `gh` stores an OAuth token under this directory
/// once the operator logs in).
pub fn ensure_gh_profile_dir(config_dir: &Path, profile: &str) -> Result<std::path::PathBuf> {
    let dir = gh_profile_dir(config_dir, profile);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create gh profile directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not set permissions on {}", dir.display()))?;
    }
    Ok(dir)
}

/// Whether `gh` is authenticated for `host` inside `profile_dir`.
///
/// Checked via `GH_CONFIG_DIR=<profile_dir> gh auth status --hostname
/// <host>` — lightweight (no network call needed for a cached login), used
/// by `nexus run`'s pre-launch check. Only confirms *a* login exists, not
/// which one (see [`gh_verify_login`] for that, used by `nexus git
/// verify`). Never blocks `nexus run`; the caller only warns.
pub fn gh_is_authenticated(profile_dir: &Path, host: &str) -> bool {
    Command::new("gh")
        .env("GH_CONFIG_DIR", profile_dir)
        .args(["auth", "status", "--hostname", host])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Outcome of checking the local `gh` CLI login against a project's
/// expected `git_config.gh` identity, used by `nexus git verify`.
#[derive(Debug, PartialEq, Eq)]
pub enum GhVerifyStatus {
    /// Logged in and the active login matches the expected user (or no
    /// `user` was configured to compare against).
    Ok { active: String },
    /// Logged in, but as a different user than configured.
    Mismatch { active: String, expected: String },
    /// Not logged in to `host` under this profile at all.
    NotLoggedIn,
    /// The `gh` binary itself is not installed / not on `PATH`.
    GhNotFound,
}

/// Resolve the active `gh` login for `profile_dir`/`gh.host` via
/// `GH_CONFIG_DIR=<profile_dir> gh api user --jq .login --hostname
/// <host>` and compare it against `gh.user` if configured.
pub fn gh_verify_login(profile_dir: &Path, gh: &GhConfig) -> GhVerifyStatus {
    let output = Command::new("gh")
        .env("GH_CONFIG_DIR", profile_dir)
        .args(["api", "user", "--jq", ".login", "--hostname", &gh.host])
        .output();

    match output {
        Ok(o) => classify_gh_verify_output(o.status.success(), &o.stdout, gh),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GhVerifyStatus::GhNotFound,
        Err(_) => GhVerifyStatus::NotLoggedIn,
    }
}

/// Pure core of [`gh_verify_login`]: classify a `gh api user` invocation's
/// result into a [`GhVerifyStatus`], given its exit success and raw stdout,
/// without shelling out. Split out so tests can exercise every branch
/// (ok / mismatch / not logged in) without mocking the `gh` binary or
/// touching `PATH` (NEXUS-APP dispatch 8776d208 test requirements).
fn classify_gh_verify_output(success: bool, stdout: &[u8], gh: &GhConfig) -> GhVerifyStatus {
    if !success {
        return GhVerifyStatus::NotLoggedIn;
    }

    let active = String::from_utf8_lossy(stdout).trim().to_string();
    if active.is_empty() {
        return GhVerifyStatus::NotLoggedIn;
    }

    match &gh.user {
        Some(expected) if !expected.eq_ignore_ascii_case(&active) => GhVerifyStatus::Mismatch {
            active,
            expected: expected.clone(),
        },
        _ => GhVerifyStatus::Ok { active },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(suffix: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-git-test-{}-{}", std::process::id(), suffix));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_gh(user: Option<&str>) -> GhConfig {
        GhConfig {
            host: "github.com".to_string(),
            user: user.map(str::to_string),
            profile: "octocat".to_string(),
        }
    }

    #[test]
    fn test_gh_profile_dir_is_scoped_under_config_dir() {
        let config_dir = Path::new("/home/user/.config/nexus");
        assert_eq!(
            gh_profile_dir(config_dir, "octocat"),
            Path::new("/home/user/.config/nexus/gh-profiles/octocat")
        );
    }

    #[test]
    fn test_ensure_gh_profile_dir_creates_with_owner_only_permissions() {
        let base = tmp_dir("ensure-profile");
        let dir = ensure_gh_profile_dir(&base, "octocat").unwrap();
        assert!(dir.exists());
        assert_eq!(dir, base.join("gh-profiles").join("octocat"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "expected owner-only permissions, got {mode:o}");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_ensure_gh_profile_dir_is_idempotent() {
        let base = tmp_dir("ensure-profile-idempotent");
        let dir1 = ensure_gh_profile_dir(&base, "octocat").unwrap();
        let dir2 = ensure_gh_profile_dir(&base, "octocat").unwrap();
        assert_eq!(dir1, dir2);
        assert!(dir2.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── classify_gh_verify_output (pure core of gh_verify_login) ───────────

    #[test]
    fn test_classify_gh_verify_ok_no_expected_user() {
        let gh = sample_gh(None);
        match classify_gh_verify_output(true, b"octocat\n", &gh) {
            GhVerifyStatus::Ok { active } => assert_eq!(active, "octocat"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_ok_matches_expected_user() {
        let gh = sample_gh(Some("octocat"));
        match classify_gh_verify_output(true, b"octocat\n", &gh) {
            GhVerifyStatus::Ok { active } => assert_eq!(active, "octocat"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_ok_case_insensitive_match() {
        let gh = sample_gh(Some("OctoCat"));
        match classify_gh_verify_output(true, b"octocat\n", &gh) {
            GhVerifyStatus::Ok { .. } => {}
            other => panic!("expected Ok (case-insensitive match), got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_mismatch() {
        let gh = sample_gh(Some("someone-else"));
        match classify_gh_verify_output(true, b"octocat\n", &gh) {
            GhVerifyStatus::Mismatch { active, expected } => {
                assert_eq!(active, "octocat");
                assert_eq!(expected, "someone-else");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_not_logged_in_on_failure() {
        let gh = sample_gh(None);
        assert_eq!(
            classify_gh_verify_output(false, b"", &gh),
            GhVerifyStatus::NotLoggedIn
        );
    }

    #[test]
    fn test_classify_gh_verify_not_logged_in_on_empty_stdout() {
        // Success exit code but no login line is treated as not logged in,
        // not a false "Ok" with an empty active user.
        let gh = sample_gh(None);
        assert_eq!(
            classify_gh_verify_output(true, b"\n", &gh),
            GhVerifyStatus::NotLoggedIn
        );
    }
}
