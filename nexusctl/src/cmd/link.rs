//! The `nexus link` and `nexus unlink` commands.
//!
//! `link` associates the current directory with a Nexus project by writing
//! the project info to `.nexus/config.toml`.
//!
//! `unlink` removes the `[project]` section from `.nexus/config.toml`.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::Credentials;
use nexus_core::config::{self, ProjectConfig, ProjectInfo};

/// Run the `nexus link` command.
///
/// If `--project-id` is provided, validates it against the API and links directly.
/// Otherwise, lists available projects and lets the user pick interactively.
pub async fn link(api_url: &str, project_id: Option<&str>) -> anyhow::Result<()> {
    println!(
        "{} Link directory to a Nexus project",
        style(">>").bold().cyan()
    );
    println!();

    // Require authentication
    let token = resolve_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    // Verify identity first
    let identity = client.get_identity().await.map_err(|e| {
        anyhow::anyhow!(
            "Authentication failed: {}. Run 'nexus login' first.",
            e
        )
    })?;
    println!(
        "   {} Authenticated as {}",
        style("+").bold().green(),
        style(&identity.email).bold()
    );

    let selected_project = if let Some(pid) = project_id {
        // Direct link: validate the project exists and user has access
        println!(
            "   Validating project {}...",
            style(pid).dim()
        );
        let resp = client.get_project(pid).await.map_err(|e| {
            anyhow::anyhow!(
                "Cannot access project '{}': {}",
                pid,
                e
            )
        })?;
        resp.project
    } else {
        // Interactive: list projects and let user pick
        let resp = client.list_projects().await.map_err(|e| {
            anyhow::anyhow!("Failed to list projects: {}", e)
        })?;

        if resp.projects.is_empty() {
            anyhow::bail!("No projects found. Create a project in the Nexus dashboard first.");
        }

        println!();
        println!("   Available projects:");
        println!();
        for (i, p) in resp.projects.iter().enumerate() {
            let slug_display = p.slug.as_deref().unwrap_or("-");
            println!(
                "   {}  {} ({})",
                style(format!("[{}]", i + 1)).bold(),
                style(&p.name).bold(),
                style(slug_display).dim()
            );
        }
        println!();

        // Read selection from stdin
        let selection = read_selection(resp.projects.len())?;
        resp.projects[selection].clone()
    };

    // Write to .nexus/config.toml
    let project_info = ProjectInfo {
        id: selected_project.id.clone(),
        name: selected_project.name.clone(),
        slug: selected_project
            .slug
            .clone()
            .unwrap_or_default(),
    };

    // Load existing config or create new
    let mut project_config = config::load_project_config(None)?
        .unwrap_or_else(|| ProjectConfig {
            project: None,
            mcp: None,
        });
    project_config.project = Some(project_info);
    config::save_project_config(None, &project_config)?;

    println!();
    println!(
        "{} Linked to project: {} ({})",
        style("OK").bold().green(),
        style(&selected_project.name).bold(),
        &selected_project.id
    );

    Ok(())
}

/// Run the `nexus unlink` command.
///
/// Removes the `[project]` section from `.nexus/config.toml`.
/// Does NOT delete the `.nexus/` directory.
pub fn unlink() -> anyhow::Result<()> {
    let removed = config::remove_project_section(None)?;

    if removed {
        println!(
            "{} Project unlinked. The .nexus/ directory has been preserved.",
            style("OK").bold().green()
        );
    } else {
        println!(
            "{} No project is currently linked.",
            style("--").bold().yellow()
        );
    }

    Ok(())
}

/// Resolve a token from env var or stored credentials.
fn resolve_token() -> anyhow::Result<String> {
    if let Ok(token) = std::env::var("NEXUS_PRIVATE_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }

    match Credentials::load()? {
        Some(creds) => Ok(creds.token),
        None => anyhow::bail!(
            "Not authenticated. Run 'nexus login' first."
        ),
    }
}

/// Read a numeric selection from stdin (1-indexed).
fn read_selection(max: usize) -> anyhow::Result<usize> {
    use std::io::{self, BufRead, Write};

    print!("   Select project [1-{}]: ", max);
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();

    let num: usize = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid selection: '{}'", trimmed))?;

    if num < 1 || num > max {
        anyhow::bail!("Selection out of range: {} (expected 1-{})", num, max);
    }

    Ok(num - 1)
}
