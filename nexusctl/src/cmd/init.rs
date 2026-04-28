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
use nexus_core::McpSource;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cmd::pull::{
    detect_agentic_conflicts, notify_agentic_coexistence, warn_agentic_conflicts,
};

/// Run the init command.
pub async fn run(
    path: &str,
    name: Option<&str>,
    project_id: Option<&str>,
    api_url: &str,
    force: bool,
    mcp_source: McpSource,
    shadowed_ai: bool,
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

    // -----------------------------------------------------------------------
    // Phase 2: Server-aware init (when project_id + token are available)
    // -----------------------------------------------------------------------
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

                    // Fall back to default .claude/ scaffold since we cannot
                    // determine the server-configured agentic_root.
                    let default_agentic_root = ".claude";
                    let conflicts = detect_agentic_conflicts(&target);
                    if !conflicts.is_empty() && !warn_agentic_conflicts(&conflicts, force)? {
                        return Ok(());
                    }
                    create_claude_dir(&target, project_name, default_agentic_root)?;
                    create_agents_md(&target, project_name, force, default_agentic_root)?;
                    append_gitignore(&target, shadowed_ai, default_agentic_root)?;

                    print_done();
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
                .unwrap_or_else(|| ".claude".to_string());

            // Agentic conflict detection / coexistence notice
            if agentic_root == ".claude" {
                let conflicts = detect_agentic_conflicts(&target);
                if !conflicts.is_empty() && !warn_agentic_conflicts(&conflicts, force)? {
                    return Ok(());
                }
            } else {
                notify_agentic_coexistence(&target, &agentic_root);
            }

            // Create the agentic root directory (.claude/ or .nexus/ etc.)
            create_claude_dir(&target, project_name, &agentic_root)?;
            create_agents_md(&target, project_name, force, &agentic_root)?;
            append_gitignore(&target, shadowed_ai, &agentic_root)?;

            match client.export_skills(pid).await {
                Ok(export) => {
                    println!(
                        "   {} Project: {} ({})",
                        style("+").bold().green(),
                        style(&export.project.name).bold(),
                        &export.project.slug
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
            let mcp_api_url = if force {
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
                            write_agent_file(&target, af)?;
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
                println!(
                    "   This project is optimized for: {}",
                    style(flavor_label).bold().cyan()
                );
                println!();
                println!("Next steps:");
                match flavor {
                    "claude-cli" => {
                        println!("  1. Start Claude CLI in this directory");
                        println!(
                            "  2. Run {} to bootstrap the agent",
                            style("/nexus-init").bold()
                        );
                    }
                    "opencode" => {
                        println!("  1. Start OpenCode in this directory");
                        println!(
                            "  2. Run {} to bootstrap the agent",
                            style("/nexus-init").bold()
                        );
                    }
                    _ => {
                        println!("  1. Start OpenCode or Claude CLI in this directory");
                        println!(
                            "  2. Run {} to bootstrap the agent",
                            style("/nexus-init").bold()
                        );
                    }
                }
                println!(
                    "  3. Run {} periodically to sync skills and agent files",
                    style("nexus pull").bold()
                );
                println!();
                println!(
                    "   {} Local changes to agent files (AGENTS.md, CLAUDE.md, etc.)",
                    style("!").bold().yellow()
                );
                println!("     will be overwritten by 'nexus pull'. These files are managed");
                println!("     by the Nexus platform and cannot be pushed back yet.");
            }
        } else {
            // No token — fall back to default .claude/ scaffold
            let default_agentic_root = ".claude";
            let conflicts = detect_agentic_conflicts(&target);
            if !conflicts.is_empty() && !warn_agentic_conflicts(&conflicts, force)? {
                return Ok(());
            }
            create_claude_dir(&target, project_name, default_agentic_root)?;
            create_agents_md(&target, project_name, force, default_agentic_root)?;
            append_gitignore(&target, shadowed_ai, default_agentic_root)?;
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
        // No project linked — create default .claude/ scaffold so the
        // workspace is immediately usable with coding agents.
        let default_agentic_root = ".claude";
        let conflicts = detect_agentic_conflicts(&target);
        if !conflicts.is_empty() && !warn_agentic_conflicts(&conflicts, force)? {
            return Ok(());
        }
        create_claude_dir(&target, project_name, default_agentic_root)?;
        create_agents_md(&target, project_name, force, default_agentic_root)?;
        append_gitignore(&target, shadowed_ai, default_agentic_root)?;
    }

    print_done();
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1 helpers: Local scaffolding
// ---------------------------------------------------------------------------

/// Create .nexus/ directory with project-local config.
fn create_nexus_dir(
    target: &Path,
    project_name: &str,
    project_id: Option<&str>,
) -> anyhow::Result<()> {
    let nexus_dir = target.join(".nexus");
    fs::create_dir_all(&nexus_dir)?;

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
/// When `shadowed_ai` is true, ALL AI scaffold files are excluded
/// (AGENTS.md, .claude/, .opencode/, opencode.json). Without this flag, only
/// sensitive/user-managed files are excluded (.env.local, credentials, opencode.json,
/// CLAUDE.md).
fn append_gitignore(target: &Path, shadowed_ai: bool, agentic_root: &str) -> anyhow::Result<()> {
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

    let base_ignores = format!(
        r#"
{marker}
.env.local
.nexus/credentials.toml
opencode.json
{agentic_root}/CLAUDE.md
"#,
        marker = marker,
        agentic_root = agentic_root,
    );

    // Build shadow ignores: exclude all Nexus-managed AI scaffold files.
    // When using an alternate agentic_root (e.g. ".nexus"), we must NOT
    // exclude .claude/ — that belongs to the customer's existing config.
    let shadow_ignores = if agentic_root == ".claude" {
        format!(
            r#"
{marker}
.env.local
.nexus/
.claude/
.opencode/
opencode.json
AGENTS.md
"#,
            marker = marker,
        )
    } else {
        // Alternate root: exclude .nexus/ (config) + agentic_root/ + .opencode/
        // but NOT .claude/ (customer-owned) and NOT root AGENTS.md (may be customer-owned)
        let mut entries = format!(
            r#"
{marker}
.env.local
.nexus/
.opencode/
opencode.json
"#,
            marker = marker,
        );
        // Only add agentic_root if it differs from .nexus (avoid duplicate)
        if agentic_root != ".nexus" {
            entries.push_str(&format!("{}/\n", agentic_root));
        }
        // AGENTS.md lives inside the agentic root for alternate paths
        entries.push_str(&format!("{}/AGENTS.md\n", agentic_root));
        entries
    };

    let ignores = if shadowed_ai {
        shadow_ignores
    } else {
        base_ignores
    };

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;

    use std::io::Write;
    file.write_all(ignores.as_bytes())?;

    if shadowed_ai {
        print_created(".git/info/exclude (appended, --shadowed-ai: all AI files excluded)");
    } else {
        print_created(".git/info/exclude (appended)");
    }

    Ok(())
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
fn write_agent_file(target: &Path, af: &nexus_core::api::ExportedAgentFile) -> anyhow::Result<()> {
    // Path traversal protection: reject target_path with parent-dir components
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

    Ok(())
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

            let opencode_json = format!(
                r#"{{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {{
    "nexus": {{
      "type": "local",
      {command_block},
      "environment": {{
        "NEXUS_API_URL": "{api_url}",
        "NEXUS_PRIVATE_TOKEN": "{token}"
      }}
    }}
  }}
}}
"#,
                command_block = command_block,
                api_url = api_url,
                token = token,
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

/// Print the completion message.
fn print_done() {
    println!();
    println!(
        "{} Nexus workspace initialized successfully.",
        style("OK").bold().green()
    );
    println!();
    println!("Next steps:");
    println!("  1. Run 'nexus login' to authenticate (if not already done)");
    println!("  2. Review AGENTS.md and adjust agent roles");
    println!("  3. Review .claude/skills/ for pulled skill definitions");
    println!("  4. Start your coding agent -- skills and MCP are ready");
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
            false,
        )
        .await
        .unwrap();

        assert!(dir.join(".nexus/config.toml").exists());
        assert!(dir.join(".claude/CLAUDE.md").exists());
        assert!(dir.join(".claude/skills").is_dir());
        assert!(dir.join(".opencode/commands").is_dir());
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join(".git/info/exclude").exists());

        // Verify config content
        let config = fs::read_to_string(dir.join(".nexus/config.toml")).unwrap();
        assert!(config.contains("test-project"));
        assert!(config.contains("# id ="));

        // Verify AGENTS.md content
        let agents = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        )
        .await
        .unwrap();

        let agents = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
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
            false,
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
            false,
        )
        .await
        .unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        let marker_count = exclude.matches("# Nexus CLI").count();
        assert_eq!(marker_count, 1, "exclude marker should appear only once");

        // Verify that opencode.json and .claude/CLAUDE.md are in .git/info/exclude
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be in .git/info/exclude"
        );
        assert!(
            exclude.contains(".claude/CLAUDE.md"),
            ".claude/CLAUDE.md must be in .git/info/exclude"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_shadowed_ai_exclude() {
        let dir = temp_project_dir("shadowed-ai");
        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.gatewarden.eu",
            false,
            McpSource::Npm,
            true,
        )
        .await
        .unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains("AGENTS.md"),
            "AGENTS.md must be in .git/info/exclude with --shadowed-ai"
        );
        assert!(
            exclude.contains(".claude/"),
            ".claude/ must be in .git/info/exclude with --shadowed-ai"
        );
        assert!(
            exclude.contains(".opencode/"),
            ".opencode/ must be in .git/info/exclude with --shadowed-ai"
        );
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be in .git/info/exclude with --shadowed-ai"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be in .git/info/exclude with --shadowed-ai"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shadow_exclude_default_claude_root() {
        let dir = temp_project_dir("shadow-claude-root");

        append_gitignore(&dir, true, ".claude").unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains(".claude/"),
            ".claude/ must be excluded in shadow mode with default root"
        );
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be excluded in shadow mode"
        );
        assert!(
            exclude.contains(".opencode/"),
            ".opencode/ must be excluded in shadow mode"
        );
        assert!(
            exclude.contains("AGENTS.md"),
            "root AGENTS.md must be excluded in shadow mode with default root"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be excluded in shadow mode"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shadow_exclude_alternate_nexus_root() {
        let dir = temp_project_dir("shadow-nexus-root");

        append_gitignore(&dir, true, ".nexus").unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains(".nexus/"),
            ".nexus/ must be excluded in shadow mode"
        );
        assert!(
            exclude.contains(".opencode/"),
            ".opencode/ must be excluded in shadow mode"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be excluded in shadow mode"
        );
        assert!(
            exclude.contains(".nexus/AGENTS.md"),
            ".nexus/AGENTS.md must be excluded in shadow mode with alternate root"
        );
        // .claude/ must NOT be excluded — it belongs to the customer
        assert!(
            !exclude.contains(".claude/"),
            ".claude/ must NOT be excluded in shadow mode with alternate root"
        );
        // Root-level AGENTS.md must NOT be excluded (may be customer-owned)
        let standalone_agents = exclude.lines().any(|l| l.trim() == "AGENTS.md");
        assert!(
            !standalone_agents,
            "root AGENTS.md must NOT be excluded in shadow mode with alternate root"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shadow_exclude_custom_alternate_root() {
        let dir = temp_project_dir("shadow-custom-root");

        append_gitignore(&dir, true, ".myagent").unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains(".nexus/"), ".nexus/ must be excluded");
        assert!(
            exclude.contains(".myagent/"),
            "custom agentic root must be excluded"
        );
        assert!(
            exclude.contains(".myagent/AGENTS.md"),
            "AGENTS.md inside custom root must be excluded"
        );
        assert!(
            !exclude.contains(".claude/"),
            ".claude/ must NOT be excluded with custom alternate root"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_base_exclude_alternate_root() {
        let dir = temp_project_dir("base-nexus-root");

        append_gitignore(&dir, false, ".nexus").unwrap();

        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains(".nexus/CLAUDE.md"),
            "agentic_root/CLAUDE.md must be excluded in base mode"
        );
        assert!(
            exclude.contains("opencode.json"),
            "opencode.json must be excluded in base mode"
        );
        assert!(
            !exclude.contains(".claude/"),
            ".claude/ must NOT be in base excludes with alternate root"
        );

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
        // Must contain literal token and URL (not env var references)
        assert!(oc.contains("https://nexus.gatewarden.eu"));
        assert!(oc.contains("nxs_pat_test-token-1234567890"));
        assert!(!oc.contains("{env:"));

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
}
