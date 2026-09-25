//! `nexus status | diff | push | reset` over the shared workspace
//! classification (NEXUS-APP dispatch b5f7bfb0). `nexus pull` stays the only
//! command that materializes the workspace; `reset` only restores the named
//! (or listed) files to what the backend / next pull wants.

use std::fs;
use std::path::Path;

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;

use super::workspace_state::{self, Entry, FileClass, Kind, State, WorkspaceState};

/// Resolve workspace, project and client for the linked project.
fn connect(
    api_url: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<(std::path::PathBuf, String, NexusClient)> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    Ok((
        workspace,
        project_id,
        NexusClient::new(api_url, Some(token))?,
    ))
}

fn label(text: &str, state: State) -> console::StyledObject<String> {
    let text = format!("{text:<9}");
    match state {
        State::Update | State::Create | State::New | State::Adopt => style(text).green(),
        State::Stale => style(text).dim(),
        _ => style(text).yellow(),
    }
}

fn state_label(state: State) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn class_label(class: FileClass) -> &'static str {
    match class {
        FileClass::Content => "content",
        FileClass::Projection => "projection",
    }
}

// ---------------------------------------------------------------------------
// nexus status
// ---------------------------------------------------------------------------

/// `nexus status`: auth/project (as before), then executioner and run
/// target, git identity, and every non-clean file with its class, state and
/// next action. Returns the exit code: 0 clean, 1 pending.
pub async fn status(api_url: &str, api_url_source: &str, json: bool) -> anyhow::Result<i32> {
    if !json {
        super::auth::status(api_url, api_url_source).await?;
    }

    let linked = config::load_linked_project(None).ok().flatten();
    if linked.is_none() || resolve_token().is_none() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "api_url": api_url,
                    "project_id": linked.map(|p| p.id),
                    "authenticated": resolve_token().is_some(),
                    "pending": false,
                }))?
            );
        }
        return Ok(0);
    }
    let (workspace, project_id, client) = connect(api_url, None)?;
    let state = match workspace_state::load(&client, &project_id, &workspace).await {
        Ok(s) => s,
        Err(e) => {
            if json {
                return Err(e);
            }
            println!();
            println!(
                "  Files:    {} could not classify the workspace: {}",
                style("!").bold().yellow(),
                e
            );
            return Ok(1);
        }
    };
    let pending = state.pending().count();

    if json {
        let files: Vec<_> = state
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "path": e.path,
                    "class": class_label(e.class),
                    "state": state_label(e.state),
                    "next": e.next_action(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_url": api_url,
                "project_id": project_id,
                "executioner": state.export.agent_owner,
                "run_target": state.export.run_target,
                "environment": state.environment_label(),
                "claude_code": state.is_claude.then(|| super::claude_cmd::runtime_details(
                    &workspace,
                    &super::claude_render::read_claude_settings(&workspace),
                    state.export.ccx.as_ref().and_then(|c| c.compatibility.claude_code.as_deref()),
                    false,
                )),
                "files": files,
                "pending": pending > 0,
            }))?
        );
        return Ok(i32::from(pending > 0));
    }

    println!(
        "  Environment: {} (start with {})",
        style(state.environment_label()).bold(),
        style("nexus run").bold()
    );
    println!();

    let compat = state
        .export
        .ccx
        .as_ref()
        .and_then(|c| c.compatibility.claude_code.as_deref());
    if state.is_claude {
        super::claude_cmd::runtime_details(
            &workspace,
            &super::claude_render::read_claude_settings(&workspace),
            compat,
            true,
        );
    }

    if let Ok(detail) = client.get_project(&project_id).await {
        let git_config = detail.project.git_config.as_ref();
        let gh = detail.project.gh_effective.as_ref();
        if git_config.is_some() || gh.is_some() {
            super::git::run_verify(&workspace, git_config, gh);
            println!();
        }
    }

    print_entries(&state);
    Ok(i32::from(pending > 0))
}

