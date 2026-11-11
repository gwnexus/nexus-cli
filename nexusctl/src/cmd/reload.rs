//! `nexus reload`: bring backend changes into a running workspace.
//!
//! Runs `nexus pull` and the managed plugin step while the agent session
//! and the terminal workspace keep running, then says per area what the
//! running Claude Code session picks up by itself and what needs an action:
//!
//! - skills, subagents, `.claude/settings*.json`: picked up live;
//! - plugins: `/reload-plugins` in the session;
//! - `CLAUDE.md`, `AGENTS.md`, rules, directives: read at session start, so
//!   the session re-reads them (or `/compact`, or a restart with
//!   `claude --continue`);
//! - `.mcp.json`: a restart with `claude --continue`;
//! - devbox workspace and terminal layout: the next `nexus run`.
//!
//! Running Claude Code sessions of this repository get the same summary
//! through their cross-session inbox, unless they hold or refuse inbound
//! messages or `--no-notify` is given.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use console::style;
use nexus_core::config::McpSource;

use super::claude_plugins::{self, InstalledPlugin, SystemClaude};
use super::dispatch_listen::{self, Inbound};

/// A part of the workspace and how a running session takes its changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Area {
    Skills,
    Agents,
    Settings,
    Plugins,
    Memory,
    Mcp,
    Workspace,
    Layout,
}

impl Area {
    fn label(self) -> &'static str {
        match self {
            Area::Skills => "skills and commands",
            Area::Agents => "subagents",
            Area::Settings => "Claude Code settings",
            Area::Plugins => "plugins",
            Area::Memory => "CLAUDE.md, agent files and rules",
            Area::Mcp => "MCP servers (.mcp.json)",
            Area::Workspace => "devbox workspace",
            Area::Layout => "terminal layout",
        }
    }

    /// What the operator does for the running session.
    fn action(self) -> &'static str {
        match self {
            Area::Skills | Area::Agents | Area::Settings => "active now",
            Area::Plugins => "run /reload-plugins in the Claude Code session",
            Area::Memory => {
                "read at session start: the session re-reads them, or run /compact, or restart with claude --continue"
            }
            Area::Mcp => "restart the session: Ctrl-C, then claude --continue",
            Area::Workspace => "takes effect with the next devbox shell (nexus run)",
            Area::Layout => "takes effect with the next nexus run",
        }
    }
}

/// The area a workspace-relative path belongs to, if reload watches it.
pub(crate) fn area_of(path: &str, agentic_root: &str) -> Option<Area> {
    let in_dir = |dir: &str| path.starts_with(&format!("{dir}/"));
    if in_dir(".claude/skills") || in_dir(".claude/commands") || in_dir(".opencode/commands") {
        return Some(Area::Skills);
    }
    if in_dir(".claude/agents") {
        return Some(Area::Agents);
    }
    if path == ".claude/settings.json"
        || path == ".claude/settings.local.json"
        || in_dir(".claude/statusline")
    {
        return Some(Area::Settings);
    }
    if path == "CLAUDE.md"
        || path == "AGENTS.md"
        || in_dir(".claude/rules")
        || path == format!("{agentic_root}/AGENTS.md")
        || path == format!("{agentic_root}/directives.md")
    {
        return Some(Area::Memory);
    }
    if path == ".mcp.json" || path == "opencode.json" {
        return Some(Area::Mcp);
    }
    if path == "devbox.json" || in_dir("scripts/devbox") {
        return Some(Area::Workspace);
    }
    if path.starts_with(&format!("{agentic_root}/claude/")) && path.ends_with(".kdl") {
        return Some(Area::Layout);
    }
    None
}

/// Hashes of the watched files, by workspace-relative path.
pub(crate) fn snapshot(workspace: &Path, agentic_root: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let roots = [
        ".claude",
        ".opencode",
        "scripts/devbox",
        agentic_root,
        "CLAUDE.md",
        "AGENTS.md",
        ".mcp.json",
        "opencode.json",
        "devbox.json",
    ];
    for root in roots {
        collect(workspace, &workspace.join(root), agentic_root, &mut out);
    }
    out
}

