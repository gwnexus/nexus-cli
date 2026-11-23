// nexusctl/src/cmd/gh_account.rs
//
// GitHub account per project path: `.nexus/gh.env` for shells (sourced by
// the devbox init hook), the transport identity in the local git config
// (`core.sshCommand`, the HTTPS credential helper), `gh repo set-default`,
// and the commit identity of the account. Used by `nexus pull`,
// `nexus git apply|login|verify`, `nexus status` and `nexus run`.
//
// Tokens never pass through here except in `nexus git login --with-token`,
// where the token goes to `gh` over stdin only and is never printed.

use anyhow::{anyhow, bail, Context, Result};
use console::style;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use nexus_core::api::{GhEffective, GitConfig};

use super::git::{self, GhVerifyStatus};
use super::run::GH_TOKEN_ENV_VARS;

/// Workspace-relative path of the account file. Fixed: the devbox init hook
/// and the server exclude list name exactly this path.
pub const GH_ENV_REL: &str = ".nexus/gh.env";

/// First line of every `gh.env` Nexus writes. The devbox hook only sources
/// a file carrying it, and only such a file is ever replaced or removed.
pub const GH_ENV_HEADER: &str = "# GitHub account for this project path";

/// Local git config keys recording the value Nexus last wrote for a key, so
/// a changed account replaces its own value without asking.
const MANAGED_SSH_KEY: &str = "nexus.sshCommand";
const MANAGED_NAME_KEY: &str = "nexus.userName";
const MANAGED_EMAIL_KEY: &str = "nexus.userEmail";

/// Status line label for `GhEffective.origin`.
pub fn origin_label(origin: &str) -> &'static str {
    match origin.to_ascii_lowercase().as_str() {
        "user" => "user default",
        "binding" => "project binding",
        _ => "project pin",
    }
}

/// The one-time command that logs the account's profile in.
pub fn login_command(gh: &GhEffective) -> String {
    match &gh.user {
        Some(user) => format!("nexus git login --account {user}"),
        None => "nexus git login".to_string(),
    }
}

/// `gh` scoped to `profile_dir`, with ambient token variables removed so
/// they cannot authenticate the call as another account.
fn gh_cmd(profile_dir: &Path, host: &str) -> Command {
    let mut cmd = Command::new("gh");
    cmd.env("GH_CONFIG_DIR", profile_dir);
    if !host.eq_ignore_ascii_case("github.com") {
        cmd.env("GH_HOST", host);
    }
    for var in GH_TOKEN_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd
}

// ── Login state ─────────────────────────────────────────────────────────────

/// Login of the account's profile compared with the expected user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// Logged in as the expected user (or no user is expected).
    Verified(String),
    /// Logged in as another user.
    Mismatch {
        active: String,
        expected: String,
    },
    NotLoggedIn,
    /// `gh auth status` reports a login, but `gh api user` failed (offline).
    Unverified,
    GhNotFound,
}

pub fn check_login(gh: &GhEffective, profile_dir: &Path) -> Login {
    if !profile_dir.is_dir() {
        return Login::NotLoggedIn;
    }
    match git::gh_verify_login(profile_dir, &gh.host, gh.user.as_deref()) {
        GhVerifyStatus::Ok { active } => Login::Verified(active),
        GhVerifyStatus::Mismatch { active, expected } => Login::Mismatch { active, expected },
        GhVerifyStatus::GhNotFound => Login::GhNotFound,
        GhVerifyStatus::NotLoggedIn if git::gh_is_authenticated(profile_dir, &gh.host) => {
            Login::Unverified
        }
        GhVerifyStatus::NotLoggedIn => Login::NotLoggedIn,
    }
}

// ── .nexus/gh.env ───────────────────────────────────────────────────────────

/// A `KEY=VALUE` value: unquoted when it is `[A-Za-z0-9_./@:-]+` (so
/// Makefiles can `include` the file), POSIX single-quoted otherwise. The
/// same quoting is safe inside a shell command line.
fn env_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./@:-".contains(c));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Content of `.nexus/gh.env` for a profile: header, `GH_CONFIG_DIR`, and
/// `GH_HOST` for a host other than github.com. Never a token.
pub fn render_gh_env(profile_dir: &Path, host: &str) -> String {
    let mut out = format!(
        "{GH_ENV_HEADER}\nGH_CONFIG_DIR={}\n",
        env_value(&profile_dir.to_string_lossy())
    );
    if !host.eq_ignore_ascii_case("github.com") {
        out.push_str(&format!("GH_HOST={}\n", env_value(host)));
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum GhEnvAction {
    Write,
    Remove(String),
    /// Leave the file as it is.
    Keep(String),
}

fn gh_env_action(login: &Login, profile_dir: &Path) -> GhEnvAction {
    if !profile_dir.is_absolute() {
        return GhEnvAction::Remove("profile path is not absolute".into());
    }
    match login {
        Login::Verified(_) => GhEnvAction::Write,
        Login::Mismatch { active, expected } => GhEnvAction::Remove(format!(
            "profile is logged in as {active}, expected {expected}"
        )),
        Login::NotLoggedIn => GhEnvAction::Remove("profile not logged in".into()),
        Login::GhNotFound => GhEnvAction::Remove("gh not found".into()),
        Login::Unverified => GhEnvAction::Keep("login could not be verified".into()),
    }
}

/// State of `.nexus/gh.env` on disk compared with what it should contain.
#[derive(Debug, PartialEq, Eq)]
pub enum GhEnvFile {
    Current,
    Stale,
    Missing,
    /// A file without the Nexus header; never touched.
    Foreign,
}

impl GhEnvFile {
    /// Status line wording, for a profile that is logged in.
    pub fn describe(&self) -> &'static str {
        match self {
            GhEnvFile::Current => "gh.env written",
            GhEnvFile::Stale => "gh.env not written (stale, run nexus git apply)",
            GhEnvFile::Missing => "gh.env not written (run nexus git apply)",
            GhEnvFile::Foreign => "gh.env not written (existing file is not managed by Nexus)",
        }
    }
}

fn is_ours(content: &str) -> bool {
    content.lines().next().map(str::trim_end) == Some(GH_ENV_HEADER)
}

pub fn gh_env_file_state(workspace: &Path, profile_dir: &Path, host: &str) -> GhEnvFile {
    match std::fs::read_to_string(workspace.join(GH_ENV_REL)) {
        Err(_) => GhEnvFile::Missing,
        Ok(c) if !is_ours(&c) => GhEnvFile::Foreign,
        Ok(c) if c == render_gh_env(profile_dir, host) => GhEnvFile::Current,
        Ok(_) => GhEnvFile::Stale,
    }
}

/// What [`sync_gh_env`] left on disk, worded for the status line.
#[derive(Debug, PartialEq, Eq)]
pub struct GhEnvReport {
    pub written: bool,
    pub reason: Option<String>,
}

impl GhEnvReport {
    pub fn describe(&self) -> String {
        match (self.written, &self.reason) {
            (true, None) => "gh.env written".into(),
            (true, Some(r)) => format!("gh.env written ({r})"),
            (false, Some(r)) => format!("gh.env not written ({r})"),
            (false, None) => "gh.env not written".into(),
        }
    }
}

fn remove_ours(path: &Path) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|c| is_ours(&c)) {
        std::fs::remove_file(path)
            .with_context(|| format!("could not remove {}", path.display()))?;
    }
    Ok(())
}

