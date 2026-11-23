// nexusctl/src/cmd/lane.rs
//
// Claude Code local lane. Actors on a gateway route run as their own
// `claude` process against the Nexus gateway's Anthropic Messages surface,
// authenticated with the project inference token; the main session keeps
// its subscription. Claude Code allows one base URL per process, so the
// split happens per process, not per subagent.
//
// `nexus pull` caches the lane actors in `<agentic_root>/generated/
// claude-lane.json`; `nexus actors run`, `nexus run --actor-panes` and
// `nexus doctor claude` read it.

use std::fs;
use std::path::{Path, PathBuf};

use console::style;
use nexus_core::api::{AgentFileExportResponse, ClaudeLaneActor, NexusClient};
use nexus_core::auth::{mask_token, resolve_project_token, resolve_token};
use nexus_core::config;
use serde::{Deserialize, Serialize};

use super::preflight::CheckResult;

/// Lane cache, relative to the agentic root.
const LANE_FILE: &str = "generated/claude-lane.json";

/// Env override for the gateway base URL.
const GATEWAY_URL_ENV: &str = "NEXUS_GATEWAY_URL";

/// Credentials and provider switches removed from the actor process, so
/// nothing but the project token can authenticate it.
const SCRUBBED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaneCache {
    /// The project the cache was written for; a cache of another project
    /// (after a relink) is never used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub actors: Vec<ClaudeLaneActor>,
}

fn cache_path(workspace: &Path, agentic_root: &str) -> PathBuf {
    workspace.join(agentic_root).join(LANE_FILE)
}

/// The lane cache `nexus pull` wrote. A copy tracked by git is ignored: it
/// comes from the repository, and its `base_url` decides where the project
/// inference token is sent.
pub(crate) fn load_cache(workspace: &Path, agentic_root: &str) -> Option<LaneCache> {
    let path = cache_path(workspace, agentic_root);
    let raw = fs::read_to_string(&path).ok()?;
    if config::trust::is_git_tracked(&path) {
        return None;
    }
    serde_json::from_str(&raw).ok()
}

/// The lane cache when it belongs to the workspace's linked project; a cache
/// from before a relink (or one without a project id) is ignored.
fn load_project_cache(workspace: &Path, agentic_root: &str) -> Option<LaneCache> {
    let project_id = config::resolve_project_id(None, Some(workspace)).ok()?;
    load_cache(workspace, agentic_root).filter(|c| c.project_id.as_deref() == Some(&project_id))
}

/// The lane cache content of an export, `None` without lane actors.
fn cache_from_export(export: &AgentFileExportResponse) -> Option<LaneCache> {
    let actors = export.claude_lane_actors.clone().unwrap_or_default();
    (!actors.is_empty()).then(|| LaneCache {
        project_id: Some(export.project_id.clone()).filter(|id| !id.is_empty()),
        base_url: export
            .claude_lane_base_url
            .clone()
            .filter(|u| !u.trim().is_empty()),
        actors,
    })
}

/// What [`sync_cache`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheSync {
    Written(usize),
    Removed,
    Unchanged,
    /// Left alone: outside the workspace, through a symlink, or tracked.
    Refused(&'static str),
}

/// Write the lane actors of an export to the cache, or remove the cache
/// when the export has none.
pub(crate) fn sync_cache(
    workspace: &Path,
    agentic_root: &str,
    export: &AgentFileExportResponse,
) -> anyhow::Result<CacheSync> {
    let path = cache_path(workspace, agentic_root);
    // The same rules as every other pulled file, and a tracked cache (which
    // the reader ignores) is never overwritten or removed.
    let rel = format!("{agentic_root}/{LANE_FILE}");
    if super::pull::validate_agent_file_target_path(workspace, &rel).is_err() {
        return Ok(CacheSync::Refused(
            "outside the workspace or through a symlink",
        ));
    }
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(CacheSync::Refused("a symlink"));
    }
    // Tracked even when deleted from the working tree; an indeterminate
    // answer does not block a fresh cache.
    if config::trust::git_tracking(&path) == Some(true) {
        return Ok(CacheSync::Refused("tracked by git"));
    }
    let Some(cache) = cache_from_export(export) else {
        if path.is_file() {
            fs::remove_file(&path)?;
            return Ok(CacheSync::Removed);
        }
        return Ok(CacheSync::Unchanged);
    };
    let content = serde_json::to_string_pretty(&cache)? + "\n";
    if fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
        return Ok(CacheSync::Unchanged);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(CacheSync::Written(cache.actors.len()))
}

/// Print the outcome of [`sync_cache`] in the pull output.
pub(crate) fn print_sync(agentic_root: &str, outcome: CacheSync) {
    match outcome {
        CacheSync::Written(n) => println!(
            "   {} {agentic_root}/{LANE_FILE} ({n} local-lane actor(s), start with nexus actors run <slug>)",
            style("+").bold().green()
        ),
        CacheSync::Removed => println!(
            "   {} {agentic_root}/{LANE_FILE} (no local-lane actors)",
            style("-").red()
        ),
        CacheSync::Unchanged => {}
        CacheSync::Refused(reason) => println!(
            "   {} {agentic_root}/{LANE_FILE} not updated ({reason})",
            style("!").bold().yellow()
        ),
    }
}

