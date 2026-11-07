//! `nexus dispatch runner`: this machine's Dispatch runner (notify mode).
//!
//! `register` uses the personal access token once: the hub returns a runner
//! token (`nxs_run_...`) a single time, which is kept in
//! `~/.config/nexus/dispatch-runner.json` (mode 0600) and never printed. All
//! later calls (`listen`, `status`, `pause`, `resume`, `revoke`) use that
//! runner token. A project binding carries the sha256 of the repository
//! path, never the path itself.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use console::style;
use nexus_core::api::{
    NexusClient, RunnerBinding, RunnerProjectRequest, RunnerRegisterRequest, RunnerRuntime,
};
use nexus_core::auth::resolve_token;
use nexus_core::config;
use serde::{Deserialize, Serialize};

/// Claude Code versions the cross-session inbox was tested with: the frame
/// format beyond the auth line is not documented, so delivery stays off
/// outside this range (lowest tested version, and the minor line).
pub(crate) const CLAUDE_TESTED_MIN: (u64, u64, u64) = (2, 1, 319);
pub(crate) const CLAUDE_TESTED_LINE: (u64, u64) = (2, 1);

/// The capability the hub knows for pushing notices into running sessions.
pub(crate) const CROSS_SESSION_INBOX: &str = "cross_session_inbox";

/// What `register` stored: the runner token and the bindings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunnerFile {
    pub runner_id: String,
    /// Runner token (`nxs_run_...`); never printed or logged.
    pub token: String,
    #[serde(default)]
    pub token_prefix: Option<String>,
    #[serde(default)]
    pub token_expires_at: Option<String>,
    /// Hub the runner was registered with; `listen` talks to the same one.
    pub api_url: String,
    pub name: String,
    pub registered_at: String,
    pub projects: Vec<BoundProject>,
    /// Project names known at register time, for notice texts (a runner
    /// token cannot list projects).
    #[serde(default)]
    pub project_names: BTreeMap<String, String>,
    #[serde(default)]
    pub runtimes: Vec<RunnerRuntime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BoundProject {
    pub project_id: String,
    pub mode: String,
    pub runtime: String,
    /// sha256 of the repository path; sessions are matched against it.
    #[serde(default)]
    pub repo_path_hash: Option<String>,
}

impl RunnerFile {
    pub fn name_of(&self, project_id: &str) -> String {
        self.project_names
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| project_id.chars().take(8).collect())
    }
}

/// `~/.config/nexus/dispatch-runner.json`.
pub(crate) fn runner_file_path() -> anyhow::Result<PathBuf> {
    Ok(config::Config::dir()?.join("dispatch-runner.json"))
}

/// `~/.config/nexus/state/dispatch`: runner state, lock, notices, sessions.
pub(crate) fn state_dir() -> anyhow::Result<PathBuf> {
    Ok(config::Config::dir()?.join("state").join("dispatch"))
}

