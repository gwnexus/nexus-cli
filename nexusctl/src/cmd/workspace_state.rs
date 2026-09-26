//! Workspace file classification shared by `nexus status | diff | push |
//! reset | stash` (NEXUS-APP dispatch b5f7bfb0).
//!
//! Every managed file is either
//! - **content**: authored, allowed back into the backend (`nexus push`):
//!   assigned agent files and the devbox workspace (`devbox.json`,
//!   `scripts/devbox/**`); or
//! - **projection**: generated from backend settings, a local edit is
//!   drift (`nexus reset`, or change the setting with `nexus env set`):
//!   CCX files, synthetic/generated agent files, skills and OpenCode
//!   commands rendered by pull, the `CLAUDE.md` managed block and managed
//!   `.claude/settings.json` keys.
//!
//! Unmanaged files (no marker, not in any lock or manifest) are never
//! listed. States are derived with exactly the rules `nexus pull` applies,
//! so the commands never disagree with a pull.

use std::path::{Path, PathBuf};

use nexus_core::api::{AgentFileExportResponse, ExportedAgentFile, NexusClient};
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use serde::Serialize;

use super::ccx::{self, FileState};
use super::claude_render;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileClass {
    Content,
    Projection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum State {
    /// Content edited locally.
    Modified,
    /// Content file not known to the backend yet (new workspace script).
    New,
    /// Content removed locally.
    Deleted,
    /// The backend has a newer version; local is unmodified.
    Update,
    /// Both the local file and the backend changed.
    Conflict,
    /// Projection edited locally.
    Drifted,
    /// Projection file missing locally.
    Create,
    /// Projection file Nexus no longer sends.
    Orphaned,
    /// A local file in the way of a projection file.
    Unmanaged,
    /// Local file already matches; only the lock would be recorded.
    Adopt,
    /// Files of the runtime this project does not use.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// An `af_export` agent file (content, or projection when generated).
    AgentFile { file_key: String },
    /// `devbox.json` or a workspace script.
    Workspace,
    /// A CCX lock-governed file.
    Ccx,
    /// A skill / skill resource / OpenCode command rendered by pull.
    Generated,
    /// The `CLAUDE.md` nexus-managed block.
    ClaudeMdBlock,
    /// A managed `.claude/settings.json` key.
    SettingsKey(String),
    /// Leftovers of the non-selected runtime's projection (removed by the
    /// next `nexus pull`).
    Stale,
}

/// One non-clean file (or settings key / block).
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub class: FileClass,
    pub state: State,
    pub kind: Kind,
    /// Local content (for diffs); `None` if missing.
    pub local: Option<String>,
    /// Content the backend / next pull wants; `None` if it would be removed.
    pub desired: Option<String>,
}

impl Entry {
    /// The next command to run for this entry.
    pub fn next_action(&self) -> String {
        let p = &self.path;
        match (self.class, self.state) {
            (_, State::Stale) => {
                "not used by this project's runtime; run nexus pull to remove it (nexus pull --force also removes modified files)".into()
            }
            (FileClass::Content, State::Modified) => {
                format!("nexus push {p} (or nexus reset {p})")
            }
            (FileClass::Content, State::New) => format!("nexus push {p}"),
            (FileClass::Content, State::Deleted) => format!("nexus reset {p}"),
            (_, State::Conflict) => {
                format!(
                    "nexus diff {p}, then nexus reset {p}{}",
                    self.setting_hint()
                )
            }
            (FileClass::Projection, State::Drifted) => {
                format!("nexus reset {p}{}", self.setting_hint())
            }
            (_, State::Unmanaged) => "keep, or nexus pull --force-unmanaged".into(),
            _ => "nexus pull".into(),
        }
    }

    /// ", or nexus env set <key> ..." for projection files generated from a
    /// known setting.
    fn setting_hint(&self) -> String {
        match projection_setting(&self.path, &self.kind) {
            Some(key) => format!(", or change it with nexus env set {key} <value>"),
            None => String::new(),
        }
    }
}

