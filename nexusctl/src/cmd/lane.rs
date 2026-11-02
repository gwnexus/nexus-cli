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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub actors: Vec<ClaudeLaneActor>,
}

fn cache_path(workspace: &Path, agentic_root: &str) -> PathBuf {
    workspace.join(agentic_root).join(LANE_FILE)
}

pub(crate) fn load_cache(workspace: &Path, agentic_root: &str) -> Option<LaneCache> {
    let raw = fs::read_to_string(cache_path(workspace, agentic_root)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// What [`sync_cache`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheSync {
    Written(usize),
    Removed,
    Unchanged,
}

/// Write the lane actors of an export to the cache, or remove the cache
/// when the export has none.
pub(crate) fn sync_cache(
    workspace: &Path,
    agentic_root: &str,
    export: &AgentFileExportResponse,
) -> anyhow::Result<CacheSync> {
    let path = cache_path(workspace, agentic_root);
    let actors = export.claude_lane_actors.clone().unwrap_or_default();
    if actors.is_empty() {
        if path.is_file() {
            fs::remove_file(&path)?;
            return Ok(CacheSync::Removed);
        }
        return Ok(CacheSync::Unchanged);
    }
    let cache = LaneCache {
        base_url: export
            .claude_lane_base_url
            .clone()
            .filter(|u| !u.trim().is_empty()),
        actors,
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

/// The `env` block of the actor's settings file. Every model tier points
/// at the actor's profile so background tasks stay on the lane.
fn lane_env(actor: &ClaudeLaneActor, base_url: &str, token: &str) -> serde_json::Value {
    let mut env = serde_json::json!({
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_AUTH_TOKEN": token,
        "ANTHROPIC_MODEL": actor.profile_slug,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": actor.profile_slug,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": actor.profile_slug,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": actor.profile_slug,
        "CLAUDE_CODE_SUBAGENT_MODEL": actor.profile_slug,
        "ANTHROPIC_CUSTOM_HEADERS": format!("x-nexus-actor: {}", actor.slug),
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
) -> Vec<String> {
    let mut args = vec![
        "--settings".to_string(),
        settings.display().to_string(),
        "--model".to_string(),
        actor.profile_slug.clone(),
    ];
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

fn write_settings(dir: &Path, slug: &str, env: &serde_json::Value) -> anyhow::Result<SettingsFile> {
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
    let path = dir.join(format!("{slug}-{}.json", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let guard = SettingsFile(path.clone());
    let mut file = opts.open(&path)?;
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
    let cached = load_cache(workspace, agentic_root);
    if cached
        .as_ref()
        .is_some_and(|c| c.actors.iter().any(|a| a.slug == slug))
    {
        return cached;
    }
    let client = NexusClient::new(api_url, Some(resolve_token()?)).ok()?;
    let export = client.export_agent_files(project_id).await.ok()?;
    let _ = sync_cache(workspace, agentic_root, &export);
    load_cache(workspace, agentic_root).or(cached)
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

    let settings = write_settings(&settings_dir()?, slug, &lane_env(actor, &base_url, &token))?;
    if prompt.is_none() {
        println!(
            "{} Actor {}: model {} via {} ({source}), token {}",
            style(">>").bold().cyan(),
            style(slug).bold(),
            actor.profile_slug,
            base_url,
            mask_token(&token)
        );
    }
    let mut cmd = std::process::Command::new("claude");
    cmd.args(claude_args(actor, &settings.0, prompt, extra));
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
    let actors = load_cache(workspace, agentic_root)
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
    let Some(cache) = load_cache(workspace, &agentic_root).filter(|c| !c.actors.is_empty()) else {
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
    let token = config::resolve_project_id(None, Some(workspace))
        .ok()
        .and_then(|id| resolve_project_token(&id));
    rows.push((
        "lane token",
        match &token {
            Some(t) => CheckResult::Pass(format!("project token {}", mask_token(t))),
            None => CheckResult::Warn("no project token; run nexus project link".into()),
        },
    ));
    let env_url = std::env::var(GATEWAY_URL_ENV).ok();
    let Some((base, source)) =
        resolve_base_url(None, env_url.as_deref(), cache.base_url.as_deref())
    else {
        rows.push((
            "lane gateway",
            CheckResult::Warn(format!(
                "no gateway URL from the backend; set {GATEWAY_URL_ENV}"
            )),
        ));
        return rows;
    };
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
                let missing: Vec<&str> = cache
                    .actors
                    .iter()
                    .map(|a| a.profile_slug.as_str())
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
        let env = lane_env(&a, "https://gw.example.invalid", "nxs_proj_x");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gw.example.invalid");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "nxs_proj_x");
        for k in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            assert_eq!(env[k], "local-coder-main", "{k}");
        }
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"],
            "x-nexus-actor: spark-coder"
        );
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
        assert!(env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none());
        a.context_window = Some(131_072);
        let env = lane_env(&a, "u", "t");
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "131072");
    }

    #[test]
    fn test_claude_args_keep_the_token_off_the_command_line() {
        let a = actor("spark-coder", "local-coder-main");
        let args = claude_args(
            &a,
            Path::new("/s/x.json"),
            Some("fix it"),
            &["--verbose".into()],
        );
        assert_eq!(
            &args[..4],
            ["--settings", "/s/x.json", "--model", "local-coder-main"]
        );
        assert!(args.windows(2).any(|w| w == ["-p", "fix it"]));
        assert_eq!(args.last().unwrap(), "--verbose");
        assert!(args.iter().all(|a| !a.contains("nxs_proj")));
        let interactive = claude_args(&a, Path::new("/s/x.json"), None, &[]);
        assert!(!interactive.contains(&"-p".to_string()));
    }

    #[test]
    fn test_settings_file_is_private_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let env = serde_json::json!({"ANTHROPIC_AUTH_TOKEN": "nxs_proj_x"});
        let path = {
            let f = write_settings(dir.path(), "spark-coder", &env).unwrap();
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
    fn test_model_ids() {
        let body =
            serde_json::json!({"data": [{"id": "local-coder-main"}, {"id": "local-review"}]});
        assert_eq!(model_ids(&body), ["local-coder-main", "local-review"]);
        assert!(model_ids(&serde_json::json!({})).is_empty());
    }
}
