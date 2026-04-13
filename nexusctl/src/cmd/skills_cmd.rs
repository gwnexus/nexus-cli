//! The `nexus skills export` command.
//!
//! Exports the enabled skills for a linked project as structured JSON to stdout.
//! Useful for piping to other tools or inspecting the skill configuration.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::Credentials;
use nexus_core::config;

/// Export skills for the linked project as JSON.
pub async fn export(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // Resolve project ID from CLI flag or linked project
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    // Resolve authentication token
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!(
            "No authentication token found. Run 'nexus login' first."
        )
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
