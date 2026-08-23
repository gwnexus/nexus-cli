//! The `nexus stash` command.
//!
//! Temporarily save local workspace changes to a stash directory,
//! restore them later, or list available stashes.

use console::style;
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

/// Metadata for a stash entry.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct StashMeta {
    timestamp: String,
    files: Vec<String>,
    description: Option<String>,
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
                    // Check if hash differs from manifest
                    let current_hash = sha256_hex(&content);
                    let manifest_hash = manifest
                        .get(tracked)
                        .and_then(|v| v.get("hash"))
                        .and_then(|h| h.as_str())
                        .unwrap_or("");
                    if current_hash != manifest_hash {
                        modified.push((full_path, content));
                    }
                }
                Err(e) => warn!("Failed to read {}: {}", full_path.display(), e),
            }
        } else if full_path.is_dir() {
            collect_modified_in_dir(workspace, &full_path, &manifest, &mut modified);
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
                        let current_hash = sha256_hex(&content);
                        let manifest_hash = manifest
                            .get(&rel)
                            .and_then(|v| v.get("hash"))
                            .and_then(|h| h.as_str())
                            .unwrap_or("");
                        if current_hash != manifest_hash {
                            modified.push((path, content));
                        }
                    }
                    Err(e) => warn!("Failed to read {}: {}", path.display(), e),
                }
            }
        }
    }
}

/// `nexus stash` — save local workspace changes to a stash.
pub fn save(workspace: &Path) -> anyhow::Result<()> {
    let modified = detect_modified_files(workspace);

    if modified.is_empty() {
        println!(
            "   {} No modified workspace files to stash.",
            style("✓").green()
        );
        return Ok(());
    }

    // Create stash directory with timestamp
    let timestamp = chrono_lite_timestamp();
    let stash_path = workspace.join(STASH_DIR).join(&timestamp);
    fs::create_dir_all(&stash_path)?;

    let mut stashed_files = Vec::new();

    for (file_path, content) in &modified {
        let rel = file_path
            .strip_prefix(workspace)
            .unwrap_or(file_path)
            .display()
            .to_string();

        // Preserve directory structure in stash
        let target = stash_path.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        stashed_files.push(rel);
    }

    // Write stash metadata
    let meta = StashMeta {
        timestamp: timestamp.clone(),
        files: stashed_files.clone(),
        description: None,
    };
    let meta_path = stash_path.join("stash.json");
    fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

    println!(
        "   {} Stashed {} file(s) → {}",
        style("✓").green().bold(),
        stashed_files.len(),
        style(&timestamp).dim(),
    );
    for f in &stashed_files {
        println!("     {}", f);
    }

    Ok(())
}

/// `nexus stash pop` — restore the most recent stash.
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

    let meta_path = latest.join("stash.json");
    let meta: StashMeta = if meta_path.exists() {
        serde_json::from_str(&fs::read_to_string(&meta_path)?)?
    } else {
        anyhow::bail!("Stash metadata not found: {}", meta_path.display());
    };

    // Restore files
    for file in &meta.files {
        let source = latest.join(file);
        let target = workspace.join(file);
        if source.exists() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target)?;
        }
    }

    // Remove the stash
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

/// `nexus stash list` — list all available stashes.
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
        let meta_path = stash_path.join("stash.json");
        if let Ok(content) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<StashMeta>(&content) {
                let label = if i == 0 { " (latest)" } else { "" };
                println!(
                    "   {} {}{} — {} file(s)",
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