/// The backend setting a projection file is generated from, if known.
pub fn projection_setting(path: &str, kind: &Kind) -> Option<&'static str> {
    if let Kind::SettingsKey(key) = kind {
        return match key.as_str() {
            "statusLine" => Some("claude.hud"),
            k if k.starts_with("enabledPlugins") => Some("claude.plugins.<name>"),
            _ => None,
        };
    }
    // No hint for `.claude/rules/`: several rules ship with every profile,
    // so `claude.profile` would not remove the drift (dispatch b5f7bfb0).
    if path.starts_with(".claude/statusline/") {
        Some("claude.hud")
    } else if path.ends_with("nexus-claude.kdl") {
        Some("claude.workspace")
    } else {
        None
    }
}

/// Whether an agent file is generated (projection) rather than authored
/// content. Mirrors the keys the backend refuses in `af_sync` push:
/// synthetic files, CCX, actor profiles, `env-nexus-local`, RTK filters, the
/// headroom plugin, and `AGENTS.md` / `CLAUDE.md`, which the backend
/// regenerates on every export (a pushed edit would be lost).
pub fn is_projection_agent_file(
    file_key: &str,
    target_path: &str,
    af: Option<&ExportedAgentFile>,
) -> bool {
    if let Some(af) = af {
        if af.category == ccx::CCX_CATEGORY
            || af
                .agent_file_id
                .as_deref()
                .is_some_and(|id| id.starts_with("synthetic:"))
        {
            return true;
        }
    }
    let file_name = target_path.rsplit('/').next().unwrap_or(target_path);
    file_name.eq_ignore_ascii_case("AGENTS.md")
        || file_name.eq_ignore_ascii_case("CLAUDE.md")
        || file_key.eq_ignore_ascii_case("agents.md")
        || file_key.eq_ignore_ascii_case("claude.md")
        || file_key.starts_with("ccx-")
        || file_key.starts_with("actor-profile-")
        || file_key.starts_with("rtk-filters")
        || file_key == "actors-json"
        || file_key == "env-nexus-local"
        || file_key.contains("headroom")
        || target_path.starts_with(".opencode/plugins/")
}

/// Classify one file from its local bytes, the hash recorded when Nexus
/// last wrote it, and the desired content.
pub fn classify(
    class: FileClass,
    local: Option<&[u8]>,
    recorded: Option<&str>,
    desired: &str,
    marker_managed: bool,
) -> Option<State> {
    use nexus_core::hash::{hash_matches, sha256_hex_normalized};
    let Some(local) = local else {
        return Some(match (class, recorded) {
            (FileClass::Content, Some(_)) => State::Deleted,
            (FileClass::Content, None) => State::Update,
            (FileClass::Projection, _) => State::Create,
        });
    };
    let local = String::from_utf8_lossy(local);
    // A difference only in the export timestamp is not a change.
    if local == desired || sha256_hex_normalized(&local) == sha256_hex_normalized(desired) {
        return None;
    }
    Some(match recorded {
        Some(r) if hash_matches(r, &local) => State::Update,
        Some(r) if hash_matches(r, desired) => match class {
            FileClass::Content => State::Modified,
            FileClass::Projection => State::Drifted,
        },
        Some(_) => State::Conflict,
        None if marker_managed => State::Update,
        None => State::Unmanaged,
    })
}

/// [`classify`] for text workspace files (`devbox.json`, scripts):
/// differences only in trailing newlines are not changes, and a hash
/// recorded for the backend's copy still matches a local file that only
/// gained (or lost) its final newline.
pub fn classify_workspace_file(
    local: Option<&[u8]>,
    recorded: Option<&str>,
    desired: &str,
) -> Option<State> {
    use nexus_core::hash::{hash_matches_text, text_equivalent};
    let Some(local) = local else {
        return classify(FileClass::Content, None, recorded, desired, false);
    };
    let local = String::from_utf8_lossy(local);
    if text_equivalent(&local, desired) {
        return None;
    }
    Some(match recorded {
        Some(r) if hash_matches_text(r, &local) => State::Update,
        Some(r) if hash_matches_text(r, desired) => State::Modified,
        Some(_) => State::Conflict,
        None => State::Unmanaged,
    })
}

