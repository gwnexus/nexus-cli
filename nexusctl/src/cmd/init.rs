//! The `nexus init` command.
//!
//! Creates the standard Nexus project workspace structure:
//!
//! ```text
//! <project>/
//! +-- .nexus/
//! |   +-- config.toml          # project-local Nexus config
//! +-- .claude/
//! |   +-- CLAUDE.md            # Claude agent instructions (git-ignored)
//! |   +-- skills/              # Claude skill definitions (pulled from server)
//! +-- .opencode/
//! |   +-- commands/            # OpenCode command definitions (from skills)
//! +-- opencode.json            # OpenCode MCP server configuration (git-ignored)
//! +-- AGENTS.md                # Agent role definitions
//! ```
//!
//! Both `opencode.json` and `.claude/CLAUDE.md` are user-managed files that
//! must NOT be committed to the repository. They are added to `.git/info/exclude`
//! by this command and are only created if they do not already exist.
//!
//! When `--project-id` is provided and a valid token exists, the init command
//! becomes **server-aware**: it verifies the identity, exports skills from the
//! Nexus platform, and materializes them into the local workspace.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::config::{ExtraMcpServer, PluginDef};
use nexus_core::McpSource;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cmd::pull::{detect_importable_files, write_plugin_env_file};

/// Run the init command.
pub async fn run(
    path: &str,
    name: Option<&str>,
    project_id: Option<&str>,
    api_url: &str,
    force: bool,
    mcp_source: McpSource,
) -> anyhow::Result<()> {
    let target = PathBuf::from(path).canonicalize().unwrap_or_else(|_| {
        // Path doesn't exist yet; resolve relative to cwd
        std::env::current_dir().unwrap_or_default().join(path)
    });

    let project_name = name.unwrap_or_else(|| {
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("nexus-project")
    });

    println!(
        "{} Initializing Nexus workspace: {}",
        style(">>").bold().cyan(),
        style(project_name).bold()
    );
    println!("   Target: {}", target.display());

    // Resolve project ID from CLI flag or linked project config
    let resolved_pid = config::resolve_project_id(project_id, Some(&target)).ok();

    if let Some(ref pid) = resolved_pid {
        println!("   Project: {}", style(pid).dim());
    } else if !force {
        println!(
            "   {} No project linked to this workspace.",
            style("!").bold().yellow()
        );
        println!();
        println!(
            "   Without a linked project, {} will only create the local",
            style("nexus init").bold()
        );
        println!("   scaffold. Project-specific skills, agent files, directives,");
        println!("   and MCP configuration will not be pulled.");
        println!();
        println!(
            "   After init, run {} to bind a project, then {}.",
            style("nexus link").bold().cyan(),
            style("nexus pull").bold().cyan()
        );
        println!();

        // Only prompt if stdin is a TTY (skip in tests / pipes)
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            print!("   Continue without a project? [y/N] ");
            use std::io::{self, BufRead, Write};
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let answer = line.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("   Aborted.");
                return Ok(());
            }
            println!();
        }
    }
    println!();

    // Check if already fully initialized (skip if .nexus/ only contains config.toml from `nexus link`)
    let nexus_dir = target.join(".nexus");
    if nexus_dir.exists() && !force {
        let is_link_only = is_nexus_dir_link_only(&nexus_dir);
        if !is_link_only {
            anyhow::bail!("Directory already contains .nexus/. Use --force to reinitialize.");
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1: Local scaffolding (server-agnostic — only .nexus/ + .opencode/)
    // -----------------------------------------------------------------------
    // NOTE: agentic root directory (.claude/ or .nexus/) is NOT created here.
    // It is deferred to Phase 2 where the server-configured agentic_root is
    // known. This prevents writing into .claude/ when the project is configured
    // to use .nexus/ as its agentic root.

    // Warn if npx is not available (needed for MCP server)
    if std::process::Command::new("npx")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        println!(
            "   {} npx not found -- MCP server requires Node.js / npm",
            style("!").bold().yellow()
        );
    }

    create_nexus_dir(&target, project_name, resolved_pid.as_deref())?;
    create_opencode_dir(&target)?;

    // Ensure a persistent machine identity exists
    match nexus_core::machine::MachineIdentity::load_or_create() {
        Ok(identity) => {
            println!(
                "   {} Machine ID: {}",
                style("+").bold().green(),
                style(identity.id()).dim()
            );
        }
        Err(e) => {
            println!(
                "   {} Could not create machine ID: {}",
                style("!").yellow(),
                e
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: Server-aware init (when project_id + token are available)
    // -----------------------------------------------------------------------
    let mut server_aware = false;
    if let Some(ref pid) = resolved_pid {
        let token = resolve_token();
        if let Some(ref tok) = token {
            println!(
                "{} Connecting to Nexus platform...",
                style(">>").bold().cyan()
            );

            let client = NexusClient::new(api_url, Some(tok.clone()))?;

            // Verify identity
            match client.get_identity().await {
                Ok(identity) => {
                    println!(
                        "   {} Authenticated as {} ({})",
                        style("+").bold().green(),
                        style(&identity.email).bold(),
                        if identity.is_platform_owner {
                            "platform_owner"
                        } else if identity.is_platform_admin {
                            "platform_admin"
                        } else {
                            "member"
                        }
                    );
                }
                Err(e) => {
                    println!(
                        "   {} Identity check failed: {}",
                        style("!").bold().yellow(),
                        e
                    );
                    println!("   Skipping server-aware setup. Run 'nexus login' first.");

                    // Fall back to default .nexus/ scaffold since we cannot
                    // determine the server-configured agentic_root.
                    let default_agentic_root = ".nexus";
                    create_claude_dir(&target, project_name, default_agentic_root)?;
                    create_agents_md(&target, project_name, force, default_agentic_root)?;
                    append_gitignore(&target)?;

                    print_done(false);
                    return Ok(());
                }
            }

            // Fetch project detail and agent file export for agentic_root.
            // get_project provides agentic_root directly; export_agent_files
            // is used as fallback (it also returns agentic_root on the envelope).
            let project_detail = client.get_project(pid).await.ok();
            let af_export_result = client.export_agent_files(pid).await;

            let tool_flavor = project_detail
                .as_ref()
                .and_then(|d| d.project.agent_owner.clone());
            let agentic_root = project_detail
                .as_ref()
                .and_then(|d| d.project.agentic_root.clone())
                .or_else(|| {
                    af_export_result
                        .as_ref()
                        .ok()
                        .map(|af| af.agentic_root.clone())
                        .filter(|r| !r.is_empty())
                })
                .unwrap_or_else(|| ".nexus".to_string());

            // Detect existing .claude/ files and hint at import (v0.7.0)
            detect_importable_files(&target);

            // Create the agentic root directory (.claude/ or .nexus/ etc.)
            create_claude_dir(&target, project_name, &agentic_root)?;
            create_agents_md(&target, project_name, force, &agentic_root)?;
            append_gitignore(&target)?;

            match client.export_skills(pid).await {
                Ok(export) => {
                    println!(
                        "   {} Project: {} ({})",
                        style("+").bold().green(),
                        style(&export.project.name).bold(),
                        export.project.slug
                    );
                    println!(
                        "   {} {} skill(s) exported",
                        style("+").bold().green(),
                        export.count
                    );

                    // Materialize skills
                    for skill in &export.skills {
                        write_skill(&target, skill, &agentic_root)?;
                        write_command(&target, skill, &agentic_root)?;
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Skill export failed: {}",
                        style("!").bold().yellow(),
                        e
                    );
                    println!("   Local scaffolding is complete; skills not synced.");
                }
            }

            // Write MCP server configs (independent of skill export)
            // Skip the interactive URL prompt when the global config file
            // already exists (i.e. the user ran `nexus auth login` at least
            // once) — the resolved api_url is already correct.
            let has_configured_url = config::Config::path().map(|p| p.exists()).unwrap_or(false);
            let mcp_api_url = if force || has_configured_url {
                api_url.to_string()
            } else {
                prompt_with_default("   Nexus API URL", api_url)?
            };
            write_mcp_configs(
                &target,
                project_name,
                &mcp_api_url,
                tok,
                mcp_source,
                tool_flavor.as_deref(),
                &agentic_root,
            )?;

            // Write .nexus/env from af_export.plugin_env (platform-managed, full overwrite)
            {
                let plugin_env = af_export_result
                    .as_ref()
                    .ok()
                    .map(|r| r.plugin_env.clone())
                    .unwrap_or_default();
                write_plugin_env_file(&target, &plugin_env, project_name, &agentic_root)?;
            }

            // Apply extra MCP servers and plugins from .nexus/config.toml
            if let Ok(Some(proj_config)) = config::load_project_config(Some(&target)) {
                if let Some(ref extras) = proj_config.mcp_extra {
                    merge_extra_mcp_servers(&target, extras, &agentic_root)?;
                }
                if let Some(ref plugins) = proj_config.plugins {
                    install_plugins(&target, plugins).await?;
                }
            }

            // Install platform-selected plugins from af_export plugins list.
            // Uses the built-in plugin registry to resolve download URLs for
            // known Nexus plugins (nexus-compaction-plus, nexus-cost-control).
            if let Ok(ref af_export) = af_export_result {
                if !af_export.plugins.is_empty() {
                    let platform_plugins = resolve_platform_plugins(&af_export.plugins);
                    if !platform_plugins.is_empty() {
                        install_plugins(&target, &platform_plugins).await?;
                    }
                }
            }

            // Export directives for this project
            match client.export_directives(pid).await {
                Ok(dir_export) => {
                    if dir_export.directives.is_empty() {
                        println!(
                            "   {} No directives for this project.",
                            style("--").yellow()
                        );
                    } else {
                        write_directives(&target, &dir_export.directives, &agentic_root)?;
                        println!(
                            "   {} {} directive(s) synced",
                            style("+").bold().green(),
                            dir_export.directives.len()
                        );
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Directive export failed: {}",
                        style("!").bold().yellow(),
                        e
                    );
                }
            }

            // Export agent files (AGENTS.md, CLAUDE.md, etc.) from platform
            // Overwrites the Phase 1 local templates with server-managed content.
            // Re-uses the already-fetched af_export_result from above.
            match af_export_result {
                Ok(ref af_export) => {
                    if !af_export.agent_files.is_empty() {
                        for af in &af_export.agent_files {
                            let written = write_agent_file(&target, af)?;
                            if !written {
                                continue;
                            }

                            // Update sync manifest with content hash from server
                            if let Some(ref hash) = af.content_hash {
                                let _ = super::sync::update_manifest_after_pull(
                                    &target,
                                    &af.file_key,
                                    &af.target_path,
                                    hash,
                                );
                            }
                        }
                        println!(
                            "   {} {} agent file(s) synced",
                            style("+").bold().green(),
                            af_export.agent_files.len()
                        );
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Agent file export not available ({}), using local templates",
                        style("!").bold().yellow(),
                        e
                    );
                }
            }

            // Export workspace files (devbox.json + scripts) — same as `nexus pull`
            match client.export_workspace_mcp(pid).await {
                Ok(export) => {
                    let mut ws_written = 0;

                    // Write devbox.json
                    let devbox_target = target.join("devbox.json");
                    fs::write(&devbox_target, &export.devbox_json)?;
                    ws_written += 1;

                    // Write scripts
                    for script in &export.scripts {
                        let script_target = target.join(&script.path);
                        if let Some(parent) = script_target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&script_target, &script.body)?;

                        #[cfg(unix)]
                        if script.executable {
                            use std::os::unix::fs::PermissionsExt;
                            let perms = std::fs::Permissions::from_mode(0o755);
                            fs::set_permissions(&script_target, perms)?;
                        }

                        ws_written += 1;
                    }

                    if ws_written > 0 {
                        println!(
                            "   {} {} workspace file(s) synced",
                            style("+").bold().green(),
                            ws_written
                        );
                    }
                }
                Err(e) => {
                    let msg = format!("{}", e);
                    if msg.contains("No active workspace fork") {
                        // No workspace configured — surface an advisory prompt so the user
                        // knowingly proceeds without devbox integration.
                        println!(
                            "   {} No workspace (devbox integration) is configured for this project.",
                            style("!").bold().yellow()
                        );
                        println!();
                        println!(
                            "   It is unusual to initialize a NEXUS project without a workspace."
                        );
                        println!(
                            "   A workspace provides the devbox.json environment definition and"
                        );
                        println!("   project scripts required for consistent local development.");
                        println!();
                        println!("   To add a workspace later:");
                        println!(
                            "     1. Open the project settings in the Nexus backend and add a workspace."
                        );
                        println!(
                            "     2. Then run:  {}",
                            style("nexus pull --force").bold().cyan()
                        );
                        println!(
                            "        — or — re-run {} in this directory.",
                            style("nexus init").bold().cyan()
                        );
                        println!();

                        // Prompt on TTY; skip (continue) in CI / pipes.
                        use std::io::IsTerminal;
                        if std::io::stdin().is_terminal() && !force {
                            print!(
                                "   {} ",
                                style("Understood — continue without workspace? [y/N]").dim()
                            );
                            use std::io::{self, BufRead, Write};
                            io::stdout().flush()?;
                            let mut line = String::new();
                            io::stdin().lock().read_line(&mut line)?;
                            let answer = line.trim().to_lowercase();
                            if answer != "y" && answer != "yes" {
                                println!("   Aborted.");
                                return Ok(());
                            }
                            println!();
                        }
                    } else {
                        println!(
                            "   {} Workspace export not available: {}",
                            style("!").bold().yellow(),
                            e
                        );
                    }
                }
            }

            // Export open tasks as TASKS.md
            match client
                .list_tasks(pid, Some(&["open", "in_progress", "blocked"]))
                .await
            {
                Ok(task_response) => {
                    if !task_response.tasks.is_empty() {
                        super::pull::write_tasks(&target, &task_response.tasks, &agentic_root)?;
                        println!(
                            "   {} {} task(s) synced to {}/TASKS.md",
                            style("+").bold().green(),
                            task_response.tasks.len(),
                            agentic_root,
                        );
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Could not fetch tasks: {}",
                        style("!").bold().yellow(),
                        e
                    );
                }
            }

            // Show tool flavor and next steps
            {
                let flavor = tool_flavor.as_deref().unwrap_or("opencode");
                let flavor_label = match flavor {
                    "both" => "OpenCode + Claude CLI",
                    "claude-cli" => "Claude CLI",
                    _ => "OpenCode",
                };
                println!();
                println!(
                    "{} Nexus workspace initialized successfully.",
                    style("OK").bold().green()
                );

                // Auto-apply git identity if configured
                if let Some(ref git_cfg) = project_detail
                    .as_ref()
                    .and_then(|d| d.project.git_config.clone())
                {
                    if git_cfg.user_name.is_some() || git_cfg.user_email.is_some() {
                        match super::git::apply_git_config(&target, git_cfg) {
                            Ok(n) if n > 0 => println!(
                                "   {} Applied {} git identity setting(s) from project config.",
                                style("GIT").bold().blue(),
                                n
                            ),
                            _ => {}
                        }
                    }
                }

                println!(
                    "   This project is optimized for: {}",
                    style(flavor_label).bold().cyan()
                );
                println!();
                println!("Next steps:");
                match flavor {
                    "claude-cli" => {
                        println!("  1. Start Claude CLI in this directory");
                    }
                    "opencode" => {
                        println!("  1. Start OpenCode in this directory");
                    }
                    _ => {
                        println!("  1. Start OpenCode or Claude CLI in this directory");
                    }
                }
                println!(
                    "  2. Run {} to bootstrap the agent",
                    style("/nexus-init").bold()
                );
                println!(
                    "  3. Skills are in {}, commands/configs in {}",
                    style(".nexus/skills/").bold(),
                    style(".opencode/").bold()
                );
                println!(
                    "  4. Run {} periodically to sync from the platform",
                    style("nexus pull").bold()
                );
                println!(
                    "  5. Use {} commands in your agent for Nexus operations",
                    style("/nexus-*").bold()
                );
                println!();
                println!(
                    "   {} Local changes to agent files (AGENTS.md, CLAUDE.md, etc.)",
                    style("!").bold().yellow()
                );
                println!("     will be overwritten by 'nexus pull'. These files are managed");
                println!("     by the Nexus platform and cannot be pushed back yet.");

                // Hint: use `nexus run` for env-var injection
                crate::cmd::pull::print_nexus_run_hint(&target);

                server_aware = true;
            }
        } else {
            // No token — fall back to default .nexus/ scaffold
            let default_agentic_root = ".nexus";
            detect_importable_files(&target);
            create_claude_dir(&target, project_name, default_agentic_root)?;
            create_agents_md(&target, project_name, force, default_agentic_root)?;
            append_gitignore(&target)?;
            println!(
                "   {} No token found. Skipping server sync.",
                style("--").yellow()
            );
            println!(
                "   Run 'nexus login' and then 'nexus init --force --project-id {}' again.",
                pid
            );
        }
    } else {
        // No project linked — create default .nexus/ scaffold so the
        // workspace is immediately usable with coding agents.
        let default_agentic_root = ".nexus";
        detect_importable_files(&target);
        create_claude_dir(&target, project_name, default_agentic_root)?;
        create_agents_md(&target, project_name, force, default_agentic_root)?;
        append_gitignore(&target)?;
    }

    print_done(server_aware);
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1 helpers: Local scaffolding
// ---------------------------------------------------------------------------

/// Create .nexus/ directory with project-local config.
///
/// If `config.toml` already exists (e.g. preserved from deinit), it is NOT
/// overwritten — user-configured `[mcp_extra]` and `[plugins]` sections
/// would be lost. Only updates `[project]` section if needed.
fn create_nexus_dir(
    target: &Path,
    project_name: &str,
    project_id: Option<&str>,
) -> anyhow::Result<()> {
    let nexus_dir = target.join(".nexus");
    fs::create_dir_all(&nexus_dir)?;

    let config_path = nexus_dir.join("config.toml");
    if config_path.exists() {
        // Config exists — preserve it but ensure [project] section is current
        if let Some(id) = project_id {
            if let Ok(Some(mut existing)) = config::load_project_config(Some(target)) {
                let needs_update = existing
                    .project
                    .as_ref()
                    .map(|p| p.id != id || p.name != project_name)
                    .unwrap_or(true);

                if needs_update {
                    existing.project = Some(config::ProjectInfo {
                        id: id.to_string(),
                        name: project_name.to_string(),
                        slug: String::new(),
                    });
                    config::save_project_config(Some(target), &existing)?;
                }
            }
        }
        println!(
            "   {} .nexus/config.toml already exists, preserved",
            style("--").yellow()
        );
        return Ok(());
    }

    let project_section = match project_id {
        Some(id) => format!(
            r#"[project]
name = "{name}"
id = "{id}"
"#,
            name = project_name,
            id = id,
        ),
        None => format!(
            r#"# Project not yet linked. Run `nexus link` or `nexus init --project-id <UUID>`.
# [project]
# name = "{name}"
# id = "<project-uuid>"
"#,
            name = project_name,
        ),
    };

    let config_content = format!(
        r#"# Nexus project-local configuration
# Managed by `nexus init`. Safe to edit manually.

{project_section}
[mcp]
# MCP server binary (resolved relative to project root)
# server_cmd = "node"
# server_args = ["tools/nexus-mcp/dist/server.js"]
"#,
        project_section = project_section,
    );

    fs::write(nexus_dir.join("config.toml"), config_content)?;
    print_created(".nexus/config.toml");

    Ok(())
}

/// Create .claude/ directory with instruction template and skills folder.
///
/// CLAUDE.md is only written if it does not already exist (it is user-managed
/// and excluded from version control via .git/info/exclude).
fn create_claude_dir(target: &Path, project_name: &str, agentic_root: &str) -> anyhow::Result<()> {
    let claude_dir = target.join(agentic_root);
    fs::create_dir_all(claude_dir.join("skills"))?;

    let claude_md_path = claude_dir.join("CLAUDE.md");
    if claude_md_path.exists() {
        println!(
            "   {} {}/CLAUDE.md already exists, skipping",
            style("--").yellow(),
            agentic_root
        );
    } else {
        let claude_md = format!(
            r#"---
type: bootstrap
scope: repo
project: {name}
status: active
---

# BOOTSTRAP SEQUENCE

1. Load agent identity from `AGENTS.md`
2. Connect to the Nexus MCP server
3. Load the project index from the Nexus platform
4. Review active planning and ADR context
5. Continue with the active workstream

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
        );

        fs::write(&claude_md_path, claude_md)?;
        print_created(&format!("{}/CLAUDE.md", agentic_root));
    }

    print_created(&format!("{}/skills/", agentic_root));

    Ok(())
}

/// Create .opencode/ directory with commands subfolder.
fn create_opencode_dir(target: &Path) -> anyhow::Result<()> {
    let opencode_dir = target.join(".opencode");
    fs::create_dir_all(opencode_dir.join("commands"))?;
    print_created(".opencode/commands/");
    Ok(())
}

/// Check if `.nexus/` only contains a `config.toml` (created by `nexus link`).
/// Returns true when the directory is a "link-only" state and can be safely
/// re-initialized without `--force`.
fn is_nexus_dir_link_only(nexus_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(nexus_dir) else {
        return false;
    };
    let files: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    files.len() == 1 && files[0].file_name().to_str() == Some("config.toml")
}

/// Create AGENTS.md with a template agent definition.
fn create_agents_md(
    target: &Path,
    project_name: &str,
    force: bool,
    agentic_root: &str,
) -> anyhow::Result<()> {
    // When using alternate agentic root, AGENTS.md goes inside that directory
    let agents_path = if agentic_root != ".claude" {
        target.join(agentic_root).join("AGENTS.md")
    } else {
        target.join("AGENTS.md")
    };
    if agents_path.exists() && !force {
        println!(
            "   {} AGENTS.md already exists, skipping",
            style("--").yellow()
        );
        return Ok(());
    }

    let agents_md = format!(
        r#"---
type: agent-policy
scope: repo
project: {name}
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
    );

    fs::write(&agents_path, agents_md)?;
    let label = if agentic_root != ".claude" {
        format!("{}/AGENTS.md", agentic_root)
    } else {
        "AGENTS.md".to_string()
    };
    print_created(&label);

    Ok(())
}

/// Append Nexus-specific entries to .git/info/exclude if not already present.
///
/// Uses `.git/info/exclude` (local, never committed) instead of `.gitignore`
/// to avoid polluting the repository with tool-specific ignore rules.
///
/// Always writes a unified block of entries regardless of agentic_root
/// configuration (ADR-0029).
fn append_gitignore(target: &Path) -> anyhow::Result<()> {
    let git_dir = target.join(".git");
    if !git_dir.is_dir() {
        // Not a git repo — skip silently
        return Ok(());
    }

    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let marker = "# Nexus CLI";

    if exclude_path.exists() {
        let content = fs::read_to_string(&exclude_path)?;
        if content.contains(marker) {
            return Ok(());
        }
    }

    // Migrate: if .gitignore contains any of these entries, remove them
    migrate_gitignore_entries(target);

    let ignores = r#"
# Nexus CLI — local workspace (ADR-0029)
.nexus/
.opencode/
opencode.json
.env.local
"#;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;

    use std::io::Write;
    file.write_all(ignores.as_bytes())?;

    print_created(".git/info/exclude (appended)");

    Ok(())
}

/// Migrate entries from `.gitignore` to `.git/info/exclude`.
///
/// If `.gitignore` contains any of the unified exclude entries, remove them
/// and print an info message.
fn migrate_gitignore_entries(target: &Path) {
    let gitignore_path = target.join(".gitignore");
    if !gitignore_path.exists() {
        return;
    }

    let entries_to_remove = [".nexus/", ".opencode/", "opencode.json", ".env.local"];

    let Ok(content) = fs::read_to_string(&gitignore_path) else {
        return;
    };

    let mut removed = Vec::new();
    let mut new_lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if entries_to_remove.contains(&trimmed) {
            removed.push(trimmed.to_string());
        } else {
            new_lines.push(line);
        }
    }

    if !removed.is_empty() {
        // Write back the cleaned .gitignore
        let new_content = new_lines.join("\n");
        let new_content = if new_content.ends_with('\n') {
            new_content
        } else {
            format!("{}\n", new_content)
        };
        let _ = fs::write(&gitignore_path, new_content);
        println!(
            "   {} Migrated {} entries from .gitignore to .git/info/exclude",
            style("i").bold().blue(),
            removed.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 2 helpers: Server-aware init
// ---------------------------------------------------------------------------

/// Write a skill definition to `.claude/skills/<skill_id>/SKILL.md`.
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

    let path = skill_dir.join("SKILL.md");
    fs::write(&path, content)?;
    print_created(&format!(
        "{}/skills/{}/SKILL.md",
        agentic_root, skill.skill_id
    ));

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
        _ => return Ok(()), // No command slug, skip
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

    let path = commands_dir.join(format!("{}.md", slug));
    fs::write(&path, content)?;
    print_created(&format!(".opencode/commands/{}.md", slug));

    Ok(())
}

/// Write all directives to `.claude/directives.md` as a single Markdown file.
///
/// Directives are grouped by category, with priority indicated inline.
fn write_directives(
    target: &Path,
    directives: &[nexus_core::api::ExportedDirective],
    agentic_root: &str,
) -> anyhow::Result<()> {
    let dir = target.join(agentic_root);
    fs::create_dir_all(&dir)?;

    let mut content = String::from(
        "---\ntype: project-directives\nsource: nexus-platform\n---\n\n# Project Directives\n\n",
    );

    // Group by category
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
                "high" | "urgent" => format!(" [{}]", d.priority.to_uppercase()),
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

    let path = dir.join("directives.md");
    fs::write(&path, format!("{}\n", content.trim_end()))?;
    print_created(&format!("{}/directives.md", agentic_root));

    Ok(())
}

/// Write a single agent file exported from the platform to its `target_path`.
///
/// Creates any intermediate directories as needed.
/// The file body is already template-substituted by the server.
///
/// Returns `Ok(false)` when the file was skipped due to a protected path
/// (existing secrets/env file) — the caller should `continue` and not
/// count this as an error. Returns `Ok(true)` on a successful write.
fn write_agent_file(
    target: &Path,
    af: &nexus_core::api::ExportedAgentFile,
) -> anyhow::Result<bool> {
    // Protected file guard: never overwrite secrets/env files.
    // This is NOT an error — the workspace init succeeded, the env file is
    // intentionally left untouched. Emit a warning and skip silently.
    if super::pull::is_protected_path(&af.target_path) {
        let dest = target.join(&af.target_path);
        if dest.exists() {
            println!(
                "   {} '{}' matches a protected file pattern, skipping (secrets/env files are never overwritten)",
                style("!").bold().yellow(),
                af.target_path
            );
            return Ok(false);
        }
    }

    // Path traversal protection: reject target_path with parent-dir components.
    // This IS a hard error — it indicates a malformed or malicious response.
    let normalized = Path::new(&af.target_path);
    for component in normalized.components() {
        if matches!(component, std::path::Component::ParentDir) {
            anyhow::bail!(
                "refusing to write: target_path '{}' contains '..' traversal",
                af.target_path
            );
        }
    }

    let dest = target.join(&af.target_path);

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&dest, &af.body)?;
    print_created(&af.target_path);

    Ok(true)
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

/// Write MCP server configuration to `opencode.json`.
///
/// Uses the correct OpenCode config schema:
/// - Top-level key `mcp` (not `mcpServers`)
/// - `type: "local"` (not `"stdio"`)
/// - `command` is an array including arguments
/// - `environment` (not `env`)
/// - Variable substitution via `{env:VAR}` syntax
///
/// When `mcp_source` is `Npm` (default), the command runs the published
/// `@gwdn/nexus-mcp` package via npx. When `Local`, it points to the
/// local checkout at `tools/nexus-mcp/dist/server.js`.
///
/// Generates both `opencode.json` (OpenCode) and `.claude/mcp.json` (Claude Code).
/// Skips writing each file if it already exists (user-managed).
fn write_mcp_configs(
    target: &Path,
    _project_name: &str,
    api_url: &str,
    token: &str,
    mcp_source: McpSource,
    tool_flavor: Option<&str>,
    agentic_root: &str,
) -> anyhow::Result<()> {
    let source_label = match mcp_source {
        McpSource::Npm => "npm (@gwdn/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    let skip_opencode = matches!(tool_flavor, Some("claude-cli"));
    let skip_claude = matches!(tool_flavor, Some("opencode"));

    // --- OpenCode config (opencode.json) ---
    if !skip_opencode {
        let opencode_path = target.join("opencode.json");

        if opencode_path.exists() {
            println!(
                "   {} opencode.json already exists, skipping",
                style("--").yellow()
            );
        } else {
            let command_block = match mcp_source {
                McpSource::Npm => r#""command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"]"#,
                McpSource::Local => r#""command": ["node", "tools/nexus-mcp/dist/server.js"]"#,
            };

            // Resolve NEXAI_SEC_OPENAI_API_KEY at write time — use literal if available
            let openai_key_value = std::env::var("NEXUS_SEC_OPENAI_API_KEY")
                .unwrap_or_else(|_| "{env:NEXUS_SEC_OPENAI_API_KEY}".to_string());

            let opencode_json = format!(
                r#"{{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {{
    "nexus": {{
      "type": "local",
      {command_block},
      "environment": {{
        "NEXUS_API_URL": "{api_url}",
        "NEXUS_PRIVATE_TOKEN": "{token}",
        "NEXUS_SEC_OPENAI_API_KEY": "{openai_key}"
      }}
    }}
  }}
}}
"#,
                command_block = command_block,
                api_url = api_url,
                token = token,
                openai_key = openai_key_value,
            );

            fs::write(&opencode_path, opencode_json)?;
            println!(
                "   {} opencode.json (MCP source: {})",
                style("+").bold().green(),
                source_label,
            );
        }
    }

    // --- Claude Code config ({agentic_root}/mcp.json) ---
    if !skip_claude {
        let claude_mcp_path = target.join(agentic_root).join("mcp.json");

        if claude_mcp_path.exists() {
            println!(
                "   {} {}/mcp.json already exists, skipping",
                style("--").yellow(),
                agentic_root
            );
        } else {
            let (cmd, args) = match mcp_source {
                McpSource::Npm => ("npx", r#""--yes", "@gwdn/nexus-mcp@latest""#),
                McpSource::Local => ("node", r#""tools/nexus-mcp/dist/server.js""#),
            };

            let claude_mcp_json = format!(
                r#"{{
  "mcpServers": {{
    "nexus": {{
      "command": "{cmd}",
      "args": [{args}],
      "env": {{
        "NEXUS_API_URL": "{api_url}",
        "NEXUS_PRIVATE_TOKEN": "{token}"
      }}
    }}
  }}
}}
"#,
                cmd = cmd,
                args = args,
                api_url = api_url,
                token = token,
            );

            // .claude/ directory should already exist from earlier init steps
            if let Some(parent) = claude_mcp_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&claude_mcp_path, claude_mcp_json)?;
            println!(
                "   {} {}/mcp.json (MCP source: {})",
                style("+").bold().green(),
                agentic_root,
                source_label,
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Merge extra MCP servers from `[mcp_extra]` config into `opencode.json`
/// and `{agentic_root}/mcp.json` (Claude Code format).
///
/// Environment values containing `${env:VAR}` are resolved from the process
/// environment first, then from `.env.local` in the project root as a
/// fallback.  Unresolvable references are replaced with an empty string and
/// a warning is printed.
fn merge_extra_mcp_servers(
    target: &Path,
    extras: &HashMap<String, ExtraMcpServer>,
    agentic_root: &str,
) -> anyhow::Result<()> {
    if extras.is_empty() {
        return Ok(());
    }

    // Pre-load .env.local so we can resolve ${env:VAR} references.
    let dotenv = load_dotenv(target);

    // --- OpenCode (opencode.json) ---
    let opencode_path = target.join("opencode.json");
    if opencode_path.exists() {
        let content = fs::read_to_string(&opencode_path)?;
        let mut doc: serde_json::Value = serde_json::from_str(&content)?;

        if let Some(mcp) = doc
            .as_object_mut()
            .and_then(|o| o.get_mut("mcp"))
            .and_then(|v| v.as_object_mut())
        {
            for (name, server) in extras {
                if mcp.contains_key(name) {
                    continue;
                }
                // Check for duplicate: same command array under a different key.
                let cmd_json = serde_json::Value::Array(
                    server
                        .command
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                );
                let duplicate_key = mcp
                    .iter()
                    .find(|(_, v)| v.get("command").is_some_and(|c| *c == cmd_json));
                if let Some((existing_key, _)) = duplicate_key {
                    println!(
                        "   {} opencode.json: skipping '{}' — duplicate of existing '{}' (same command)",
                        style("!").bold().yellow(),
                        name,
                        existing_key,
                    );
                    continue;
                }
                let entry = build_opencode_entry(server, &dotenv);
                mcp.insert(name.clone(), serde_json::Value::Object(entry));
                println!(
                    "   {} opencode.json: added MCP server '{}'",
                    style("+").bold().green(),
                    name,
                );
            }

            let output = serde_json::to_string_pretty(&doc)?;
            fs::write(&opencode_path, format!("{}\n", output))?;
        }
    }

    // --- Claude Code ({agentic_root}/mcp.json) ---
    let claude_mcp_path = target.join(agentic_root).join("mcp.json");
    if claude_mcp_path.exists() {
        let content = fs::read_to_string(&claude_mcp_path)?;
        let mut doc: serde_json::Value = serde_json::from_str(&content)?;

        if let Some(servers) = doc
            .as_object_mut()
            .and_then(|o| o.get_mut("mcpServers"))
            .and_then(|v| v.as_object_mut())
        {
            for (name, server) in extras {
                if servers.contains_key(name) {
                    continue;
                }
                // Check for duplicate: same command under a different key.
                let cmd_str = server.command.first().cloned().unwrap_or_default();
                let duplicate_key = servers.iter().find(|(_, v)| {
                    v.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c == cmd_str)
                });
                if let Some((existing_key, _)) = duplicate_key {
                    println!(
                        "   {} {}/mcp.json: skipping '{}' — duplicate of existing '{}' (same command)",
                        style("!").bold().yellow(),
                        agentic_root,
                        name,
                        existing_key,
                    );
                    continue;
                }
                let entry = build_claude_entry(server, &dotenv);
                servers.insert(name.clone(), serde_json::Value::Object(entry));
                println!(
                    "   {} {}/mcp.json: added MCP server '{}'",
                    style("+").bold().green(),
                    agentic_root,
                    name,
                );
            }

            let output = serde_json::to_string_pretty(&doc)?;
            fs::write(&claude_mcp_path, format!("{}\n", output))?;
        }
    }

    Ok(())
}

/// Build an OpenCode-format MCP entry (`type`, `command`, `environment`).
///
/// Environment values containing `${env:VAR}` are **translated** to
/// OpenCode's native `{env:VAR}` syntax instead of being resolved to their
/// actual values.  This avoids leaking secrets into `opencode.json`.
fn build_opencode_entry(
    server: &ExtraMcpServer,
    _dotenv: &HashMap<String, String>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut entry = serde_json::Map::new();
    entry.insert(
        "type".to_string(),
        serde_json::Value::String("local".to_string()),
    );
    entry.insert(
        "command".to_string(),
        serde_json::Value::Array(
            server
                .command
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    if let Some(ref env) = server.environment {
        let env_obj: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::Value::String(translate_env_ref_to_opencode(v)),
                )
            })
            .collect();
        entry.insert(
            "environment".to_string(),
            serde_json::Value::Object(env_obj),
        );
    }
    entry
}

/// Build a Claude Code-format MCP entry (`command`, `args`, `env`).
fn build_claude_entry(
    server: &ExtraMcpServer,
    dotenv: &HashMap<String, String>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut entry = serde_json::Map::new();
    if let Some(cmd) = server.command.first() {
        entry.insert(
            "command".to_string(),
            serde_json::Value::String(cmd.clone()),
        );
    }
    if server.command.len() > 1 {
        entry.insert(
            "args".to_string(),
            serde_json::Value::Array(
                server.command[1..]
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(ref env) = server.environment {
        let env_obj: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::Value::String(resolve_env_ref(v, dotenv)),
                )
            })
            .collect();
        entry.insert("env".to_string(), serde_json::Value::Object(env_obj));
    }
    entry
}

/// Translate `${env:VAR_NAME}` references to OpenCode's native `{env:VAR_NAME}`
/// syntax.  Values without env references are passed through unchanged.
fn translate_env_ref_to_opencode(value: &str) -> String {
    let re = regex::Regex::new(r"\$\{env:([^}]+)\}").unwrap();
    re.replace_all(value, "{env:$1}").into_owned()
}

/// Resolve `${env:VAR_NAME}` references in a string value.
///
/// Lookup order: OS environment -> `.env.local` entries.
/// Unresolvable references are replaced with `""` and a warning is printed.
fn resolve_env_ref(value: &str, dotenv: &HashMap<String, String>) -> String {
    let re = regex::Regex::new(r"\$\{env:([^}]+)\}").unwrap();
    re.replace_all(value, |caps: &regex::Captures| {
        let var = &caps[1];
        if let Ok(val) = std::env::var(var) {
            val
        } else if let Some(val) = dotenv.get(var) {
            val.clone()
        } else {
            eprintln!(
                "   {} env ref ${{env:{}}} not found in environment or .env.local",
                style("!").bold().yellow(),
                var,
            );
            String::new()
        }
    })
    .into_owned()
}

/// Parse a `.env.local` file (KEY=VALUE lines, # comments, empty lines).
fn load_dotenv(target: &Path) -> HashMap<String, String> {
    let dotenv_path = target.join(".env.local");
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string(&dotenv_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim().to_string();
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                map.insert(key, value);
            }
        }
    }
    map
}

