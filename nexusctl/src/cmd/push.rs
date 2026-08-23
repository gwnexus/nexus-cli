//! The `nexus push` command.
//!
//! Detects local workspace changes and pushes them as a new workspace fork
//! to the linked Nexus project.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Directory containing workspace scripts.
const SCRIPTS_DIR: &str = "scripts/devbox";

/// Compute SHA-256 hash of content (matches server-side `computeContentHash`).
fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Collect local workspace file hashes for comparison.
pub fn collect_workspace_hashes(workspace: &Path) -> HashMap<String, String> {
    let mut hashes = HashMap::new();

    // devbox.json
    let devbox_path = workspace.join("devbox.json");
    if devbox_path.exists() {
        if let Ok(content) = fs::read_to_string(&devbox_path) {
            hashes.insert("devbox.json".to_string(), compute_hash(&content));
        }
    }

    // scripts/devbox/**
    let scripts_dir = workspace.join(SCRIPTS_DIR);
    if scripts_dir.is_dir() {
        collect_script_hashes(&scripts_dir, &scripts_dir, &mut hashes);
    }

    hashes
}

/// Recursively collect script file hashes.
fn collect_script_hashes(base: &Path, dir: &Path, hashes: &mut HashMap<String, String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_script_hashes(base, &path, hashes);
            } else if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    let rel = path.strip_prefix(base).unwrap_or(&path);
                    let key = format!("script:{}", rel.display());
                    hashes.insert(key, compute_hash(&content));
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
                if let Ok(content) = fs::read_to_string(&path) {
                    let rel = path.strip_prefix(base).unwrap_or(&path);
                    files.insert(rel.display().to_string(), content);
                }
            }
        }
    }
}

/// `nexus push` — push local workspace changes to the platform.
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    fork_name: Option<&str>,
    dry_run: bool,
    _workspace_only: bool,
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

    // Collect local hashes
    let local_hashes = collect_workspace_hashes(&workspace);
    if local_hashes.is_empty() {
        println!("   {} No workspace files found.", style("!").yellow());
        return Ok(());
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
    fn test_compute_hash_deterministic() {
        let h1 = compute_hash("hello");
        let h2 = compute_hash("hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn test_compute_hash_different_inputs() {
        let h1 = compute_hash("hello");
        let h2 = compute_hash("world");
        assert_ne!(h1, h2);
    }

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
        assert!(hashes.contains_key("script:init.sh"));
        assert!(hashes.contains_key("script:post.sh"));
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
}