/// `https://gw.example/v1/` -> `https://gw.example`: Claude Code appends
/// `/v1/messages` itself.
fn normalize_base_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    url.strip_suffix("/v1").unwrap_or(url).to_string()
}

/// The gateway base URL: `--gateway-url`, then `NEXUS_GATEWAY_URL`, then
/// what the backend sent (`claude_lane_base_url`). Returns the URL and
/// where it came from.
fn resolve_base_url(
    flag: Option<&str>,
    env: Option<&str>,
    cached: Option<&str>,
) -> Option<(String, &'static str)> {
    fn pick(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    pick(flag)
        .map(|u| (u, "--gateway-url"))
        .or_else(|| pick(env).map(|u| (u, GATEWAY_URL_ENV)))
        .or_else(|| pick(cached).map(|u| (u, "backend")))
        .map(|(u, src)| (normalize_base_url(u), src))
}

/// Whether the project token may be sent to `url`: HTTPS, or plain HTTP to
/// a loopback host (a local gateway).
fn gateway_url_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    // Credentials in the URL would be printed and logged with it.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    match parsed.scheme() {
        "https" => parsed.host_str().is_some(),
        "http" => matches!(
            parsed.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]")
        ),
        _ => false,
    }
}

/// `url` for display: user name and password replaced by `***`.
fn display_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
            let _ = parsed.set_username("***");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        // `user:pw@host` without a scheme parses as an opaque URL whose
        // "scheme" is the user name: no user info to mask, so hide it all.
        Ok(parsed) if parsed.cannot_be_a_base() && url.contains('@') => "<invalid URL>".to_string(),
        Ok(_) => url.to_string(),
        // Unparseable: show it only when it cannot hide credentials.
        Err(_) if url.contains('@') => "<invalid URL>".to_string(),
        Err(_) => url.to_string(),
    }
}

/// Gateway model id of a profile: `nexus/<profile_slug>`.
fn gateway_model(profile_slug: &str) -> String {
    if profile_slug.starts_with("nexus/") {
        profile_slug.to_string()
    } else {
        format!("nexus/{profile_slug}")
    }
}

/// Upper bound for a session id sent as `x-nexus-session-id`.
const SESSION_ID_MAX_LEN: usize = 128;

/// Whether `id` may go into the `x-nexus-session-id` header: the charset
/// the gateway accepts (ASCII alphanumerics and `-_.:`), bounded length.
fn session_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SESSION_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// `staging/release-1.2.3` -> `1.2.3`; `None` for any other branch.
fn release_line_of(branch: &str) -> Option<&str> {
    let line = branch.trim().strip_prefix("staging/release-")?;
    let parts: Vec<&str> = line.split('.').collect();
    (parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
    .then_some(line)
}

/// The checked-out branch of `workspace`, `None` outside git or detached.
fn current_branch(workspace: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
}

/// The open session of `user_id` in a `session_list` answer (bare or in a
/// `data` envelope). With a release line, only a session of that line
/// counts; without one, a session outside any release line is preferred.
fn pick_session(
    list: &serde_json::Value,
    user_id: &str,
    release_line: Option<&str>,
) -> Option<String> {
    let sessions = list
        .get("sessions")
        .or_else(|| list.get("data").and_then(|d| d.get("sessions")))?
        .as_array()?;
    let str_of =
        |s: &serde_json::Value, k: &str| s.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let mine: Vec<&serde_json::Value> = sessions
        .iter()
        .filter(|s| str_of(s, "status").is_none_or(|st| st == "open"))
        .filter(|s| str_of(s, "created_by").as_deref() == Some(user_id))
        .filter(|s| str_of(s, "id").is_some_and(|id| session_id_valid(&id)))
        .collect();
    let line_of = |s: &serde_json::Value| str_of(s, "release_line").filter(|l| !l.is_empty());
    let chosen = match release_line {
        Some(line) => mine.iter().find(|s| line_of(s).as_deref() == Some(line)),
        None => mine
            .iter()
            .find(|s| line_of(s).is_none())
            .or_else(|| mine.first()),
    };
    chosen.and_then(|s| str_of(s, "id"))
}

/// The open Nexus session of the current user for this project (of the
/// release line on a `staging/release-X.Y.Z` branch). Best effort, bounded
/// to a few seconds: `None` when there is none or the hub is unreachable.
async fn resolve_session_id(api_url: &str, workspace: &Path, project_id: &str) -> Option<String> {
    let lookup = async {
        let client = NexusClient::new(api_url, Some(resolve_token()?)).ok()?;
        let branch = current_branch(workspace);
        let line = branch.as_deref().and_then(release_line_of);
        let user_id = client.get_identity().await.ok()?.user_id;
        let list = client.list_sessions(project_id, line).await.ok()?;
        pick_session(&list, &user_id, line)
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), lookup)
        .await
        .ok()
        .flatten()
}

