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
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::McpSource;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Marker in YAML frontmatter indicating the file is managed by Nexus CLI.
/// Files without this marker are considered user-managed and will not be
/// overwritten by `nexus pull`.
const MANAGED_MARKER: &str = "source: nexus-platform";

/// Run the pull command.
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    force: bool,
    mcp_source: McpSource,
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

    // Verify identity
    let identity = client.get_identity().await?;
    println!(
        "   {} Authenticated as {}",
        style("+").bold().green(),
        style(&identity.email).bold()
    );

    // Export skills
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
        let existing = detect_existing_files(&workspace, &export.skills);
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
        fs::create_dir_all(workspace.join(".claude/skills"))?;
        fs::create_dir_all(workspace.join(".opencode/commands"))?;

        // Write skills and commands
        let mut written = 0;
        for skill in &export.skills {
            write_skill(&workspace, skill)?;
            write_command(&workspace, skill)?;
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
                let directives_path = workspace.join(".claude/directives.md");
                if directives_path.exists() && !force {
                    if !is_managed_file(&directives_path) {
                        println!(
                            "   {} .claude/directives.md is user-managed (no nexus-platform marker), skipping",
                            style("--").yellow()
                        );
                    } else {
                        // Managed file, overwrite silently on pull
                        write_directives(&workspace, &dir_export.directives)?;
                        println!(
                            "   {} {} directive(s) synced",
                            style("+").bold().green(),
                            dir_export.directives.len()
                        );
                    }
                } else {
                    write_directives(&workspace, &dir_export.directives)?;
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
    match client.export_agent_files(&project_id).await {
        Ok(af_export) => {
            if af_export.agent_files.is_empty() {
                println!(
                    "   {} No agent files configured for this project.",
                    style("--").yellow()
                );
            } else {
                let mut af_written = 0;
                for af in &af_export.agent_files {
                    let target_path = workspace.join(&af.target_path);

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
        Err(e) => {
            // Fallback to hardcoded templates when af_export is unavailable
            println!(
                "   {} Agent file export not available ({}), using local templates",
                style("!").bold().yellow(),
                e
            );
            sync_claude_md(&workspace, &project_name, has_directives, force)?;
            sync_agents_md(&workspace, &project_name, force)?;
        }
    }

    // Resolve tool flavor from project details
    let tool_flavor = client
        .get_project(&project_id)
        .await
        .ok()
        .and_then(|d| d.project.agent_owner);

    // Write MCP server configs if they don't exist yet
    write_mcp_configs_if_missing(
        &workspace,
        api_url,
        &token,
        mcp_source,
        tool_flavor.as_deref(),
    )?;

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
) -> anyhow::Result<()> {
    let claude_dir = workspace.join(".claude");
    fs::create_dir_all(&claude_dir)?;

    let path = claude_dir.join("CLAUDE.md");

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} .claude/CLAUDE.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow()
        );
        return Ok(());
    }

    let content = render_claude_md(project_name, has_directives);
    fs::write(&path, content)?;
    print_synced(".claude/CLAUDE.md");

    Ok(())
}

/// Sync `AGENTS.md` — the agent role definition file.
///
/// - If the file does not exist → create it
/// - If it exists and has the nexus-platform marker → overwrite
/// - If it exists without the marker → user-managed, skip (warn)
fn sync_agents_md(workspace: &Path, project_name: &str, force: bool) -> anyhow::Result<()> {
    let path = workspace.join("AGENTS.md");

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} AGENTS.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow()
        );
        return Ok(());
    }

    let content = render_agents_md(project_name);
    fs::write(&path, content)?;
    print_synced("AGENTS.md");

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
pub fn render_claude_md(project_name: &str, has_directives: bool) -> String {
    let directives_step = if has_directives {
        "\n3. Load project directives from `.claude/directives.md`"
    } else {
        ""
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
) -> Vec<String> {
    let mut existing = Vec::new();
    for skill in skills {
        let skill_path = workspace
            .join(".claude/skills")
            .join(&skill.skill_id)
            .join("SKILL.md");
        if skill_path.exists() {
            existing.push(format!(".claude/skills/{}/SKILL.md", skill.skill_id));
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

/// Write MCP server configs (`opencode.json`, `.claude/mcp.json`) if they
/// don't already exist. This ensures `nexus pull` can bootstrap a workspace
/// that was initialized before MCP config generation was added.
fn write_mcp_configs_if_missing(
    workspace: &Path,
    api_url: &str,
    token: &str,
    mcp_source: McpSource,
    tool_flavor: Option<&str>,
) -> anyhow::Result<()> {
    let opencode_path = workspace.join("opencode.json");
    let claude_mcp_path = workspace.join(".claude").join("mcp.json");

    let skip_opencode = matches!(tool_flavor, Some("claude-cli"));
    let skip_claude = matches!(tool_flavor, Some("opencode"));

    // Nothing to do if all relevant configs already exist
    let opencode_done = skip_opencode || opencode_path.exists();
    let claude_done = skip_claude || claude_mcp_path.exists();
    if opencode_done && claude_done {
        return Ok(());
    }

    let source_label = match mcp_source {
        McpSource::Npm => "npm (@gwdn/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    if !skip_opencode && !opencode_path.exists() {
        let command_block = match mcp_source {
            McpSource::Npm => r#""command": ["npx", "@gwdn/nexus-mcp"]"#,
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

    if !skip_claude && !claude_mcp_path.exists() {
        let (cmd, args) = match mcp_source {
            McpSource::Npm => ("npx", r#""@gwdn/nexus-mcp""#),
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
// File writers
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

    fs::write(skill_dir.join("SKILL.md"), content)?;
    print_synced(&format!(".claude/skills/{}/SKILL.md", skill.skill_id));

    Ok(())
}

/// Write an OpenCode command for a skill to `.opencode/commands/<slug>.md`.
fn write_command(target: &Path, skill: &nexus_core::api::ExportedSkill) -> anyhow::Result<()> {
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

Load the skill file at `.claude/skills/{skill_id}/SKILL.md` and follow its instructions.
"#,
        name = skill.name,
        skill_id = skill.skill_id,
        version = skill.version,
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
) -> anyhow::Result<()> {
    let claude_dir = target.join(".claude");
    fs::create_dir_all(&claude_dir)?;

    let content = render_directives_markdown(directives);

    let path = claude_dir.join("directives.md");
    fs::write(&path, content)?;
    print_synced(".claude/directives.md");

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
        }];

        let existing = detect_existing_files(&tmp, &skills);
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
        }];

        let existing = detect_existing_files(&tmp, &skills);
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

        write_directives(&tmp, &directives).unwrap();

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
        let md = render_claude_md("MyProject", true);
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
        let md = render_claude_md("MyProject", false);
        assert!(md.contains("source: nexus-platform"));
        assert!(!md.contains("directives"));
        assert!(md.contains("3. Review active planning"));
        assert!(md.contains("4. Continue with the active workstream"));
    }

    #[test]
    fn test_render_claude_md_environment_section() {
        let md = render_claude_md("Test", true);
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
        };

        write_agent_file(&tmp, &af).unwrap();

        let path = tmp.join(".claude/CLAUDE.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Bootstrap"));

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
    }

    // ── MCP config generation tests ────────────────────────────────────────

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

        write_mcp_configs_if_missing(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            McpSource::Npm,
            None,
        )
        .unwrap();

        // opencode.json must exist with literal values
        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"nexus\""));
        assert!(oc.contains("nxs_pat_pull-test-token"));
        assert!(oc.contains("https://nexus.gatewarden.eu"));
        assert!(oc.contains("npx"));
        assert!(!oc.contains("{env:"));

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

        write_mcp_configs_if_missing(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_should-not-appear",
            McpSource::Npm,
            None,
        )
        .unwrap();

        // Must NOT overwrite
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

        write_mcp_configs_if_missing(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_partial-token",
            McpSource::Npm,
            None,
        )
        .unwrap();

        // opencode.json untouched
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

        write_mcp_configs_if_missing(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-token",
            McpSource::Local,
            None,
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
}
