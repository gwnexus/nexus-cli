//! Agent-file sync with the Nexus platform: the sync manifest and the
//! single-file push used by `nexus push` (the former `nexus sync status|
//! push|reset` commands are deprecated aliases of `nexus status|push|reset`).
//!
//! Part of the sync protocol.

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

/// Change the sync manifest under its lock (see [`super::json_store`]):
/// re-read, apply `change`, write back atomically. A manifest that cannot
/// be parsed is an error, never reset to empty.
fn update_manifest<T>(
    workspace: &Path,
    change: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> T,
) -> anyhow::Result<T> {
    super::json_store::update_object(&workspace.join(SYNC_MANIFEST), change)
}

/// `nexus sync push <file>`: upload local agent file changes to the platform.
/// Every failure is returned as an error; the caller prints it.
pub async fn push(
    api_url: &str,
    cli_project_id: Option<&str>,
    file_key: &str,
    expected_path: &str,
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

    // The manifest entry must name the path the caller selected, so the
    // upload is the file that was shown.
    if expected_path != target_path {
        anyhow::bail!(
            "{file_key}: the sync manifest names {target_path}, not {expected_path}; run nexus pull"
        );
    }
    // Only a file inside the workspace is read for the upload.
    super::pull::validate_agent_file_target_path(&workspace, target_path)?;
    let local_path = workspace.join(target_path);
    if !local_path.exists() {
        anyhow::bail!("Local file not found: {}", target_path);
    }

    let content = fs::read_to_string(&local_path)?;
    let local_hash = sha256_hex(&content);
    // The binding the upload belongs to: its record is updated afterwards
    // only if no other command changed it meanwhile.
    let recorded_hash = manifest[file_key]["hash"].as_str().map(str::to_string);
    let target_path = target_path.to_string();

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
            let new_hash = resp.new_hash.as_deref().unwrap_or(&local_hash).to_string();
            let updated = update_manifest(&workspace, |m| {
                record_pushed_hash(
                    m,
                    file_key,
                    &target_path,
                    recorded_hash.as_deref(),
                    &new_hash,
                )
            })?;
            if !updated {
                println!(
                    "   {} {} was pushed, but its local record changed meanwhile; run nexus pull.",
                    style("!").bold().yellow(),
                    file_key
                );
            }

            println!(
                "   {} {} pushed successfully.",
                style("OK").bold().green(),
                file_key
            );
            if let Some(msg) = &resp.message {
                println!("   {}", style(msg).dim());
            }
        }
        Err(e) => return Err(anyhow::anyhow!("Push failed: {e}")),
    }

    Ok(())
}

/// Set the hash of `file_key` after a push, only when the entry is still
/// the binding that was uploaded (same target path and previous hash):
/// another command may have re-bound or re-pulled it meanwhile.
fn record_pushed_hash(
    manifest: &mut serde_json::Map<String, serde_json::Value>,
    file_key: &str,
    target_path: &str,
    previous_hash: Option<&str>,
    new_hash: &str,
) -> bool {
    let Some(entry) = manifest.get_mut(file_key) else {
        return false;
    };
    let same_binding = entry["target_path"].as_str() == Some(target_path)
        && entry["hash"].as_str() == previous_hash;
    if same_binding {
        entry["hash"] = serde_json::json!(new_hash);
    }
    same_binding
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
    update_manifest(workspace, |m| {
        m.insert(
            file_key.to_string(),
            serde_json::json!({
                "target_path": target_path,
                "hash": content_hash,
            }),
        );
    })
}

/// Record a devbox workspace file (`devbox.json`, a script) as last written
/// by pull. The `kind` marker lets a later pull find the workspace files it
/// wrote and remove the ones the fork no longer contains.
pub fn update_workspace_manifest_after_pull(
    workspace: &Path,
    path: &str,
    content_hash: &str,
) -> anyhow::Result<()> {
    update_manifest(workspace, |m| {
        m.insert(
            path.to_string(),
            serde_json::json!({
                "target_path": path,
                "hash": content_hash,
                "kind": WORKSPACE_KIND,
            }),
        );
    })
}

/// `kind` of sync-manifest entries for devbox workspace files.
const WORKSPACE_KIND: &str = "workspace";

/// `kind` of sync-manifest entries for machine-local scripts
/// (`ws_export.local_scripts`).
const LOCAL_SCRIPT_KIND: &str = "local_script";

/// Default path of the machine-local script; the backend rejects it in a
/// workspace push.
const DEFAULT_LOCAL_SCRIPT: &str = "scripts/devbox/init/local.sh";

