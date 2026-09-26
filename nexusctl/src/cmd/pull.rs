//! The `nexus pull` command.
//!
//! Pulls skills, commands, directives, and MCP configuration from the Nexus
//! platform into the current workspace. Requires a linked project (via
//! `nexus link` or `nexus init --project-id`) and valid authentication.
//!
//! This is the incremental sync counterpart to `nexus init`:
//! - `init` creates the full scaffold from scratch
//! - `pull` updates skills, commands, and directives in an existing workspace
//!
//! When existing files are detected, the user is prompted for confirmation
//! unless `--force` or `-y` is passed.

use console::style;
use nexus_core::api::ExportWarning;
use nexus_core::api::McpServerConfig;
use nexus_core::api::NexusClient;
use nexus_core::api::ProviderConfig;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use nexus_core::McpSource;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::claude_render;
use super::init::resolve_platform_plugins;
use super::shadow;

/// Marker in YAML frontmatter indicating the file is managed by Nexus CLI.
/// Files without this marker are considered user-managed and will not be
/// overwritten by `nexus pull`.
pub(crate) const MANAGED_MARKER: &str = "source: nexus-platform";

// ---------------------------------------------------------------------------
// URL allowlist for remote downloads (SEC-003)
//
// Plugin downloads and actor avatar downloads only fetch from these trusted
// domains. HTTP (non-TLS) and non-allowlisted hosts are refused to prevent
// SSRF if the Nexus API response is ever compromised.
// ---------------------------------------------------------------------------
const ALLOWED_DOWNLOAD_HOSTS: &[&str] = &[
    "nexus.gatewarden.eu",
    "cdn.gatewarden.eu",
    "raw.githubusercontent.com",
    "github.com",
    "objects.githubusercontent.com",
];

