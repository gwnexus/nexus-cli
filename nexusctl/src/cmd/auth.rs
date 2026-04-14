//! Authentication commands: login, logout, status.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::{Credentials, TOKEN_PREFIX};

/// Interactive login flow.
///
/// Prompts the user for a personal access token, validates the format,
/// verifies it against the Nexus API, and stores the credentials.
pub async fn login(api_url: &str) -> anyhow::Result<()> {
    println!("{} Nexus authentication", style(">>").bold().cyan());
    println!();
    println!(
        "Enter your personal access token (starts with '{}').",
        TOKEN_PREFIX
    );
    println!("You can generate one at: {}/dashboard/settings", api_url);
    println!();

    // Read token from stdin
    let token = read_token_from_stdin()?;

    // Validate format
    Credentials::validate_token_format(&token)?;

    // Verify against API
    println!("{} Verifying token...", style(">>").bold().cyan());

    let client = NexusClient::new(api_url, Some(token.clone()))?;
    let auth_status = client
        .auth_status()
        .await
        .map_err(|e| anyhow::anyhow!("Token verification failed: {}", e))?;

    // Store credentials
    let creds = Credentials {
        token,
        expires_at: None,
    };
    creds.save()?;

    println!();
    println!(
        "{} Authenticated as {} ({})",
        style("OK").bold().green(),
        style(&auth_status.user.email).bold(),
        auth_status.user.platform_role,
    );

    Ok(())
}

/// Remove stored credentials.
pub fn logout() -> anyhow::Result<()> {
    Credentials::remove()?;
    println!("{} Credentials removed.", style("OK").bold().green());
    Ok(())
}

/// Display current authentication and workspace status.
pub async fn status(api_url: &str) -> anyhow::Result<()> {
    println!("{} Nexus Status", style(">>").bold().cyan());
    println!();

    // --- API URL ---
    println!("  API URL:  {}", style(api_url).dim());

    // --- Workspace ---
    let cwd = std::env::current_dir()?;
    let has_nexus_dir = cwd.join(".nexus").exists();
    if has_nexus_dir {
        println!("  Workspace: {}", cwd.display());
    } else {
        println!(
            "  Workspace: {} (no .nexus/ found)",
            style(cwd.display()).dim()
        );
    }
    println!();

    // --- Auth status ---
    let creds = Credentials::load()?;

    match creds {
        None => {
            println!(
                "  Auth:     {} Not authenticated",
                style("--").bold().yellow()
            );
            println!("            Run 'nexus login' to authenticate.");
        }
        Some(ref c) => {
            // Show token prefix
            let prefix = if c.token.len() > 16 {
                format!("{}...", &c.token[..16])
            } else {
                c.token.clone()
            };

            // Verify against API
            let client = NexusClient::new(api_url, Some(c.token.clone()))?;
            match client.auth_status().await {
                Ok(auth) => {
                    println!(
                        "  Auth:     {} {} ({})",
                        style("OK").bold().green(),
                        style(&auth.user.email).bold(),
                        auth.user.platform_role,
                    );
                    if let Some(ref name) = auth.user.display_name {
                        println!("            Name: {}", name);
                    }
                    println!("            Token: {}", style(prefix).dim());
                }
                Err(e) => {
                    println!(
                        "  Auth:     {} Token invalid: {}",
                        style("ERR").bold().red(),
                        e
                    );
                    println!("            Token: {}", style(prefix).dim());
                }
            }
        }
    }
    println!();

    // --- Linked project ---
    match nexus_core::config::load_linked_project(None)? {
        Some(project) => {
            println!(
                "  Project:  {} {} ({})",
                style("OK").bold().green(),
                style(&project.name).bold(),
                if !project.slug.is_empty() {
                    &project.slug
                } else {
                    "-"
                }
            );
            println!("            ID: {}", style(&project.id).dim());
        }
        None => {
            println!(
                "  Project:  {} No project linked",
                style("--").bold().yellow()
            );
            println!("            Run 'nexus link' to link this directory to a project.");
        }
    }

    Ok(())
}

/// Read a token from stdin (one line, trimmed).
fn read_token_from_stdin() -> anyhow::Result<String> {
    use std::io::{self, BufRead};
    print!("Token: ");
    // Flush to ensure prompt is visible
    use std::io::Write;
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let token = line.trim().to_string();

    if token.is_empty() {
        anyhow::bail!("No token provided");
    }

    Ok(token)
}
