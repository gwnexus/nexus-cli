//! The `nexus skills` subcommands.
//!
//! - `nexus skills list`   -- list all skills for the current tenant
//! - `nexus skills export` -- export enabled skills for a linked project as JSON

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::OutputPreference;

/// List skills for the current tenant.
pub async fn list(
    api_url: &str,
    status_filter: Option<&str>,
    limit: Option<u32>,
    output: OutputPreference,
) -> anyhow::Result<()> {
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;

    // Parse comma-separated status filter
    let statuses: Option<Vec<String>> = status_filter.map(|s| {
        s.split(',')
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .collect()
    });

    let resp = client.list_skills(statuses.as_deref(), limit).await?;

    match output {
        OutputPreference::Json => {
            let json = serde_json::to_string_pretty(&resp)?;
            println!("{}", json);
        }
        _ => {
            println!(
                "{}\n",
                style(format!(">> Skills ({} total)", resp.count)).bold()
            );

            if resp.skills.is_empty() {
                println!("  No skills found.");
            } else {
                // Column headers
                println!(
                    "  {:<24} {:<30} {:<10} {:<4} {:<20}",
                    style("SKILL ID").underlined(),
                    style("NAME").underlined(),
                    style("STATUS").underlined(),
                    style("VER").underlined(),
                    style("COMMAND").underlined(),
                );

                for skill in &resp.skills {
                    let status_styled = match skill.status.as_str() {
                        "active" => style(&skill.status).green(),
                        "draft" => style(&skill.status).yellow(),
                        "archived" => style(&skill.status).dim(),
                        _ => style(&skill.status).white(),
                    };

                    let cmd = skill.command_slug.as_deref().unwrap_or("-");

                    // Truncate name to 28 chars to keep table aligned
                    let name_display = if skill.name.len() > 28 {
                        format!("{}...", skill.name.chars().take(25).collect::<String>())
                    } else {
                        skill.name.clone()
                    };

                    println!(
                        "  {:<24} {:<30} {:<10} {:>4} {:<20}",
                        style(&skill.skill_id).cyan(),
                        name_display,
                        status_styled,
                        skill.version,
                        cmd,
                    );
                }
            }

            println!();
        }
    }

    Ok(())
}

/// Export skills for the linked project as JSON.
pub async fn export(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // Resolve project ID from CLI flag or linked project
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    // Resolve authentication token
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;

    // Export skills
    let export = client.export_skills(&project_id).await?;

    // Print structured JSON to stdout
    let json = serde_json::to_string_pretty(&export)?;
    println!("{}", json);

    // Print summary to stderr so it doesn't interfere with JSON piping
    eprintln!(
        "{} {} skill(s) for project {} ({})",
        style("OK").bold().green(),
        export.count,
        style(&export.project.name).bold(),
        &export.project.slug
    );

    Ok(())
}