/// The `env` block of the actor's settings file. Every model tier points
/// at the actor's profile so background tasks stay on the lane. With a
/// session id, gateway usage is attributed to that Nexus session; the env
/// is fixed at process start, so a session opened later is not picked up.
fn lane_env(
    actor: &ClaudeLaneActor,
    base_url: &str,
    token: &str,
    session_id: Option<&str>,
) -> serde_json::Value {
    let model = gateway_model(&actor.profile_slug);
    let mut headers = format!("x-nexus-actor: {}", actor.slug);
    if let Some(id) = session_id.filter(|id| session_id_valid(id)) {
        headers.push_str(&format!("\nx-nexus-session-id: {id}"));
    }
    let mut env = serde_json::json!({
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_AUTH_TOKEN": token,
        "ANTHROPIC_MODEL": model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
        "CLAUDE_CODE_SUBAGENT_MODEL": model,
        "ANTHROPIC_CUSTOM_HEADERS": headers,
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    });
    if let Some(window) = actor.context_window.filter(|w| *w > 0) {
        env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] = window.to_string().into();
    }
    env
}

/// Arguments for `claude` (the settings file carries the credentials).
fn claude_args(
    actor: &ClaudeLaneActor,
    settings: &Path,
    prompt: Option<&str>,
    extra: &[String],
    neutral_agent: bool,
) -> Vec<String> {
    let mut args = vec![
        "--settings".to_string(),
        settings.display().to_string(),
        "--model".to_string(),
        gateway_model(&actor.profile_slug),
    ];
    if neutral_agent
        && !extra
            .iter()
            .any(|a| a == "--agent" || a.starts_with("--agent="))
    {
        // An empty prompt keeps the default system prompt; no `model` or
        // `tools` means the session's own apply.
        let agents = serde_json::json!({
            LANE_AGENT: {"description": "Nexus local lane actor", "prompt": ""}
        });
        args.extend([
            "--agents".to_string(),
            agents.to_string(),
            "--agent".to_string(),
            LANE_AGENT.to_string(),
        ]);
    }
    if !actor.body.trim().is_empty() {
        args.push("--append-system-prompt".into());
        args.push(actor.body.trim().to_string());
    }
    if let Some(p) = prompt {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.extend(extra.iter().cloned());
    args
}

/// Neutral agent a lane session starts under when a settings file names a
/// main-thread `agent`; otherwise that agent's prompt and model would
/// replace the actor's.
const LANE_AGENT: &str = "nexus-lane";

/// Whether a Claude settings file in scope (project, local, user) sets
/// `agent`.
fn main_agent_configured(workspace: &Path, home: Option<&Path>) -> bool {
    let mut files = vec![
        workspace.join(".claude/settings.json"),
        workspace.join(".claude/settings.local.json"),
    ];
    files.extend(home.map(|h| h.join(".claude/settings.json")));
    files.iter().any(|f| {
        fs::read_to_string(f)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("agent")
                    .and_then(|a| a.as_str())
                    .map(|a| !a.trim().is_empty())
            })
            .unwrap_or(false)
    })
}