fn print_entries(state: &WorkspaceState) {
    if state.entries.is_empty() {
        println!("{} Workspace is clean.", style("OK").bold().green());
        return;
    }
    println!("{}", style("Workspace files").bold());
    println!();
    for e in &state.entries {
        println!(
            "  {} {:<10} {}",
            label(&state_label(e.state), e.state),
            class_label(e.class),
            e.path
        );
        println!("  {:<9} {:<10} {}", "", "", style(e.next_action()).dim());
    }
    println!();
    let pending = state.pending().count();
    if pending == 0 {
        println!(
            "{} No pending changes (see the stale projection note above).",
            style("OK").bold().green()
        );
    } else {
        println!(
            "{} {} pending change(s). Details: {}",
            style("!").bold().yellow(),
            pending,
            style("nexus diff [path]").bold()
        );
    }
}

// ---------------------------------------------------------------------------
// nexus diff
// ---------------------------------------------------------------------------

/// `nexus diff [path]`: unified diffs (local vs backend / desired) for every
/// non-clean entry, or the ones at or below `path`. Read-only. Returns the
/// exit code (1 when differences exist).
pub async fn diff(api_url: &str, path: Option<&str>) -> anyhow::Result<i32> {
    let (workspace, project_id, client) = connect(api_url, None)?;
    let state = workspace_state::load(&client, &project_id, &workspace).await?;
    let entries: Vec<&Entry> = match path {
        Some(p) => state.matching(p).collect(),
        None => state.entries.iter().collect(),
    };

    let mut shown = 0;
    for e in entries.iter().filter(|e| e.state != State::Stale) {
        print!("{}", entry_diff(e));
        shown += 1;
    }
    if shown == 0 {
        println!("{} No differences.", style("OK").bold().green());
    }
    Ok(i32::from(shown > 0))
}

/// Diff for one entry. Content: the backend version is the base, so local
/// edits show as `+`. Projection: local is the base, and `+` is what the
/// next pull writes.
fn entry_diff(e: &Entry) -> String {
    let local = e.local.as_deref().unwrap_or("");
    let desired = e.desired.as_deref().unwrap_or("");
    let local_label = format!(
        "{} (local, {} {})",
        e.path,
        class_label(e.class),
        state_label(e.state)
    );
    match e.class {
        FileClass::Content => unified_diff(
            desired,
            local,
            &format!("{} (backend)", e.path),
            &local_label,
        ),
        FileClass::Projection => {
            unified_diff(local, desired, &local_label, &format!("{} (nexus)", e.path))
        }
    }
}