/// Write or remove `.nexus/gh.env`. `account` is `None` when the project
/// has no GitHub account: a file written by Nexus is removed then. A file
/// without the Nexus header is never touched.
pub fn sync_gh_env(
    workspace: &Path,
    account: Option<(&Path, &str, &Login)>,
) -> Result<GhEnvReport> {
    let path = workspace.join(GH_ENV_REL);
    let not_written = |reason: &str| GhEnvReport {
        written: false,
        reason: Some(reason.to_string()),
    };
    let Some((profile_dir, host, login)) = account else {
        remove_ours(&path)?;
        return Ok(not_written("no GitHub account for this project"));
    };
    let existing = std::fs::read_to_string(&path).ok();
    if existing.as_deref().is_some_and(|c| !is_ours(c)) {
        return Ok(not_written("existing file is not managed by Nexus"));
    }
    match gh_env_action(login, profile_dir) {
        GhEnvAction::Write => {
            let content = render_gh_env(profile_dir, host);
            if existing.as_deref() != Some(content.as_str()) {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, content)
                    .with_context(|| format!("could not write {}", path.display()))?;
            }
            super::shadow::ensure_git_excluded_in(workspace, &[GH_ENV_REL], "GitHub account file");
            Ok(GhEnvReport {
                written: true,
                reason: None,
            })
        }
        GhEnvAction::Remove(reason) => {
            remove_ours(&path)?;
            Ok(not_written(&reason))
        }
        GhEnvAction::Keep(reason) => Ok(GhEnvReport {
            written: existing.is_some(),
            reason: Some(reason),
        }),
    }
}

// ── Local git config ────────────────────────────────────────────────────────

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run git {}", args.first().unwrap_or(&"")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn local_get(dir: &Path, key: &str) -> Option<String> {
    git_out(dir, &["config", "--local", "--get", key]).filter(|v| !v.trim().is_empty())
}

