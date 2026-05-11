//! The `nexus sync` command.
//!
//! Provides sync operations for agent files between the local workspace
//! and the Nexus platform:
//! - `nexus sync status` — compare local vs remote content hashes
//! - `nexus sync push <file>` — upload local changes to the platform
//! - `nexus sync reset <file>` — discard local changes and pull from platform
//!
//! Part of the sync protocol defined in ADR-0036.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::api::SyncFileHash;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Path to the local sync manifest file that stores content hashes.
const SYNC_MANIFEST: &str = ".nexus/sync-manifest.json";

/// Compute SHA-256 hash of content (matches server-side `computeContentHash`).
fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Load the sync manifest (file_key → { hash, target_path }).
fn load_manifest(workspace: &Path) -> serde_json::Value {
    let path = workspace.join(SYNC_MANIFEST);
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(val) = serde_json::from_str(&content) {
                return val;
            }
        }
    }
    serde_json::json!({})
}

/// Save the sync manifest.
fn save_manifest(workspace: &Path, manifest: &serde_json::Value) -> anyhow::Result<()> {
    let path = workspace.join(SYNC_MANIFEST);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(manifest)?;
    fs::write(&path, content)?;
    Ok(())
}

/// `nexus sync status` — show sync state of all agent files.
pub async fn status(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;

    println!("{} Checking sync status...", style(">>").bold().cyan());
    println!("   Project: {}", style(&project_id).dim());
    println!();

    // Load local manifest and compute current file hashes
    let manifest = load_manifest(&workspace);
    let manifest_obj = manifest.as_object();

    // Build file hash list from manifest
    let mut file_hashes: Vec<SyncFileHash> = Vec::new();
    if let Some(entries) = manifest_obj {
        for (file_key, info) in entries {
            let target_path = info["target_path"].as_str().unwrap_or("");
            let local_path = workspace.join(target_path);

            let local_hash = if local_path.exists() {
                let content = fs::read_to_string(&local_path)?;
                compute_hash(&content)
            } else {
                // File was deleted locally
                String::new()
            };

            file_hashes.push(SyncFileHash {
                file_key: file_key.clone(),
                local_hash,
            });
        }
    }

    if file_hashes.is_empty() {
        // No manifest — try getting status from server directly
        match client.sync_status(&project_id).await {
            Ok(resp) => {
                if resp.files.is_empty() {
                    println!(
                        "   {} No agent files configured for this project.",
                        style("--").yellow()
                    );
                } else {
                    println!(
                        "   {} {} agent file(s):",
                        style("+").bold().green(),
                        resp.files.len()
                    );
                    println!();
                    for f in &resp.files {
                        let status_style = match f.sync_status.as_str() {
                            "synced" => style(&f.sync_status).green(),
                            "unknown" => style(&f.sync_status).yellow(),
                            _ => style(&f.sync_status).red(),
                        };
                        let source = f.body_override_source.as_deref().unwrap_or("-");
                        println!(
                            "   {:<30} {} (source: {}, last sync: {})",
                            style(&f.name).bold(),
                            status_style,
                            source,
                            f.last_synced_at.as_deref().unwrap_or("never"),
                        );
                    }
                    println!();
                    println!(
                        "   {} Run {} to pull agent files and initialize sync tracking.",
                        style("*").cyan(),
                        style("nexus pull --scope agents").bold()
                    );
                }
            }
            Err(e) => {
                println!(
                    "   {} Could not fetch sync status: {}",
                    style("!").bold().red(),
                    e
                );
            }
        }
        return Ok(());
    }

    // Check against server
    match client.sync_check(&project_id, &file_hashes).await {
        Ok(resp) => {
            let mut synced = 0;
            let mut drifted = 0;

            println!(
                "   {} {} file(s) tracked:",
                style("+").bold().green(),
                resp.results.len()
            );
            println!();

            for r in &resp.results {
                let icon = match r.status.as_str() {
                    "synced" => {
                        synced += 1;
                        style("=").green()
                    }
                    "local_modified" => {
                        drifted += 1;
                        style("M").yellow()
                    }
                    "remote_modified" => {
                        drifted += 1;
                        style("R").cyan()
                    }
                    "conflict" => {
                        drifted += 1;
                        style("C").red()
                    }
                    _ => {
                        drifted += 1;
                        style("?").dim()
                    }
                };

                println!("   [{}] {}", icon, r.file_key);
            }

            if !resp.deprecated_skills.is_empty() {
                println!();
                println!(
                    "   {} Deprecated skills detected:",
                    style("!").bold().yellow()
                );
                for s in &resp.deprecated_skills {
                    println!("      {}", style(s).dim());
                }
            }

            println!();
            if drifted == 0 {
                println!(
                    "   {} All {} file(s) in sync.",
                    style("OK").bold().green(),
                    synced
                );
            } else {
                println!(
                    "   {} in sync, {} drifted",
                    synced,
                    style(drifted).bold().yellow()
                );
                println!(
                    "   Run {} to push local changes, or {} to discard them.",
                    style("nexus sync push <file>").bold(),
                    style("nexus sync reset <file>").bold()
                );
            }
        }
        Err(e) => {
            println!("   {} Sync check failed: {}", style("!").bold().red(), e);
        }
    }

    Ok(())
}