/// Built-in registry of known Nexus OpenCode plugins.
///
/// Maps a plugin slug (as stored in the platform) to a `PluginDef` with the
/// canonical GitHub raw download URL. These are the "platform-managed" plugins
/// that are automatically installed when selected in the project wizard.
///
/// To add a new plugin: add an entry here with `source = "github-raw"` and the
/// raw URL of the `.ts` file in the `gwnexus/nexus-oc-plugins` repository.
pub fn resolve_platform_plugins(slugs: &[String]) -> HashMap<String, PluginDef> {
    let mut registry: HashMap<&str, PluginDef> = HashMap::new();

    registry.insert(
        "nexus-compaction-plus",
        PluginDef {
            source: "github-raw".to_string(),
            url: Some(
                "https://raw.githubusercontent.com/gwnexus/nexus-oc-plugins/main/100-compaction-plus/nexus-compaction-plus.ts"
                    .to_string(),
            ),
            path: None,
            filename: Some("nexus-compaction-plus.ts".to_string()),
        },
    );

    registry.insert(
        "nexus-cost-control",
        PluginDef {
            source: "github-raw".to_string(),
            url: Some(
                "https://raw.githubusercontent.com/gwnexus/nexus-oc-plugins/main/200-cost-control/nexus-cost-control.ts"
                    .to_string(),
            ),
            path: None,
            filename: Some("nexus-cost-control.ts".to_string()),
        },
    );

    slugs
        .iter()
        .filter_map(|slug| {
            registry
                .remove(slug.as_str())
                .map(|def| (slug.clone(), def))
        })
        .collect()
}