/// Directory for the per-run settings files (0700).
fn settings_dir() -> anyhow::Result<PathBuf> {
    let dir = config::Config::dir()?.join("state").join("lane");
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Settings file holding the token; removed when dropped.
struct SettingsFile(PathBuf);

impl Drop for SettingsFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_settings(dir: &Path, env: &serde_json::Value) -> anyhow::Result<SettingsFile> {
    use std::io::Write as _;
    // Files of runs that were killed before they could clean up.
    if let Ok(entries) = fs::read_dir(dir) {
        let day = std::time::Duration::from_secs(86_400);
        for e in entries.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if stale {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    // The name does not depend on the actor slug, and the file is created
    // exclusively (owner-only before the token is written).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("run-{}-{nanos}.json", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    // Armed only once the file is ours, so a name clash never deletes
    // someone else's file.
    let guard = SettingsFile(path.clone());
    file.write_all(serde_json::to_string(&serde_json::json!({ "env": env }))?.as_bytes())?;
    Ok(guard)
}

/// The cached lane actors, refreshed from the backend when the cache is
/// missing or does not know `slug`.
async fn lane_for(
    api_url: &str,
    workspace: &Path,
    agentic_root: &str,
    project_id: &str,
    slug: &str,
) -> Option<LaneCache> {
    let cached =
        load_cache(workspace, agentic_root).filter(|c| c.project_id.as_deref() == Some(project_id));
    if cached
        .as_ref()
        .is_some_and(|c| c.actors.iter().any(|a| a.slug == slug))
    {
        return cached;
    }
    let client = NexusClient::new(api_url, Some(resolve_token()?)).ok()?;
    let export = client.export_agent_files(project_id).await.ok()?;
    let _ = sync_cache(workspace, agentic_root, &export);
    load_cache(workspace, agentic_root)
        .filter(|c| c.project_id.as_deref() == Some(project_id))
        .or_else(|| cache_from_export(&export))
        .or(cached)
}

/// `nexus actors run <slug> [-p <prompt>] [-- <claude args>]`.
pub async fn run(
    api_url: &str,
    slug: &str,
    prompt: Option<&str>,
    gateway_url: Option<&str>,
    extra: &[String],
) -> anyhow::Result<i32> {
    let workspace = std::env::current_dir()?;
    let agentic_root = super::run::resolve_agentic_root(&workspace);
    let project_id = config::resolve_project_id(None, Some(&workspace))?;
    let cache = lane_for(api_url, &workspace, &agentic_root, &project_id, slug)
        .await
        .unwrap_or_default();
    let Some(actor) = cache.actors.iter().find(|a| a.slug == slug) else {
        let known: Vec<&str> = cache.actors.iter().map(|a| a.slug.as_str()).collect();
        anyhow::bail!(
            "'{slug}' is not a local-lane actor of this project{}",
            if known.is_empty() {
                " (the project has none; run nexus pull after assigning one)".to_string()
            } else {
                format!(" (local-lane actors: {})", known.join(", "))
            }
        );
    };
    let env_url = std::env::var(GATEWAY_URL_ENV).ok();
    let Some((base_url, source)) =
        resolve_base_url(gateway_url, env_url.as_deref(), cache.base_url.as_deref())
    else {
        anyhow::bail!(
            "no gateway URL for the local lane: the backend did not send one; \
             pass --gateway-url or set {GATEWAY_URL_ENV}"
        );
    };
    if !gateway_url_allowed(&base_url) {
        anyhow::bail!(
            "refusing to send the project token to {} ({source}): the gateway URL \
             must use https (http only for localhost) and carry no credentials",
            display_url(&base_url)
        );
    }
    // Never fall back to a claude.ai login: without the project token the
    // process would run on the subscription.
    let token = resolve_project_token(&project_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no project inference token for this project; run nexus project link first \
             (the local lane never uses a claude.ai login)"
        )
    })?;
    nexus_core::auth::validate_project_token_format(&token)?;
    if !super::run::on_path("claude") {
        anyhow::bail!("claude not found on PATH (nexus doctor claude --fix)");
    }

    let session_id = resolve_session_id(api_url, &workspace, &project_id).await;
    if session_id.is_none() {
        eprintln!(
            "{} no open Nexus session: gateway usage of this run is not attributed to a session \
             (open one, then restart the actor)",
            style("Nexus:").dim()
        );
    }
    let env = lane_env(actor, &base_url, &token, session_id.as_deref());
    let settings = write_settings(&settings_dir()?, &env)?;
    if prompt.is_none() {
        println!(
            "{} Actor {}: model {} via {} ({source}), token {}",
            style(">>").bold().cyan(),
            style(slug).bold(),
            gateway_model(&actor.profile_slug),
            display_url(&base_url),
            mask_token(&token)
        );
    }
    let mut cmd = std::process::Command::new("claude");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let neutral_agent = main_agent_configured(&workspace, home.as_deref());
    cmd.args(claude_args(
        actor,
        &settings.0,
        prompt,
        extra,
        neutral_agent,
    ));
    for key in SCRUBBED_ENV {
        cmd.env_remove(key);
    }
    let mut child = cmd.spawn()?;
    guard_signals(child.id());
    let status = child.wait()?;
    drop(settings);
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// While the actor runs, this process must outlive it to remove the
/// settings file: Ctrl-C and Ctrl-\ belong to claude (same process group),
/// SIGTERM and SIGHUP are passed on to it.
#[cfg(unix)]
fn guard_signals(pid: u32) {
    CHILD_PID.store(pid as i32, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: installs plain handlers; the child was spawned before, so its
    // own signal dispositions are unaffected.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        let handler = forward_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

#[cfg(not(unix))]
fn guard_signals(_pid: u32) {}

/// KDL string literal.
fn kdl_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The layout with an `actors` tab: one pane per actor running
/// `nexus actors run <slug>`. `None` when the layout has no closing brace.
fn layout_with_actor_panes(
    layout: &str,
    actors: &[ClaudeLaneActor],
    nexus: &str,
) -> Option<String> {
    let end = layout.rfind('}')?;
    let mut tab =
        String::from("    tab name=\"actors\" {\n        pane split_direction=\"vertical\" {\n");
    for a in actors {
        tab.push_str(&format!(
            "            pane name={} command={} {{\n                args \"actors\" \"run\" {}\n            }}\n",
            kdl_string(&a.slug),
            kdl_string(nexus),
            kdl_string(&a.slug)
        ));
    }
    tab.push_str("        }\n    }\n");
    let head = layout[..end].trim_end();
    Some(format!("{head}\n{tab}{}", &layout[end..]))
}

/// `nexus run --actor-panes`: point the zellij `--layout` argument at a
/// copy of the layout with an `actors` tab. Returns a status line.
pub(crate) fn apply_actor_panes(
    workspace: &Path,
    agentic_root: &str,
    leading_args: &mut [String],
) -> anyhow::Result<CheckResult> {
    let actors = load_project_cache(workspace, agentic_root)
        .map(|c| c.actors)
        .unwrap_or_default();
    if actors.is_empty() {
        return Ok(CheckResult::Info(
            "no local-lane actors in this project (run nexus pull)".into(),
        ));
    }
    let Some(i) = leading_args.iter().position(|a| a == "--layout") else {
        return Ok(CheckResult::Warn("no zellij layout to extend".into()));
    };
    let Some(layout_path) = leading_args.get(i + 1).cloned() else {
        return Ok(CheckResult::Warn("no zellij layout to extend".into()));
    };
    let nexus = std::env::current_exe()?.display().to_string();
    let Some(kdl) = fs::read_to_string(&layout_path)
        .ok()
        .and_then(|l| layout_with_actor_panes(&l, &actors, &nexus))
    else {
        return Ok(CheckResult::Warn(format!("could not extend {layout_path}")));
    };
    let dir = config::Config::dir()?.join("state").join("layouts");
    fs::create_dir_all(&dir)?;
    let name = nexus_core::hash::sha256_hex(&workspace.display().to_string());
    let path = dir.join(format!("{}.kdl", &name[..16]));
    fs::write(&path, kdl)?;
    leading_args[i + 1] = path.display().to_string();
    Ok(CheckResult::Pass(format!(
        "actors tab: {}",
        actors
            .iter()
            .map(|a| a.slug.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Why a gateway request failed, with the route hint for connect errors.
fn describe_error(err: &reqwest::Error) -> String {
    if err.is_connect() || err.is_timeout() {
        format!("not reachable ({err}); check the WireGuard or LAN route to the gateway")
    } else {
        err.to_string()
    }
}

/// Model IDs from an OpenAI- or Anthropic-style `/v1/models` body.
fn model_ids(body: &serde_json::Value) -> Vec<String> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Local-lane rows for `nexus doctor claude`; empty without lane actors.
pub(crate) async fn doctor_checks(workspace: &Path) -> Vec<(&'static str, CheckResult)> {
    let agentic_root = super::run::resolve_agentic_root(workspace);
    let Some(cache) = load_project_cache(workspace, &agentic_root).filter(|c| !c.actors.is_empty())
    else {
        return Vec::new();
    };
    let mut rows = vec![(
        "lane actors",
        CheckResult::Info(
            cache
                .actors
                .iter()
                .map(|a| format!("{} ({})", a.slug, a.profile_slug))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    )];
    let cache_base = cache.base_url.as_deref();
    let token = cache.project_id.as_deref().and_then(resolve_project_token);
    // Only a well-formed project token is ever sent to the gateway.
    let token_ok = token
        .as_deref()
        .is_some_and(|t| nexus_core::auth::validate_project_token_format(t).is_ok());
    rows.push((
        "lane token",
        match &token {
            Some(t) if token_ok => CheckResult::Pass(format!("project token {}", mask_token(t))),
            Some(t) => CheckResult::Fail(format!(
                "{} is not a project token (nxs_proj_*); check NEXUS_PROJECT_TOKEN",
                mask_token(t)
            )),
            None => CheckResult::Warn("no project token; run nexus project link".into()),
        },
    ));
    let token = token.filter(|_| token_ok);
    let env_url = std::env::var(GATEWAY_URL_ENV).ok();
    let Some((base, source)) = resolve_base_url(None, env_url.as_deref(), cache_base) else {
        rows.push((
            "lane gateway",
            CheckResult::Warn(format!(
                "no gateway URL from the backend; set {GATEWAY_URL_ENV}"
            )),
        ));
        return rows;
    };
    if !gateway_url_allowed(&base) {
        rows.push((
            "lane gateway",
            CheckResult::Fail(format!(
                "{} ({source}): the gateway URL must use https (http only for localhost) \
                 and carry no credentials",
                display_url(&base)
            )),
        ));
        return rows;
    }
    let Ok(http) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    else {
        return rows;
    };
    let health = http.get(format!("{base}/health/ready")).send().await;
    let reachable = matches!(&health, Ok(r) if r.status().is_success());
    rows.push((
        "lane gateway",
        match health {
            Ok(r) if r.status().is_success() => CheckResult::Pass(format!("{base} ({source})")),
            Ok(r) => CheckResult::Warn(format!("{base}: /health/ready returned {}", r.status())),
            Err(e) => CheckResult::Warn(format!("{base}: {}", describe_error(&e))),
        },
    ));
    let (true, Some(token)) = (reachable, token) else {
        return rows;
    };
    let models = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(&token)
        .send()
        .await;
    rows.push((
        "lane models",
        match models {
            Ok(r) if r.status().is_success() => {
                let ids = r
                    .json::<serde_json::Value>()
                    .await
                    .map(|b| model_ids(&b))
                    .unwrap_or_default();
                let missing: Vec<String> = cache
                    .actors
                    .iter()
                    .map(|a| gateway_model(&a.profile_slug))
                    .filter(|p| !ids.iter().any(|id| id == p))
                    .collect();
                if missing.is_empty() {
                    CheckResult::Pass("all actor profiles listed".into())
                } else {
                    CheckResult::Warn(format!("not listed by the gateway: {}", missing.join(", ")))
                }
            }
            Ok(r) => CheckResult::Warn(format!("/v1/models returned {}", r.status())),
            Err(e) => CheckResult::Warn(describe_error(&e)),
        },
    ));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(slug: &str, profile: &str) -> ClaudeLaneActor {
        ClaudeLaneActor {
            slug: slug.into(),
            profile_slug: profile.into(),
            body: "# Coder\n\nWrites code.".into(),
            context_window: None,
        }
    }

    fn export(actors: Option<Vec<ClaudeLaneActor>>, url: Option<&str>) -> AgentFileExportResponse {
        let mut e: AgentFileExportResponse = serde_json::from_value(serde_json::json!({
            "project_id": "p",
            "project_name": "p",
            "agent_files": [],
            "count": 0,
        }))
        .unwrap();
        e.claude_lane_actors = actors;
        e.claude_lane_base_url = url.map(str::to_string);
        e
    }

    #[test]
    fn test_gateway_url_must_be_https_or_loopback() {
        assert!(gateway_url_allowed("https://gw.example.invalid"));
        assert!(gateway_url_allowed("https://gw.example.invalid:8443"));
        assert!(gateway_url_allowed("http://localhost:4000"));
        assert!(gateway_url_allowed("http://127.0.0.1:4000"));
        assert!(!gateway_url_allowed("http://gw.example.invalid"));
        assert!(!gateway_url_allowed("http://localhost.evil.invalid"));
        assert!(!gateway_url_allowed("ftp://gw.example.invalid"));
        assert!(!gateway_url_allowed("not a url"));
        // Credentials in the URL are refused and never displayed.
        assert!(!gateway_url_allowed("https://user:pw@gw.example.invalid"));
        assert!(!gateway_url_allowed("https://user@gw.example.invalid"));
        let shown = display_url("https://user:pw@gw.example.invalid/v1");
        assert!(!shown.contains("pw") && !shown.contains("user"), "{shown}");
        assert_eq!(
            display_url("https://gw.example.invalid"),
            "https://gw.example.invalid"
        );
        assert_eq!(display_url("not a url @ secret"), "<invalid URL>");
        assert_eq!(
            display_url("alice:secret@gw.example.invalid"),
            "<invalid URL>"
        );
    }

    #[test]
    fn test_git_tracked_lane_cache_is_ignored() {
        let ws = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(ws.path())
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        let path = ws.path().join(".nexus").join(LANE_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"base_url":"https://attacker.invalid","actors":[]}"#,
        )
        .unwrap();
        assert!(load_cache(ws.path(), ".nexus").is_some());
        git(&["add", "-f", ".nexus/generated/claude-lane.json"]);
        assert!(load_cache(ws.path(), ".nexus").is_none());
    }

    #[test]
    fn test_normalize_and_resolve_base_url() {
        assert_eq!(
            normalize_base_url("https://gw.example.invalid/v1/"),
            "https://gw.example.invalid"
        );
        assert_eq!(
            normalize_base_url(" https://gw.example.invalid:8443 "),
            "https://gw.example.invalid:8443"
        );
        let cached = Some("https://cached.invalid");
        assert_eq!(
            resolve_base_url(
                Some("https://flag.invalid"),
                Some("https://env.invalid"),
                cached
            ),
            Some(("https://flag.invalid".into(), "--gateway-url"))
        );
        assert_eq!(
            resolve_base_url(None, Some("https://env.invalid/v1"), cached),
            Some(("https://env.invalid".into(), GATEWAY_URL_ENV))
        );
        assert_eq!(
            resolve_base_url(Some(" "), None, cached),
            Some(("https://cached.invalid".into(), "backend"))
        );
        assert_eq!(resolve_base_url(None, None, None), None);
    }

    #[test]
    fn test_lane_env_points_every_tier_at_the_profile() {
        let mut a = actor("spark-coder", "local-coder-main");
        let env = lane_env(&a, "https://gw.example.invalid", "nxs_proj_x", None);
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gw.example.invalid");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "nxs_proj_x");
        for k in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            assert_eq!(env[k], "nexus/local-coder-main", "{k}");
        }
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"],
            "x-nexus-actor: spark-coder"
        );
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
        assert!(env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none());
        a.context_window = Some(131_072);
        let env = lane_env(&a, "u", "t", None);
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "131072");
    }

    #[test]
    fn test_lane_env_adds_the_session_header() {
        let a = actor("spark-coder", "local-coder-main");
        let env = lane_env(&a, "u", "t", Some("sess-0001"));
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"],
            "x-nexus-actor: spark-coder\nx-nexus-session-id: sess-0001"
        );
        // An id the gateway would reject is left out, never mangled.
        for bad in [
            "",
            "a b",
            "a\nx-evil: 1",
            &"x".repeat(SESSION_ID_MAX_LEN + 1),
        ] {
            let env = lane_env(&a, "u", "t", Some(bad));
            assert_eq!(
                env["ANTHROPIC_CUSTOM_HEADERS"], "x-nexus-actor: spark-coder",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn test_release_line_of_branch() {
        assert_eq!(release_line_of("staging/release-0.21.0"), Some("0.21.0"));
        assert_eq!(release_line_of("main"), None);
        assert_eq!(release_line_of("staging/release-0.21"), None);
        assert_eq!(release_line_of("staging/release-0.21.x"), None);
        assert_eq!(release_line_of("feature/staging/release-1.0.0"), None);
    }

    #[test]
    fn test_pick_session_by_user_and_release_line() {
        let list = serde_json::json!({ "sessions": [
            {"id": "sess-other-user", "status": "open", "created_by": "user-b", "release_line": null},
            {"id": "sess-line", "status": "open", "created_by": "user-a", "release_line": "0.21.0"},
            {"id": "sess-plain", "status": "open", "created_by": "user-a", "release_line": null},
            {"id": "sess-closed", "status": "closed", "created_by": "user-a", "release_line": null},
        ]});
        assert_eq!(
            pick_session(&list, "user-a", None).as_deref(),
            Some("sess-plain")
        );
        assert_eq!(
            pick_session(&list, "user-a", Some("0.21.0")).as_deref(),
            Some("sess-line")
        );
        assert_eq!(pick_session(&list, "user-a", Some("0.22.0")), None);
        assert_eq!(pick_session(&list, "user-c", None), None);
        // MCP envelope; a session of a release line is used off the line
        // only when the user has no other one.
        let enveloped = serde_json::json!({ "data": { "sessions": [
            {"id": "sess-line", "status": "open", "created_by": "user-a", "release_line": "0.21.0"},
        ]}});
        assert_eq!(
            pick_session(&enveloped, "user-a", None).as_deref(),
            Some("sess-line")
        );
        assert_eq!(pick_session(&serde_json::json!({}), "user-a", None), None);
    }

    #[test]
    fn test_claude_args_keep_the_token_off_the_command_line() {
        let a = actor("spark-coder", "local-coder-main");
        let args = claude_args(
            &a,
            Path::new("/s/x.json"),
            Some("fix it"),
            &["--verbose".into()],
            false,
        );
        assert_eq!(
            &args[..4],
            [
                "--settings",
                "/s/x.json",
                "--model",
                "nexus/local-coder-main"
            ]
        );
        assert!(args.windows(2).any(|w| w == ["-p", "fix it"]));
        assert_eq!(args.last().unwrap(), "--verbose");
        assert!(args.iter().all(|a| !a.contains("nxs_proj")));
        let interactive = claude_args(&a, Path::new("/s/x.json"), None, &[], false);
        assert!(!interactive.contains(&"-p".to_string()));
        assert!(!interactive.contains(&"--agent".to_string()));
    }

    #[test]
    fn test_gateway_model_prefixes_once() {
        assert_eq!(gateway_model("nexus-coder-long"), "nexus/nexus-coder-long");
        assert_eq!(
            gateway_model("nexus/nexus-coder-long"),
            "nexus/nexus-coder-long"
        );
    }

    #[test]
    fn test_main_agent_setting_gets_a_neutral_lane_agent() {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        assert!(!main_agent_configured(ws.path(), Some(home.path())));
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"agent":"main"}"#,
        )
        .unwrap();
        assert!(main_agent_configured(ws.path(), Some(home.path())));
        assert!(!main_agent_configured(ws.path(), None));
        fs::create_dir_all(ws.path().join(".claude")).unwrap();
        fs::write(
            ws.path().join(".claude/settings.json"),
            r#"{"agent":"main"}"#,
        )
        .unwrap();
        assert!(main_agent_configured(ws.path(), None));

        let a = actor("spark-coder", "local-coder-main");
        let args = claude_args(&a, Path::new("/s/x.json"), None, &[], true);
        assert!(args.windows(2).any(|w| w == ["--agent", LANE_AGENT]));
        let agents = args.iter().position(|x| x == "--agents").unwrap();
        let def: serde_json::Value = serde_json::from_str(&args[agents + 1]).unwrap();
        assert_eq!(def[LANE_AGENT]["prompt"], "");
        assert!(def[LANE_AGENT].get("model").is_none());
        // An explicit --agent from the caller wins.
        let own = claude_args(
            &a,
            Path::new("/s/x.json"),
            None,
            &["--agent".into(), "x".into()],
            true,
        );
        assert!(!own.contains(&"--agents".to_string()));
    }

    #[test]
    fn test_settings_file_is_private_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let env = serde_json::json!({"ANTHROPIC_AUTH_TOKEN": "nxs_proj_x"});
        let path = {
            let f = write_settings(dir.path(), &env).unwrap();
            let raw = fs::read_to_string(&f.0).unwrap();
            assert!(raw.contains("\"env\""));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&f.0).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            f.0.clone()
        };
        assert!(!path.exists());
    }

    #[test]
    fn test_sync_cache_writes_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let e = export(
            Some(vec![actor("spark-coder", "local-coder-main")]),
            Some("https://gw.example.invalid"),
        );
        assert_eq!(sync_cache(ws, ".nexus", &e).unwrap(), CacheSync::Written(1));
        assert_eq!(sync_cache(ws, ".nexus", &e).unwrap(), CacheSync::Unchanged);
        let cache = load_cache(ws, ".nexus").unwrap();
        assert_eq!(
            cache.base_url.as_deref(),
            Some("https://gw.example.invalid")
        );
        assert_eq!(cache.actors[0].profile_slug, "local-coder-main");
        // The cache records the project it belongs to.
        assert_eq!(cache.project_id.as_deref(), Some(e.project_id.as_str()));
        // Older backend or no lane actors: the cache goes away.
        assert_eq!(
            sync_cache(ws, ".nexus", &export(None, None)).unwrap(),
            CacheSync::Removed
        );
        assert!(load_cache(ws, ".nexus").is_none());
        assert_eq!(
            sync_cache(ws, ".nexus", &export(Some(vec![]), None)).unwrap(),
            CacheSync::Unchanged
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_sync_cache_refuses_a_linked_root_and_a_tracked_cache() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join(".nexus")).unwrap();
        let e = export(Some(vec![actor("spark-coder", "local-coder-main")]), None);
        assert!(matches!(
            sync_cache(ws, ".nexus", &e).unwrap(),
            CacheSync::Refused(_)
        ));
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());

        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        fs::create_dir_all(cache_path(ws, ".nexus").parent().unwrap()).unwrap();
        fs::write(cache_path(ws, ".nexus"), "{}\n").unwrap();
        let git = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            for var in nexus_core::config::trust::GIT_LOCATION_VARS {
                cmd.env_remove(var);
            }
            assert!(cmd
                .args(args)
                .current_dir(ws)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        git(&["add", "-f", ".nexus"]);
        assert_eq!(
            sync_cache(ws, ".nexus", &export(None, None)).unwrap(),
            CacheSync::Refused("tracked by git")
        );
        assert_eq!(
            fs::read_to_string(cache_path(ws, ".nexus")).unwrap(),
            "{}\n"
        );
        // Deleted from the working tree but still in the index.
        fs::remove_file(cache_path(ws, ".nexus")).unwrap();
        let e = export(Some(vec![actor("spark-coder", "local-coder-main")]), None);
        assert_eq!(
            sync_cache(ws, ".nexus", &e).unwrap(),
            CacheSync::Refused("tracked by git")
        );
        assert!(!cache_path(ws, ".nexus").exists());

        // A symlink at the cache path, even to a file inside the workspace.
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        fs::write(ws.join("README.md"), "readme\n").unwrap();
        fs::create_dir_all(cache_path(ws, ".nexus").parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../../README.md", cache_path(ws, ".nexus")).unwrap();
        assert_eq!(
            sync_cache(ws, ".nexus", &e).unwrap(),
            CacheSync::Refused("a symlink")
        );
        assert_eq!(
            fs::read_to_string(ws.join("README.md")).unwrap(),
            "readme\n"
        );
    }

    #[test]
    fn test_layout_with_actor_panes_adds_a_tab() {
        let layout = "layout {\n    session_name \"beta\"\n    tab name=\"main\" {\n        pane command=\"claude\"\n    }\n}\n";
        let actors = [actor("spark-coder", "a"), actor("spark-review", "b")];
        let out = layout_with_actor_panes(layout, &actors, "/bin/nexus").unwrap();
        assert!(out.starts_with("layout {\n    session_name \"beta\""));
        assert!(out.contains("tab name=\"actors\""));
        assert!(out.contains("pane name=\"spark-review\" command=\"/bin/nexus\""));
        assert!(out.contains("args \"actors\" \"run\" \"spark-coder\""));
        assert!(out.trim_end().ends_with('}'));
        // Balanced braces: the tab sits inside the layout node.
        assert_eq!(out.matches('{').count(), out.matches('}').count());
        assert_eq!(layout_with_actor_panes("no braces", &actors, "n"), None);
    }

    #[test]
    fn test_actor_panes_keep_the_claude_pane_args() {
        let args = "args \"-c\" \"claude --chrome; zellij kill-session $ZELLIJ_SESSION_NAME\"";
        let layout = format!("layout {{\n    tab name=\"CLAUDE\" {{\n        pane command=\"sh\" name=\"claude\" {{\n            {args}\n        }}\n    }}\n}}\n");
        let out =
            layout_with_actor_panes(&layout, &[actor("spark-coder", "a")], "/bin/nexus").unwrap();
        assert!(out.contains(&format!("            {args}\n")), "{out}");
        assert_eq!(out.matches("--chrome").count(), 1);
    }

    #[test]
    fn test_model_ids() {
        let body =
            serde_json::json!({"data": [{"id": "local-coder-main"}, {"id": "local-review"}]});
        assert_eq!(model_ids(&body), ["local-coder-main", "local-review"]);
        assert!(model_ids(&serde_json::json!({})).is_empty());
    }
}
