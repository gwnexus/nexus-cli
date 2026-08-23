//! The `nexus push` command.
//!
//! Detects local workspace changes and pushes them as a new workspace fork
//! to the linked Nexus project.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::warn;

/// Directory containing workspace scripts.
const SCRIPTS_DIR: &str = "scripts/devbox";

/// Collect local workspace file hashes for comparison.
pub fn collect_workspace_hashes(workspace: &Path) -> HashMap<String, String> {
    let mut hashes = HashMap::new();

    // devbox.json
    let devbox_path = workspace.join("devbox.json");
    if devbox_path.exists() {
        match fs::read_to_string(&devbox_path) {
            Ok(content) => {
                hashes.insert("devbox.json".to_string(), sha256_hex(&content));
            }
            Err(e) => warn!("Failed to read devbox.json: {}", e),
        }
    }

    // scripts/devbox/**
    let scripts_dir = workspace.join(SCRIPTS_DIR);
    if scripts_dir.is_dir() {
        collect_script_hashes(workspace, &scripts_dir, &mut hashes);
    }

    hashes
}

/// Recursively collect script file hashes using workspace-relative paths.
fn collect_script_hashes(workspace: &Path, dir: &Path, hashes: &mut HashMap<String, String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_script_hashes(workspace, &path, hashes);
            } else if path.is_file() {
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        let rel = path.strip_prefix(workspace).unwrap_or(&path);
                        let key = rel.display().to_string();
                        hashes.insert(key, sha256_hex(&content));
                    }
                    Err(e) => warn!("Failed to read {}: {}", path.display(), e),
                }
            }
        }
    }
}

/// Read local workspace files for push payload.
fn read_workspace_files(
    workspace: &Path,
) -> (Option<serde_json::Value>, Option<HashMap<String, String>>) {
    // Read devbox.json
    let devbox_json = workspace
        .join("devbox.json")
        .exists()
        .then(|| {
            fs::read_to_string(workspace.join("devbox.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        })
        .flatten();

    // Read script files
    let scripts_dir = workspace.join(SCRIPTS_DIR);
    let script_files = if scripts_dir.is_dir() {
        let mut files = HashMap::new();
        read_scripts_recursive(&scripts_dir, &scripts_dir, &mut files);
        if files.is_empty() {
            None
        } else {
            Some(files)
        }
    } else {
        None
    };

    (devbox_json, script_files)
}

/// Recursively read script files into a HashMap.
fn read_scripts_recursive(base: &Path, dir: &Path, files: &mut HashMap<String, String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                read_scripts_recursive(base, &path, files);
            } else if path.is_file() {
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        let rel = path.strip_prefix(base).unwrap_or(&path);
                        files.insert(rel.display().to_string(), content);
                    }
                    Err(e) => warn!("Failed to read {}: {}", path.display(), e),
                }
            }
        }
    }
}

/// Sync manifest path.
const SYNC_MANIFEST: &str = ".nexus/sync-manifest.json";

/// Check if a file path is tracked in the sync manifest (proving Nexus origin).
fn is_manifest_tracked(key: &str, manifest: &serde_json::Value) -> bool {
    // Direct key lookup
    if manifest.get(key).is_some() {
        return true;
    }
    // Search by target_path value
    manifest.as_object().is_some_and(|m| {
        m.values()
            .any(|v| v.get("target_path").and_then(|p| p.as_str()) == Some(key))
    })
}