fn is_git_repo(dir: &Path) -> bool {
    git_out(dir, &["rev-parse", "--git-dir"]).is_some()
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigPlan {
    Unchanged,
    Set,
    /// The current value is the one Nexus wrote last: replace it.
    Replace,
    /// The current value is someone else's: ask first.
    Ask,
}

fn plan_config(current: Option<&str>, desired: &str, managed: Option<&str>) -> ConfigPlan {
    match current.map(str::trim).filter(|c| !c.is_empty()) {
        None => ConfigPlan::Set,
        Some(c) if c == desired => ConfigPlan::Unchanged,
        Some(c) if managed.map(str::trim) == Some(c) => ConfigPlan::Replace,
        Some(_) => ConfigPlan::Ask,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigOutcome {
    Unchanged,
    Set,
    Replaced,
    /// A different local value stays (declined or non-interactive).
    Kept,
}

/// Set a local git config value, recording it under `managed_key`. A
/// different value Nexus did not write is replaced only when `ask` agrees.
fn apply_config(
    dir: &Path,
    key: &str,
    managed_key: &str,
    desired: &str,
    ask: &dyn Fn(&str) -> bool,
) -> Result<ConfigOutcome> {
    let current = local_get(dir, key);
    let managed = local_get(dir, managed_key);
    let outcome = match plan_config(current.as_deref(), desired, managed.as_deref()) {
        ConfigPlan::Unchanged => return Ok(ConfigOutcome::Unchanged),
        ConfigPlan::Set => ConfigOutcome::Set,
        ConfigPlan::Replace => ConfigOutcome::Replaced,
        ConfigPlan::Ask => {
            let question = format!(
                "Replace local {key} '{}' with '{desired}'?",
                current.as_deref().unwrap_or_default()
            );
            if !ask(&question) {
                return Ok(ConfigOutcome::Kept);
            }
            ConfigOutcome::Replaced
        }
    };
    git_ok(dir, &["config", "--local", key, desired])?;
    git_ok(dir, &["config", "--local", managed_key, desired])?;
    Ok(outcome)
}

/// Yes/no prompt; `--yes` answers yes, a non-interactive session no.
fn confirm(question: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("   {} {question} [y/N] ", style("?").bold().cyan());
    let _ = std::io::stdout().flush();
    let ch = console::Term::stdout().read_char().unwrap_or('n');
    println!("{ch}");
    ch == 'y' || ch == 'Y'
}

fn report_config(key: &str, outcome: &ConfigOutcome) {
    match outcome {
        ConfigOutcome::Unchanged => {}
        ConfigOutcome::Set => println!("   {} {key} set", style("GIT").bold().blue()),
        ConfigOutcome::Replaced => println!("   {} {key} replaced", style("GIT").bold().blue()),
        ConfigOutcome::Kept => println!(
            "   {} {key}: kept the different local value (rerun with --yes to replace)",
            style("!").bold().yellow()
        ),
    }
}

// ── Transport identity ──────────────────────────────────────────────────────

/// A key file name inside `~/.ssh`: `[A-Za-z0-9._-]{1,128}`, no leading
/// dot, not a `.pub` file. Never a path.
pub fn valid_ssh_key_name(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !key.starts_with('.')
        && !key.ends_with(".pub")
}

pub fn ssh_command(key: &str) -> String {
    format!("ssh -i ~/.ssh/{key} -o IdentitiesOnly=yes")
}

/// The account's SSH key, if set and valid.
fn ssh_key(gh: &GhEffective) -> Option<&str> {
    gh.ssh_key.as_deref().filter(|k| valid_ssh_key_name(k))
}

/// A parsed git remote URL.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteUrl {
    pub ssh: bool,
    /// Lower-cased host as written in the URL (an SSH host alias stays an
    /// alias).
    pub host: String,
    /// `owner/name`, without `.git`.
    pub repo: String,
}

/// Parse `git@host:owner/name.git`, `ssh://[user@]host[:port]/owner/name`
/// and `https://[user@]host[:port]/owner/name[.git]`.
pub fn parse_remote_url(url: &str) -> Option<RemoteUrl> {
    let url = url.trim();
    let build = |ssh: bool, host: &str, path: &str| {
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        if host.is_empty() || owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(RemoteUrl {
            ssh,
            host: host.to_ascii_lowercase(),
            repo: format!("{owner}/{name}"),
        })
    };
    let (ssh, rest) = if let Some(r) = url.strip_prefix("ssh://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("https://") {
        (false, r)
    } else if url.contains("://") {
        return None;
    } else {
        // scp-like: [user@]host:owner/name
        let (host, path) = url.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        return build(true, host, path);
    };
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next().unwrap_or_default();
    build(ssh, host, path)
}

/// Whether `remote` goes over an SSH host alias instead of `host` itself.
fn is_host_alias(remote: &RemoteUrl, host: &str) -> bool {
    remote.ssh && !remote.host.eq_ignore_ascii_case(host) && remote.host != format!("ssh.{host}")
}

/// `(name, url)` of every remote of the repository at `dir`.
fn remotes(dir: &Path) -> Vec<(String, String)> {
    git_out(dir, &["remote"])
        .unwrap_or_default()
        .lines()
        .filter(|n| !n.trim().is_empty())
        .filter_map(|n| git_out(dir, &["remote", "get-url", n]).map(|u| (n.to_string(), u)))
        .collect()
}

fn apply_ssh_command(dir: &Path, key: &str, assume_yes: bool) -> Result<()> {
    let ask = |q: &str| confirm(q, assume_yes);
    let outcome = apply_config(
        dir,
        "core.sshCommand",
        MANAGED_SSH_KEY,
        &ssh_command(key),
        &ask,
    )?;
    report_config("core.sshCommand", &outcome);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if home.is_some_and(|h| !h.join(".ssh").join(key).is_file()) {
        println!(
            "   {} ssh key ~/.ssh/{key} not found",
            style("!").bold().yellow()
        );
    }
    Ok(())
}

/// Route git over HTTPS to `host` through the profile:
/// `credential.https://<host>.helper` reset, then `gh auth git-credential`
/// with the profile's `GH_CONFIG_DIR`. Local config only.
fn apply_credential_helper(dir: &Path, host: &str, profile_dir: &Path) -> Result<()> {
    let key = format!("credential.https://{host}.helper");
    let helper = format!(
        "!GH_CONFIG_DIR={} gh auth git-credential",
        env_value(&profile_dir.to_string_lossy())
    );
    let current = git_out(dir, &["config", "--local", "--get-all", &key]).unwrap_or_default();
    if current.lines().collect::<Vec<_>>() == ["", helper.as_str()] {
        return Ok(());
    }
    // Exit code 5 (nothing to unset) is fine.
    let _ = git_out(dir, &["config", "--local", "--unset-all", &key]);
    git_ok(dir, &["config", "--local", "--add", &key, ""])?;
    git_ok(dir, &["config", "--local", "--add", &key, &helper])?;
    println!("   {} {key} set", style("GIT").bold().blue());
    Ok(())
}

fn valid_repo(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(o, n)| {
        [o, n].iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        })
    })
}

/// `gh repo set-default` for the repository at `dir`: `gh.repo`, else the
/// `owner/name` of `origin` (or the only remote).
fn apply_default_repo(
    dir: &Path,
    gh: &GhEffective,
    profile_dir: &Path,
    remotes: &[(String, String)],
) {
    let derived = || {
        let url = remotes
            .iter()
            .find(|(n, _)| n == "origin")
            .or(if remotes.len() == 1 {
                remotes.first()
            } else {
                None
            })?;
        parse_remote_url(&url.1).map(|r| r.repo)
    };
    let Some(repo) = gh.repo.clone().or_else(derived) else {
        return;
    };
    if !valid_repo(&repo) {
        return;
    }
    let current = gh_cmd(profile_dir, &gh.host)
        .args(["repo", "set-default", "--view"])
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if current.eq_ignore_ascii_case(&repo) {
        return;
    }
    let ok = gh_cmd(profile_dir, &gh.host)
        .args(["repo", "set-default", &repo])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!(
            "   {} gh default repository: {repo}",
            style("GIT").bold().blue()
        );
    } else {
        println!(
            "   {} gh repo set-default {repo} failed (no remote resolves to it?)",
            style("!").bold().yellow()
        );
    }
}