/// Validate a download URL against the trusted-host allowlist.
///
/// Returns `Ok(())` if the URL is safe to fetch, or an error describing
/// why it was rejected. Enforces HTTPS and domain allowlist.
pub fn validate_download_url(url: &str) -> anyhow::Result<()> {
    if !url.starts_with("https://") {
        anyhow::bail!(
            "refusing download: URL must use HTTPS (got: {})",
            url.chars().take(80).collect::<String>()
        );
    }
    // Extract host from https://host/path
    let host = url
        .strip_prefix("https://")
        .and_then(|s| s.split('/').next())
        .unwrap_or("");
    // Strip port if present
    let host_no_port = host.split(':').next().unwrap_or(host);

    if !ALLOWED_DOWNLOAD_HOSTS
        .iter()
        .any(|&allowed| host_no_port == allowed || host_no_port.ends_with(&format!(".{}", allowed)))
    {
        anyhow::bail!(
            "refusing download: host '{}' is not in the trusted allowlist",
            host_no_port
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Protected file patterns — NEVER overwritten, even with --force.
// These patterns match against the target_path (relative to workspace root).
// ---------------------------------------------------------------------------
const PROTECTED_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "id_rsa",
    "id_ed25519",
];

/// Check whether a target path matches a protected file pattern.
/// Protected files are NEVER overwritten by pull, regardless of --force.
pub fn is_protected_path(target_path: &str) -> bool {
    let filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(target_path);

    for pattern in PROTECTED_PATTERNS {
        if pattern.contains('*') {
            // Simple glob: *.ext
            if let Some(suffix) = pattern.strip_prefix('*') {
                if filename.ends_with(suffix) {
                    return true;
                }
            }
            // Prefix glob: prefix.*
            if let Some(prefix) = pattern.strip_suffix(".*") {
                if filename.starts_with(prefix) && filename.contains('.') {
                    return true;
                }
            }
        } else if filename == *pattern {
            return true;
        }
    }
    false
}

/// Validate an `ExportedAgentFile.target_path` before it is ever joined
/// onto a workspace root, rejecting anything that could escape the
/// workspace (NEXUS-APP ADR-0117 hardening item, dispatch bb782869).
///
/// The previous check only rejected `..` (parent-dir) components. That
/// missed a much simpler escape: `Path::join` on an *absolute* path
/// **replaces** the base entirely rather than nesting under it, so a
/// `target_path` of e.g. `/etc/passwd` or (on Windows) `C:\Windows\...`
/// would silently write outside the workspace with no `..` in sight.
///
/// This rejects any `RootDir`/`Prefix` component in addition to the
/// existing `ParentDir` check, then does one more pass in depth: joins
/// the (now known-relative) path onto `workspace`, lexically normalizes
/// the result (no filesystem access -- the target may not exist yet, so
/// this cannot use `fs::canonicalize`), and confirms it still starts
/// with the normalized workspace root.
pub fn validate_agent_file_target_path(workspace: &Path, target_path: &str) -> anyhow::Result<()> {
    let normalized = std::path::Path::new(target_path);
    for component in normalized.components() {
        match component {
            std::path::Component::ParentDir => {
                anyhow::bail!(
                    "refusing to write: target_path '{}' contains '..' traversal",
                    target_path
                );
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!(
                    "refusing to write: target_path '{}' is an absolute path",
                    target_path
                );
            }
            _ => {}
        }
    }

    let joined = workspace.join(normalized);
    let normalized_joined = lexically_normalize(&joined);
    let normalized_workspace = lexically_normalize(workspace);
    if !normalized_joined.starts_with(&normalized_workspace) {
        anyhow::bail!(
            "refusing to write: target_path '{}' escapes the workspace",
            target_path
        );
    }

    Ok(())
}

/// Lexically normalize a path (resolve `.`/`..` components without
/// touching the filesystem). Used by [`validate_agent_file_target_path`]
/// since the target of a not-yet-written file may not exist yet, ruling
/// out `fs::canonicalize`.
fn lexically_normalize(path: &Path) -> std::path::PathBuf {
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Check whether a local file was modified since the last pull.
///
/// Compares the current file content hash against the hash stored in the
/// sync manifest (`.nexus/sync-manifest.json`). Returns `true` if the file
/// exists, has a manifest entry, and the hashes differ.
fn is_locally_modified(workspace: &Path, target_path: &str, manifest: &serde_json::Value) -> bool {
    let file_path = workspace.join(target_path);
    if !file_path.exists() {
        return false;
    }

    // Look up manifest hash by target_path (searching values) or direct key
    let manifest_hash = manifest
        .as_object()
        .and_then(|obj| {
            obj.values().find_map(|entry| {
                let tp = entry.get("target_path")?.as_str()?;
                if tp == target_path {
                    entry.get("hash")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            manifest
                .get(target_path)
                .and_then(|v| v.get("hash"))
                .and_then(|h| h.as_str())
                .map(|s| s.to_string())
        });

    let Some(expected_hash) = manifest_hash else {
        return false;
    };

    match fs::read_to_string(&file_path) {
        Ok(content) => !nexus_core::hash::hash_matches(&expected_hash, &content),
        Err(_) => false,
    }
}

/// Write `.nexus/env` (or `.<agentic_root>/env`) from the platform-managed `plugin_env` map.
///
/// This file is the single source of truth for non-sensitive, platform-managed env vars
/// (e.g. `HEADROOM_*`). It is fully platform-owned — full overwrite on every pull/init.
/// Also ensures `env` is present in `.<agentic_root>/.gitignore`.
///
/// If `plugin_env` is empty the file is deleted (if it exists) and the function returns `Ok(())`.
pub(crate) fn write_plugin_env_file(
    workspace: &Path,
    plugin_env: &std::collections::HashMap<String, String>,
    project_name: &str,
    agentic_root: &str,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let env_dir = workspace.join(agentic_root);
    let env_path = env_dir.join("env");
    let gitignore_path = env_dir.join(".gitignore");

    if plugin_env.is_empty() {
        if env_path.exists() {
            let _ = fs::remove_file(&env_path);
            println!(
                "   {} {}/env removed (no plugin env vars)",
                style("-").bold().yellow(),
                agentic_root
            );
        }
        return Ok(());
    }

    // Build file content
    let timestamp = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Format as ISO 8601 UTC (YYYY-MM-DDTHH:MM:SSZ) without chrono
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        // Epoch: 1970-01-01 — simple date reconstruction
        let (y, mo, d) = days_to_ymd(days);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
    };
    let mut content = format!(
        "# {agentic_root}/env — Platform-managed plugin environment variables\n\
         # Generated by: nexus pull ({timestamp})\n\
         # Source: Nexus platform (project: {project_name})\n\
         # DO NOT edit manually — overwritten by nexus pull / nexus init\n\
         # Secrets go in .env.nexus.local (never committed, never here)\n\n",
    );

    // Sort keys for deterministic output
    let mut sorted: Vec<(&String, &String)> = plugin_env.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in &sorted {
        content.push_str(&format!("{k}={v}\n"));
    }

    // The header carries a timestamp: compare only the variable lines, so
    // an unchanged env is neither rewritten nor reported.
    let vars_only = |c: &str| {
        c.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    if fs::read_to_string(&env_path)
        .is_ok_and(|existing| vars_only(&existing) == vars_only(&content))
    {
        return Ok(());
    }

    fs::create_dir_all(&env_dir)?;
    fs::write(&env_path, &content)?;
    println!(
        "   {} {}/env ({} plugin var{})",
        style("+").bold().green(),
        agentic_root,
        sorted.len(),
        if sorted.len() == 1 { "" } else { "s" }
    );

    // Ensure `env` is in .<agentic_root>/.gitignore
    let gitignore_entry = "env\n";
    if gitignore_path.exists() {
        let existing = fs::read_to_string(&gitignore_path).unwrap_or_default();
        if !existing.lines().any(|l| l.trim() == "env") {
            let mut f = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            f.write_all(gitignore_entry.as_bytes())?;
        }
    } else {
        fs::write(&gitignore_path, gitignore_entry)?;
    }

    Ok(())
}

/// What `nexus run` will start, for the post-pull/init tip (NEXUS-APP
/// dispatch 442f0e97): the backend's `run_target` when known, else the
/// project's runtime.
pub(crate) fn run_start_label(
    run_target: Option<&nexus_core::api::RunTarget>,
    is_claude: bool,
) -> &'static str {
    match run_target {
        Some(t) if t.tool == "claude" && t.workspace.as_deref() == Some("zellij") => {
            "the Claude Code workspace"
        }
        Some(t) if t.tool == "claude" => "Claude Code",
        Some(_) => "OpenCode",
        None if is_claude => "Claude Code",
        None => "OpenCode",
    }
}

/// Print a hint about `nexus run` after pull/init. `start_label` names what
/// it starts (see [`run_start_label`]).
///
/// - If `devbox.json` exists in the workspace → optional tip (devbox shell already injects vars).
/// - If no `devbox.json` → important notice (nexus run is required for plugin env vars).
pub(crate) fn print_nexus_run_hint(workspace: &Path, start_label: &str) {
    let has_devbox = workspace.join("devbox.json").exists();
    println!();
    if has_devbox {
        println!(
            "   {} Use {} to start {} with plugin env vars (e.g. HEADROOM_*).",
            style("Tip:").bold().cyan(),
            style("nexus run").bold(),
            start_label,
        );
        println!(
            "        Inside {}, these are already set — nexus run is optional.",
            style("devbox shell").dim(),
        );
    } else {
        println!(
            "   {} Use {} to start {}, so all plugin env vars",
            style("Important:").bold().yellow(),
            style("nexus run").bold(),
            start_label,
        );
        println!("              (e.g. HEADROOM_*) are injected. Without nexus run,");
        println!("              headroom and other plugins may not function correctly.");
    }
}

/// Convert days since Unix epoch to (year, month, day). Used for ISO 8601 timestamps
/// in `.nexus/env` without requiring the `chrono` crate.
pub(crate) fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Gregorian proleptic calendar — good enough for dates in our range
    let mut y = 1970u64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31u64,
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
    let mut mo = 1u64;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        mo += 1;
    }
    (y, mo, days + 1)
}

/// Read a single key from a `.env`-style file in the workspace.
///
/// Searches `.env.nexus.local` (primary) and `.env.local` (fallback) for a
/// line of the form `KEY=value` or `export KEY=value`. Strips surrounding
/// quotes (`"` or `'`). Returns `None` when neither file exists or the key is
/// not present.
fn read_key_from_env_file(workspace: &Path, key: &str) -> Option<String> {
    let candidates = [".env.nexus.local", ".env.local"];
    for candidate in &candidates {
        let path = workspace.join(candidate);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            // Strip optional `export ` prefix
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(value) = rest.strip_prefix('=') {
                    let value = value.trim();
                    // Strip surrounding quotes
                    let value = value
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                        .unwrap_or(value);
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Run the pull command.
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    force: bool,
    mcp_source: McpSource,
    scope: &[String],
    with_actor_assets: bool,
    ccx_force: super::ccx::ForceMode,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // Resolve project ID from CLI flag or linked project
    let project_id = match config::resolve_project_id(cli_project_id, Some(&workspace)) {
        Ok(id) => id,
        Err(_) => {
            println!();
            println!(
                "   {} No project linked to this workspace.",
                style("!").bold().yellow()
            );
            println!();
            println!(
                "   Without a linked project, {} cannot pull project-specific",
                style("nexus pull").bold()
            );
            println!("   skills, agent files, directives, or MCP configuration.");
            println!();
            println!(
                "   Run {} to bind this workspace to a Nexus project,",
                style("nexus link").bold().cyan()
            );
            println!(
                "   or pass {} directly.",
                style("--project-id <UUID>").bold().cyan()
            );
            println!();
            return Ok(());
        }
    };

    // Resolve authentication token
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token.clone()))?;

    println!(
        "{} Pulling from Nexus platform...",
        style(">>").bold().cyan()
    );
    let project_name =
        crate::cmd::display::resolve_project_display_name(&client, &project_id, Some(&workspace))
            .await;
    crate::cmd::display::print_project_banner(api_url, &project_id, project_name.as_deref());
    println!();

    // Scope filter: if empty, pull everything. Otherwise only named scopes.
    let pull_all = scope.is_empty();
    let pull_scope = |name: &str| pull_all || scope.iter().any(|s| s.eq_ignore_ascii_case(name));

    // Verify identity
    let identity = client.get_identity().await?;
    println!(
        "   {} Authenticated as {}",
        style("+").bold().green(),
        style(&identity.email).bold()
    );

    // Default agentic root; may be updated from af_export response later
    let mut agentic_root = ".nexus".to_string();

    // Try to get agentic_root early from af_export (before any file writes)
    let af_export_result = client.export_agent_files(&project_id).await;
    if let Ok(ref af_export) = af_export_result {
        if !af_export.agentic_root.is_empty() {
            agentic_root = af_export.agentic_root.clone();
        }
    }

    // Resolve the runtime before writing anything, so only the selected
    // runtime's projection is rendered (NEXUS-APP dispatches 4820e584,
    // 442f0e97).
    // `flavor_known`: the backend actually answered (an absent owner then
    // legitimately means OpenCode); an unreachable backend must never
    // trigger the removal of the Claude Code projection below.
    let (tool_flavor, flavor_known) = match af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.agent_owner.clone())
    {
        Some(owner) => (Some(owner), true),
        None => {
            // Fallback: fetch from project details API
            match client.get_project(&project_id).await {
                Ok(d) => (d.project.agent_owner, true),
                Err(_) => (None, false),
            }
        }
    };
    let is_claude = config::is_claude_owner(tool_flavor.as_deref());
    // The owner cached by the previous pull, before it is overwritten below.
    let previous_owner = config::load_agent_owner(Some(&workspace));

    // Load sync manifest for local-modification detection
    let manifest = super::sync::load_manifest_pub(&workspace);

    // Detect existing .claude/ files and hint at import (v0.7.0)
    detect_importable_files(&workspace, is_claude, &manifest);
    let mut skipped_modified: Vec<String> = Vec::new();
    let mut overwrote_modified: Vec<String> = Vec::new();
    // Only the explicit flags, never -y (see cmd/mod.rs): destructive
    // decisions (projection cleanup of modified files, reverting committed
    // workspace files) need --force / --force-unmanaged.
    let explicit_force = ccx_force != super::ccx::ForceMode::None;
    let mut ws_report = WorkspaceSyncReport::default();

    // Export skills (always needed for project_name)
    let export = client.export_skills(&project_id).await?;
    let project_name = export.project.name.clone();

    println!(
        "   {} Project: {} {}",
        style("+").bold().green(),
        style(&export.project.name).bold(),
        style(format!("({})", project_id)).dim()
    );

    if export.skills.is_empty() {
        println!(
            "   {} No skills assigned to this project.",
            style("--").yellow()
        );
    } else {
        // Skills (and, for OpenCode projects, their slash commands) are
        // rendered here and compared with disk: unchanged files are left
        // alone, files unmodified since the last pull are updated silently,
        // and only locally edited files need confirmation (dispatch 4820e584).
        let mut generated: Vec<(String, String)> = Vec::new();
        for skill in &export.skills {
            generated.extend(render_skill_files(skill, &agentic_root));
            if !is_claude {
                generated.extend(render_command_file(skill, &agentic_root));
            }
        }
        let (written, skipped) =
            sync_generated_files(&workspace, &agentic_root, &generated, force)?;
        skipped_modified.extend(skipped);

        println!(
            "   {} {} skill(s) synced ({})",
            style("+").bold().green(),
            export.skills.len(),
            if written == 0 {
                "up to date".to_string()
            } else {
                format!("{written} file(s) updated")
            }
        );
    }

    // Export directives
    let has_directives = match client.export_directives(&project_id).await {
        Ok(dir_export) => {
            if dir_export.directives.is_empty() {
                println!(
                    "   {} No directives for this project.",
                    style("--").yellow()
                );
                false
            } else {
                // Check for existing directives file
                let directives_path = workspace.join(&agentic_root).join("directives.md");
                if directives_path.exists() && !force {
                    if !is_managed_file(&directives_path) {
                        println!(
                            "   {} {}/directives.md is user-managed (no nexus-platform marker), skipping",
                            style("--").yellow(),
                            agentic_root
                        );
                    } else {
                        // Managed file, overwrite silently on pull
                        write_directives(&workspace, &dir_export.directives, &agentic_root)?;
                        println!(
                            "   {} {} directive(s) synced",
                            style("+").bold().green(),
                            dir_export.directives.len()
                        );
                    }
                } else {
                    write_directives(&workspace, &dir_export.directives, &agentic_root)?;
                    println!(
                        "   {} {} directive(s) synced",
                        style("+").bold().green(),
                        dir_export.directives.len()
                    );
                }
                true
            }
        }
        Err(e) => {
            println!(
                "   {} Could not fetch directives: {}",
                style("!").bold().yellow(),
                e
            );
            false
        }
    };

    // Export agent files (AGENTS.md, CLAUDE.md, etc.) from platform
    // Reuse the af_export result fetched earlier (avoids duplicate API call)
    // Track written agent file paths so the plugin download step doesn't
    // overwrite them with stale content from the GitHub registry.
    let mut af_written_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    match af_export_result {
        Ok(ref af_export) => {
            if af_export.agent_files.is_empty() {
                println!(
                    "   {} No agent files configured for this project.",
                    style("--").yellow()
                );
            } else {
                let mut af_written = 0;
                let (agent_files, duplicates) = unique_agent_files(&af_export.agent_files);
                for (path, dropped) in &duplicates {
                    println!(
                        "   {} backend sent several agent files for {}; using the first, ignoring {}",
                        style("!").bold().yellow(),
                        path,
                        dropped.join(", ")
                    );
                }
                for af in agent_files {
                    // CCX files are reconciled against the CCX lock below
                    // (NEXUS-APP ADR-0117, dispatch 99f335e8).
                    if af_export.ccx.is_some() && af.category == super::ccx::CCX_CATEGORY {
                        continue;
                    }
                    let target_path = workspace.join(&af.target_path);

                    // Only the selected runtime's projection is written.
                    if is_other_runtime_path(&af.target_path, is_claude) {
                        continue;
                    }

                    // Protected files (secrets/env scaffolds) are write-if-missing:
                    // an existing one is left alone without comment.
                    if is_protected_path(&af.target_path) && target_path.exists() {
                        continue;
                    }

                    // Already identical (ignoring the export timestamp):
                    // nothing to write or report.
                    if agent_file_matches(&target_path, &af.body) {
                        // Normalized hash: also true for the local file,
                        // whose generated_at may differ from this export's.
                        let _ = super::sync::update_manifest_after_pull(
                            &workspace,
                            &af.file_key,
                            &af.target_path,
                            &nexus_core::hash::sha256_hex_normalized(&af.body),
                        );
                        af_written_paths.insert(af.target_path.clone());
                        continue;
                    }

                    // Check managed-file marker / sync manifest before overwriting
                    if target_path.exists()
                        && !force
                        && !is_pull_managed(&workspace, &af.target_path, &manifest)
                    {
                        println!(
                            "   {} {} is user-managed, skipping (use --force to overwrite)",
                            style("--").yellow(),
                            af.target_path
                        );
                        continue;
                    }

                    // Check for local modifications since last pull
                    if target_path.exists()
                        && is_locally_modified(&workspace, &af.target_path, &manifest)
                    {
                        if !force {
                            println!(
                                "   {} skipped: {} (locally modified, use --force to overwrite or nexus stash to save)",
                                style("!").bold().yellow(),
                                af.target_path
                            );
                            skipped_modified.push(af.target_path.clone());
                            continue;
                        } else {
                            overwrote_modified.push(af.target_path.clone());
                        }
                    }

                    write_agent_file(&workspace, af)?;
                    af_written_paths.insert(af.target_path.clone());

                    // Record the (generated_at-normalized) hash of what was
                    // written, for local-modification detection.
                    let _ = super::sync::update_manifest_after_pull(
                        &workspace,
                        &af.file_key,
                        &af.target_path,
                        &nexus_core::hash::sha256_hex_normalized(&af.body),
                    );

                    af_written += 1;
                }
                if af_written > 0 {
                    println!(
                        "   {} {} agent file(s) synced",
                        style("+").bold().green(),
                        af_written
                    );
                }
            }
        }
        Err(ref e) => {
            // Fallback to hardcoded templates when af_export is unavailable
            println!(
                "   {} Agent file export not available ({}), using local templates",
                style("!").bold().yellow(),
                e
            );
            sync_claude_md(
                &workspace,
                &project_name,
                has_directives,
                force,
                &agentic_root,
            )?;
            sync_agents_md(&workspace, &project_name, force, &agentic_root)?;
        }
    }

    // Export actor profiles from af_export response.
    // Writes `.nexus/actors/<slug>.md` for each assigned actor and
    // `.nexus/generated/actors.json` with the full actor metadata.
    if let Some(Ok(ref af_export)) = Some(&af_export_result) {
        if !af_export.actors.is_empty() {
            let actors_dir = workspace.join(&agentic_root).join("actors");
            fs::create_dir_all(&actors_dir)?;

            let generated_dir = workspace.join(&agentic_root).join("generated");
            fs::create_dir_all(&generated_dir)?;

            let mut actors_written = 0;
            for actor in &af_export.actors {
                let actor_path = actors_dir.join(format!("{}.md", actor.slug));
                fs::write(&actor_path, &actor.body)?;
                actors_written += 1;
            }

            // Write actors.json with full metadata
            let actors_json = serde_json::to_string_pretty(&af_export.actors)?;
            fs::write(generated_dir.join("actors.json"), &actors_json)?;

            if actors_written > 0 {
                println!(
                    "   {} {} actor profile(s) synced",
                    style("+").bold().green(),
                    actors_written
                );
            }

            // Download avatar assets if --with-actor-assets was passed
            if with_actor_assets {
                let assets_dir = actors_dir.join("assets");
                fs::create_dir_all(&assets_dir)?;

                let mut assets_downloaded = 0;
                for actor in &af_export.actors {
                    if let Some(ref avatar) = actor.avatar {
                        if let Some(ref url) = avatar.url {
                            // SEC-003: validate URL against trusted allowlist
                            if let Err(e) = validate_download_url(url) {
                                println!(
                                    "   {} Avatar for '{}' skipped: {}",
                                    style("!").bold().yellow(),
                                    actor.slug,
                                    e
                                );
                                continue;
                            }
                            match client.download_actor_avatar(url).await {
                                Ok(svg_bytes) => {
                                    let dest = assets_dir.join(format!("{}.svg", actor.slug));
                                    fs::write(&dest, &svg_bytes)?;
                                    assets_downloaded += 1;
                                }
                                Err(e) => {
                                    println!(
                                        "   {} Avatar for '{}' download failed: {}",
                                        style("!").bold().yellow(),
                                        actor.slug,
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
                if assets_downloaded > 0 {
                    println!(
                        "   {} {} actor avatar(s) downloaded",
                        style("+").bold().green(),
                        assets_downloaded
                    );
                }
            }
        }
    }

    // Check for deprecated model routes used by actors (ADR-0055)
    // Note: model_routes is now Option<serde_json::Value> (map format, ADR-0057)
    if let Some(Ok(ref _af_export)) = Some(&af_export_result) {
        // Deprecation checks via the new map format are handled when writing model-routes.json
    }

    // Cache the flavor (and the backend's run target) in .nexus/config.toml
    // so launch-time commands (`nexus run`, `nexus preflight`) can pick the
    // right artifacts and binary offline (NEXUS-APP dispatches dfd4e655,
    // 442f0e97).
    let _ = nexus_core::config::update_agent_owner(Some(&workspace), tool_flavor.as_deref());
    if let Ok(ref af_export) = af_export_result {
        let _ =
            nexus_core::config::update_run_target(Some(&workspace), af_export.run_target.as_ref());
    }

    let plugin_mcp_servers = af_export_result
        .as_ref()
        .ok()
        .map(|r| r.mcp_servers.clone())
        .unwrap_or_default();

    let providers = af_export_result
        .as_ref()
        .ok()
        .map(|r| r.provider.clone())
        .unwrap_or_default();

    // Use auth_token from response if available — avoids a separate credentials.toml
    // read and guarantees the token written to opencode.json is always fresh.
    let effective_token = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.auth_token.clone())
        .unwrap_or_else(|| token.to_string());

    // Resolve opencode_agents, default model, and model_routes from af_export response
    let opencode_agents = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.opencode_agents.clone());

    let opencode_default_model = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.opencode_default_model.clone());

    let opencode_default_agent = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.opencode_default_agent.clone());

    let model_routes_export = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.model_routes.clone());

    // Instructions paths to merge into opencode.json's top-level
    // "instructions" array (e.g. "<agentic_root>/AGENTS.md"), so OpenCode
    // loads the project's agent policy deterministically instead of relying
    // on its own upward AGENTS.md auto-discovery.
    let opencode_instructions = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.opencode_instructions.clone());

    // Model routing / provider divergence warnings from the backend (e.g. an
    // agent's model uses a provider Nexus can't verify, or a route-alias
    // migration hasn't been applied). Rendered verbatim and gated on operator
    // confirmation before opencode.json is written -- see dispatch e0ee68d5.
    let export_warnings = af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.export_warnings.clone())
        .unwrap_or_default();

    // Write model-routes.json for traceability (ADR-0057)
    if let Some(ref routes) = model_routes_export {
        let generated_dir = workspace.join(&agentic_root).join("generated");
        if let Err(e) = fs::create_dir_all(&generated_dir) {
            println!(
                "   {} Could not create generated/ dir: {}",
                style("!").bold().yellow(),
                e
            );
        } else {
            let routes_path = generated_dir.join("model-routes.json");
            let routes_json = serde_json::json!({ "routes": routes });
            if let Some(content) = serde_json::to_string_pretty(&routes_json)
                .ok()
                .map(|c| c + "\n")
                .filter(|c| !content_matches(&routes_path, c))
            {
                let _ = fs::write(&routes_path, content);
                println!(
                    "   {} {}/generated/model-routes.json",
                    style("+").bold().green(),
                    agentic_root
                );
            }
        }
    }

    // Gate the opencode.json write on the operator confirming any
    // model-routing warnings. Only relevant when opencode.json is actually
    // going to be written (skipped entirely for the claude-cli-only flavor,
    // since mcp.json carries no model/agent routing config).
    let opencode_will_be_written = !is_claude;

    let mut claude_report: Option<claude_render::ClaudeProjectionReport> = None;
    let proceed_with_opencode = if opencode_will_be_written {
        confirm_export_warnings(&export_warnings, force)?
    } else {
        true
    };

    if proceed_with_opencode {
        // Write MCP server configs (creates if missing, merges plugin servers, force-overwrites)
        write_mcp_configs(
            &workspace,
            api_url,
            &effective_token,
            &project_id,
            mcp_source,
            tool_flavor.as_deref(),
            &agentic_root,
            &plugin_mcp_servers,
            &providers,
            &opencode_agents,
            &opencode_default_model,
            &opencode_default_agent,
            &opencode_instructions,
            force,
        )?;

        // Render the native Claude Code projection (Track B1, ADR-C04/C06):
        // CLAUDE.md, .claude/settings.json, .claude/skills/, .claude/agents/.
        // Additive only; skipped entirely for the opencode-only flavor.
        // .mcp.json itself was already written above by write_mcp_configs.
        if is_claude {
            let actors_for_claude = af_export_result
                .as_ref()
                .ok()
                .map(|r| r.actors.clone())
                .unwrap_or_default();
            let agent_files_for_claude = af_export_result
                .as_ref()
                .ok()
                .map(|r| r.agent_files.clone())
                .unwrap_or_default();
            let runtime_spec_for_claude = af_export_result
                .as_ref()
                .ok()
                .and_then(|r| r.runtime_spec.as_ref());
            let hook_adapters_for_claude = af_export_result
                .as_ref()
                .ok()
                .and_then(|r| r.claude_hook_adapters.clone())
                .unwrap_or_default();
            // Same project-detail fetch pattern already used below for git
            // identity application (line ~1363): af_export does not carry
            // git_config, so this is a dedicated, cheap lookup.
            let include_co_authored_by = client
                .get_project(&project_id)
                .await
                .ok()
                .and_then(|d| d.project.git_config)
                .and_then(|g| g.include_co_authored_by);
            claude_report = Some(claude_render::render_claude_projection(
                &workspace,
                &project_name,
                &agentic_root,
                &export.skills,
                &actors_for_claude,
                &agent_files_for_claude,
                runtime_spec_for_claude,
                &hook_adapters_for_claude,
                include_co_authored_by,
                af_export_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.claude_settings.as_ref()),
                af_export_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.claude_md_managed_block.as_deref()),
                ccx_force != super::ccx::ForceMode::None,
            )?);
        }
    } else {
        println!(
            "   {} Skipped opencode.json / {}/mcp.json (declined after model-routing warning(s) above).",
            style("--").bold().yellow(),
            agentic_root
        );
        println!("            Re-run 'nexus pull' to retry once the warning(s) are addressed.");
    }

    // CCX files (NEXUS-APP ADR-0117, dispatch 99f335e8): reconciled against
    // the CCX lock; the lock is only rewritten once everything succeeded.
    // Claude Code only: an OpenCode project gets no .claude/rules etc.
    let mut ccx_outcomes: Vec<super::ccx::FileOutcome> = Vec::new();
    if let (true, Ok(ref af_export)) = (is_claude, &af_export_result) {
        if let Some(ref info) = af_export.ccx {
            let lock = super::ccx::load_lock(&workspace, &agentic_root);
            let desired = super::ccx::ccx_files(&af_export.agent_files);
            let plans = super::ccx::plan_files(&workspace, &desired, lock.as_ref(), &manifest)?;
            let (outcomes, files) = super::ccx::apply_file_plans(&workspace, &plans, ccx_force)?;
            super::ccx::record_ccx_in_lock(&workspace, &agentic_root, info, files)?;
            ccx_outcomes = outcomes;
        }
    }

    // Write .nexus/env from af_export.plugin_env (platform-managed, full overwrite)
    let plugin_env = af_export_result
        .as_ref()
        .ok()
        .map(|r| r.plugin_env.clone())
        .unwrap_or_default();
    write_plugin_env_file(&workspace, &plugin_env, &project_name, &agentic_root)?;

    // Install platform-selected plugins from af_export plugins list.
    // Downloads known Nexus plugins (nexus-compaction-plus, nexus-cost-control)
    // into .opencode/plugins/ using the built-in registry. Existing plugin files
    // are not overwritten unless --force is set.
    if let Some(Ok(ref af_export)) = Some(&af_export_result) {
        if !is_claude && !af_export.plugins.is_empty() {
            let platform_plugins = resolve_platform_plugins(&af_export.plugins);
            if !platform_plugins.is_empty() {
                let plugins_dir = workspace.join(".opencode").join("plugins");
                std::fs::create_dir_all(&plugins_dir)?;
                for (name, def) in &platform_plugins {
                    let filename = def
                        .filename
                        .clone()
                        .unwrap_or_else(|| format!("{}.ts", name));
                    let dest = plugins_dir.join(&filename);
                    // Skip plugins already delivered via agent_files (DB is source of truth)
                    let plugin_target = format!(".opencode/plugins/{}", filename);
                    if af_written_paths.contains(&plugin_target) {
                        continue;
                    }
                    // Skip if already present and not forcing
                    if dest.exists() && !force {
                        println!(
                            "   {} .opencode/plugins/{} (already present, skipping)",
                            style("·").dim(),
                            filename
                        );
                        continue;
                    }
                    if let Some(ref url) = def.url {
                        // SEC-003: validate URL against trusted allowlist
                        if let Err(e) = validate_download_url(url) {
                            println!(
                                "   {} Plugin '{}' skipped: {}",
                                style("!").bold().yellow(),
                                name,
                                e
                            );
                            continue;
                        }
                        let client = reqwest::Client::new();
                        match client.get(url).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                let body = resp.text().await.unwrap_or_default();
                                std::fs::write(&dest, &body)?;
                                println!(
                                    "   {} .opencode/plugins/{} ({})",
                                    style("+").bold().green(),
                                    filename,
                                    if dest.exists() {
                                        "updated"
                                    } else {
                                        "downloaded"
                                    }
                                );
                            }
                            Ok(resp) => {
                                println!(
                                    "   {} Plugin '{}' download failed: HTTP {}",
                                    style("!").bold().yellow(),
                                    name,
                                    resp.status()
                                );
                            }
                            Err(e) => {
                                println!(
                                    "   {} Plugin '{}' download failed: {}",
                                    style("!").bold().yellow(),
                                    name,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Remove the projection of the runtime this project no longer uses
    // (v0.29.0), now that the selected one is on disk. Skipped when the
    // owner is unknown or the OpenCode config write was declined, so a
    // failed pull never leaves the workspace without a projection.
    if let (true, true, Ok(ref af_export)) = (
        flavor_known,
        is_claude || proceed_with_opencode,
        &af_export_result,
    ) {
        let projection = super::projection_cleanup::Projection::unselected(is_claude);
        let ctx = cleanup_context(
            af_export,
            &export.skills,
            &agentic_root,
            &project_name,
            is_claude,
            explicit_force,
        );
        let plan = super::projection_cleanup::plan(&workspace, projection, &ctx);
        // The selected projection is already complete: a cleanup error is
        // reported, and the rest of the pull continues.
        match super::projection_cleanup::apply(&workspace, &plan, &ctx) {
            Ok(report) => {
                let previous = previous_owner.as_deref().unwrap_or("opencode");
                let current = tool_flavor.as_deref().unwrap_or("opencode");
                let switched = (config::is_claude_owner(previous_owner.as_deref()) != is_claude)
                    .then_some((previous, current));
                super::projection_cleanup::print_report(&report, projection, switched);
            }
            Err(e) => println!(
                "   {} Could not remove the unused {} projection: {} (re-run nexus pull)",
                style("!").bold().yellow(),
                projection.label(),
                e
            ),
        }
    }

    // Check prerequisites from af_export response.
    // Warn for each required tool that is not found in PATH.
    // This is non-blocking — pull succeeds regardless, but the user
    // needs the tool for the plugin to actually work.
    if let Some(Ok(ref af_export)) = Some(&af_export_result) {
        if !af_export.prerequisites.is_empty() {
            let mut missing: Vec<String> = Vec::new();
            for prereq in &af_export.prerequisites {
                // SEC-004: validate check_command against shell injection.
                // Only allow simple commands: alphanumeric, hyphens, underscores,
                // dots, forward slashes, spaces, and common flags (--version).
                let is_safe = prereq.check_command.chars().all(|c| {
                    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ' ' | '=' | ':')
                });
                let found = if !is_safe {
                    false
                } else {
                    std::process::Command::new("sh")
                        .args(["-c", &prereq.check_command])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                };
                if !found {
                    missing.push(prereq.tool.clone());
                    println!(
                        "   {} {} not found in PATH",
                        style("!").bold().yellow(),
                        style(&prereq.tool).bold()
                    );
                    println!("     Required by: {}", prereq.required_by);
                    println!("     Install: {}", style(&prereq.install_hint).dim());
                }
            }
            if !missing.is_empty() {
                println!(
                    "   {} {} prerequisite{} missing — {} will not function until installed.",
                    style("!").bold().yellow(),
                    missing.len(),
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", ")
                );
            }
        }
    }

    // Export open tasks as TASKS.md
    match client
        .list_tasks(&project_id, Some(&["open", "in_progress", "blocked"]))
        .await
    {
        Ok(task_response) => {
            if task_response.tasks.is_empty() {
                println!("   {} No open tasks to export.", style("·").dim());
            } else {
                write_tasks(&workspace, &task_response.tasks, &agentic_root)?;
                println!(
                    "   {} Wrote {} task{} to {}/TASKS.md",
                    style("✓").green(),
                    task_response.tasks.len(),
                    if task_response.tasks.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    agentic_root,
                );
            }
        }
        Err(e) => {
            println!("   {} Could not fetch tasks: {}", style("!").yellow(), e);
        }
    }

    // Export workspace files (devbox.json + scripts) — ADR-0034 (v2 fork API, v1 fallback)
    if pull_scope("workspace") {
        // Try MCP-authenticated ws_export first (PAT-compatible)
        let mut v2_ok = false;
        match client.export_workspace_mcp(&project_id).await {
            Ok(export) => {
                v2_ok = true;
                let mut ws_written = 0;

                // devbox.json and scripts: same rules for every workspace
                // file (see sync_workspace_file).
                if sync_workspace_file(
                    &workspace,
                    "devbox.json",
                    &export.devbox_json,
                    false,
                    force,
                    explicit_force,
                    &manifest,
                    &mut ws_report,
                )? {
                    ws_written += 1;
                }
                for script in &export.scripts {
                    if sync_workspace_file(
                        &workspace,
                        &script.path,
                        &script.body,
                        script.executable,
                        force,
                        explicit_force,
                        &manifest,
                        &mut ws_report,
                    )? {
                        ws_written += 1;
                    }
                }

                if ws_written > 0 {
                    println!(
                        "   {} {} workspace file(s) synced (v2{}, {})",
                        style("+").bold().green(),
                        ws_written,
                        if export.meta.upstream_changed {
                            ", upstream changed"
                        } else {
                            ""
                        },
                        if export.meta.shadow_mode {
                            "shadow mode"
                        } else {
                            "direct mode"
                        }
                    );

                    // Auto-apply workspace git-exclude when shadow_mode is active
                    if export.meta.shadow_mode {
                        match shadow::workspace_on() {
                            Ok(()) => {}
                            Err(e) => {
                                println!(
                                    "   {} Could not apply workspace git-exclude: {}",
                                    style("!").yellow(),
                                    e
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("No active workspace fork") {
                    println!(
                        "   {} No workspace fork assigned to this project.",
                        style("·").dim()
                    );
                    v2_ok = true; // Not an error, just no fork
                } else {
                    println!(
                        "   {} ws_export failed: {}, trying v1...",
                        style("!").yellow(),
                        e
                    );
                }
            }
        }

        // Fall back to v1 (legacy wf_export)
        if !v2_ok {
            match client.export_workspace(&project_id).await {
                Ok(ws_export) => {
                    // Check if workspace provisioning is explicitly disabled
                    if ws_export.workspace_provisioning_enabled == Some(false) {
                        println!(
                            "   {} Workspace provisioning disabled for this project.",
                            style("·").dim()
                        );
                    } else if ws_export.workspace.is_none() && ws_export.scripts.is_empty() {
                        println!(
                            "   {} No workspace files assigned to this project.",
                            style("·").dim()
                        );
                    } else {
                        let mut ws_written = 0;

                        // Composed workspace template (devbox.json) and
                        // scripts, with the same rules as the v2 path.
                        if let Some(ref tpl) = ws_export.workspace {
                            if sync_workspace_file(
                                &workspace,
                                &tpl.target_path,
                                &tpl.body,
                                false,
                                force,
                                explicit_force,
                                &manifest,
                                &mut ws_report,
                            )? {
                                ws_written += 1;
                            }
                        }
                        for script in &ws_export.scripts {
                            if sync_workspace_file(
                                &workspace,
                                &script.target_path,
                                &script.body,
                                script.executable,
                                force,
                                explicit_force,
                                &manifest,
                                &mut ws_report,
                            )? {
                                ws_written += 1;
                            }
                        }

                        if ws_written > 0 {
                            println!(
                                "   {} {} workspace file(s) synced ({})",
                                style("+").bold().green(),
                                ws_written,
                                if ws_export.shadow_mode {
                                    "shadow mode"
                                } else {
                                    "direct mode"
                                }
                            );

                            // Auto-apply workspace git-exclude when shadow_mode is active
                            if ws_export.shadow_mode {
                                match shadow::workspace_on() {
                                    Ok(()) => {}
                                    Err(e) => {
                                        println!(
                                            "   {} Could not apply workspace git-exclude: {}",
                                            style("!").yellow(),
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Could not fetch workspace files: {}",
                        style("!").yellow(),
                        e
                    );
                }
            }
        } // end if !v2_ok
    }

    // Auto-apply git identity if configured
    if pull_all || pull_scope("agents") {
        if let Ok(detail) = client.get_project(&project_id).await {
            if let Some(ref git_cfg) = detail.project.git_config {
                if git_cfg.user_name.is_some() || git_cfg.user_email.is_some() {
                    match super::git::apply_git_config(&workspace, git_cfg) {
                        Ok(n) if n > 0 => println!(
                            "   {} Applied {} git identity setting(s).",
                            style("GIT").bold().blue(),
                            n
                        ),
                        _ => {}
                    }
                }
            }
        }
    }

    skipped_modified.append(&mut ws_report.skipped_modified);
    overwrote_modified.append(&mut ws_report.overwrote_modified);

    // Summary: locally modified files
    if !skipped_modified.is_empty() {
        println!();
        println!(
            "   {} {} locally modified file(s) skipped:",
            style("!").bold().yellow(),
            skipped_modified.len(),
        );
        for path in &skipped_modified {
            println!("      {}", style(path).yellow());
        }
        println!(
            "   Use {} to save local changes, or {} to overwrite.",
            style("nexus stash").bold(),
            style("nexus pull --force").bold(),
        );
    }
    if !overwrote_modified.is_empty() {
        println!();
        println!(
            "   {} Overwrote {} locally modified file(s):",
            style("!").bold().yellow(),
            overwrote_modified.len(),
        );
        for path in &overwrote_modified {
            println!("      {}", style(path).yellow());
        }
    }

    print_committed_workspace_summary(&ws_report);

    if let (true, Ok(ref af_export)) = (is_claude, &af_export_result) {
        if let Some(ref info) = af_export.ccx {
            print_ccx_summary(
                info,
                &ccx_outcomes,
                claude_report.as_ref(),
                af_export.claude_settings.as_ref(),
            );
        }
    }

    // Git hook self-heal (v0.29.0): cheap, never fails the pull.
    super::githooks::run(&workspace);

    println!();
    println!("{} Pull complete.", style("OK").bold().green());

    // Login follow-up line for the effective gh CLI profile, if one is
    // configured (NEXUS-APP ADR-0116, dispatch 0350aee7). Informational
    // only -- pull never seeds or writes an auth token; that is `nexus
    // run`'s job. Applies to all tool flavors, not just Claude Code.
    if let Ok(detail) = client.get_project(&project_id).await {
        if let Some(gh) = detail.project.gh_effective {
            match nexus_core::config::Config::dir()
                .map(|d| super::git::gh_profile_dir(&d, &gh.profile))
            {
                Ok(dir) if super::git::gh_is_authenticated(&dir, &gh.host) => {
                    println!(
                        "   GitHub: {}@{} via profile {} ({})",
                        gh.user.as_deref().unwrap_or("?"),
                        gh.host,
                        gh.profile,
                        if gh.origin.eq_ignore_ascii_case("user") {
                            "user default"
                        } else {
                            "project"
                        }
                    );
                }
                Ok(dir) => {
                    println!(
                        "   {} GitHub: profile '{}' not logged in to {} yet -- \
                         'nexus run' will offer to import it, or run: \
                         GH_CONFIG_DIR={} gh auth login --hostname {}",
                        style("!").bold().yellow(),
                        gh.profile,
                        gh.host,
                        dir.display(),
                        gh.host
                    );
                }
                Err(_) => {}
            }
        }
    }

    // Hint: use `nexus run` for env-var injection
    print_nexus_run_hint(
        &workspace,
        run_start_label(
            af_export_result
                .as_ref()
                .ok()
                .and_then(|r| r.run_target.as_ref()),
            is_claude,
        ),
    );

    Ok(())
}

/// Print the Claude Code Experience section of the pull output (NEXUS-APP
/// ADR-0117, dispatch 99f335e8).
fn print_ccx_summary(
    info: &nexus_core::api::CcxBundleInfo,
    outcomes: &[super::ccx::FileOutcome],
    claude_report: Option<&claude_render::ClaudeProjectionReport>,
    claude_settings: Option<&nexus_core::api::ClaudeSettingsSpec>,
) {
    println!();
    println!(
        "{} {}@{} ({})",
        style("Claude Code Experience:").bold(),
        info.bundle,
        info.version,
        info.revision
    );
    for outcome in outcomes {
        let (label, note) = super::ccx::outcome_label(outcome);
        let label = format!("{label:<9}");
        let label = match outcome.state {
            super::ccx::FileState::Clean | super::ccx::FileState::Adopt => style(label).dim(),
            super::ccx::FileState::Create | super::ccx::FileState::Updated => style(label).green(),
            _ => style(label).yellow(),
        };
        if note.is_empty() {
            println!("  {} {}", label, outcome.target_path);
        } else {
            println!(
                "  {} {}   {}",
                label,
                outcome.target_path,
                style(note).dim()
            );
        }
    }
    let Some(report) = claude_report else {
        return;
    };
    match report.claude_md {
        claude_render::ClaudeMdOutcome::Written(_) => {
            println!(
                "  {} CLAUDE.md (nexus-managed block)",
                style(format!("{:<9}", "UPDATE")).green()
            )
        }
        claude_render::ClaudeMdOutcome::Unchanged(_) => {
            println!(
                "  {} CLAUDE.md (nexus-managed block)",
                style(format!("{:<9}", "CLEAN")).dim()
            )
        }
        claude_render::ClaudeMdOutcome::Conflict => println!(
            "  {} CLAUDE.md (nexus-managed block)   {}",
            style(format!("{:<9}", "CONFLICT")).yellow(),
            style("modified locally, run nexus claude diff").dim()
        ),
        claude_render::ClaudeMdOutcome::Skipped => {}
    }
    if !report.settings_changes.is_empty() {
        println!(
            "  {} {}",
            style(format!("{:<9}", "SETTINGS")).cyan(),
            report.settings_changes.join(", ")
        );
    }
    let nexus_core_plugin = claude_settings
        .and_then(|s| s.values.get("enabledPlugins"))
        .and_then(|v| v.as_object())
        .is_some_and(|plugins| {
            plugins
                .iter()
                .any(|(id, on)| id.starts_with("nexus-core@") && on.as_bool() == Some(true))
        });
    for plugin in &report.removed_hook_plugins {
        println!(
            "  {} {} removed ({})",
            style(format!("{:<9}", "HOOKS")).cyan(),
            plugin,
            if nexus_core_plugin {
                "now provided by nexus-core"
            } else {
                "no longer sent"
            }
        );
    }
}

// ---------------------------------------------------------------------------
// Managed file sync (CLAUDE.md, AGENTS.md)
// ---------------------------------------------------------------------------

/// Whether the file at `path` already has exactly `body` (no write needed).
fn content_matches(path: &Path, body: &str) -> bool {
    fs::read(path).is_ok_and(|c| c == body.as_bytes())
}

/// [`content_matches`] for platform-rendered agent files, ignoring the
/// `generated_at:` frontmatter line, which the backend re-stamps on every
/// export even when nothing else changed.
fn agent_file_matches(path: &Path, body: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|c| {
        c == body
            || nexus_core::hash::sha256_hex_normalized(&c)
                == nexus_core::hash::sha256_hex_normalized(body)
    })
}

/// Whether pull may treat an existing file as its own: it carries the
/// `source: nexus-platform` marker, or an earlier pull recorded it in the
/// sync manifest (files like YAML/TOML that cannot carry the marker).
/// Whether it was edited since is decided separately by
/// [`is_locally_modified`].
fn is_pull_managed(workspace: &Path, target_path: &str, manifest: &serde_json::Value) -> bool {
    is_managed_file(&workspace.join(target_path))
        || manifest.as_object().is_some_and(|m| {
            m.get(target_path).is_some()
                || m.values()
                    .any(|e| e.get("target_path").and_then(|t| t.as_str()) == Some(target_path))
        })
}

// ---------------------------------------------------------------------------
// Workspace files (devbox.json, scripts/devbox/**)
// ---------------------------------------------------------------------------

/// What happened to workspace files during one pull, for the summary.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceSyncReport {
    pub skipped_modified: Vec<String>,
    pub overwrote_modified: Vec<String>,
    /// Committed files kept because the fork version differs (no --force).
    pub skipped_committed: Vec<String>,
    /// Committed files replaced by the fork version (--force).
    pub overwrote_committed: Vec<String>,
}

/// Why a workspace file is not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceSkip {
    /// Exists, carries no marker and is not in the sync manifest.
    UserManaged,
    /// Edited since the last pull.
    LocallyModified,
    /// Tracked in git, the local file equals HEAD, and the fork version
    /// differs from HEAD: writing it would revert committed repo state.
    CommittedDiffers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceAction {
    /// Local file already equals the fork version (modulo trailing
    /// newlines).
    Unchanged,
    Skip(WorkspaceSkip),
    Write {
        /// Overwrites a local edit (--force / -y).
        overwrote_modified: bool,
        /// Overwrites a committed version (explicit --force).
        reverted_commit: bool,
    },
}

/// Decide what pull does with one workspace file. Text comparisons ignore
/// trailing newlines (the backend sends e.g. `devbox.json` without one;
/// repos with an end-of-file fixer commit it with one). `head` is only
/// evaluated when the local file differs from `incoming`.
///
/// `force` is `--force` or `-y`; `explicit_force` only `--force` /
/// `--force-unmanaged`, required to revert a committed file.
pub(crate) fn decide_workspace_write(
    local: Option<&str>,
    incoming: &str,
    pull_managed: bool,
    recorded: Option<&str>,
    head: impl FnOnce() -> Option<String>,
    force: bool,
    explicit_force: bool,
) -> WorkspaceAction {
    let Some(local) = local else {
        return WorkspaceAction::Write {
            overwrote_modified: false,
            reverted_commit: false,
        };
    };
    if nexus_core::hash::text_equivalent(local, incoming) {
        return WorkspaceAction::Unchanged;
    }
    if !pull_managed && !force {
        return WorkspaceAction::Skip(WorkspaceSkip::UserManaged);
    }
    let committed = head().is_some_and(|h| {
        nexus_core::hash::text_equivalent(local, &h)
            && !nexus_core::hash::text_equivalent(&h, incoming)
    });
    if committed && !explicit_force {
        return WorkspaceAction::Skip(WorkspaceSkip::CommittedDiffers);
    }
    let modified = recorded.is_some_and(|r| !nexus_core::hash::hash_matches_text(r, local));
    if modified && !force {
        return WorkspaceAction::Skip(WorkspaceSkip::LocallyModified);
    }
    WorkspaceAction::Write {
        overwrote_modified: modified && !committed,
        reverted_commit: committed,
    }
}

/// Content of `rel` at `HEAD`, if the file is tracked by git in the
/// repository containing `workspace`. `None` outside a repository, for
/// untracked files, and before the first commit.
pub(crate) fn git_head_content(workspace: &Path, rel: &str) -> Option<String> {
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", rel])
        .current_dir(workspace)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !tracked {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["--no-pager", "show", &format!("HEAD:./{rel}")])
        .current_dir(workspace)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Sync one workspace file (`devbox.json` or a script) per
/// [`decide_workspace_write`]. Written files always end with exactly one
/// newline; the sync manifest records the hash of the backend's copy.
/// Returns `true` if the file was written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_workspace_file(
    workspace: &Path,
    rel: &str,
    body: &str,
    executable: bool,
    force: bool,
    explicit_force: bool,
    manifest: &serde_json::Value,
    report: &mut WorkspaceSyncReport,
) -> anyhow::Result<bool> {
    let target = workspace.join(rel);
    let local = fs::read(&target)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let recorded = super::ccx::sync_manifest_hash(manifest, rel);
    let action = decide_workspace_write(
        local.as_deref(),
        body,
        local.is_some() && is_pull_managed(workspace, rel, manifest),
        recorded.as_deref(),
        || git_head_content(workspace, rel),
        force,
        explicit_force,
    );
    let record = || {
        let _ = super::sync::update_manifest_after_pull(workspace, rel, rel, &sha256_hex(body));
    };
    match action {
        WorkspaceAction::Unchanged => {
            record();
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::UserManaged) => {
            println!(
                "   {} {} is user-managed, skipping",
                style("--").yellow(),
                rel
            );
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::LocallyModified) => {
            println!(
                "   {} skipped: {} (locally modified, use --force to overwrite or nexus stash to save)",
                style("!").bold().yellow(),
                rel
            );
            report.skipped_modified.push(rel.to_string());
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::CommittedDiffers) => {
            println!(
                "   {} skipped: {} (committed locally and differs from the Nexus workspace fork; \
                 run `nexus push` to update the fork, or `nexus pull --force` to take the fork version)",
                style("!").bold().yellow(),
                rel
            );
            report.skipped_committed.push(rel.to_string());
            Ok(false)
        }
        WorkspaceAction::Write {
            overwrote_modified,
            reverted_commit,
        } => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &target,
                nexus_core::hash::with_single_trailing_newline(body),
            )?;
            record();
            #[cfg(unix)]
            if executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
            #[cfg(not(unix))]
            let _ = executable;
            if overwrote_modified {
                report.overwrote_modified.push(rel.to_string());
            }
            if reverted_commit {
                report.overwrote_committed.push(rel.to_string());
            }
            Ok(true)
        }
    }
}

/// Summary lines for committed workspace files the fork version would
/// revert (C3 overwrite guard).
fn print_committed_workspace_summary(report: &WorkspaceSyncReport) {
    if !report.overwrote_committed.is_empty() {
        println!();
        println!(
            "   {} Overwrote {} committed file(s) with the Nexus workspace fork version (--force):",
            style("!!").bold().red(),
            report.overwrote_committed.len()
        );
        for path in &report.overwrote_committed {
            println!("      {}", style(path).red());
        }
        println!(
            "   They now differ from HEAD. Review with {}; if the committed version was newer,",
            style("git diff").bold()
        );
        println!(
            "   restore it ({}) and run {} to update the fork.",
            style("git checkout -- <file>").bold(),
            style("nexus push").bold()
        );
    }
    if !report.skipped_committed.is_empty() {
        println!();
        println!(
            "   {} {} committed file(s) differ from the Nexus workspace fork and were kept:",
            style("!").bold().yellow(),
            report.skipped_committed.len()
        );
        for path in &report.skipped_committed {
            println!("      {}", style(path).yellow());
        }
        println!(
            "   Run {} to update the fork, or {} to take the fork version.",
            style("nexus push").bold(),
            style("nexus pull --force").bold()
        );
    }
}

/// Check whether a file contains the `source: nexus-platform` marker,
/// indicating it is managed by Nexus CLI and safe to overwrite.
pub fn is_managed_file(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match fs::read_to_string(path) {
        Ok(content) => content.contains(MANAGED_MARKER),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Importable .claude/ file detection (replaces conflict detection per ADR-0027)
// ---------------------------------------------------------------------------

/// Scan the workspace for existing `.claude/` files that could be imported into
/// the Nexus project. This is a purely informational notice — no files are
/// modified, no confirmation is required.
///
/// Replaces the old `detect_agentic_conflicts` + `warn_agentic_conflicts` flow
/// from ADR-0024. Only files Nexus did not generate are listed (NEXUS-APP
/// dispatch 4820e584): files carrying the `source: nexus-platform` marker or
/// the `CLAUDE.md` nexus-managed block, files tracked in the sync manifest,
/// and `.claude/settings.json` of a Claude Code project (the projection's own
/// settings file) are skipped.
pub fn detect_importable_files(
    workspace: &Path,
    is_claude: bool,
    sync_manifest: &serde_json::Value,
) {
    let found = importable_files(workspace, is_claude, sync_manifest);
    if found.is_empty() {
        return;
    }

    println!();
    println!(
        "   {} Existing agent files not managed by Nexus:",
        style("i").bold().blue()
    );
    for path in &found {
        println!("      {} {}", style("-").dim(), style(path).dim());
    }
    println!();
    println!(
        "   These can be imported into your Nexus project with {}.",
        style("nexus import").bold().cyan()
    );
    println!();
}

/// The files [`detect_importable_files`] reports.
fn importable_files(
    workspace: &Path,
    is_claude: bool,
    sync_manifest: &serde_json::Value,
) -> Vec<String> {
    let tracked = |rel: &str| {
        sync_manifest.as_object().is_some_and(|m| {
            m.values()
                .any(|e| e.get("target_path").and_then(|t| t.as_str()) == Some(rel))
        })
    };
    let generated_by_nexus = |rel: &str| {
        tracked(rel)
            || fs::read_to_string(workspace.join(rel)).is_ok_and(|c| {
                c.contains(MANAGED_MARKER) || c.contains("<!-- BEGIN:nexus-managed -->")
            })
    };

    let mut candidates: Vec<String> = Vec::new();
    let claude_dir = workspace.join(".claude");
    for name in &["CLAUDE.md", "settings.json", "mcp.json", "commands.md"] {
        if claude_dir.join(name).exists() {
            candidates.push(format!(".claude/{}", name));
        }
    }
    for name in &["AGENTS.md", "CLAUDE.md"] {
        if workspace.join(name).exists() {
            candidates.push((*name).to_string());
        }
    }
    if let Ok(entries) = fs::read_dir(claude_dir.join("skills")) {
        for entry in entries.flatten() {
            if entry.path().join("SKILL.md").exists() {
                let name = entry.file_name().to_string_lossy().to_string();
                candidates.push(format!(".claude/skills/{}/SKILL.md", name));
            }
        }
    }

    candidates
        .into_iter()
        .filter(|rel| !(is_claude && rel == ".claude/settings.json"))
        .filter(|rel| !generated_by_nexus(rel))
        .map(|rel| {
            rel.strip_suffix("SKILL.md")
                .map(str::to_string)
                .unwrap_or(rel)
        })
        .collect()
}

/// Sync `.claude/CLAUDE.md` — the agent bootstrap file.
///
/// - If the file does not exist → create it
/// - If it exists and has the nexus-platform marker → overwrite
/// - If it exists without the marker → user-managed, skip (warn)
fn sync_claude_md(
    workspace: &Path,
    project_name: &str,
    has_directives: bool,
    force: bool,
    agentic_root: &str,
) -> anyhow::Result<()> {
    let claude_dir = workspace.join(agentic_root);
    fs::create_dir_all(&claude_dir)?;

    let path = claude_dir.join("CLAUDE.md");

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} {}/CLAUDE.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow(),
            agentic_root
        );
        return Ok(());
    }

    let content = render_claude_md(project_name, has_directives, agentic_root);
    fs::write(&path, content)?;
    print_synced(&format!("{}/CLAUDE.md", agentic_root));

    Ok(())
}

/// Sync `AGENTS.md` — the agent role definition file.
///
/// - If the file does not exist → create it
/// - If it exists and has the nexus-platform marker → overwrite
/// - If it exists without the marker → user-managed, skip (warn)
fn sync_agents_md(
    workspace: &Path,
    project_name: &str,
    force: bool,
    agentic_root: &str,
) -> anyhow::Result<()> {
    // When using alternate agentic root, AGENTS.md goes inside that directory
    let path = if agentic_root != ".claude" {
        workspace.join(agentic_root).join("AGENTS.md")
    } else {
        workspace.join("AGENTS.md")
    };

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} AGENTS.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow()
        );
        return Ok(());
    }

    let content = render_agents_md(project_name);
    fs::write(&path, content)?;
    let label = if agentic_root != ".claude" {
        format!("{}/AGENTS.md", agentic_root)
    } else {
        "AGENTS.md".to_string()
    };
    print_synced(&label);

    Ok(())
}

// ---------------------------------------------------------------------------
// Agent file writer (server-driven)
// ---------------------------------------------------------------------------

/// Write a single agent file exported from the platform to its `target_path`.
///
/// Creates any intermediate directories as needed.
/// The file body is already template-substituted by the server.
pub fn write_agent_file(
    workspace: &Path,
    af: &nexus_core::api::ExportedAgentFile,
) -> anyhow::Result<()> {
    // Path traversal / absolute-path escape protection, checked first so
    // a malformed target_path is rejected before any exists()/join() call
    // touches it (NEXUS-APP ADR-0117 hardening item, dispatch bb782869).
    validate_agent_file_target_path(workspace, &af.target_path)?;

    // Protected file guard: never overwrite secrets/env files
    if is_protected_path(&af.target_path) {
        let target = workspace.join(&af.target_path);
        if target.exists() {
            anyhow::bail!(
                "refusing to write: '{}' matches a protected file pattern (secrets/env files are never overwritten)",
                af.target_path
            );
        }
    }

    let target = workspace.join(&af.target_path);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&target, &af.body)?;
    print_synced(&af.target_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Template renderers (fallback when af_export is unavailable)
// ---------------------------------------------------------------------------

/// Render the `.claude/CLAUDE.md` bootstrap file content.
///
/// When `has_directives` is true, a step to load directives is included
/// in the bootstrap sequence.
pub fn render_claude_md(project_name: &str, has_directives: bool, agentic_root: &str) -> String {
    let directives_step = if has_directives {
        format!(
            "\n3. Load project directives from `{}/directives.md`",
            agentic_root
        )
    } else {
        String::new()
    };

    // Adjust step numbering based on whether directives are included
    let (review_step, continue_step) = if has_directives {
        ("4", "5")
    } else {
        ("3", "4")
    };

    format!(
        r#"---
type: bootstrap
scope: repo
project: {name}
source: nexus-platform
status: active
---

# BOOTSTRAP SEQUENCE

1. Load agent identity from `AGENTS.md`
2. Connect to the Nexus MCP server{directives_step}
{review_step}. Review active planning and ADR context
{continue_step}. Continue with the active workstream

---

# PROJECT

This workspace is configured for the **{name}** project.

Treat all project memory and coordination artifacts as architecture-critical.

---

# ENVIRONMENT

Read secrets only from `.env.local`.

NEVER:
- print secrets
- commit secrets
- persist secrets into shared memory
"#,
        name = project_name,
        directives_step = directives_step,
        review_step = review_step,
        continue_step = continue_step,
    )
}

/// Render the `AGENTS.md` agent policy file content.
pub fn render_agents_md(project_name: &str) -> String {
    format!(
        r#"---
type: agent-policy
scope: repo
project: {name}
source: nexus-platform
status: active
---

# ACTIVE AGENTS

- app-agent (PRIMARY)

---

# AGENT ROLE DEFINITION

## app-agent (PRIMARY)

You are responsible for:

- Application architecture and development
- Code quality and testing
- Documentation and knowledge management

You are expected to:

- Maintain architectural clarity
- Keep durable truth out of ephemeral chat context
- Preserve auditability and handoff quality

---

# GLOBAL RULES

- Decisions must be documented (ADR or architectural note)
- Sessions are execution history, not long-term truth
- Durable learnings go to project memory
- No speculation presented as fact
- Correctness over speed
"#,
        name = project_name,
    )
}

// ---------------------------------------------------------------------------
// File detection & confirmation
// ---------------------------------------------------------------------------

/// Ask the user for overwrite confirmation.
fn confirm_overwrite() -> anyhow::Result<bool> {
    print!("   {} Overwrite? [y/N] ", style("?").bold().cyan());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();

    Ok(answer == "y" || answer == "yes")
}

/// Render `af_export`'s `export_warnings` grouped by code and gate the
/// `opencode.json` write on operator confirmation.
///
/// Returns `true` if it's safe to proceed writing `opencode.json`, `false`
/// if the operator declined. `bypass` (`--yes` / `--force`) skips the prompt
/// outright. Never blocks in a non-interactive context (CI, piped input,
/// no TTY): warnings are printed and the pull proceeds -- a loud log line
/// beats a stuck pipeline.
///
/// Warnings are rendered verbatim from the backend, not re-derived
/// client-side: only the backend knows execution_mode, agent_mode, gateway
/// availability, and which provider blocks it actually emitted.
fn confirm_export_warnings(warnings: &[ExportWarning], bypass: bool) -> anyhow::Result<bool> {
    if warnings.is_empty() {
        return Ok(true);
    }

    println!();
    println!(
        "   {} Nexus detected {} model-routing warning(s) for this project:",
        style("!").bold().yellow(),
        warnings.len()
    );

    // Group by code, preserving first-seen order.
    let mut order: Vec<&str> = Vec::new();
    let mut grouped: HashMap<&str, Vec<&ExportWarning>> = HashMap::new();
    for w in warnings {
        grouped
            .entry(w.code.as_str())
            .or_insert_with(|| {
                order.push(w.code.as_str());
                Vec::new()
            })
            .push(w);
    }

    for code in &order {
        println!();
        println!("   {} {}", style("•").bold().yellow(), style(code).bold());
        for w in &grouped[code] {
            println!("     {}", w.message);
            if let Some(ref hint) = w.hint {
                println!("     {} {}", style("hint:").dim(), style(hint).dim());
            }
        }
    }
    println!();

    if bypass {
        println!(
            "   {} Continuing (--yes/--force): opencode.json will be written despite the warning(s) above.",
            style("--yes").dim()
        );
        return Ok(true);
    }

    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        println!(
            "   {} Non-interactive session: proceeding despite the warning(s) above. Pass --yes to silence this notice.",
            style("!").dim()
        );
        return Ok(true);
    }

    print!("   {} Continue anyway? [y/N] ", style("?").bold().cyan());
    io::stdout().flush()?;
    let ch = console::Term::stdout().read_char().unwrap_or('n');
    println!("{}", ch);
    Ok(ch == 'y' || ch == 'Y')
}

// ---------------------------------------------------------------------------
// MCP config generation
// ---------------------------------------------------------------------------

/// Write MCP server configs (`opencode.json`, `.mcp.json` at project root).
///
/// Behavior:
/// - If the file does not exist: create it with the nexus MCP server + any
///   plugin servers from the platform.
/// - If the file exists and `force` is set: rewrite it, merging the nexus
///   server with plugin servers from the platform.
/// - If the file exists and `force` is NOT set but new plugin servers need
///   to be added: merge them into the existing config (additive only).
#[allow(clippy::too_many_arguments)]
fn write_mcp_configs(
    workspace: &Path,
    api_url: &str,
    token: &str,
    project_id: &str,
    mcp_source: McpSource,
    tool_flavor: Option<&str>,
    _agentic_root: &str,
    plugin_mcp_servers: &HashMap<String, McpServerConfig>,
    providers: &HashMap<String, ProviderConfig>,
    opencode_agents: &Option<serde_json::Value>,
    opencode_default_model: &Option<String>,
    opencode_default_agent: &Option<String>,
    opencode_instructions: &Option<Vec<String>>,
    force: bool,
) -> anyhow::Result<()> {
    let opencode_path = workspace.join("opencode.json");
    let claude_mcp_path = workspace.join(".mcp.json");

    let skip_opencode = config::is_claude_owner(tool_flavor);
    let skip_claude = !skip_opencode;

    let source_label = match mcp_source {
        McpSource::Npm => "npm (@gwdn/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    // ── opencode.json ──────────────────────────────────────────────────────
    if !skip_opencode {
        let exists = opencode_path.exists();
        let needs_write = !exists
            || force
            || json_lacks_nexus_server(&opencode_path, "mcp")
            || !plugin_mcp_servers.is_empty()
            || !providers.is_empty()
            || opencode_agents.is_some()
            || opencode_instructions.is_some();

        if needs_write {
            // Parse the existing file once (if applicable) and reuse it for
            // both the "mcp" merge and the "instructions" merge below.
            let existing_parsed: Option<serde_json::Value> = if exists && !force {
                fs::read_to_string(&opencode_path)
                    .ok()
                    .and_then(|content| serde_json::from_str(&content).ok())
            } else {
                None
            };

            let mut mcp_block: serde_json::Map<String, serde_json::Value> = existing_parsed
                .as_ref()
                .and_then(|parsed| parsed.get("mcp"))
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            // Preserve any pre-existing top-level "instructions" entries
            // (e.g. user-added paths) so merging in the platform-managed
            // entries below is additive, not a silent overwrite.
            let existing_instructions: Vec<String> = existing_parsed
                .as_ref()
                .and_then(|parsed| parsed.get("instructions"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // Nexus server (always present)
            let nexus_command = match mcp_source {
                McpSource::Npm => serde_json::json!(["npx", "--yes", "@gwdn/nexus-mcp@latest"]),
                McpSource::Local => {
                    serde_json::json!(["node", "tools/nexus-mcp/dist/server.js"])
                }
            };

            // Resolve NEXUS_SEC_OPENAI_API_KEY in priority order:
            //   1. Shell environment variable (CI/CD, manually exported)
            //   2. .env.nexus.local or .env.local in the workspace root
            //   3. {env:} template fallback (OpenCode expands at startup)
            let openai_key_value = std::env::var("NEXUS_SEC_OPENAI_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| read_key_from_env_file(workspace, "NEXUS_SEC_OPENAI_API_KEY"))
                .unwrap_or_else(|| "{env:NEXUS_SEC_OPENAI_API_KEY}".to_string());

            mcp_block.insert(
                "nexus".to_string(),
                serde_json::json!({
                    "type": "local",
                    "command": nexus_command,
                    "environment": {
                        "NEXUS_API_URL": api_url,
                        "NEXUS_PRIVATE_TOKEN": token,
                        "NEXUS_PROJECT_ID": project_id,
                        "NEXUS_SEC_OPENAI_API_KEY": openai_key_value
                    }
                }),
            );

            // Plugin servers from platform
            for (name, cfg) in plugin_mcp_servers {
                // Build env object.
                // Priority: inline `environment` map (e.g. HEADROOM_* vars from the platform)
                // merged with `env_keys` rendered as OpenCode {env:KEY} templates.
                // Inline values take precedence so the platform can deliver real values
                // (like HEADROOM_MODE=transform) without leaking secrets.
                let mut env: serde_json::Map<String, serde_json::Value> = cfg
                    .env_keys
                    .iter()
                    .map(|k: &String| {
                        let template = format!("{{env:{}}}", k);
                        (k.clone(), serde_json::Value::String(template))
                    })
                    .collect();
                // Overlay inline environment vars (platform-managed, non-secret)
                for (k, v) in &cfg.environment {
                    env.insert(k.clone(), serde_json::Value::String(v.clone()));
                }

                // Build the full command array: cfg.command (Vec<String>) + cfg.args
                let full_command: Vec<serde_json::Value> = cfg
                    .command
                    .iter()
                    .chain(cfg.args.iter())
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect();

                mcp_block.insert(
                    name.to_string(),
                    serde_json::json!({
                        "type": "local",
                        "command": full_command,
                        "environment": env
                    }),
                );
            }

            // LLM providers from platform (e.g. DGX Spark)
            // Pass through the API-provided config verbatim — it already
            // matches the opencode.json provider schema.
            let mut provider_block: serde_json::Map<String, serde_json::Value> =
                serde_json::Map::new();
            for (name, cfg) in providers {
                provider_block.insert(name.to_string(), cfg.clone());
            }

            let mut opencode_obj = serde_json::Map::new();
            opencode_obj.insert(
                "$schema".to_string(),
                serde_json::Value::String("https://opencode.ai/config.json".to_string()),
            );
            opencode_obj.insert("mcp".to_string(), serde_json::Value::Object(mcp_block));
            if !provider_block.is_empty() {
                opencode_obj.insert(
                    "provider".to_string(),
                    serde_json::Value::Object(provider_block),
                );
            }

            // OpenCode agents block from platform actor system (key must be "agent", singular)
            // https://opencode.ai/docs/agents/#json
            if let Some(agents) = opencode_agents {
                opencode_obj.insert("agent".to_string(), agents.clone());
            }

            // Instructions paths (e.g. "<agentic_root>/AGENTS.md") merged into the
            // top-level "instructions" array, so OpenCode loads them deterministically
            // at session start instead of relying on its own upward AGENTS.md
            // auto-discovery, which has no awareness of the agentic_root convention.
            // Additive: preserves any pre-existing entries, append-only, deduped.
            let mut merged_instructions = existing_instructions;
            if let Some(paths) = opencode_instructions {
                for path in paths {
                    if !merged_instructions.contains(path) {
                        merged_instructions.push(path.clone());
                    }
                }
            }
            if !merged_instructions.is_empty() {
                opencode_obj.insert(
                    "instructions".to_string(),
                    serde_json::Value::Array(
                        merged_instructions
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
            }

            // Global default model — DGX Spark local-first (ADR-0057)
            if let Some(ref default_model) = opencode_default_model {
                opencode_obj.insert(
                    "model".to_string(),
                    serde_json::Value::String(default_model.clone()),
                );
            }

            // Default agent — starts in planning mode (ADR-0058)
            if let Some(ref default_agent) = opencode_default_agent {
                opencode_obj.insert(
                    "default_agent".to_string(),
                    serde_json::Value::String(default_agent.clone()),
                );
            }

            let opencode_json = serde_json::Value::Object(opencode_obj);
            let opencode_content = serde_json::to_string_pretty(&opencode_json)? + "\n";
            let unchanged = content_matches(&opencode_path, &opencode_content);
            if !unchanged {
                fs::write(&opencode_path, &opencode_content)?;
            }

            // SEC-002: ensure token-bearing config files are git-excluded
            shadow::ensure_git_excluded(&["opencode.json", "opencode.jsonc"]);

            let verb = if exists { "updated" } else { "created" };
            let mut extras = Vec::new();
            if !plugin_mcp_servers.is_empty() {
                extras.push(format!(
                    "plugins: {}",
                    plugin_mcp_servers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !providers.is_empty() {
                extras.push(format!(
                    "providers: {}",
                    providers.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            if !unchanged {
                println!(
                    "   {} opencode.json {} (MCP source: {}{})",
                    style("+").bold().green(),
                    verb,
                    source_label,
                    if extras.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", extras.join(", "))
                    },
                );
            }
        }
    }

    // ── .mcp.json (project root, Claude Code project scope) ────────────────
    if !skip_claude {
        let exists = claude_mcp_path.exists();
        let needs_write = !exists
            || force
            || !plugin_mcp_servers.is_empty()
            || json_lacks_nexus_server(&claude_mcp_path, "mcpServers");

        if needs_write {
            let mut servers_block: serde_json::Map<String, serde_json::Value> = if exists && !force
            {
                let content = fs::read_to_string(&claude_mcp_path).unwrap_or_default();
                let parsed: serde_json::Value = serde_json::from_str(&content)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                parsed
                    .get("mcpServers")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default()
            } else {
                serde_json::Map::new()
            };

            // Nexus server
            let (cmd, args) = match mcp_source {
                McpSource::Npm => ("npx", vec!["--yes", "@gwdn/nexus-mcp@latest"]),
                McpSource::Local => ("node", vec!["tools/nexus-mcp/dist/server.js"]),
            };
            servers_block.insert(
                "nexus".to_string(),
                serde_json::json!({
                    "command": cmd,
                    "args": args,
                    "env": {
                        "NEXUS_API_URL": api_url,
                        "NEXUS_PRIVATE_TOKEN": token
                    }
                }),
            );

            // Local MCP server (NEXUS-APP dispatch af407643): tools that
            // need filesystem/session-local access and have no Claude Code
            // custom-tool equivalent (nexus_headroom_intercept_retrieve,
            // nexus_cost_summary). Added if missing, never overwritten if
            // the operator has customized it.
            servers_block
                .entry("nexus-local-tools".to_string())
                .or_insert_with(|| {
                    serde_json::json!({
                        "command": "nexus",
                        "args": ["mcp-local"]
                    })
                });

            // Plugin servers
            for (name, cfg) in plugin_mcp_servers {
                // Build env object: map env_keys to shell-style template variables ${KEY}
                // so secrets are resolved at runtime, never persisted to disk.
                // Also overlay inline environment vars from the platform.
                let mut env: serde_json::Map<String, serde_json::Value> = cfg
                    .env_keys
                    .iter()
                    .map(|k: &String| {
                        let template = format!("${{{}}}", k);
                        (k.clone(), serde_json::Value::String(template))
                    })
                    .collect();
                for (k, v) in &cfg.environment {
                    env.insert(k.clone(), serde_json::Value::String(v.clone()));
                }

                // mcp.json uses Claude Code format: command = string, args = array.
                // cfg.command is Vec<String> (array form from backend), so split
                // first element as command and remainder + cfg.args as args.
                let cmd_str = cfg.command.first().cloned().unwrap_or_default();
                let args: Vec<serde_json::Value> = cfg.command[1..]
                    .iter()
                    .chain(cfg.args.iter())
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect();

                servers_block.insert(
                    name.to_string(),
                    serde_json::json!({
                        "command": cmd_str,
                        "args": args,
                        "env": env
                    }),
                );
            }

            let claude_mcp_json = serde_json::json!({
                "mcpServers": servers_block
            });
            let claude_mcp_content = serde_json::to_string_pretty(&claude_mcp_json)? + "\n";
            let unchanged = content_matches(&claude_mcp_path, &claude_mcp_content);
            if !unchanged {
                fs::write(&claude_mcp_path, &claude_mcp_content)?;
            }

            // SEC-002: ensure token-bearing config files are git-excluded
            shadow::ensure_git_excluded(&[".mcp.json"]);

            let verb = if exists { "updated" } else { "created" };
            if !unchanged {
                println!(
                    "   {} .mcp.json {} (MCP source: {}{})",
                    style("+").bold().green(),
                    verb,
                    source_label,
                    if plugin_mcp_servers.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", plugins: {}",
                            plugin_mcp_servers
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                );
            }
        }
    }

    Ok(())
}

/// Whether the JSON config at `path` parses but has no `nexus` entry under
/// `block` (e.g. a runtime switch removed it and the operator's own servers
/// remain): the next pull adds it back. Unparseable files are left alone.
fn json_lacks_nexus_server(path: &Path, block: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|v| v.get(block).and_then(|b| b.get("nexus")).is_none())
}

// ---------------------------------------------------------------------------
// File writers
// ---------------------------------------------------------------------------

/// Render a skill's canonical files (`<agentic_root>/skills/<id>/SKILL.md`
/// plus its resource files) as `(workspace-relative path, content)` pairs.
pub(crate) fn render_skill_files(
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> Vec<(String, String)> {
    let skill_dir = format!("{}/skills/{}", agentic_root, skill.skill_id);

    let body = skill
        .body
        .as_deref()
        .unwrap_or("<!-- No skill body defined -->");
    let body = claude_render::strip_frontmatter(body);

    let content = format!(
        r#"---
skill_id: {skill_id}
name: {name}
description: {description}
version: {version}
command_slug: {command_slug}
source: nexus-platform
---

{body}
"#,
        skill_id = skill.skill_id,
        name = skill.name,
        description = claude_render::yaml_escape(skill.description.as_deref().unwrap_or("")),
        version = skill.version,
        command_slug = skill.command_slug.as_deref().unwrap_or("none"),
        body = body,
    );

    let mut files = vec![(format!("{skill_dir}/SKILL.md"), content)];
    for res in &skill.resources {
        // Sanitise filename: prevent directory traversal
        let filename = res.filename.replace(['/', '\\'], "_");
        if filename.is_empty() || filename == "SKILL.md" {
            continue;
        }
        files.push((format!("{skill_dir}/{filename}"), res.body.clone()));
    }
    files
}

/// Render a skill's OpenCode slash command (`.opencode/commands/<slug>.md`),
/// if it has a command slug. OpenCode projects only.
pub(crate) fn render_command_file(
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> Option<(String, String)> {
    let slug = skill.command_slug.as_deref().filter(|s| !s.is_empty())?;
    let content = format!(
        r#"---
description: "{name}"
skill_id: "{skill_id}"
version: {version}
source: nexus-platform
---

Load the skill file at `{agentic_root}/skills/{skill_id}/SKILL.md` and follow its instructions.
"#,
        name = skill.name,
        skill_id = skill.skill_id,
        version = skill.version,
        agentic_root = agentic_root,
    );
    Some((format!(".opencode/commands/{slug}.md"), content))
}

/// Hashes of the files pull renders itself (skills, skill resources, OpenCode
/// commands) as last written, keyed by workspace-relative path. Lets a pull
/// tell "unmodified since the last pull" apart from "edited locally".
fn pull_manifest_path(workspace: &Path, agentic_root: &str) -> std::path::PathBuf {
    workspace
        .join(agentic_root)
        .join("generated")
        .join("pull-manifest.json")
}

pub(crate) fn load_pull_manifest(
    workspace: &Path,
    agentic_root: &str,
) -> std::collections::BTreeMap<String, String> {
    fs::read_to_string(pull_manifest_path(workspace, agentic_root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// How a rendered file compares to what is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedState {
    /// Missing, or unmodified since pull last wrote it: write silently.
    Write,
    /// Already identical: nothing to do.
    Unchanged,
    /// Differs and was not (provably) written by the last pull: needs
    /// confirmation before it is overwritten.
    LocallyModified,
}

pub(crate) fn classify_generated(
    local: Option<&[u8]>,
    desired: &str,
    recorded: Option<&str>,
) -> GeneratedState {
    let Some(local) = local else {
        return GeneratedState::Write;
    };
    let local_hash = nexus_core::hash::sha256_hex_bytes(local);
    if local_hash == sha256_hex(desired) {
        GeneratedState::Unchanged
    } else if recorded == Some(local_hash.as_str()) {
        GeneratedState::Write
    } else {
        GeneratedState::LocallyModified
    }
}

/// Write the rendered `files` per [`classify_generated`]. Locally modified
/// files are listed and confirmed with a single prompt (skipped without
/// `force` if declined); the rest of the pull always continues. Returns the
/// number of files written and the paths that were skipped.
pub(crate) fn sync_generated_files(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
    force: bool,
) -> anyhow::Result<(usize, Vec<String>)> {
    sync_generated_files_with(workspace, agentic_root, files, force, confirm_overwrite)
}

/// [`sync_generated_files`] with the confirmation prompt injected (tests).
fn sync_generated_files_with(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
    force: bool,
    confirm: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<(usize, Vec<String>)> {
    let mut recorded = load_pull_manifest(workspace, agentic_root);
    let states: Vec<GeneratedState> = files
        .iter()
        .map(|(path, content)| {
            let local = fs::read(workspace.join(path)).ok();
            classify_generated(
                local.as_deref(),
                content,
                recorded.get(path).map(String::as_str),
            )
        })
        .collect();

    let modified: Vec<&str> = files
        .iter()
        .zip(&states)
        .filter(|(_, s)| **s == GeneratedState::LocallyModified)
        .map(|((p, _), _)| p.as_str())
        .collect();
    let overwrite_modified = if modified.is_empty() || force {
        true
    } else {
        println!();
        println!(
            "   {} Locally modified since the last pull:",
            style("!").bold().yellow()
        );
        for path in &modified {
            println!("      {}", style(path).dim());
        }
        confirm()?
    };

    let mut written = 0;
    let mut skipped = Vec::new();
    for ((path, content), state) in files.iter().zip(&states) {
        match state {
            GeneratedState::Unchanged => {}
            GeneratedState::LocallyModified if !overwrite_modified => {
                skipped.push(path.clone());
                continue;
            }
            _ => {
                let target = workspace.join(path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, content)?;
                print_synced(path);
                written += 1;
            }
        }
        recorded.insert(path.clone(), sha256_hex(content));
    }

    let manifest_path = pull_manifest_path(workspace, agentic_root);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&recorded)? + "\n",
    )?;
    Ok((written, skipped))
}

/// The agent files to materialize, one per `target_path` (the first one
/// wins), plus the file keys dropped per duplicated path. Several files
/// for one path would otherwise overwrite each other on every pull.
pub(crate) fn unique_agent_files(
    files: &[nexus_core::api::ExportedAgentFile],
) -> (
    Vec<&nexus_core::api::ExportedAgentFile>,
    Vec<(String, Vec<String>)>,
) {
    let mut kept: Vec<&nexus_core::api::ExportedAgentFile> = Vec::new();
    let mut dropped: Vec<(String, Vec<String>)> = Vec::new();
    for af in files {
        if kept.iter().any(|k| k.target_path == af.target_path) {
            match dropped.iter_mut().find(|(p, _)| *p == af.target_path) {
                Some((_, keys)) => keys.push(af.file_key.clone()),
                None => dropped.push((af.target_path.clone(), vec![af.file_key.clone()])),
            }
        } else {
            kept.push(af);
        }
    }
    (kept, dropped)
}

/// Record several files as last written by pull (one manifest write).
pub(crate) fn record_generated_many(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
) -> anyhow::Result<()> {
    let mut recorded = load_pull_manifest(workspace, agentic_root);
    let mut changed = false;
    for (rel, content) in files {
        let hash = sha256_hex(content);
        if recorded.get(rel) != Some(&hash) {
            recorded.insert(rel.clone(), hash);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    save_pull_manifest(workspace, agentic_root, &recorded)
}

/// Drop the pull-manifest records of the paths matching `remove`. Returns
/// the number of records dropped.
pub(crate) fn remove_generated_records(
    workspace: &Path,
    agentic_root: &str,
    remove: impl Fn(&str) -> bool,
) -> anyhow::Result<usize> {
    let mut recorded = load_pull_manifest(workspace, agentic_root);
    let before = recorded.len();
    recorded.retain(|path, _| !remove(path));
    let dropped = before - recorded.len();
    if dropped > 0 {
        save_pull_manifest(workspace, agentic_root, &recorded)?;
    }
    Ok(dropped)
}

fn save_pull_manifest(
    workspace: &Path,
    agentic_root: &str,
    recorded: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let manifest_path = pull_manifest_path(workspace, agentic_root);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(recorded)? + "\n",
    )?;
    Ok(())
}

/// Record `content` as last written by pull for `rel` in the pull manifest.
fn record_generated(
    workspace: &Path,
    agentic_root: &str,
    rel: &str,
    content: &str,
) -> anyhow::Result<()> {
    let mut recorded = load_pull_manifest(workspace, agentic_root);
    let hash = sha256_hex(content);
    if recorded.get(rel) == Some(&hash) {
        return Ok(());
    }
    recorded.insert(rel.to_string(), hash);
    let manifest_path = pull_manifest_path(workspace, agentic_root);
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&recorded)? + "\n",
    )?;
    Ok(())
}

/// Whether an agent file belongs to the OpenCode projection.
fn is_opencode_path(target_path: &str) -> bool {
    target_path == "opencode.json" || target_path.starts_with(".opencode/")
}

/// Whether an agent file belongs to the projection of the runtime the
/// project does not use (`.opencode/`/`opencode.json` for Claude Code,
/// `.claude/` for OpenCode). Same rule as `nexus status`.
pub(crate) fn is_other_runtime_path(target_path: &str, is_claude: bool) -> bool {
    if is_claude {
        is_opencode_path(target_path)
    } else {
        target_path.starts_with(".claude/")
    }
}

/// The [`super::projection_cleanup::CleanupContext`] for this pull: the
/// selected runtime's paths are kept; the unselected runtime's rendered
/// content identifies its unmodified files.
pub(crate) fn cleanup_context(
    af_export: &nexus_core::api::AgentFileExportResponse,
    skills: &[nexus_core::api::ExportedSkill],
    agentic_root: &str,
    project_name: &str,
    is_claude: bool,
    force: bool,
) -> super::projection_cleanup::CleanupContext {
    let mut ctx = super::projection_cleanup::CleanupContext {
        agentic_root: agentic_root.to_string(),
        project_name: project_name.to_string(),
        plugin_filenames: super::init::platform_plugin_filenames(),
        mcp_server_names: af_export.mcp_servers.keys().cloned().collect(),
        force,
        ..Default::default()
    };
    for af in &af_export.agent_files {
        let ccx_file = af.category == super::ccx::CCX_CATEGORY;
        if is_other_runtime_path(&af.target_path, is_claude) || (ccx_file && !is_claude) {
            ctx.add_known(&af.target_path, &af.body);
        } else {
            ctx.keep.insert(af.target_path.clone());
        }
    }
    for skill in skills {
        for (path, _) in render_skill_files(skill, agentic_root) {
            ctx.keep.insert(path);
        }
        if is_claude {
            if let Some((path, content)) = render_command_file(skill, agentic_root) {
                ctx.add_known(&path, &content);
            }
        } else {
            for (path, content) in claude_render::render_claude_skill_files(skill) {
                ctx.add_known(&path, &content);
            }
        }
    }
    if !is_claude {
        for (path, content) in
            claude_render::claude_agent_files(&af_export.actors, &af_export.agent_files)
        {
            ctx.add_known(&path, &content);
        }
    }
    ctx
}

fn print_synced(path: &str) {
    println!("   {} {}", style("~").bold().blue(), path);
}

/// Write all directives to `.claude/directives.md` as a single Markdown file.
///
/// Directives are grouped by category, with priority indicated inline.
/// High and urgent directives are tagged with `[HIGH]` / `[URGENT]`.
pub fn write_directives(
    target: &Path,
    directives: &[nexus_core::api::ExportedDirective],
    agentic_root: &str,
) -> anyhow::Result<()> {
    let dir = target.join(agentic_root);
    fs::create_dir_all(&dir)?;

    let content = render_directives_markdown(directives);

    let path = dir.join("directives.md");
    let rel = format!("{}/directives.md", agentic_root);
    if !content_matches(&path, &content) {
        fs::write(&path, &content)?;
        print_synced(&rel);
    }
    // Recorded like the other generated files, so `nexus status` can tell
    // a local edit (DRIFTED) from a new backend version (UPDATE).
    record_generated(target, agentic_root, &rel, &content)?;

    Ok(())
}

/// Render directives into a Markdown string.
///
/// Exported as a standalone function for testability.
pub fn render_directives_markdown(directives: &[nexus_core::api::ExportedDirective]) -> String {
    let mut content = String::from(
        "---\ntype: project-directives\nsource: nexus-platform\n---\n\n# Project Directives\n\n",
    );

    // Group by category (BTreeMap for stable ordering)
    let mut categories: std::collections::BTreeMap<
        String,
        Vec<&nexus_core::api::ExportedDirective>,
    > = std::collections::BTreeMap::new();
    for d in directives {
        categories.entry(d.category.clone()).or_default().push(d);
    }

    for (category, items) in &categories {
        content.push_str(&format!("## {}\n\n", capitalize(category)));

        for d in items {
            let priority_tag = match d.priority.as_str() {
                "high" => " [HIGH]".to_string(),
                "urgent" => " [URGENT]".to_string(),
                _ => String::new(),
            };

            content.push_str(&format!("### {}{}\n\n", d.title, priority_tag));

            if let Some(ref body) = d.body {
                if !body.is_empty() {
                    content.push_str(body);
                    content.push_str("\n\n");
                }
            }
        }
    }

    format!("{}\n", content.trim_end())
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

// ---------------------------------------------------------------------------
// Tasks export
// ---------------------------------------------------------------------------

/// Write open tasks as `<agentic_root>/TASKS.md`.
///
/// The file is always overwritten (it is a snapshot of the current backlog,
/// not a user-editable document).
pub fn write_tasks(
    workspace: &Path,
    tasks: &[nexus_core::api::TaskSummary],
    agentic_root: &str,
) -> anyhow::Result<()> {
    let dir = workspace.join(agentic_root);
    fs::create_dir_all(&dir)?;

    let priority_order = |p: &str| -> u8 {
        match p {
            "urgent" => 0,
            "high" => 1,
            "medium" => 2,
            "low" => 3,
            _ => 4,
        }
    };

    let status_label = |s: &str| -> &'static str {
        match s {
            "open" => "Open",
            "in_progress" => "In Progress",
            "blocked" => "Blocked",
            "done" => "Done",
            "cancelled" => "Cancelled",
            _ => "Unknown",
        }
    };

    let priority_label = |p: &str| -> &'static str {
        match p {
            "urgent" => "URGENT",
            "high" => "High",
            "medium" => "Medium",
            "low" => "Low",
            _ => "Normal",
        }
    };

    // Sort by priority (urgent first), then by updated_at descending
    let mut sorted: Vec<_> = tasks.iter().collect();
    sorted.sort_by(|a, b| {
        let pa = priority_order(&a.priority);
        let pb = priority_order(&b.priority);
        pa.cmp(&pb).then_with(|| b.updated_at.cmp(&a.updated_at))
    });

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("source: nexus-platform\n");
    out.push_str("---\n\n");
    out.push_str("# Active Tasks\n\n");
    out.push_str(&format!(
        "> {} open task{} pulled from Nexus platform.\n\n",
        sorted.len(),
        if sorted.len() == 1 { "" } else { "s" },
    ));

    for task in &sorted {
        let status = status_label(&task.status);
        let priority = priority_label(&task.priority);
        let checkbox = if task.status == "done" { "[x]" } else { "[ ]" };

        out.push_str(&format!(
            "- {} **{}** `[{}]` `{}`\n",
            checkbox, task.title, status, priority,
        ));

        if let Some(ref desc) = task.description {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                // Indent description lines under the list item
                for line in trimmed.lines().take(5) {
                    out.push_str(&format!("  {}\n", line));
                }
                if trimmed.lines().count() > 5 {
                    out.push_str("  _...truncated_\n");
                }
            }
        }
    }

    fs::write(dir.join("TASKS.md"), &out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test shim: write a skill's files through the pull renderer.
    fn write_skill(
        dir: &Path,
        skill: &nexus_core::api::ExportedSkill,
        root: &str,
    ) -> anyhow::Result<()> {
        sync_generated_files(dir, root, &render_skill_files(skill, root), true).map(|_| ())
    }
    use nexus_core::api::ExportedDirective;

    fn temp_dir(suffix: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-pull-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── workspace files: newline tolerance and overwrite guard ─────────────

    /// `git init` + one commit of `files` in `dir` (signing disabled, fixed
    /// identity), for tests that need tracked files and a HEAD.
    pub(crate) fn init_git_repo(dir: &Path, files: &[(&str, &str)]) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        for (rel, content) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            git(&["add", rel]);
        }
        git(&["commit", "-q", "--allow-empty", "-m", "initial"]);
    }

    fn ws_manifest(rel: &str, recorded_body: &str) -> serde_json::Value {
        serde_json::json!({ rel: { "target_path": rel, "hash": sha256_hex(recorded_body) } })
    }

    #[test]
    fn test_decide_workspace_write_matrix() {
        use WorkspaceAction as A;
        let none = || None;
        let write = |m, r| A::Write {
            overwrote_modified: m,
            reverted_commit: r,
        };
        assert_eq!(
            decide_workspace_write(None, "x", false, None, none, false, false),
            write(false, false)
        );
        // Trailing newline only: unchanged, even with --force.
        assert_eq!(
            decide_workspace_write(Some("x\n"), "x", true, None, none, true, true),
            A::Unchanged
        );
        assert_eq!(
            decide_workspace_write(Some("mine"), "x", false, None, none, false, false),
            A::Skip(WorkspaceSkip::UserManaged)
        );
        let rec = sha256_hex("old");
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), none, false, false),
            A::Skip(WorkspaceSkip::LocallyModified)
        );
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), none, true, false),
            write(true, false)
        );
        // Unmodified since the last pull (modulo newline): plain update.
        assert_eq!(
            decide_workspace_write(Some("old\n"), "new", true, Some(&rec), none, false, false),
            write(false, false)
        );
        // Committed version differs from the fork: -y alone never reverts it.
        let head = || Some("committed\n".to_string());
        assert_eq!(
            decide_workspace_write(
                Some("committed"),
                "old",
                true,
                Some(&rec),
                head,
                true,
                false
            ),
            A::Skip(WorkspaceSkip::CommittedDiffers)
        );
        assert_eq!(
            decide_workspace_write(Some("committed"), "old", true, Some(&rec), head, true, true),
            write(false, true)
        );
        // Local differs from HEAD (uncommitted edit): not a committed revert.
        let head = || Some("other".to_string());
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), head, false, false),
            A::Skip(WorkspaceSkip::LocallyModified)
        );
    }

    #[test]
    fn test_sync_workspace_file_trailing_newline_no_flip_flop() {
        let dir = temp_dir("ws-newline");
        let server = "{\"packages\":[]}";
        // Committed by an end-of-file fixer with a final newline.
        fs::write(dir.join("devbox.json"), format!("{server}\n")).unwrap();
        let manifest = ws_manifest("devbox.json", server);
        for (force, explicit) in [(false, false), (true, true)] {
            let mut report = WorkspaceSyncReport::default();
            let written = sync_workspace_file(
                &dir,
                "devbox.json",
                server,
                false,
                force,
                explicit,
                &manifest,
                &mut report,
            )
            .unwrap();
            assert!(!written, "newline-only difference must not be rewritten");
            assert!(report.overwrote_modified.is_empty());
            assert!(report.skipped_modified.is_empty());
        }
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            format!("{server}\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_writes_single_trailing_newline() {
        let dir = temp_dir("ws-write-newline");
        let mut report = WorkspaceSyncReport::default();
        let manifest = serde_json::json!({});
        assert!(sync_workspace_file(
            &dir,
            "scripts/devbox/a.sh",
            "echo a",
            true,
            false,
            false,
            &manifest,
            &mut report
        )
        .unwrap());
        assert!(sync_workspace_file(
            &dir,
            "devbox.json",
            "{}\n\n",
            false,
            false,
            false,
            &manifest,
            &mut report
        )
        .unwrap());
        assert_eq!(
            fs::read_to_string(dir.join("scripts/devbox/a.sh")).unwrap(),
            "echo a\n"
        );
        assert_eq!(fs::read_to_string(dir.join("devbox.json")).unwrap(), "{}\n");
        // The manifest keeps the backend's hash, so the next pull is clean.
        let m = super::super::sync::load_manifest_pub(&dir);
        assert_eq!(
            m["devbox.json"]["hash"].as_str().unwrap(),
            sha256_hex("{}\n\n")
        );
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            "{}\n\n",
            false,
            false,
            false,
            &m,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_locally_modified_skipped_without_force() {
        // Same rule for the v1 fallback, which used to overwrite edits.
        let dir = temp_dir("ws-modified");
        fs::write(dir.join("devbox.json"), "edited\n").unwrap();
        let manifest = ws_manifest("devbox.json", "old");
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            "new",
            false,
            false,
            false,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        assert_eq!(report.skipped_modified, vec!["devbox.json".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            "edited\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_guards_committed_file() {
        let dir = temp_dir("ws-committed");
        let committed = "{\"packages\":[\"new\"]}\n";
        let fork = "{\"packages\":[\"old\"]}";
        init_git_repo(&dir, &[("devbox.json", committed)]);
        assert_eq!(
            git_head_content(&dir, "devbox.json").as_deref(),
            Some(committed)
        );
        assert_eq!(git_head_content(&dir, "untracked.json"), None);
        // The last pull wrote the (older) fork version; the repo moved on.
        let manifest = ws_manifest("devbox.json", fork);

        // -y (force without explicit flag) must not revert the commit.
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            fork,
            false,
            true,
            false,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        assert_eq!(report.skipped_committed, vec!["devbox.json".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            committed
        );

        // --force: overwritten, reported prominently.
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            fork,
            false,
            true,
            true,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(written);
        assert_eq!(report.overwrote_committed, vec!["devbox.json".to_string()]);
        assert!(report.overwrote_modified.is_empty());
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            format!("{fork}\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_git_head_content_outside_repo_is_none() {
        let dir = temp_dir("ws-no-git");
        fs::write(dir.join("devbox.json"), "{}").unwrap();
        // temp dirs may live inside a repo on some machines; only assert
        // when git itself says this is not a work tree.
        let in_repo = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&dir)
            .output()
            .is_ok_and(|o| o.status.success());
        if !in_repo {
            assert_eq!(git_head_content(&dir, "devbox.json"), None);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ── projection switch (v0.29.0) ────────────────────────────────────────

    #[test]
    fn test_is_other_runtime_path_is_symmetric() {
        assert!(is_other_runtime_path(".opencode/plugins/x.ts", true));
        assert!(is_other_runtime_path("opencode.json", true));
        assert!(!is_other_runtime_path(".claude/rules/a.md", true));
        assert!(is_other_runtime_path(".claude/rules/a.md", false));
        assert!(!is_other_runtime_path(".opencode/plugins/x.ts", false));
        assert!(!is_other_runtime_path(".nexus/AGENTS.md", false));
        assert!(!is_other_runtime_path(".nexus/AGENTS.md", true));
    }

    fn export_for_cleanup() -> nexus_core::api::AgentFileExportResponse {
        let af = |key: &str, path: &str, category: &str| {
            serde_json::json!({
                "file_key": key, "target_path": path, "name": key,
                "category": category, "version": 1, "body": format!("body {key}\n")
            })
        };
        serde_json::from_value(serde_json::json!({
            "project_id": "p",
            "project_name": "Demo",
            "count": 4,
            "agentic_root": ".nexus",
            "agent_files": [
                af("agents", ".nexus/AGENTS.md", "agent"),
                af("plugin", ".opencode/plugins/nexus-x.ts", "plugin"),
                af("rules", ".claude/rules/10.md", "claude_experience"),
                af("actor", ".nexus/actors/planner.md", "actor"),
            ],
            "mcp_servers": { "task-master-ai": { "command": "npx" } }
        }))
        .unwrap()
    }

    #[test]
    fn test_cleanup_context_claude_project_keeps_claude_side() {
        let export = export_for_cleanup();
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-a".into(),
            name: "a".into(),
            description: None,
            version: 1,
            body: None,
            command_slug: Some("a".into()),
            pinned: false,
            resources: vec![],
        };
        let ctx = cleanup_context(&export, &[skill], ".nexus", "Demo", true, false);
        assert!(ctx.keep.contains(".nexus/AGENTS.md"));
        assert!(ctx.keep.contains(".claude/rules/10.md"));
        assert!(ctx.keep.contains(".nexus/skills/nx-a/SKILL.md"));
        assert!(!ctx.keep.contains(".opencode/plugins/nexus-x.ts"));
        // The OpenCode side is known content (identifies unmodified files).
        assert!(ctx.known.contains_key(".opencode/plugins/nexus-x.ts"));
        assert!(ctx.known.contains_key(".opencode/commands/a.md"));
        assert_eq!(ctx.mcp_server_names, vec!["task-master-ai".to_string()]);
        assert!(!ctx.force);
    }

    #[test]
    fn test_cleanup_context_opencode_project_knows_claude_side() {
        let export = export_for_cleanup();
        let ctx = cleanup_context(&export, &[], ".nexus", "Demo", false, true);
        assert!(ctx.keep.contains(".opencode/plugins/nexus-x.ts"));
        // CCX is Claude-only: known, never kept, for an OpenCode project.
        assert!(!ctx.keep.contains(".claude/rules/10.md"));
        assert!(ctx.known.contains_key(".claude/rules/10.md"));
        // Actor agents rendered for Claude Code are known content.
        assert!(ctx.known.contains_key(".claude/agents/planner.md"));
        assert!(ctx.force);
    }

    // ── generated files (skills/commands), NEXUS-APP dispatch 4820e584 ─────

    fn gen_files() -> Vec<(String, String)> {
        vec![
            (
                ".nexus/skills/nx-a/SKILL.md".to_string(),
                "a v1\n".to_string(),
            ),
            (
                ".opencode/commands/a.md".to_string(),
                "cmd v1\n".to_string(),
            ),
        ]
    }

    fn never_prompt() -> anyhow::Result<bool> {
        panic!("must not prompt")
    }

    #[test]
    fn test_agent_file_matches_ignores_generated_at_only() {
        let dir = temp_dir("af-matches");
        let p = dir.join("AGENTS.md");
        fs::write(&p, "---\ngenerated_at: 2026-09-25T01:00:00Z\n---\nbody\n").unwrap();
        assert!(agent_file_matches(
            &p,
            "---\ngenerated_at: 2026-09-25T02:00:00Z\n---\nbody\n"
        ));
        assert!(!agent_file_matches(
            &p,
            "---\ngenerated_at: 2026-09-25T02:00:00Z\n---\nnew body\n"
        ));
        assert!(!agent_file_matches(&dir.join("missing.md"), "x"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_classify_generated() {
        let d = "desired";
        let h = sha256_hex;
        assert_eq!(classify_generated(None, d, None), GeneratedState::Write);
        assert_eq!(
            classify_generated(Some(b"desired"), d, None),
            GeneratedState::Unchanged
        );
        // Unmodified since the last pull: update without asking.
        assert_eq!(
            classify_generated(Some(b"old"), d, Some(&h("old"))),
            GeneratedState::Write
        );
        // Edited locally (or unknown origin): needs confirmation.
        assert_eq!(
            classify_generated(Some(b"edited"), d, Some(&h("old"))),
            GeneratedState::LocallyModified
        );
        assert_eq!(
            classify_generated(Some(b"edited"), d, None),
            GeneratedState::LocallyModified
        );
    }

    #[test]
    fn test_generated_pull_twice_no_prompt_no_writes() {
        let dir = temp_dir("gen-twice");
        let files = gen_files();
        let (w1, s1) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!((w1, s1.len()), (2, 0));
        let (w2, s2) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!((w2, s2.len()), (0, 0), "second pull must not write");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_existing_identical_files_no_prompt() {
        // Files written by an older CLI with identical content (no hash
        // record yet): silent, and recorded for next time.
        let dir = temp_dir("gen-identical");
        for (p, c) in gen_files() {
            fs::create_dir_all(dir.join(&p).parent().unwrap()).unwrap();
            fs::write(dir.join(&p), c).unwrap();
        }
        let (w, s) =
            sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        assert_eq!((w, s.len()), (0, 0));
        assert_eq!(load_pull_manifest(&dir, ".nexus").len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_new_revision_updates_without_prompt() {
        let dir = temp_dir("gen-update");
        sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        let mut files = gen_files();
        files[0].1 = "a v2\n".to_string();
        let (w, _) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!(w, 1);
        assert_eq!(
            fs::read_to_string(dir.join(".nexus/skills/nx-a/SKILL.md")).unwrap(),
            "a v2\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_locally_edited_prompts_only_for_that_file() {
        let dir = temp_dir("gen-edited");
        sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        let skill = dir.join(".nexus/skills/nx-a/SKILL.md");
        fs::write(&skill, "my edit\n").unwrap();
        let mut files = gen_files();
        files[1].1 = "cmd v2\n".to_string();

        // Declined: the edited skill is kept, the rest is still applied.
        let mut prompted = false;
        let (w, skipped) = sync_generated_files_with(&dir, ".nexus", &files, false, || {
            prompted = true;
            Ok(false)
        })
        .unwrap();
        assert!(prompted);
        assert_eq!(w, 1);
        assert_eq!(skipped, vec![".nexus/skills/nx-a/SKILL.md".to_string()]);
        assert_eq!(fs::read_to_string(&skill).unwrap(), "my edit\n");
        assert_eq!(
            fs::read_to_string(dir.join(".opencode/commands/a.md")).unwrap(),
            "cmd v2\n"
        );
        // Still reported as modified next time (hash record kept).
        let (_, skipped) =
            sync_generated_files_with(&dir, ".nexus", &files, false, || Ok(false)).unwrap();
        assert_eq!(skipped.len(), 1);

        // --force overwrites without asking.
        let (w, skipped) =
            sync_generated_files_with(&dir, ".nexus", &files, true, never_prompt).unwrap();
        assert_eq!((w, skipped.len()), (1, 0));
        assert_eq!(fs::read_to_string(&skill).unwrap(), "a v1\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_command_file_requires_slug() {
        let mut skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-a".to_string(),
            name: "A".to_string(),
            description: None,
            version: 1,
            body: None,
            command_slug: None,
            pinned: false,
            resources: vec![],
        };
        assert!(render_command_file(&skill, ".nexus").is_none());
        skill.command_slug = Some("a".to_string());
        let (path, content) = render_command_file(&skill, ".nexus").unwrap();
        assert_eq!(path, ".opencode/commands/a.md");
        assert!(content.contains("`.nexus/skills/nx-a/SKILL.md`"));
    }

    #[test]
    fn test_unique_agent_files_keeps_first_per_path() {
        let af = |key: &str, path: &str| nexus_core::api::ExportedAgentFile {
            file_key: key.into(),
            target_path: path.into(),
            name: key.into(),
            description: None,
            category: "general".into(),
            version: 1,
            body: key.into(),
            content_hash: None,
            agent_file_id: None,
        };
        let files = vec![
            af("rtk-filters-default", ".rtk/filters.toml"),
            af("AGENTS.md", ".nexus/AGENTS.md"),
            af("rtk-filters-rust", ".rtk/filters.toml"),
            af("rtk-filters-docker", ".rtk/filters.toml"),
        ];
        let (kept, dropped) = unique_agent_files(&files);
        assert_eq!(
            kept.iter().map(|a| a.file_key.as_str()).collect::<Vec<_>>(),
            vec!["rtk-filters-default", "AGENTS.md"]
        );
        assert_eq!(
            dropped,
            vec![(
                ".rtk/filters.toml".to_string(),
                vec![
                    "rtk-filters-rust".to_string(),
                    "rtk-filters-docker".to_string()
                ]
            )]
        );
    }

    #[test]
    fn test_write_directives_records_hash_for_status() {
        let dir = temp_dir("directives-record");
        let directives = vec![ExportedDirective {
            id: "1".into(),
            title: "Use HTTPS".into(),
            body: Some("Always.".into()),
            category: "security".into(),
            priority: "normal".into(),
        }];
        write_directives(&dir, &directives, ".nexus").unwrap();
        let content = render_directives_markdown(&directives);
        assert_eq!(
            load_pull_manifest(&dir, ".nexus").get(".nexus/directives.md"),
            Some(&sha256_hex(&content))
        );
        // A local edit is then distinguishable from a backend change.
        assert_eq!(
            classify_generated(
                Some(b"edited"),
                &content,
                load_pull_manifest(&dir, ".nexus")
                    .get(".nexus/directives.md")
                    .map(String::as_str)
            ),
            GeneratedState::LocallyModified
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_opencode_path() {
        assert!(is_opencode_path("opencode.json"));
        assert!(is_opencode_path(".opencode/plugins/x.ts"));
        assert!(!is_opencode_path(".nexus/AGENTS.md"));
        assert!(!is_opencode_path(".opencodex/y"));
    }

    #[test]
    fn test_importable_files_skips_nexus_generated() {
        let dir = temp_dir("importable");
        fs::create_dir_all(dir.join(".claude/skills/mine")).unwrap();
        fs::create_dir_all(dir.join(".claude/skills/nx-gen")).unwrap();
        fs::write(dir.join(".claude/skills/mine/SKILL.md"), "my skill").unwrap();
        fs::write(
            dir.join(".claude/skills/nx-gen/SKILL.md"),
            "---\nsource: nexus-platform\n---\n",
        )
        .unwrap();
        fs::write(dir.join(".claude/settings.json"), "{}").unwrap();
        fs::write(
            dir.join("CLAUDE.md"),
            "<!-- BEGIN:nexus-managed -->\nx\n<!-- END:nexus-managed -->\n",
        )
        .unwrap();
        fs::write(dir.join("AGENTS.md"), "tracked").unwrap();
        let manifest = serde_json::json!({"AGENTS.md": {"target_path": "AGENTS.md", "hash": "x"}});

        assert_eq!(
            importable_files(&dir, true, &manifest),
            vec![".claude/skills/mine/".to_string()]
        );
        // For an OpenCode project .claude/settings.json is not Nexus's.
        assert!(
            importable_files(&dir, false, &manifest).contains(&".claude/settings.json".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_start_label() {
        use nexus_core::api::RunTarget;
        let t = |tool: &str, ws: Option<&str>| RunTarget {
            tool: tool.to_string(),
            workspace: ws.map(str::to_string),
            layout: None,
        };
        assert_eq!(
            run_start_label(Some(&t("claude", Some("zellij"))), true),
            "the Claude Code workspace"
        );
        assert_eq!(
            run_start_label(Some(&t("claude", Some("none"))), true),
            "Claude Code"
        );
        assert_eq!(
            run_start_label(Some(&t("opencode", None)), false),
            "OpenCode"
        );
        assert_eq!(run_start_label(None, true), "Claude Code");
        assert_eq!(run_start_label(None, false), "OpenCode");
    }

    #[test]
    fn test_write_skill_includes_description_and_command_slug() {
        // NEXUS-APP dispatch 5ddd6355: same defect as init.rs's write_skill.
        let dir = temp_dir("write-skill");
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-test-skill".to_string(),
            name: "Test Skill".to_string(),
            description: Some("A test skill".to_string()),
            version: 1,
            body: Some("Do the thing.".to_string()),
            command_slug: Some("nexus-test-skill".to_string()),
            pinned: false,
            resources: vec![],
        };

        write_skill(&dir, &skill, ".claude").unwrap();

        let content =
            fs::read_to_string(dir.join(".claude/skills/nx-test-skill/SKILL.md")).unwrap();
        assert!(content.contains(r#"description: "A test skill""#));
        assert!(content.contains("command_slug: nexus-test-skill"));
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_skill_strips_duplicate_backend_frontmatter() {
        let dir = temp_dir("write-skill-dup-frontmatter");
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-dup".to_string(),
            name: "Test Skill".to_string(),
            description: Some("A test skill".to_string()),
            version: 1,
            body: Some(
                "---\nskill_id: nx-dup\nname: Test Skill\nversion: 1\n\
                 command_slug: nexus-dup\nsource: nexus-platform\n---\n\n\
                 Do the thing."
                    .to_string(),
            ),
            command_slug: Some("nexus-dup".to_string()),
            pinned: false,
            resources: vec![],
        };

        write_skill(&dir, &skill, ".claude").unwrap();

        let content = fs::read_to_string(dir.join(".claude/skills/nx-dup/SKILL.md")).unwrap();
        assert_eq!(
            content.matches("---").count(),
            2,
            "expected exactly one frontmatter block, got: {content}"
        );
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_directives_groups_by_category() {
        let directives = vec![
            ExportedDirective {
                id: "1".into(),
                title: "Use HTTPS".into(),
                body: Some("Always use HTTPS in production.".into()),
                category: "security".into(),
                priority: "high".into(),
            },
            ExportedDirective {
                id: "2".into(),
                title: "Run migrations locally".into(),
                body: Some("Use makefile targets.".into()),
                category: "migration".into(),
                priority: "medium".into(),
            },
            ExportedDirective {
                id: "3".into(),
                title: "Enable MFA".into(),
                body: None,
                category: "security".into(),
                priority: "urgent".into(),
            },
        ];

        let md = render_directives_markdown(&directives);

        // Frontmatter
        assert!(md.starts_with("---\ntype: project-directives\n"));
        assert!(md.contains("source: nexus-platform"));

        // Category headings (BTreeMap => alphabetical: Migration before Security)
        let migration_pos = md.find("## Migration").unwrap();
        let security_pos = md.find("## Security").unwrap();
        assert!(
            migration_pos < security_pos,
            "categories should be alphabetical"
        );

        // Priority tags
        assert!(md.contains("### Use HTTPS [HIGH]"));
        assert!(md.contains("### Enable MFA [URGENT]"));
        assert!(md.contains("### Run migrations locally\n")); // no tag for medium

        // Body content
        assert!(md.contains("Always use HTTPS in production."));
        assert!(md.contains("Use makefile targets."));

        // Ends with newline
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn test_render_directives_empty() {
        let md = render_directives_markdown(&[]);
        assert!(md.contains("# Project Directives"));
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn test_render_directives_empty_body_skipped() {
        let directives = vec![ExportedDirective {
            id: "1".into(),
            title: "No body directive".into(),
            body: Some("".into()),
            category: "general".into(),
            priority: "low".into(),
        }];

        let md = render_directives_markdown(&directives);
        assert!(md.contains("### No body directive\n"));
        // Should NOT have double newlines after the heading (empty body skipped)
        assert!(!md.contains("### No body directive\n\n\n"));
    }

    #[test]
    fn test_render_directives_null_body() {
        let directives = vec![ExportedDirective {
            id: "1".into(),
            title: "Null body".into(),
            body: None,
            category: "general".into(),
            priority: "medium".into(),
        }];

        let md = render_directives_markdown(&directives);
        assert!(md.contains("### Null body\n"));
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("security"), "Security");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("a"), "A");
        assert_eq!(capitalize("ABC"), "ABC");
        assert_eq!(capitalize("migration"), "Migration");
    }

    #[test]
    fn test_write_directives_creates_file() {
        let tmp = std::env::temp_dir().join("nexus_test_write_dir");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let directives = vec![ExportedDirective {
            id: "d1".into(),
            title: "Test directive".into(),
            body: Some("Do the thing.".into()),
            category: "testing".into(),
            priority: "high".into(),
        }];

        write_directives(&tmp, &directives, ".claude").unwrap();

        let path = tmp.join(".claude/directives.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("### Test directive [HIGH]"));
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&tmp);
    }

    // -- CLAUDE.md template tests --

    #[test]
    fn test_render_claude_md_with_directives() {
        let md = render_claude_md("MyProject", true, ".claude");
        assert!(md.contains("source: nexus-platform"));
        assert!(md.contains("project: MyProject"));
        assert!(md.contains("Load project directives from `.claude/directives.md`"));
        assert!(md.contains("1. Load agent identity"));
        assert!(md.contains("2. Connect to the Nexus MCP server"));
        assert!(md.contains("3. Load project directives"));
        assert!(md.contains("4. Review active planning"));
        assert!(md.contains("5. Continue with the active workstream"));
    }

    #[test]
    fn test_render_claude_md_without_directives() {
        let md = render_claude_md("MyProject", false, ".claude");
        assert!(md.contains("source: nexus-platform"));
        assert!(!md.contains("directives"));
        assert!(md.contains("3. Review active planning"));
        assert!(md.contains("4. Continue with the active workstream"));
    }

    #[test]
    fn test_render_claude_md_environment_section() {
        let md = render_claude_md("Test", true, ".claude");
        assert!(md.contains("Read secrets only from `.env.local`"));
        assert!(md.contains("NEVER:"));
        assert!(md.contains("- print secrets"));
    }

    // -- AGENTS.md template tests --

    #[test]
    fn test_render_agents_md() {
        let md = render_agents_md("MyProject");
        assert!(md.contains("source: nexus-platform"));
        assert!(md.contains("project: MyProject"));
        assert!(md.contains("app-agent (PRIMARY)"));
        assert!(md.contains("# GLOBAL RULES"));
        assert!(md.contains("Correctness over speed"));
    }

    // -- is_managed_file tests --

    #[test]
    fn test_is_managed_file_with_marker() {
        let tmp = std::env::temp_dir().join("nexus_test_managed_yes");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.md");
        fs::write(&path, "---\nsource: nexus-platform\n---\n# Hello").unwrap();
        assert!(is_managed_file(&path));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_managed_file_without_marker() {
        let tmp = std::env::temp_dir().join("nexus_test_managed_no");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.md");
        fs::write(&path, "---\ntype: bootstrap\n---\n# Hello").unwrap();
        assert!(!is_managed_file(&path));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_managed_file_nonexistent() {
        let path = std::env::temp_dir().join("nexus_test_managed_nofile/nope.md");
        assert!(!is_managed_file(&path));
    }

    // -- write_agent_file tests --

    // -- validate_agent_file_target_path (NEXUS-APP ADR-0117 hardening,
    // dispatch bb782869) --

    #[test]
    fn test_validate_target_path_accepts_plain_relative_path() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "AGENTS.md").is_ok());
        assert!(validate_agent_file_target_path(ws, ".claude/skills/foo/SKILL.md").is_ok());
    }

    #[test]
    fn test_validate_target_path_rejects_parent_dir_traversal() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "../../etc/passwd").is_err());
        assert!(validate_agent_file_target_path(ws, ".claude/../../escape").is_err());
    }

    #[test]
    fn test_validate_target_path_rejects_absolute_unix_path() {
        // The bug this hardens against: Path::join replaces the base
        // entirely for an absolute path, so this previously slipped past
        // the '..'-only check and would have written outside the
        // workspace with no '..' anywhere in the string.
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "/etc/passwd").is_err());
        assert!(validate_agent_file_target_path(ws, "/tmp/evil").is_err());
    }

    #[test]
    fn test_validate_target_path_accepts_dot_slash_prefix() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "./AGENTS.md").is_ok());
    }

    #[test]
    fn test_lexically_normalize_resolves_dot_dot() {
        assert_eq!(
            lexically_normalize(Path::new("/a/b/../c")),
            Path::new("/a/c")
        );
        assert_eq!(lexically_normalize(Path::new("/a/./b")), Path::new("/a/b"));
    }

    #[test]
    fn test_write_agent_file_refuses_absolute_target_path() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af_absolute");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "evil".into(),
            target_path: "/tmp/nexus-escape-test-should-not-exist".into(),
            name: "evil".into(),
            description: None,
            category: "agent".into(),
            version: 1,
            body: "pwned".into(),
            content_hash: None,
            agent_file_id: None,
        };

        let result = write_agent_file(&tmp, &af);
        assert!(
            result.is_err(),
            "expected absolute target_path to be rejected"
        );
        assert!(
            !Path::new("/tmp/nexus-escape-test-should-not-exist").exists(),
            "escape must not have written outside the workspace"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_agent_file_creates_file() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "agents-md".into(),
            target_path: "AGENTS.md".into(),
            name: "AGENTS.md".into(),
            description: None,
            category: "agent".into(),
            version: 1,
            body: "---\ntype: agent-policy\nsource: nexus-platform\n---\n# Test".into(),
            content_hash: None,
            agent_file_id: None,
        };

        write_agent_file(&tmp, &af).unwrap();

        let path = tmp.join("AGENTS.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("source: nexus-platform"));
        assert!(content.contains("# Test"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_agent_file_creates_subdirectories() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af_sub");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "claude-md".into(),
            target_path: ".claude/CLAUDE.md".into(),
            name: "CLAUDE.md".into(),
            description: Some("Bootstrap file".into()),
            category: "agent".into(),
            version: 2,
            body: "# Bootstrap\nTest content".into(),
            content_hash: None,
            agent_file_id: None,
        };

        write_agent_file(&tmp, &af).unwrap();

        let path = tmp.join(".claude/CLAUDE.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Bootstrap"));

        let _ = fs::remove_dir_all(&tmp);
    }

    // -- is_protected_path tests --

    #[test]
    fn test_protected_path_env_files() {
        assert!(is_protected_path(".env"));
        assert!(is_protected_path(".env.local"));
        assert!(is_protected_path(".env.nexus.local"));
        assert!(is_protected_path(".env.production"));
    }

    #[test]
    fn test_protected_path_key_files() {
        assert!(is_protected_path("server.pem"));
        assert!(is_protected_path("private.key"));
        assert!(is_protected_path("id_rsa"));
        assert!(is_protected_path("id_ed25519"));
        assert!(is_protected_path("keystore.p12"));
    }

    #[test]
    fn test_protected_path_safe_files() {
        assert!(!is_protected_path("AGENTS.md"));
        assert!(!is_protected_path(".nexus/CLAUDE.md"));
        assert!(!is_protected_path("opencode.json"));
        assert!(!is_protected_path("skills/nx-init/SKILL.md"));
    }

    #[test]
    fn test_write_agent_file_refuses_existing_env() {
        let tmp = std::env::temp_dir().join("nexus_test_protected_env");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Pre-create the .env.local file
        fs::write(tmp.join(".env.local"), "SECRET=value").unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "env-local".into(),
            target_path: ".env.local".into(),
            name: ".env.local".into(),
            description: None,
            category: "config".into(),
            version: 1,
            body: "OVERWRITTEN=true".into(),
            content_hash: None,
            agent_file_id: None,
        };

        let result = write_agent_file(&tmp, &af);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("protected file pattern"));

        // Original content must be preserved
        let content = fs::read_to_string(tmp.join(".env.local")).unwrap();
        assert_eq!(content, "SECRET=value");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_agent_file_export_response_deserialize() {
        let json = r##"{
            "project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
            "project_name": "NEXUS-APP",
            "agent_files": [
                {
                    "file_key": "agents-md",
                    "target_path": "AGENTS.md",
                    "name": "AGENTS.md",
                    "description": null,
                    "category": "agent",
                    "version": 1,
                    "body": "# Test"
                }
            ],
            "count": 1
        }"##;

        let resp: nexus_core::api::AgentFileExportResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.project_name, "NEXUS-APP");
        assert_eq!(resp.agent_files.len(), 1);
        assert_eq!(resp.agent_files[0].file_key, "agents-md");
        assert_eq!(resp.agent_files[0].target_path, "AGENTS.md");
        assert_eq!(resp.agent_files[0].version, 1);
        assert_eq!(resp.count, 1);
        // agentic_root defaults to ".nexus" when not in JSON (ADR-0027)
        assert_eq!(resp.agentic_root, ".nexus");
    }

    #[test]
    fn test_agent_file_export_response_with_agentic_root() {
        let json = r##"{
            "project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
            "project_name": "NEXUS-APP",
            "agent_files": [],
            "count": 0,
            "agentic_root": ".nexus"
        }"##;

        let resp: nexus_core::api::AgentFileExportResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.agentic_root, ".nexus");
    }

    // ── Agentic conflict detection tests ───────────────────────────────────

    /// Legacy helper kept for test coverage only (removed from production code
    /// by ADR-0027). Scans .claude/ for non-Nexus files.
    fn detect_agentic_conflicts(workspace: &Path) -> Vec<String> {
        let candidates = [
            ".claude/CLAUDE.md",
            ".claude/commands.md",
            "AGENTS.md",
            "CLAUDE.md",
        ];
        let mut conflicts = Vec::new();
        for rel in &candidates {
            let path = workspace.join(rel);
            if path.exists() && !is_managed_file(&path) {
                conflicts.push(rel.to_string());
            }
        }
        let skills_dir = workspace.join(".claude/skills");
        if skills_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let skill_md = entry.path().join("SKILL.md");
                    if skill_md.exists() && !is_managed_file(&skill_md) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        conflicts.push(format!(".claude/skills/{}/SKILL.md", name));
                    }
                }
            }
        }
        conflicts
    }

    #[test]
    fn test_detect_agentic_conflicts_empty_workspace() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_with_user_files() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_user");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // User-managed file (no nexus marker)
        fs::write(tmp.join(".claude/CLAUDE.md"), "# My custom config").unwrap();
        fs::write(tmp.join("AGENTS.md"), "# My agents").unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.contains(&".claude/CLAUDE.md".to_string()));
        assert!(conflicts.contains(&"AGENTS.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_skips_managed_files() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_managed");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // Nexus-managed file
        fs::write(
            tmp.join(".claude/CLAUDE.md"),
            "---\nsource: nexus-platform\n---\n# Managed",
        )
        .unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_found_for_alternate_root() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_alt");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(tmp.join(".claude/CLAUDE.md"), "# User file").unwrap();

        // detect_agentic_conflicts always returns existing non-Nexus files;
        // the caller decides whether to warn (hard) or notify (info) based
        // on whether the project uses an alternate agentic root.
        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(!conflicts.is_empty());
        assert!(conflicts.contains(&".claude/CLAUDE.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Alternate agentic root tests ───────────────────────────────────────

    #[test]
    fn test_render_claude_md_alternate_root() {
        let md = render_claude_md("MyProject", true, ".nexus");
        assert!(md.contains("Load project directives from `.nexus/directives.md`"));
    }

    #[test]
    fn test_write_directives_alternate_root() {
        let tmp = std::env::temp_dir().join("nexus_test_write_dir_alt");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let directives = vec![ExportedDirective {
            id: "d1".into(),
            title: "Alt root directive".into(),
            body: Some("Under .nexus".into()),
            category: "testing".into(),
            priority: "high".into(),
        }];

        write_directives(&tmp, &directives, ".nexus").unwrap();

        let path = tmp.join(".nexus/directives.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("### Alt root directive [HIGH]"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_mcp_configs_alternate_root() {
        let dir = temp_pull_dir("mcp-alt-root");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_alt-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // One runtime per call (NEXUS-APP dispatch 442f0e97): render the
        // Claude Code projection separately for its .mcp.json.
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_alt-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json should still be at root
        assert!(dir.join("opencode.json").exists());
        // .mcp.json is always at project root, regardless of agentic_root,
        // per Claude Code's documented project-scope location.
        assert!(dir.join(".mcp.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // export_warnings gate (dispatch e0ee68d5)
    // -----------------------------------------------------------------------

    #[test]
    fn test_confirm_export_warnings_empty_proceeds_without_prompt() {
        // No warnings -> proceed silently, regardless of bypass.
        assert!(confirm_export_warnings(&[], false).unwrap());
        assert!(confirm_export_warnings(&[], true).unwrap());
    }

    #[test]
    fn test_confirm_export_warnings_bypass_proceeds_without_prompt() {
        let warnings = vec![ExportWarning {
            code: "unverifiable_provider".to_string(),
            message: "Agent nexus-plan uses provider \"github-copilot\".".to_string(),
            agent: Some("nexus-plan".to_string()),
            model: Some("github-copilot/claude-sonnet-4.6".to_string()),
            hint: Some("Ensure github-copilot is configured.".to_string()),
        }];
        // --yes / --force bypass: must not attempt to read from stdin.
        assert!(confirm_export_warnings(&warnings, true).unwrap());
    }

    #[test]
    fn test_confirm_export_warnings_non_interactive_proceeds() {
        // The test harness's stdin is not a TTY, so the non-bypass path must
        // fall through to the "proceed, no hang" branch rather than blocking
        // on a read that would never return in CI.
        let warnings = vec![
            ExportWarning {
                code: "unverifiable_provider".to_string(),
                message: "Agent nexus-plan uses provider \"github-copilot\".".to_string(),
                agent: Some("nexus-plan".to_string()),
                model: Some("github-copilot/claude-sonnet-4.6".to_string()),
                hint: None,
            },
            ExportWarning {
                code: "route_alias_missing".to_string(),
                message: "Route alias \"balanced-fast\" is not seeded on this backend.".to_string(),
                agent: None,
                model: None,
                hint: Some("Apply migration 0218.".to_string()),
            },
        ];
        assert!(confirm_export_warnings(&warnings, false).unwrap());
    }

    #[test]
    fn test_export_warning_unknown_code_deserializes() {
        // `code` is open-ended; unknown codes must still deserialize (render
        // generically) rather than error, per the dispatch's stability contract.
        let json = r#"{"code":"some_future_code","message":"hello"}"#;
        let warning: ExportWarning = serde_json::from_str(json).unwrap();
        assert_eq!(warning.code, "some_future_code");
        assert_eq!(warning.message, "hello");
        assert!(warning.agent.is_none());
        assert!(warning.model.is_none());
        assert!(warning.hint.is_none());
    }

    #[test]
    fn test_write_mcp_configs_includes_nexus_project_id() {
        // Dispatch ef9b0b0e: NEXUS_PROJECT_ID must reach the nexus MCP server's
        // environment block so agents can bind to the correct project without
        // guessing via a project-listing tool.
        let dir = temp_pull_dir("mcp-project-id");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_project-id-token",
            "07303f0c-3713-4cb0-b03e-35f4db0c1acb",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            parsed["mcp"]["nexus"]["environment"]["NEXUS_PROJECT_ID"],
            "07303f0c-3713-4cb0-b03e-35f4db0c1acb"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_merges_opencode_instructions() {
        // Dispatch d15aa6f5: opencode_instructions from af_export must merge
        // into the top-level "instructions" array so OpenCode deterministically
        // loads <agentic_root>/AGENTS.md instead of relying on its own upward
        // auto-discovery (which has no awareness of the agentic_root convention).
        let dir = temp_pull_dir("mcp-instructions-new");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &Some(vec![".nexus/AGENTS.md".to_string()]),
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(instructions, vec![".nexus/AGENTS.md"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_preserves_existing_custom_instructions() {
        // Additive merge: a user's own pre-existing "instructions" entries
        // must survive, not be silently overwritten.
        let dir = temp_pull_dir("mcp-instructions-preserve");

        // Seed an existing opencode.json with a custom instructions entry
        // and a plugin server, so the merge path (exists && !force) is taken.
        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec!["-y".to_string(), "task-master-ai@latest".to_string()],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // Manually inject a custom instructions entry, simulating a user edit.
        let existing_content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let mut existing: serde_json::Value = serde_json::from_str(&existing_content).unwrap();
        existing["instructions"] = serde_json::json!(["CUSTOM.md"]);
        fs::write(
            dir.join("opencode.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        // Re-run with opencode_instructions set and a new plugin server, to
        // trigger needs_write via the additive-merge path (exists && !force).
        let mut plugin_servers2 = plugin_servers.clone();
        plugin_servers2.insert(
            "other-plugin".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec![],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers2,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &Some(vec![".nexus/AGENTS.md".to_string()]),
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(instructions.contains(&"CUSTOM.md"));
        assert!(instructions.contains(&".nexus/AGENTS.md"));
        assert_eq!(instructions.len(), 2, "no duplicate entries expected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_instructions_dedup_on_rerun() {
        // Running pull twice with the same opencode_instructions must not
        // duplicate the entry in the instructions[] array.
        let dir = temp_pull_dir("mcp-instructions-dedup");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec![],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );

        for _ in 0..2 {
            write_mcp_configs(
                &dir,
                "https://nexus.gatewarden.eu",
                "nxs_pat_instructions-token",
                "test-project-id",
                McpSource::Npm,
                None,
                ".nexus",
                &plugin_servers,
                &HashMap::new(),
                &None,
                &None,
                &None,
                &Some(vec![".nexus/AGENTS.md".to_string()]),
                false,
            )
            .unwrap();
        }

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(instructions, vec![".nexus/AGENTS.md"]);

        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_pull_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-pull-test-{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".claude")).unwrap();
        dir
    }

    #[test]
    fn test_write_mcp_configs_if_missing_creates_both() {
        let dir = temp_pull_dir("mcp-creates");

        // Isolate from shell environment: NEXUS_SEC_OPENAI_API_KEY must be absent
        // so the {env:} fallback is written. Devbox / .env.nexus.local may inject it.
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json must exist with literal values for Nexus credentials
        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"nexus\""));
        assert!(oc.contains("nxs_pat_pull-test-token"));
        assert!(oc.contains("https://nexus.gatewarden.eu"));
        assert!(oc.contains("npx"));
        // NEXUS_PRIVATE_TOKEN must never be an {env:} reference
        assert!(!oc.contains("{env:NEXUS_PRIVATE_TOKEN}"));
        assert!(!oc.contains("{env:NEXUS_API_URL}"));
        // NEXUS_SEC_OPENAI_API_KEY: no env-file present in temp dir and no shell var set,
        // so the {env:} fallback must be used.
        assert!(oc.contains("{env:NEXUS_SEC_OPENAI_API_KEY}"));

        // One runtime per call (NEXUS-APP dispatch 442f0e97): render the
        // Claude Code projection separately for its .mcp.json.
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // .mcp.json must exist
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("\"mcpServers\""));
        assert!(cm.contains("nxs_pat_pull-test-token"));
        // Local MCP server (dispatch af407643) must be registered alongside
        // the bootstrap "nexus" entry.
        assert!(cm.contains("\"nexus-local-tools\""));
        assert!(cm.contains("\"mcp-local\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_skips_existing() {
        let dir = temp_pull_dir("mcp-skips");

        // Pre-create both files
        fs::write(dir.join("opencode.json"), "existing-oc").unwrap();
        fs::write(dir.join(".mcp.json"), "existing-cm").unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_should-not-appear",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // Must NOT overwrite (no plugin servers, no force)
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "existing-oc"
        );
        assert_eq!(
            fs::read_to_string(dir.join(".mcp.json")).unwrap(),
            "existing-cm"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_readds_missing_nexus_server() {
        // After a switch away and back, .mcp.json may hold only the
        // operator's servers: a plain pull adds the Nexus server again.
        let dir = temp_pull_dir("mcp-readd");
        fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"mine":{"command":"mine"}}}"#,
        )
        .unwrap();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "tok",
            "pid",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"]["nexus"].is_object());
        assert!(mcp["mcpServers"]["mine"].is_object());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_creates_only_missing() {
        let dir = temp_pull_dir("mcp-partial");

        // Only opencode.json exists
        fs::write(dir.join("opencode.json"), "existing-oc").unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_partial-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json untouched (no plugins, no force)
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "existing-oc"
        );
        // .mcp.json created
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("nxs_pat_partial-token"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_local_mode() {
        let dir = temp_pull_dir("mcp-local");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-token",
            "test-project-id",
            McpSource::Local,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("tools/nexus-mcp/dist/server.js"));
        assert!(!oc.contains("npx"));

        // One runtime per call (NEXUS-APP dispatch 442f0e97): render the

        // Claude Code projection separately for its .mcp.json.

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-token",
            "test-project-id",
            McpSource::Local,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("\"command\": \"node\""));
        assert!(cm.contains("tools/nexus-mcp/dist/server.js"));

        let _ = fs::remove_dir_all(&dir);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Workspace file write tests
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_workspace_script_write_creates_file_with_content() {
        let dir = std::env::temp_dir().join("nexus_test_ws_script");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let scripts_path = dir.join(".nexus/scripts/devbox");
        fs::create_dir_all(&scripts_path).unwrap();

        let target = scripts_path.join("dbx_init.sh");
        let body = "#!/usr/bin/env bash\necho \"hello workspace\"\n";
        fs::write(&target, body).unwrap();

        // Verify content
        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("hello workspace"));
        assert!(content.starts_with("#!/usr/bin/env bash"));

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            fs::set_permissions(&target, perms).unwrap();
            let meta = fs::metadata(&target).unwrap();
            assert_eq!(meta.permissions().mode() & 0o755, 0o755);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_devbox_json_write() {
        let dir = std::env::temp_dir().join("nexus_test_ws_devbox");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let devbox_json = r#"{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.17.1/.schema/devbox.schema.json",
  "packages": {
    "nodejs": "latest",
    "git": "latest"
  },
  "env": {
    "PROJECT_NAME": "test-project"
  },
  "shell": {
    "init_hook": [".nexus/scripts/devbox/dbx_init.sh"]
  }
}"#;

        let target = dir.join("devbox.json");
        fs::write(&target, devbox_json).unwrap();

        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("0.17.1"));
        assert!(content.contains("test-project"));
        assert!(content.contains("dbx_init.sh"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_respects_managed_file_marker() {
        let dir = std::env::temp_dir().join("nexus_test_ws_managed");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // File WITHOUT managed marker — should not be overwritten
        let user_file = dir.join("devbox.json");
        fs::write(&user_file, r#"{"packages": {"custom": "1.0"}}"#).unwrap();
        assert!(!is_managed_file(&user_file));

        // File WITH managed marker — safe to overwrite
        let managed_file = dir.join("managed.json");
        fs::write(
            &managed_file,
            "---\n# source: nexus-platform\n---\n{\"packages\": {}}",
        )
        .unwrap();
        assert!(is_managed_file(&managed_file));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_scope_filter() {
        // Scope filter helper logic
        let scope_empty: Vec<String> = vec![];
        let scope_ws: Vec<String> = vec!["workspace".into()];
        let scope_skills: Vec<String> = vec!["skills".into()];

        let _pull_all_empty = scope_empty.is_empty();
        let check = |scope: &[String], name: &str| {
            scope.is_empty() || scope.iter().any(|s| s.eq_ignore_ascii_case(name))
        };

        // Empty scope = pull everything
        assert!(check(&scope_empty, "workspace"));
        assert!(check(&scope_empty, "skills"));

        // Explicit scope = only that scope
        assert!(check(&scope_ws, "workspace"));
        assert!(!check(&scope_ws, "skills"));
        assert!(!check(&scope_skills, "workspace"));
        assert!(check(&scope_skills, "skills"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Provider integration tests
    // ═══════════════════════════════════════════════════════════════════════

    fn dgx_spark_provider() -> HashMap<String, ProviderConfig> {
        let mut providers = HashMap::new();
        providers.insert(
            "dgx-spark".to_string(),
            serde_json::json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "DGX Spark (HomeLab)",
                "options": {
                    "baseURL": "http://10.0.10.121/v1"
                },
                "models": {
                    "nexus-coder-main": {
                        "name": "Nexus Coder Main"
                    }
                }
            }),
        );
        providers
    }

    #[test]
    fn test_write_mcp_configs_with_providers_writes_provider_block() {
        let dir = temp_pull_dir("mcp-providers");

        let providers = dgx_spark_provider();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_provider-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // "provider" block must exist (singular key)
        let provider = parsed.get("provider").expect("provider key must exist");
        let spark = provider
            .get("dgx-spark")
            .expect("dgx-spark provider must exist");

        assert_eq!(spark["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(spark["name"], "DGX Spark (HomeLab)");
        assert_eq!(spark["options"]["baseURL"], "http://10.0.10.121/v1");
        assert_eq!(
            spark["models"]["nexus-coder-main"]["name"],
            "Nexus Coder Main"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_empty_providers_omits_provider_key() {
        let dir = temp_pull_dir("mcp-no-providers");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_no-provider-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // "provider" key must NOT exist when providers map is empty
        assert!(
            parsed.get("provider").is_none(),
            "provider key must be absent when no providers"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_trigger_write_on_existing_file() {
        let dir = temp_pull_dir("mcp-provider-trigger");

        // Pre-create opencode.json with dummy content
        fs::write(dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

        let providers = dgx_spark_provider();
        // force=false, no plugins, but providers non-empty -> must still write
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_trigger-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Must have been overwritten (contains provider block now)
        assert!(oc.contains("dgx-spark"));
        assert!(oc.contains("@ai-sdk/openai-compatible"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_not_in_mcp_json() {
        let dir = temp_pull_dir("mcp-provider-no-claude");

        let providers = dgx_spark_provider();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_no-claude-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // mcp.json must NOT contain provider config
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(
            !cm.contains("dgx-spark"),
            "providers must not appear in mcp.json"
        );
        assert!(!cm.contains("@ai-sdk/openai-compatible"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_and_plugins_coexist() {
        let dir = temp_pull_dir("mcp-provider-plugin");

        let providers = dgx_spark_provider();
        let mut plugins = HashMap::new();
        plugins.insert(
            "nexus-plugin".to_string(),
            McpServerConfig {
                command: vec!["npx".into()],
                args: vec!["nexus-plugin".into()],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_coexist-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &plugins,
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // Both MCP servers and providers must coexist
        assert!(parsed.get("mcp").is_some(), "mcp block must exist");
        assert!(
            parsed.get("provider").is_some(),
            "provider block must exist"
        );

        let mcp = parsed.get("mcp").unwrap();
        assert!(mcp.get("nexus-plugin").is_some(), "plugin must be in mcp");

        let provider = parsed.get("provider").unwrap();
        assert!(
            provider.get("dgx-spark").is_some(),
            "dgx-spark must be in provider"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_key_from_env_file_nexus_local() {
        let dir = temp_pull_dir("read-env-nexus-local");
        fs::write(
            dir.join(".env.nexus.local"),
            "# comment\nNEXUS_SEC_OPENAI_API_KEY=sk-test-literal-value\nOTHER=ignore\n",
        )
        .unwrap();
        let result = read_key_from_env_file(&dir, "NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(result, Some("sk-test-literal-value".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_key_from_env_file_quoted_value() {
        let dir = temp_pull_dir("read-env-quoted");
        fs::write(
            dir.join(".env.nexus.local"),
            "NEXUS_SEC_OPENAI_API_KEY=\"sk-quoted-value\"\n",
        )
        .unwrap();
        let result = read_key_from_env_file(&dir, "NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(result, Some("sk-quoted-value".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_key_from_env_file_export_prefix() {
        let dir = temp_pull_dir("read-env-export");
        fs::write(
            dir.join(".env.nexus.local"),
            "export NEXUS_SEC_OPENAI_API_KEY=sk-exported-value\n",
        )
        .unwrap();
        let result = read_key_from_env_file(&dir, "NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(result, Some("sk-exported-value".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_key_from_env_file_missing_key() {
        let dir = temp_pull_dir("read-env-missing-key");
        fs::write(dir.join(".env.nexus.local"), "OTHER_KEY=something\n").unwrap();
        let result = read_key_from_env_file(&dir, "NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(result, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_key_from_env_file_no_file() {
        let dir = temp_pull_dir("read-env-no-file");
        let result = read_key_from_env_file(&dir, "NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(result, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_reads_key_from_env_file() {
        let dir = temp_pull_dir("mcp-env-file-key");

        // Isolate from shell environment: the env-file key must win over any
        // shell-level NEXUS_SEC_OPENAI_API_KEY that devbox may have injected.
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        fs::write(
            dir.join(".env.nexus.local"),
            "NEXUS_SEC_OPENAI_API_KEY=sk-from-env-file\n",
        )
        .unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Literal key from env file must be written — no {env:} template
        assert!(
            oc.contains("sk-from-env-file"),
            "literal key must be present"
        );
        assert!(
            !oc.contains("{env:NEXUS_SEC_OPENAI_API_KEY}"),
            "env template must not be used when file provides the key"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // T5: Plugin server with Vec<String> command → opencode.json "command": [array]
    #[test]
    fn test_write_mcp_configs_plugin_array_command_in_opencode_json() {
        let dir = temp_pull_dir("plugin-array-cmd");
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec![],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // opencode.json uses array command form
        assert!(oc.contains("\"nexus-headroom\""), "plugin block must exist");
        assert!(
            oc.contains("\"headroom\""),
            "first command element must appear"
        );
        assert!(oc.contains("\"mcp\""), "second command element must appear");
        assert!(
            oc.contains("\"serve\""),
            "third command element must appear"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T6: Plugin server with inline environment → opencode.json "environment": {inline vars}
    #[test]
    fn test_write_mcp_configs_plugin_inline_environment_in_opencode_json() {
        let dir = temp_pull_dir("plugin-inline-env");
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        let mut env_map = HashMap::new();
        env_map.insert("HEADROOM_MODE".to_string(), "transform".to_string());
        env_map.insert("HEADROOM_DEBUG".to_string(), "false".to_string());

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec![],
                env_keys: vec![],
                environment: env_map,
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"HEADROOM_MODE\""), "env key must be present");
        assert!(oc.contains("\"transform\""), "env value must be present");
        assert!(oc.contains("\"HEADROOM_DEBUG\""));
        assert!(oc.contains("\"false\""));

        let _ = fs::remove_dir_all(&dir);
    }

    // T7: Plugin server with env_keys → opencode.json "environment": {"{env:KEY}"}
    #[test]
    fn test_write_mcp_configs_plugin_env_keys_template_in_opencode_json() {
        let dir = temp_pull_dir("plugin-env-keys");
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["npx".into(), "task-master-ai".into()],
                args: vec![],
                env_keys: vec!["ANTHROPIC_API_KEY".to_string()],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(
            oc.contains("{env:ANTHROPIC_API_KEY}"),
            "env_key must render as {{env:KEY}} template"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T8: env_keys + inline environment → inline overrides env_keys template
    #[test]
    fn test_write_mcp_configs_inline_env_overrides_env_keys() {
        let dir = temp_pull_dir("plugin-env-override");
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        let mut env_map = HashMap::new();
        // Inline value for a key that is also in env_keys — inline must win
        env_map.insert("HEADROOM_MODE".to_string(), "transform".to_string());

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into()],
                args: vec![],
                env_keys: vec!["HEADROOM_MODE".to_string()],
                environment: env_map,
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Inline value must be present; env template must NOT be present for the same key
        assert!(oc.contains("\"transform\""), "inline value must win");
        assert!(
            !oc.contains("{env:HEADROOM_MODE}"),
            "env template must not appear when inline value is provided"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T9: mcp.json Claude format: command[0] → "command" string, rest → "args" array
    #[test]
    fn test_write_mcp_configs_plugin_mcp_json_claude_format() {
        let dir = temp_pull_dir("plugin-mcp-json");
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec!["--port".into(), "9000".into()],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // mcp.json uses "command": string + "args": array (Claude Code format)
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(
            cm.contains("\"command\": \"headroom\""),
            "mcp.json command must be the first element as a string"
        );
        assert!(cm.contains("\"mcp\""), "mcp subcommand must appear in args");
        assert!(
            cm.contains("\"serve\""),
            "serve subcommand must appear in args"
        );
        assert!(cm.contains("\"--port\""), "extra args must be forwarded");
        assert!(cm.contains("\"9000\""), "extra arg value must be forwarded");

        let _ = fs::remove_dir_all(&dir);
    }

    // -- is_locally_modified tests --

    #[test]
    fn test_is_locally_modified_no_file() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_nofile");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let manifest = serde_json::json!({});
        assert!(!is_locally_modified(&tmp, "nonexistent.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_no_manifest_entry() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_nomanifest");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("devbox.json"), "{}").unwrap();

        let manifest = serde_json::json!({});
        // No manifest entry: cannot determine modification, returns false
        assert!(!is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_unchanged() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_unchanged");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let content = r#"{"packages":{}}"#;
        fs::write(tmp.join("devbox.json"), content).unwrap();

        let hash = sha256_hex(content);
        let manifest = serde_json::json!({
            "devbox.json": { "target_path": "devbox.json", "hash": hash }
        });
        assert!(!is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_changed() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_changed");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Write file with content that differs from manifest hash
        fs::write(tmp.join("devbox.json"), r#"{"packages":{"new":"pkg"}}"#).unwrap();

        let old_hash = sha256_hex(r#"{"packages":{}}"#);
        let manifest = serde_json::json!({
            "devbox.json": { "target_path": "devbox.json", "hash": old_hash }
        });
        assert!(is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_by_target_path_lookup() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_target_path");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fs::write(tmp.join("AGENTS.md"), "# Modified").unwrap();

        // Manifest keyed by file_key, not target_path
        let old_hash = sha256_hex("# Original");
        let manifest = serde_json::json!({
            "agents-md": { "target_path": "AGENTS.md", "hash": old_hash }
        });
        assert!(is_locally_modified(&tmp, "AGENTS.md", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }
}
