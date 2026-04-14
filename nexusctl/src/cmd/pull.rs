//! The `nexus pull` command.
//!
//! Pulls skills, commands, and MCP configuration from the Nexus platform
//! into the current workspace. Requires a linked project (via `nexus link`
//! or `nexus init --project-id`) and valid authentication.
//!
//! This is the incremental sync counterpart to `nexus init`:
//! - `init` creates the full scaffold from scratch
//! - `pull` updates skills and commands in an existing workspace

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::Credentials;
use nexus_core::config;
use std::fs;
use std::path::Path;

/// Run the pull command.
pub async fn run(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // Resolve project ID from CLI flag or linked project
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

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

    let client = NexusClient::new(api_url, Some(token))?;

    // Verify identity
    let identity = client.get_identity().await?;
    println!(
        "   {} Authenticated as {}",
        style("+").bold().green(),
        style(&identity.email).bold()
    );

    // Export skills
    let export = client.export_skills(&project_id).await?;
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
        return Ok(());
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

    println!();
    println!("{} Pull complete.", style("OK").bold().green());

    Ok(())
}

/// Resolve a token from (1) NEXUS_PRIVATE_TOKEN env var, or (2) stored credentials.
fn resolve_token() -> Option<String> {
    if let Ok(token) = std::env::var("NEXUS_PRIVATE_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }
    match Credentials::load() {
        Ok(Some(creds)) => Some(creds.token),
        _ => None,
    }
}

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