fn apply_canonical_remotes(
    dir: &Path,
    host: &str,
    remotes: &[(String, String)],
    assume_yes: bool,
) -> Result<()> {
    for (name, url) in remotes {
        let Some(remote) = parse_remote_url(url).filter(|r| is_host_alias(r, host)) else {
            continue;
        };
        let canonical = format!("git@{host}:{}.git", remote.repo);
        if confirm(
            &format!("Rewrite remote {name} {url} to {canonical}?"),
            assume_yes,
        ) {
            git_ok(dir, &["remote", "set-url", name, &canonical])?;
            println!(
                "   {} remote {name}: {canonical}",
                style("GIT").bold().blue()
            );
        }
    }
    Ok(())
}

// ── Commit identity ─────────────────────────────────────────────────────────

/// The account's commit email, unless the project sets its own.
pub fn account_commit_email<'a>(gh: &'a GhEffective, cfg: Option<&GitConfig>) -> Option<&'a str> {
    if cfg.is_some_and(|c| c.user_email.is_some()) {
        return None;
    }
    gh.commit_email.as_deref().filter(|e| !e.trim().is_empty())
}

fn account_commit_name<'a>(gh: &'a GhEffective, cfg: Option<&GitConfig>) -> Option<&'a str> {
    if cfg.is_some_and(|c| c.user_name.is_some()) {
        return None;
    }
    gh.commit_name.as_deref().filter(|n| !n.trim().is_empty())
}

fn apply_commit_identity(
    dir: &Path,
    gh: &GhEffective,
    cfg: Option<&GitConfig>,
    assume_yes: bool,
) -> Result<()> {
    let ask = |q: &str| confirm(q, assume_yes);
    if let Some(name) = account_commit_name(gh, cfg) {
        let outcome = apply_config(dir, "user.name", MANAGED_NAME_KEY, name, &ask)?;
        report_config("user.name", &outcome);
    }
    if let Some(email) = account_commit_email(gh, cfg) {
        let outcome = apply_config(dir, "user.email", MANAGED_EMAIL_KEY, email, &ask)?;
        report_config("user.email", &outcome);
    }
    Ok(())
}

// ── Apply / status ──────────────────────────────────────────────────────────

/// `GitHub:` status line (without the prefix).
pub fn status_line(
    gh: &GhEffective,
    active: Option<&str>,
    gh_env: &str,
    commit_email: Option<&str>,
) -> String {
    let login = active.or(gh.user.as_deref()).unwrap_or("?");
    let ssh = match ssh_key(gh) {
        Some(k) => format!("ssh key {k}"),
        None => "no ssh key".to_string(),
    };
    let mut line = format!(
        "{login}@{} via profile {} ({}), {ssh}, {gh_env}",
        gh.host,
        gh.profile,
        origin_label(&gh.origin)
    );
    if let Some(email) = commit_email {
        line.push_str(&format!(", commits as {email}"));
    }
    line
}

#[derive(Debug, Default)]
pub struct ApplyOptions {
    pub assume_yes: bool,
    pub canonical_remotes: bool,
}

/// Apply the project's GitHub account to `workspace`: `.nexus/gh.env`, the
/// transport identity, the gh default repository and the commit identity.
/// Prints one line per change and returns the status line. Without an
/// account only a `gh.env` written by Nexus is removed; git config stays.
pub fn apply(
    workspace: &Path,
    gh: Option<&GhEffective>,
    cfg: Option<&GitConfig>,
    opts: &ApplyOptions,
) -> Option<String> {
    let warn = |e: anyhow::Error| println!("   {} GitHub: {e:#}", style("!").bold().yellow());
    let Some(gh) = gh else {
        if let Err(e) = sync_gh_env(workspace, None) {
            warn(e);
        }
        return None;
    };
    let profile_dir = match nexus_core::config::Config::dir() {
        Ok(d) => git::gh_profile_dir(&d, &gh.profile),
        Err(e) => {
            warn(e.into());
            return None;
        }
    };
    let login = check_login(gh, &profile_dir);
    let gh_env = match sync_gh_env(workspace, Some((&profile_dir, &gh.host, &login))) {
        Ok(report) => report.describe(),
        Err(e) => {
            let text = format!("gh.env not written ({e})");
            warn(e);
            text
        }
    };

    if is_git_repo(workspace) {
        let remotes = remotes(workspace);
        let mut steps: Vec<Result<()>> = Vec::new();
        match (gh.ssh_key.as_deref(), ssh_key(gh)) {
            (_, Some(key)) => steps.push(apply_ssh_command(workspace, key, opts.assume_yes)),
            (Some(bad), None) => println!(
                "   {} ignoring invalid ssh key name '{bad}'",
                style("!").bold().yellow()
            ),
            _ => {}
        }
        if matches!(login, Login::Verified(_)) {
            let https_to_host = remotes.iter().any(|(_, u)| {
                parse_remote_url(u).is_some_and(|r| !r.ssh && r.host.eq_ignore_ascii_case(&gh.host))
            });
            if https_to_host {
                steps.push(apply_credential_helper(workspace, &gh.host, &profile_dir));
            }
            apply_default_repo(workspace, gh, &profile_dir, &remotes);
        }
        steps.push(apply_commit_identity(workspace, gh, cfg, opts.assume_yes));
        if opts.canonical_remotes {
            if ssh_key(gh).is_some() {
                steps.push(apply_canonical_remotes(
                    workspace,
                    &gh.host,
                    &remotes,
                    opts.assume_yes,
                ));
            } else {
                println!(
                    "   {} --canonical-remotes needs an ssh key on the account; remotes unchanged",
                    style("!").bold().yellow()
                );
            }
        }
        for e in steps.into_iter().filter_map(Result::err) {
            warn(e);
        }
    }

    if !matches!(login, Login::Verified(_) | Login::Unverified) {
        println!(
            "   {} GitHub: profile '{}' not ready for {} -- run: {}",
            style("!").bold().yellow(),
            gh.profile,
            gh.host,
            login_command(gh)
        );
    }
    let active = match &login {
        Login::Verified(a) => Some(a.as_str()),
        _ => None,
    };
    Some(status_line(
        gh,
        active,
        &gh_env,
        account_commit_email(gh, cfg),
    ))
}

