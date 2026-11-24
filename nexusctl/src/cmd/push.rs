//! The `nexus push` command.
//!
//! Detects local workspace changes and pushes them as a new workspace fork
//! to the linked Nexus project.

use crate::cmd::workspace_state::{is_generated_script, is_retired_script};
use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use tracing::warn;

/// Directory containing workspace scripts.
pub(crate) const SCRIPTS_DIR: &str = "scripts/devbox";

/// Collect local workspace file hashes for comparison.
pub fn collect_workspace_hashes(workspace: &Path) -> HashMap<String, String> {
    collect_workspace_files(workspace).0
}

/// Local workspace file hashes, plus the workspace files that exist but
/// cannot be read (as text).
pub fn collect_workspace_files(workspace: &Path) -> (HashMap<String, String>, Vec<String>) {
    let mut hashes = HashMap::new();
    let mut unreadable = Vec::new();

    // devbox.json
    let devbox_path = workspace.join("devbox.json");
    if devbox_path.exists() && super::pull::readable_inside(workspace, "devbox.json") {
        match fs::read_to_string(&devbox_path) {
            Ok(content) => {
                hashes.insert("devbox.json".to_string(), sha256_hex(&content));
            }
            Err(e) => {
                warn!("Failed to read devbox.json: {}", e);
                unreadable.push("devbox.json".to_string());
            }
        }
    }

    // scripts/devbox/**, without the machine-local scripts
    let scripts_dir = workspace.join(SCRIPTS_DIR);
    if scripts_root_ok(workspace) {
        collect_script_hashes(workspace, &scripts_dir, &mut hashes, &mut unreadable);
    }
    let manifest = super::sync::load_manifest_pub(workspace);
    hashes.retain(|path, _| !super::sync::is_local_script(&manifest, path));
    unreadable.retain(|path| !super::sync::is_local_script(&manifest, path));
    unreadable.sort();

    (hashes, unreadable)
}

/// Whether the scripts directory is a real directory inside the
/// workspace (not a symlink, not leading out of it).
fn scripts_root_ok(workspace: &Path) -> bool {
    let dir = workspace.join(SCRIPTS_DIR);
    dir.is_dir()
        && !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
        && super::pull::readable_inside(workspace, SCRIPTS_DIR)
}

/// Recursively collect script file hashes using workspace-relative paths.
/// Symlinks are skipped (never followed), the same set as the upload.
fn collect_script_hashes(
    workspace: &Path,
    dir: &Path,
    hashes: &mut HashMap<String, String>,
    unreadable: &mut Vec<String>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Git metadata of a nested repository is never content.
            if super::pull::is_git_dir_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                collect_script_hashes(workspace, &path, hashes, unreadable);
            } else if kind.is_file() {
                let rel = path.strip_prefix(workspace).unwrap_or(&path);
                let key = rel.display().to_string();
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        hashes.insert(key, sha256_hex(&content));
                    }
                    Err(e) => {
                        warn!("Failed to read {}: {}", path.display(), e);
                        unreadable.push(key);
                    }
                }
            }
        }
    }
}

/// Stop when a file selected from the first scan is missing from the
/// payload read afterwards (it vanished or became a symlink in between):
/// the fork is built from the upload alone, so it would drop the file.
fn ensure_payload_complete(
    workspace: &Path,
    selected: &[String],
    devbox_json: Option<&serde_json::Value>,
    scripts: Option<&HashMap<String, String>>,
) -> anyhow::Result<()> {
    // A devbox.json that is there but does not parse is the user's to
    // fix; retrying would fail the same way.
    if devbox_json.is_none() && selected.iter().any(|p| p == "devbox.json") {
        let path = workspace.join("devbox.json");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            if let Ok(raw) = fs::read_to_string(&path) {
                if let Err(e) = serde_json::from_str::<serde_json::Value>(&raw) {
                    anyhow::bail!("push stopped: devbox.json is not valid JSON ({e}); fix it and run nexus push again");
                }
            }
        }
    }
    let prefix = format!("{SCRIPTS_DIR}/");
    let missing: Vec<&str> = selected
        .iter()
        .filter(|path| match path.strip_prefix(&prefix) {
            Some(rel) => !scripts.is_some_and(|s| s.contains_key(rel)),
            None if path.as_str() == "devbox.json" => devbox_json.is_none(),
            None => false,
        })
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "push stopped: {} changed while reading (missing or now a symlink); nothing was pushed, run nexus push again",
            missing.join(", ")
        );
    }
    Ok(())
}

/// Whether `key` lies outside the paths `nexus push <path>` selected
/// (never, without a selection).
fn outside_selection(key: &str, only: Option<&[String]>) -> bool {
    only.is_some_and(|sel| !sel.iter().any(|p| p == key))
}

/// The fork files outside the selection, with their fork bodies: the new
/// fork carries them unchanged. Generated scripts are left to the backend.
fn kept_fork_files(
    desired: &HashMap<String, String>,
    only: Option<&[String]>,
) -> HashMap<String, String> {
    desired
        .iter()
        .filter(|(key, _)| outside_selection(key, only) && !is_generated_script(key))
        .map(|(key, body)| (key.clone(), body.clone()))
        .collect()
}