/// Install plugins from `[plugins]` config into `.opencode/plugins/`.
///
/// Supports `source = "github-raw"` (downloads via HTTP) and
/// `source = "local"` (copies from a local path).
async fn install_plugins(
    target: &Path,
    plugins: &HashMap<String, PluginDef>,
) -> anyhow::Result<()> {
    if plugins.is_empty() {
        return Ok(());
    }

    let plugins_dir = target.join(".opencode").join("plugins");
    fs::create_dir_all(&plugins_dir)?;

    for (name, def) in plugins {
        let filename = def.filename.clone().unwrap_or_else(|| {
            // Derive filename from URL or name
            if let Some(ref url) = def.url {
                url.rsplit('/').next().unwrap_or(name).to_string()
            } else {
                format!("{}.ts", name)
            }
        });

        let dest = plugins_dir.join(&filename);

        match def.source.as_str() {
            "github-raw" => {
                if let Some(ref url) = def.url {
                    // SEC-003: validate URL against trusted allowlist
                    if let Err(e) = super::pull::validate_download_url(url) {
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
                            fs::write(&dest, &body)?;
                            println!(
                                "   {} .opencode/plugins/{} (downloaded)",
                                style("+").bold().green(),
                                filename,
                            );
                        }
                        Ok(resp) => {
                            println!(
                                "   {} Plugin '{}' download failed: HTTP {}",
                                style("!").bold().yellow(),
                                name,
                                resp.status(),
                            );
                        }
                        Err(e) => {
                            println!(
                                "   {} Plugin '{}' download failed: {}",
                                style("!").bold().yellow(),
                                name,
                                e,
                            );
                        }
                    }
                } else {
                    println!(
                        "   {} Plugin '{}': no url specified for github-raw source",
                        style("!").bold().yellow(),
                        name,
                    );
                }
            }
            "local" => {
                if let Some(ref path) = def.path {
                    let src = PathBuf::from(path);
                    if src.exists() {
                        fs::copy(&src, &dest)?;
                        println!(
                            "   {} .opencode/plugins/{} (copied from local)",
                            style("+").bold().green(),
                            filename,
                        );
                    } else {
                        println!(
                            "   {} Plugin '{}': local path not found: {}",
                            style("!").bold().yellow(),
                            name,
                            path,
                        );
                    }
                }
            }
            other => {
                println!(
                    "   {} Plugin '{}': unknown source type '{}'",
                    style("!").bold().yellow(),
                    name,
                    other,
                );
            }
        }
    }

    Ok(())
}