/// `nexus push` — push local workspace changes to the platform.
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    fork_name: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;

    println!(
        "{} Detecting workspace changes...",
        style(">>").bold().cyan()
    );
    println!("   Project: {}", style(&project_id).dim());
    println!();

    // Origin guard: require sync manifest (proves nexus pull was run)
    let manifest_path = workspace.join(SYNC_MANIFEST);
    if !manifest_path.exists() {
        anyhow::bail!(
            "No sync manifest found. Run 'nexus pull' first to establish a Nexus baseline."
        );
    }
    let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    if manifest.as_object().is_none_or(|m| m.is_empty()) {
        anyhow::bail!(
            "Sync manifest is empty. Run 'nexus pull' first to establish a Nexus baseline."
        );
    }

    // Collect local hashes
    let all_hashes = collect_workspace_hashes(&workspace);
    if all_hashes.is_empty() {
        println!("   {} No workspace files found.", style("!").yellow());
        return Ok(());
    }

    // Filter to only manifest-tracked files (origin guard)
    let mut local_hashes = HashMap::new();
    for (key, hash) in all_hashes {
        if is_manifest_tracked(&key, &manifest) {
            local_hashes.insert(key, hash);
        } else {
            println!(
                "     {} {} (not tracked by Nexus, skipped)",
                style("S").dim(),
                key
            );
        }
    }

    if local_hashes.is_empty() {
        anyhow::bail!("No Nexus-managed workspace files found. Run 'nexus pull' first.");
    }

    // Compare with server
    let status = client
        .file_status(&project_id, local_hashes.clone())
        .await?;

    let workspace_modified: Vec<_> = status
        .modified
        .iter()
        .filter(|m| m.category == "workspace")
        .collect();

    if workspace_modified.is_empty() && status.new_local.is_empty() {
        println!(
            "   {} All workspace files are in sync. Nothing to push.",
            style("✓").green()
        );
        return Ok(());
    }

    // Show what would be pushed
    println!("   {} files to push:", style("Files").bold());
    for m in &workspace_modified {
        println!("     {} {}", style("M").yellow().bold(), m.path);
    }
    for n in &status.new_local {
        println!("     {} {}", style("A").green().bold(), n.path);
    }
    println!();

    if dry_run {
        println!(
            "   {} Dry run — no changes pushed.",
            style("--dry-run").dim()
        );
        return Ok(());
    }

    // Read files and push
    let (devbox_json, script_files) = read_workspace_files(&workspace);

    let name_display = fork_name.unwrap_or("(auto-generated)");
    println!(
        "   {} Pushing as fork: {}",
        style(">>").bold().cyan(),
        style(name_display).bold()
    );

    let result = client
        .workspace_push(&project_id, devbox_json, script_files, fork_name)
        .await?;

    println!();
    println!(
        "   {} Fork created: {} (v{})",
        style("✓").green().bold(),
        style(&result.fork_name).bold(),
        result.version,
    );
    println!(
        "     Previous fork archived: {}",
        style(&result.previous_fork_name).dim()
    );
    println!("     Files pushed: {}", result.files_pushed.join(", "));
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_collect_workspace_hashes_empty_dir() {
        let dir = TempDir::new().unwrap();
        let hashes = collect_workspace_hashes(dir.path());
        assert!(hashes.is_empty());
    }

    #[test]
    fn test_collect_workspace_hashes_with_devbox() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("devbox.json"), "test content").unwrap();
        let hashes = collect_workspace_hashes(dir.path());
        assert!(hashes.contains_key("devbox.json"));
        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn test_collect_workspace_hashes_with_scripts() {
        let dir = TempDir::new().unwrap();
        let scripts = dir.path().join("scripts/devbox");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("init.sh"), "#!/bin/bash").unwrap();
        fs::write(scripts.join("post.sh"), "echo done").unwrap();
        let hashes = collect_workspace_hashes(dir.path());
        assert_eq!(hashes.len(), 2);
        // Keys use workspace-relative paths (not script: prefix)
        assert!(hashes.contains_key("scripts/devbox/init.sh"));
        assert!(hashes.contains_key("scripts/devbox/post.sh"));
    }

    #[test]
    fn test_read_workspace_files_empty() {
        let dir = TempDir::new().unwrap();
        let (devbox, scripts) = read_workspace_files(dir.path());
        assert!(devbox.is_none());
        assert!(scripts.is_none());
    }

    #[test]
    fn test_read_workspace_files_with_devbox() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("devbox.json"), r#"{"packages":{}}"#).unwrap();
        let (devbox, scripts) = read_workspace_files(dir.path());
        assert!(devbox.is_some());
        assert!(scripts.is_none());
    }

    #[test]
    fn test_is_manifest_tracked_direct_key() {
        let manifest = serde_json::json!({
            "devbox.json": { "hash": "abc", "target_path": "devbox.json" }
        });
        assert!(is_manifest_tracked("devbox.json", &manifest));
        assert!(!is_manifest_tracked("unknown.json", &manifest));
    }

    #[test]
    fn test_is_manifest_tracked_by_target_path() {
        let manifest = serde_json::json!({
            "workspace-devbox": { "hash": "abc", "target_path": "devbox.json" }
        });
        assert!(is_manifest_tracked("devbox.json", &manifest));
    }

    #[test]
    fn test_is_manifest_tracked_scripts() {
        let manifest = serde_json::json!({
            "scripts/devbox/init.sh": { "hash": "abc", "target_path": "scripts/devbox/init.sh" }
        });
        assert!(is_manifest_tracked("scripts/devbox/init.sh", &manifest));
        assert!(!is_manifest_tracked("scripts/devbox/custom.sh", &manifest));
    }

    #[test]
    fn test_origin_guard_filters_untracked() {
        let dir = TempDir::new().unwrap();

        // Create workspace files
        fs::write(dir.path().join("devbox.json"), "{}").unwrap();
        let scripts = dir.path().join("scripts/devbox");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("tracked.sh"), "#!/bin/bash").unwrap();
        fs::write(scripts.join("untracked.sh"), "#!/bin/bash").unwrap();

        // Manifest only tracks devbox.json and tracked.sh
        let manifest = serde_json::json!({
            "devbox.json": { "hash": "old", "target_path": "devbox.json" },
            "scripts/devbox/tracked.sh": { "hash": "old", "target_path": "scripts/devbox/tracked.sh" }
        });

        let all_hashes = collect_workspace_hashes(dir.path());
        assert_eq!(all_hashes.len(), 3);

        // Filter by manifest
        let tracked: HashMap<String, String> = all_hashes
            .into_iter()
            .filter(|(k, _)| is_manifest_tracked(k, &manifest))
            .collect();
        assert_eq!(tracked.len(), 2);
        assert!(tracked.contains_key("devbox.json"));
        assert!(tracked.contains_key("scripts/devbox/tracked.sh"));
        assert!(!tracked.contains_key("scripts/devbox/untracked.sh"));
    }
}
