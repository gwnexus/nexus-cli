//! CCX (Claude Code Experience Bundles) lock file and reconciliation
//! foundation (NEXUS-APP ADR-0117, dispatch 99f335e8, follow-up to
//! bb782869/v0.24.0).
//!
//! This module currently provides:
//! - The lock file format (`<agentic_root>/claude/manifest.lock.json`)
//!   and atomic read/write.
//! - [`classify_file_state`]: the pure per-file reconciliation state
//!   machine (create/clean/updated/drifted/conflict/unmanaged/adopt/
//!   orphaned), fully unit tested against every row of the state table
//!   in the dispatch, ready for the next phase to wire into `nexus pull`.
//! - [`reconcile_settings_removed_keys`]: the settings-key cleanup this
//!   dispatch specifically called out as not yet implemented in v0.24.0 --
//!   removing a `.claude/settings.json` key (or array entries) that Nexus
//!   used to manage but no longer sends, without touching anything the
//!   operator changed themselves.
//!
//! Not yet implemented (tracked as further follow-up): per-file pull
//! wiring (writing CCX-governed files with lock tracking, the `--force`/
//! `--force-unmanaged` distinction, orphan deletion), the CLAUDE.md-block
//! conflict path, the CCX pull output section, and the three new
//! `nexus claude status`/`diff`/`launch` commands. `classify_file_state`
//! and the lock format are deliberately built and tested now so that next
//! phase is pure wiring against an already-correct core, not new design.

use std::path::{Path, PathBuf};

use nexus_core::api::ClaudeSettingsSpec;
use serde::{Deserialize, Serialize};

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
    let mut lock = load_lock(workspace, agentic_root).unwrap_or_else(|| CcxLock {
        schema: 1,
        bundle: None,
        version: None,
        revision: None,
        compatibility: CcxCompatibility::default(),
        applied_at: String::new(),
        files: std::collections::BTreeMap::new(),
        settings: None,
        claude_md_block_sha256: None,
    });

    if lock.settings.as_ref() == settings {
        return Ok(());
    }

    lock.settings = settings.cloned();
    lock.applied_at = chrono_like_now();
    save_lock(workspace, agentic_root, &lock)
}

/// A UTC timestamp string in the same shape as the dispatch's example
/// (`"2026-09-24T12:00:00Z"`), without pulling in a `chrono` dependency
/// for a single formatted timestamp.
fn chrono_like_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // Not calendar-accurate beyond epoch-seconds formatting; sufficient
    // for an informational "when was this lock last updated" field that
    // nothing currently parses back out.
    format!("{}", now.as_secs())
}

/// Per-file CCX reconciliation state (dispatch 99f335e8's state table).
///
/// Not yet consumed anywhere -- this is groundwork for the next phase
/// (per-file pull wiring), built and fully unit tested now so that phase
/// is pure wiring against an already-correct, already-tested core rather
/// than new design under time pressure.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// No local file yet: write it.
    Create,
    /// Local file already matches the desired content.
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
#[allow(dead_code)]
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
        (Some(d), Some(l), Some(k)) => {
            if l == k {
                if d == k {
                    FileState::Clean
                } else {
                    FileState::Updated
                }
            } else if d == k {
                FileState::Drifted
            } else {
                FileState::Conflict
            }
        }
    }
}

/// Remove a `.claude/settings.json` key (or array entries) that Nexus
/// used to manage (per `previous`, the CCX lock's recorded settings
/// state) but no longer sends (per `current`, this run's `af_export`
/// spec) -- the "removed-key cleanup" flagged as missing in v0.24.0
/// (NEXUS-APP dispatch 99f335e8).
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
        if still_managed.contains(key_path.as_str()) {
            continue;
        }
        let Some(lock_value) = previous.values.get(key_path) else {
            continue;
        };
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

fn json_get_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
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