/// Prompt the user for input with a default value. Returns default on empty input.
fn prompt_with_default(label: &str, default: &str) -> anyhow::Result<String> {
    use std::io::{self, BufRead, Write};

    print!("{} [{}]: ", label, style(default).dim());
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();

    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Print a "created" status line.
fn print_created(path: &str) {
    println!("   {} {}", style("+").bold().green(), path);
}

/// Print the completion message and next steps.
///
/// When `server_aware` is true (project was synced from platform), the caller
/// already printed flavour-specific next steps, so we only show the banner.
/// When false (offline / no project), we print generic getting-started steps.
fn print_done(server_aware: bool) {
    println!();
    println!(
        "{} Nexus workspace initialized successfully.",
        style("OK").bold().green()
    );

    if !server_aware {
        println!();
        println!("Next steps:");
        println!("  1. Run {} to authenticate", style("nexus login").bold());
        println!(
            "  2. Run {} to link and sync from the platform",
            style("nexus init --project-id <UUID>").bold()
        );
        println!(
            "  3. Review {} for skill definitions",
            style(".nexus/skills/").bold()
        );
        println!(
            "  4. Review {} for agent commands & configs",
            style(".opencode/").bold()
        );
        println!("  5. Start your coding agent — skills and MCP are ready");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a unique temp directory for testing init.
    /// Initializes a bare git repo so .git/info/exclude can be tested.
    fn temp_project_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Initialize a git repo so append_gitignore can write to .git/info/exclude
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .output()
            .ok();
        dir
    }

    #[tokio::test]
    async fn test_init_creates_structure() {
        let dir = temp_project_dir("creates-structure");
        run(
            dir.to_str().unwrap(),
            Some("test-project"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        assert!(dir.join(".nexus/config.toml").exists());
        assert!(dir.join(".nexus/CLAUDE.md").exists());
        assert!(dir.join(".nexus/skills").is_dir());
        assert!(dir.join(".opencode/commands").is_dir());
        assert!(dir.join(".nexus/AGENTS.md").exists());
        assert!(dir.join(".git/info/exclude").exists());

        // Verify config content
        let config = fs::read_to_string(dir.join(".nexus/config.toml")).unwrap();
        assert!(config.contains("test-project"));
        assert!(config.contains("# id ="));

        // Verify AGENTS.md content
        let agents = fs::read_to_string(dir.join(".nexus/AGENTS.md")).unwrap();
        assert!(agents.contains("test-project"));

        // Should NOT have opencode.json (no project_id, so no server-aware phase)
        assert!(!dir.join("opencode.json").exists());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_with_project_id_stores_uuid() {
        let dir = temp_project_dir("project-id-uuid");
        // This will attempt server sync but fail (no real token), which is fine.
        // The local scaffolding should still work and store the project ID.
        run(
            dir.to_str().unwrap(),
            Some("test-project"),
            Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d"),
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        let config = fs::read_to_string(dir.join(".nexus/config.toml")).unwrap();
        assert!(config.contains("fdc7a78c-d0b9-46fd-8206-9fc57301de2d"));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_fails_if_exists_without_force() {
        let dir = temp_project_dir("fails-no-force");
        fs::create_dir_all(dir.join(".nexus")).unwrap();

        let result = run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await;
        assert!(result.is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_force_reinitializes() {
        let dir = temp_project_dir("force-reinit");
        fs::create_dir_all(dir.join(".nexus")).unwrap();

        let result = run(
            dir.to_str().unwrap(),
            Some("reinit-test"),
            None,
            "https://nexus.gatewarden.eu",
            true,
            McpSource::Npm,
        )
        .await;
        assert!(result.is_ok());
        assert!(dir.join(".nexus/config.toml").exists());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_preserves_existing_agents_md() {
        let dir = temp_project_dir("preserves-agents");
        fs::write(dir.join("AGENTS.md"), "custom content").unwrap();

        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        let agents = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert_eq!(agents, "custom content");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_preserves_existing_claude_md() {
        let dir = temp_project_dir("preserves-claude-md");
        let claude_dir = dir.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "user-managed instructions").unwrap();

        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        // Must NOT overwrite existing CLAUDE.md
        let claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert_eq!(claude_md, "user-managed instructions");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_force_overwrites_agents_md() {
        let dir = temp_project_dir("force-agents");
        fs::write(dir.join("AGENTS.md"), "old content").unwrap();

        run(
            dir.to_str().unwrap(),
            Some("fresh"),
            None,
            "https://nexus.gatewarden.eu",
            true,
            McpSource::Npm,
        )
        .await
        .unwrap();

        let agents = fs::read_to_string(dir.join(".nexus/AGENTS.md")).unwrap();
        assert!(agents.contains("fresh"));
        assert!(!agents.contains("old content"));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_gitignore_not_duplicated() {
        let dir = temp_project_dir("gitignore-dedup");
        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        // Run again with force
        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            true,
            McpSource::Npm,
        )
        .await
        .unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        let marker_count = exclude.matches("# Nexus CLI").count();
        assert_eq!(marker_count, 1, "exclude marker should appear only once");

        // Verify that opencode.json and .nexus/ are in .git/info/exclude
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be in .git/info/exclude"
        );
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be in .git/info/exclude"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_exclude_unified() {
        let dir = temp_project_dir("exclude-unified");
        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
        )
        .await
        .unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be in .git/info/exclude"
        );
        assert!(
            exclude.contains(".opencode/"),
            ".opencode/ must be in .git/info/exclude"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be in .git/info/exclude"
        );
        assert!(
            exclude.contains(".env.local"),
            ".env.local must be in .git/info/exclude"
        );
        assert!(
            exclude.contains("ADR-0029"),
            "ADR-0029 marker must be in .git/info/exclude"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unified_exclude_block() {
        let dir = temp_project_dir("unified-block");

        append_gitignore(&dir).unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be in unified exclude block"
        );
        assert!(
            exclude.contains(".opencode/"),
            ".opencode/ must be in unified exclude block"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be in unified exclude block"
        );
        assert!(
            exclude.contains(".env.local"),
            ".env.local must be in unified exclude block"
        );
        assert!(exclude.contains("# Nexus CLI"), "marker must be present");

        // Idempotency: calling again should not duplicate
        append_gitignore(&dir).unwrap();
        let exclude2 = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        let marker_count = exclude2.matches("# Nexus CLI").count();
        assert_eq!(marker_count, 1, "marker should appear only once");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_skill_creates_file() {
        let dir = temp_project_dir("write-skill");
        fs::create_dir_all(dir.join(".claude/skills")).unwrap();

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

        let path = dir.join(".claude/skills/nx-test-skill/SKILL.md");
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("skill_id: nx-test-skill"));
        assert!(content.contains("Do the thing."));
        assert!(content.contains("version: 1"));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_command_creates_file() {
        let dir = temp_project_dir("write-cmd");
        fs::create_dir_all(dir.join(".opencode/commands")).unwrap();

        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-test-skill".to_string(),
            name: "Test Skill".to_string(),
            description: None,
            version: 2,
            body: Some("Instructions here.".to_string()),
            command_slug: Some("nexus-test-skill".to_string()),
            pinned: false,
            resources: vec![],
        };

        write_command(&dir, &skill, ".claude").unwrap();

        let path = dir.join(".opencode/commands/nexus-test-skill.md");
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("skill_id: \"nx-test-skill\""));
        assert!(content.contains(".claude/skills/nx-test-skill/SKILL.md"));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_command_skips_without_slug() {
        let dir = temp_project_dir("cmd-no-slug");
        fs::create_dir_all(dir.join(".opencode/commands")).unwrap();

        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-no-cmd".to_string(),
            name: "No Command".to_string(),
            description: None,
            version: 1,
            body: None,
            command_slug: None,
            pinned: false,
            resources: vec![],
        };

        write_command(&dir, &skill, ".claude").unwrap();

        // No file should be created
        let entries: Vec<_> = fs::read_dir(dir.join(".opencode/commands"))
            .unwrap()
            .collect();
        assert!(entries.is_empty());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_npm_mode() {
        let dir = temp_project_dir("mcp-configs-npm");

        // Isolate from shell environment: if NEXUS_SEC_OPENAI_API_KEY is set
        // in the shell (e.g. from .env.nexus.local via devbox) the assertion
        // `oc.contains("{env:NEXUS_SEC_OPENAI_API_KEY}")` would fail because
        // write_mcp_configs would write the literal value instead.
        let _prev_key = std::env::var("NEXUS_SEC_OPENAI_API_KEY").ok();
        std::env::remove_var("NEXUS_SEC_OPENAI_API_KEY");

        write_mcp_configs(
            &dir,
            "test-proj",
            "https://nexus.gatewarden.eu",
            "nxs_pat_test-token-1234567890",
            McpSource::Npm,
            None,
            ".claude",
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"nexus\""));
        // Must use npx command for npm mode
        assert!(oc.contains("@gwdn/nexus-mcp"));
        assert!(oc.contains("npx"));
        assert!(!oc.contains("tools/nexus-mcp/dist/server.js"));
        // Must use "mcp" key (not "mcpServers")
        assert!(oc.contains("\"mcp\""));
        assert!(!oc.contains("\"mcpServers\""));
        // Must use "local" type (not "stdio")
        assert!(oc.contains("\"local\""));
        assert!(!oc.contains("\"stdio\""));
        // Must use "environment" (not "env")
        assert!(oc.contains("\"environment\""));
        // Must contain literal token and URL (not env var references for secrets)
        assert!(oc.contains("https://nexus.gatewarden.eu"));
        assert!(oc.contains("nxs_pat_test-token-1234567890"));
        // NEXUS_PRIVATE_TOKEN must be a literal value, never an {env:} reference
        assert!(!oc.contains("{env:NEXUS_PRIVATE_TOKEN}"));
        assert!(!oc.contains("{env:NEXUS_API_URL}"));
        // NEXUS_SEC_OPENAI_API_KEY is intentionally an {env:} reference —
        // it is not a Nexus credential and should be resolved from the shell at runtime
        assert!(oc.contains("{env:NEXUS_SEC_OPENAI_API_KEY}"));

        // .mcp.json must NOT be created (legacy root-level format)
        assert!(!dir.join(".mcp.json").exists());

        // .claude/mcp.json MUST be created
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
        assert!(cm.contains("\"mcpServers\""));
        assert!(cm.contains("@gwdn/nexus-mcp"));
        assert!(cm.contains("nxs_pat_test-token-1234567890"));
        // Claude uses "command" + "args" format, not "command": [array]
        assert!(cm.contains("\"command\": \"npx\""));
        assert!(cm.contains("\"args\""));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_local_mode() {
        let dir = temp_project_dir("mcp-configs-local");

        write_mcp_configs(
            &dir,
            "test-proj",
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-test-token",
            McpSource::Local,
            None,
            ".claude",
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Must use local command
        assert!(oc.contains("tools/nexus-mcp/dist/server.js"));
        assert!(!oc.contains("npx"));
        assert!(!oc.contains("@gwdn/nexus-mcp"));

        // .claude/mcp.json must also exist with local path
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
        assert!(cm.contains("tools/nexus-mcp/dist/server.js"));
        assert!(cm.contains("\"command\": \"node\""));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_skips_existing() {
        let dir = temp_project_dir("mcp-configs-skip");

        // Pre-create both config files with custom content
        fs::write(dir.join("opencode.json"), "user-managed content").unwrap();
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(".claude/mcp.json"), "user-managed claude").unwrap();

        write_mcp_configs(
            &dir,
            "test-proj",
            "https://nexus.gatewarden.eu",
            "nxs_pat_skip-test-token",
            McpSource::Npm,
            None,
            ".claude",
        )
        .unwrap();

        // Must NOT overwrite existing files
        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert_eq!(oc, "user-managed content");
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
        assert_eq!(cm, "user-managed claude");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_extra_mcp_servers() {
        let dir = temp_project_dir("merge-extra-mcp");

        // Create a minimal opencode.json
        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"],
      "environment": {
        "NEXUS_API_URL": "https://nexus.gatewarden.eu",
        "NEXUS_PRIVATE_TOKEN": "nxs_pat_test"
      }
    }
  }
}
"#;
        fs::write(dir.join("opencode.json"), initial).unwrap();

        let mut extras = HashMap::new();
        extras.insert(
            "taskmaster-ai".to_string(),
            ExtraMcpServer {
                command: vec![
                    "npx".to_string(),
                    "-y".to_string(),
                    "task-master-ai@latest".to_string(),
                ],
                environment: None,
            },
        );

        merge_extra_mcp_servers(&dir, &extras, ".nexus").unwrap();

        let result = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(
            result.contains("taskmaster-ai"),
            "extra MCP server must be added"
        );
        assert!(
            result.contains("task-master-ai@latest"),
            "command must be present"
        );
        assert!(
            result.contains("nexus"),
            "original nexus entry must be preserved"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_extra_mcp_servers_does_not_overwrite_existing() {
        let dir = temp_project_dir("merge-extra-no-overwrite");

        // Create opencode.json that already has taskmaster-ai
        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"]
    },
    "taskmaster-ai": {
      "type": "local",
      "command": ["npx", "-y", "task-master-ai@0.1.0"]
    }
  }
}
"#;
        fs::write(dir.join("opencode.json"), initial).unwrap();

        let mut extras = HashMap::new();
        extras.insert(
            "taskmaster-ai".to_string(),
            ExtraMcpServer {
                command: vec![
                    "npx".to_string(),
                    "-y".to_string(),
                    "task-master-ai@latest".to_string(),
                ],
                environment: None,
            },
        );

        merge_extra_mcp_servers(&dir, &extras, ".nexus").unwrap();

        let result = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Must preserve the existing version, not overwrite with @latest
        assert!(
            result.contains("task-master-ai@0.1.0"),
            "existing entry must NOT be overwritten"
        );
        assert!(
            !result.contains("task-master-ai@latest"),
            "new version must NOT replace existing"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_extra_mcp_servers_with_environment() {
        let dir = temp_project_dir("merge-extra-env");

        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"]
    }
  }
}
"#;
        fs::write(dir.join("opencode.json"), initial).unwrap();

        let mut env = std::collections::HashMap::new();
        env.insert("API_KEY".to_string(), "secret123".to_string());

        let mut extras = HashMap::new();
        extras.insert(
            "my-server".to_string(),
            ExtraMcpServer {
                command: vec!["node".to_string(), "server.js".to_string()],
                environment: Some(env),
            },
        );

        merge_extra_mcp_servers(&dir, &extras, ".nexus").unwrap();

        let result = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(result.contains("my-server"), "new server must be added");
        assert!(result.contains("API_KEY"), "environment must be present");
        assert!(
            result.contains("secret123"),
            "plain env value must be present"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_extra_mcp_servers_translates_env_refs() {
        let dir = temp_project_dir("merge-extra-env-ref");

        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"]
    }
  }
}
"#;
        fs::write(dir.join("opencode.json"), initial).unwrap();

        let mut env = std::collections::HashMap::new();
        env.insert(
            "MY_API_KEY".to_string(),
            "${env:NEXUS_SEC_MY_API_KEY}".to_string(),
        );

        let mut extras = HashMap::new();
        extras.insert(
            "my-server".to_string(),
            ExtraMcpServer {
                command: vec!["node".to_string(), "server.js".to_string()],
                environment: Some(env),
            },
        );

        merge_extra_mcp_servers(&dir, &extras, ".nexus").unwrap();

        let result = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(
            result.contains("{env:NEXUS_SEC_MY_API_KEY}"),
            "env ref must be translated to OpenCode syntax, got: {}",
            result,
        );
        assert!(
            !result.contains("${env:"),
            "config.toml syntax must not appear in opencode.json",
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_extra_mcp_servers_dedup_by_command() {
        let dir = temp_project_dir("merge-extra-dedup");

        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "task-master-ai": {
      "type": "local",
      "command": ["npx", "-y", "task-master-ai@latest"]
    }
  }
}
"#;
        fs::write(dir.join("opencode.json"), initial).unwrap();

        let mut extras = HashMap::new();
        extras.insert(
            "taskmaster-ai".to_string(),
            ExtraMcpServer {
                command: vec![
                    "npx".to_string(),
                    "-y".to_string(),
                    "task-master-ai@latest".to_string(),
                ],
                environment: None,
            },
        );

        merge_extra_mcp_servers(&dir, &extras, ".nexus").unwrap();

        let result = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(
            result.contains("task-master-ai"),
            "original entry must remain"
        );
        assert!(
            !result.contains("taskmaster-ai"),
            "duplicate must NOT be added"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_install_plugins_local_source() {
        let dir = temp_project_dir("install-plugins-local");
        let plugins_dir = dir.join(".opencode").join("plugins");
        fs::create_dir_all(&plugins_dir).unwrap();

        // Create a fake plugin source file
        let source_file = dir.join("my-plugin.ts");
        fs::write(&source_file, "// plugin content").unwrap();

        let mut plugins = HashMap::new();
        plugins.insert(
            "my-plugin".to_string(),
            nexus_core::config::PluginDef {
                source: "local".to_string(),
                url: None,
                path: Some(source_file.to_str().unwrap().to_string()),
                filename: Some("my-plugin.ts".to_string()),
            },
        );

        install_plugins(&dir, &plugins).await.unwrap();

        let dest = plugins_dir.join("my-plugin.ts");
        assert!(dest.exists(), "plugin must be copied to .opencode/plugins/");
        let content = fs::read_to_string(&dest).unwrap();
        assert_eq!(content, "// plugin content");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_install_plugins_local_missing_path() {
        let dir = temp_project_dir("install-plugins-missing");
        fs::create_dir_all(dir.join(".opencode/plugins")).unwrap();

        let mut plugins = HashMap::new();
        plugins.insert(
            "missing-plugin".to_string(),
            nexus_core::config::PluginDef {
                source: "local".to_string(),
                url: None,
                path: Some("/nonexistent/path/plugin.ts".to_string()),
                filename: Some("plugin.ts".to_string()),
            },
        );

        // Should not error, just warn
        install_plugins(&dir, &plugins).await.unwrap();

        let dest = dir.join(".opencode/plugins/plugin.ts");
        assert!(
            !dest.exists(),
            "plugin must NOT be created from missing source"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_nexus_dir_preserves_existing_config() {
        let dir = temp_project_dir("preserve-config");
        let nexus_dir = dir.join(".nexus");
        fs::create_dir_all(&nexus_dir).unwrap();

        // Write a config with mcp_extra and plugins
        let config_content = r#"[project]
name = "test"
id = "fdc7a78c-d0b9-46fd-8206-9fc57301de2d"

[mcp_extra.taskmaster-ai]
command = ["npx", "-y", "task-master-ai@latest"]

[plugins.compaction-plus]
source = "github-raw"
url = "https://example.com/plugin.ts"
"#;
        fs::write(nexus_dir.join("config.toml"), config_content).unwrap();

        // Run create_nexus_dir — it should NOT overwrite the config
        create_nexus_dir(&dir, "test", Some("fdc7a78c-d0b9-46fd-8206-9fc57301de2d")).unwrap();

        let result = fs::read_to_string(nexus_dir.join("config.toml")).unwrap();
        assert!(
            result.contains("mcp_extra"),
            "mcp_extra section must be preserved"
        );
        assert!(
            result.contains("plugins"),
            "plugins section must be preserved"
        );
        assert!(
            result.contains("taskmaster-ai"),
            "extra server name must be preserved"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_platform_plugins_known_slugs() {
        let slugs = vec![
            "nexus-compaction-plus".to_string(),
            "nexus-cost-control".to_string(),
        ];
        let result = resolve_platform_plugins(&slugs);

        assert_eq!(result.len(), 2, "both known slugs must resolve");

        let compaction = result.get("nexus-compaction-plus").unwrap();
        assert_eq!(compaction.source, "github-raw");
        assert!(
            compaction
                .url
                .as_deref()
                .unwrap_or("")
                .contains("100-compaction-plus"),
            "compaction-plus URL must point to 100-compaction-plus directory"
        );
        assert_eq!(
            compaction.filename.as_deref(),
            Some("nexus-compaction-plus.ts")
        );

        let cost = result.get("nexus-cost-control").unwrap();
        assert_eq!(cost.source, "github-raw");
        assert!(
            cost.url
                .as_deref()
                .unwrap_or("")
                .contains("200-cost-control"),
            "cost-control URL must point to 200-cost-control directory"
        );
        assert_eq!(cost.filename.as_deref(), Some("nexus-cost-control.ts"));
    }

    #[test]
    fn test_resolve_platform_plugins_unknown_slug_ignored() {
        let slugs = vec!["rtk".to_string(), "taskmaster-ai".to_string()];
        let result = resolve_platform_plugins(&slugs);
        assert!(
            result.is_empty(),
            "unknown/optional plugin slugs must not resolve to download entries"
        );
    }

    #[test]
    fn test_resolve_platform_plugins_partial_match() {
        let slugs = vec![
            "nexus-compaction-plus".to_string(),
            "rtk".to_string(), // not in registry
        ];
        let result = resolve_platform_plugins(&slugs);
        assert_eq!(result.len(), 1, "only known slugs should resolve");
        assert!(result.contains_key("nexus-compaction-plus"));
    }

    #[test]
    fn test_resolve_platform_plugins_empty() {
        let result = resolve_platform_plugins(&[]);
        assert!(result.is_empty(), "empty input must return empty map");
    }
}