pub(crate) fn load_runner_file(path: &Path) -> anyhow::Result<Option<RunnerFile>> {
    match fs::read_to_string(path) {
        Ok(body) => Ok(Some(
            serde_json::from_str(&body).context("the runner file is not valid")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write `body` to `path` through a temp file, with mode 0600 (directory
/// 0700) before the content lands.
pub(crate) fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn save_runner_file(path: &Path, file: &RunnerFile) -> anyhow::Result<()> {
    write_private(path, &(serde_json::to_string_pretty(file)? + "\n"))?;
    Ok(())
}

/// sha256 of a repository path (canonical where possible), never the path.
pub(crate) fn repo_path_hash(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    nexus_core::hash::sha256_hex(&canonical.to_string_lossy())
}

/// `2.1.319 (Claude Code)` -> (2, 1, 319).
pub(crate) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let v = text.split_whitespace().next()?;
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Whether a Claude Code version is inside the tested range.
pub(crate) fn claude_version_tested(v: (u64, u64, u64)) -> bool {
    v >= CLAUDE_TESTED_MIN && (v.0, v.1) == CLAUDE_TESTED_LINE
}

/// Probe result for the Claude Code delivery path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeProbe {
    pub version: Option<String>,
    pub inbox: Result<(), String>,
}

impl ClaudeProbe {
    pub fn runtime(&self) -> RunnerRuntime {
        RunnerRuntime {
            runtime: "claude_code".into(),
            version: self.version.clone(),
            capabilities: if self.inbox.is_ok() {
                vec![CROSS_SESSION_INBOX.into()]
            } else {
                vec![]
            },
        }
    }
}

/// Capability probe of the installed Claude Code (`claude --version`). The
/// cross-session inbox counts as available only for a tested version; the
/// per-session socket is checked again at each delivery.
pub(crate) fn probe_claude() -> ClaudeProbe {
    let out = std::process::Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            return ClaudeProbe {
                version: None,
                inbox: Err("claude not found on PATH".into()),
            }
        }
    };
    probe_from_version(&text)
}

pub(crate) fn probe_from_version(text: &str) -> ClaudeProbe {
    let Some(v) = parse_version(text) else {
        return ClaudeProbe {
            version: None,
            inbox: Err(format!("unrecognised version output: {text}")),
        };
    };
    let version = format!("{}.{}.{}", v.0, v.1, v.2);
    let inbox = if claude_version_tested(v) {
        Ok(())
    } else {
        Err(format!(
            "version {version} is outside the tested range ({}.{}.{} to {}.{}.x)",
            CLAUDE_TESTED_MIN.0,
            CLAUDE_TESTED_MIN.1,
            CLAUDE_TESTED_MIN.2,
            CLAUDE_TESTED_LINE.0,
            CLAUDE_TESTED_LINE.1
        ))
    };
    ClaudeProbe {
        version: Some(version),
        inbox,
    }
}

fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes a
    // NUL-terminated name into it or fails.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown-host".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim().to_string()
}

fn now_rfc3339() -> String {
    super::dispatch_status::rfc3339_now()
}

pub struct RegisterOptions<'a> {
    pub project_ids: &'a [String],
    pub mode: &'a str,
    pub name: Option<&'a str>,
    pub expires_days: u32,
    /// Drop the bindings of an earlier register instead of keeping them.
    pub replace: bool,
}