fn sync_manifest_hash(manifest: &serde_json::Value, target_path: &str) -> Option<String> {
    let obj = manifest.as_object()?;
    obj.values()
        .find_map(|e| {
            (e.get("target_path")?.as_str()? == target_path)
                .then(|| e.get("hash")?.as_str().map(str::to_string))
                .flatten()
        })
        .or_else(|| {
            obj.get(target_path)?
                .get("hash")?
                .as_str()
                .map(str::to_string)
        })
}

/// Everything the commands need, loaded once.
pub struct WorkspaceState {
    pub workspace: PathBuf,
    pub agentic_root: String,
    pub is_claude: bool,
    pub export: AgentFileExportResponse,
    pub entries: Vec<Entry>,
    /// `.claude/settings.json` as the next pull would leave it.
    pub settings_after: serde_json::Value,
}

impl WorkspaceState {
    /// Non-clean entries excluding stale-projection warnings.
    pub fn pending(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.state != State::Stale)
    }

    /// Entries whose path equals `path` or lies below it.
    pub fn matching<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a Entry> {
        let path = path.trim_end_matches('/');
        self.entries.iter().filter(move |e| {
            e.path == path
                || e.path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// Human label of what `nexus run` starts, including a local
    /// `[run] workspace` override (NEXUS-APP dispatch be6be18e).
    pub fn environment_label(&self) -> String {
        let project = self.project_environment_label();
        let project_ws = self
            .export
            .run_target
            .as_ref()
            .and_then(|t| t.workspace.as_deref())
            .unwrap_or("none");
        match config::load_run_workspace(Some(&self.workspace)) {
            Some(local) if local != project_ws => format!(
                "{project} (local override: {})",
                if local == "none" {
                    "plain"
                } else {
                    local.as_str()
                }
            ),
            _ => project,
        }
    }

    fn project_environment_label(&self) -> String {
        match &self.export.run_target {
            Some(t) if t.tool == "claude" && t.workspace.as_deref() == Some("zellij") => {
                "Claude Code (workspace zellij)".into()
            }
            Some(t) if t.tool == "claude" => "Claude Code".into(),
            Some(_) => "OpenCode".into(),
            None if self.is_claude => "Claude Code".into(),
            None => "OpenCode".into(),
        }
    }
}

/// Load and classify the workspace. Network: `af_export`, skill export,
/// and workspace export (the latter two best-effort).
pub async fn load(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
) -> anyhow::Result<WorkspaceState> {
    let export = client.export_agent_files(project_id).await?;
    let agentic_root = if export.agentic_root.is_empty() {
        ".nexus".to_string()
    } else {
        export.agentic_root.clone()
    };
    let owner = export
        .agent_owner
        .clone()
        .or_else(|| config::load_agent_owner(Some(workspace)));
    let is_claude = config::is_claude_owner(owner.as_deref());
    let manifest = super::sync::load_manifest_pub(workspace);
    let read = |p: &str| std::fs::read(workspace.join(p)).ok();
    let mut entries = Vec::new();

    // Agent files (CCX files are handled against the lock below), one per
    // path, exactly as pull materializes them.
    let (agent_files, _) = super::pull::unique_agent_files(&export.agent_files);
    for af in agent_files {
        if export.ccx.is_some() && af.category == ccx::CCX_CATEGORY {
            continue;
        }
        let other_runtime = if is_claude {
            af.target_path == "opencode.json" || af.target_path.starts_with(".opencode/")
        } else {
            af.target_path.starts_with(".claude/")
        };
        if other_runtime
            || super::pull::validate_agent_file_target_path(workspace, &af.target_path).is_err()
            || (super::pull::is_protected_path(&af.target_path)
                && workspace.join(&af.target_path).exists())
        {
            continue;
        }
        let class = if is_projection_agent_file(&af.file_key, &af.target_path, Some(af)) {
            FileClass::Projection
        } else {
            FileClass::Content
        };
        let local = read(&af.target_path);
        let marker = local
            .as_deref()
            .is_some_and(|l| String::from_utf8_lossy(l).contains("source: nexus-platform"));
        if let Some(state) = classify(
            class,
            local.as_deref(),
            sync_manifest_hash(&manifest, &af.target_path).as_deref(),
            &af.body,
            marker,
        ) {
            entries.push(Entry {
                path: af.target_path.clone(),
                class,
                state,
                kind: Kind::AgentFile {
                    file_key: af.file_key.clone(),
                },
                local: local.map(|l| String::from_utf8_lossy(&l).into_owned()),
                desired: Some(af.body.clone()),
            });
        }
    }

    // Devbox workspace (content).
    entries.extend(workspace_entries(client, project_id, workspace, &manifest).await);

    // CCX files, CLAUDE.md block and managed settings keys (Claude Code).
    let lock = ccx::load_lock(workspace, &agentic_root);
    let settings_before = claude_render::read_claude_settings(workspace);
    let mut settings_after = settings_before.clone();
    if is_claude {
        if export.ccx.is_some() {
            let plans = ccx::plan_files(
                workspace,
                &ccx::ccx_files(&export.agent_files),
                lock.as_ref(),
                &manifest,
            )?;
            for plan in plans {
                let state = match plan.state {
                    FileState::Clean => continue,
                    FileState::Create => State::Create,
                    FileState::Updated => State::Update,
                    FileState::Drifted => State::Drifted,
                    FileState::Conflict => State::Conflict,
                    FileState::Unmanaged => State::Unmanaged,
                    FileState::Adopt => State::Adopt,
                    FileState::Orphaned => State::Orphaned,
                };
                entries.push(Entry {
                    path: plan.target_path,
                    class: FileClass::Projection,
                    state,
                    kind: Kind::Ccx,
                    local: plan.local.map(|l| String::from_utf8_lossy(&l).into_owned()),
                    desired: plan.desired,
                });
            }
        }

        if let Some(block) = export.claude_md_managed_block.as_deref() {
            let claude_md = std::fs::read_to_string(workspace.join("CLAUDE.md")).ok();
            let desired = claude_render::claude_md_desired_block_text(block);
            let local = claude_md
                .as_deref()
                .and_then(claude_render::claude_md_block_text)
                .map(str::to_string);
            let locked = lock
                .as_ref()
                .and_then(|l| l.claude_md_block_sha256.as_deref());
            let state = match ccx::classify_file_state(
                Some(&sha256_hex(&desired)),
                local.as_deref().map(sha256_hex).as_deref(),
                locked,
            ) {
                FileState::Clean => None,
                FileState::Create => Some(State::Create),
                // Without a lock hash, pull replaces a differing block.
                FileState::Updated | FileState::Unmanaged => Some(State::Update),
                FileState::Drifted => Some(State::Drifted),
                _ => Some(State::Conflict),
            };
            if let Some(state) = state {
                entries.push(Entry {
                    path: "CLAUDE.md".into(),
                    class: FileClass::Projection,
                    state,
                    kind: Kind::ClaudeMdBlock,
                    local,
                    desired: Some(desired),
                });
            }
        }

        let previous = lock.as_ref().and_then(|l| l.settings.as_ref());
        claude_render::apply_generic_settings(
            &mut settings_after,
            export.claude_settings.as_ref(),
            previous,
        );
        let mut keys: Vec<&String> = Vec::new();
        for spec in [export.claude_settings.as_ref(), previous]
            .into_iter()
            .flatten()
        {
            for k in &spec.managed_keys {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        for key in keys {
            let before = ccx::json_get_path(&settings_before, key);
            let after = ccx::json_get_path(&settings_after, key);
            if before == after {
                continue;
            }
            // Local edit when the value differs from what the lock recorded.
            let recorded = previous.and_then(|p| p.values.get(key.as_str()));
            let state = if before.is_some() && recorded.is_some() && before != recorded {
                State::Drifted
            } else {
                State::Update
            };
            let render = |v: Option<&serde_json::Value>| {
                v.map(|v| serde_json::to_string_pretty(v).unwrap_or_default() + "\n")
            };
            entries.push(Entry {
                path: format!(".claude/settings.json#{key}"),
                class: FileClass::Projection,
                state,
                kind: Kind::SettingsKey(key.clone()),
                local: render(before),
                desired: render(after),
            });
        }
    }

    // Skills, OpenCode commands and directives.md rendered by pull.
    let mut generated: Vec<(String, String)> = Vec::new();
    if let Ok(skills) = client.export_skills(project_id).await {
        for skill in &skills.skills {
            generated.extend(super::pull::render_skill_files(skill, &agentic_root));
            if !is_claude {
                generated.extend(super::pull::render_command_file(skill, &agentic_root));
            }
        }
    }
    if let Ok(dir_export) = client.export_directives(project_id).await {
        if !dir_export.directives.is_empty() {
            generated.push((
                format!("{agentic_root}/directives.md"),
                super::pull::render_directives_markdown(&dir_export.directives),
            ));
        }
    }
    let recorded = super::pull::load_pull_manifest(workspace, &agentic_root);
    for (path, content) in generated {
        let local = read(&path);
        let state = match super::pull::classify_generated(
            local.as_deref(),
            &content,
            recorded.get(&path).map(String::as_str),
        ) {
            super::pull::GeneratedState::Unchanged => continue,
            super::pull::GeneratedState::Write if local.is_none() => State::Create,
            super::pull::GeneratedState::Write => State::Update,
            super::pull::GeneratedState::LocallyModified => State::Drifted,
        };
        entries.push(Entry {
            path,
            class: FileClass::Projection,
            state,
            kind: Kind::Generated,
            local: local.map(|l| String::from_utf8_lossy(&l).into_owned()),
            desired: Some(content),
        });
    }

    // Leftovers of the non-selected runtime's projection, which the next
    // `nexus pull` removes (v0.29.0). The operator's own files under
    // `.claude/` do not count.
    let unselected = super::projection_cleanup::Projection::unselected(is_claude);
    let cleanup_ctx = super::pull::cleanup_context(
        &export,
        &[],
        &agentic_root,
        &export.project_name,
        is_claude,
        false,
    );
    if super::projection_cleanup::plan(workspace, unselected, &cleanup_ctx).has_leftovers() {
        entries.push(Entry {
            path: unselected.dir().to_string(),
            class: FileClass::Projection,
            state: State::Stale,
            kind: Kind::Stale,
            local: None,
            desired: None,
        });
    }

    Ok(WorkspaceState {
        workspace: workspace.to_path_buf(),
        agentic_root,
        is_claude,
        export,
        entries,
        settings_after,
    })
}

/// Devbox workspace files: compared with the backend's workspace export when
/// available, otherwise local vs. the sync manifest only. Untracked local
/// scripts are NEW.
async fn workspace_entries(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
    manifest: &serde_json::Value,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut desired: Vec<(String, String)> = Vec::new();
    if let Ok(ws) = client.export_workspace_mcp(project_id).await {
        desired.push(("devbox.json".into(), ws.devbox_json));
        desired.extend(ws.scripts.into_iter().map(|s| (s.path, s.body)));
    }
    let known: Vec<String> = desired.iter().map(|(p, _)| p.clone()).collect();

    for (path, body) in desired {
        let local = std::fs::read(workspace.join(&path)).ok();
        if let Some(state) = classify_workspace_file(
            local.as_deref(),
            sync_manifest_hash(manifest, &path).as_deref(),
            &body,
        ) {
            // An untracked local file where the backend has one is a
            // conflict for content, never silently "unmanaged".
            let state = if state == State::Unmanaged {
                State::Conflict
            } else {
                state
            };
            entries.push(Entry {
                path,
                class: FileClass::Content,
                state,
                kind: Kind::Workspace,
                local: local.map(|l| String::from_utf8_lossy(&l).into_owned()),
                desired: Some(body),
            });
        }
    }

    // Local-only view: modified tracked files when the export is
    // unavailable, and new scripts the backend does not know.
    for (path, hash) in super::push::collect_workspace_hashes(workspace) {
        if known.contains(&path) {
            continue;
        }
        let recorded = sync_manifest_hash(manifest, &path);
        let unchanged = |r: &str| {
            r == hash
                || std::fs::read_to_string(workspace.join(&path))
                    .is_ok_and(|c| nexus_core::hash::hash_matches_text(r, &c))
        };
        let state = match recorded {
            None => State::New,
            Some(r) if !unchanged(&r) => State::Modified,
            Some(_) => continue,
        };
        entries.push(Entry {
            local: std::fs::read_to_string(workspace.join(&path)).ok(),
            path,
            class: FileClass::Content,
            state,
            kind: Kind::Workspace,
            desired: None,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "desired\n";

    fn h(s: &str) -> String {
        sha256_hex(s)
    }

    #[test]
    fn test_classify_workspace_file_ignores_trailing_newline() {
        let server = "{\"packages\":[]}";
        let recorded = h(server);
        // End-of-file fixer added a newline: clean, not modified/conflict.
        assert_eq!(
            classify_workspace_file(Some(b"{\"packages\":[]}\n"), Some(&recorded), server),
            None
        );
        // Unchanged local (with newline), backend moved on: update.
        assert_eq!(
            classify_workspace_file(Some(b"{\"packages\":[]}\n"), Some(&recorded), "{}"),
            Some(State::Update)
        );
        // A real local edit is still modified.
        assert_eq!(
            classify_workspace_file(Some(b"{\"packages\":[1]}\n"), Some(&recorded), server),
            Some(State::Modified)
        );
        assert_eq!(
            classify_workspace_file(None, Some(&recorded), server),
            Some(State::Deleted)
        );
    }

    #[test]
    fn test_classify_content_states() {
        let c = FileClass::Content;
        assert_eq!(classify(c, Some(D.as_bytes()), Some(&h(D)), D, false), None);
        assert_eq!(
            classify(c, None, Some(&h(D)), D, false),
            Some(State::Deleted)
        );
        assert_eq!(classify(c, None, None, D, false), Some(State::Update));
        // Unmodified local, backend moved on.
        assert_eq!(
            classify(c, Some(b"old\n"), Some(&h("old\n")), D, false),
            Some(State::Update)
        );
        // Local edit, backend unchanged.
        assert_eq!(
            classify(c, Some(b"edit\n"), Some(&h(D)), D, false),
            Some(State::Modified)
        );
        // Both changed.
        assert_eq!(
            classify(c, Some(b"edit\n"), Some(&h("old\n")), D, false),
            Some(State::Conflict)
        );
        // Never written by Nexus.
        assert_eq!(
            classify(c, Some(b"mine\n"), None, D, false),
            Some(State::Unmanaged)
        );
        assert_eq!(
            classify(c, Some(b"mine\n"), None, D, true),
            Some(State::Update)
        );
    }

    #[test]
    fn test_classify_projection_states() {
        let p = FileClass::Projection;
        assert_eq!(classify(p, None, None, D, false), Some(State::Create));
        assert_eq!(
            classify(p, Some(b"edit\n"), Some(&h(D)), D, false),
            Some(State::Drifted)
        );
    }

    #[test]
    fn test_classify_ignores_generated_at() {
        let desired = "---\ngenerated_at: 2\n---\nx\n";
        assert_eq!(
            classify(
                FileClass::Content,
                Some(b"---\ngenerated_at: 1\n---\nx\n"),
                None,
                desired,
                false
            ),
            None
        );
    }

    #[test]
    fn test_classify_local_edit_with_new_export_timestamp_is_modified() {
        // Pulled with generated_at 1 (normalized hash recorded), edited
        // locally; the next export only re-stamps generated_at.
        let pulled = "---\ngenerated_at: 1\n---\nbody\n";
        let recorded = nexus_core::hash::sha256_hex_normalized(pulled);
        let desired = "---\ngenerated_at: 2\n---\nbody\n";
        let edited = b"---\ngenerated_at: 1\n---\nbody\nmy edit\n";
        assert_eq!(
            classify(
                FileClass::Content,
                Some(edited),
                Some(&recorded),
                desired,
                true
            ),
            Some(State::Modified)
        );
        // Unedited: clean.
        assert_eq!(
            classify(
                FileClass::Content,
                Some(pulled.as_bytes()),
                Some(&recorded),
                desired,
                true
            ),
            None
        );
    }

    #[test]
    fn test_is_projection_agent_file() {
        assert!(is_projection_agent_file(
            "ccx-rule-base",
            ".claude/rules/10.md",
            None
        ));
        assert!(is_projection_agent_file(
            "actor-profile-dev",
            ".nexus/actors/dev.md",
            None
        ));
        assert!(is_projection_agent_file(
            "rtk-filters-default",
            ".rtk/filters.toml",
            None
        ));
        assert!(is_projection_agent_file(
            "env-nexus-local",
            ".env.nexus.local",
            None
        ));
        assert!(is_projection_agent_file(
            "x",
            ".opencode/plugins/nexus-headroom-intercept.ts",
            None
        ));
        // Regenerated by the backend on every export (dispatch b5f7bfb0).
        assert!(is_projection_agent_file(
            "AGENTS.md",
            ".nexus/AGENTS.md",
            None
        ));
        assert!(is_projection_agent_file(
            "claude.md",
            ".nexus/CLAUDE.md",
            None
        ));
        assert!(!is_projection_agent_file(
            "cursorrules",
            ".cursorrules",
            None
        ));
        assert!(!is_projection_agent_file(
            "copilot-instructions",
            ".github/copilot-instructions.md",
            None
        ));
        let synthetic = ExportedAgentFile {
            file_key: "whatever".into(),
            target_path: ".nexus/x.md".into(),
            name: "x".into(),
            description: None,
            category: "general".into(),
            version: 1,
            body: String::new(),
            content_hash: None,
            agent_file_id: Some("synthetic:whatever".into()),
        };
        assert!(is_projection_agent_file(
            "whatever",
            ".nexus/x.md",
            Some(&synthetic)
        ));
    }

    fn entry(path: &str, class: FileClass, state: State, kind: Kind) -> Entry {
        Entry {
            path: path.into(),
            class,
            state,
            kind,
            local: None,
            desired: None,
        }
    }

    #[test]
    fn test_next_action() {
        let e = entry(
            ".nexus/AGENTS.md",
            FileClass::Content,
            State::Modified,
            Kind::Workspace,
        );
        assert_eq!(
            e.next_action(),
            "nexus push .nexus/AGENTS.md (or nexus reset .nexus/AGENTS.md)"
        );
        let e = entry(
            ".claude/rules/10.md",
            FileClass::Projection,
            State::Drifted,
            Kind::Ccx,
        );
        // Rules ship with every profile: no claude.profile hint.
        assert_eq!(e.next_action(), "nexus reset .claude/rules/10.md");
        let e = entry(
            ".claude/statusline/nexus-hud.mjs",
            FileClass::Projection,
            State::Drifted,
            Kind::Ccx,
        );
        assert_eq!(
            e.next_action(),
            "nexus reset .claude/statusline/nexus-hud.mjs, or change it with nexus env set claude.hud <value>"
        );
        let e = entry(
            ".claude/settings.json#statusLine",
            FileClass::Projection,
            State::Update,
            Kind::SettingsKey("statusLine".into()),
        );
        assert_eq!(e.next_action(), "nexus pull");
        let e = entry(
            ".opencode/",
            FileClass::Projection,
            State::Stale,
            Kind::Stale,
        );
        assert!(e.next_action().contains("run nexus pull to remove"));
    }

    #[test]
    fn test_matching_paths() {
        let state = WorkspaceState {
            workspace: PathBuf::from("/w"),
            agentic_root: ".nexus".into(),
            is_claude: true,
            export: serde_json::from_value(serde_json::json!({
                "project_id": "p", "project_name": "n", "agent_files": [], "count": 0
            }))
            .unwrap(),
            entries: vec![
                entry(
                    ".claude/rules/a.md",
                    FileClass::Projection,
                    State::Drifted,
                    Kind::Ccx,
                ),
                entry(
                    ".claude/rules-x/b.md",
                    FileClass::Projection,
                    State::Drifted,
                    Kind::Ccx,
                ),
                entry(
                    ".opencode/",
                    FileClass::Projection,
                    State::Stale,
                    Kind::Stale,
                ),
            ],
            settings_after: serde_json::json!({}),
        };
        let m: Vec<_> = state
            .matching(".claude/rules")
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(m, vec![".claude/rules/a.md"]);
        assert_eq!(state.matching(".claude/rules/a.md").count(), 1);
        assert_eq!(state.pending().count(), 2);
    }
}