/// Record a machine-local script as last written by pull.
pub fn update_local_script_manifest_after_pull(
    workspace: &Path,
    path: &str,
    content_hash: &str,
) -> anyhow::Result<()> {
    update_manifest(workspace, |m| {
        m.insert(
            path.to_string(),
            serde_json::json!({
                "target_path": path,
                "hash": content_hash,
                "kind": LOCAL_SCRIPT_KIND,
            }),
        );
    })
}

/// The machine-local scripts recorded in the sync manifest, as
/// `(target_path, hash)`.
pub fn local_script_manifest_entries(manifest: &serde_json::Value) -> Vec<(String, String)> {
    let Some(obj) = manifest.as_object() else {
        return Vec::new();
    };
    obj.values()
        .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some(LOCAL_SCRIPT_KIND))
        .filter_map(|e| {
            Some((
                e.get("target_path")?.as_str()?.to_string(),
                e.get("hash")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Whether `path` is a machine-local script: the default path or one pull
/// recorded as such. These are never pushed, shown or diffed.
pub fn is_local_script(manifest: &serde_json::Value, path: &str) -> bool {
    path == DEFAULT_LOCAL_SCRIPT
        || local_script_manifest_entries(manifest)
            .iter()
            .any(|(p, _)| p == path)
}

/// The devbox workspace files recorded in the sync manifest, as
/// `(target_path, hash)`. Entries written before the `kind` marker existed
/// are recognized by their shape: keyed by their own path, and
/// `devbox.json` or below `scripts/devbox/`.
pub fn workspace_manifest_entries(manifest: &serde_json::Value) -> Vec<(String, String)> {
    let Some(obj) = manifest.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(key, entry)| {
            let target = entry.get("target_path")?.as_str()?;
            let hash = entry.get("hash")?.as_str()?;
            let kind = entry.get("kind").and_then(|k| k.as_str());
            let marked = kind == Some(WORKSPACE_KIND);
            let legacy = kind.is_none()
                && key == target
                && (target == "devbox.json"
                    || target.starts_with(&format!("{}/", super::push::SCRIPTS_DIR)));
            (marked || legacy).then(|| (target.to_string(), hash.to_string()))
        })
        .collect()
}

/// Drop the sync-manifest entries whose `target_path` matches `remove`
/// (e.g. files of a projection that was just removed). Returns the number
/// of entries dropped; the manifest is only rewritten when that is > 0.
pub fn remove_manifest_entries(
    workspace: &Path,
    remove: impl Fn(&str) -> bool,
) -> anyhow::Result<usize> {
    remove_manifest_entries_with(workspace, |p, _| remove(p))
}

/// [`remove_manifest_entries`] with the recorded hash passed along.
pub fn remove_manifest_entries_with(
    workspace: &Path,
    remove: impl Fn(&str, Option<&str>) -> bool,
) -> anyhow::Result<usize> {
    update_manifest(workspace, |m| {
        let before = m.len();
        m.retain(|key, entry| {
            let target = entry
                .get("target_path")
                .and_then(|t| t.as_str())
                .unwrap_or(key);
            !remove(target, entry.get("hash").and_then(|h| h.as_str()))
        });
        before - m.len()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pushed_hash_only_for_the_uploaded_binding() {
        let mut m = serde_json::json!({
            "AGENTS.md": { "target_path": ".nexus/AGENTS.md", "hash": "old" }
        })
        .as_object()
        .unwrap()
        .clone();
        // Re-bound or re-pulled meanwhile: untouched.
        assert!(!record_pushed_hash(
            &mut m,
            "AGENTS.md",
            ".nexus/OTHER.md",
            Some("old"),
            "new"
        ));
        assert!(!record_pushed_hash(
            &mut m,
            "AGENTS.md",
            ".nexus/AGENTS.md",
            Some("x"),
            "new"
        ));
        assert!(!record_pushed_hash(
            &mut m,
            "gone",
            ".nexus/AGENTS.md",
            Some("old"),
            "new"
        ));
        assert_eq!(m["AGENTS.md"]["hash"], "old");
        assert!(record_pushed_hash(
            &mut m,
            "AGENTS.md",
            ".nexus/AGENTS.md",
            Some("old"),
            "new"
        ));
        assert_eq!(m["AGENTS.md"]["hash"], "new");
    }

    #[test]
    fn test_corrupt_sync_manifest_is_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".nexus")).unwrap();
        fs::write(dir.path().join(SYNC_MANIFEST), "{ broken").unwrap();
        assert!(update_workspace_manifest_after_pull(dir.path(), "devbox.json", "h").is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join(SYNC_MANIFEST)).unwrap(),
            "{ broken"
        );
    }
}