pub(crate) fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let out = diff
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string();
    if out.is_empty() {
        // Identical content with a pending lock-only change (ADOPT).
        format!("--- {old_label}\n+++ {new_label}\n(content identical)\n")
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// nexus push
// ---------------------------------------------------------------------------

/// Why a path cannot be pushed, if it is a projection file.
fn refuse_projection(e: &Entry) -> Option<String> {
    if e.class != FileClass::Projection {
        return None;
    }
    let setting = workspace_state::projection_setting(&e.path, &e.kind);
    Some(match setting {
        Some(key) => format!(
            "{} is generated from project settings ({key}); change it with nexus env set {key} <value>, or discard the local edit with nexus reset {}",
            e.path, e.path
        ),
        None => format!(
            "{} is generated by Nexus and cannot be pushed; discard the local edit with nexus reset {}{}",
            e.path,
            e.path,
            if e.kind == Kind::Generated && e.path.contains("/skills/") {
                " (skills are managed in the Nexus dashboard)"
            } else {
                ""
            }
        ),
    })
}

/// `nexus push [path]`: content only. Agent files go through the agent-file
/// sync (`af_sync` push); devbox changes become a workspace fork (as before,
/// including `--name`, `--dry-run`, `--adopt-local`). Projection files are
/// refused with the setting to change instead.
pub async fn push(
    api_url: &str,
    cli_project_id: Option<&str>,
    path: Option<&str>,
    fork_name: Option<&str>,
    dry_run: bool,
    adopt_local: bool,
) -> anyhow::Result<()> {
    if adopt_local {
        return super::push::run(api_url, cli_project_id, fork_name, dry_run, true).await;
    }
    let (workspace, project_id, client) = connect(api_url, cli_project_id)?;
    let state = workspace_state::load(&client, &project_id, &workspace).await?;

    let selected: Vec<&Entry> = match path {
        Some(p) => {
            let matched: Vec<&Entry> = state.matching(p).collect();
            if let Some(reason) = matched.iter().find_map(|e| refuse_projection(e)) {
                anyhow::bail!(reason);
            }
            matched
        }
        None => state
            .pending()
            .filter(|e| e.class == FileClass::Content)
            .collect(),
    };

    let agent_files: Vec<(&Entry, &str)> = selected
        .iter()
        .filter(|e| matches!(e.state, State::Modified))
        .filter_map(|e| match &e.kind {
            Kind::AgentFile { file_key } => Some((*e, file_key.as_str())),
            _ => None,
        })
        .collect();
    let workspace_changes = selected
        .iter()
        .any(|e| e.kind == Kind::Workspace && matches!(e.state, State::Modified | State::New));
    for e in selected.iter().filter(|e| e.state == State::Conflict) {
        println!(
            "   {} {} changed locally and in the backend; run nexus diff {}, then edit or nexus reset it",
            style("!").bold().yellow(),
            e.path,
            e.path
        );
    }

    if agent_files.is_empty() && !workspace_changes {
        println!("{} Nothing to push.", style("OK").bold().green());
        return Ok(());
    }

    for (e, file_key) in agent_files {
        if dry_run {
            println!(
                "   {} would push {} ({})",
                style("~").cyan(),
                e.path,
                file_key
            );
        } else {
            super::sync::push(api_url, Some(&project_id), file_key).await?;
        }
    }
    if workspace_changes {
        super::push::run(api_url, Some(&project_id), fork_name, dry_run, false).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// nexus reset
// ---------------------------------------------------------------------------

/// `nexus reset [path]`: discard local changes. Content returns to the
/// backend version; projection files are restored to what the next pull
/// writes (a targeted `pull --force`). Without a path, every pending entry
/// is reset after one confirmation (`-y` skips it).
pub async fn reset(
    api_url: &str,
    cli_project_id: Option<&str>,
    path: Option<&str>,
    assume_yes: bool,
) -> anyhow::Result<()> {
    let (workspace, project_id, client) = connect(api_url, cli_project_id)?;
    let state = workspace_state::load(&client, &project_id, &workspace).await?;
    let selected: Vec<&Entry> = match path {
        Some(p) => state.matching(p).collect(),
        None => state.pending().collect(),
    };
    let selected: Vec<&Entry> = selected
        .into_iter()
        .filter(|e| !matches!(e.state, State::Stale | State::Adopt))
        .collect();

    if selected.is_empty() {
        println!("{} Nothing to reset.", style("OK").bold().green());
        return Ok(());
    }
    if path.is_none() && !assume_yes {
        println!("   The following local changes will be discarded:");
        for e in &selected {
            println!("      {} {}", state_label(e.state), e.path);
        }
        print!("   {} Continue? [y/N] ", style("?").bold().cyan());
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("   {} Reset cancelled.", style("--").yellow());
            return Ok(());
        }
    }

    for e in selected {
        match reset_entry(&state, e)? {
            Some(note) => println!("   {} {}: {}", style("--").yellow(), e.path, note),
            None => println!("   {} {} restored", style("~").bold().blue(), e.path),
        }
    }
    Ok(())
}

/// Restore one entry. Returns a note when it could not be reset.
fn reset_entry(state: &WorkspaceState, e: &Entry) -> anyhow::Result<Option<String>> {
    let ws = &state.workspace;
    let root = &state.agentic_root;
    match (&e.kind, e.state) {
        (_, State::Unmanaged) => Ok(Some(
            "not managed by Nexus; left as is (nexus pull --force-unmanaged replaces it)".into(),
        )),
        (Kind::Workspace, State::New) => Ok(Some(
            "not known to the backend; push it with nexus push, or delete it manually".into(),
        )),
        (Kind::AgentFile { file_key }, _) => {
            let body = e.desired.as_deref().unwrap_or_default();
            write_file(ws, &e.path, body)?;
            super::sync::update_manifest_after_pull(
                ws,
                file_key,
                &e.path,
                &nexus_core::hash::sha256_hex_normalized(body),
            )?;
            Ok(None)
        }
        (Kind::Workspace, _) => match e.desired.as_deref() {
            Some(body) => {
                write_file(ws, &e.path, body)?;
                super::sync::update_manifest_after_pull(ws, &e.path, &e.path, &sha256_hex(body))?;
                Ok(None)
            }
            None => Ok(Some(
                "the backend workspace export is unavailable; run nexus pull --force".into(),
            )),
        },
        (Kind::Ccx, _) => match e.desired.as_deref() {
            Some(body) => {
                write_file(ws, &e.path, body)?;
                let file_key = state
                    .export
                    .agent_files
                    .iter()
                    .find(|af| af.target_path == e.path)
                    .map(|af| af.file_key.clone())
                    .unwrap_or_default();
                super::ccx::set_lock_file_entry(
                    ws,
                    root,
                    &e.path,
                    Some(super::ccx::CcxLockFileEntry {
                        file_key,
                        sha256: sha256_hex(body),
                    }),
                )?;
                Ok(None)
            }
            None => {
                let _ = fs::remove_file(ws.join(&e.path));
                super::ccx::set_lock_file_entry(ws, root, &e.path, None)?;
                Ok(None)
            }
        },
        (Kind::Generated, _) => {
            let files = [(e.path.clone(), e.desired.clone().unwrap_or_default())];
            super::pull::sync_generated_files(ws, root, &files, true)?;
            Ok(None)
        }
        (Kind::ClaudeMdBlock, _) => {
            let outcome = super::claude_render::merge_claude_md_managed_block(
                ws,
                state.export.claude_md_managed_block.as_deref(),
                None,
                true,
            )?;
            if let super::claude_render::ClaudeMdOutcome::Written(sha)
            | super::claude_render::ClaudeMdOutcome::Unchanged(sha) = outcome
            {
                super::ccx::record_claude_md_block_in_lock(ws, root, &sha)?;
            }
            Ok(None)
        }
        (Kind::SettingsKey(key), _) => {
            let mut settings = super::claude_render::read_claude_settings(ws);
            match super::ccx::json_get_path(&state.settings_after, key).cloned() {
                Some(v) => super::ccx::json_set_path(&mut settings, key, v),
                None => super::ccx::json_remove_path(&mut settings, key),
            }
            write_file(
                ws,
                ".claude/settings.json",
                &(serde_json::to_string_pretty(&settings)? + "\n"),
            )?;
            Ok(None)
        }
        (Kind::Stale, _) => Ok(Some("stale projection; remove manually".into())),
    }
}

fn write_file(workspace: &Path, rel: &str, content: &str) -> anyhow::Result<()> {
    super::pull::validate_agent_file_target_path(workspace, rel)?;
    let target = workspace.join(rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, class: FileClass, state: State, kind: Kind) -> Entry {
        Entry {
            path: path.into(),
            class,
            state,
            kind,
            local: Some("local\n".into()),
            desired: Some("desired\n".into()),
        }
    }

    #[test]
    fn test_entry_diff_direction() {
        // Content: local additions appear as `+` against the backend base.
        let e = Entry {
            path: "devbox.json".into(),
            class: FileClass::Content,
            state: State::Modified,
            kind: Kind::Workspace,
            local: Some("a\nmine\n".into()),
            desired: Some("a\n".into()),
        };
        let out = entry_diff(&e);
        assert!(out.starts_with("--- devbox.json (backend)"), "{out}");
        assert!(out.contains("\n+mine"), "{out}");
        // Projection: the next pull's content appears as `+`.
        let e = Entry {
            class: FileClass::Projection,
            state: State::Drifted,
            kind: Kind::Ccx,
            ..e
        };
        let out = entry_diff(&e);
        assert!(out.contains("+++ devbox.json (nexus)"), "{out}");
        assert!(out.contains("\n-mine"), "{out}");
    }

    #[test]
    fn test_push_refuses_projection_with_setting() {
        let e = entry(
            ".claude/statusline/nexus-hud.mjs",
            FileClass::Projection,
            State::Drifted,
            Kind::Ccx,
        );
        let msg = refuse_projection(&e).unwrap();
        assert!(msg.contains("nexus env set claude.hud"), "{msg}");
        let e = entry(
            ".nexus/skills/nx-a/SKILL.md",
            FileClass::Projection,
            State::Drifted,
            Kind::Generated,
        );
        assert!(refuse_projection(&e).unwrap().contains("nexus reset"));
        let e = entry(
            ".nexus/AGENTS.md",
            FileClass::Content,
            State::Modified,
            Kind::Workspace,
        );
        assert!(refuse_projection(&e).is_none());
    }

    fn state_with(entries: Vec<Entry>, dir: &Path) -> WorkspaceState {
        WorkspaceState {
            workspace: dir.to_path_buf(),
            agentic_root: ".nexus".into(),
            is_claude: true,
            export: serde_json::from_value(serde_json::json!({
                "project_id": "p", "project_name": "n", "count": 1,
                "agent_files": [{
                    "file_key": "AGENTS.md", "target_path": ".nexus/AGENTS.md", "name": "a",
                    "category": "general", "version": 1, "body": "desired\n",
                    "content_hash": "h-from-server"
                }]
            }))
            .unwrap(),
            entries,
            settings_after: serde_json::json!({"statusLine": {"command": "hud"}}),
        }
    }

    fn tmp(suffix: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-state-cmd-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_reset_content_agent_file_restores_backend_and_manifest() {
        let dir = tmp("reset-af");
        fs::create_dir_all(dir.join(".nexus")).unwrap();
        fs::write(dir.join(".nexus/AGENTS.md"), "local\n").unwrap();
        let e = entry(
            ".nexus/AGENTS.md",
            FileClass::Content,
            State::Modified,
            Kind::AgentFile {
                file_key: "AGENTS.md".into(),
            },
        );
        let state = state_with(vec![e.clone()], &dir);
        assert!(reset_entry(&state, &e).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(dir.join(".nexus/AGENTS.md")).unwrap(),
            "desired\n"
        );
        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert_eq!(manifest["AGENTS.md"]["hash"], sha256_hex("desired\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reset_ccx_file_updates_lock() {
        let dir = tmp("reset-ccx");
        let e = entry(
            ".claude/rules/10.md",
            FileClass::Projection,
            State::Drifted,
            Kind::Ccx,
        );
        let state = state_with(vec![e.clone()], &dir);
        reset_entry(&state, &e).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(".claude/rules/10.md")).unwrap(),
            "desired\n"
        );
        let lock = super::super::ccx::load_lock(&dir, ".nexus").unwrap();
        assert_eq!(
            lock.files[".claude/rules/10.md"].sha256,
            sha256_hex("desired\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reset_settings_key_applies_desired_value_only() {
        let dir = tmp("reset-settings");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{"statusLine": {"command": "mine"}, "model": "keep"}"#,
        )
        .unwrap();
        let e = entry(
            ".claude/settings.json#statusLine",
            FileClass::Projection,
            State::Drifted,
            Kind::SettingsKey("statusLine".into()),
        );
        let state = state_with(vec![e.clone()], &dir);
        reset_entry(&state, &e).unwrap();
        let settings = super::super::claude_render::read_claude_settings(&dir);
        assert_eq!(settings["statusLine"]["command"], "hud");
        assert_eq!(settings["model"], "keep");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reset_new_workspace_file_is_not_deleted() {
        let dir = tmp("reset-new");
        let e = entry(
            "scripts/devbox/new.sh",
            FileClass::Content,
            State::New,
            Kind::Workspace,
        );
        let state = state_with(vec![e.clone()], &dir);
        assert!(reset_entry(&state, &e).unwrap().is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reset_rejects_traversal() {
        let dir = tmp("reset-traversal");
        let e = entry(
            "../escape.md",
            FileClass::Content,
            State::Modified,
            Kind::AgentFile {
                file_key: "x".into(),
            },
        );
        let state = state_with(vec![e.clone()], &dir);
        assert!(reset_entry(&state, &e).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
