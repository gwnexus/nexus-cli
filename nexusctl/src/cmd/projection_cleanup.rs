//! Removal of the runtime projection a project no longer uses (v0.29.0).
//!
//! A project's `agent_owner` selects exactly one runtime projection:
//! OpenCode (`.opencode/`, `opencode.json`) or Claude Code (`.claude/`,
//! `.mcp.json`, the `CLAUDE.md` managed block, the CCX lock). The owner can
//! be switched at any time; `nexus pull` then removes what Nexus itself put
//! on disk for the runtime that is no longer selected, after the selected
//! projection has been written:
//!
//! - Files Nexus wrote and nobody edited since (per the sync manifest, the
//!   pull manifest, the CCX lock, the `source: nexus-platform` marker, or
//!   the content the current export renders) are deleted, together with
//!   OpenCode install artifacts (`node_modules/`, lock files).
//! - Locally modified Nexus files are kept and listed; `--force` /
//!   `--force-unmanaged` removes them too (never `-y` alone).
//! - Files Nexus never wrote are kept. Under `.opencode/`, `--force`
//!   removes the whole directory; under `.claude/` they are never touched,
//!   and neither is `.claude/settings.local.json`.
//! - Mixed files (`.claude/settings.json`, `.mcp.json`, `CLAUDE.md`) lose
//!   only their Nexus parts and are deleted once nothing else remains.
//!
//! [`plan`] only reads, so `nexus status` uses it to report leftovers;
//! [`apply`] performs the plan and prunes the manifests and the CCX lock.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use console::style;
use nexus_core::hash::{hash_matches, sha256_hex, sha256_hex_bytes};

use super::ccx;
use super::claude_render;

/// A runtime projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    OpenCode,
    Claude,
}

impl Projection {
    /// The projection a project with this runtime does not use.
    pub fn unselected(is_claude: bool) -> Self {
        if is_claude {
            Projection::OpenCode
        } else {
            Projection::Claude
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Projection::OpenCode => "OpenCode",
            Projection::Claude => "Claude Code",
        }
    }

    /// The projection's own directory, as shown in `nexus status`.
    pub fn dir(self) -> &'static str {
        match self {
            Projection::OpenCode => ".opencode/",
            Projection::Claude => ".claude/",
        }
    }
}

/// OpenCode install artifacts (created by OpenCode / its package manager,
/// never user content) directly under `.opencode/`.
const OPENCODE_ARTIFACTS: &[&str] = &[
    ".opencode/package-lock.json",
    ".opencode/bun.lock",
    ".opencode/.gitignore",
];

/// Root files of the OpenCode projection (token-bearing, git-excluded).
const OPENCODE_ROOT_FILES: &[&str] = &["opencode.json", "opencode.jsonc"];

/// Everything the cleanup needs from the current pull.
#[derive(Debug, Clone, Default)]
pub struct CleanupContext {
    pub agentic_root: String,
    pub project_name: String,
    /// Paths the current pull materializes; never removed.
    pub keep: HashSet<String>,
    /// Hashes (raw or `generated_at`-normalized) of content Nexus writes at
    /// a path, rendered from the current export. A path listed here is a
    /// Nexus path even when the local content differs.
    pub known: BTreeMap<String, Vec<String>>,
    /// File names of registry-downloaded OpenCode plugins.
    pub plugin_filenames: Vec<String>,
    /// Plugin MCP server names Nexus adds to `.mcp.json`.
    pub mcp_server_names: Vec<String>,
    /// `--force` / `--force-unmanaged` (never `-y` alone).
    pub force: bool,
}

impl CleanupContext {
    /// Record `content` as what Nexus writes at `path`.
    pub fn add_known(&mut self, path: &str, content: &str) {
        let hashes = self.known.entry(path.to_string()).or_default();
        for h in [
            sha256_hex(content),
            nexus_core::hash::sha256_hex_normalized(content),
        ] {
            if !hashes.contains(&h) {
                hashes.push(h);
            }
        }
    }
}

/// Why a file is left in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeepReason {
    /// Written by Nexus, edited since.
    Modified,
    /// Not written by Nexus.
    Unmanaged,
    /// Tracked in git (only for root files Nexus does not track itself).
    Tracked,
}

impl KeepReason {
    fn text(self) -> &'static str {
        match self {
            KeepReason::Modified => "modified locally",
            KeepReason::Unmanaged => "not managed by Nexus",
            KeepReason::Tracked => "tracked in git",
        }
    }
}

/// One step of a cleanup plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Delete a file.
    RemoveFile { path: String, note: &'static str },
    /// Delete a directory tree (`.opencode/node_modules/`, or `.opencode/`
    /// itself with `--force`).
    RemoveDir { path: String, note: &'static str },
    /// Rewrite a mixed file without its Nexus parts.
    Rewrite {
        path: String,
        content: String,
        note: &'static str,
    },
    /// Leave a file in place.
    Keep { path: String, reason: KeepReason },
}

/// What happens to the CCX lock after a Claude Code cleanup.
#[derive(Debug, Clone, PartialEq)]
pub enum LockAfter {
    Untouched,
    Delete,
    /// Kept files stay recorded, so a switch back reports them as drifted
    /// instead of as unmanaged.
    Save(Box<ccx::CcxLock>),
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub projection: Projection,
    pub actions: Vec<Action>,
    pub lock_after: LockAfter,
}

impl Plan {
    /// Whether anything of the projection is left that Nexus removes now or
    /// with `--force`. Files Nexus never wrote under `.claude/` are not
    /// part of a plan at all, so they never count.
    pub fn has_leftovers(&self) -> bool {
        self.lock_after != LockAfter::Untouched || !self.actions.is_empty()
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Removed files/directories (directories end with `/`), with a note.
    pub removed: Vec<(String, &'static str)>,
    /// Mixed files rewritten without their Nexus parts.
    pub rewritten: Vec<(String, &'static str)>,
    pub kept: Vec<(String, KeepReason)>,
}

impl CleanupReport {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.rewritten.is_empty() && self.kept.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Plan the removal of `projection` from `workspace`. Reads only.
pub fn plan(workspace: &Path, projection: Projection, ctx: &CleanupContext) -> Plan {
    let sync_manifest = super::sync::load_manifest_pub(workspace);
    let pull_manifest = super::pull::load_pull_manifest(workspace, &ctx.agentic_root);
    let records = Records {
        ctx,
        sync_manifest: &sync_manifest,
        pull_manifest: &pull_manifest,
    };
    match projection {
        Projection::OpenCode => Plan {
            projection,
            actions: plan_opencode(workspace, ctx, &records),
            lock_after: LockAfter::Untouched,
        },
        Projection::Claude => {
            let (actions, lock_after) = plan_claude(workspace, ctx, &records, &sync_manifest);
            Plan {
                projection,
                actions,
                lock_after,
            }
        }
    }
}

/// Where Nexus recorded what it wrote.
struct Records<'a> {
    ctx: &'a CleanupContext,
    sync_manifest: &'a serde_json::Value,
    pull_manifest: &'a BTreeMap<String, String>,
}

/// Origin of one file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Recorded (or rendered) by Nexus and unchanged.
    Pristine,
    /// Recorded (or rendered) by Nexus, content differs.
    Modified,
    /// Carries the `source: nexus-platform` marker (or is a registry plugin)
    /// but has no hash record, so an edit cannot be told apart: removed
    /// only with `--force`.
    Owned,
    /// Not written by Nexus.
    Unknown,
}

impl Records<'_> {
    fn origin(&self, rel: &str, bytes: &[u8], owned_by_name: bool) -> Origin {
        let content = String::from_utf8_lossy(bytes);
        let mut hashes: Vec<&str> = Vec::new();
        if let Some(known) = self.ctx.known.get(rel) {
            hashes.extend(known.iter().map(String::as_str));
        }
        if let Some(h) = self.pull_manifest.get(rel) {
            hashes.push(h);
        }
        let sync_hash = ccx::sync_manifest_hash(self.sync_manifest, rel);
        if let Some(ref h) = sync_hash {
            hashes.push(h);
        }
        if !hashes.is_empty() {
            let raw = sha256_hex_bytes(bytes);
            return if hashes
                .iter()
                .any(|h| *h == raw || hash_matches(h, &content))
            {
                Origin::Pristine
            } else {
                Origin::Modified
            };
        }
        if owned_by_name || content.contains(super::pull::MANAGED_MARKER) {
            Origin::Owned
        } else {
            Origin::Unknown
        }
    }
}

/// All files below `dir` (workspace-relative, `/`-separated), skipping
/// symlinked directories. Directories named in `stop` are returned as
/// directories (with a trailing `/`) instead of being descended into.
fn walk(workspace: &Path, dir: &str, stop: &[&str], out: &mut Vec<String>) {
    if through_symlink(workspace, &format!("{dir}/x")) {
        return;
    }
    let Ok(entries) = fs::read_dir(workspace.join(dir)) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{dir}/{name}");
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if stop.contains(&rel.as_str()) {
                out.push(format!("{rel}/"));
            } else {
                walk(workspace, &rel, stop, out);
            }
        } else {
            out.push(rel);
        }
    }
}

