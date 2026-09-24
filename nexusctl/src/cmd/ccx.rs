//! CCX (Claude Code Experience Bundles) lock file and reconciliation
//! foundation (NEXUS-APP ADR-0117, dispatch 99f335e8, follow-up to
//! bb782869/v0.24.0).
//!
//! This module provides:
//! - The lock file format (`<agentic_root>/claude/manifest.lock.json`)
//!   and atomic read/write.
//! - [`classify_file_state`]: the pure per-file reconciliation state
//!   machine (create/clean/updated/drifted/conflict/unmanaged/adopt/
//!   orphaned) from the dispatch's state table.
//! - [`plan_files`] / [`apply_file_plans`]: the per-file pull wiring for
//!   `agent_files[category="claude_experience"]`, including the
//!   sync-manifest migration rule and `--force`/`--force-unmanaged`.
//! - [`reconcile_settings_removed_keys`]: removal of `.claude/settings.json`
//!   keys (or array entries) that Nexus used to manage but no longer sends,
//!   without touching anything the operator changed themselves.

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use nexus_core::api::{CcxBundleInfo, ClaudeSettingsSpec, ExportedAgentFile};
use nexus_core::hash::{sha256_hex, sha256_hex_bytes};
use serde::{Deserialize, Serialize};

/// `agent_files[].category` of files governed by the CCX lock. All other
/// categories keep the regular pull behaviour.
pub const CCX_CATEGORY: &str = "claude_experience";

/// The CCX lock file: `<agentic_root>/claude/manifest.lock.json`.
/// CLI-owned; never shipped by the server. Records what was actually
/// written to disk on the last successful pull, so a later pull can tell
/// "clean" apart from "the operator edited this" apart from "Nexus wants
/// to change this again".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CcxLock {
    pub schema: u32,
    /// `None` until full CCX file-bundle metadata (dispatch 99f335e8's
    /// `ccx: { bundle, version, revision }` af_export field) is wired up;
    /// the lock can already track settings-merge history on its own in
    /// the meantime (see [`reconcile_settings_removed_keys`]).
    #[serde(default)]
    pub bundle: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub compatibility: CcxCompatibility,
    pub applied_at: String,
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, CcxLockFileEntry>,
    #[serde(default)]
    pub settings: Option<ClaudeSettingsSpec>,
    #[serde(default)]
    pub claude_md_block_sha256: Option<String>,
    /// Nexus-managed Claude Code hook adapters recorded on the last pull
    /// (NEXUS-APP dispatch 99f335e8 follow-up: hook adapter removal when
    /// e.g. the `nexus-core` Claude plugin takes over the same hooks),
    /// keyed by `target_path`.
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, CcxLockHookEntry>,
}

