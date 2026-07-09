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
use nexus_core::api::McpServerConfig;
use nexus_core::api::NexusClient;
use nexus_core::api::ProviderConfig;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::McpSource;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::init::resolve_platform_plugins;
use super::shadow;

/// Marker in YAML frontmatter indicating the file is managed by Nexus CLI.
/// Files without this marker are considered user-managed and will not be
/// overwritten by `nexus pull`.
const MANAGED_MARKER: &str = "source: nexus-platform";

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
                "   {} .{}/env removed (no plugin env vars)",
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
        "# .{agentic_root}/env — Platform-managed plugin environment variables\n\
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

    fs::create_dir_all(&env_dir)?;
    fs::write(&env_path, &content)?;
    println!(
        "   {} .{}/env ({} plugin var{})",
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

/// Convert days since Unix epoch to (year, month, day). Used for ISO 8601 timestamps
/// in `.nexus/env` without requiring the `chrono` crate.
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
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

    println!(
        "{} Pulling from Nexus platform...",
        style(">>").bold().cyan()
    );
    println!("   Project: {}", style(&project_id).dim());
    println!();

    // Resolve authentication token
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token.clone()))?;

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

    // Detect existing .claude/ files and hint at import (v0.7.0)
    detect_importable_files(&workspace);

    // Export skills (always needed for project_name)
    let export = client.export_skills(&project_id).await?;
    let project_name = export.project.name.clone();

    println!(
        "   {} Project: {} ({})",
        style("+").bold().green(),
        style(&export.project.name).bold(),
        &export.project.slug
    );

    if export.skills.is_empty() {
        println!(
            "   {} No skills assigned to this project.",
            style("--").yellow()
        );
    } else {
        // Check for existing files and prompt if not forced
        let existing = detect_existing_files(&workspace, &export.skills, &agentic_root);
        if !existing.is_empty() && !force {
            println!();
            println!(
                "   {} The following files already exist and will be overwritten:",
                style("!").bold().yellow()
            );
            for path in &existing {
                println!("      {}", style(path).dim());
            }
            println!();
            if !confirm_overwrite()? {
                println!(
                    "   {} Pull cancelled. Use {} to overwrite.",
                    style("--").yellow(),
                    style("--force").bold()
                );
                return Ok(());
            }
        }

        // Ensure directories exist
        fs::create_dir_all(workspace.join(&agentic_root).join("skills"))?;
        fs::create_dir_all(workspace.join(".opencode/commands"))?;

        // Write skills and commands
        let mut written = 0;
        for skill in &export.skills {
            write_skill(&workspace, skill, &agentic_root)?;
            write_command(&workspace, skill, &agentic_root)?;
            written += 1;
        }

        println!(
            "   {} {} skill(s) synced",
            style("+").bold().green(),
            written
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
                for af in &af_export.agent_files {
                    let target_path = workspace.join(&af.target_path);

                    // Hard block: protected files are NEVER overwritten
                    if is_protected_path(&af.target_path) && target_path.exists() {
                        println!(
                            "   {} {} is a protected file, refusing to overwrite (secrets/env files are never touched by pull)",
                            style("!!").bold().red(),
                            af.target_path
                        );
                        continue;
                    }

                    // Check managed-file marker before overwriting
                    if target_path.exists() && !force && !is_managed_file(&target_path) {
                        println!(
                            "   {} {} is user-managed, skipping (use --force to overwrite)",
                            style("--").yellow(),
                            af.target_path
                        );
                        continue;
                    }

                    write_agent_file(&workspace, af)?;
                    af_written_paths.insert(af.target_path.clone());

                    // Update sync manifest with content hash from server
                    if let Some(ref hash) = af.content_hash {
                        let _ = super::sync::update_manifest_after_pull(
                            &workspace,
                            &af.file_key,
                            &af.target_path,
                            hash,
                        );
                    }

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

    // Resolve tool flavor and plugin MCP servers from af_export response
    let tool_flavor = match af_export_result
        .as_ref()
        .ok()
        .and_then(|r| r.agent_owner.clone())
    {
        Some(owner) => Some(owner),
        None => {
            // Fallback: fetch from project details API
            client
                .get_project(&project_id)
                .await
                .ok()
                .and_then(|d| d.project.agent_owner)
        }
    };

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
            if let Ok(content) = serde_json::to_string_pretty(&routes_json) {
                let _ = fs::write(&routes_path, content + "\n");
                println!(
                    "   {} .{}/generated/model-routes.json",
                    style("+").bold().green(),
                    agentic_root
                );
            }
        }
    }

    // Write MCP server configs (creates if missing, merges plugin servers, force-overwrites)
    write_mcp_configs(
        &workspace,
        api_url,
        &effective_token,
        mcp_source,
        tool_flavor.as_deref(),
        &agentic_root,
        &plugin_mcp_servers,
        &providers,
        &opencode_agents,
        &opencode_default_model,
        &opencode_default_agent,
        force,
    )?;

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
        if !af_export.plugins.is_empty() {
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

    // Check prerequisites from af_export response.
    // Warn for each required tool that is not found in PATH.
    // This is non-blocking — pull succeeds regardless, but the user
    // needs the tool for the plugin to actually work.
    if let Some(Ok(ref af_export)) = Some(&af_export_result) {
        if !af_export.prerequisites.is_empty() {
            let mut missing: Vec<String> = Vec::new();
            for prereq in &af_export.prerequisites {
                let found = std::process::Command::new("sh")
                    .args(["-c", &prereq.check_command])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
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

                // Write devbox.json
                let target = workspace.join("devbox.json");
                if target.exists() && !force && !is_managed_file(&target) {
                    println!(
                        "   {} devbox.json is user-managed, skipping",
                        style("--").yellow(),
                    );
                } else {
                    fs::write(&target, &export.devbox_json)?;
                    ws_written += 1;
                }

                // Write scripts
                for script in &export.scripts {
                    let target = workspace.join(&script.path);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }

                    if target.exists() && !force && !is_managed_file(&target) {
                        println!(
                            "   {} {} is user-managed, skipping",
                            style("--").yellow(),
                            script.path
                        );
                        continue;
                    }

                    fs::write(&target, &script.body)?;

                    #[cfg(unix)]
                    if script.executable {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(0o755);
                        fs::set_permissions(&target, perms)?;
                    }

                    ws_written += 1;
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

                        // Write composed workspace template (devbox.json)
                        if let Some(ref tpl) = ws_export.workspace {
                            let target = workspace.join(&tpl.target_path);
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)?;
                            }

                            if target.exists() && !force && !is_managed_file(&target) {
                                println!(
                                    "   {} {} is user-managed, skipping",
                                    style("--").yellow(),
                                    tpl.target_path
                                );
                            } else {
                                fs::write(&target, &tpl.body)?;
                                ws_written += 1;
                            }
                        }

                        // Write scripts
                        let _scripts_base = if ws_export.scripts_path.is_empty() {
                            ".nexus/scripts/devbox".to_string()
                        } else {
                            ws_export.scripts_path.clone()
                        };

                        for script in &ws_export.scripts {
                            let target = workspace.join(&script.target_path);
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)?;
                            }

                            if target.exists() && !force && !is_managed_file(&target) {
                                println!(
                                    "   {} {} is user-managed, skipping",
                                    style("--").yellow(),
                                    script.target_path
                                );
                                continue;
                            }

                            fs::write(&target, &script.body)?;

                            // Make executable on Unix
                            #[cfg(unix)]
                            if script.executable {
                                use std::os::unix::fs::PermissionsExt;
                                let perms = std::fs::Permissions::from_mode(0o755);
                                fs::set_permissions(&target, perms)?;
                            }

                            ws_written += 1;
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

    println!();
    println!("{} Pull complete.", style("OK").bold().green());

    Ok(())
}

// ---------------------------------------------------------------------------
// Managed file sync (CLAUDE.md, AGENTS.md)
// ---------------------------------------------------------------------------

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
/// from ADR-0024. Since `.nexus` is now the exclusive agentic root (ADR-0027),
/// any `.claude/` content is by definition customer-owned and importable.
pub fn detect_importable_files(workspace: &Path) {
    let claude_dir = workspace.join(".claude");
    if !claude_dir.is_dir() {
        return;
    }

    let mut found: Vec<String> = Vec::new();

    // Check known agentic files
    for name in &["CLAUDE.md", "settings.json", "mcp.json", "commands.md"] {
        if claude_dir.join(name).exists() {
            found.push(format!(".claude/{}", name));
        }
    }

    // Check root-level AGENTS.md and CLAUDE.md
    if workspace.join("AGENTS.md").exists() {
        found.push("AGENTS.md".to_string());
    }
    if workspace.join("CLAUDE.md").exists() {
        found.push("CLAUDE.md".to_string());
    }

    // Check for skills
    let skills_dir = claude_dir.join("skills");
    if skills_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                if entry.path().join("SKILL.md").exists() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    found.push(format!(".claude/skills/{}/", name));
                }
            }
        }
    }

    if found.is_empty() {
        return;
    }

    println!();
    println!(
        "   {} Existing .claude/ files detected:",
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

    // Path traversal protection: reject target_path with parent-dir components
    let normalized = std::path::Path::new(&af.target_path);
    for component in normalized.components() {
        if matches!(component, std::path::Component::ParentDir) {
            anyhow::bail!(
                "refusing to write: target_path '{}' contains '..' traversal",
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

/// Detect existing skill/command files that would be overwritten.
fn detect_existing_files(
    workspace: &Path,
    skills: &[nexus_core::api::ExportedSkill],
    agentic_root: &str,
) -> Vec<String> {
    let mut existing = Vec::new();
    for skill in skills {
        let skill_path = workspace
            .join(agentic_root)
            .join("skills")
            .join(&skill.skill_id)
            .join("SKILL.md");
        if skill_path.exists() {
            existing.push(format!(
                "{}/skills/{}/SKILL.md",
                agentic_root, skill.skill_id
            ));
        }
        if let Some(ref slug) = skill.command_slug {
            if !slug.is_empty() {
                let cmd_path = workspace
                    .join(".opencode/commands")
                    .join(format!("{}.md", slug));
                if cmd_path.exists() {
                    existing.push(format!(".opencode/commands/{}.md", slug));
                }
            }
        }
    }
    existing
}

/// Ask the user for overwrite confirmation.
fn confirm_overwrite() -> anyhow::Result<bool> {
    print!("   {} Overwrite? [y/N] ", style("?").bold().cyan());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();

    Ok(answer == "y" || answer == "yes")
}

// ---------------------------------------------------------------------------
// MCP config generation
// ---------------------------------------------------------------------------

/// Write MCP server configs (`opencode.json`, `<agentic_root>/mcp.json`).
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
    mcp_source: McpSource,
    tool_flavor: Option<&str>,
    agentic_root: &str,
    plugin_mcp_servers: &HashMap<String, McpServerConfig>,
    providers: &HashMap<String, ProviderConfig>,
    opencode_agents: &Option<serde_json::Value>,
    opencode_default_model: &Option<String>,
    opencode_default_agent: &Option<String>,
    force: bool,
) -> anyhow::Result<()> {
    let opencode_path = workspace.join("opencode.json");
    let claude_mcp_path = workspace.join(agentic_root).join("mcp.json");

    let skip_opencode = matches!(tool_flavor, Some("claude-cli"));
    let skip_claude = matches!(tool_flavor, Some("opencode"));

    let source_label = match mcp_source {
        McpSource::Npm => "npm (@gwdn/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    // ── opencode.json ──────────────────────────────────────────────────────
    if !skip_opencode {
        let exists = opencode_path.exists();
        let needs_write = !exists
            || force
            || !plugin_mcp_servers.is_empty()
            || !providers.is_empty()
            || opencode_agents.is_some();

        if needs_write {
            let mut mcp_block: serde_json::Map<String, serde_json::Value> = if exists && !force {
                // Additive merge: load existing config
                let content = fs::read_to_string(&opencode_path).unwrap_or_default();
                let parsed: serde_json::Value = serde_json::from_str(&content)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                parsed
                    .get("mcp")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default()
            } else {
                serde_json::Map::new()
            };

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
                        "NEXUS_SEC_OPENAI_API_KEY": openai_key_value
                    }
                }),
            );

            // Plugin servers from platform
            for (name, cfg) in plugin_mcp_servers {
                // Build env object: map env_keys to OpenCode template variables {env:KEY}
                // so secrets are resolved at runtime, never persisted to disk.
                let env: serde_json::Map<String, serde_json::Value> = cfg
                    .env_keys
                    .iter()
                    .map(|k: &String| {
                        let template = format!("{{env:{}}}", k);
                        (k.clone(), serde_json::Value::String(template))
                    })
                    .collect();

                mcp_block.insert(
                    name.to_string(),
                    serde_json::json!({
                        "type": "local",
                        "command": std::iter::once(cfg.command.clone())
                            .chain(cfg.args.iter().cloned())
                            .collect::<Vec<_>>(),
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

            fs::write(
                &opencode_path,
                serde_json::to_string_pretty(&opencode_json)? + "\n",
            )?;

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

    // ── <agentic_root>/mcp.json ────────────────────────────────────────────
    if !skip_claude {
        let exists = claude_mcp_path.exists();
        let needs_write = !exists || force || !plugin_mcp_servers.is_empty();

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

            // Plugin servers
            for (name, cfg) in plugin_mcp_servers {
                // Build env object: map env_keys to shell-style template variables ${KEY}
                // so secrets are resolved at runtime, never persisted to disk.
                let env: serde_json::Map<String, serde_json::Value> = cfg
                    .env_keys
                    .iter()
                    .map(|k: &String| {
                        let template = format!("${{{}}}", k);
                        (k.clone(), serde_json::Value::String(template))
                    })
                    .collect();

                servers_block.insert(
                    name.to_string(),
                    serde_json::json!({
                        "command": cfg.command,
                        "args": cfg.args,
                        "env": env
                    }),
                );
            }

            let claude_mcp_json = serde_json::json!({
                "mcpServers": servers_block
            });

            if let Some(parent) = claude_mcp_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &claude_mcp_path,
                serde_json::to_string_pretty(&claude_mcp_json)? + "\n",
            )?;

            let verb = if exists { "updated" } else { "created" };
            println!(
                "   {} {}/mcp.json {} (MCP source: {}{})",
                style("+").bold().green(),
                agentic_root,
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

    Ok(())
}

// ---------------------------------------------------------------------------
// File writers
// ---------------------------------------------------------------------------

/// Write a skill definition to `<agentic_root>/skills/<skill_id>/SKILL.md`.
fn write_skill(
    target: &Path,
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> anyhow::Result<()> {
    let skill_dir = target
        .join(agentic_root)
        .join("skills")
        .join(&skill.skill_id);
    fs::create_dir_all(&skill_dir)?;

    let body = skill
        .body
        .as_deref()
        .unwrap_or("<!-- No skill body defined -->");

    let content = format!(
        r#"---
skill_id: {skill_id}
name: {name}
version: {version}
command_slug: {command_slug}
source: nexus-platform
---

{body}
"#,
        skill_id = skill.skill_id,
        name = skill.name,
        version = skill.version,
        command_slug = skill.command_slug.as_deref().unwrap_or("none"),
        body = body,
    );

    fs::write(skill_dir.join("SKILL.md"), content)?;
    print_synced(&format!(
        "{}/skills/{}/SKILL.md",
        agentic_root, skill.skill_id
    ));

    // Write resource files alongside SKILL.md
    for res in &skill.resources {
        // Sanitise filename: prevent directory traversal
        let filename = res.filename.replace(['/', '\\'], "_");
        if filename.is_empty() || filename == "SKILL.md" {
            continue;
        }
        fs::write(skill_dir.join(&filename), &res.body)?;
        print_synced(&format!(
            "{}/skills/{}/{}",
            agentic_root, skill.skill_id, filename
        ));
    }

    Ok(())
}

/// Write an OpenCode command for a skill to `.opencode/commands/<slug>.md`.
fn write_command(
    target: &Path,
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> anyhow::Result<()> {
    let slug = match skill.command_slug.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };

    let commands_dir = target.join(".opencode").join("commands");
    fs::create_dir_all(&commands_dir)?;

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

    fs::write(commands_dir.join(format!("{}.md", slug)), content)?;
    print_synced(&format!(".opencode/commands/{}.md", slug));

    Ok(())
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
    fs::write(&path, content)?;
    print_synced(&format!("{}/directives.md", agentic_root));

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
    use nexus_core::api::ExportedDirective;

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
    fn test_detect_existing_files_empty_workspace() {
        let tmp = std::env::temp_dir().join("nexus_test_detect_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let skills = vec![nexus_core::api::ExportedSkill {
            skill_id: "nx-test".into(),
            name: "Test".into(),
            description: None,
            version: 1,
            body: None,
            command_slug: Some("test-cmd".into()),
            pinned: false,
            resources: vec![],
        }];

        let existing = detect_existing_files(&tmp, &skills, ".claude");
        assert!(existing.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_existing_files_with_existing() {
        let tmp = std::env::temp_dir().join("nexus_test_detect_existing");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude/skills/nx-test")).unwrap();
        fs::write(tmp.join(".claude/skills/nx-test/SKILL.md"), "old").unwrap();
        fs::create_dir_all(tmp.join(".opencode/commands")).unwrap();
        fs::write(tmp.join(".opencode/commands/test-cmd.md"), "old").unwrap();

        let skills = vec![nexus_core::api::ExportedSkill {
            skill_id: "nx-test".into(),
            name: "Test".into(),
            description: None,
            version: 1,
            body: None,
            command_slug: Some("test-cmd".into()),
            pinned: false,
            resources: vec![],
        }];

        let existing = detect_existing_files(&tmp, &skills, ".claude");
        assert_eq!(existing.len(), 2);
        assert!(existing.contains(&".claude/skills/nx-test/SKILL.md".to_string()));
        assert!(existing.contains(&".opencode/commands/test-cmd.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
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
    fn test_detect_existing_files_alternate_root() {
        let tmp = std::env::temp_dir().join("nexus_test_detect_alt_root");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".nexus/skills/nx-test")).unwrap();
        fs::write(tmp.join(".nexus/skills/nx-test/SKILL.md"), "old").unwrap();

        let skills = vec![nexus_core::api::ExportedSkill {
            skill_id: "nx-test".into(),
            name: "Test".into(),
            description: None,
            version: 1,
            body: None,
            command_slug: None,
            pinned: false,
            resources: vec![],
        }];

        let existing = detect_existing_files(&tmp, &skills, ".nexus");
        assert_eq!(existing.len(), 1);
        assert!(existing.contains(&".nexus/skills/nx-test/SKILL.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
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
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json should still be at root
        assert!(dir.join("opencode.json").exists());
        // mcp.json should be under .nexus/, not .claude/
        assert!(dir.join(".nexus/mcp.json").exists());
        assert!(!dir.join(".claude/mcp.json").exists());

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

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
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

        // .claude/mcp.json must exist
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
        assert!(cm.contains("\"mcpServers\""));
        assert!(cm.contains("nxs_pat_pull-test-token"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_skips_existing() {
        let dir = temp_pull_dir("mcp-skips");

        // Pre-create both files
        fs::write(dir.join("opencode.json"), "existing-oc").unwrap();
        fs::write(dir.join(".claude/mcp.json"), "existing-cm").unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_should-not-appear",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
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
            fs::read_to_string(dir.join(".claude/mcp.json")).unwrap(),
            "existing-cm"
        );

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
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
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
        // .claude/mcp.json created
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
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
            McpSource::Local,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("tools/nexus-mcp/dist/server.js"));
        assert!(!oc.contains("npx"));

        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
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
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
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
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
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
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
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
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // mcp.json must NOT contain provider config
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
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
                command: "npx".into(),
                args: vec!["nexus-plugin".into()],
                env_keys: vec![],
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_coexist-token",
            McpSource::Npm,
            None,
            ".claude",
            &plugins,
            &providers,
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
        fs::write(
            dir.join(".env.nexus.local"),
            "NEXUS_SEC_OPENAI_API_KEY=sk-from-env-file\n",
        )
        .unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
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
}
