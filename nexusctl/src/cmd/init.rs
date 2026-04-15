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
//! +-- .gitignore (appended)    # Nexus-specific ignores
//! ```
//!
//! Both `opencode.json` and `.claude/CLAUDE.md` are user-managed files that
//! must NOT be committed to the repository. They are added to `.gitignore`
//! by this command and are only created if they do not already exist.
//!
//! When `--project-id` is provided and a valid token exists, the init command
//! becomes **server-aware**: it verifies the identity, exports skills from the
//! Nexus platform, and materializes them into the local workspace.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::Credentials;
use nexus_core::config;
use nexus_core::McpSource;
use std::fs;
use std::path::{Path, PathBuf};

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
    }
    println!();

    // Check if already initialized
    let nexus_dir = target.join(".nexus");
    if nexus_dir.exists() && !force {
        anyhow::bail!("Directory already contains .nexus/. Use --force to reinitialize.");
    }

    // -----------------------------------------------------------------------
    // Phase 1: Local scaffolding
    // -----------------------------------------------------------------------

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
    create_claude_dir(&target, project_name)?;
    create_opencode_dir(&target)?;
    create_agents_md(&target, project_name, force)?;
    append_gitignore(&target, shadowed_ai)?;

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
                    print_done();
                    return Ok(());
                }
            }

            // Export skills for this project
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
                        write_skill(&target, skill)?;
                        write_command(&target, skill)?;
                    }

                    // Write MCP server configs
                    write_mcp_configs(&target, project_name, api_url, mcp_source)?;
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

            // Export directives for this project
            match client.export_directives(pid).await {
                Ok(dir_export) => {
                    if dir_export.directives.is_empty() {
                        println!(
                            "   {} No directives for this project.",
                            style("--").yellow()
                        );
                    } else {
                        write_directives(&target, &dir_export.directives)?;
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
            match client.export_agent_files(pid).await {
                Ok(af_export) => {
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
        } else {
            println!(
                "   {} No token found. Skipping server sync.",
                style("--").yellow()
            );
            println!(
                "   Run 'nexus login' and then 'nexus init --force --project-id {}' again.",
                pid
            );
        }
    }

    print_done();
    Ok(())
}

/// Resolve a token from (1) NEXUS_PRIVATE_TOKEN env var, or (2) stored credentials.
fn resolve_token() -> Option<String> {
    // Env var takes precedence (useful for CI/CD and MCP servers)
    if let Ok(token) = std::env::var("NEXUS_PRIVATE_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }

    // Fall back to stored credentials
    match Credentials::load() {
        Ok(Some(creds)) => Some(creds.token),
        _ => None,
    }
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

    let project_id_line = match project_id {
        Some(id) => format!("id = \"{}\"", id),
        None => "# id = \"<set via --project-id>\"".to_string(),
    };

    let config_content = format!(
        r#"# Nexus project-local configuration
# Managed by `nexus init`. Safe to edit manually.

[project]
name = "{name}"
{project_id_line}

[mcp]
# MCP server binary (resolved relative to project root)
# server_cmd = "node"
# server_args = ["tools/nexus-mcp/dist/server.js"]
"#,
        name = project_name,
        project_id_line = project_id_line,
    );

    fs::write(nexus_dir.join("config.toml"), config_content)?;
    print_created(".nexus/config.toml");

    Ok(())
}

/// Create .claude/ directory with instruction template and skills folder.
///
/// CLAUDE.md is only written if it does not already exist (it is user-managed
/// and excluded from version control via .gitignore).
fn create_claude_dir(target: &Path, project_name: &str) -> anyhow::Result<()> {
    let claude_dir = target.join(".claude");
    fs::create_dir_all(claude_dir.join("skills"))?;

    let claude_md_path = claude_dir.join("CLAUDE.md");
    if claude_md_path.exists() {
        println!(
            "   {} .claude/CLAUDE.md already exists, skipping",
            style("--").yellow()
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
        print_created(".claude/CLAUDE.md");
    }

    print_created(".claude/skills/");

    Ok(())
}

/// Create .opencode/ directory with commands subfolder.
fn create_opencode_dir(target: &Path) -> anyhow::Result<()> {
    let opencode_dir = target.join(".opencode");
    fs::create_dir_all(opencode_dir.join("commands"))?;
    print_created(".opencode/commands/");
    Ok(())
}

/// Create AGENTS.md with a template agent definition.
fn create_agents_md(target: &Path, project_name: &str, force: bool) -> anyhow::Result<()> {
    let agents_path = target.join("AGENTS.md");
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
    print_created("AGENTS.md");

    Ok(())
}

/// Append Nexus-specific entries to .gitignore if not already present.
///
/// When `shadowed_ai` is true, ALL AI scaffold files are added to .gitignore
/// (AGENTS.md, .claude/, .opencode/, opencode.json). Without this flag, only
/// sensitive/user-managed files are ignored (.env.local, credentials, opencode.json,
/// CLAUDE.md).
fn append_gitignore(target: &Path, shadowed_ai: bool) -> anyhow::Result<()> {
    let gitignore_path = target.join(".gitignore");
    let marker = "# Nexus CLI";

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
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
.claude/CLAUDE.md
"#,
        marker = marker,
    );

    let shadow_ignores = format!(
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
    );

    let ignores = if shadowed_ai {
        shadow_ignores
    } else {
        base_ignores
    };

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)?;

    use std::io::Write;
    file.write_all(ignores.as_bytes())?;

    if shadowed_ai {
        print_created(".gitignore (appended, --shadowed-ai: all AI files ignored)");
    } else {
        print_created(".gitignore (appended)");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2 helpers: Server-aware init
// ---------------------------------------------------------------------------

/// Write a skill definition to `.claude/skills/<skill_id>/SKILL.md`.
fn write_skill(target: &Path, skill: &nexus_core::api::ExportedSkill) -> anyhow::Result<()> {
    let skill_dir = target.join(".claude").join("skills").join(&skill.skill_id);
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
    print_created(&format!(".claude/skills/{}/SKILL.md", skill.skill_id));

    Ok(())
}

/// Write an OpenCode command for a skill to `.opencode/commands/<slug>.md`.
fn write_command(target: &Path, skill: &nexus_core::api::ExportedSkill) -> anyhow::Result<()> {
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

Load the skill file at `.claude/skills/{skill_id}/SKILL.md` and follow its instructions.
"#,
        name = skill.name,
        skill_id = skill.skill_id,
        version = skill.version,
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
) -> anyhow::Result<()> {
    let claude_dir = target.join(".claude");
    fs::create_dir_all(&claude_dir)?;

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

    let path = claude_dir.join("directives.md");
    fs::write(&path, format!("{}\n", content.trim_end()))?;
    print_created(".claude/directives.md");

    Ok(())
}

/// Write a single agent file exported from the platform to its `target_path`.
///
/// Creates any intermediate directories as needed.
/// The file body is already template-substituted by the server.
fn write_agent_file(target: &Path, af: &nexus_core::api::ExportedAgentFile) -> anyhow::Result<()> {
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
/// `@mpowr/nexus-mcp` package via npx. When `Local`, it points to the
/// local checkout at `tools/nexus-mcp/dist/server.js`.
///
/// Generates both `opencode.json` (OpenCode) and `.claude/mcp.json` (Claude Code).
/// Skips writing each file if it already exists (user-managed).
fn write_mcp_configs(
    target: &Path,
    _project_name: &str,
    _api_url: &str,
    mcp_source: McpSource,
) -> anyhow::Result<()> {
    let source_label = match mcp_source {
        McpSource::Npm => "npm (@mpowr/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    // --- OpenCode config (opencode.json) ---
    let opencode_path = target.join("opencode.json");

    if opencode_path.exists() {
        println!(
            "   {} opencode.json already exists, skipping",
            style("--").yellow()
        );
    } else {
        let command_block = match mcp_source {
            McpSource::Npm => r#""command": ["npx", "@mpowr/nexus-mcp"]"#,
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
        "NEXUS_API_URL": "{{env:NEXUS_API_URL}}",
        "NEXUS_PRIVATE_TOKEN": "{{env:NEXUS_PRIVATE_TOKEN}}"
      }}
    }}
  }}
}}
"#,
            command_block = command_block,
        );

        fs::write(&opencode_path, opencode_json)?;
        println!(
            "   {} opencode.json (MCP source: {})",
            style("+").bold().green(),
            source_label,
        );
    }

    // --- Claude Code config (.claude/mcp.json) ---
    let claude_mcp_path = target.join(".claude").join("mcp.json");

    if claude_mcp_path.exists() {
        println!(
            "   {} .claude/mcp.json already exists, skipping",
            style("--").yellow()
        );
    } else {
        let (cmd, args) = match mcp_source {
            McpSource::Npm => ("npx", r#""@mpowr/nexus-mcp""#),
            McpSource::Local => ("node", r#""tools/nexus-mcp/dist/server.js""#),
        };

        let claude_mcp_json = format!(
            r#"{{
  "mcpServers": {{
    "nexus": {{
      "command": "{cmd}",
      "args": [{args}],
      "env": {{
        "NEXUS_API_URL": "https://nexus.mpowr.tech",
        "NEXUS_PRIVATE_TOKEN": "${{NEXUS_PRIVATE_TOKEN}}"
      }}
    }}
  }}
}}
"#,
            cmd = cmd,
            args = args,
        );

        // .claude/ directory should already exist from earlier init steps
        if let Some(parent) = claude_mcp_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&claude_mcp_path, claude_mcp_json)?;
        println!(
            "   {} .claude/mcp.json (MCP source: {})",
            style("+").bold().green(),
            source_label,
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

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
    fn temp_project_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_init_creates_structure() {
        let dir = temp_project_dir("creates-structure");
        run(
            dir.to_str().unwrap(),
            Some("test-project"),
            None,
            "https://nexus.mpowr.tech",
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
        assert!(dir.join(".gitignore").exists());

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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
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
            "https://nexus.mpowr.tech",
            true,
            McpSource::Npm,
            false,
        )
        .await
        .unwrap();

        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let marker_count = gitignore.matches("# Nexus CLI").count();
        assert_eq!(marker_count, 1, "gitignore marker should appear only once");

        // Verify that opencode.json and .claude/CLAUDE.md are in .gitignore
        assert!(
            gitignore.contains("opencode.json"),
            "opencode.json must be in .gitignore"
        );
        assert!(
            gitignore.contains(".claude/CLAUDE.md"),
            ".claude/CLAUDE.md must be in .gitignore"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_shadowed_ai_gitignore() {
        let dir = temp_project_dir("shadowed-ai");
        run(
            dir.to_str().unwrap(),
            Some("test"),
            None,
            "https://nexus.mpowr.tech",
            false,
            McpSource::Npm,
            true,
        )
        .await
        .unwrap();

        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            gitignore.contains("AGENTS.md"),
            "AGENTS.md must be in .gitignore with --shadowed-ai"
        );
        assert!(
            gitignore.contains(".claude/"),
            ".claude/ must be in .gitignore with --shadowed-ai"
        );
        assert!(
            gitignore.contains(".opencode/"),
            ".opencode/ must be in .gitignore with --shadowed-ai"
        );
        assert!(
            gitignore.contains(".nexus/"),
            ".nexus/ must be in .gitignore with --shadowed-ai"
        );
        assert!(
            gitignore.contains("opencode.json"),
            "opencode.json must be in .gitignore with --shadowed-ai"
        );

        // Cleanup
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

        write_skill(&dir, &skill).unwrap();

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

        write_command(&dir, &skill).unwrap();

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

        write_command(&dir, &skill).unwrap();

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
            "https://nexus.mpowr.tech",
            McpSource::Npm,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"nexus\""));
        // Must use npx command for npm mode
        assert!(oc.contains("@mpowr/nexus-mcp"));
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
        // Must use {env:VAR} syntax
        assert!(oc.contains("{env:NEXUS_API_URL}"));

        // .mcp.json must NOT be created (legacy root-level format)
        assert!(!dir.join(".mcp.json").exists());

        // .claude/mcp.json MUST be created
        let cm = fs::read_to_string(dir.join(".claude/mcp.json")).unwrap();
        assert!(cm.contains("\"mcpServers\""));
        assert!(cm.contains("@mpowr/nexus-mcp"));
        assert!(cm.contains("NEXUS_PRIVATE_TOKEN"));
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
            "https://nexus.mpowr.tech",
            McpSource::Local,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Must use local command
        assert!(oc.contains("tools/nexus-mcp/dist/server.js"));
        assert!(!oc.contains("npx"));
        assert!(!oc.contains("@mpowr/nexus-mcp"));

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
            "https://nexus.mpowr.tech",
            McpSource::Npm,
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