/// `nexus dispatch runner register`.
pub async fn register(api_url: &str, opts: RegisterOptions<'_>) -> anyhow::Result<()> {
    let pat = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let path = runner_file_path()?;
    let previous = load_runner_file(&path).ok().flatten();
    let workspace = std::env::current_dir()?;

    let mut projects: Vec<BoundProject> = match (&previous, opts.replace) {
        (Some(p), false) if p.api_url == api_url => p.projects.clone(),
        _ => vec![],
    };
    let mut wanted: Vec<(String, Option<String>)> = opts
        .project_ids
        .iter()
        .map(|id| (id.clone(), None))
        .collect();
    if wanted.is_empty() {
        let id = config::resolve_project_id(None, Some(&workspace))?;
        let root = super::shadow_guard::repo_root(&workspace).unwrap_or(workspace.clone());
        wanted.push((id, Some(repo_path_hash(&root))));
    }
    for (id, hash) in wanted {
        let entry = BoundProject {
            project_id: id.clone(),
            mode: opts.mode.to_string(),
            runtime: "claude_code".into(),
            repo_path_hash: hash,
        };
        match projects.iter_mut().find(|p| p.project_id == id) {
            Some(p) => {
                p.mode = entry.mode;
                if entry.repo_path_hash.is_some() {
                    p.repo_path_hash = entry.repo_path_hash;
                }
            }
            None => projects.push(entry),
        }
    }

    let probe = probe_claude();
    let client = NexusClient::new(api_url, Some(pat))?;
    let names: BTreeMap<String, String> = client
        .list_projects()
        .await
        .map(|r| r.projects.into_iter().map(|p| (p.id, p.name)).collect())
        .unwrap_or_default();
    let name = opts
        .name
        .map(str::to_string)
        .unwrap_or_else(|| format!("nexus-cli@{}", host_name()));
    let request = RunnerRegisterRequest {
        machine_id: nexus_core::machine::resolve_machine_id()?,
        name: name.clone(),
        cli_version: env!("CARGO_PKG_VERSION").into(),
        runtimes: vec![probe.runtime()],
        projects: projects
            .iter()
            .map(|p| RunnerProjectRequest {
                project_id: p.project_id.clone(),
                mode: p.mode.clone(),
                runtime: p.runtime.clone(),
                repo_path_hash: p.repo_path_hash.clone(),
            })
            .collect(),
        expires_in_days: opts.expires_days,
    };
    let resp = client.runner_register(&request).await?;
    let file = RunnerFile {
        runner_id: resp.runner_id.clone(),
        token: resp.token,
        token_prefix: resp.token_prefix.clone(),
        token_expires_at: resp.token_expires_at.clone(),
        api_url: api_url.to_string(),
        name,
        registered_at: now_rfc3339(),
        projects,
        project_names: names,
        runtimes: vec![probe.runtime()],
    };
    save_runner_file(&path, &file)?;

    println!(
        "{} runner {} ({}), token {}… stored in {} (0600)",
        style("Registered").green().bold(),
        file.runner_id,
        file.name,
        file.token_prefix.as_deref().unwrap_or("nxs_run_"),
        path.display()
    );
    if let Some(exp) = &file.token_expires_at {
        println!("  token expires {exp}");
    }
    if !resp.replaced.is_empty() {
        println!(
            "  replaced {} earlier runner(s) on this machine",
            resp.replaced.len()
        );
    }
    print_probe(&probe);
    print_bindings(&file, &resp.projects, resp.platform_enabled);
    Ok(())
}

fn print_probe(probe: &ClaudeProbe) {
    match &probe.inbox {
        Ok(()) => println!(
            "  claude_code {}: {} (tested range)",
            probe.version.as_deref().unwrap_or("?"),
            CROSS_SESSION_INBOX
        ),
        Err(reason) => println!(
            "  claude_code {}: no {} ({reason}); notices fall back to a desktop notification",
            probe.version.as_deref().unwrap_or("?"),
            CROSS_SESSION_INBOX
        ),
    }
}

/// One line per binding: effective mode, the switch that blocks it, lag.
pub(crate) fn binding_lines(file: &RunnerFile, bindings: &[RunnerBinding]) -> Vec<String> {
    bindings
        .iter()
        .map(|b| {
            let mut line = format!(
                "{} mode {} (requested {})",
                file.name_of(&b.project_id),
                b.mode_effective.as_deref().unwrap_or("?"),
                b.mode_requested.as_deref().unwrap_or("?"),
            );
            if let Some(by) = &b.blocked_by {
                line.push_str(&format!(", blocked by {by}"));
            }
            if let Some(lag) = b.lag {
                line.push_str(&format!(", lag {lag}"));
            }
            if let Some(err) = &b.last_error {
                line.push_str(&format!(", last error: {err}"));
            }
            line
        })
        .collect()
}

fn print_bindings(file: &RunnerFile, bindings: &[RunnerBinding], platform_enabled: bool) {
    if !platform_enabled {
        println!(
            "  {} automation is off on the platform; the runner stays idle",
            style("!").yellow().bold()
        );
    }
    for line in binding_lines(file, bindings) {
        println!("  {line}");
    }
}

fn require_runner() -> anyhow::Result<(PathBuf, RunnerFile)> {
    let path = runner_file_path()?;
    let file = load_runner_file(&path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No runner registered on this machine. Run 'nexus dispatch runner register'."
        )
    })?;
    Ok((path, file))
}

pub(crate) fn runner_client(file: &RunnerFile) -> anyhow::Result<NexusClient> {
    Ok(NexusClient::new(&file.api_url, Some(file.token.clone()))?)
}