/// Whether `rel` is reached through a symlinked directory inside the
/// workspace (e.g. a `.claude/skills` linked to a shared dotfiles tree).
/// The cleanup never deletes or rewrites anything outside the workspace.
fn through_symlink(workspace: &Path, rel: &str) -> bool {
    let mut current = workspace.to_path_buf();
    let parts: Vec<&str> = rel.trim_end_matches('/').split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        if fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

fn is_git_tracked(workspace: &Path, rel: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", rel])
        .current_dir(workspace)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn plan_opencode(workspace: &Path, ctx: &CleanupContext, records: &Records) -> Vec<Action> {
    let mut actions = Vec::new();
    let kept_by_pull = |rel: &str| ctx.keep.contains(rel);

    if workspace.join(".opencode").is_dir() {
        if ctx.force && !ctx.keep.iter().any(|k| k.starts_with(".opencode/")) {
            actions.push(Action::RemoveDir {
                path: ".opencode/".into(),
                note: "projection removed (--force)",
            });
        } else {
            let mut files = Vec::new();
            walk(
                workspace,
                ".opencode",
                &[".opencode/node_modules"],
                &mut files,
            );
            for rel in files {
                if kept_by_pull(&rel) {
                    continue;
                }
                if rel.ends_with('/') {
                    actions.push(Action::RemoveDir {
                        path: rel,
                        note: "install artifact",
                    });
                    continue;
                }
                let name = rel.rsplit('/').next().unwrap_or(&rel);
                if OPENCODE_ARTIFACTS.contains(&rel.as_str()) || name == ".DS_Store" {
                    actions.push(Action::RemoveFile {
                        path: rel,
                        note: "install artifact",
                    });
                    continue;
                }
                let Ok(bytes) = fs::read(workspace.join(&rel)) else {
                    continue;
                };
                let registry_plugin = rel.starts_with(".opencode/plugins/")
                    && ctx.plugin_filenames.iter().any(|f| f == name);
                actions.push(match records.origin(&rel, &bytes, registry_plugin) {
                    Origin::Pristine => Action::RemoveFile {
                        path: rel,
                        note: "projection removed",
                    },
                    Origin::Owned if ctx.force => Action::RemoveFile {
                        path: rel,
                        note: "projection removed",
                    },
                    Origin::Modified | Origin::Owned => Action::Keep {
                        path: rel,
                        reason: KeepReason::Modified,
                    },
                    Origin::Unknown => Action::Keep {
                        path: rel,
                        reason: KeepReason::Unmanaged,
                    },
                });
            }
        }
    }

    // opencode.json: rebuilt by Nexus on every pull and git-excluded (it
    // carries the token). Only the operator's own `mcp` servers and
    // `instructions` survive a pull, so only those are kept; a committed
    // file is left alone without --force. Nexus never writes
    // opencode.jsonc, so it is removed only with --force.
    for rel in OPENCODE_ROOT_FILES {
        if !workspace.join(rel).is_file() || kept_by_pull(rel) {
            continue;
        }
        if ctx.force {
            actions.push(Action::RemoveFile {
                path: (*rel).into(),
                note: "projection removed",
            });
            continue;
        }
        if is_git_tracked(workspace, rel) {
            actions.push(Action::Keep {
                path: (*rel).into(),
                reason: KeepReason::Tracked,
            });
            continue;
        }
        if *rel != "opencode.json" {
            actions.push(Action::Keep {
                path: (*rel).into(),
                reason: KeepReason::Unmanaged,
            });
            continue;
        }
        let Some(current) = fs::read_to_string(workspace.join(rel))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        else {
            actions.push(Action::Keep {
                path: (*rel).into(),
                reason: KeepReason::Modified,
            });
            continue;
        };
        match opencode_json_user_part(&current, ctx) {
            None => actions.push(Action::RemoveFile {
                path: (*rel).into(),
                note: "projection removed",
            }),
            Some(user) if user != current => actions.push(Action::Rewrite {
                path: (*rel).into(),
                content: serde_json::to_string_pretty(&user).unwrap_or_default() + "\n",
                note: "Nexus entries removed",
            }),
            // Only the operator's own entries are left: not a leftover.
            Some(_) => {}
        }
    }
    actions
}

/// The operator's part of an `opencode.json`: their own `mcp` servers
/// (not `nexus` or a Nexus plugin server) and `instructions` outside the
/// agentic root. Everything else is rebuilt by Nexus on every pull. `None`
/// when nothing of the operator's is left.
fn opencode_json_user_part(
    current: &serde_json::Value,
    ctx: &CleanupContext,
) -> Option<serde_json::Value> {
    let root_prefix = format!("{}/", ctx.agentic_root.trim_end_matches('/'));
    let mcp: serde_json::Map<String, serde_json::Value> = current
        .get("mcp")
        .and_then(|m| m.as_object())
        .map(|m| {
            m.iter()
                .filter(|(name, _)| {
                    name.as_str() != "nexus" && !ctx.mcp_server_names.contains(name)
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let instructions: Vec<serde_json::Value> = current
        .get("instructions")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|v| v.as_str().is_some_and(|p| !p.starts_with(&root_prefix)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if mcp.is_empty() && instructions.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    if let Some(schema) = current.get("$schema") {
        out.insert("$schema".into(), schema.clone());
    }
    if !mcp.is_empty() {
        out.insert("mcp".into(), serde_json::Value::Object(mcp));
    }
    if !instructions.is_empty() {
        out.insert(
            "instructions".into(),
            serde_json::Value::Array(instructions),
        );
    }
    Some(serde_json::Value::Object(out))
}

fn plan_claude(
    workspace: &Path,
    ctx: &CleanupContext,
    records: &Records,
    sync_manifest: &serde_json::Value,
) -> (Vec<Action>, LockAfter) {
    let root = ctx.agentic_root.as_str();
    let mut actions: Vec<Action> = Vec::new();
    let mut planned: HashSet<String> = HashSet::new();
    let lock = ccx::load_lock(workspace, root);
    let mut kept_lock = lock.clone().map(|mut l| {
        l.files.clear();
        l.hooks.clear();
        l.settings = None;
        l.claude_md_block_sha256 = None;
        l
    });
    let mut keep_any_lock_entry = false;

    let remove_or_keep = |path: String, pristine: bool| {
        if pristine || ctx.force {
            Action::RemoveFile {
                path,
                note: "projection removed",
            }
        } else {
            Action::Keep {
                path,
                reason: KeepReason::Modified,
            }
        }
    };

    if let Some(ref lock) = lock {
        // CCX files: every locked file is orphaned now (existing orphan
        // rule: pristine -> delete, modified -> kept unless --force).
        let plans = ccx::plan_files(workspace, &[], Some(lock), sync_manifest).unwrap_or_default();
        for p in plans {
            if ctx.keep.contains(&p.target_path) {
                continue;
            }
            planned.insert(p.target_path.clone());
            let Some(local) = p.local else { continue };
            let pristine = p.locked.as_deref() == Some(sha256_hex_bytes(&local).as_str());
            let action = remove_or_keep(p.target_path.clone(), pristine);
            if matches!(action, Action::Keep { .. }) {
                if let (Some(kl), Some(entry)) =
                    (kept_lock.as_mut(), lock.files.get(&p.target_path))
                {
                    kl.files.insert(p.target_path.clone(), entry.clone());
                    keep_any_lock_entry = true;
                }
            }
            actions.push(action);
        }
        // Hook adapter scripts (their settings.json registrations are
        // removed with the settings below).
        for entry in lock.hooks.values() {
            if ctx.keep.contains(&entry.target_path)
                || super::pull::validate_agent_file_target_path(workspace, &entry.target_path)
                    .is_err()
            {
                continue;
            }
            planned.insert(entry.target_path.clone());
            let Ok(bytes) = fs::read(workspace.join(&entry.target_path)) else {
                continue;
            };
            let pristine = sha256_hex_bytes(&bytes) == entry.file_sha256;
            let action = remove_or_keep(entry.target_path.clone(), pristine);
            if matches!(action, Action::Keep { .. }) {
                if let Some(kl) = kept_lock.as_mut() {
                    let mut kept = entry.clone();
                    kept.registrations.clear();
                    kl.hooks.insert(entry.target_path.clone(), kept);
                    keep_any_lock_entry = true;
                }
            }
            actions.push(action);
        }
    }

    // Skills, agents and other files Nexus wrote under .claude/: the
    // skills/agents directories plus every .claude/ path Nexus recorded or
    // renders. Nothing else under .claude/ is looked at (it may hold the
    // operator's own files, worktrees, local settings). With the legacy
    // agentic root ".claude" the canonical skills live there too, so only
    // the Claude-only agents directory is walked.
    let mut files = Vec::new();
    walk(workspace, ".claude/agents", &[], &mut files);
    if root != ".claude" {
        walk(workspace, ".claude/skills", &[], &mut files);
        let recorded = records
            .pull_manifest
            .keys()
            .cloned()
            .chain(ctx.known.keys().cloned())
            .chain(
                sync_manifest
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.values())
                    .filter_map(|e| e.get("target_path")?.as_str().map(str::to_string)),
            )
            .filter(|p| p.starts_with(".claude/"));
        files.extend(recorded);
    }
    files.sort();
    files.dedup();
    for rel in files {
        if planned.contains(&rel)
            || ctx.keep.contains(&rel)
            || rel == ".claude/settings.json"
            || rel == ".claude/settings.local.json"
        {
            continue;
        }
        if super::pull::validate_agent_file_target_path(workspace, &rel).is_err() {
            continue;
        }
        let Ok(bytes) = fs::read(workspace.join(&rel)) else {
            continue;
        };
        match records.origin(&rel, &bytes, false) {
            Origin::Pristine => actions.push(Action::RemoveFile {
                path: rel,
                note: "projection removed",
            }),
            Origin::Modified | Origin::Owned => actions.push(remove_or_keep(rel, false)),
            // Never written by Nexus: not ours to touch, not reported.
            Origin::Unknown => {}
        }
    }

    // Routing-guard adapter inputs (Claude Code only).
    for name in ["routing-catalog.json", "agent-routing.json"] {
        let rel = format!("{root}/generated/{name}");
        if workspace.join(&rel).is_file() && !ctx.keep.contains(&rel) {
            actions.push(Action::RemoveFile {
                path: rel,
                note: "projection removed",
            });
        }
    }

    // .claude/settings.json: only the Nexus-managed parts.
    let settings_rel = ".claude/settings.json";
    if !ctx.keep.contains(settings_rel) {
        if let Some(settings) = fs::read_to_string(workspace.join(settings_rel))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            let mut cleaned = settings.clone();
            if strip_nexus_settings(&mut cleaned, lock.as_ref(), ctx.force) {
                actions.push(if settings_only_schema(&cleaned) {
                    Action::RemoveFile {
                        path: settings_rel.into(),
                        note: "only Nexus settings",
                    }
                } else {
                    Action::Rewrite {
                        path: settings_rel.into(),
                        content: serde_json::to_string_pretty(&cleaned).unwrap_or_default() + "\n",
                        note: "Nexus settings removed",
                    }
                });
            }
        }
    }

    // .mcp.json: only the Nexus servers.
    if !ctx.keep.contains(".mcp.json") {
        if let Some(mcp) = fs::read_to_string(workspace.join(".mcp.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            let mut cleaned = mcp.clone();
            if strip_nexus_mcp_servers(&mut cleaned, &ctx.mcp_server_names, ctx.force) {
                actions.push(if cleaned.as_object().is_some_and(|o| o.is_empty()) {
                    Action::RemoveFile {
                        path: ".mcp.json".into(),
                        note: "only Nexus MCP servers",
                    }
                } else {
                    Action::Rewrite {
                        path: ".mcp.json".into(),
                        content: serde_json::to_string_pretty(&cleaned).unwrap_or_default() + "\n",
                        note: "Nexus MCP servers removed",
                    }
                });
            }
        }
    }

    // Root CLAUDE.md: the managed block, and the bootstrap template when
    // nothing else remains.
    if !ctx.keep.contains("CLAUDE.md") {
        if let Ok(content) = fs::read_to_string(workspace.join("CLAUDE.md")) {
            let template = claude_render::render_claude_root_md(&ctx.project_name, root);
            let locked = lock
                .as_ref()
                .and_then(|l| l.claude_md_block_sha256.as_deref());
            match strip_claude_md(&content, locked, &template, ctx.force) {
                ClaudeMdCleanup::Untouched => {}
                ClaudeMdCleanup::Keep => {
                    actions.push(Action::Keep {
                        path: "CLAUDE.md".into(),
                        reason: KeepReason::Modified,
                    });
                    if let (Some(kl), Some(l)) = (kept_lock.as_mut(), lock.as_ref()) {
                        kl.claude_md_block_sha256 = l.claude_md_block_sha256.clone();
                        keep_any_lock_entry = true;
                    }
                }
                ClaudeMdCleanup::Remove => actions.push(Action::RemoveFile {
                    path: "CLAUDE.md".into(),
                    note: "only Nexus content",
                }),
                ClaudeMdCleanup::Rewrite(rest) => actions.push(Action::Rewrite {
                    path: "CLAUDE.md".into(),
                    content: rest,
                    note: "nexus-managed block removed",
                }),
            }
        }
    }

    let lock_after = match (lock, kept_lock) {
        (None, _) => LockAfter::Untouched,
        (Some(_), Some(kl)) if keep_any_lock_entry => LockAfter::Save(Box::new(kl)),
        (Some(_), _) => LockAfter::Delete,
    };
    (actions, lock_after)
}

/// Remove the Nexus-managed parts of a `.claude/settings.json` value:
/// settings keys recorded in the CCX lock (values the operator changed stay
/// unless `force`), the lock's hook registrations, the baseline MCP
/// permissions, `includeCoAuthoredBy`, the routing-guard `env` keys, and
/// containers left empty by that. Returns whether anything changed.
pub(crate) fn strip_nexus_settings(
    settings: &mut serde_json::Value,
    lock: Option<&ccx::CcxLock>,
    force: bool,
) -> bool {
    let before = settings.clone();
    let Some(_) = settings.as_object() else {
        return false;
    };

    let mut nexus_evidence = lock.is_some();
    if let Some(previous) = lock.and_then(|l| l.settings.as_ref()) {
        ccx::reconcile_settings_removed_keys(settings, None, Some(previous));
        if force {
            for key in &previous.managed_keys {
                if !previous.value(key).is_some_and(|v| v.is_array()) {
                    ccx::json_remove_path(settings, key);
                }
            }
        }
    }

    let obj = settings.as_object_mut().expect("checked above");
    if let Some(lock) = lock {
        if let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            for entry in lock.hooks.values() {
                claude_render::remove_hook_registrations(hooks, entry);
            }
        }
    }

    if let Some(allow) = obj
        .get_mut("permissions")
        .and_then(|p| p.get_mut("allow"))
        .and_then(|a| a.as_array_mut())
    {
        let before = allow.len();
        allow.retain(|v| {
            !v.as_str()
                .is_some_and(|s| claude_render::BASELINE_MCP_PERMISSIONS.contains(&s))
        });
        nexus_evidence |= allow.len() != before;
    }

    if let Some(env) = obj.get_mut("env").and_then(|e| e.as_object_mut()) {
        for key in claude_render::ROUTING_ENV_KEYS {
            nexus_evidence |= env.remove(key).is_some();
        }
    }

    // Written by Nexus on every Claude Code pull (dispatch 84e38bd7).
    if nexus_evidence {
        obj.remove("includeCoAuthoredBy");
    }

    // Containers emptied by the removals above.
    if let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        hooks.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
    }
    if let Some(perms) = obj.get_mut("permissions").and_then(|p| p.as_object_mut()) {
        perms.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
    }
    for key in ["hooks", "permissions", "env", "attribution"] {
        if obj
            .get(key)
            .is_some_and(|v| v.as_object().is_some_and(|o| o.is_empty()))
        {
            obj.remove(key);
        }
    }

    *settings != before
}

/// Whether a settings value has nothing left but (at most) `$schema`.
fn settings_only_schema(settings: &serde_json::Value) -> bool {
    settings
        .as_object()
        .is_some_and(|o| o.keys().all(|k| k == "$schema"))
}

/// Remove the Nexus MCP servers from a `.mcp.json` value: `nexus` (it
/// carries the token), `nexus-local-tools` when unchanged (or with
/// `force`), and the plugin servers Nexus adds. An empty `mcpServers` is
/// removed too. Returns whether anything changed.
pub(crate) fn strip_nexus_mcp_servers(
    mcp: &mut serde_json::Value,
    plugin_servers: &[String],
    force: bool,
) -> bool {
    let Some(obj) = mcp.as_object_mut() else {
        return false;
    };
    let Some(servers) = obj.get_mut("mcpServers").and_then(|s| s.as_object_mut()) else {
        return false;
    };
    let before = servers.len();
    servers.remove("nexus");
    let local_default = serde_json::json!({ "command": "nexus", "args": ["mcp-local"] });
    if force || servers.get("nexus-local-tools") == Some(&local_default) {
        servers.remove("nexus-local-tools");
    }
    for name in plugin_servers {
        servers.remove(name);
    }
    let changed = servers.len() != before;
    if changed && servers.is_empty() {
        obj.remove("mcpServers");
    }
    changed
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeMdCleanup {
    Untouched,
    /// The managed block was edited locally (kept without `force`).
    Keep,
    Remove,
    Rewrite(String),
}

/// Remove the nexus-managed block from a root `CLAUDE.md`, preserving
/// everything else. The file goes away when nothing else remains, or only
/// the untouched bootstrap template Nexus created (`template`); with
/// `force` also when a modified bootstrap template remains. A block the CCX
/// lock does not record as written unchanged is kept without `force`.
pub(crate) fn strip_claude_md(
    content: &str,
    locked_block_sha: Option<&str>,
    template: &str,
    force: bool,
) -> ClaudeMdCleanup {
    let begin = claude_render::CLAUDE_MD_MANAGED_BEGIN;
    let end = claude_render::CLAUDE_MD_MANAGED_END;
    let (rest, had_block) = match (content.find(begin), content.find(end)) {
        (Some(b), Some(e)) if e > b => {
            let block = &content[b + begin.len()..e];
            // Without the lock's record an edited block cannot be told
            // apart from Nexus's, so it is kept unless --force.
            if !force && locked_block_sha.is_none_or(|k| sha256_hex(block) != k) {
                return ClaudeMdCleanup::Keep;
            }
            let before = &content[..b];
            let after = &content[e + end.len()..];
            let rest = if before.trim().is_empty() {
                after.trim_start_matches(['\n', '\r']).to_string()
            } else {
                let after = after
                    .strip_prefix("\n\n")
                    .or_else(|| after.strip_prefix('\n'))
                    .unwrap_or(after);
                format!("{before}{after}")
            };
            (rest, true)
        }
        _ => (content.to_string(), false),
    };
    let is_template = rest.trim_start().starts_with("---\ntype: bootstrap")
        && rest.contains(super::pull::MANAGED_MARKER);
    if rest.trim().is_empty() || rest == template || (force && is_template) {
        ClaudeMdCleanup::Remove
    } else if had_block {
        ClaudeMdCleanup::Rewrite(rest)
    } else {
        ClaudeMdCleanup::Untouched
    }
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// Apply `plan`: delete/rewrite files, update the CCX lock, prune the sync
/// and pull manifests for everything removed, and remove directories left
/// empty inside the projection.
pub fn apply(workspace: &Path, plan: &Plan, ctx: &CleanupContext) -> anyhow::Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let mut removed_files: HashSet<String> = HashSet::new();
    let mut removed_dirs: Vec<String> = Vec::new();

    for action in &plan.actions {
        if let Action::RemoveFile { path, .. }
        | Action::RemoveDir { path, .. }
        | Action::Rewrite { path, .. } = action
        {
            if through_symlink(workspace, path) {
                report.kept.push((path.clone(), KeepReason::Unmanaged));
                continue;
            }
        }
        match action {
            Action::RemoveFile { path, note } => {
                let target = workspace.join(path);
                match fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                }
                removed_files.insert(path.clone());
                report.removed.push((path.clone(), note));
            }
            Action::RemoveDir { path, note } => {
                let target = workspace.join(path.trim_end_matches('/'));
                if !target.exists() {
                    continue;
                }
                fs::remove_dir_all(&target)?;
                removed_dirs.push(path.clone());
                report.removed.push((path.clone(), note));
            }
            Action::Rewrite {
                path,
                content,
                note,
            } => {
                fs::write(workspace.join(path), content)?;
                report.rewritten.push((path.clone(), note));
            }
            Action::Keep { path, reason } => report.kept.push((path.clone(), *reason)),
        }
    }

    let root = ctx.agentic_root.as_str();
    match &plan.lock_after {
        LockAfter::Untouched => {}
        LockAfter::Delete => {
            let lock = ccx::lock_path(workspace, root);
            if lock.exists() {
                fs::remove_file(&lock)?;
                let rel = format!("{root}/claude/manifest.lock.json");
                removed_files.insert(rel.clone());
                report.removed.push((rel, "projection removed"));
            }
        }
        LockAfter::Save(lock) => ccx::save_lock(workspace, root, lock)?,
    }

    let removed = |path: &str| {
        removed_files.contains(path)
            || removed_dirs
                .iter()
                .any(|d| path.starts_with(d.as_str()) || path == d.trim_end_matches('/'))
    };
    super::sync::remove_manifest_entries(workspace, removed)?;
    super::pull::remove_generated_records(workspace, root, removed)?;

    // Directories emptied by the removals, bottom-up, inside the
    // projection only (never the agentic root itself).
    let prunable: Vec<String> = match plan.projection {
        Projection::OpenCode => vec![".opencode".into()],
        Projection::Claude if root == ".claude" => vec![".claude/agents".into()],
        Projection::Claude => vec![".claude".into(), format!("{root}/claude")],
    };
    let mut dirs: Vec<String> = removed_files
        .iter()
        .flat_map(|f| {
            let mut parents = Vec::new();
            let mut p = Path::new(f).parent();
            while let Some(dir) = p {
                let d = dir.to_string_lossy().into_owned();
                if d.is_empty() {
                    break;
                }
                parents.push(d);
                p = dir.parent();
            }
            parents
        })
        .filter(|d| {
            prunable
                .iter()
                .any(|p| d == p || d.starts_with(&format!("{p}/")))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    dirs.dedup();
    for dir in dirs {
        let path = workspace.join(&dir);
        if fs::read_dir(&path).is_ok_and(|mut e| e.next().is_none()) {
            fs::remove_dir(&path)?;
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Print the cleanup section of the pull output. `switched` names the
/// previous and new `agent_owner` when the owner changed since the last
/// pull.
pub fn print_report(
    report: &CleanupReport,
    projection: Projection,
    switched: Option<(&str, &str)>,
) {
    if report.is_empty() {
        return;
    }
    let only_kept = report.removed.is_empty() && report.rewritten.is_empty();
    if only_kept && switched.is_none() {
        println!(
            "   {} {} file(s) of the unused {} projection kept ({}); {} removes them.",
            style("!").bold().yellow(),
            report.kept.len(),
            projection.label(),
            summarize_reasons(&report.kept),
            style("nexus pull --force").bold()
        );
        return;
    }
    println!();
    match switched {
        Some((from, to)) => println!(
            "{} agent_owner changed ({} -> {}): removing the {} projection",
            style("Projection cleanup:").bold(),
            from,
            to,
            projection.label()
        ),
        None => println!(
            "{} removing leftovers of the unused {} projection",
            style("Projection cleanup:").bold(),
            projection.label()
        ),
    }
    for (path, note) in &report.removed {
        println!("   {} {} ({})", style("-").bold().yellow(), path, note);
    }
    for (path, note) in &report.rewritten {
        println!("   {} {} ({})", style("~").bold().blue(), path, note);
    }
    for (path, reason) in &report.kept {
        println!(
            "   {} {} (kept: {})",
            style("!").bold().yellow(),
            path,
            reason.text()
        );
    }
    if !report.kept.is_empty() {
        let hint = match projection {
            Projection::OpenCode => "removes them (and the rest of .opencode/)",
            Projection::Claude => "removes modified Nexus files",
        };
        println!(
            "   Kept files stay in place; {} {}.",
            style("nexus pull --force").bold(),
            hint
        );
    }
}

fn summarize_reasons(kept: &[(String, KeepReason)]) -> String {
    let mut reasons: Vec<&str> = kept.iter().map(|(_, r)| r.text()).collect();
    reasons.sort();
    reasons.dedup();
    reasons.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::api::{
        CcxBundleInfo, ClaudeHookAdapter, ClaudeHookEvent, ClaudeSettingsSpec, ExportedActorFile,
        ExportedAgentFile, ExportedSkill,
    };
    use nexus_core::hash::sha256_hex_normalized;
    use std::path::PathBuf;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-projection-cleanup-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn json(dir: &Path, rel: &str) -> serde_json::Value {
        serde_json::from_str(&read(dir, rel)).unwrap()
    }

    fn ctx(force: bool) -> CleanupContext {
        CleanupContext {
            agentic_root: ".nexus".into(),
            project_name: "Demo".into(),
            plugin_filenames: super::super::init::platform_plugin_filenames(),
            mcp_server_names: vec!["task-master-ai".into()],
            force,
            ..Default::default()
        }
    }

    fn run(dir: &Path, projection: Projection, ctx: &CleanupContext) -> CleanupReport {
        let plan = plan(dir, projection, ctx);
        apply(dir, &plan, ctx).unwrap()
    }

    fn removed(report: &CleanupReport) -> Vec<String> {
        let mut r: Vec<String> = report.removed.iter().map(|(p, _)| p.clone()).collect();
        r.sort();
        r
    }

    fn kept(report: &CleanupReport) -> Vec<(String, KeepReason)> {
        let mut k = report.kept.clone();
        k.sort();
        k
    }

    fn git_init(dir: &Path, files: &[&str]) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        for f in files {
            git(&["add", f]);
        }
        git(&["commit", "-q", "--allow-empty", "-m", "initial"]);
    }

    // ── OpenCode projection (project switched to claude-cli) ──────────────

    const CMD_A: &str = "---\nsource: nexus-platform\n---\nLoad a\n";
    const PLUGIN: &str = "---\ngenerated_at: 1\n---\nexport const p = 1\n";

    /// A typical OpenCode projection as earlier pulls left it.
    fn opencode_projection(dir: &Path) {
        put(dir, ".opencode/commands/a.md", CMD_A);
        put(dir, ".opencode/commands/b.md", "edited locally\n");
        put(
            dir,
            ".opencode/commands/c.md",
            "---\nsource: nexus-platform\n---\n",
        );
        put(dir, ".opencode/plugins/nexus-p.ts", PLUGIN);
        put(
            dir,
            ".opencode/plugins/nexus-cost-control.ts",
            "// downloaded\n",
        );
        put(dir, ".opencode/plugins/mine.ts", "// my own plugin\n");
        put(dir, ".opencode/package.json", "{\"dependencies\":{}}\n");
        put(dir, ".opencode/package-lock.json", "{}");
        put(dir, ".opencode/bun.lock", "{}");
        put(dir, ".opencode/.gitignore", "node_modules\n");
        put(dir, ".opencode/.DS_Store", "x");
        put(
            dir,
            ".opencode/node_modules/@opencode-ai/plugin/index.js",
            "x",
        );
        put(dir, "opencode.json", "{\"mcp\":{}}\n");
        // Records of earlier pulls.
        put(
            dir,
            ".nexus/generated/pull-manifest.json",
            &serde_json::to_string(&serde_json::json!({
                ".opencode/commands/a.md": sha256_hex(CMD_A),
                ".opencode/commands/b.md": sha256_hex("original\n"),
                ".nexus/skills/nx-a/SKILL.md": sha256_hex("skill"),
            }))
            .unwrap(),
        );
        put(
            dir,
            ".nexus/sync-manifest.json",
            &serde_json::to_string(&serde_json::json!({
                "opencode-plugin-nexus-p": {
                    "target_path": ".opencode/plugins/nexus-p.ts",
                    // Recorded with a different export timestamp.
                    "hash": sha256_hex_normalized("---\ngenerated_at: 2\n---\nexport const p = 1\n"),
                },
                "opencode-package-json": {
                    "target_path": ".opencode/package.json",
                    "hash": sha256_hex("{\"dependencies\":{}}\n"),
                },
                "AGENTS.md": { "target_path": ".nexus/AGENTS.md", "hash": "x" },
            }))
            .unwrap(),
        );
    }

    #[test]
    fn test_opencode_cleanup_removes_pristine_keeps_modified_and_unknown() {
        let dir = temp_dir("oc-default");
        opencode_projection(&dir);
        let report = run(&dir, Projection::OpenCode, &ctx(false));

        assert_eq!(
            removed(&report),
            vec![
                ".opencode/.DS_Store",
                ".opencode/.gitignore",
                ".opencode/bun.lock",
                ".opencode/commands/a.md",
                ".opencode/node_modules/",
                ".opencode/package-lock.json",
                ".opencode/package.json",
                ".opencode/plugins/nexus-p.ts",
                "opencode.json",
            ]
        );
        // Marker / registry name without a hash record: an edit cannot be
        // ruled out, so only --force removes them.
        assert_eq!(
            kept(&report),
            vec![
                (".opencode/commands/b.md".into(), KeepReason::Modified),
                (".opencode/commands/c.md".into(), KeepReason::Modified),
                (".opencode/plugins/mine.ts".into(), KeepReason::Unmanaged),
                (
                    ".opencode/plugins/nexus-cost-control.ts".into(),
                    KeepReason::Modified
                ),
            ]
        );
        assert!(dir.join(".opencode/commands/b.md").exists());
        assert!(dir.join(".opencode/plugins/mine.ts").exists());
        assert!(!dir.join(".opencode/node_modules").exists());

        // Records of removed files are pruned; kept and unrelated stay.
        let sync = super::super::sync::load_manifest_pub(&dir);
        assert!(sync.get("opencode-plugin-nexus-p").is_none());
        assert!(sync.get("opencode-package-json").is_none());
        assert!(sync.get("AGENTS.md").is_some());
        let pulled = super::super::pull::load_pull_manifest(&dir, ".nexus");
        assert!(!pulled.contains_key(".opencode/commands/a.md"));
        assert!(pulled.contains_key(".opencode/commands/b.md"));
        assert!(pulled.contains_key(".nexus/skills/nx-a/SKILL.md"));

        // Idempotent: the next pull removes nothing, only reports the rest.
        let again = plan(&dir, Projection::OpenCode, &ctx(false));
        assert!(again
            .actions
            .iter()
            .all(|a| matches!(a, Action::Keep { .. })));
        let report = apply(&dir, &again, &ctx(false)).unwrap();
        assert!(report.removed.is_empty() && report.rewritten.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_cleanup_force_removes_directory_and_records() {
        let dir = temp_dir("oc-force");
        opencode_projection(&dir);
        put(&dir, "opencode.jsonc", "{}");
        let report = run(&dir, Projection::OpenCode, &ctx(true));
        assert_eq!(
            removed(&report),
            vec![".opencode/", "opencode.json", "opencode.jsonc"]
        );
        assert!(!dir.join(".opencode").exists());
        let sync = super::super::sync::load_manifest_pub(&dir);
        assert!(sync.get("opencode-plugin-nexus-p").is_none());
        let pulled = super::super::pull::load_pull_manifest(&dir, ".nexus");
        assert!(pulled.keys().all(|k| !k.starts_with(".opencode/")));
        // Nothing left afterwards.
        assert!(!plan(&dir, Projection::OpenCode, &ctx(true)).has_leftovers());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_cleanup_all_pristine_removes_empty_directory() {
        let dir = temp_dir("oc-pristine");
        put(&dir, ".opencode/commands/a.md", CMD_A);
        put(
            &dir,
            ".nexus/generated/pull-manifest.json",
            &serde_json::json!({".opencode/commands/a.md": sha256_hex(CMD_A)}).to_string(),
        );
        put(&dir, ".opencode/package-lock.json", "{}");
        put(&dir, "opencode.json", "{}");
        let report = run(&dir, Projection::OpenCode, &ctx(false));
        assert!(report.kept.is_empty());
        assert!(!dir.join(".opencode").exists());
        assert!(!dir.join("opencode.json").exists());
        let second = plan(&dir, Projection::OpenCode, &ctx(false));
        assert!(second.actions.is_empty() && !second.has_leftovers());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_cleanup_keeps_committed_opencode_json_without_force() {
        let dir = temp_dir("oc-tracked");
        put(&dir, "opencode.json", "{}");
        git_init(&dir, &["opencode.json"]);
        let report = run(&dir, Projection::OpenCode, &ctx(false));
        assert_eq!(
            kept(&report),
            vec![("opencode.json".into(), KeepReason::Tracked)]
        );
        assert!(dir.join("opencode.json").exists());
        let report = run(&dir, Projection::OpenCode, &ctx(true));
        assert_eq!(removed(&report), vec!["opencode.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_cleanup_never_removes_paths_the_pull_keeps() {
        let dir = temp_dir("oc-keep");
        put(&dir, ".opencode/commands/a.md", CMD_A);
        let mut c = ctx(true);
        c.keep.insert(".opencode/commands/a.md".into());
        let report = run(&dir, Projection::OpenCode, &c);
        assert!(report.is_empty());
        assert!(dir.join(".opencode/commands/a.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_recreated_after_switch_back() {
        // claude-cli -> opencode: the commands are rendered again without
        // a "locally modified" prompt, since their records were pruned.
        let dir = temp_dir("oc-recreate");
        let files = vec![(".opencode/commands/a.md".to_string(), CMD_A.to_string())];
        super::super::pull::sync_generated_files(&dir, ".nexus", &files, false).unwrap();
        run(&dir, Projection::OpenCode, &ctx(false));
        assert!(!dir.join(".opencode").exists());
        let (written, skipped) =
            super::super::pull::sync_generated_files(&dir, ".nexus", &files, false).unwrap();
        assert_eq!((written, skipped.len()), (1, 0));
        assert_eq!(read(&dir, ".opencode/commands/a.md"), CMD_A);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── Claude Code projection (project switched to opencode) ─────────────

    fn skill(id: &str) -> ExportedSkill {
        ExportedSkill {
            skill_id: id.into(),
            name: id.into(),
            description: Some("d".into()),
            version: 1,
            body: Some(format!("body of {id}")),
            command_slug: Some(id.into()),
            pinned: false,
            resources: vec![],
        }
    }

    fn actor(slug: &str) -> ExportedActorFile {
        ExportedActorFile {
            slug: slug.into(),
            name: slug.into(),
            role: "primary".into(),
            body: format!("# {slug}\n"),
            avatar: None,
            route_alias: None,
        }
    }

    fn adapter() -> ClaudeHookAdapter {
        ClaudeHookAdapter {
            plugin_name: "session-guard".into(),
            target_path: ".claude/hooks/nexus-session-guard.mjs".into(),
            body: "// adapter\n".into(),
            hook_events: vec![ClaudeHookEvent {
                event: "Stop".into(),
                matcher: None,
                timeout: None,
            }],
        }
    }

    fn settings_spec() -> ClaudeSettingsSpec {
        serde_json::from_value(serde_json::json!({
            "managed_keys": ["statusLine", "attribution", "permissions.deny"],
            "values": {
                "statusLine": { "type": "command", "command": "node hud.mjs" },
                "attribution": { "commit": "", "pr": "" },
                "permissions.deny": ["Read(./.env)"]
            }
        }))
        .unwrap()
    }

    fn ccx_file(path: &str, body: &str) -> ExportedAgentFile {
        ExportedAgentFile {
            file_key: format!("ccx-{path}"),
            target_path: path.into(),
            name: path.into(),
            description: None,
            category: ccx::CCX_CATEGORY.into(),
            version: 1,
            body: body.into(),
            content_hash: None,
            agent_file_id: None,
        }
    }

    const BLOCK: &str = "Nexus (managed) block";

    /// Render a full Claude Code projection exactly as `nexus pull` does
    /// for a claude-cli project (projection, CCX files and lock, .mcp.json).
    fn claude_projection(dir: &Path) {
        let runtime_spec = serde_json::json!({ "model_routes": {}, "actors": [] });
        claude_render::render_claude_projection(
            dir,
            "Demo",
            ".nexus",
            &[skill("nx-a"), skill("nexus-b")],
            &[actor("planner")],
            &[],
            Some(&runtime_spec),
            &[adapter()],
            None,
            Some(&settings_spec()),
            Some(BLOCK),
            false,
        )
        .unwrap();
        let files = [
            ccx_file(".claude/rules/10.md", "rule 10\n"),
            ccx_file(".claude/rules/20.md", "rule 20\n"),
            ccx_file(".nexus/claude/bundle.json", "{}\n"),
        ];
        let desired: Vec<&ExportedAgentFile> = files.iter().collect();
        let plans = ccx::plan_files(dir, &desired, None, &serde_json::json!({})).unwrap();
        let (_, locked) = ccx::apply_file_plans(dir, &plans, ccx::ForceMode::None).unwrap();
        let info = CcxBundleInfo {
            bundle: "engineering".into(),
            version: "1.0.0".into(),
            revision: "r1".into(),
            compatibility: Default::default(),
        };
        ccx::record_ccx_in_lock(dir, ".nexus", &info, locked).unwrap();
        put(
            dir,
            ".mcp.json",
            &serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "nexus": { "command": "npx", "env": { "NEXUS_PRIVATE_TOKEN": "t" } },
                    "nexus-local-tools": { "command": "nexus", "args": ["mcp-local"] },
                    "task-master-ai": { "command": "npx" }
                }
            }))
            .unwrap(),
        );
    }

    /// The operator's own additions to a Claude Code workspace.
    fn user_claude_files(dir: &Path) {
        put(dir, ".claude/settings.local.json", "{\"model\":\"opus\"}\n");
        put(dir, ".claude/skills/mine/SKILL.md", "my skill\n");
        put(dir, ".claude/agents/own.md", "my agent\n");
        let mut settings = json(dir, ".claude/settings.json");
        settings["model"] = serde_json::json!("sonnet");
        settings["permissions"]["allow"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("Bash(ls)"));
        settings["permissions"]["deny"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("Read(./secret)"));
        settings["env"]["USER_VAR"] = serde_json::json!("1");
        settings["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "hooks": [{ "type": "command", "command": "my-hook" }] }));
        put(
            dir,
            ".claude/settings.json",
            &serde_json::to_string_pretty(&settings).unwrap(),
        );
        let md = read(dir, "CLAUDE.md");
        put(dir, "CLAUDE.md", &format!("{md}\n## My notes\n\nkeep me\n"));
        let mut mcp = json(dir, ".mcp.json");
        mcp["mcpServers"]["mine"] = serde_json::json!({ "command": "mine" });
        put(
            dir,
            ".mcp.json",
            &serde_json::to_string_pretty(&mcp).unwrap(),
        );
    }

    fn claude_ctx(force: bool) -> CleanupContext {
        let mut c = ctx(force);
        for s in [skill("nx-a"), skill("nexus-b")] {
            for (p, content) in claude_render::render_claude_skill_files(&s) {
                c.add_known(&p, &content);
            }
        }
        for (p, content) in claude_render::claude_agent_files(&[actor("planner")], &[]) {
            c.add_known(&p, &content);
        }
        c.keep.insert(".nexus/skills/nx-a/SKILL.md".into());
        c
    }

    #[test]
    fn test_claude_cleanup_removes_nexus_parts_preserves_user_content() {
        let dir = temp_dir("cl-default");
        claude_projection(&dir);
        user_claude_files(&dir);
        // Local edits to Nexus files.
        put(&dir, ".claude/rules/20.md", "rule 20 edited\n");
        let local_settings = read(&dir, ".claude/settings.local.json");

        let report = run(&dir, Projection::Claude, &claude_ctx(false));
        assert_eq!(
            removed(&report),
            vec![
                ".claude/agents/planner.md",
                ".claude/hooks/nexus-session-guard.mjs",
                ".claude/rules/10.md",
                ".claude/skills/nexus-a/SKILL.md",
                ".claude/skills/nexus-b/SKILL.md",
                ".nexus/claude/bundle.json",
                ".nexus/generated/agent-routing.json",
                ".nexus/generated/routing-catalog.json",
            ]
        );
        assert_eq!(
            kept(&report),
            vec![(".claude/rules/20.md".into(), KeepReason::Modified)]
        );
        let mut rewritten: Vec<&str> = report.rewritten.iter().map(|(p, _)| p.as_str()).collect();
        rewritten.sort();
        assert_eq!(
            rewritten,
            vec![".claude/settings.json", ".mcp.json", "CLAUDE.md"]
        );

        // settings.json: only the operator's keys and entries remain.
        let settings = json(&dir, ".claude/settings.json");
        assert_eq!(
            settings,
            serde_json::json!({
                "$schema": "https://json.schemastore.org/claude-code-settings.json",
                "model": "sonnet",
                "permissions": { "allow": ["Bash(ls)"], "deny": ["Read(./secret)"] },
                "env": { "USER_VAR": "1" },
                "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "my-hook" }] }] }
            })
        );
        assert_eq!(read(&dir, ".claude/settings.local.json"), local_settings);
        // User files untouched, not even reported.
        assert_eq!(read(&dir, ".claude/skills/mine/SKILL.md"), "my skill\n");
        assert_eq!(read(&dir, ".claude/agents/own.md"), "my agent\n");
        // .mcp.json keeps only the operator's server.
        assert_eq!(
            json(&dir, ".mcp.json"),
            serde_json::json!({ "mcpServers": { "mine": { "command": "mine" } } })
        );
        // CLAUDE.md: block gone, template and notes kept.
        let md = read(&dir, "CLAUDE.md");
        assert!(!md.contains("nexus-managed") && !md.contains(BLOCK));
        assert!(md.starts_with("---\ntype: bootstrap"));
        assert!(md.ends_with("## My notes\n\nkeep me\n"));
        // The lock keeps only the kept file, so a switch back reports it.
        let lock = ccx::load_lock(&dir, ".nexus").unwrap();
        assert_eq!(
            lock.files.keys().collect::<Vec<_>>(),
            vec![".claude/rules/20.md"]
        );
        assert!(lock.hooks.is_empty() && lock.settings.is_none());
        let pulled = super::super::pull::load_pull_manifest(&dir, ".nexus");
        assert!(pulled
            .keys()
            .all(|k| !k.starts_with(".claude/skills/nexus-")));

        // Idempotent: a second run only reports the kept file.
        let again = plan(&dir, Projection::Claude, &claude_ctx(false));
        assert!(again
            .actions
            .iter()
            .all(|a| matches!(a, Action::Keep { .. })));
        let report = apply(&dir, &again, &claude_ctx(false)).unwrap();
        assert!(report.removed.is_empty() && report.rewritten.is_empty());
        assert_eq!(report.kept.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_cleanup_force_removes_modified_nexus_files_only() {
        let dir = temp_dir("cl-force");
        claude_projection(&dir);
        user_claude_files(&dir);
        put(&dir, ".claude/rules/20.md", "rule 20 edited\n");
        put(&dir, ".claude/hooks/nexus-session-guard.mjs", "// edited\n");
        put(&dir, ".claude/agents/planner.md", "# planner, edited\n");
        let mut settings = json(&dir, ".claude/settings.json");
        settings["attribution"] = serde_json::json!({ "commit": "mine" });
        put(
            &dir,
            ".claude/settings.json",
            &serde_json::to_string_pretty(&settings).unwrap(),
        );

        // Without --force: the edited Nexus files and key stay.
        let dry = plan(&dir, Projection::Claude, &claude_ctx(false));
        assert!(dry.actions.contains(&Action::Keep {
            path: ".claude/agents/planner.md".into(),
            reason: KeepReason::Modified
        }));

        let report = run(&dir, Projection::Claude, &claude_ctx(true));
        assert!(report.kept.is_empty());
        for gone in [
            ".claude/rules/20.md",
            ".claude/hooks/nexus-session-guard.mjs",
            ".claude/agents/planner.md",
            ".nexus/claude/manifest.lock.json",
        ] {
            assert!(!dir.join(gone).exists(), "{gone} must be removed");
        }
        assert!(json(&dir, ".claude/settings.json")
            .get("attribution")
            .is_none());
        // Never files Nexus did not write, never settings.local.json.
        assert!(dir.join(".claude/settings.local.json").exists());
        assert!(dir.join(".claude/skills/mine/SKILL.md").exists());
        assert!(dir.join(".claude/agents/own.md").exists());
        assert!(!dir.join(".nexus/claude").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_cleanup_pristine_projection_leaves_nothing() {
        let dir = temp_dir("cl-pristine");
        claude_projection(&dir);
        let report = run(&dir, Projection::Claude, &claude_ctx(false));
        assert!(report.kept.is_empty() && report.rewritten.is_empty());
        for gone in [".claude", "CLAUDE.md", ".mcp.json", ".nexus/claude"] {
            assert!(!dir.join(gone).exists(), "{gone} must be removed");
        }
        let again = plan(&dir, Projection::Claude, &claude_ctx(false));
        assert!(again.actions.is_empty() && !again.has_leftovers());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_recreated_after_switch_back() {
        // claude-cli -> opencode -> claude-cli: rendering again recreates
        // everything, nothing is reported as user-managed or conflicting.
        let dir = temp_dir("cl-recreate");
        claude_projection(&dir);
        user_claude_files(&dir);
        run(&dir, Projection::Claude, &claude_ctx(false));
        claude_projection(&dir);

        for present in [
            ".claude/skills/nexus-a/SKILL.md",
            ".claude/agents/planner.md",
            ".claude/hooks/nexus-session-guard.mjs",
            ".claude/rules/10.md",
            ".nexus/claude/bundle.json",
            ".nexus/generated/routing-catalog.json",
        ] {
            assert!(dir.join(present).exists(), "{present} must be recreated");
        }
        let settings = json(&dir, ".claude/settings.json");
        assert_eq!(settings["model"], "sonnet");
        assert_eq!(settings["statusLine"]["command"], "node hud.mjs");
        assert!(settings["env"]["NEXUS_ROUTING_GUARD_CATALOG_PATH"].is_string());
        assert!(settings["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h.to_string().contains("nexus-session-guard.mjs")));
        assert_eq!(settings["includeCoAuthoredBy"], false);
        let md = read(&dir, "CLAUDE.md");
        assert!(md.contains(BLOCK) && md.contains("keep me"));
        let lock = ccx::load_lock(&dir, ".nexus").unwrap();
        assert_eq!(lock.files.len(), 3);
        assert_eq!(lock.hooks.len(), 1);
        assert!(lock.settings.is_some() && lock.claude_md_block_sha256.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_cleanup_only_user_files_is_not_stale() {
        let dir = temp_dir("cl-user-only");
        put(&dir, ".claude/settings.local.json", "{}");
        put(&dir, ".claude/skills/mine/SKILL.md", "mine\n");
        put(&dir, ".claude/settings.json", "{\"model\":\"opus\"}\n");
        let p = plan(&dir, Projection::Claude, &claude_ctx(true));
        assert!(p.actions.is_empty());
        assert!(!p.has_leftovers());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_cleanup_legacy_claude_root_keeps_canonical_skills() {
        let dir = temp_dir("cl-legacy-root");
        put(
            &dir,
            ".claude/skills/nx-a/SKILL.md",
            "---\nsource: nexus-platform\n---\n",
        );
        let mut c = claude_ctx(false);
        c.agentic_root = ".claude".into();
        let report = run(&dir, Projection::Claude, &c);
        assert!(report.is_empty());
        assert!(dir.join(".claude/skills/nx-a/SKILL.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_json_keeps_operator_entries_only() {
        let dir = temp_dir("oc-json-user");
        let mut c = ctx(false);
        c.mcp_server_names = vec!["headroom".into()];
        put(
            &dir,
            "opencode.json",
            &serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": {"nexus": {"type": "local"}, "headroom": {}, "mine": {"type": "remote"}},
                "provider": {"dgx": {}},
                "model": "dgx/x",
                "instructions": [".nexus/AGENTS.md", "docs/mine.md"]
            })
            .to_string(),
        );
        let report = run(&dir, Projection::OpenCode, &c);
        assert_eq!(report.rewritten.len(), 1, "{report:?}");
        let left: serde_json::Value = serde_json::from_str(&read(&dir, "opencode.json")).unwrap();
        assert_eq!(
            left,
            serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": {"mine": {"type": "remote"}},
                "instructions": ["docs/mine.md"]
            })
        );
        // Idempotent: only the operator's part is left, nothing to do.
        assert!(!plan(&dir, Projection::OpenCode, &c).has_leftovers());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_cleanup_never_follows_symlinked_directories() {
        let dir = temp_dir("symlink");
        let shared = temp_dir("symlink-shared");
        put(
            &shared,
            "nx-a/SKILL.md",
            "---\nsource: nexus-platform\n---\nx\n",
        );
        fs::create_dir_all(dir.join(".claude")).unwrap();
        std::os::unix::fs::symlink(&shared, dir.join(".claude/skills")).unwrap();
        put(
            &dir,
            ".nexus/generated/pull-manifest.json",
            &serde_json::json!({
                ".claude/skills/nx-a/SKILL.md":
                    sha256_hex("---\nsource: nexus-platform\n---\nx\n")
            })
            .to_string(),
        );
        let report = run(&dir, Projection::Claude, &claude_ctx(false));
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(shared.join("nx-a/SKILL.md").exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&shared);
    }

    // ── mixed files ──────────────────────────────────────────────────────

    #[test]
    fn test_strip_claude_md_variants() {
        let template = claude_render::render_claude_root_md("Demo", ".nexus");
        let block = claude_render::claude_md_desired_block_text(BLOCK);
        // The lock records the block as written unchanged.
        let ok = sha256_hex(&block);
        let with_block = |rest: &str| {
            format!("<!-- BEGIN:nexus-managed -->{block}<!-- END:nexus-managed -->\n\n{rest}")
        };
        // Block + untouched template: Nexus created it all.
        assert_eq!(
            strip_claude_md(&with_block(&template), Some(&ok), &template, false),
            ClaudeMdCleanup::Remove
        );
        // Block only.
        assert_eq!(
            strip_claude_md(&with_block(""), Some(&ok), &template, false),
            ClaudeMdCleanup::Remove
        );
        // User content around the block survives byte for byte.
        let user = "# Mine\n\ntext\n";
        assert_eq!(
            strip_claude_md(&with_block(user), Some(&ok), &template, false),
            ClaudeMdCleanup::Rewrite(user.to_string())
        );
        let middle = format!(
            "# Top\n\n<!-- BEGIN:nexus-managed -->{block}<!-- END:nexus-managed -->\n\n# Bottom\n"
        );
        assert_eq!(
            strip_claude_md(&middle, Some(&ok), &template, false),
            ClaudeMdCleanup::Rewrite("# Top\n\n# Bottom\n".into())
        );
        // Without a lock record the block is kept unless --force.
        assert_eq!(
            strip_claude_md(&with_block(user), None, &template, false),
            ClaudeMdCleanup::Keep
        );
        assert_eq!(
            strip_claude_md(&with_block(user), None, &template, true),
            ClaudeMdCleanup::Rewrite(user.to_string())
        );
        // A locally edited block is kept unless --force.
        let locked = sha256_hex("\nsomething else\n");
        assert_eq!(
            strip_claude_md(&with_block(user), Some(&locked), &template, false),
            ClaudeMdCleanup::Keep
        );
        assert_eq!(
            strip_claude_md(&with_block(user), Some(&locked), &template, true),
            ClaudeMdCleanup::Rewrite(user.to_string())
        );
        // A modified bootstrap template: kept, removed with --force.
        let edited = format!("{template}\nextra line\n");
        assert_eq!(
            strip_claude_md(&with_block(&edited), Some(&ok), &template, false),
            ClaudeMdCleanup::Rewrite(edited.clone())
        );
        assert_eq!(
            strip_claude_md(&with_block(&edited), Some(&ok), &template, true),
            ClaudeMdCleanup::Remove
        );
        // No block, plain user file: untouched.
        assert_eq!(
            strip_claude_md(user, None, &template, true),
            ClaudeMdCleanup::Untouched
        );
    }

    #[test]
    fn test_strip_nexus_settings_only_schema_left() {
        let mut settings = serde_json::json!({
            "$schema": "s",
            "permissions": { "allow": ["mcp__nexus__kb_get"] },
            "includeCoAuthoredBy": false,
            "env": { "NEXUS_ROUTING_GUARD_AGENTS_PATH": "x" }
        });
        assert!(strip_nexus_settings(&mut settings, None, false));
        assert!(settings_only_schema(&settings));
        // Nothing of Nexus in it: untouched (includeCoAuthoredBy is the
        // operator's without other Nexus evidence).
        let mut user = serde_json::json!({ "includeCoAuthoredBy": true, "model": "x" });
        assert!(!strip_nexus_settings(&mut user, None, true));
    }

    #[test]
    fn test_strip_nexus_settings_nested_lock_values() {
        // A lock recording a dotted key's value nested (as some backends
        // send it) still identifies the Nexus entries.
        let lock = ccx::CcxLock {
            schema: 1,
            bundle: None,
            version: None,
            revision: None,
            compatibility: Default::default(),
            applied_at: String::new(),
            files: Default::default(),
            settings: Some(
                serde_json::from_value(serde_json::json!({
                    "managed_keys": ["permissions.deny", "statusLine"],
                    "values": {
                        "permissions": { "deny": ["Read(./.env)"] },
                        "statusLine": { "type": "command" }
                    }
                }))
                .unwrap(),
            ),
            claude_md_block_sha256: None,
            hooks: Default::default(),
        };
        let mut settings = serde_json::json!({
            "permissions": { "deny": ["Read(./.env)", "Read(./mine)"] },
            "statusLine": { "type": "command" },
            "model": "x"
        });
        assert!(strip_nexus_settings(&mut settings, Some(&lock), false));
        assert_eq!(
            settings,
            serde_json::json!({ "permissions": { "deny": ["Read(./mine)"] }, "model": "x" })
        );
    }

    #[test]
    fn test_strip_nexus_mcp_servers() {
        let mut mcp = serde_json::json!({
            "mcpServers": {
                "nexus": {},
                "nexus-local-tools": { "command": "nexus", "args": ["mcp-local", "--x"] },
                "task-master-ai": {}
            }
        });
        let plugins = vec!["task-master-ai".to_string()];
        assert!(strip_nexus_mcp_servers(&mut mcp, &plugins, false));
        // A customized local-tools server stays without --force.
        assert_eq!(
            mcp,
            serde_json::json!({ "mcpServers": {
                "nexus-local-tools": { "command": "nexus", "args": ["mcp-local", "--x"] }
            }})
        );
        assert!(strip_nexus_mcp_servers(&mut mcp, &plugins, true));
        assert_eq!(mcp, serde_json::json!({}));
        assert!(!strip_nexus_mcp_servers(&mut mcp, &plugins, true));
    }

    #[test]
    fn test_print_report_does_not_panic() {
        let report = CleanupReport {
            removed: vec![(".opencode/x".into(), "projection removed")],
            rewritten: vec![(".mcp.json".into(), "Nexus MCP servers removed")],
            kept: vec![(".opencode/y".into(), KeepReason::Modified)],
        };
        print_report(
            &report,
            Projection::OpenCode,
            Some(("opencode", "claude-cli")),
        );
        print_report(&report, Projection::Claude, None);
        let kept_only = CleanupReport {
            kept: report.kept.clone(),
            ..Default::default()
        };
        print_report(&kept_only, Projection::OpenCode, None);
        print_report(&CleanupReport::default(), Projection::Claude, None);
    }
}