fn collect(workspace: &Path, path: &Path, agentic_root: &str, out: &mut BTreeMap<String, String>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                collect(workspace, &e.path(), agentic_root, out);
            }
        }
        return;
    }
    let Ok(rel) = path.strip_prefix(workspace) else {
        return;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if area_of(&rel, agentic_root).is_none() {
        return;
    }
    if let Ok(bytes) = std::fs::read(path) {
        out.insert(
            rel,
            nexus_core::hash::sha256_hex(&String::from_utf8_lossy(&bytes)),
        );
    }
}

/// Areas whose files were added, changed or removed between two snapshots.
pub(crate) fn changed_areas(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    agentic_root: &str,
) -> BTreeMap<Area, Vec<String>> {
    let paths: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut out: BTreeMap<Area, Vec<String>> = BTreeMap::new();
    for p in paths {
        if before.get(p) != after.get(p) {
            if let Some(area) = area_of(p, agentic_root) {
                out.entry(area).or_default().push(p.clone());
            }
        }
    }
    out
}

/// Plugin versions this workspace sees: user scope and this project.
fn plugin_versions(workspace: &Path) -> BTreeMap<String, String> {
    let cli = SystemClaude {
        workspace: workspace.to_path_buf(),
    };
    let here = workspace.display().to_string();
    claude_plugins::list_installed(&cli)
        .unwrap_or_default()
        .into_iter()
        .filter(|p: &InstalledPlugin| {
            p.scope.as_deref() == Some("user") || p.project_path.as_deref() == Some(here.as_str())
        })
        .map(|p| (p.id, p.version.unwrap_or_default()))
        .collect()
}

/// Plugins added or moved to another version, as `id from -> to`.
pub(crate) fn plugin_changes(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    after
        .iter()
        .filter(|(id, v)| before.get(*id) != Some(*v))
        .map(|(id, v)| match before.get(id) {
            Some(old) => format!("{id} {old} -> {v}"),
            None => format!("{id} {v} (new)"),
        })
        .collect()
}

/// The message for running sessions: what changed and what to do. Built
/// from area names and plugin ids only, never from file contents.
pub(crate) fn session_message(areas: &BTreeMap<Area, Vec<String>>) -> Option<String> {
    let mut lines = vec![];
    for (area, items) in areas {
        let line = match area {
            Area::Plugins => format!(
                "- Plugins changed ({}). Ask the operator to run /reload-plugins.",
                items.join(", ")
            ),
            Area::Memory => format!(
                "- Project instructions changed: {}. Read these files again now and follow the new version.",
                items.join(", ")
            ),
            Area::Mcp => "- MCP server configuration changed. It applies after a restart; ask the operator to restart with claude --continue when convenient.".into(),
            Area::Skills | Area::Agents | Area::Settings => format!("- {} changed and are active now.", area.label()),
            Area::Workspace | Area::Layout => continue,
        };
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "Nexus: the operator ran `nexus reload`; this workspace was updated from the Nexus platform.\n{}",
        lines.join("\n")
    ))
}

/// Push `message` into every running Claude Code session of this
/// repository that accepts inbound messages. Returns (reached, skipped).
async fn notify_sessions(workspace: &Path, message: &str) -> (usize, usize) {
    if super::dispatch_runner::probe_claude().inbox.is_err() {
        return (0, 0);
    }
    let Ok(dir) = nexus_core::config::Config::dir() else {
        return (0, 0);
    };
    let sessions_dir = dir.join("state").join("dispatch").join("sessions");
    notify_sessions_in(&sessions_dir, &dirs_home(), workspace, message).await
}

/// [`notify_sessions`] against an explicit sessions directory and home.
pub(crate) async fn notify_sessions_in(
    sessions_dir: &Path,
    home: &Path,
    workspace: &Path,
    message: &str,
) -> (usize, usize) {
    let root = super::shadow_guard::repo_root(workspace).unwrap_or(workspace.to_path_buf());
    let hash = super::dispatch_runner::repo_path_hash(&root);
    let (mut reached, mut skipped) = (0, 0);
    for s in dispatch_listen::sessions(sessions_dir) {
        let session_root = dispatch_listen::session_root(&s);
        if super::dispatch_runner::repo_path_hash(&session_root) != hash {
            continue;
        }
        let policy =
            dispatch_listen::inbound_policy(&dispatch_listen::inbound_sources(&session_root, home));
        if matches!(policy, Inbound::Hold | Inbound::Refuse) {
            skipped += 1;
            continue;
        }
        match dispatch_listen::push_frames(&s, dispatch_listen::user_frames(&s.token, message))
            .await
        {
            Ok(()) => reached += 1,
            Err(_) => skipped += 1,
        }
    }
    (reached, skipped)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub struct ReloadOptions<'a> {
    pub project_id: Option<&'a str>,
    pub force: bool,
    pub force_unmanaged: bool,
    pub trust: bool,
    pub notify: bool,
    pub mcp_source: McpSource,
}