/// A single Nexus-managed Claude Code hook adapter as recorded in the CCX
/// lock: enough to find and remove exactly what Nexus added to
/// `.claude/settings.json`'s `hooks` block, and to know whether the hook
/// script file on disk is still exactly what Nexus wrote (safe to delete)
/// or was modified locally (leave it, report instead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcxLockHookEntry {
    pub plugin_name: String,
    pub target_path: String,
    pub file_sha256: String,
    pub registrations: Vec<CcxLockHookRegistration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcxLockHookRegistration {
    pub event: String,
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CcxCompatibility {
    #[serde(default, rename = "claudeCode")]
    pub claude_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CcxLockFileEntry {
    pub file_key: String,
    /// SHA-256 hex of the exact bytes last written to (or read from) disk
    /// for this file. No normalization.
    pub sha256: String,
}

/// Path to the CCX lock file for this workspace.
pub fn lock_path(workspace: &Path, agentic_root: &str) -> PathBuf {
    workspace
        .join(agentic_root)
        .join("claude")
        .join("manifest.lock.json")
}

/// Load the CCX lock file, if one exists. Returns `Ok(None)` if the file
/// is absent (no prior CCX-aware pull) and `Ok(None)` (not an error) if it
/// exists but fails to parse, since a corrupt lock should not block a
/// pull -- it is treated the same as "no lock yet" and will be rewritten.
pub fn load_lock(workspace: &Path, agentic_root: &str) -> Option<CcxLock> {
    let path = lock_path(workspace, agentic_root);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the CCX lock file atomically (write to a temp file in the same
/// directory, then rename over the target), so a crash mid-write can
/// never leave a corrupt or half-written lock behind.
pub fn save_lock(workspace: &Path, agentic_root: &str, lock: &CcxLock) -> anyhow::Result<()> {
    let path = lock_path(workspace, agentic_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(lock)? + "\n";
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Record `settings` as the new "previously managed" state in the CCX
/// lock, creating a minimal lock if none exists yet (before full
/// CCX file-bundle wiring lands, `bundle`/`version`/`revision` stay
/// `None` -- see [`CcxLock`]). No-op if `settings` is `None` and there is
/// nothing to clear from an existing lock either.
pub fn record_settings_in_lock(
    workspace: &Path,
    agentic_root: &str,
    settings: Option<&ClaudeSettingsSpec>,
) -> anyhow::Result<()> {
    let mut lock = load_lock(workspace, agentic_root).unwrap_or_else(empty_lock);

    if lock.settings.as_ref() == settings {
        return Ok(());
    }

    lock.settings = settings.cloned();
    lock.applied_at = chrono_like_now();
    save_lock(workspace, agentic_root, &lock)
}

/// Record the current set of Nexus-managed Claude Code hook adapters as
/// the new "previously managed" state in the CCX lock (NEXUS-APP dispatch
/// 99f335e8 follow-up), so a later pull can tell which entries in
/// `.claude/settings.json`'s `hooks` block, and which files under
/// `.claude/hooks/`, Nexus itself put there and is safe to remove once it
/// stops sending them (e.g. when the `nexus-core` Claude plugin takes
/// over the same hooks).
pub fn record_hooks_in_lock(
    workspace: &Path,
    agentic_root: &str,
    hooks: std::collections::BTreeMap<String, CcxLockHookEntry>,
) -> anyhow::Result<()> {
    let mut lock = load_lock(workspace, agentic_root).unwrap_or_else(empty_lock);

    if lock.hooks == hooks {
        return Ok(());
    }

    lock.hooks = hooks;
    lock.applied_at = chrono_like_now();
    save_lock(workspace, agentic_root, &lock)
}

fn empty_lock() -> CcxLock {
    CcxLock {
        schema: 1,
        bundle: None,
        version: None,
        revision: None,
        compatibility: CcxCompatibility::default(),
        applied_at: String::new(),
        files: std::collections::BTreeMap::new(),
        settings: None,
        claude_md_block_sha256: None,
        hooks: std::collections::BTreeMap::new(),
    }
}

/// A UTC timestamp string in the same shape as the dispatch's example
/// (`"2026-09-24T12:00:00Z"`), without pulling in a `chrono` dependency
/// for a single formatted timestamp.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d) = super::pull::days_to_ymd(secs / 86400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mo,
        d,
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Record the CCX bundle metadata and per-file hashes of a completed
/// CCX-aware pull. Called once, after all CCX files were reconciled, so an
/// error anywhere earlier leaves the previous lock in place.
pub fn record_ccx_in_lock(
    workspace: &Path,
    agentic_root: &str,
    info: &CcxBundleInfo,
    files: BTreeMap<String, CcxLockFileEntry>,
) -> anyhow::Result<()> {
    let mut lock = load_lock(workspace, agentic_root).unwrap_or_else(empty_lock);
    lock.bundle = Some(info.bundle.clone());
    lock.version = Some(info.version.clone());
    lock.revision = Some(info.revision.clone());
    lock.compatibility = CcxCompatibility {
        claude_code: info.compatibility.claude_code.clone(),
    };
    lock.files = files;
    lock.applied_at = chrono_like_now();
    save_lock(workspace, agentic_root, &lock)
}

/// Record the sha256 of the `CLAUDE.md` managed block text last written
/// (or confirmed unchanged) by the CLI.
pub fn record_claude_md_block_in_lock(
    workspace: &Path,
    agentic_root: &str,
    block_sha256: &str,
) -> anyhow::Result<()> {
    let mut lock = load_lock(workspace, agentic_root).unwrap_or_else(empty_lock);
    if lock.claude_md_block_sha256.as_deref() == Some(block_sha256) {
        return Ok(());
    }
    lock.claude_md_block_sha256 = Some(block_sha256.to_string());
    lock.applied_at = chrono_like_now();
    save_lock(workspace, agentic_root, &lock)
}

/// Per-file CCX reconciliation state (dispatch 99f335e8's state table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// No local file yet: write it.
    Create,
    /// Locked, and the local file already matches the desired content
    /// (regardless of what the lock recorded, e.g. after a pull that wrote
    /// the file but failed before saving the lock).
    Clean,
    /// Locked, local matches the lock, but the desired content changed:
    /// a new Nexus revision to apply.
    Updated,
    /// Locked, local does not match the lock, but the desired content
    /// still matches the lock (the operator edited it, Nexus hasn't sent
    /// a new revision): a local edit with no new revision to reconcile
    /// against yet.
    Drifted,
    /// Locked, local differs from the lock, AND the desired content also
    /// differs from the lock: both sides changed independently.
    Conflict,
    /// Never locked, a local file exists, and it does not match the
    /// desired content: an operator's own file in the way.
    Unmanaged,
    /// Never locked, a local file exists, and it already matches the
    /// desired content exactly: adopt it as managed without rewriting it.
    Adopt,
    /// Was in the lock, but is no longer part of the desired set at all.
    Orphaned,
}

/// Classify a single CCX-governed file's reconciliation state
/// (dispatch 99f335e8's state table), given the SHA-256 hex hashes of:
/// - `desired`: the content Nexus wants this file to have (`None` if this
///   file is no longer part of the desired set at all -- i.e. orphaned).
/// - `local`: the file currently on disk (`None` if it doesn't exist).
/// - `locked`: what the CCX lock recorded for this file on the last
///   successful pull (`None` if never locked).
///
/// Pure and side-effect-free: takes hashes, not paths, so every row of
/// the table is independently unit-testable without touching a
/// filesystem.
pub fn classify_file_state(
    desired: Option<&str>,
    local: Option<&str>,
    locked: Option<&str>,
) -> FileState {
    match (desired, local, locked) {
        (None, _, Some(_)) => FileState::Orphaned,
        (None, _, None) => {
            // Not desired and never locked: nothing to do, but this
            // shape (no lock, not desired) shouldn't be classified as a
            // CCX file in the first place -- callers should not invoke
            // this for files outside the desired+locked union. Treat as
            // Orphaned defensively rather than panicking.
            FileState::Orphaned
        }
        (Some(d), None, _) => {
            let _ = d;
            FileState::Create
        }
        (Some(d), Some(l), None) => {
            if l == d {
                FileState::Adopt
            } else {
                FileState::Unmanaged
            }
        }
        (Some(d), Some(l), Some(_)) if l == d => FileState::Clean,
        (Some(d), Some(l), Some(k)) => {
            if l == k {
                FileState::Updated
            } else if d == k {
                FileState::Drifted
            } else {
                FileState::Conflict
            }
        }
    }
}

/// How far a pull may go in overwriting local state for CCX files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForceMode {
    /// Keep every local edit and every unmanaged file; report instead.
    #[default]
    None,
    /// `--force`: overwrite drifted/conflicting files, delete modified
    /// orphans, replace a locally edited `CLAUDE.md` block.
    Force,
    /// `--force-unmanaged`: everything `--force` does, plus replacing
    /// files that exist locally but were never managed by Nexus.
    ForceUnmanaged,
}

impl ForceMode {
    pub fn from_flags(force: bool, force_unmanaged: bool) -> Self {
        if force_unmanaged {
            ForceMode::ForceUnmanaged
        } else if force {
            ForceMode::Force
        } else {
            ForceMode::None
        }
    }
}

/// The agent files governed by the CCX lock.
pub fn ccx_files(agent_files: &[ExportedAgentFile]) -> Vec<&ExportedAgentFile> {
    agent_files
        .iter()
        .filter(|af| af.category == CCX_CATEGORY)
        .collect()
}

/// A single CCX file's classification, computed without writing anything
/// (shared by `nexus pull`, `nexus claude status` and `nexus claude diff`).
#[derive(Debug, Clone)]
pub struct FilePlan {
    pub target_path: String,
    pub file_key: String,
    pub state: FileState,
    /// Desired body; `None` for orphaned files.
    pub desired: Option<String>,
    /// Exact bytes currently on disk; `None` if the file does not exist.
    pub local: Option<Vec<u8>>,
    /// Hash treated as "last written by Nexus" (lock, or sync manifest
    /// during migration); `None` if the file was never managed.
    pub locked: Option<String>,
}

/// Classify every desired CCX file plus every locked file that is no
/// longer desired (orphaned).
///
/// Migration: on the first CCX-aware pull (no lock, or a lock without a
/// recorded `revision`), hashes from `.nexus/sync-manifest.json` stand in
/// for lock hashes, so files written by earlier CLIs are recognised as
/// managed instead of unmanaged.
pub fn plan_files(
    workspace: &Path,
    desired: &[&ExportedAgentFile],
    lock: Option<&CcxLock>,
    sync_manifest: &serde_json::Value,
) -> anyhow::Result<Vec<FilePlan>> {
    let migrating = lock.is_none_or(|l| l.revision.is_none());
    let mut plans = Vec::new();

    for af in desired {
        super::pull::validate_agent_file_target_path(workspace, &af.target_path)?;
        let local = std::fs::read(workspace.join(&af.target_path)).ok();
        let locked = lock
            .and_then(|l| l.files.get(&af.target_path))
            .map(|e| e.sha256.clone())
            .or_else(|| {
                if migrating {
                    sync_manifest_hash(sync_manifest, &af.target_path)
                } else {
                    None
                }
            });
        let state = classify_file_state(
            Some(&sha256_hex(&af.body)),
            local.as_deref().map(sha256_hex_bytes).as_deref(),
            locked.as_deref(),
        );
        plans.push(FilePlan {
            target_path: af.target_path.clone(),
            file_key: af.file_key.clone(),
            state,
            desired: Some(af.body.clone()),
            local,
            locked,
        });
    }

    if let Some(lock) = lock {
        for (path, entry) in &lock.files {
            if desired.iter().any(|af| &af.target_path == path) {
                continue;
            }
            // The lock is an editable file on disk: never follow a path
            // out of the workspace, even one the CLI once wrote itself.
            if super::pull::validate_agent_file_target_path(workspace, path).is_err() {
                continue;
            }
            plans.push(FilePlan {
                target_path: path.clone(),
                file_key: entry.file_key.clone(),
                state: FileState::Orphaned,
                desired: None,
                local: std::fs::read(workspace.join(path)).ok(),
                locked: Some(entry.sha256.clone()),
            });
        }
    }

    Ok(plans)
}

/// Hash recorded in `.nexus/sync-manifest.json` (`file_key -> { hash,
/// target_path }`) for `target_path`, if any.
fn sync_manifest_hash(manifest: &serde_json::Value, target_path: &str) -> Option<String> {
    manifest.as_object()?.values().find_map(|entry| {
        if entry.get("target_path")?.as_str()? == target_path {
            entry.get("hash")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// What a pull actually did with one CCX file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Desired content written to disk.
    Written,
    /// Local file already matched; only the lock entry was recorded.
    Recorded,
    /// Local file left as is (local edit, unmanaged, protected, or a
    /// modified orphan).
    Kept,
    /// Orphaned file deleted.
    Deleted,
    /// Orphaned file was already gone; dropped from the lock.
    Dropped,
}

#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub target_path: String,
    pub state: FileState,
    pub action: FileAction,
}

/// Apply the plans from [`plan_files`] and return what happened plus the
/// new `files` section for the lock. Hashes recorded are always over the
/// exact bytes written (or found) on disk.
pub fn apply_file_plans(
    workspace: &Path,
    plans: &[FilePlan],
    force: ForceMode,
) -> anyhow::Result<(Vec<FileOutcome>, BTreeMap<String, CcxLockFileEntry>)> {
    let mut outcomes = Vec::new();
    let mut files = BTreeMap::new();
    let entry = |plan: &FilePlan, sha256: String| CcxLockFileEntry {
        file_key: plan.file_key.clone(),
        sha256,
    };

    for plan in plans {
        let path = workspace.join(&plan.target_path);
        let action = if plan.state == FileState::Orphaned {
            let pristine = match (&plan.local, &plan.locked) {
                (Some(local), Some(locked)) => sha256_hex_bytes(local) == *locked,
                _ => false,
            };
            if plan.local.is_none() {
                FileAction::Dropped
            } else if pristine || force != ForceMode::None {
                std::fs::remove_file(&path)?;
                FileAction::Deleted
            } else {
                FileAction::Kept
            }
        } else {
            let desired = plan
                .desired
                .as_deref()
                .expect("non-orphaned plans carry desired content");
            let overwrite = match plan.state {
                FileState::Create | FileState::Updated => true,
                FileState::Drifted | FileState::Conflict => force != ForceMode::None,
                FileState::Unmanaged => force == ForceMode::ForceUnmanaged,
                FileState::Clean | FileState::Adopt | FileState::Orphaned => false,
            };
            let protected = super::pull::is_protected_path(&plan.target_path) && path.exists();
            if overwrite && !protected {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, desired)?;
                files.insert(plan.target_path.clone(), entry(plan, sha256_hex(desired)));
                FileAction::Written
            } else if matches!(plan.state, FileState::Clean | FileState::Adopt) {
                files.insert(plan.target_path.clone(), entry(plan, sha256_hex(desired)));
                FileAction::Recorded
            } else {
                // Keep the previous lock hash, so a local edit stays
                // detectable as drifted/conflict on the next pull.
                if let Some(locked) = &plan.locked {
                    files.insert(plan.target_path.clone(), entry(plan, locked.clone()));
                }
                FileAction::Kept
            }
        };
        outcomes.push(FileOutcome {
            target_path: plan.target_path.clone(),
            state: plan.state,
            action,
        });
    }

    Ok((outcomes, files))
}

/// Status label and optional note for one line of the CCX pull output.
pub fn outcome_label(outcome: &FileOutcome) -> (&'static str, &'static str) {
    use FileAction as A;
    use FileState as S;
    match (outcome.state, outcome.action) {
        (S::Create, A::Written) => ("CREATE", ""),
        (S::Updated, A::Written) => ("UPDATE", ""),
        (S::Create | S::Updated, _) => ("SKIPPED", "protected file, never overwritten"),
        (S::Clean, _) => ("CLEAN", ""),
        (S::Adopt, _) => ("ADOPT", "already matches, now managed"),
        (S::Drifted, A::Written) => ("DRIFTED", "local edit overwritten (--force)"),
        (S::Drifted, _) => (
            "DRIFTED",
            "modified locally, run nexus claude diff (nexus pull --force restores)",
        ),
        (S::Conflict, A::Written) => ("CONFLICT", "local edit overwritten (--force)"),
        (S::Conflict, _) => ("CONFLICT", "modified locally, run nexus claude diff"),
        (S::Unmanaged, A::Written) => ("UNMANAGED", "replaced (--force-unmanaged)"),
        (S::Unmanaged, _) => (
            "UNMANAGED",
            "not managed by Nexus, kept (use --force-unmanaged to replace)",
        ),
        (S::Orphaned, A::Deleted) => ("ORPHANED", "deleted (no longer sent)"),
        (S::Orphaned, A::Dropped) => ("ORPHANED", "already removed"),
        (S::Orphaned, _) => ("ORPHANED", "modified locally, kept (no longer managed)"),
    }
}

/// Status label for a planned (not yet applied) state, as used by
/// `nexus claude status`.
pub fn state_label(state: FileState) -> &'static str {
    match state {
        FileState::Create => "CREATE",
        FileState::Clean => "CLEAN",
        FileState::Updated => "UPDATE",
        FileState::Drifted => "DRIFTED",
        FileState::Conflict => "CONFLICT",
        FileState::Unmanaged => "UNMANAGED",
        FileState::Adopt => "ADOPT",
        FileState::Orphaned => "ORPHANED",
    }
}

/// One entry per managed settings key (current and previously managed),
/// describing how `before` changed into `after`: `statusLine set`,
/// `attribution removed`, `permissions.deny +1/-0`, `model unchanged`.
pub fn describe_settings_changes(
    before: &serde_json::Value,
    after: &serde_json::Value,
    current: Option<&ClaudeSettingsSpec>,
    previous: Option<&ClaudeSettingsSpec>,
) -> Vec<String> {
    let mut keys: Vec<&str> = Vec::new();
    for spec in [current, previous].into_iter().flatten() {
        for key in &spec.managed_keys {
            if !keys.contains(&key.as_str()) {
                keys.push(key);
            }
        }
    }
    let empty = Vec::new();
    keys.into_iter()
        .map(|key| {
            let b = json_get_path(before, key);
            let a = json_get_path(after, key);
            if b.is_some_and(|v| v.is_array()) || a.is_some_and(|v| v.is_array()) {
                let b = b.and_then(|v| v.as_array()).unwrap_or(&empty);
                let a = a.and_then(|v| v.as_array()).unwrap_or(&empty);
                let added = a.iter().filter(|x| !b.contains(x)).count();
                let removed = b.iter().filter(|x| !a.contains(x)).count();
                format!("{key} +{added}/-{removed}")
            } else if b == a {
                format!("{key} unchanged")
            } else if a.is_none() {
                format!("{key} removed")
            } else {
                format!("{key} set")
            }
        })
        .collect()
}

/// Remove a `.claude/settings.json` key (or array entries) that Nexus
/// used to manage (per `previous`, the CCX lock's recorded settings
/// state) but no longer sends (per `current`, this run's `af_export`
/// spec) -- the "removed-key cleanup" flagged as missing in v0.24.0
/// (NEXUS-APP dispatch 99f335e8).
///
/// For an array key that is still managed, entries present in the lock's
/// recorded value but no longer in the new desired value are removed.
///
/// For a key that was managed and is no longer:
/// - scalar/object value: the dot-path is deleted only if the value
///   currently in `settings` deep-equals what the lock recorded for that
///   key -- an operator's own edit since then is never clobbered.
/// - array value: only the entries present in the lock's recorded array
///   are removed from the current array; anything else (including
///   operator additions) is kept.
///
/// Returns the number of keys/paths actually changed; `settings` is
/// mutated in place.
pub fn reconcile_settings_removed_keys(
    settings: &mut serde_json::Value,
    current: Option<&ClaudeSettingsSpec>,
    previous: Option<&ClaudeSettingsSpec>,
) -> usize {
    let Some(previous) = previous else {
        return 0;
    };
    let still_managed: std::collections::HashSet<&str> = current
        .map(|c| c.managed_keys.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let mut changed = 0usize;
    for key_path in &previous.managed_keys {
        let Some(lock_value) = previous.values.get(key_path) else {
            continue;
        };
        if still_managed.contains(key_path.as_str()) {
            // Still managed: for array unions, drop only the entries Nexus
            // previously added but no longer sends (operator entries stay).
            let new_value = current.and_then(|c| c.values.get(key_path));
            if let (
                Some(serde_json::Value::Array(existing)),
                serde_json::Value::Array(lock_arr),
                Some(serde_json::Value::Array(new_arr)),
            ) = (json_get_path(settings, key_path), lock_value, new_value)
            {
                let filtered: Vec<serde_json::Value> = existing
                    .iter()
                    .filter(|item| !(lock_arr.contains(item) && !new_arr.contains(item)))
                    .cloned()
                    .collect();
                if &filtered != existing {
                    json_set_path(settings, key_path, serde_json::Value::Array(filtered));
                    changed += 1;
                }
            }
            continue;
        }
        match (json_get_path(settings, key_path), lock_value) {
            (Some(serde_json::Value::Array(existing)), serde_json::Value::Array(lock_arr)) => {
                let filtered: Vec<serde_json::Value> = existing
                    .iter()
                    .filter(|item| !lock_arr.contains(item))
                    .cloned()
                    .collect();
                if &filtered != existing {
                    json_set_path(settings, key_path, serde_json::Value::Array(filtered));
                    changed += 1;
                }
            }
            (Some(current_value), lock_value) if current_value == lock_value => {
                json_remove_path(settings, key_path);
                changed += 1;
            }
            _ => {
                // Operator changed it since the lock was recorded; leave
                // it alone rather than clobbering their edit.
            }
        }
    }
    changed
}

pub(crate) fn json_get_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

fn json_set_path(value: &mut serde_json::Value, path: &str, new_value: serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if !current.is_object() {
            *current = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = current.as_object_mut().expect("just ensured object");
        if i == parts.len() - 1 {
            obj.insert((*part).to_string(), new_value);
            return;
        }
        current = obj
            .entry((*part).to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
}

/// Remove the value at a dot-separated path entirely, if present.
/// Intermediate objects that become empty as a result are left in place
/// (not pruned) -- an empty `{}` is harmless and pruning risks removing
/// an object the operator or another tool put there for its own reasons.
fn json_remove_path(value: &mut serde_json::Value, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for part in &parts[..parts.len() - 1] {
        let Some(obj) = current.as_object_mut() else {
            return;
        };
        let Some(next) = obj.get_mut(*part) else {
            return;
        };
        current = next;
    }
    if let Some(obj) = current.as_object_mut() {
        obj.remove(parts[parts.len() - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-ccx-test-{}-{}", std::process::id(), suffix));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_lock() -> CcxLock {
        CcxLock {
            schema: 1,
            bundle: Some("nexus-engineering".to_string()),
            version: Some("1.0.0".to_string()),
            revision: Some("ccx-0123456789ab".to_string()),
            compatibility: CcxCompatibility {
                claude_code: Some(">=2.1.257 <3.0.0".to_string()),
            },
            applied_at: "2026-09-24T12:00:00Z".to_string(),
            files: std::collections::BTreeMap::from([(
                ".claude/rules/10-nexus-base.md".to_string(),
                CcxLockFileEntry {
                    file_key: "ccx-rule-base".to_string(),
                    sha256: "abc123".to_string(),
                },
            )]),
            settings: None,
            claude_md_block_sha256: None,
            hooks: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn test_lock_path_is_under_agentic_root_claude() {
        let ws = Path::new("/home/user/project");
        assert_eq!(
            lock_path(ws, ".claude"),
            Path::new("/home/user/project/.claude/claude/manifest.lock.json")
        );
        assert_eq!(
            lock_path(ws, ".nexus"),
            Path::new("/home/user/project/.nexus/claude/manifest.lock.json")
        );
    }

    #[test]
    fn test_save_and_load_lock_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let lock = sample_lock();
        save_lock(&dir, ".nexus", &lock).unwrap();
        let loaded = load_lock(&dir, ".nexus").unwrap();
        assert_eq!(loaded, lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_lock_absent_returns_none() {
        let dir = tmp_dir("absent");
        assert!(load_lock(&dir, ".nexus").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_lock_corrupt_returns_none_not_error() {
        let dir = tmp_dir("corrupt");
        std::fs::create_dir_all(dir.join(".nexus/claude")).unwrap();
        std::fs::write(
            dir.join(".nexus/claude/manifest.lock.json"),
            "{ not valid json",
        )
        .unwrap();
        assert!(load_lock(&dir, ".nexus").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_lock_no_leftover_tmp_file() {
        let dir = tmp_dir("no-tmp-leftover");
        save_lock(&dir, ".nexus", &sample_lock()).unwrap();
        assert!(!dir.join(".nexus/claude/manifest.lock.json.tmp").exists());
        assert!(dir.join(".nexus/claude/manifest.lock.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── classify_file_state: every row of the dispatch 99f335e8 table ──────

    #[test]
    fn test_state_create_when_no_local_file() {
        assert_eq!(
            classify_file_state(Some("d"), None, None),
            FileState::Create
        );
        assert_eq!(
            classify_file_state(Some("d"), None, Some("k")),
            FileState::Create
        );
    }

    #[test]
    fn test_state_clean_when_local_matches_lock_and_desired() {
        assert_eq!(
            classify_file_state(Some("h"), Some("h"), Some("h")),
            FileState::Clean
        );
    }

    #[test]
    fn test_state_updated_when_desired_changes_but_local_still_matches_lock() {
        assert_eq!(
            classify_file_state(Some("new"), Some("old"), Some("old")),
            FileState::Updated
        );
    }

    #[test]
    fn test_state_drifted_when_local_edited_but_desired_unchanged() {
        assert_eq!(
            classify_file_state(Some("old"), Some("edited"), Some("old")),
            FileState::Drifted
        );
    }

    #[test]
    fn test_state_conflict_when_both_sides_changed() {
        assert_eq!(
            classify_file_state(Some("new"), Some("edited"), Some("old")),
            FileState::Conflict
        );
    }

    #[test]
    fn test_state_unmanaged_when_never_locked_and_local_differs() {
        assert_eq!(
            classify_file_state(Some("d"), Some("l"), None),
            FileState::Unmanaged
        );
    }

    #[test]
    fn test_state_adopt_when_never_locked_and_local_already_matches() {
        assert_eq!(
            classify_file_state(Some("h"), Some("h"), None),
            FileState::Adopt
        );
    }

    #[test]
    fn test_state_orphaned_when_no_longer_desired_but_was_locked() {
        assert_eq!(
            classify_file_state(None, Some("l"), Some("k")),
            FileState::Orphaned
        );
        assert_eq!(
            classify_file_state(None, None, Some("k")),
            FileState::Orphaned
        );
    }

    #[test]
    fn test_state_clean_when_local_matches_desired_even_if_lock_is_stale() {
        // A previous pull wrote the file but failed before saving the lock:
        // the file already has the desired content, so it is clean, not a
        // conflict.
        assert_eq!(
            classify_file_state(Some("new"), Some("new"), Some("old")),
            FileState::Clean
        );
    }

    // ── plan_files / apply_file_plans (pull wiring) ─────────────────────────

    fn ccx_file(path: &str, body: &str) -> ExportedAgentFile {
        ExportedAgentFile {
            file_key: format!("key:{path}"),
            target_path: path.to_string(),
            name: path.to_string(),
            description: None,
            category: CCX_CATEGORY.to_string(),
            version: 1,
            body: body.to_string(),
            content_hash: None,
            agent_file_id: Some(format!("synthetic:key:{path}")),
        }
    }

    fn bundle(revision: &str) -> CcxBundleInfo {
        CcxBundleInfo {
            bundle: "nexus-engineering".to_string(),
            version: "1.0.0".to_string(),
            revision: revision.to_string(),
            compatibility: Default::default(),
        }
    }

    /// One CCX-aware pull, exactly as `nexus pull` wires it.
    fn pull(dir: &Path, files: &[ExportedAgentFile], force: ForceMode) -> Vec<FileOutcome> {
        pull_with_manifest(dir, files, force, &serde_json::json!({}))
    }

    fn pull_with_manifest(
        dir: &Path,
        files: &[ExportedAgentFile],
        force: ForceMode,
        manifest: &serde_json::Value,
    ) -> Vec<FileOutcome> {
        let lock = load_lock(dir, ".nexus");
        let desired: Vec<&ExportedAgentFile> = files.iter().collect();
        let plans = plan_files(dir, &desired, lock.as_ref(), manifest).unwrap();
        let (outcomes, lock_files) = apply_file_plans(dir, &plans, force).unwrap();
        record_ccx_in_lock(dir, ".nexus", &bundle("rev-1"), lock_files).unwrap();
        outcomes
    }

    fn states(outcomes: &[FileOutcome]) -> Vec<(String, FileState, FileAction)> {
        outcomes
            .iter()
            .map(|o| (o.target_path.clone(), o.state, o.action))
            .collect()
    }

    const RULE: &str = ".claude/rules/10-nexus-base.md";

    #[test]
    fn test_pull_twice_second_run_all_clean() {
        let dir = tmp_dir("pull-twice");
        let files = [
            ccx_file(RULE, "v1\n"),
            ccx_file(".nexus/claude/x.kdl", "k\n"),
        ];
        let first = pull(&dir, &files, ForceMode::None);
        assert!(first
            .iter()
            .all(|o| o.state == FileState::Create && o.action == FileAction::Written));
        assert_eq!(fs_read(&dir, RULE), "v1\n");

        let second = pull(&dir, &files, ForceMode::None);
        assert!(second.iter().all(|o| o.state == FileState::Clean));

        let lock = load_lock(&dir, ".nexus").unwrap();
        assert_eq!(lock.revision.as_deref(), Some("rev-1"));
        assert_eq!(lock.files[RULE].sha256, sha256_hex("v1\n"));
        assert_eq!(lock.files[RULE].file_key, format!("key:{RULE}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fs_read(dir: &Path, path: &str) -> String {
        std::fs::read_to_string(dir.join(path)).unwrap()
    }

    fn fs_write(dir: &Path, path: &str, content: &str) {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn test_pull_updated_writes_new_revision() {
        let dir = tmp_dir("pull-updated");
        pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        let out = pull(&dir, &[ccx_file(RULE, "v2\n")], ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(RULE.to_string(), FileState::Updated, FileAction::Written)]
        );
        assert_eq!(fs_read(&dir, RULE), "v2\n");
        assert_eq!(
            load_lock(&dir, ".nexus").unwrap().files[RULE].sha256,
            sha256_hex("v2\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_drifted_kept_then_force_restores() {
        let dir = tmp_dir("pull-drifted");
        let files = [ccx_file(RULE, "v1\n")];
        pull(&dir, &files, ForceMode::None);
        fs_write(&dir, RULE, "my edit\n");

        let out = pull(&dir, &files, ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(RULE.to_string(), FileState::Drifted, FileAction::Kept)]
        );
        assert_eq!(fs_read(&dir, RULE), "my edit\n");
        // Lock keeps the old hash, so the edit stays detectable.
        assert_eq!(
            load_lock(&dir, ".nexus").unwrap().files[RULE].sha256,
            sha256_hex("v1\n")
        );
        let again = pull(&dir, &files, ForceMode::None);
        assert_eq!(again[0].state, FileState::Drifted);

        let forced = pull(&dir, &files, ForceMode::Force);
        assert_eq!(forced[0].action, FileAction::Written);
        assert_eq!(fs_read(&dir, RULE), "v1\n");
        assert_eq!(
            pull(&dir, &files, ForceMode::None)[0].state,
            FileState::Clean
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_conflict_kept_then_force_overwrites() {
        let dir = tmp_dir("pull-conflict");
        pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        fs_write(&dir, RULE, "my edit\n");

        let out = pull(&dir, &[ccx_file(RULE, "v2\n")], ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(RULE.to_string(), FileState::Conflict, FileAction::Kept)]
        );
        assert_eq!(fs_read(&dir, RULE), "my edit\n");

        let out = pull(&dir, &[ccx_file(RULE, "v2\n")], ForceMode::Force);
        assert_eq!(out[0].action, FileAction::Written);
        assert_eq!(fs_read(&dir, RULE), "v2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_unmanaged_needs_force_unmanaged() {
        let dir = tmp_dir("pull-unmanaged");
        fs_write(&dir, RULE, "operator file\n");
        let files = [ccx_file(RULE, "v1\n")];

        let out = pull(&dir, &files, ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(RULE.to_string(), FileState::Unmanaged, FileAction::Kept)]
        );
        assert!(!load_lock(&dir, ".nexus").unwrap().files.contains_key(RULE));

        let out = pull(&dir, &files, ForceMode::Force);
        assert_eq!(out[0].action, FileAction::Kept, "--force keeps unmanaged");
        assert_eq!(fs_read(&dir, RULE), "operator file\n");

        let out = pull(&dir, &files, ForceMode::ForceUnmanaged);
        assert_eq!(out[0].action, FileAction::Written);
        assert_eq!(fs_read(&dir, RULE), "v1\n");
        assert!(load_lock(&dir, ".nexus").unwrap().files.contains_key(RULE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_adopt_records_without_write() {
        let dir = tmp_dir("pull-adopt");
        fs_write(&dir, RULE, "v1\n");
        let out = pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(RULE.to_string(), FileState::Adopt, FileAction::Recorded)]
        );
        assert!(load_lock(&dir, ".nexus").unwrap().files.contains_key(RULE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_orphaned_pristine_deleted_and_dropped_from_lock() {
        let dir = tmp_dir("pull-orphan-pristine");
        let hud = ".claude/statusline/nexus-hud.mjs";
        pull(
            &dir,
            &[ccx_file(RULE, "v1\n"), ccx_file(hud, "hud\n")],
            ForceMode::None,
        );
        let out = pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        assert!(states(&out).contains(&(
            hud.to_string(),
            FileState::Orphaned,
            FileAction::Deleted
        )));
        assert!(!dir.join(hud).exists());
        assert!(!load_lock(&dir, ".nexus").unwrap().files.contains_key(hud));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_orphaned_modified_kept_unless_force() {
        let dir = tmp_dir("pull-orphan-modified");
        let hud = ".claude/statusline/nexus-hud.mjs";
        pull(&dir, &[ccx_file(hud, "hud\n")], ForceMode::None);
        fs_write(&dir, hud, "my hud\n");

        let out = pull(&dir, &[], ForceMode::None);
        assert_eq!(
            states(&out),
            vec![(hud.to_string(), FileState::Orphaned, FileAction::Kept)]
        );
        assert_eq!(fs_read(&dir, hud), "my hud\n");
        // Dropped from the lock either way: no longer managed.
        assert!(load_lock(&dir, ".nexus").unwrap().files.is_empty());

        // Re-lock it, edit, and remove with --force.
        pull(&dir, &[ccx_file(hud, "hud\n")], ForceMode::ForceUnmanaged);
        fs_write(&dir, hud, "my hud\n");
        let out = pull(&dir, &[], ForceMode::Force);
        assert_eq!(out[0].action, FileAction::Deleted);
        assert!(!dir.join(hud).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pull_orphaned_already_gone_is_dropped() {
        let dir = tmp_dir("pull-orphan-gone");
        pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        std::fs::remove_file(dir.join(RULE)).unwrap();
        let out = pull(&dir, &[], ForceMode::None);
        assert_eq!(out[0].action, FileAction::Dropped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_migration_uses_sync_manifest_hash_when_no_lock() {
        // Written by an earlier CLI (tracked in the sync manifest only),
        // then edited locally: must be seen as managed (drifted), not as an
        // unmanaged file.
        let dir = tmp_dir("migration-manifest");
        fs_write(&dir, RULE, "my edit\n");
        let manifest = serde_json::json!({
            "ccx-rule-base": {"target_path": RULE, "hash": sha256_hex("v1\n")}
        });
        let out = pull_with_manifest(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None, &manifest);
        assert_eq!(out[0].state, FileState::Drifted);

        // Unedited file from an earlier CLI plus a new revision: updated.
        let dir2 = tmp_dir("migration-manifest-updated");
        fs_write(&dir2, RULE, "v1\n");
        let out = pull_with_manifest(&dir2, &[ccx_file(RULE, "v2\n")], ForceMode::None, &manifest);
        assert_eq!(out[0].state, FileState::Updated);
        assert_eq!(fs_read(&dir2, RULE), "v2\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_migration_applies_to_settings_only_lock_from_v0_25() {
        // A v0.25.x lock has settings/hooks but no revision yet: still the
        // first CCX-aware pull, so the sync manifest is consulted.
        let dir = tmp_dir("migration-v025-lock");
        record_settings_in_lock(
            &dir,
            ".nexus",
            Some(&spec(&["x"], &[("x", serde_json::json!(1))])),
        )
        .unwrap();
        fs_write(&dir, RULE, "my edit\n");
        let manifest = serde_json::json!({
            "k": {"target_path": RULE, "hash": sha256_hex("v1\n")}
        });
        let out = pull_with_manifest(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None, &manifest);
        assert_eq!(out[0].state, FileState::Drifted);
        // The settings section recorded earlier survives the CCX record.
        assert!(load_lock(&dir, ".nexus").unwrap().settings.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_no_migration_after_first_ccx_pull() {
        // Once a revision is recorded, the sync manifest is ignored: a file
        // outside the lock is unmanaged.
        let dir = tmp_dir("no-migration");
        pull(&dir, &[], ForceMode::None);
        fs_write(&dir, RULE, "my edit\n");
        let manifest = serde_json::json!({
            "k": {"target_path": RULE, "hash": sha256_hex("v1\n")}
        });
        let out = pull_with_manifest(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None, &manifest);
        assert_eq!(out[0].state, FileState::Unmanaged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plan_rejects_path_traversal() {
        let dir = tmp_dir("plan-traversal");
        let bad = ccx_file("../escape.md", "x");
        assert!(plan_files(&dir, &[&bad], None, &serde_json::json!({})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_error_leaves_previous_lock_untouched() {
        let dir = tmp_dir("apply-error-lock");
        pull(&dir, &[ccx_file(RULE, "v1\n")], ForceMode::None);
        let before = std::fs::read_to_string(lock_path(&dir, ".nexus")).unwrap();

        // A directory where a new file must go makes the write fail.
        let blocked = ".claude/rules/blocked.md";
        std::fs::create_dir_all(dir.join(blocked).join("sub")).unwrap();
        let files = [ccx_file(RULE, "v2\n"), ccx_file(blocked, "x")];
        let lock = load_lock(&dir, ".nexus");
        let desired: Vec<&ExportedAgentFile> = files.iter().collect();
        let plans = plan_files(&dir, &desired, lock.as_ref(), &serde_json::json!({})).unwrap();
        assert!(apply_file_plans(&dir, &plans, ForceMode::None).is_err());

        // The pull bails out before record_ccx_in_lock: lock unchanged.
        assert_eq!(
            std::fs::read_to_string(lock_path(&dir, ".nexus")).unwrap(),
            before
        );
        // The file that was written is recognised as clean next time.
        let out = pull(&dir, &[ccx_file(RULE, "v2\n")], ForceMode::None);
        assert_eq!(out[0].state, FileState::Clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_record_ccx_preserves_hooks_and_settings_sections() {
        let dir = tmp_dir("record-preserves");
        record_settings_in_lock(
            &dir,
            ".nexus",
            Some(&spec(&["x"], &[("x", serde_json::json!(1))])),
        )
        .unwrap();
        record_claude_md_block_in_lock(&dir, ".nexus", "blocksha").unwrap();
        record_ccx_in_lock(&dir, ".nexus", &bundle("rev-9"), BTreeMap::new()).unwrap();
        let lock = load_lock(&dir, ".nexus").unwrap();
        assert!(lock.settings.is_some());
        assert_eq!(lock.claude_md_block_sha256.as_deref(), Some("blocksha"));
        assert_eq!(lock.revision.as_deref(), Some("rev-9"));
        assert!(lock.applied_at.ends_with('Z') && lock.applied_at.len() == 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_force_mode_from_flags() {
        assert_eq!(ForceMode::from_flags(false, false), ForceMode::None);
        assert_eq!(ForceMode::from_flags(true, false), ForceMode::Force);
        assert_eq!(
            ForceMode::from_flags(false, true),
            ForceMode::ForceUnmanaged
        );
        assert_eq!(ForceMode::from_flags(true, true), ForceMode::ForceUnmanaged);
    }

    #[test]
    fn test_describe_settings_changes() {
        let before = serde_json::json!({
            "attribution": {"commit": ""},
            "permissions": {"deny": ["a", "user"]},
            "model": "x"
        });
        let after = serde_json::json!({
            "statusLine": {"command": "hud"},
            "permissions": {"deny": ["user", "b"]},
            "model": "x"
        });
        let current = spec(&["statusLine", "permissions.deny", "model"], &[]);
        let previous = spec(&["attribution", "permissions.deny"], &[]);
        assert_eq!(
            describe_settings_changes(&before, &after, Some(&current), Some(&previous)),
            vec![
                "statusLine set",
                "permissions.deny +1/-1",
                "model unchanged",
                "attribution removed"
            ]
        );
    }

    // ── reconcile_settings_removed_keys ─────────────────────────────────────

    fn spec(keys: &[&str], values: &[(&str, serde_json::Value)]) -> ClaudeSettingsSpec {
        let mut map = serde_json::Map::new();
        for (k, v) in values {
            map.insert((*k).to_string(), v.clone());
        }
        ClaudeSettingsSpec {
            managed_keys: keys.iter().map(|s| s.to_string()).collect(),
            values: map,
        }
    }

    #[test]
    fn test_reconcile_no_previous_is_noop() {
        let mut settings = serde_json::json!({"statusLine": {"command": "x"}});
        let changed = reconcile_settings_removed_keys(&mut settings, None, None);
        assert_eq!(changed, 0);
    }

    #[test]
    fn test_reconcile_removes_scalar_key_no_longer_managed() {
        let mut settings = serde_json::json!({
            "statusLine": {"type": "command", "command": "node hud.mjs"}
        });
        let previous = spec(
            &["statusLine"],
            &[(
                "statusLine",
                serde_json::json!({"type": "command", "command": "node hud.mjs"}),
            )],
        );
        let changed = reconcile_settings_removed_keys(&mut settings, None, Some(&previous));
        assert_eq!(changed, 1);
        assert!(settings.get("statusLine").is_none());
    }

    #[test]
    fn test_reconcile_does_not_remove_if_operator_edited_since() {
        // Current value differs from what the lock recorded: the
        // operator (or another tool) changed it since -- must not clobber.
        let mut settings = serde_json::json!({
            "statusLine": {"type": "command", "command": "custom-operator-value"}
        });
        let previous = spec(
            &["statusLine"],
            &[(
                "statusLine",
                serde_json::json!({"type": "command", "command": "node hud.mjs"}),
            )],
        );
        let changed = reconcile_settings_removed_keys(&mut settings, None, Some(&previous));
        assert_eq!(changed, 0);
        assert_eq!(settings["statusLine"]["command"], "custom-operator-value");
    }

    #[test]
    fn test_reconcile_keeps_key_still_in_current_managed_keys() {
        let mut settings = serde_json::json!({"statusLine": {"command": "x"}});
        let previous = spec(
            &["statusLine"],
            &[("statusLine", serde_json::json!({"command": "x"}))],
        );
        let current = spec(
            &["statusLine"],
            &[("statusLine", serde_json::json!({"command": "x"}))],
        );
        let changed =
            reconcile_settings_removed_keys(&mut settings, Some(&current), Some(&previous));
        assert_eq!(changed, 0);
        assert!(settings.get("statusLine").is_some());
    }

    #[test]
    fn test_reconcile_removes_only_lock_recorded_array_entries() {
        // Operator-added entries in the array must survive even though
        // the key itself is no longer managed at all.
        let mut settings = serde_json::json!({
            "permissions": {"deny": ["Read(./.env)", "Read(./.env.*)", "Read(./my-secret.txt)"]}
        });
        let previous = spec(
            &["permissions.deny"],
            &[(
                "permissions.deny",
                serde_json::json!(["Read(./.env)", "Read(./.env.*)"]),
            )],
        );
        let changed = reconcile_settings_removed_keys(&mut settings, None, Some(&previous));
        assert_eq!(changed, 1);
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 1);
        assert_eq!(deny[0], "Read(./my-secret.txt)");
    }

    #[test]
    fn test_reconcile_still_managed_array_drops_entries_no_longer_sent() {
        let mut settings = serde_json::json!({
            "permissions": {"deny": ["Read(./.env)", "Read(./.env.*)", "Read(./mine)"]}
        });
        let previous = spec(
            &["permissions.deny"],
            &[(
                "permissions.deny",
                serde_json::json!(["Read(./.env)", "Read(./.env.*)"]),
            )],
        );
        let current = spec(
            &["permissions.deny"],
            &[("permissions.deny", serde_json::json!(["Read(./.env)"]))],
        );
        let changed =
            reconcile_settings_removed_keys(&mut settings, Some(&current), Some(&previous));
        assert_eq!(changed, 1);
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(["Read(./.env)", "Read(./mine)"])
        );
    }

    #[test]
    fn test_reconcile_missing_key_in_settings_already_is_noop() {
        let mut settings = serde_json::json!({});
        let previous = spec(
            &["statusLine"],
            &[("statusLine", serde_json::json!({"command": "x"}))],
        );
        let changed = reconcile_settings_removed_keys(&mut settings, None, Some(&previous));
        assert_eq!(changed, 0);
    }
}
