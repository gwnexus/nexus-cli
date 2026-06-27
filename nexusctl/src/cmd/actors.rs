//! The `nexus actors` command group.
//!
//! Provides subcommands for listing, viewing, and managing actor profiles
//! assigned to the linked Nexus project.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;

/// List actors assigned to the project.
pub async fn list(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;
    let response = client.list_actors(&project_id).await?;

    if response.actors.is_empty() {
        println!(
            "   {} No actors assigned to this project.",
            style("·").dim()
        );
        return Ok(());
    }

    println!(
        "{} Actors assigned to project ({}):",
        style(">>").bold().cyan(),
        response.count
    );
    println!();

    // Header
    println!(
        "   {:<20} {:<24} {:<16} {}",
        style("SLUG").bold().underlined(),
        style("NAME").bold().underlined(),
        style("ROLE").bold().underlined(),
        style("STATUS").bold().underlined(),
    );

    for actor in &response.actors {
        let status = actor.status.as_deref().unwrap_or("active");
        let status_styled = match status {
            "active" => style(status).green(),
            "inactive" => style(status).yellow(),
            _ => style(status).dim(),
        };

        println!(
            "   {:<20} {:<24} {:<16} {}",
            style(&actor.slug).cyan(),
            &actor.name,
            &actor.role,
            status_styled,
        );
    }

    println!();
    Ok(())
}

/// Show full actor profile.
pub async fn show(api_url: &str, slug: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;
    let response = client.get_actor(&project_id, slug).await?;
    let actor = &response.actor;

    println!(
        "{} Actor: {} ({})",
        style(">>").bold().cyan(),
        style(&actor.name).bold(),
        style(&actor.slug).dim(),
    );
    println!();
    println!("   Role:        {}", &actor.role);
    if let Some(ref desc) = actor.description {
        println!("   Description: {}", desc);
    }
    if let Some(ref model) = actor.model_routing {
        println!("   Model:       {}", model);
    }
    if let Some(ref status) = actor.status {
        println!("   Status:      {}", status);
    }
    if let Some(ref avatar) = actor.avatar {
        if let Some(ref avatar_style) = avatar.style {
            println!("   Avatar:      {}", avatar_style);
        }
        if let Some(ref url) = avatar.url {
            println!("   Avatar URL:  {}", style(url).dim());
        }
    }
    println!("   Created:     {}", &actor.created_at);
    println!("   Updated:     {}", &actor.updated_at);

    // Show profile body if available
    if let Some(ref body) = actor.profile_body {
        println!();
        println!("{}", style("--- Profile ---").bold());
        println!();
        println!("{}", body);
    }

    println!();
    Ok(())
}

/// Trigger avatar regeneration for an actor.
pub async fn avatar_generate(
    api_url: &str,
    slug: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;
    let response = client.generate_actor_avatar(&project_id, slug).await?;

    println!(
        "   {} Avatar regenerated for '{}'",
        style("+").bold().green(),
        slug
    );

    if let Some(ref avatar) = response.avatar {
        if let Some(ref url) = avatar.url {
            println!("   URL: {}", style(url).dim());
        }
    }
    if let Some(ref msg) = response.message {
        println!("   {}", msg);
    }

    Ok(())
}

/// Reset actor avatar to DiceBear default.
pub async fn avatar_reset(
    api_url: &str,
    slug: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;
    let response = client.reset_actor_avatar(&project_id, slug).await?;

    println!(
        "   {} Avatar reset to DiceBear default for '{}'",
        style("+").bold().green(),
        slug
    );

    if let Some(ref avatar) = response.avatar {
        if let Some(ref url) = avatar.url {
            println!("   URL: {}", style(url).dim());
        }
    }
    if let Some(ref msg) = response.message {
        println!("   {}", msg);
    }

    Ok(())
}
