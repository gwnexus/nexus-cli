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
use nexus_core::auth::Credentials;
use nexus_core::config;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Run the pull command.
pub async fn run(api_url: &str, cli_project_id: Option<&str>, force: bool) -> anyhow::Result<()> {
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
    match client.export_directives(&project_id).await {
        Ok(dir_export) => {
            if dir_export.directives.is_empty() {
                println!(
                    "   {} No directives for this project.",
                    style("--").yellow()
                );
            } else {
                // Check for existing directives file
                let directives_path = workspace.join(".claude/directives.md");
                if directives_path.exists() && !force {
                    println!();
                    println!(
                        "   {} {} already exists and will be overwritten.",
                        style("!").bold().yellow(),
                        style(".claude/directives.md").dim()
                    );
                    if !confirm_overwrite()? {
                        println!(
                            "   {} Directives skipped. Use {} to overwrite.",
                            style("--").yellow(),
                            style("--force").bold()
                        );
                        println!();
                        println!("{} Pull complete (directives skipped).", style("OK").bold().green());
                        return Ok(());
                    }
                }

                write_directives(&workspace, &dir_export.directives)?;
                println!(
                    "   {} {} directive(s) synced",
                    style("+").bold().green(),
                    dir_export.directives.len()
                );
            }
        }
        Err(e) => {
            println!(
                "   {} Could not fetch directives: {}",
                style("!").bold().yellow(),
                e
            );
        }
    }

    println!();
    println!("{} Pull complete.", style("OK").bold().green());

    Ok(())
}

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
    print!(
        "   {} Overwrite? [y/N] ",
        style("?").bold().cyan()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();

    Ok(answer == "y" || answer == "yes")
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
        categories
            .entry(d.category.clone())
            .or_default()
            .push(d);
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
        assert!(migration_pos < security_pos, "categories should be alphabetical");

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
}
