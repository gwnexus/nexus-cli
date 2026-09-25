//! Agent-file sync with the Nexus platform: the sync manifest and the
//! single-file push used by `nexus push` (the former `nexus sync status|
//! push|reset` commands are deprecated aliases of `nexus status|push|reset`,
//! NEXUS-APP dispatch b5f7bfb0).
//!
//! Part of the sync protocol defined in ADR-0036.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use std::fs;
use std::path::Path;

/// Path to the local sync manifest file that stores content hashes.
const SYNC_MANIFEST: &str = ".nexus/sync-manifest.json";

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
    let local_hash = sha256_hex(&content);

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

/// Public accessor for the sync manifest, used by pull.rs for local-modification detection.
pub fn load_manifest_pub(workspace: &Path) -> serde_json::Value {
    load_manifest(workspace)
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
