//! `nexus dispatch runner`: this machine's Dispatch runners (notify mode),
//! one per hub.
//!
//! `register` uses the personal access token once: the hub returns a runner
//! token (`nxs_run_...`) a single time, which is kept in
//! `~/.config/nexus/dispatch-runners/<hub>.json` (mode 0600) and never
//! printed; `<hub>` is the API origin without the scheme (`https`), so a
//! production and a staging runner live side by side. All later calls
//! (`listen`, `status`, `pause`, `resume`, `revoke`) use that runner token.
//! A project binding carries the sha256 of the repository path, never the
//! path itself.
//!
//! The single `dispatch-runner.json` of earlier versions is moved to its
//! hub's file on first use, together with the listener state (cursor).

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

/// The runner file of earlier versions: one runner per machine.
const LEGACY_RUNNER_FILE: &str = "dispatch-runner.json";
const RUNNERS_DIR: &str = "dispatch-runners";

/// A file and directory name for a hub: its origin without `https://`
/// (`http_` kept for plain HTTP), the port joined with `_`.
pub(crate) fn hub_slug(api_url: &str) -> String {
    let origin = config::trust::url_origin(api_url).unwrap_or_else(|| api_url.trim().to_string());
    let rest = origin
        .strip_prefix("https://")
        .map(str::to_string)
        .unwrap_or_else(|| origin.replacen("://", "_", 1));
    rest.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether two API URLs name the same hub.
pub(crate) fn same_hub(a: &str, b: &str) -> bool {
    hub_slug(a) == hub_slug(b)
}

/// Where one hub's runner lives.
#[derive(Debug, Clone)]
pub(crate) struct HubPaths {
    /// `~/.config/nexus/dispatch-runners/<hub>.json`: token and bindings.
    pub runner_file: PathBuf,
    /// `~/.config/nexus/state/dispatch`: notices and sessions, shared by all
    /// hubs (project ids are unique across hubs).
    pub state_dir: PathBuf,
    /// `<state_dir>/hubs/<hub>`: this hub's listener state and lock.
    pub hub_dir: PathBuf,
}

fn hub_paths_in(config_dir: &Path, api_url: &str) -> HubPaths {
    let slug = hub_slug(api_url);
    let state_dir = config_dir.join("state").join("dispatch");
    HubPaths {
        runner_file: config_dir.join(RUNNERS_DIR).join(format!("{slug}.json")),
        hub_dir: state_dir.join("hubs").join(&slug),
        state_dir,
    }
}

/// The paths of the hub `api_url` names, after moving an earlier single
/// runner into place.
pub(crate) fn hub_paths(api_url: &str) -> anyhow::Result<HubPaths> {
    let dir = config::Config::dir()?;
    let _ = migrate_legacy(&dir);
    let paths = hub_paths_in(&dir, api_url);
    // The move failed: keep reading the earlier file for its own hub.
    if !paths.runner_file.exists() {
        let legacy = dir.join(LEGACY_RUNNER_FILE);
        if let Ok(Some(f)) = load_runner_file(&legacy) {
            if same_hub(&f.api_url, api_url) {
                return Ok(HubPaths {
                    runner_file: legacy,
                    hub_dir: paths.state_dir.clone(),
                    ..paths
                });
            }
        }
    }
    Ok(paths)
}

/// Move `dispatch-runner.json` to its hub's file and the listener state
/// (`state/dispatch/runner.json`, with the cursor) to the hub's directory,
/// so nothing has to be registered again. Files already in place win.
pub(crate) fn migrate_legacy(config_dir: &Path) -> anyhow::Result<bool> {
    let legacy = config_dir.join(LEGACY_RUNNER_FILE);
    let Some(file) = load_runner_file(&legacy)? else {
        return Ok(false);
    };
    let paths = hub_paths_in(config_dir, &file.api_url);
    if !paths.runner_file.exists() {
        write_private(&paths.runner_file, &fs::read_to_string(&legacy)?)?;
    }
    let old_state = paths.state_dir.join("runner.json");
    let new_state = paths.hub_dir.join("runner.json");
    if old_state.exists() && !new_state.exists() {
        fs::create_dir_all(&paths.hub_dir)?;
        fs::rename(&old_state, &new_state)?;
    }
    fs::remove_file(&legacy)?;
    Ok(true)
}

/// Every registered runner on this machine, sorted by hub.
pub(crate) fn all_runners() -> anyhow::Result<Vec<(HubPaths, RunnerFile)>> {
    let dir = config::Config::dir()?;
    let _ = migrate_legacy(&dir);
    let mut out = vec![];
    if let Ok(entries) = fs::read_dir(dir.join(RUNNERS_DIR)) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                if let Ok(Some(f)) = load_runner_file(&e.path()) {
                    out.push((hub_paths_in(&dir, &f.api_url), f));
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(Some(f)) = load_runner_file(&dir.join(LEGACY_RUNNER_FILE)) {
            out.push((hub_paths(&f.api_url)?, f));
        }
    }
    out.sort_by(|a, b| a.1.api_url.cmp(&b.1.api_url));
    Ok(out)
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

/// The token prefix for display: the hub's prefix after `nxs_run_`.
pub(crate) fn shown_prefix(prefix: Option<&str>) -> String {
    match prefix {
        Some(p) if p.starts_with("nxs_run_") => p.to_string(),
        Some(p) => format!("nxs_run_{p}"),
        None => "nxs_run_".into(),
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
    let paths = hub_paths(api_url)?;
    let previous = load_runner_file(&paths.runner_file).ok().flatten();
    // Store under the hub's own file, also when it was read from the
    // earlier single file.
    let path = hub_paths_in(&config::Config::dir()?, api_url).runner_file;
    let workspace = std::env::current_dir()?;

    let mut projects: Vec<BoundProject> = match (&previous, opts.replace) {
        (Some(p), false) if same_hub(&p.api_url, api_url) => p.projects.clone(),
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
    if paths.runner_file != path {
        let _ = fs::remove_file(&paths.runner_file);
    }

    println!(
        "{} runner {} ({}), token {}… stored in {} (0600)",
        style("Registered").green().bold(),
        file.runner_id,
        file.name,
        shown_prefix(file.token_prefix.as_deref()),
        path.display()
    );
    if let Some(exp) = &file.token_expires_at {
        println!("  token expires {exp}");
    }
    if !resp.replaced.is_empty() {
        println!(
            "  replaced {} earlier runner(s) of this machine on this hub",
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
            match b.blocked_by.as_deref() {
                // Notify mode only withholds writes; listening is not blocked.
                Some("mode_notify") => line.push_str(", read only"),
                Some(by) => line.push_str(&format!(", blocked by {by}")),
                None => {}
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

fn require_runner(api_url: &str) -> anyhow::Result<(PathBuf, RunnerFile)> {
    let path = hub_paths(api_url)?.runner_file;
    let file = load_runner_file(&path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No runner registered for {api_url} on this machine. Run 'nexus dispatch runner register'."
        )
    })?;
    Ok((path, file))
}

pub(crate) fn runner_client(file: &RunnerFile) -> anyhow::Result<NexusClient> {
    Ok(NexusClient::new(&file.api_url, Some(file.token.clone()))?)
}

/// `nexus dispatch runner status`: every runner on this machine, one
/// heartbeat each; the runner of `api_url` (this workspace's hub) first.
pub async fn status(api_url: &str, json: bool) -> anyhow::Result<()> {
    let mut runners = all_runners()?;
    if runners.is_empty() {
        anyhow::bail!(
            "No runner registered on this machine. Run 'nexus dispatch runner register'."
        );
    }
    runners.sort_by_key(|(_, f)| !same_hub(&f.api_url, api_url));
    let probe = probe_claude();
    let mut out = vec![];
    for (i, (_, file)) in runners.iter().enumerate() {
        let current = same_hub(&file.api_url, api_url);
        let hb = match runner_client(file) {
            Ok(client) => client
                .runner_heartbeat(serde_json::json!({ "cli_version": env!("CARGO_PKG_VERSION") }))
                .await
                .map_err(|e| super::dispatch_status::error_text(&e)),
            Err(e) => Err(e.to_string()),
        };
        if json {
            let mut v = serde_json::json!({
                "runner_id": file.runner_id,
                "name": file.name,
                "api_url": file.api_url,
                "current_hub": current,
                "token_prefix": file.token_prefix,
                "token_expires_at": file.token_expires_at,
            });
            match &hb {
                Ok(hb) => {
                    v["paused"] = hb.paused.into();
                    v["platform_enabled"] = hb.platform_enabled.into();
                    v["projects"] = serde_json::to_value(&hb.projects)?;
                }
                Err(e) => v["error"] = e.clone().into(),
            }
            out.push(v);
            continue;
        }
        if i > 0 {
            println!();
        }
        let state = match &hb {
            Ok(hb) if hb.paused => style("paused").yellow().to_string(),
            Ok(_) => style("active").green().to_string(),
            Err(_) => style("unreachable").red().to_string(),
        };
        println!(
            "{} {} ({}), {}{}",
            style("Dispatch manager").bold(),
            file.runner_id,
            file.name,
            state,
            if current {
                ", this workspace's hub"
            } else {
                ""
            }
        );
        println!(
            "  hub {}, token {}…{}",
            file.api_url,
            shown_prefix(file.token_prefix.as_deref()),
            file.token_expires_at
                .as_deref()
                .map(|e| format!(", expires {e}"))
                .unwrap_or_default()
        );
        match &hb {
            Ok(hb) => print_bindings(file, &hb.projects, hb.platform_enabled),
            Err(e) => println!("  {} {e}", style("!").red().bold()),
        }
    }
    if json {
        let doc = serde_json::json!({
            "schema": "nexus.dispatch-runners.v1",
            "claude_code": {
                "version": probe.version,
                "cross_session_inbox": probe.inbox.is_ok(),
            },
            "runners": out,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        println!();
        print_probe(&probe);
    }
    Ok(())
}

/// `nexus dispatch runner pause | resume | revoke`.
pub async fn control(api_url: &str, action: &str) -> anyhow::Result<()> {
    let (path, file) = require_runner(api_url)?;
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
    fn test_prefix_display() {
        assert_eq!(shown_prefix(Some("WlnNZnn7")), "nxs_run_WlnNZnn7");
        assert_eq!(shown_prefix(Some("nxs_run_ab")), "nxs_run_ab");
        assert_eq!(shown_prefix(None), "nxs_run_");
    }

    #[test]
    fn test_notify_mode_reads_as_read_only() {
        let file: RunnerFile = serde_json::from_value(serde_json::json!({
            "runner_id": "r", "token": "t", "api_url": "u", "name": "n",
            "registered_at": "2026-01-01T00:00:00Z", "projects": [],
            "project_names": {"p1": "Beta"}
        }))
        .unwrap();
        let binding = |mode: &str, by: &str| RunnerBinding {
            project_id: "p1".into(),
            mode_requested: Some("notify".into()),
            mode_effective: Some(mode.into()),
            blocked_by: Some(by.into()),
            ..Default::default()
        };
        let lines = binding_lines(
            &file,
            &[
                binding("notify", "mode_notify"),
                binding("off", "platform_disabled"),
            ],
        );
        assert_eq!(lines[0], "Beta mode notify (requested notify), read only");
        assert_eq!(
            lines[1],
            "Beta mode off (requested notify), blocked by platform_disabled"
        );
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

    const PROD: &str = "https://nexus.example";
    const STAGING: &str = "https://staging-release-0-20-0--nexus.example.app/";

    fn runner(api_url: &str, id: &str) -> String {
        serde_json::json!({
            "runner_id": id, "token": "t", "api_url": api_url, "name": "n",
            "registered_at": "2026-11-11T00:00:00Z", "projects": []
        })
        .to_string()
    }

    #[test]
    fn test_one_runner_file_per_hub() {
        assert_eq!(hub_slug(PROD), "nexus.example");
        assert_eq!(hub_slug("https://Nexus.Example:443/api"), "nexus.example");
        assert_eq!(
            hub_slug(STAGING),
            "staging-release-0-20-0--nexus.example.app"
        );
        assert_eq!(hub_slug("http://localhost:3000"), "http_localhost_3000");
        assert!(same_hub(PROD, "https://nexus.example/"));
        assert!(!same_hub(PROD, STAGING));
        let dir = Path::new("/c");
        let prod = hub_paths_in(dir, PROD);
        let staging = hub_paths_in(dir, STAGING);
        assert_eq!(
            prod.runner_file,
            Path::new("/c/dispatch-runners/nexus.example.json")
        );
        assert_eq!(
            prod.hub_dir,
            Path::new("/c/state/dispatch/hubs/nexus.example")
        );
        assert_ne!(prod.runner_file, staging.runner_file);
        assert_ne!(prod.hub_dir, staging.hub_dir);
        // Notices and sessions stay shared.
        assert_eq!(prod.state_dir, staging.state_dir);
    }

    #[test]
    fn test_the_single_runner_file_moves_to_its_hub() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path();
        write_private(&c.join(LEGACY_RUNNER_FILE), &runner(STAGING, "r-staging")).unwrap();
        let old_state = c.join("state/dispatch/runner.json");
        fs::create_dir_all(old_state.parent().unwrap()).unwrap();
        fs::write(&old_state, r#"{"runner_id":"r-staging","cursor":42}"#).unwrap();

        assert!(migrate_legacy(c).unwrap());
        let paths = hub_paths_in(c, STAGING);
        let moved = load_runner_file(&paths.runner_file).unwrap().unwrap();
        assert_eq!(moved.runner_id, "r-staging");
        assert!(fs::read_to_string(paths.hub_dir.join("runner.json"))
            .unwrap()
            .contains("\"cursor\":42"));
        assert!(!c.join(LEGACY_RUNNER_FILE).exists());
        assert!(!old_state.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.runner_file)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing left to move; a second hub's runner sits next to it.
        assert!(!migrate_legacy(c).unwrap());
        write_private(&hub_paths_in(c, PROD).runner_file, &runner(PROD, "r-prod")).unwrap();
        let ids: Vec<String> = fs::read_dir(c.join(RUNNERS_DIR))
            .unwrap()
            .flatten()
            .filter_map(|e| load_runner_file(&e.path()).ok().flatten())
            .map(|f| f.runner_id)
            .collect();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_migration_keeps_a_runner_already_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path();
        let paths = hub_paths_in(c, PROD);
        write_private(&paths.runner_file, &runner(PROD, "r-new")).unwrap();
        write_private(&c.join(LEGACY_RUNNER_FILE), &runner(PROD, "r-old")).unwrap();
        assert!(migrate_legacy(c).unwrap());
        let kept = load_runner_file(&paths.runner_file).unwrap().unwrap();
        assert_eq!(kept.runner_id, "r-new");
        assert!(!c.join(LEGACY_RUNNER_FILE).exists());
    }

    #[test]
    fn test_hubs_have_separate_locks() {
        let dir = tempfile::tempdir().unwrap();
        let prod = hub_paths_in(dir.path(), PROD).hub_dir.join("runner.lock");
        let staging = hub_paths_in(dir.path(), STAGING)
            .hub_dir
            .join("runner.lock");
        let a = super::super::dispatch_listen::lock(&prod).unwrap();
        let b = super::super::dispatch_listen::lock(&staging).unwrap();
        // The same hub still allows one listener only.
        assert!(super::super::dispatch_listen::lock(&prod).is_err());
        drop((a, b));
    }
}
