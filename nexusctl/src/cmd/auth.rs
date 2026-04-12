//! Authentication commands: login, logout, status.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::{Credentials, TOKEN_PREFIX};

/// Interactive login flow.
///
/// Prompts the user for a personal access token, validates the format,
/// verifies it against the Nexus API, and stores the credentials.
pub async fn login(api_url: &str) -> anyhow::Result<()> {
    println!(
        "{} Nexus authentication",
        style(">>").bold().cyan()
    );
    println!();
    println!(
        "Enter your personal access token (starts with '{}').",
        TOKEN_PREFIX
    );
    println!(
        "You can generate one at: {}/dashboard/settings",
        api_url
    );
    println!();

    // Read token from stdin
    let token = read_token_from_stdin()?;

    // Validate format
    Credentials::validate_token_format(&token)?;

    // Verify against API
    println!(
        "{} Verifying token...",
        style(">>").bold().cyan()
    );

    let client = NexusClient::new(api_url, Some(token.clone()))?;
    let auth_status = client.auth_status().await.map_err(|e| {
        anyhow::anyhow!("Token verification failed: {}", e)
    })?;

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
    println!(
        "{} Credentials removed.",
        style("OK").bold().green()
    );
    Ok(())
}

/// Display current authentication status.
pub async fn status(api_url: &str) -> anyhow::Result<()> {
    let creds = Credentials::load()?;

    match creds {
        None => {
            println!(
                "{} Not authenticated. Run 'nexus login' first.",
                style("--").bold().yellow()
            );
        }
        Some(ref c) => {
            // Show token prefix
            let prefix = if c.token.len() > 16 {
                format!("{}...", &c.token[..16])
            } else {
                c.token.clone()
            };
            println!(
                "{} Token: {}",
                style(">>").bold().cyan(),
                prefix
            );

            // Verify against API
            let client = NexusClient::new(api_url, Some(c.token.clone()))?;
            match client.auth_status().await {
                Ok(status) => {
                    println!(
                        "{} Authenticated as {} ({})",
                        style("OK").bold().green(),
                        style(&status.user.email).bold(),
                        status.user.platform_role,
                    );
                    if let Some(ref name) = status.user.display_name {
                        println!("   Name: {}", name);
                    }
                }
                Err(e) => {
                    println!(
                        "{} Token verification failed: {}",
                        style("ERR").bold().red(),
                        e
                    );
                }
            }
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
