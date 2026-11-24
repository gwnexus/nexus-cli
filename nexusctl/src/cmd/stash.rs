//! The `nexus stash` command.
//!
//! Temporarily save local workspace changes to a stash directory,
//! restore them later, or list available stashes.

use console::style;
use nexus_core::hash::hash_matches_text;
#[cfg(test)]
use nexus_core::hash::sha256_hex;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Stash storage directory inside the agentic root.
const STASH_DIR: &str = ".nexus/stash";

/// Sync manifest path.
const SYNC_MANIFEST: &str = ".nexus/sync-manifest.json";

/// Workspace files to track for stash.
const WORKSPACE_PATHS: &[&str] = &["devbox.json", "scripts/devbox"];

/// Workspace scripts, whose executable bit a stash keeps.
const SCRIPTS_PREFIX: &str = "scripts/devbox/";

/// The metadata file at the root of a stash directory.
const META_FILE: &str = "stash.json";

/// The directory inside a stash that holds the saved files, apart from
/// the metadata.
const PAYLOAD_DIR: &str = "files";

/// Metadata for a stash entry.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct StashMeta {
    timestamp: String,
    files: Vec<String>,
    description: Option<String>,
    /// Saved workspace scripts that were executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    executable: Vec<String>,
    /// Where the saved files live: [`PAYLOAD_DIR`]; `None` for stashes of
    /// older versions, which kept them next to the metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

/// Detect workspace files that have been modified since the last pull.
/// Returns (path, content) pairs for modified files.
fn detect_modified_files(workspace: &Path) -> Vec<(PathBuf, String)> {
    let manifest_path = workspace.join(SYNC_MANIFEST);
    let manifest: serde_json::Value = if manifest_path.exists() {
        fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mut modified = Vec::new();

    for &tracked in WORKSPACE_PATHS {
        let full_path = workspace.join(tracked);
        if full_path.is_file() {
            match fs::read_to_string(&full_path) {
                Ok(content) => {
                    // Pull's comparison: a final newline added on disk is
                    // not an edit.
                    let manifest_hash = manifest
                        .get(tracked)
                        .and_then(|v| v.get("hash"))
                        .and_then(|h| h.as_str())
                        .unwrap_or("");
                    if !hash_matches_text(manifest_hash, &content) {
                        modified.push((full_path, content));
                    }
                }
                Err(e) => warn!("Failed to read {}: {}", full_path.display(), e),
            }
        } else if full_path.is_dir() {
            collect_modified_in_dir(workspace, &full_path, &manifest, &mut modified);
        }
    }

    // Agent files (content only; projection files are never stashed):
    // tracked in the sync manifest and edited
    // since the last pull.
    if let Some(entries) = manifest.as_object() {
        for (file_key, info) in entries {
            let Some(target) = info.get("target_path").and_then(|t| t.as_str()) else {
                continue;
            };
            let is_workspace_file = WORKSPACE_PATHS
                .iter()
                .any(|w| target == *w || target.starts_with(&format!("{w}/")));
            if is_workspace_file
                || super::workspace_state::is_projection_agent_file(file_key, target, None)
                || super::pull::validate_agent_file_target_path(workspace, target).is_err()
            {
                continue;
            }
            let expected = info.get("hash").and_then(|h| h.as_str()).unwrap_or("");
            if let Ok(content) = fs::read_to_string(workspace.join(target)) {
                if !nexus_core::hash::hash_matches(expected, &content) {
                    modified.push((workspace.join(target), content));
                }
            }
        }
    }

    modified
}

/// Recursively find modified files in a directory.
fn collect_modified_in_dir(
    workspace: &Path,
    dir: &Path,
    manifest: &serde_json::Value,
    modified: &mut Vec<(PathBuf, String)>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_modified_in_dir(workspace, &path, manifest, modified);
            } else if path.is_file() {
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        let rel = path
                            .strip_prefix(workspace)
                            .unwrap_or(&path)
                            .display()
                            .to_string();
                        if super::sync::is_local_script(manifest, &rel) {
                            continue;
                        }
                        let manifest_hash = manifest
                            .get(&rel)
                            .and_then(|v| v.get("hash"))
                            .and_then(|h| h.as_str())
                            .unwrap_or("");
                        if !hash_matches_text(manifest_hash, &content) {
                            modified.push((path, content));
                        }
                    }
                    Err(e) => warn!("Failed to read {}: {}", path.display(), e),
                }
            }
        }
    }
}