/// The push payload with the kept fork files in place of local ones.
#[allow(clippy::type_complexity)]
fn with_kept_fork_files(
    devbox_json: Option<serde_json::Value>,
    scripts: Option<HashMap<String, String>>,
    kept: &HashMap<String, String>,
) -> anyhow::Result<(Option<serde_json::Value>, Option<HashMap<String, String>>)> {
    let devbox_json = match kept.get("devbox.json") {
        Some(body) => Some(serde_json::from_str(body).map_err(|e| {
            anyhow::anyhow!("push stopped: the fork's devbox.json is not valid JSON ({e})")
        })?),
        None => devbox_json,
    };
    let prefix = format!("{SCRIPTS_DIR}/");
    let kept_scripts: Vec<(String, String)> = kept
        .iter()
        .filter_map(|(key, body)| {
            key.strip_prefix(&prefix)
                .map(|rel| (rel.to_string(), body.clone()))
        })
        .collect();
    let scripts = if kept_scripts.is_empty() {
        scripts
    } else {
        let mut files = scripts.unwrap_or_default();
        files.extend(kept_scripts);
        Some(files)
    };
    Ok((devbox_json, scripts))
}

/// Read local workspace files for push payload.
fn read_workspace_files(
    workspace: &Path,
) -> (Option<serde_json::Value>, Option<HashMap<String, String>>) {
    // Read devbox.json
    let devbox_json = (workspace.join("devbox.json").exists()
        && super::pull::readable_inside(workspace, "devbox.json"))
    .then(|| {
        fs::read_to_string(workspace.join("devbox.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    })
    .flatten();

    // Read script files
    let scripts_dir = workspace.join(SCRIPTS_DIR);
    let script_files = if scripts_root_ok(workspace) {
        let mut files = HashMap::new();
        read_scripts_recursive(&scripts_dir, &scripts_dir, &mut files);
        let manifest = super::sync::load_manifest_pub(workspace);
        files.retain(|rel, _| {
            !super::sync::is_local_script(&manifest, &format!("{SCRIPTS_DIR}/{rel}"))
        });
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

/// Recursively read script files into a HashMap. Symlinks are skipped
/// (never followed), as in [`collect_script_hashes`].
fn read_scripts_recursive(base: &Path, dir: &Path, files: &mut HashMap<String, String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Git metadata of a nested repository is never content.
            if super::pull::is_git_dir_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                read_scripts_recursive(base, &path, files);
            } else if kind.is_file() {
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

/// How a local workspace file differs from what the fork delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Modified,
    Added,
    Deleted,
}

/// A file the backend never delivers, and the fork does not carry: not
/// pushed. A fork override reverted to the template is a change instead,
/// one that drops it from the fork. Without the fork export a tracked file
/// is assumed to be in the fork.
fn skip_as_retired(key: &str, local: &str, in_fork: bool, exported: bool, tracked: bool) -> bool {
    is_retired_script(key, local) && !in_fork && (exported || !tracked)
}

fn read_or_empty(workspace: &Path, path: &str) -> String {
    fs::read_to_string(workspace.join(path)).unwrap_or_default()
}

/// A script of the fork that an earlier pull wrote and that is gone
/// locally. Generated scripts and files the backend drops anyway do not
/// count.
fn is_deleted_fork_file(
    key: &str,
    body: &str,
    manifest: &serde_json::Value,
    workspace: &Path,
) -> bool {
    key != "devbox.json"
        && !is_generated_script(key)
        && !is_retired_script(key, body)
        && is_manifest_tracked(key, manifest)
        && !workspace.join(key).exists()
}

/// Compare one local workspace file with the baseline `nexus status` uses:
/// the fork export when it is available (`exported`), otherwise the hash
/// recorded at the last pull. `desired` is the file's body in the export.
/// Differences only in trailing newlines are not changes.
fn workspace_change(
    local: &str,
    recorded: Option<&str>,
    desired: Option<&str>,
    exported: bool,
) -> Option<Change> {
    use crate::cmd::workspace_state::{classify_workspace_file, State};
    match desired {
        Some(body) => match classify_workspace_file(Some(local.as_bytes()), recorded, body)? {
            State::Modified | State::Conflict | State::Unmanaged => Some(Change::Modified),
            _ => None,
        },
        None if exported => Some(Change::Added),
        None => match recorded {
            Some(r) if nexus_core::hash::hash_matches_text(r, local) => None,
            Some(_) => Some(Change::Modified),
            None => Some(Change::Added),
        },
    }
}

/// The fork's body when the local file is unchanged since the last pull
/// and the fork has a newer version: the push must carry that version, not
/// the stale local copy.
fn newer_fork_body<'a>(
    local: &str,
    recorded: Option<&str>,
    desired: Option<&'a String>,
) -> Option<&'a String> {
    let body = desired?;
    (nexus_core::hash::hash_matches_text(recorded?, local)
        && !nexus_core::hash::text_equivalent(local, body))
    .then_some(body)
}

/// Hash recorded in the sync manifest for a workspace path.
fn recorded_hash<'a>(key: &str, manifest: &'a serde_json::Value) -> Option<&'a str> {
    manifest
        .get(key)
        .and_then(|e| e.get("hash"))
        .and_then(|h| h.as_str())
        .or_else(|| {
            manifest.as_object()?.values().find_map(|e| {
                (e.get("target_path")?.as_str()? == key)
                    .then(|| e.get("hash")?.as_str())
                    .flatten()
            })
        })
}

/// Verify the target project exists and is reachable on the configured backend.
///
/// Maps API errors to actionable messages so a wrong `NEXUS_API_URL` (project on a
/// different environment) or an auth problem is not misreported as a missing baseline.
async fn verify_project(
    client: &NexusClient,
    project_id: &str,
    api_url: &str,
) -> anyhow::Result<()> {
    use nexus_core::error::Error as CoreError;
    match client.get_project(project_id).await {
        Ok(_) => Ok(()),
        Err(CoreError::NotFound(_)) => anyhow::bail!(
            "Project {project_id} was not found on {api_url}.\n\
             You may be targeting the wrong backend. Check NEXUS_API_URL (or --api-url); \
             the project may live on a different environment (e.g. prod vs staging)."
        ),
        Err(CoreError::Unauthorized(msg)) => anyhow::bail!(
            "Authentication failed against {api_url}: {msg}\n\
             Run 'nexus login' or check NEXUS_PRIVATE_TOKEN."
        ),
        Err(CoreError::Forbidden(msg)) => {
            anyhow::bail!("Access denied to project {project_id} on {api_url}: {msg}")
        }
        Err(e) => Err(e.into()),
    }
}

/// Publish the current local workspace files as a new fork, bypassing the
/// sync-manifest origin guard. Used by `nexus push --adopt-local` to bootstrap
/// a fork from an untracked-but-existing local workspace.
async fn adopt_local_push(
    client: &NexusClient,
    workspace: &Path,
    project_id: &str,
    fork_name: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut all_hashes = collect_workspace_hashes(workspace);
    all_hashes.retain(|k, _| {
        !is_generated_script(k)
            && !is_retired_script(
                k,
                &fs::read_to_string(workspace.join(k)).unwrap_or_default(),
            )
    });
    if all_hashes.is_empty() {
        anyhow::bail!(
            "No local workspace files found (expected devbox.json and/or scripts/devbox/**). \
             Nothing to adopt."
        );
    }

    println!(
        "   {} adopt-local: publishing the current local workspace as a new fork (origin guard bypassed).",
        style("!").yellow()
    );
    println!("   {} files to push:", style("Files").bold());
    let mut keys: Vec<_> = all_hashes.keys().cloned().collect();
    keys.sort();
    for key in &keys {
        println!("     {} {}", style("A").green().bold(), key);
    }
    println!();
    print_fork_plan(fork_name, None);

    if dry_run {
        println!(
            "   {} Dry run: no changes pushed.",
            style("--dry-run").dim()
        );
        return Ok(());
    }

    let (devbox_json, script_files) = read_workspace_files(workspace);
    let script_files = script_files
        .map(|files| {
            files
                .into_iter()
                .filter(|(rel, body)| {
                    let path = format!("{SCRIPTS_DIR}/{rel}");
                    !is_generated_script(&path) && !is_retired_script(&path, body)
                })
                .collect::<HashMap<_, _>>()
        })
        .filter(|files| !files.is_empty());
    ensure_payload_complete(
        workspace,
        &keys,
        devbox_json.as_ref(),
        script_files.as_ref(),
    )?;

    let result = client
        .workspace_push(project_id, devbox_json, script_files, fork_name)
        .await?;

    print_push_result(&result);
    Ok(())
}

/// Print the outcome of a successful workspace push.
fn print_push_result(result: &nexus_core::api::WorkspacePushResponse) {
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
}

/// `nexus push`: push local workspace changes to the platform.
///
/// Tracked files the current fork no longer contains, and untracked local
/// scripts, are skipped unless listed in `include_named` (named explicitly,
/// e.g. `nexus push <path>`). Generated scripts are never pushed.
///
/// With `only` (the workspace paths `nexus push <path>` selected), only
/// those files are pushed; every other file keeps its version from the
/// current fork, so local edits outside the selection stay local.
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    fork_name: Option<&str>,
    dry_run: bool,
    adopt_local: bool,
    include_named: &[String],
    only: Option<&[String]>,
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
    let project_name =
        crate::cmd::display::resolve_project_display_name(&client, &project_id, Some(&workspace))
            .await;
    crate::cmd::display::print_project_banner(api_url, &project_id, project_name.as_deref());
    println!();

    // Verify the project exists on the targeted backend before anything else.
    // This turns the previously misleading "run nexus pull first" into a precise
    // wrong-backend / auth signal when the project is simply not on this API URL.
    verify_project(&client, &project_id, api_url).await?;

    // Bootstrap path: publish the current local workspace as a new fork without
    // requiring a prior pull (which could clobber newer local edits with an older
    // server fork). Bypasses the sync-manifest origin guard by explicit opt-in.
    if adopt_local {
        return adopt_local_push(&client, &workspace, &project_id, fork_name, dry_run).await;
    }

    // Origin guard: require sync manifest (proves nexus pull was run)
    let manifest_path = workspace.join(SYNC_MANIFEST);
    if !manifest_path.exists() {
        anyhow::bail!(
            "Workspace scope is not initialized locally: no sync manifest at {SYNC_MANIFEST}.\n\
             Run 'nexus pull --scope workspace' to establish a baseline, or\n\
             run 'nexus push --adopt-local' to publish the current local workspace as a new fork."
        );
    }
    let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    if manifest.as_object().is_none_or(|m| m.is_empty()) {
        anyhow::bail!(
            "Sync manifest at {SYNC_MANIFEST} is empty.\n\
             Run 'nexus pull --scope workspace' to establish a baseline, or\n\
             run 'nexus push --adopt-local' to publish the current local workspace as a new fork."
        );
    }

    // Collect local hashes
    let (all_hashes, unreadable) = collect_workspace_files(&workspace);
    if all_hashes.is_empty() && unreadable.is_empty() {
        println!("   {} No workspace files found.", style("!").yellow());
        return Ok(());
    }

    // The current fork, the same baseline `nexus status` compares with;
    // `None` when there is none, then local files are compared with the
    // sync manifest and nothing is treated as dropped. A fork that cannot
    // be read stops the push: the new fork would be built without it.
    let export = match client.export_workspace_mcp(&project_id).await {
        Ok(export) => Some(export),
        Err(e) if super::pull::is_no_workspace_fork(&e) => None,
        Err(e) => anyhow::bail!(
            "push stopped: the current workspace fork could not be read ({e}); nothing was pushed"
        ),
    };
    // A file that should go into the new fork but cannot be read would be
    // dropped from it silently.
    let blocked: Vec<&String> = unreadable
        .iter()
        .filter(|k| !outside_selection(k, only))
        .filter(|k| {
            is_manifest_tracked(k, &manifest)
                || include_named.iter().any(|p| p == *k)
                || export.as_ref().is_some_and(|e| {
                    k.as_str() == "devbox.json" || e.scripts.iter().any(|s| &s.path == *k)
                })
        })
        .collect();
    if !blocked.is_empty() {
        anyhow::bail!(
            "push stopped: {} cannot be read; fix the file(s) and push again (nothing was pushed)",
            blocked
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let exported = export.as_ref().map(exported_paths);
    if only.is_some() && export.is_none() {
        anyhow::bail!(
            "push stopped: the current workspace fork could not be read, so the files outside the selection cannot keep their fork version; nothing was pushed"
        );
    }
    let outside = |key: &str| outside_selection(key, only);
    let active_fork = export.as_ref().and_then(active_fork_label);
    let desired: HashMap<String, String> = export
        .map(|e| {
            std::iter::once(("devbox.json".to_string(), e.devbox_json))
                .chain(e.scripts.into_iter().map(|s| (s.path, s.body)))
                .collect()
        })
        .unwrap_or_default();

    // Files to push (manifest-tracked or named explicitly) and their changes.
    let mut tracked = HashSet::new();
    let mut changes = Vec::new();
    // Files unchanged locally whose fork version is newer: the new fork
    // takes the fork version, not the stale local copy.
    let mut newer_in_fork: Vec<(String, String)> = Vec::new();
    let mut sorted: Vec<_> = all_hashes.into_keys().collect();
    sorted.sort();
    for key in sorted {
        // Outside the selection: the fork version is kept (below).
        if outside(&key) {
            continue;
        }
        let local = fs::read_to_string(workspace.join(&key)).map_err(|e| {
            anyhow::anyhow!("push stopped: {key} cannot be read ({e}); nothing was pushed")
        })?;
        if is_generated_script(&key) {
            // The backend generates these and strips them from a push.
            if desired
                .get(&key)
                .is_some_and(|d| !nexus_core::hash::text_equivalent(&local, d))
            {
                println!(
                    "     {} {} (generated by Nexus, not pushed; put local changes into {SCRIPTS_DIR}/init/os_<os>/override.sh or an own script, or nexus reset {})",
                    style("G").dim(),
                    key,
                    key
                );
            }
            continue;
        }
        if skip_as_retired(
            &key,
            &local,
            desired.contains_key(&key),
            exported.is_some(),
            is_manifest_tracked(&key, &manifest),
        ) {
            println!(
                "     {} {} (Nexus never delivers or accepts it, not pushed; remove it with git rm {})",
                style("R").dim(),
                key,
                key
            );
            continue;
        }
        let named = include_named.iter().any(|p| p == &key);
        if !is_manifest_tracked(&key, &manifest) && !named {
            println!(
                "     {} {} (not tracked by Nexus, skipped; nexus push {} adds it)",
                style("S").dim(),
                key,
                key
            );
            continue;
        }
        if is_dropped(&key, exported.as_deref(), include_named) {
            println!(
                "     {} {} (no longer in the workspace fork, skipped; nexus push {} adds it back)",
                style("D").dim(),
                key,
                key
            );
            continue;
        }
        let recorded = recorded_hash(&key, &manifest);
        if let Some(change) = workspace_change(
            &local,
            recorded,
            desired.get(&key).map(String::as_str),
            exported.is_some(),
        ) {
            changes.push((change, key.clone()));
        } else if let Some(body) = newer_fork_body(&local, recorded, desired.get(&key)) {
            newer_in_fork.push((key.clone(), body.clone()));
        }
        tracked.insert(key);
    }

    // Fork files deleted locally: the new fork is built from the pushed
    // files only, so leaving them out removes them from it.
    let mut deleted: Vec<String> = desired
        .iter()
        .filter(|(key, _)| !outside(key))
        .filter(|(key, body)| is_deleted_fork_file(key, body, &manifest, &workspace))
        .map(|(key, _)| key.clone())
        .collect();
    deleted.sort();
    // Fork files outside the selection go into the new fork unchanged.
    let mut kept = kept_fork_files(&desired, only);
    kept.extend(newer_in_fork);
    tracked.extend(kept.keys().cloned());
    changes.extend(deleted.iter().map(|k| (Change::Deleted, k.clone())));

    if tracked.is_empty() && deleted.is_empty() {
        anyhow::bail!(
            "No workspace files are tracked in the sync manifest (only agent files were pulled).\n\
             Run 'nexus pull --scope workspace' to track the workspace files, or\n\
             run 'nexus push --adopt-local' to publish the current local files as a new fork\n\
             without pulling (avoids overwriting newer local edits with an older server fork)."
        );
    }

    if changes.is_empty() {
        println!(
            "   {} All workspace files are in sync. Nothing to push.",
            style("✓").green()
        );
        return Ok(());
    }

    // Show what would be pushed
    println!("   {} files to push:", style("Files").bold());
    for (change, path) in &changes {
        match change {
            Change::Modified if is_retired_script(path, &read_or_empty(&workspace, path)) => {
                println!(
                    "     {} {} (back to the template; the new fork no longer carries it)",
                    style("M").yellow().bold(),
                    path
                )
            }
            Change::Modified => println!("     {} {}", style("M").yellow().bold(), path),
            Change::Added => println!("     {} {}", style("A").green().bold(), path),
            Change::Deleted => println!(
                "     {} {} (deleted locally; the new fork no longer carries it)",
                style("D").red().bold(),
                path
            ),
        }
    }
    println!();
    print_fork_plan(fork_name, active_fork.as_deref());

    if dry_run {
        println!(
            "   {} Dry run: no changes pushed.",
            style("--dry-run").dim()
        );
        return Ok(());
    }

    // Read files and push: only the files Nexus tracks, the same set the
    // comparison above used (untracked local scripts were reported as
    // skipped but still uploaded into the fork).
    let (devbox_json, script_files) = read_workspace_files(&workspace);
    let devbox_json = devbox_json
        .filter(|_| tracked.contains("devbox.json") && !kept.contains_key("devbox.json"));
    let script_files = script_files
        .map(|files| {
            files
                .into_iter()
                .filter(|(rel, _)| tracked.contains(&format!("{SCRIPTS_DIR}/{rel}")))
                .collect::<HashMap<_, _>>()
        })
        // An empty set still replaces the fork's scripts when files were
        // deleted; without deletions it would push nothing.
        .filter(|files| !files.is_empty() || !deleted.is_empty())
        .or_else(|| (!deleted.is_empty()).then(HashMap::new));
    let (devbox_json, script_files) = with_kept_fork_files(devbox_json, script_files, &kept)?;
    let selected: Vec<String> = tracked.iter().cloned().collect();
    ensure_payload_complete(
        &workspace,
        &selected,
        devbox_json.as_ref(),
        script_files.as_ref(),
    )?;

    let result = client
        .workspace_push(&project_id, devbox_json, script_files, fork_name)
        .await?;

    print_push_result(&result);

    Ok(())
}

/// How the active fork is named in the push plan: its name, else its id.
fn active_fork_label(export: &nexus_core::api::WorkspaceForkExportResponse) -> Option<String> {
    export
        .fork_name
        .clone()
        .or_else(|| export.fork_id.as_ref().map(|id| format!("id {id}")))
}

/// The push plan's fork lines: a push always creates a new fork and the
/// backend archives the active one, so say so before (and in a dry run).
fn fork_plan_lines(fork_name: Option<&str>, active_fork: Option<&str>) -> [String; 2] {
    [
        format!(
            "New fork:       {}",
            fork_name.unwrap_or("(auto-generated name)")
        ),
        format!(
            "Archived fork:  {} (it stays in the dashboard and can be activated again)",
            active_fork.unwrap_or("the active fork, if any")
        ),
    ]
}

fn print_fork_plan(fork_name: Option<&str>, active_fork: Option<&str>) {
    for line in fork_plan_lines(fork_name, active_fork) {
        println!("   {line}");
    }
    println!();
}

/// Workspace-relative paths of the files in a fork export.
fn exported_paths(export: &nexus_core::api::WorkspaceForkExportResponse) -> Vec<String> {
    std::iter::once("devbox.json".to_string())
        .chain(export.scripts.iter().map(|s| s.path.clone()))
        .collect()
}

/// A tracked file the fork no longer contains and that was not named
/// explicitly: a new fork must not bring it back by accident.
fn is_dropped(key: &str, exported: Option<&[String]>, include_named: &[String]) -> bool {
    exported.is_some_and(|ex| !ex.iter().any(|p| p == key))
        && !include_named.iter().any(|p| p == key)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn test_push_scans_do_not_follow_symlinks() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("x.sh"), "external").unwrap();
        fs::write(outside.path().join("devbox.json"), "{\"external\": true}").unwrap();
        fs::create_dir_all(ws.path().join(SCRIPTS_DIR)).unwrap();
        fs::write(ws.path().join(SCRIPTS_DIR).join("ok.sh"), "inside").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("x.sh"),
            ws.path().join(SCRIPTS_DIR).join("link.sh"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join(SCRIPTS_DIR).join("dir"))
            .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("devbox.json"),
            ws.path().join("devbox.json"),
        )
        .unwrap();
        let hashes = collect_workspace_hashes(ws.path());
        assert!(hashes.contains_key("scripts/devbox/ok.sh"));
        assert_eq!(hashes.len(), 1, "{hashes:?}");
        let (devbox, scripts) = read_workspace_files(ws.path());
        assert!(devbox.is_none());
        let scripts = scripts.unwrap();
        assert_eq!(scripts.keys().collect::<Vec<_>>(), vec!["ok.sh"]);
        // A linked scripts root is not scanned at all.
        let ws2 = tempfile::tempdir().unwrap();
        fs::create_dir_all(ws2.path().join("scripts")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws2.path().join(SCRIPTS_DIR)).unwrap();
        assert!(collect_workspace_hashes(ws2.path()).is_empty());
        assert!(read_workspace_files(ws2.path()).1.is_none());
    }

    #[test]
    fn test_push_scans_skip_nested_git_metadata() {
        let ws = tempfile::tempdir().unwrap();
        let scripts = ws.path().join(SCRIPTS_DIR);
        fs::create_dir_all(scripts.join("vendor/.git")).unwrap();
        fs::write(scripts.join("vendor/.git/config"), "secret").unwrap();
        fs::create_dir_all(scripts.join("other/.GIT")).unwrap();
        fs::write(scripts.join("other/.GIT/config"), "secret").unwrap();
        fs::write(scripts.join("vendor/run.sh"), "ok").unwrap();
        let hashes = collect_workspace_hashes(ws.path());
        assert_eq!(
            hashes.keys().collect::<Vec<_>>(),
            vec!["scripts/devbox/vendor/run.sh"]
        );
        let scripts = read_workspace_files(ws.path()).1.unwrap();
        assert_eq!(scripts.keys().collect::<Vec<_>>(), vec!["vendor/run.sh"]);
    }

    #[test]
    fn test_ensure_payload_complete_names_missing_files() {
        let scripts = HashMap::from([("a.sh".to_string(), "x".to_string())]);
        let devbox = serde_json::json!({});
        let sel = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ws = tempfile::tempdir().unwrap();
        assert!(ensure_payload_complete(
            ws.path(),
            &sel(&["devbox.json", "scripts/devbox/a.sh"]),
            Some(&devbox),
            Some(&scripts)
        )
        .is_ok());
        let err = ensure_payload_complete(
            ws.path(),
            &sel(&["devbox.json", "scripts/devbox/a.sh", "scripts/devbox/b.sh"]),
            None,
            Some(&scripts),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("devbox.json") && err.contains("scripts/devbox/b.sh"),
            "{err}"
        );
        assert!(!err.contains("a.sh,"), "{err}");
        // A malformed devbox.json is reported as such.
        fs::write(ws.path().join("devbox.json"), "{ not json").unwrap();
        let err = ensure_payload_complete(ws.path(), &sel(&["devbox.json"]), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn test_fork_plan_names_the_archived_fork() {
        let [new, archived] = super::fork_plan_lines(Some("rust-toolchain"), Some("gitleaks"));
        assert!(new.ends_with("rust-toolchain"));
        assert!(archived.contains("gitleaks (it stays in the dashboard"));
        let [new, archived] = super::fork_plan_lines(None, None);
        assert!(new.ends_with("(auto-generated name)"));
        assert!(archived.contains("the active fork, if any"));
    }

    #[test]
    fn test_active_fork_label_prefers_the_name() {
        let mut export: nexus_core::api::WorkspaceForkExportResponse =
            serde_json::from_value(serde_json::json!({
                "devbox_json": "{}",
                "meta": {},
                "fork_id": "00000000-0000-4000-8000-000000000002"
            }))
            .unwrap();
        assert_eq!(
            super::active_fork_label(&export).as_deref(),
            Some("id 00000000-0000-4000-8000-000000000002")
        );
        export.fork_name = Some("gitleaks".into());
        assert_eq!(
            super::active_fork_label(&export).as_deref(),
            Some("gitleaks")
        );
    }

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_fork_overrides_reverted_to_the_template_are_pushed() {
        let path = "scripts/devbox/init/os_darwin/override.sh";
        let template = "#!/usr/bin/env bash\n# local changes go here\n";
        // In the fork: a change, not retired.
        assert!(!skip_as_retired(path, template, true, true, true));
        // Not in the fork, export known: retired.
        assert!(skip_as_retired(path, template, false, true, true));
        assert!(skip_as_retired(path, template, false, true, false));
        // No export: tracked files are assumed to be in the fork.
        assert!(!skip_as_retired(path, template, false, false, true));
        assert!(skip_as_retired(path, template, false, false, false));
        // A custom override is never retired.
        assert!(!skip_as_retired(path, "export A=1\n", false, true, false));
    }

    #[test]
    fn test_deleted_fork_files() {
        let dir = TempDir::new().unwrap();
        let manifest = serde_json::json!({
            "a": {"target_path": "scripts/devbox/init/rust_toolchain.sh", "hash": "x"},
            "b": {"target_path": "scripts/devbox/init/kept.sh", "hash": "x"},
            "c": {"target_path": "scripts/devbox/dbx_init.sh", "hash": "x"},
            "d": {"target_path": "scripts/devbox/init/os_linux/override.sh", "hash": "x"}
        });
        fs::create_dir_all(dir.path().join("scripts/devbox/init")).unwrap();
        fs::write(dir.path().join("scripts/devbox/init/kept.sh"), "x").unwrap();
        let ws = dir.path();
        assert!(is_deleted_fork_file(
            "scripts/devbox/init/rust_toolchain.sh",
            "echo\n",
            &manifest,
            ws
        ));
        assert!(!is_deleted_fork_file(
            "scripts/devbox/init/kept.sh",
            "x",
            &manifest,
            ws
        ));
        // Generated, never pulled, or a placeholder the backend drops anyway.
        assert!(!is_deleted_fork_file(
            "scripts/devbox/dbx_init.sh",
            "x",
            &manifest,
            ws
        ));
        assert!(!is_deleted_fork_file(
            "scripts/devbox/init/other.sh",
            "x",
            &manifest,
            ws
        ));
        assert!(!is_deleted_fork_file(
            "scripts/devbox/init/os_linux/override.sh",
            "# t\n",
            &manifest,
            ws
        ));
        assert!(is_deleted_fork_file(
            "scripts/devbox/init/os_linux/override.sh",
            "export A=1\n",
            &manifest,
            ws
        ));
        assert!(!is_deleted_fork_file("devbox.json", "{}", &manifest, ws));
    }

    #[test]
    fn test_selection_keeps_fork_versions_outside_it() {
        let desired: HashMap<String, String> = [
            ("devbox.json", r#"{"packages":["fork"]}"#),
            ("scripts/devbox/a.sh", "fork a"),
            ("scripts/devbox/b.sh", "fork b"),
            ("scripts/devbox/dbx_init.sh", "generated"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        // No selection: nothing is kept.
        assert!(kept_fork_files(&desired, None).is_empty());

        let only = vec!["scripts/devbox/a.sh".to_string()];
        let kept = kept_fork_files(&desired, Some(&only));
        let mut keys: Vec<&str> = kept.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["devbox.json", "scripts/devbox/b.sh"]);
        assert!(!outside_selection("scripts/devbox/a.sh", Some(&only)));
        assert!(outside_selection("scripts/devbox/b.sh", Some(&only)));

        // Local edits outside the selection are replaced by the fork bodies.
        let local_devbox = serde_json::json!({"packages": ["local edit"]});
        let local_scripts: HashMap<String, String> = [
            ("a.sh".to_string(), "local a".to_string()),
            ("b.sh".to_string(), "local b".to_string()),
        ]
        .into();
        let (devbox, scripts) =
            with_kept_fork_files(Some(local_devbox), Some(local_scripts), &kept).unwrap();
        assert_eq!(devbox, Some(serde_json::json!({"packages": ["fork"]})));
        let scripts = scripts.unwrap();
        assert_eq!(scripts["a.sh"], "local a");
        assert_eq!(scripts["b.sh"], "fork b");

        // A kept script that is missing locally is carried too.
        let (_, scripts) = with_kept_fork_files(None, None, &kept).unwrap();
        assert_eq!(scripts.unwrap()["b.sh"], "fork b");

        // A fork devbox.json that does not parse stops the push.
        let bad: HashMap<String, String> = [("devbox.json".to_string(), "{".to_string())].into();
        assert!(with_kept_fork_files(None, None, &bad).is_err());
    }

    #[test]
    fn test_is_dropped() {
        let exported = vec![
            "devbox.json".to_string(),
            "scripts/devbox/dbx_init.sh".to_string(),
        ];
        let old = "scripts/devbox/dbx_tmux_2w.sh";
        assert!(is_dropped(old, Some(&exported), &[]));
        assert!(!is_dropped(
            "scripts/devbox/dbx_init.sh",
            Some(&exported),
            &[]
        ));
        // Named explicitly: pushed back into the fork.
        assert!(!is_dropped(old, Some(&exported), &[old.to_string()]));
        // No fork export available: nothing is treated as dropped.
        assert!(!is_dropped(old, None, &[]));
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
    fn test_workspace_change_against_export() {
        let server = "#!/usr/bin/env bash\necho fork";
        let recorded = sha256_hex(server);
        // Freshly pulled (pull adds the final newline): nothing to push.
        assert_eq!(
            workspace_change(&format!("{server}\n"), Some(&recorded), Some(server), true),
            None
        );
        // Same with no recorded hash at all: the export is the baseline.
        assert_eq!(
            workspace_change(&format!("{server}\n"), None, Some(server), true),
            None
        );
        // A real local edit.
        assert_eq!(
            workspace_change("echo mine\n", Some(&recorded), Some(server), true),
            Some(Change::Modified)
        );
        // Unchanged local, the fork moved on: an update, not a push.
        assert_eq!(
            workspace_change(
                &format!("{server}\n"),
                Some(&recorded),
                Some("echo new"),
                true
            ),
            None
        );
        // Named file the fork does not contain.
        assert_eq!(
            workspace_change("echo mine\n", None, None, true),
            Some(Change::Added)
        );
    }

    #[test]
    fn test_workspace_change_without_export() {
        let server = r#"{"packages":[]}"#;
        let recorded = sha256_hex(server);
        assert_eq!(
            workspace_change(&format!("{server}\n"), Some(&recorded), None, false),
            None
        );
        assert_eq!(
            workspace_change("{\"packages\":[1]}\n", Some(&recorded), None, false),
            Some(Change::Modified)
        );
        assert_eq!(
            workspace_change("echo mine\n", None, None, false),
            Some(Change::Added)
        );
    }

    #[test]
    fn test_recorded_hash_by_key_and_target_path() {
        let manifest = serde_json::json!({
            "devbox.json": { "hash": "a", "target_path": "devbox.json" },
            "workspace-x": { "hash": "b", "target_path": "scripts/devbox/x.sh" }
        });
        assert_eq!(recorded_hash("devbox.json", &manifest), Some("a"));
        assert_eq!(recorded_hash("scripts/devbox/x.sh", &manifest), Some("b"));
        assert_eq!(recorded_hash("scripts/devbox/y.sh", &manifest), None);
    }

    #[test]
    fn test_generated_scripts_are_not_pushed() {
        for path in [
            "scripts/devbox/dbx_init.sh",
            "scripts/devbox/dbx_post_init.sh",
            "scripts/devbox/lib/common.sh",
            "scripts/devbox/ops/validate.sh",
        ] {
            assert!(is_generated_script(path), "{path}");
        }
        assert!(!is_generated_script(
            "scripts/devbox/init/os_darwin/override.sh"
        ));
        assert!(!is_generated_script("devbox.json"));
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

    #[test]
    fn test_newer_fork_body_only_for_unchanged_local_files() {
        let fork = "v2\n".to_string();
        let v1 = sha256_hex("v1\n");
        // Unchanged since the pull, fork newer: the fork version goes.
        assert_eq!(newer_fork_body("v1\n", Some(&v1), Some(&fork)), Some(&fork));
        // Edited locally, same as the fork, or not recorded: no.
        assert_eq!(newer_fork_body("mine\n", Some(&v1), Some(&fork)), None);
        assert_eq!(
            newer_fork_body("v2\n", Some(&sha256_hex("v2\n")), Some(&fork)),
            None
        );
        assert_eq!(newer_fork_body("v1\n", None, Some(&fork)), None);
        assert_eq!(newer_fork_body("v1\n", Some(&v1), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_workspace_files_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(SCRIPTS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.sh"), "a").unwrap();
        fs::write(dir.join("b.sh"), [0xff, 0xfe]).unwrap();
        let (hashes, unreadable) = collect_workspace_files(ws.path());
        assert!(hashes.contains_key(&format!("{SCRIPTS_DIR}/a.sh")));
        // Not valid UTF-8: not readable as text.
        assert_eq!(unreadable, vec![format!("{SCRIPTS_DIR}/b.sh")]);
        fs::write(ws.path().join("devbox.json"), "{}").unwrap();
        fs::set_permissions(
            ws.path().join("devbox.json"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        if fs::read(ws.path().join("devbox.json")).is_err() {
            assert!(collect_workspace_files(ws.path())
                .1
                .contains(&"devbox.json".to_string()));
        }
    }
}