/// `nexus sync push <file>` — upload local agent file changes to the platform.
pub async fn push(
    api_url: &str,
    cli_project_id: Option<&str>,
    file_key: &str,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;

    // Look up target path from manifest
    let manifest = load_manifest(&workspace);
    let target_path = manifest[file_key]["target_path"].as_str().ok_or_else(|| {
        anyhow::anyhow!(
            "File '{}' not found in sync manifest. Run 'nexus pull' first.",
            file_key
        )
    })?;

    let local_path = workspace.join(target_path);
    if !local_path.exists() {
        anyhow::bail!("Local file not found: {}", target_path);
    }

    let content = fs::read_to_string(&local_path)?;
    let local_hash = compute_hash(&content);

    println!(
        "{} Pushing {} to platform...",
        style(">>").bold().cyan(),
        style(file_key).bold()
    );

    match client
        .sync_file(
            &project_id,
            file_key,
            "push",
            Some(&content),
            Some(&local_hash),
        )
        .await
    {
        Ok(resp) => {
            // Update manifest with new hash
            let mut manifest = load_manifest(&workspace);
            if let Some(entry) = manifest.get_mut(file_key) {
                entry["hash"] = serde_json::json!(resp.new_hash.as_deref().unwrap_or(&local_hash));
            }
            save_manifest(&workspace, &manifest)?;

            println!(
                "   {} {} pushed successfully.",
                style("OK").bold().green(),
                file_key
            );
            if let Some(msg) = &resp.message {
                println!("   {}", style(msg).dim());
            }
        }
        Err(e) => {
            println!("   {} Push failed: {}", style("!").bold().red(), e);
        }
    }

    Ok(())
}

/// `nexus sync reset <file>` — discard local changes and pull from platform.
pub async fn reset(
    api_url: &str,
    cli_project_id: Option<&str>,
    file_key: &str,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;

    // Look up target path from manifest
    let manifest = load_manifest(&workspace);
    let target_path = manifest[file_key]["target_path"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "File '{}' not found in sync manifest. Run 'nexus pull' first.",
                file_key
            )
        })?
        .to_string();

    println!(
        "{} Resetting {} from platform...",
        style(">>").bold().cyan(),
        style(file_key).bold()
    );

    match client
        .sync_file(&project_id, file_key, "pull", None, None)
        .await
    {
        Ok(resp) => {
            if let Some(body) = &resp.body {
                let local_path = workspace.join(&target_path);
                if let Some(parent) = local_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&local_path, body)?;

                // Update manifest
                let new_hash = resp.new_hash.as_deref().unwrap_or("");
                let computed_hash = if new_hash.is_empty() {
                    compute_hash(body)
                } else {
                    new_hash.to_string()
                };

                let mut manifest = load_manifest(&workspace);
                if let Some(entry) = manifest.get_mut(file_key) {
                    entry["hash"] = serde_json::json!(computed_hash);
                }
                save_manifest(&workspace, &manifest)?;

                println!(
                    "   {} {} reset to platform version.",
                    style("OK").bold().green(),
                    file_key
                );
            } else {
                println!(
                    "   {} Server returned no content for {}.",
                    style("!").bold().yellow(),
                    file_key
                );
            }
        }
        Err(e) => {
            println!("   {} Reset failed: {}", style("!").bold().red(), e);
        }
    }

    Ok(())
}

/// Update the sync manifest after a `nexus pull` writes agent files.
/// Called from pull.rs to record content hashes for each exported file.
pub fn update_manifest_after_pull(
    workspace: &Path,
    file_key: &str,
    target_path: &str,
    content_hash: &str,
) -> anyhow::Result<()> {
    let mut manifest = load_manifest(workspace);
    let obj = manifest
        .as_object_mut()
        .expect("manifest should be an object");
    obj.insert(
        file_key.to_string(),
        serde_json::json!({
            "target_path": target_path,
            "hash": content_hash,
        }),
    );
    save_manifest(workspace, &manifest)
}