/// `nexus reload`.
pub async fn run(api_url: &str, opts: ReloadOptions<'_>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let agentic_root = super::run::resolve_agentic_root(&workspace);
    let before = snapshot(&workspace, &agentic_root);
    let plugins_before = plugin_versions(&workspace);

    super::pull::run(
        api_url,
        opts.project_id,
        opts.force,
        opts.mcp_source,
        &[],
        false,
        super::ccx::ForceMode::from_flags(opts.force, opts.force_unmanaged),
        false,
        opts.trust,
        false,
    )
    .await?;
    // The plugin step `nexus run` does before a launch; pull runs it only
    // with --force.
    if !opts.force {
        let spec = claude_plugins::locked_spec(&workspace, &agentic_root);
        claude_plugins::sync_workspace(&workspace, &agentic_root, spec.as_ref());
    }

    let mut areas = changed_areas(&before, &snapshot(&workspace, &agentic_root), &agentic_root);
    let plugins = plugin_changes(&plugins_before, &plugin_versions(&workspace));
    if !plugins.is_empty() {
        areas.insert(Area::Plugins, plugins);
    }

    println!();
    println!("{}", style("Reload").bold());
    if areas.is_empty() {
        println!(
            "  {} nothing changed; the running session is up to date.",
            style("OK").green().bold()
        );
        return Ok(());
    }
    for (area, items) in &areas {
        let marker = match area {
            Area::Skills | Area::Agents | Area::Settings => style("live").green().bold(),
            _ => style("todo").yellow().bold(),
        };
        println!(
            "  {marker} {} ({}): {}",
            area.label(),
            items.len(),
            area.action()
        );
    }

    if !opts.notify {
        return Ok(());
    }
    if let Some(message) = session_message(&areas) {
        match notify_sessions(&workspace, &message).await {
            (0, 0) => println!(
                "  {} no running Claude Code session of this repository took the summary",
                style("-").dim()
            ),
            (reached, skipped) => println!(
                "  {} summary sent to {reached} running session(s){}",
                style(">>").cyan().bold(),
                if skipped > 0 {
                    format!(", {skipped} skipped (inbound held or refused)")
                } else {
                    String::new()
                }
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_area_of() {
        let r = ".nexus";
        assert_eq!(area_of(".claude/skills/x/SKILL.md", r), Some(Area::Skills));
        assert_eq!(area_of(".claude/commands/a.md", r), Some(Area::Skills));
        assert_eq!(area_of(".claude/agents/a.md", r), Some(Area::Agents));
        assert_eq!(area_of(".claude/settings.json", r), Some(Area::Settings));
        assert_eq!(
            area_of(".claude/statusline/hud.mjs", r),
            Some(Area::Settings)
        );
        assert_eq!(area_of("CLAUDE.md", r), Some(Area::Memory));
        assert_eq!(area_of(".claude/rules/15.md", r), Some(Area::Memory));
        assert_eq!(area_of(".nexus/AGENTS.md", r), Some(Area::Memory));
        assert_eq!(area_of(".nexus/directives.md", r), Some(Area::Memory));
        assert_eq!(area_of(".mcp.json", r), Some(Area::Mcp));
        assert_eq!(area_of("devbox.json", r), Some(Area::Workspace));
        assert_eq!(
            area_of("scripts/devbox/dbx_init.sh", r),
            Some(Area::Workspace)
        );
        assert_eq!(
            area_of(".nexus/claude/nexus-claude.kdl", r),
            Some(Area::Layout)
        );
        assert_eq!(area_of(".nexus/config.toml", r), None);
        assert_eq!(area_of(".nexus/credentials.toml", r), None);
        assert_eq!(area_of("src/main.rs", r), None);
    }

    #[test]
    fn test_snapshot_and_changed_areas() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        let write = |p: &str, b: &str| {
            let path = w.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b).unwrap();
        };
        write(".claude/skills/a/SKILL.md", "v1");
        write("CLAUDE.md", "v1");
        write(".mcp.json", "{}");
        write(".nexus/credentials.toml", "secret");
        let before = snapshot(w, ".nexus");
        assert!(!before.contains_key(".nexus/credentials.toml"));
        write(".claude/skills/a/SKILL.md", "v2");
        write(".claude/rules/new.md", "r");
        std::fs::remove_file(w.join(".mcp.json")).unwrap();
        let areas = changed_areas(&before, &snapshot(w, ".nexus"), ".nexus");
        assert_eq!(
            areas.keys().copied().collect::<Vec<_>>(),
            vec![Area::Skills, Area::Memory, Area::Mcp]
        );
        assert_eq!(
            areas[&Area::Memory],
            vec![".claude/rules/new.md".to_string()]
        );
    }

    #[test]
    fn test_plugin_changes() {
        let before: BTreeMap<String, String> =
            [("core@nexus".to_string(), "1.5.1".to_string())].into();
        let after: BTreeMap<String, String> = [
            ("core@nexus".to_string(), "1.5.2".to_string()),
            ("extra@nexus".to_string(), "0.1.0".to_string()),
        ]
        .into();
        assert_eq!(
            plugin_changes(&before, &after),
            vec!["core@nexus 1.5.1 -> 1.5.2", "extra@nexus 0.1.0 (new)"]
        );
        assert!(plugin_changes(&after, &after).is_empty());
    }

    #[test]
    fn test_session_message() {
        let mut areas = BTreeMap::new();
        areas.insert(Area::Workspace, vec!["devbox.json".to_string()]);
        assert_eq!(session_message(&areas), None);
        areas.insert(Area::Plugins, vec!["core@nexus 1.5.1 -> 1.5.2".to_string()]);
        areas.insert(Area::Memory, vec!["CLAUDE.md".to_string()]);
        let m = session_message(&areas).unwrap();
        assert!(m.starts_with("Nexus: the operator ran `nexus reload`"));
        assert!(m.contains("/reload-plugins"));
        assert!(m.contains("Project instructions changed: CLAUDE.md. Read these files again now"));
        assert!(!m.contains("devbox.json"));
    }

    /// A temp dir short enough for a Unix socket path.
    fn short_temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nxr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_summary_reaches_only_accepting_sessions_of_this_repo() {
        let base = short_temp("ns");
        let (repo, other) = (base.join("repo"), base.join("other"));
        let sessions = base.join("sessions");
        for d in [&repo, &other, &sessions] {
            std::fs::create_dir_all(d).unwrap();
        }
        let listener = tokio::net::UnixListener::bind(base.join("a.sock")).unwrap();
        let got = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).await.unwrap();
            buf
        });
        let other_sock = tokio::net::UnixListener::bind(base.join("b.sock")).unwrap();
        let session = |id: &str, cwd: &Path, sock: &str| {
            std::fs::write(
                sessions.join(format!("{id}.json")),
                serde_json::json!({
                    "session_id": id, "cwd": cwd.display().to_string(),
                    "socket": base.join(sock).display().to_string(), "token": "tok"
                })
                .to_string(),
            )
            .unwrap();
        };
        session("s-repo", &repo, "a.sock");
        session("s-other", &other, "b.sock");
        // A session file whose socket is gone is ignored.
        session("s-gone", &repo, "gone.sock");

        let home = base.join("home");
        let (reached, skipped) = notify_sessions_in(&sessions, &home, &repo, "Nexus: hi").await;
        assert_eq!((reached, skipped), (1, 0));
        assert_eq!(
            got.await.unwrap(),
            dispatch_listen::user_frames("tok", "Nexus: hi")
        );
        drop(other_sock);

        // The session holds inbound messages: nothing is pushed.
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(
            repo.join(".claude/settings.local.json"),
            r#"{"crossSessionInbound":"hold"}"#,
        )
        .unwrap();
        let _again = tokio::net::UnixListener::bind(base.join("c.sock")).unwrap();
        session("s-repo", &repo, "c.sock");
        let (reached, skipped) = notify_sessions_in(&sessions, &home, &repo, "Nexus: hi").await;
        assert_eq!((reached, skipped), (0, 1));
        let _ = std::fs::remove_dir_all(&base);
    }
}
