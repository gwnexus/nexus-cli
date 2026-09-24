// nexusctl/src/cmd/git.rs
//
// `nexus git verify` — compare local git config with project git_config
// `nexus git apply`  — set local git config from project git_config

use anyhow::{Context, Result};
use console::style;
use std::path::Path;
use std::process::Command;

use nexus_core::api::{GhEffective, GitConfig};
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

/// Run `nexus git verify` — show local vs expected git identity, plus the
/// effective `gh` CLI profile (NEXUS-APP ADR-0116) if one is configured.
pub fn run_verify(dir: &Path, cfg: Option<&GitConfig>, gh: Option<&GhEffective>) {
    println!("{}", style("Git Identity Verification").bold());
    println!();

    let mut all_ok = true;

    if let Some(cfg) = cfg {
        let checks: [(&str, &Option<String>); 3] = [
            ("user.name", &cfg.user_name),
            ("user.email", &cfg.user_email),
            ("user.signingkey", &cfg.signing_key),
        ];

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
    }

    // Effective per-project gh CLI profile (NEXUS-APP ADR-0116).
    if let Some(gh) = gh {
        match config::Config::dir() {
            Ok(config_dir) => {
                let profile_dir = gh_profile_dir(&config_dir, &gh.profile);
                let status = gh_verify_login(&profile_dir, &gh.host, gh.user.as_deref());
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
pub fn run_apply(dir: &Path, cfg: Option<&GitConfig>) {
    let Some(cfg) = cfg else {
        println!("{}", style("No git identity settings to apply.").dim());
        return;
    };
    match apply_git_config(dir, cfg) {
        Ok(0) => println!("{}", style("No git identity settings to apply.").dim()),
        Ok(n) => println!(
            "{}",
            style(format!("Applied {} git config setting(s).", n)).green()
        ),
        Err(e) => eprintln!("{} {}", style("Failed to apply git config:").red(), e),
    }
    // Nothing to apply for `gh`: login is interactive by design (NEXUS-APP
    // ADR-0116 / dispatch 8776d208). `nexus run`'s pre-launch check and
    // `nexus git verify` above tell the operator the exact one-time
    // command to run, and `nexus run` itself can seed an empty profile
    // (see `resolve_gh_seed_token`/`gh_auth_login_with_token`).
}

// ── gh profile helpers (NEXUS-APP ADR-0116, dispatch 0350aee7 / 8776d208) ───

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
/// by `nexus run`'s pre-launch check and to decide whether an empty
/// profile needs seeding. Only confirms *a* login exists, not which one
/// (see [`gh_verify_login`] for that, used by `nexus git verify`).
pub fn gh_is_authenticated(profile_dir: &Path, host: &str) -> bool {
    Command::new("gh")
        .env("GH_CONFIG_DIR", profile_dir)
        .args(["auth", "status", "--hostname", host])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Outcome of checking a `gh` login against a project's expected identity,
/// used by both `nexus git verify` and the seeding-verification step in
/// `nexus run`.
#[derive(Debug, PartialEq, Eq)]
pub enum GhVerifyStatus {
    /// Logged in and the active login matches the expected user (or no
    /// expected user was configured to compare against).
    Ok { active: String },
    /// Logged in, but as a different user than configured.
    Mismatch { active: String, expected: String },
    /// Not logged in to `host` under this profile at all.
    NotLoggedIn,
    /// The `gh` binary itself is not installed / not on `PATH`.
    GhNotFound,
}

/// Resolve the active `gh` login for `profile_dir`/`host` via
/// `GH_CONFIG_DIR=<profile_dir> gh api user --jq .login --hostname
/// <host>` and compare it against `expected_user` if given.
pub fn gh_verify_login(
    profile_dir: &Path,
    host: &str,
    expected_user: Option<&str>,
) -> GhVerifyStatus {
    let output = Command::new("gh")
        .env("GH_CONFIG_DIR", profile_dir)
        .args(["api", "user", "--jq", ".login", "--hostname", host])
        .output();

    match output {
        Ok(o) => classify_gh_verify_output(o.status.success(), &o.stdout, expected_user),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GhVerifyStatus::GhNotFound,
        Err(_) => GhVerifyStatus::NotLoggedIn,
    }
}

/// Pure core of [`gh_verify_login`]: classify a `gh api user` invocation's
/// result into a [`GhVerifyStatus`], given its exit success and raw stdout,
/// without shelling out. Split out so tests can exercise every branch
/// (ok / mismatch / not logged in) without mocking the `gh` binary or
/// touching `PATH`.
fn classify_gh_verify_output(
    success: bool,
    stdout: &[u8],
    expected_user: Option<&str>,
) -> GhVerifyStatus {
    if !success {
        return GhVerifyStatus::NotLoggedIn;
    }

    let active = String::from_utf8_lossy(stdout).trim().to_string();
    if active.is_empty() {
        return GhVerifyStatus::NotLoggedIn;
    }

    match expected_user {
        Some(expected) if !expected.eq_ignore_ascii_case(&active) => GhVerifyStatus::Mismatch {
            active,
            expected: expected.to_string(),
        },
        _ => GhVerifyStatus::Ok { active },
    }
}

/// Attempt to obtain a GitHub token to seed an empty gh profile
/// (NEXUS-APP ADR-0116), per `GhEffective.source`:
/// - `"keyring"`: only the OS keyring, via the *default* (unmodified)
///   `gh` config -- i.e. without any `GH_CONFIG_DIR` override, since the
///   profile being seeded has no login yet.
/// - `"env:<VAR>"`: only that named environment variable.
/// - `"auto"` (or anything else, defensively): keyring first, then
///   `GH_TOKEN`, then `GITHUB_TOKEN`.
///
/// The token is returned to the caller for one-time use (verification,
/// then `gh auth login --with-token`); it is never logged, printed, or
/// otherwise surfaced by this function.
pub fn resolve_gh_seed_token(source: &str, host: &str, user: Option<&str>) -> Option<String> {
    resolve_gh_seed_token_with(
        source,
        || keyring_token(host, user),
        |var| std::env::var(var).ok().filter(|v| !v.is_empty()),
    )
}

/// Pure core of [`resolve_gh_seed_token`]: the source-selection priority
/// logic, with the keyring lookup and env-var lookup injected as closures
/// so tests can exercise every branch (`auto`/`keyring`/`env:VAR`,
/// fallthrough order) without a real `gh` binary or mutating real process
/// env vars.
fn resolve_gh_seed_token_with(
    source: &str,
    try_keyring: impl Fn() -> Option<String>,
    try_env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    match source {
        "keyring" => try_keyring(),
        s if s.starts_with("env:") => s.strip_prefix("env:").and_then(&try_env),
        _ => try_keyring()
            .or_else(|| try_env("GH_TOKEN"))
            .or_else(|| try_env("GITHUB_TOKEN")),
    }
}

/// Look up a cached token in the OS keyring via `gh auth token`, run
/// against the operator's *default* `gh` config (no `GH_CONFIG_DIR`
/// override).
fn keyring_token(host: &str, user: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token", "--hostname", host]);
    if let Some(u) = user {
        cmd.args(["--user", u]);
    }
    cmd.output().ok().and_then(|o| {
        if o.status.success() {
            let token = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if token.is_empty() {
                None
            } else {
                Some(token)
            }
        } else {
            None
        }
    })
}

/// Verify a candidate seed token by asking `gh` who it belongs to, without
/// touching any profile directory (`GH_TOKEN` is passed only to this one
/// child process's environment, never our own). Returns the resolved
/// login, or `None` if the token is invalid or the call fails. The token
/// itself is never logged.
pub fn verify_gh_seed_token(token: &str, host: &str) -> Option<String> {
    Command::new("gh")
        .env("GH_TOKEN", token)
        .args(["api", "user", "--jq", ".login", "--hostname", host])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let login = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if login.is_empty() {
                    None
                } else {
                    Some(login)
                }
            } else {
                None
            }
        })
}

/// Write `token` into `profile_dir`'s own gh config via `gh auth login
/// --with-token`, scoped to that profile via `GH_CONFIG_DIR`. The token is
/// piped over stdin (never an argument, never logged) and is never
/// visible in the child's argv.
pub fn gh_auth_login_with_token(profile_dir: &Path, host: &str, token: &str) -> Result<bool> {
    use std::io::Write as _;

    let mut child = Command::new("gh")
        .env("GH_CONFIG_DIR", profile_dir)
        .args(["auth", "login", "--with-token", "--hostname", host])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn 'gh auth login'")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(token.as_bytes())
            .context("failed to write token to 'gh auth login' stdin")?;
    }

    let status = child
        .wait()
        .context("failed waiting for 'gh auth login' to exit")?;
    Ok(status.success())
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
        match classify_gh_verify_output(true, b"octocat\n", None) {
            GhVerifyStatus::Ok { active } => assert_eq!(active, "octocat"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_ok_matches_expected_user() {
        match classify_gh_verify_output(true, b"octocat\n", Some("octocat")) {
            GhVerifyStatus::Ok { active } => assert_eq!(active, "octocat"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_ok_case_insensitive_match() {
        match classify_gh_verify_output(true, b"octocat\n", Some("OctoCat")) {
            GhVerifyStatus::Ok { .. } => {}
            other => panic!("expected Ok (case-insensitive match), got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_mismatch() {
        match classify_gh_verify_output(true, b"octocat\n", Some("someone-else")) {
            GhVerifyStatus::Mismatch { active, expected } => {
                assert_eq!(active, "octocat");
                assert_eq!(expected, "someone-else");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_gh_verify_not_logged_in_on_failure() {
        assert_eq!(
            classify_gh_verify_output(false, b"", None),
            GhVerifyStatus::NotLoggedIn
        );
    }

    #[test]
    fn test_classify_gh_verify_not_logged_in_on_empty_stdout() {
        // Success exit code but no login line is treated as not logged in,
        // not a false "Ok" with an empty active user.
        assert_eq!(
            classify_gh_verify_output(true, b"\n", None),
            GhVerifyStatus::NotLoggedIn
        );
    }

    // ── resolve_gh_seed_token_with (pure core of resolve_gh_seed_token) ────
    // NEXUS-APP ADR-0116 test requirement: "seeding per source".

    #[test]
    fn test_resolve_seed_token_keyring_source_uses_only_keyring() {
        let token = resolve_gh_seed_token_with(
            "keyring",
            || Some("keyring-token".to_string()),
            |_| Some("env-token".to_string()),
        );
        assert_eq!(token, Some("keyring-token".to_string()));
    }

    #[test]
    fn test_resolve_seed_token_keyring_source_none_if_keyring_empty() {
        // Must not silently fall back to env vars when source is
        // explicitly "keyring".
        let token =
            resolve_gh_seed_token_with("keyring", || None, |_| Some("env-token".to_string()));
        assert_eq!(token, None);
    }

    #[test]
    fn test_resolve_seed_token_env_source_reads_named_var_only() {
        let token = resolve_gh_seed_token_with(
            "env:MY_CUSTOM_TOKEN_VAR",
            || Some("keyring-token".to_string()),
            |var| {
                assert_eq!(var, "MY_CUSTOM_TOKEN_VAR");
                Some("custom-var-token".to_string())
            },
        );
        assert_eq!(token, Some("custom-var-token".to_string()));
    }

    #[test]
    fn test_resolve_seed_token_env_source_none_if_var_unset() {
        let token = resolve_gh_seed_token_with(
            "env:MISSING_VAR",
            || Some("keyring-token".to_string()),
            |_| None,
        );
        assert_eq!(token, None);
    }

    #[test]
    fn test_resolve_seed_token_auto_prefers_keyring() {
        let token = resolve_gh_seed_token_with(
            "auto",
            || Some("keyring-token".to_string()),
            |_| Some("gh-token".to_string()),
        );
        assert_eq!(token, Some("keyring-token".to_string()));
    }

    #[test]
    fn test_resolve_seed_token_auto_falls_back_to_gh_token() {
        let token = resolve_gh_seed_token_with(
            "auto",
            || None,
            |var| {
                if var == "GH_TOKEN" {
                    Some("gh-token".to_string())
                } else {
                    None
                }
            },
        );
        assert_eq!(token, Some("gh-token".to_string()));
    }

    #[test]
    fn test_resolve_seed_token_auto_falls_back_to_github_token_last() {
        let token = resolve_gh_seed_token_with(
            "auto",
            || None,
            |var| {
                if var == "GITHUB_TOKEN" {
                    Some("github-token".to_string())
                } else {
                    None
                }
            },
        );
        assert_eq!(token, Some("github-token".to_string()));
    }

    #[test]
    fn test_resolve_seed_token_auto_none_when_all_sources_empty() {
        let token = resolve_gh_seed_token_with("auto", || None, |_| None);
        assert_eq!(token, None);
    }

    #[test]
    fn test_resolve_seed_token_unknown_source_defaults_to_auto_behavior() {
        // Defensive default: an unrecognized source string still tries
        // keyring/GH_TOKEN/GITHUB_TOKEN rather than silently resolving no
        // token at all.
        let token = resolve_gh_seed_token_with(
            "some-future-source-value",
            || Some("keyring-token".to_string()),
            |_| None,
        );
        assert_eq!(token, Some("keyring-token".to_string()));
    }
}