/// Extra `nexus git verify` rows for the account: `(key, actual, ok,
/// detail)`. `logged_in` is the verified login state.
pub fn verify_rows(
    workspace: &Path,
    gh: &GhEffective,
    cfg: Option<&GitConfig>,
    profile_dir: &Path,
    logged_in: bool,
) -> Vec<(&'static str, String, bool, String)> {
    let mut rows = Vec::new();
    let file = gh_env_file_state(workspace, profile_dir, &gh.host);
    let (ok, detail) = match (&file, logged_in) {
        (GhEnvFile::Current, true) => (true, String::new()),
        (GhEnvFile::Missing, false) => (true, "not needed (profile not logged in)".into()),
        (GhEnvFile::Foreign, _) => (false, "not managed by Nexus".into()),
        (GhEnvFile::Missing, true) => (false, "missing; run: nexus git apply".into()),
        (_, _) => (false, "stale; run: nexus git apply".into()),
    };
    let actual = match file {
        GhEnvFile::Missing => "(missing)".to_string(),
        _ => GH_ENV_REL.to_string(),
    };
    rows.push(("gh.env", actual, ok, detail));

    if !is_git_repo(workspace) {
        return rows;
    }
    if let Some(key) = ssh_key(gh) {
        let expected = ssh_command(key);
        let local = local_get(workspace, "core.sshCommand");
        let (ok, detail) = match &local {
            Some(v) if v.trim() == expected => (true, String::new()),
            Some(_) => (false, format!("different; expected={expected}")),
            None => (false, format!("missing; expected={expected}")),
        };
        rows.push((
            "core.sshCommand",
            local.unwrap_or_else(|| "(not set)".into()),
            ok,
            detail,
        ));
    }
    for (name, url) in remotes(workspace) {
        if let Some(remote) = parse_remote_url(&url).filter(|r| is_host_alias(r, &gh.host)) {
            let detail = if ssh_key(gh).is_some() {
                format!(
                    "ssh host alias {}; run: nexus git apply --canonical-remotes",
                    remote.host
                )
            } else {
                format!("ssh host alias {}", remote.host)
            };
            // Without an ssh key the alias is the only route: informational.
            let ok = ssh_key(gh).is_none();
            rows.push(("remote", format!("{name}={url}"), ok, detail));
        }
    }
    if let Some(email) = account_commit_email(gh, cfg) {
        let local = local_get(workspace, "user.email");
        let ok = local.as_deref().map(str::trim) == Some(email.trim());
        rows.push((
            "user.email",
            local.unwrap_or_else(|| "(not set)".into()),
            ok,
            if ok {
                String::new()
            } else {
                format!("account commits as {email}")
            },
        ));
    }
    rows
}

// ── nexus git login ─────────────────────────────────────────────────────────

/// A local `gh` profile name: `^[a-z0-9][a-z0-9._-]{0,63}$`.
fn valid_profile_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// `(host, expected login, profile)` for `nexus git login`. With an
/// account configured, the flags must name that account or profile.
fn login_target(
    gh: Option<&GhEffective>,
    account: Option<&str>,
    profile: Option<&str>,
) -> Result<(String, Option<String>, String)> {
    if let Some(p) = profile {
        if !valid_profile_name(p) {
            bail!("invalid profile name '{p}' (expected [a-z0-9][a-z0-9._-]{{0,63}})");
        }
    }
    match gh {
        Some(gh) => {
            if let (Some(a), Some(u)) = (account, gh.user.as_deref()) {
                if !a.eq_ignore_ascii_case(u) {
                    bail!("this project path expects GitHub account {u}, not {a}");
                }
            }
            if let Some(p) = profile {
                if p != gh.profile {
                    bail!("this project path uses gh profile {}, not {p}", gh.profile);
                }
            }
            let expected = account.map(str::to_string).or_else(|| gh.user.clone());
            Ok((gh.host.clone(), expected, gh.profile.clone()))
        }
        None => {
            let Some(account) = account else {
                bail!("no GitHub account is configured for this project; pass --account <login>");
            };
            let profile = account.to_ascii_lowercase();
            if !valid_profile_name(&profile) {
                bail!("cannot derive a profile name from '{account}'");
            }
            Ok(("github.com".into(), Some(account.to_string()), profile))
        }
    }
}

fn check_expected(expected: Option<&str>, active: &str) -> Result<()> {
    match expected {
        Some(e) if !e.eq_ignore_ascii_case(active) => {
            bail!("the token belongs to GitHub account {active}, expected {e}; nothing written")
        }
        _ => Ok(()),
    }
}

/// Read a token from stdin: a hidden prompt on a terminal, else all input.
fn read_token() -> Result<String> {
    let token = if std::io::stdin().is_terminal() {
        eprint!("GitHub token: ");
        let _ = std::io::stderr().flush();
        console::Term::stderr()
            .read_secure_line()
            .context("could not read the token")?
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("could not read the token from stdin")?;
        buf
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("no token given on stdin");
    }
    Ok(token)
}