/// `nexus stash`: save local workspace changes to a stash.
pub fn save(workspace: &Path) -> anyhow::Result<()> {
    let modified = detect_modified_files(workspace);

    if modified.is_empty() {
        println!(
            "   {} No modified content files to stash.",
            style("✓").green()
        );
        return Ok(());
    }

    let timestamp = chrono_lite_timestamp();
    let stash_base = workspace.join(STASH_DIR);
    let (stash_path, name) = create_unique_stash_dir(&stash_base, &timestamp)?;
    let stashed_files = match write_stash(workspace, &stash_path, &timestamp, &modified) {
        Ok(files) => files,
        Err(e) => {
            // A half-written stash would be the latest one and block pop.
            let _ = fs::remove_dir_all(&stash_path);
            return Err(e);
        }
    };

    println!(
        "   {} Stashed {} file(s) → {}",
        style("✓").green().bold(),
        stashed_files.len(),
        style(&name).dim(),
    );
    for f in &stashed_files {
        println!("     {}", f);
    }

    Ok(())
}

/// Create a new stash directory named after `timestamp`, with a numbered
/// suffix when one of that second exists. The names sort in creation
/// order: `<ts>` before `<ts>-001`, and both before the next second.
fn create_unique_stash_dir(base: &Path, timestamp: &str) -> anyhow::Result<(PathBuf, String)> {
    fs::create_dir_all(base)?;
    for n in 0..1000u32 {
        let name = if n == 0 {
            timestamp.to_string()
        } else {
            format!("{timestamp}-{n:03}")
        };
        let path = base.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok((path, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no free stash name for {timestamp} in {}", base.display())
}

/// Write the saved files under [`PAYLOAD_DIR`] and then the metadata.
fn write_stash(
    workspace: &Path,
    stash_path: &Path,
    timestamp: &str,
    modified: &[(PathBuf, String)],
) -> anyhow::Result<Vec<String>> {
    let payload = stash_path.join(PAYLOAD_DIR);
    let mut files = Vec::new();
    let mut executable = Vec::new();
    for (file_path, content) in modified {
        let rel = file_path
            .strip_prefix(workspace)
            .unwrap_or(file_path)
            .display()
            .to_string();
        let target = payload.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        if rel.starts_with(SCRIPTS_PREFIX) && is_executable(file_path) {
            executable.push(rel.clone());
        }
        files.push(rel);
    }
    let meta = StashMeta {
        timestamp: timestamp.to_string(),
        files: files.clone(),
        description: None,
        executable,
        payload: Some(PAYLOAD_DIR.to_string()),
    };
    fs::write(
        stash_path.join(META_FILE),
        serde_json::to_string_pretty(&meta)?,
    )?;
    Ok(files)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// Saved files to restore: (relative path, saved copy).
type RestorePlan = Vec<(String, PathBuf)>;

/// The files of a stash that can be restored, as (relative path, saved
/// copy), or every reason one cannot. Nothing is written before all of
/// them check out.
fn restore_plan(
    workspace: &Path,
    stash: &Path,
    meta: &StashMeta,
) -> anyhow::Result<Result<RestorePlan, Vec<String>>> {
    let payload_root = match meta.payload.as_deref() {
        Some(PAYLOAD_DIR) => stash.join(PAYLOAD_DIR),
        None => stash.to_path_buf(),
        Some(other) => anyhow::bail!("unknown stash layout {other:?} in {}", stash.display()),
    };
    let mut plan = Vec::new();
    let mut problems = Vec::new();
    for file in &meta.files {
        // The stash list is a file in the workspace: every entry must be a
        // canonical path inside it (no `..`, not absolute, not into `.git`).
        if let Err(e) = super::pull::validate_agent_file_target_path(workspace, file) {
            problems.push(format!("{file}: {e}"));
            continue;
        }
        // Older stashes kept the metadata in the place of a saved
        // `stash.json`, so that copy is lost.
        if meta.payload.is_none() && file == META_FILE {
            problems.push(format!("{file}: overwritten by the stash metadata"));
            continue;
        }
        let source = payload_root.join(file);
        match fs::symlink_metadata(&source) {
            Ok(m) if m.is_file() => plan.push((file.clone(), source)),
            _ => problems.push(format!("{file}: missing from the stash")),
        }
    }
    Ok(if problems.is_empty() {
        Ok(plan)
    } else {
        Err(problems)
    })
}

/// `nexus stash pop`: restore the most recent stash.
pub fn pop(workspace: &Path) -> anyhow::Result<()> {
    let stash_base = workspace.join(STASH_DIR);
    if !stash_base.exists() {
        println!("   {} No stashes found.", style("!").yellow());
        return Ok(());
    }

    // Find the most recent stash (sorted by directory name = timestamp)
    let mut stashes: Vec<_> = fs::read_dir(&stash_base)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    stashes.sort();

    let latest = match stashes.last() {
        Some(p) => p.clone(),
        None => {
            println!("   {} No stashes found.", style("!").yellow());
            return Ok(());
        }
    };

    let meta_path = latest.join(META_FILE);
    let meta: StashMeta = if meta_path.exists() {
        serde_json::from_str(&fs::read_to_string(&meta_path)?)?
    } else {
        anyhow::bail!("Stash metadata not found: {}", meta_path.display());
    };

    let name = latest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let plan = match restore_plan(workspace, &latest, &meta)? {
        Ok(plan) => plan,
        Err(problems) => {
            for p in &problems {
                println!("   {} {p}", style("!").bold().yellow());
            }
            anyhow::bail!(
                "stash {name} kept: {} file(s) cannot be restored; nothing was changed",
                problems.len()
            );
        }
    };

    let mut restored: Vec<&str> = Vec::new();
    for (file, source) in &plan {
        if let Err(e) = restore_file(workspace, file, source, meta.executable.contains(file)) {
            let remaining: Vec<&str> = plan
                .iter()
                .map(|(f, _)| f.as_str())
                .filter(|f| !restored.contains(f))
                .collect();
            anyhow::bail!(
                "stash {name} kept: restoring {file} failed ({e}); restored: [{}], not restored: [{}]",
                restored.join(", "),
                remaining.join(", ")
            );
        }
        restored.push(file);
    }

    // Every file is back: the stash can go.
    fs::remove_dir_all(&latest)?;

    println!(
        "   {} Restored {} file(s) from stash {}",
        style("✓").green().bold(),
        meta.files.len(),
        style(&meta.timestamp).dim(),
    );
    for f in &meta.files {
        println!("     {}", f);
    }

    Ok(())
}

/// Write one saved file back. Read and write instead of `fs::copy`: no
/// permission bits come from the stash; a workspace script that was
/// executable is made executable again.
fn restore_file(
    workspace: &Path,
    file: &str,
    source: &Path,
    executable: bool,
) -> anyhow::Result<()> {
    let target = workspace.join(file);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, fs::read(source)?)?;
    #[cfg(unix)]
    if executable && file.starts_with(SCRIPTS_PREFIX) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// `nexus stash list`: list all available stashes.
pub fn list(workspace: &Path) -> anyhow::Result<()> {
    let stash_base = workspace.join(STASH_DIR);
    if !stash_base.exists() {
        println!("   {} No stashes found.", style("!").yellow());
        return Ok(());
    }

    let mut stashes: Vec<_> = fs::read_dir(&stash_base)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    stashes.sort();

    if stashes.is_empty() {
        println!("   {} No stashes found.", style("!").yellow());
        return Ok(());
    }

    println!("{} Available stashes:", style(">>").bold().cyan());
    println!();

    for (i, stash_path) in stashes.iter().rev().enumerate() {
        let meta_path = stash_path.join(META_FILE);
        if let Ok(content) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<StashMeta>(&content) {
                let label = if i == 0 { " (latest)" } else { "" };
                println!(
                    "   {} {}{}: {} file(s)",
                    style(format!("[{}]", stashes.len() - 1 - i)).dim(),
                    style(&meta.timestamp).bold(),
                    style(label).green(),
                    meta.files.len(),
                );
                for f in &meta.files {
                    println!("     {}", style(f).dim());
                }
            }
        }
    }
    println!();

    Ok(())
}

/// Generate a timestamp string suitable for directory names.
///
/// Produces ISO-like format: `2026-08-23T10-30-00Z` with correct
/// date calculation (handles leap years and actual month lengths).
fn chrono_lite_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Days since epoch
    let mut remaining_days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Compute year
    let mut year: i64 = 1970;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    // Compute month and day
    let leap = is_leap_year(year);
    let days_in_months: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &dim in &days_in_months {
        if remaining_days < dim {
            break;
        }
        remaining_days -= dim;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Check if a year is a leap year.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_stash_save_and_pop() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path();

        // Create a workspace file
        fs::write(workspace.join("devbox.json"), r#"{"packages":{}}"#).unwrap();

        // Create an empty manifest (so the file appears as "modified")
        let nexus_dir = workspace.join(".nexus");
        fs::create_dir_all(&nexus_dir).unwrap();
        fs::write(nexus_dir.join("sync-manifest.json"), "{}").unwrap();

        // Stash
        save(workspace).unwrap();

        // Verify stash exists
        let stash_base = workspace.join(STASH_DIR);
        assert!(stash_base.exists());
        let stashes: Vec<_> = fs::read_dir(&stash_base)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(stashes.len(), 1);

        // Modify the workspace file
        fs::write(
            workspace.join("devbox.json"),
            r#"{"packages":{"new":"pkg"}}"#,
        )
        .unwrap();

        // Pop
        pop(workspace).unwrap();

        // Verify content restored
        let content = fs::read_to_string(workspace.join("devbox.json")).unwrap();
        assert_eq!(content, r#"{"packages":{}}"#);

        // Verify stash is gone
        let stashes: Vec<_> = fs::read_dir(&stash_base)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(stashes.len(), 0);
    }

    #[test]
    fn test_detect_modified_includes_agent_content_not_projection() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        fs::create_dir_all(ws.join(".nexus")).unwrap();
        fs::create_dir_all(ws.join(".rtk")).unwrap();
        fs::write(ws.join(".cursorrules"), "edited").unwrap();
        fs::write(ws.join(".nexus/AGENTS.md"), "edited").unwrap();
        fs::write(ws.join(".github-unchanged.md"), "unchanged").unwrap();
        fs::write(ws.join(".rtk/filters.toml"), "edited").unwrap();
        let manifest = serde_json::json!({
            "cursorrules": {"target_path": ".cursorrules", "hash": sha256_hex("original")},
            "AGENTS.md": {"target_path": ".nexus/AGENTS.md", "hash": sha256_hex("original")},
            "unchanged": {"target_path": ".github-unchanged.md", "hash": sha256_hex("unchanged")},
            "rtk-filters-default": {"target_path": ".rtk/filters.toml", "hash": sha256_hex("original")}
        });
        fs::write(ws.join(SYNC_MANIFEST), manifest.to_string()).unwrap();

        // AGENTS.md and RTK filters are projections: never stashed.
        let modified: Vec<PathBuf> = detect_modified_files(ws)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(modified, vec![ws.join(".cursorrules")]);
    }

    fn stash_dirs(ws: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(ws.join(STASH_DIR))
            .map(|r| {
                r.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs
    }

    fn workspace_with_manifest(manifest: serde_json::Value) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".nexus")).unwrap();
        fs::write(dir.path().join(SYNC_MANIFEST), manifest.to_string()).unwrap();
        dir
    }

    #[test]
    fn test_stash_names_are_unique_and_ordered() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("stash");
        let ts = "2026-11-24T10-00-59Z";
        let names: Vec<String> = (0..3)
            .map(|_| create_unique_stash_dir(&base, ts).unwrap().1)
            .collect();
        assert_eq!(
            names,
            [ts, "2026-11-24T10-00-59Z-001", "2026-11-24T10-00-59Z-002"]
        );
        let mut sorted = names.clone();
        sorted.push("2026-11-24T10-01-00Z".to_string());
        sorted.sort();
        assert_eq!(sorted.last().unwrap(), "2026-11-24T10-01-00Z");
        assert_eq!(&sorted[..3], &names[..]);
    }

    #[test]
    fn test_two_saves_in_one_second_keep_both_stashes() {
        let dir = workspace_with_manifest(serde_json::json!({}));
        let ws = dir.path();
        fs::write(ws.join("devbox.json"), "first").unwrap();
        save(ws).unwrap();
        fs::write(ws.join("devbox.json"), "second").unwrap();
        save(ws).unwrap();
        // Whatever the clock did, both snapshots exist.
        assert_eq!(stash_dirs(ws).len(), 2);
        fs::write(ws.join("devbox.json"), "third").unwrap();
        pop(ws).unwrap();
        assert_eq!(
            fs::read_to_string(ws.join("devbox.json")).unwrap(),
            "second"
        );
        pop(ws).unwrap();
        assert_eq!(fs::read_to_string(ws.join("devbox.json")).unwrap(), "first");
        assert!(stash_dirs(ws).is_empty());
    }

    #[test]
    fn test_an_agent_file_named_like_the_metadata_survives() {
        let dir = workspace_with_manifest(serde_json::json!({
            "notes": {"target_path": "stash.json", "hash": sha256_hex("original")}
        }));
        let ws = dir.path();
        fs::write(ws.join("stash.json"), "my edit").unwrap();
        save(ws).unwrap();
        fs::write(ws.join("stash.json"), "original").unwrap();
        pop(ws).unwrap();
        assert_eq!(
            fs::read_to_string(ws.join("stash.json")).unwrap(),
            "my edit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pop_restores_the_executable_bit_of_scripts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace_with_manifest(serde_json::json!({}));
        let ws = dir.path();
        fs::create_dir_all(ws.join("scripts/devbox")).unwrap();
        let script = ws.join("scripts/devbox/run.sh");
        fs::write(&script, "#!/bin/sh\necho edited\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(ws.join("scripts/devbox/data.txt"), "plain").unwrap();
        save(ws).unwrap();
        fs::remove_file(&script).unwrap();
        fs::remove_file(ws.join("scripts/devbox/data.txt")).unwrap();
        pop(ws).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&script), 0o755);
        assert_eq!(mode(&ws.join("scripts/devbox/data.txt")) & 0o111, 0);
    }

    #[test]
    fn test_pop_keeps_the_stash_when_a_file_cannot_be_restored() {
        let dir = workspace_with_manifest(serde_json::json!({
            "rules": {"target_path": ".cursorrules", "hash": sha256_hex("original")}
        }));
        let ws = dir.path();
        fs::write(ws.join("devbox.json"), "saved devbox").unwrap();
        fs::write(ws.join(".cursorrules"), "saved rules").unwrap();
        save(ws).unwrap();
        let stash = stash_dirs(ws).pop().unwrap();
        fs::remove_file(stash.join(PAYLOAD_DIR).join(".cursorrules")).unwrap();
        fs::write(ws.join("devbox.json"), "current").unwrap();

        let err = pop(ws).unwrap_err().to_string();
        assert!(err.contains("kept"), "{err}");
        // Nothing was written, the stash and the other saved file remain.
        assert_eq!(
            fs::read_to_string(ws.join("devbox.json")).unwrap(),
            "current"
        );
        assert!(stash.join(PAYLOAD_DIR).join("devbox.json").is_file());

        // An unsafe entry is not dropped either.
        let meta_path = stash.join(META_FILE);
        let mut meta: StashMeta =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.files = vec!["devbox.json".into(), "../outside".into()];
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
        assert!(pop(ws).is_err());
        assert!(stash.is_dir());
        assert_eq!(
            fs::read_to_string(ws.join("devbox.json")).unwrap(),
            "current"
        );
    }

    #[test]
    fn test_pop_reads_stashes_of_the_older_layout() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let stash = ws.join(STASH_DIR).join("2026-01-01T00-00-00Z");
        fs::create_dir_all(&stash).unwrap();
        fs::write(stash.join("devbox.json"), "legacy").unwrap();
        fs::write(
            stash.join(META_FILE),
            r#"{"timestamp":"2026-01-01T00-00-00Z","files":["devbox.json"],"description":null}"#,
        )
        .unwrap();
        pop(ws).unwrap();
        assert_eq!(
            fs::read_to_string(ws.join("devbox.json")).unwrap(),
            "legacy"
        );
        assert!(!stash.exists());
    }

    #[test]
    fn test_a_final_newline_added_by_pull_is_not_stashed() {
        let dir = workspace_with_manifest(serde_json::json!({
            "devbox.json": {"hash": sha256_hex(r#"{"packages":{}}"#)}
        }));
        let ws = dir.path();
        fs::write(ws.join("devbox.json"), "{\"packages\":{}}\n").unwrap();
        assert!(detect_modified_files(ws).is_empty());
        fs::write(ws.join("devbox.json"), "{\"packages\":{\"x\":1}}\n").unwrap();
        assert_eq!(detect_modified_files(ws).len(), 1);
    }

    #[test]
    fn test_stash_list_empty() {
        let dir = TempDir::new().unwrap();
        list(dir.path()).unwrap(); // should not panic
    }

    #[test]
    fn test_chrono_lite_timestamp_format() {
        let ts = chrono_lite_timestamp();
        // Should match pattern YYYY-MM-DDTHH-MM-SSZ
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {}", ts);
        assert_eq!(ts.len(), 20, "unexpected length: {}", ts);
        assert!(ts.contains('T'), "missing T separator: {}", ts);
        // Year should be >= 2024
        let year: i64 = ts[..4].parse().unwrap();
        assert!(year >= 2024, "year too low: {}", year);
        // Month should be 1-12
        let month: i64 = ts[5..7].parse().unwrap();
        assert!((1..=12).contains(&month), "invalid month: {}", month);
        // Day should be 1-31
        let day: i64 = ts[8..10].parse().unwrap();
        assert!((1..=31).contains(&day), "invalid day: {}", day);
    }

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2025));
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(1900));
    }
}