/// `nexus dispatch runner status`: heartbeat once and show the bindings.
pub async fn status(json: bool) -> anyhow::Result<()> {
    let (_, file) = require_runner()?;
    let client = runner_client(&file)?;
    let hb = client
        .runner_heartbeat(serde_json::json!({ "cli_version": env!("CARGO_PKG_VERSION") }))
        .await?;
    let probe = probe_claude();
    if json {
        let out = serde_json::json!({
            "schema": "nexus.dispatch-runner.v1",
            "runner_id": file.runner_id,
            "name": file.name,
            "api_url": file.api_url,
            "token_prefix": file.token_prefix,
            "token_expires_at": file.token_expires_at,
            "paused": hb.paused,
            "platform_enabled": hb.platform_enabled,
            "claude_code": {
                "version": probe.version,
                "cross_session_inbox": probe.inbox.is_ok(),
            },
            "projects": hb.projects,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!(
        "{} {} ({}), {}",
        style("Runner").bold(),
        file.runner_id,
        file.name,
        if hb.paused {
            style("paused").yellow().to_string()
        } else {
            style("active").green().to_string()
        }
    );
    println!(
        "  hub {}, token {}…{}",
        file.api_url,
        file.token_prefix.as_deref().unwrap_or("nxs_run_"),
        file.token_expires_at
            .as_deref()
            .map(|e| format!(", expires {e}"))
            .unwrap_or_default()
    );
    print_probe(&probe);
    print_bindings(&file, &hb.projects, hb.platform_enabled);
    Ok(())
}

/// `nexus dispatch runner pause | resume | revoke`.
pub async fn control(action: &str) -> anyhow::Result<()> {
    let (path, file) = require_runner()?;
    let client = runner_client(&file)?;
    client
        .runner_control(&format!("runner_{action}"), None)
        .await?;
    match action {
        "revoke" => {
            fs::remove_file(&path)?;
            println!(
                "{} runner {}; the local token file was removed",
                style("Revoked").red().bold(),
                file.runner_id
            );
        }
        "pause" => println!(
            "{} runner {}",
            style("Paused").yellow().bold(),
            file.runner_id
        ),
        _ => println!(
            "{} runner {}",
            style("Resumed").green().bold(),
            file.runner_id
        ),
    }
    Ok(())
}

/// `nexus dispatch runner probe`: the Claude Code delivery capability.
pub fn probe(json: bool) -> anyhow::Result<()> {
    let p = probe_claude();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "runtime": "claude_code",
                "version": p.version,
                "cross_session_inbox": p.inbox.is_ok(),
                "reason": p.inbox.err(),
            })
        );
    } else {
        print_probe(&p);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parse_and_range() {
        assert_eq!(parse_version("2.1.319 (Claude Code)"), Some((2, 1, 319)));
        assert_eq!(parse_version("garbage"), None);
        assert!(claude_version_tested((2, 1, 319)));
        assert!(claude_version_tested((2, 1, 400)));
        assert!(!claude_version_tested((2, 1, 318)));
        assert!(!claude_version_tested((2, 2, 0)));
        let p = probe_from_version("2.0.9 (Claude Code)");
        assert!(p.inbox.is_err());
        assert!(p.runtime().capabilities.is_empty());
        let ok = probe_from_version("2.1.319 (Claude Code)");
        assert_eq!(ok.runtime().capabilities, [CROSS_SESSION_INBOX]);
    }

    #[test]
    fn test_repo_hash_is_not_the_path() {
        let h = repo_path_hash(Path::new("/nonexistent/repo"));
        assert_eq!(h.len(), 64);
        assert!(!h.contains("repo"));
        assert_eq!(h, repo_path_hash(Path::new("/nonexistent/repo")));
    }

    #[test]
    fn test_private_write_is_0600() {
        let dir = std::env::temp_dir().join(format!("nexus-runner-{}", std::process::id()));
        let path = dir.join("f.json");
        write_private(&path, "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        let _ = fs::remove_dir_all(&dir);
    }
}