/// Run `nexus git login`: log the account's profile in, check that the
/// login is the expected account, then apply the account to the workspace.
pub fn login(
    workspace: &Path,
    gh: Option<&GhEffective>,
    cfg: Option<&GitConfig>,
    account: Option<&str>,
    profile: Option<&str>,
    with_token: bool,
    assume_yes: bool,
) -> Result<()> {
    let (host, expected, profile) = login_target(gh, account, profile)?;
    let config_dir = nexus_core::config::Config::dir()?;
    let profile_dir = git::gh_profile_dir(&config_dir, &profile);

    if with_token {
        let token = read_token()?;
        let active = git::verify_gh_seed_token(&token, &host)
            .ok_or_else(|| anyhow!("the token could not be verified against {host}"))?;
        check_expected(expected.as_deref(), &active)?;
        git::ensure_gh_profile_dir(&config_dir, &profile)?;
        if !git::gh_auth_login_with_token(&profile_dir, &host, &token)? {
            bail!("'gh auth login --with-token' failed for profile {profile}");
        }
    } else {
        git::ensure_gh_profile_dir(&config_dir, &profile)?;
        let status = gh_cmd(&profile_dir, &host)
            .args([
                "auth",
                "login",
                "--web",
                "--hostname",
                &host,
                "--git-protocol",
                "ssh",
                "--skip-ssh-key",
            ])
            .status()
            .context("failed to run 'gh auth login'")?;
        if !status.success() {
            bail!("'gh auth login' did not complete");
        }
    }

    match git::gh_verify_login(&profile_dir, &host, expected.as_deref()) {
        GhVerifyStatus::Ok { active } => println!(
            "{} Profile {profile} logged in to {host} as {active}.",
            style("OK").bold().green()
        ),
        GhVerifyStatus::Mismatch { active, expected } => {
            let _ = gh_cmd(&profile_dir, &host)
                .args(["auth", "logout", "--hostname", &host, "--user", &active])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            bail!(
                "profile {profile} was logged in as {active}, expected {expected}; \
                 logged out again, nothing written"
            );
        }
        GhVerifyStatus::NotLoggedIn => {
            bail!("profile {profile} is not logged in to {host}")
        }
        GhVerifyStatus::GhNotFound => {
            bail!("gh binary not found -- install from https://cli.github.com")
        }
    }

    match gh.filter(|g| g.profile == profile) {
        Some(gh) => {
            let opts = ApplyOptions {
                assume_yes,
                canonical_remotes: false,
            };
            if let Some(line) = apply(workspace, Some(gh), cfg, &opts) {
                println!("   GitHub: {line}");
            }
        }
        None => println!(
            "   {} No GitHub account is configured for this project in Nexus: \
             gh.env and git config left unchanged.",
            style("!").bold().yellow()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(user: Option<&str>) -> GhEffective {
        GhEffective {
            host: "github.com".into(),
            user: user.map(str::to_string),
            profile: "octocat".into(),
            source: "prompt".into(),
            origin: "binding".into(),
            ssh_key: None,
            repo: None,
            commit_name: None,
            commit_email: None,
        }
    }

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init", "-q"]).unwrap();
        dir
    }

    // ── gh.env writer ──

    #[test]
    fn test_env_value_plain_and_quoted() {
        assert_eq!(
            env_value("/home/dev/.config/nexus/gh-profiles/a"),
            "/home/dev/.config/nexus/gh-profiles/a"
        );
        assert_eq!(env_value("ghe.example.com:8443"), "ghe.example.com:8443");
        assert_eq!(env_value("/Users/Jane Doe/x"), "'/Users/Jane Doe/x'");
        assert_eq!(env_value("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(env_value("$HOME/x"), "'$HOME/x'");
        assert_eq!(env_value(""), "''");
    }

    #[test]
    fn test_render_gh_env_github_com_has_no_host() {
        let out = render_gh_env(
            Path::new("/home/dev/.config/nexus/gh-profiles/octocat"),
            "github.com",
        );
        assert_eq!(
            out,
            "# GitHub account for this project path\n\
             GH_CONFIG_DIR=/home/dev/.config/nexus/gh-profiles/octocat\n"
        );
        assert!(!out.contains("TOKEN"));
    }

    #[test]
    fn test_render_gh_env_enterprise_host() {
        let out = render_gh_env(Path::new("/p d/x"), "ghe.example.com");
        assert!(out.contains("GH_CONFIG_DIR='/p d/x'\n"));
        assert!(out.ends_with("GH_HOST=ghe.example.com\n"));
    }

    #[test]
    fn test_gh_env_action_per_login() {
        let abs = Path::new("/abs/profile");
        assert_eq!(
            gh_env_action(&Login::Verified("a".into()), abs),
            GhEnvAction::Write
        );
        assert!(matches!(
            gh_env_action(&Login::NotLoggedIn, abs),
            GhEnvAction::Remove(_)
        ));
        assert!(matches!(
            gh_env_action(&Login::GhNotFound, abs),
            GhEnvAction::Remove(_)
        ));
        assert!(matches!(
            gh_env_action(&Login::Unverified, abs),
            GhEnvAction::Keep(_)
        ));
        match gh_env_action(
            &Login::Mismatch {
                active: "other".into(),
                expected: "octocat".into(),
            },
            abs,
        ) {
            GhEnvAction::Remove(r) => assert!(r.contains("other") && r.contains("octocat")),
            a => panic!("expected Remove, got {a:?}"),
        }
        // Never a relative path in the file.
        assert!(matches!(
            gh_env_action(&Login::Verified("a".into()), Path::new("rel/profile")),
            GhEnvAction::Remove(_)
        ));
    }

    #[test]
    fn test_sync_gh_env_writes_then_removes_on_logout() {
        let ws = tempfile::tempdir().unwrap();
        let profile = Path::new("/abs/gh-profiles/octocat");
        let report = sync_gh_env(
            ws.path(),
            Some((profile, "github.com", &Login::Verified("octocat".into()))),
        )
        .unwrap();
        assert!(report.written);
        assert_eq!(
            gh_env_file_state(ws.path(), profile, "github.com"),
            GhEnvFile::Current
        );

        let report = sync_gh_env(
            ws.path(),
            Some((profile, "github.com", &Login::NotLoggedIn)),
        )
        .unwrap();
        assert_eq!(
            report.describe(),
            "gh.env not written (profile not logged in)"
        );
        assert!(!ws.path().join(GH_ENV_REL).exists());
    }

    #[test]
    fn test_sync_gh_env_removes_when_no_account() {
        let ws = tempfile::tempdir().unwrap();
        let profile = Path::new("/abs/gh-profiles/octocat");
        sync_gh_env(
            ws.path(),
            Some((profile, "github.com", &Login::Verified("octocat".into()))),
        )
        .unwrap();
        let report = sync_gh_env(ws.path(), None).unwrap();
        assert!(!report.written);
        assert!(!ws.path().join(GH_ENV_REL).exists());
    }

    #[test]
    fn test_sync_gh_env_keeps_when_unverified_and_detects_stale() {
        let ws = tempfile::tempdir().unwrap();
        let old = Path::new("/abs/gh-profiles/old");
        sync_gh_env(
            ws.path(),
            Some((old, "github.com", &Login::Verified("a".into()))),
        )
        .unwrap();
        let new = Path::new("/abs/gh-profiles/new");
        assert_eq!(
            gh_env_file_state(ws.path(), new, "github.com"),
            GhEnvFile::Stale
        );
        let report = sync_gh_env(ws.path(), Some((new, "github.com", &Login::Unverified))).unwrap();
        assert!(report.written);
        assert_eq!(
            gh_env_file_state(ws.path(), old, "github.com"),
            GhEnvFile::Current
        );
    }

    #[test]
    fn test_sync_gh_env_never_touches_a_foreign_file() {
        let ws = tempfile::tempdir().unwrap();
        let path = ws.path().join(GH_ENV_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "GH_CONFIG_DIR=/mine\n").unwrap();
        let profile = Path::new("/abs/gh-profiles/octocat");
        let report = sync_gh_env(
            ws.path(),
            Some((profile, "github.com", &Login::Verified("a".into()))),
        )
        .unwrap();
        assert!(!report.written);
        sync_gh_env(ws.path(), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "GH_CONFIG_DIR=/mine\n"
        );
        assert_eq!(
            gh_env_file_state(ws.path(), profile, "github.com"),
            GhEnvFile::Foreign
        );
    }

    // ── login targets and the prompt source ──

    #[test]
    fn test_login_command_names_account() {
        assert_eq!(
            login_command(&sample(Some("octocat"))),
            "nexus git login --account octocat"
        );
        assert_eq!(login_command(&sample(None)), "nexus git login");
    }

    #[test]
    fn test_login_target_uses_account_of_project() {
        let gh = sample(Some("octocat"));
        let (host, expected, profile) = login_target(Some(&gh), None, None).unwrap();
        assert_eq!(
            (host.as_str(), expected.as_deref(), profile.as_str()),
            ("github.com", Some("octocat"), "octocat")
        );
        assert!(login_target(Some(&gh), Some("OctoCat"), None).is_ok());
        let err = login_target(Some(&gh), Some("someone"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("octocat") && err.contains("someone"));
        assert!(login_target(Some(&gh), None, Some("other")).is_err());
    }

    #[test]
    fn test_login_target_without_account_needs_flag() {
        assert!(login_target(None, None, None).is_err());
        let (_, expected, profile) = login_target(None, Some("OctoCat"), None).unwrap();
        assert_eq!(
            (expected.as_deref(), profile.as_str()),
            (Some("OctoCat"), "octocat")
        );
        assert!(login_target(None, None, Some("../x")).is_err());
    }

    #[test]
    fn test_check_expected_names_both_logins() {
        assert!(check_expected(Some("octocat"), "OCTOCAT").is_ok());
        assert!(check_expected(None, "anyone").is_ok());
        let err = check_expected(Some("octocat"), "intruder")
            .unwrap_err()
            .to_string();
        assert!(err.contains("octocat") && err.contains("intruder"));
    }

    #[test]
    fn test_origin_labels() {
        assert_eq!(origin_label("project"), "project pin");
        assert_eq!(origin_label("binding"), "project binding");
        assert_eq!(origin_label("USER"), "user default");
    }

    #[test]
    fn test_status_line() {
        let mut gh = sample(Some("octocat"));
        gh.ssh_key = Some("id_ed25519_customer".into());
        assert_eq!(
            status_line(&gh, None, "gh.env written", Some("dev@example.com")),
            "octocat@github.com via profile octocat (project binding), ssh key id_ed25519_customer, \
             gh.env written, commits as dev@example.com"
        );
        gh.ssh_key = Some("../id_rsa".into());
        assert!(status_line(&gh, Some("x"), "gh.env not written", None).starts_with("x@github.com"));
        assert!(status_line(&gh, None, "-", None).contains("no ssh key"));
    }

    // ── transport identity ──

    #[test]
    fn test_valid_ssh_key_name() {
        assert!(valid_ssh_key_name("id_ed25519"));
        assert!(valid_ssh_key_name("customer.key-2"));
        for bad in [
            "",
            ".hidden",
            "id_ed25519.pub",
            "../id",
            "a/b",
            "~/x",
            "a b",
            &"k".repeat(129),
        ] {
            assert!(!valid_ssh_key_name(bad), "{bad}");
        }
        assert_eq!(
            ssh_command("id_x"),
            "ssh -i ~/.ssh/id_x -o IdentitiesOnly=yes"
        );
    }

    #[test]
    fn test_parse_remote_url() {
        let r = parse_remote_url("git@github-customer:Org/repo.git").unwrap();
        assert_eq!(
            r,
            RemoteUrl {
                ssh: true,
                host: "github-customer".into(),
                repo: "Org/repo".into()
            }
        );
        assert!(is_host_alias(&r, "github.com"));
        let r = parse_remote_url("ssh://git@github.com:22/org/repo").unwrap();
        assert!(r.ssh && !is_host_alias(&r, "github.com"));
        let r = parse_remote_url("https://user@github.com/org/repo.git").unwrap();
        assert_eq!(
            (r.ssh, r.host.as_str(), r.repo.as_str()),
            (false, "github.com", "org/repo")
        );
        assert!(!is_host_alias(
            &parse_remote_url("git@ssh.github.com:o/r").unwrap(),
            "github.com"
        ));
        assert_eq!(parse_remote_url("/local/path/repo"), None);
        assert_eq!(parse_remote_url("file:///x/o/r"), None);
        assert_eq!(parse_remote_url("git@github.com:onlyname.git"), None);
    }

    #[test]
    fn test_plan_config() {
        assert_eq!(plan_config(None, "v", None), ConfigPlan::Set);
        assert_eq!(plan_config(Some(" "), "v", None), ConfigPlan::Set);
        assert_eq!(plan_config(Some("v"), "v", None), ConfigPlan::Unchanged);
        assert_eq!(
            plan_config(Some("old"), "v", Some("old")),
            ConfigPlan::Replace
        );
        assert_eq!(plan_config(Some("mine"), "v", Some("old")), ConfigPlan::Ask);
        assert_eq!(plan_config(Some("mine"), "v", None), ConfigPlan::Ask);
    }

    #[test]
    fn test_ssh_command_conflict_asks_and_keeps_on_no() {
        let repo = git_repo();
        let dir = repo.path();
        git_ok(
            dir,
            &["config", "--local", "core.sshCommand", "ssh -i ~/.ssh/mine"],
        )
        .unwrap();
        let desired = ssh_command("id_customer");

        let asked = std::cell::Cell::new(false);
        let no = |_: &str| {
            asked.set(true);
            false
        };
        let outcome = apply_config(dir, "core.sshCommand", MANAGED_SSH_KEY, &desired, &no).unwrap();
        assert!(asked.get());
        assert_eq!(outcome, ConfigOutcome::Kept);
        assert_eq!(
            local_get(dir, "core.sshCommand").as_deref(),
            Some("ssh -i ~/.ssh/mine")
        );

        let yes = |_: &str| true;
        let outcome =
            apply_config(dir, "core.sshCommand", MANAGED_SSH_KEY, &desired, &yes).unwrap();
        assert_eq!(outcome, ConfigOutcome::Replaced);
        assert_eq!(local_get(dir, "core.sshCommand"), Some(desired.clone()));

        // A value Nexus wrote is replaced without asking.
        let panic_ask = |_: &str| -> bool { panic!("must not ask") };
        let next = ssh_command("id_other");
        let outcome =
            apply_config(dir, "core.sshCommand", MANAGED_SSH_KEY, &next, &panic_ask).unwrap();
        assert_eq!(outcome, ConfigOutcome::Replaced);
        let outcome =
            apply_config(dir, "core.sshCommand", MANAGED_SSH_KEY, &next, &panic_ask).unwrap();
        assert_eq!(outcome, ConfigOutcome::Unchanged);
    }

    #[test]
    fn test_credential_helper_is_local_and_idempotent() {
        let repo = git_repo();
        let dir = repo.path();
        let profile = Path::new("/abs/gh profiles/octocat");
        apply_credential_helper(dir, "github.com", profile).unwrap();
        apply_credential_helper(dir, "github.com", profile).unwrap();
        let values = git_out(
            dir,
            &[
                "config",
                "--local",
                "--get-all",
                "credential.https://github.com.helper",
            ],
        )
        .unwrap();
        assert_eq!(
            values.lines().collect::<Vec<_>>(),
            [
                "",
                "!GH_CONFIG_DIR='/abs/gh profiles/octocat' gh auth git-credential"
            ]
        );
    }

    // ── commit identity ──

    #[test]
    fn test_project_identity_wins_over_account() {
        let mut gh = sample(Some("octocat"));
        gh.commit_name = Some("Dev".into());
        gh.commit_email = Some("dev@example.com".into());
        assert_eq!(account_commit_email(&gh, None), Some("dev@example.com"));
        let cfg = GitConfig {
            user_name: None,
            user_email: Some("project@example.com".into()),
            signing_key: None,
            commit_gpgsign: None,
            include_co_authored_by: None,
            gh: None,
        };
        assert_eq!(account_commit_email(&gh, Some(&cfg)), None);
        // Each field independently.
        assert_eq!(account_commit_name(&gh, Some(&cfg)), Some("Dev"));
    }

    #[test]
    fn test_commit_identity_applied_and_foreign_email_kept_without_yes() {
        let repo = git_repo();
        let dir = repo.path();
        git_ok(dir, &["config", "--local", "user.email", "own@example.com"]).unwrap();
        let mut gh = sample(Some("octocat"));
        gh.commit_name = Some("Dev".into());
        gh.commit_email = Some("dev@example.com".into());
        // assume_yes = false in a test process: the prompt is not interactive
        // there, or answered by the closure below.
        let no = |_: &str| false;
        let name = apply_config(dir, "user.name", MANAGED_NAME_KEY, "Dev", &no).unwrap();
        let email =
            apply_config(dir, "user.email", MANAGED_EMAIL_KEY, "dev@example.com", &no).unwrap();
        assert_eq!((name, email), (ConfigOutcome::Set, ConfigOutcome::Kept));
        assert_eq!(
            local_get(dir, "user.email").as_deref(),
            Some("own@example.com")
        );
        apply_commit_identity(dir, &gh, None, true).unwrap();
        assert_eq!(
            local_get(dir, "user.email").as_deref(),
            Some("dev@example.com")
        );
    }

    #[test]
    fn test_older_backend_json_without_new_fields() {
        let gh: GhEffective = serde_json::from_str(
            r#"{"host":"github.com","user":"octocat","profile":"octocat","source":"auto","origin":"user"}"#,
        )
        .unwrap();
        assert_eq!((gh.ssh_key, gh.repo, gh.commit_email), (None, None, None));
    }
}
