//! Authentication commands: login, logout, status.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::{resolve_token_with_source, Credentials, TokenSource, TOKEN_PREFIX};
use nexus_core::error::Error as CoreError;

/// Interactive login flow.
///
/// Prompts the user for a personal access token, validates the format,
/// verifies it against the Nexus API, and stores the credentials.
///
/// By default (`global == false`) the token is stored in the project-local
/// `.nexus/credentials.toml` in the current workspace, scoping it to this
/// project only. Pass `global == true` to write to the shared
/// `~/.config/nexus/credentials.toml` instead (mirrors `nexus config set
/// --global`), which never happens implicitly.
pub async fn login(api_url: &str, global: bool) -> anyhow::Result<()> {
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

    // Store credentials at the requested scope only.
    let path = if global {
        Credentials::global_path()?
    } else {
        Credentials::local_path(None)?
    };
    let creds = Credentials {
        token,
        expires_at: None,
    };
    creds.save_to(&path)?;

    println!();
    println!(
        "{} Authenticated as {} ({})",
        style("OK").bold().green(),
        style(&auth_status.user.email).bold(),
        auth_status.user.platform_role,
    );
    println!(
        "         Stored at {} scope: {}",
        if global { "global" } else { "local" },
        style(path.display()).dim()
    );

    Ok(())
}

/// Remove stored credentials.
///
/// By default (`global == false`) only the project-local
/// `.nexus/credentials.toml` in the current workspace is removed; other
/// projects' local credentials and the global credential store are never
/// touched. Pass `global == true` to remove the shared global credentials
/// instead.
pub fn logout(global: bool) -> anyhow::Result<()> {
    let path = if global {
        Credentials::global_path()?
    } else {
        Credentials::local_path(None)?
    };
    let existed = path.exists();
    Credentials::remove_at(&path)?;

    if existed {
        println!(
            "{} Credentials removed ({} scope).",
            style("OK").bold().green(),
            if global { "global" } else { "local" }
        );
    } else {
        println!(
            "{} No {} credentials found at {}.",
            style("--").bold().yellow(),
            if global { "global" } else { "local" },
            path.display()
        );
    }
    Ok(())
}

/// Display current authentication and workspace status.
///
/// `api_url_source` reports which layer supplied the effective API URL
/// (`flag`, `env`, `local`, `global`, `default`), so a silent env-mismatch
/// (e.g. project only exists on staging while `api_url` resolved to prod)
/// is diagnosable from the same line that shows the URL itself.
pub async fn status(api_url: &str, api_url_source: &str) -> anyhow::Result<()> {
    println!("{} Nexus Status", style(">>").bold().cyan());
    println!();

    // --- API URL ---
    println!(
        "  API URL:  {} ({})",
        style(api_url).dim(),
        style(api_url_source).dim()
    );

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
    // Resolve token via standard priority: env var > local .nexus/credentials.toml > global credentials.toml
    let resolved = resolve_token_with_source();
    let token = resolved.as_ref().map(|(t, _)| t.clone());
    let mut auth_valid = false;

    match token {
        None => {
            println!(
                "  Auth:     {} Not authenticated",
                style("--").bold().yellow()
            );
            println!("            Run 'nexus login' to authenticate.");
        }
        Some(ref t) => {
            // Show token prefix + source
            let prefix = if t.len() > 8 {
                format!("{}****...", &t[..8])
            } else {
                "****".to_string()
            };
            let source = resolved
                .as_ref()
                .map(|(_, s)| *s)
                .unwrap_or(TokenSource::Global);
            let source_label = format!(" ({})", source);

            // Verify against API
            let client = NexusClient::new(api_url, Some(t.clone()))?;
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
                    println!(
                        "            Token: {}{}",
                        style(prefix).dim(),
                        style(source_label).dim()
                    );
                    auth_valid = true;
                }
                Err(e) => {
                    println!(
                        "  Auth:     {} Token invalid: {}",
                        style("ERR").bold().red(),
                        e
                    );
                    println!(
                        "            Token: {}{}",
                        style(prefix).dim(),
                        style(source_label).dim()
                    );
                }
            }
        }
    }
    println!();

    // --- Linked project ---
    match nexus_core::config::load_linked_project(None)? {
        Some(project) => {
            // Verify the project actually exists on the configured api_url,
            // rather than just echoing back the local .nexus/config.toml
            // binding. Catches silent env-mismatch (e.g. api_url pointed at
            // prod while the project only exists on staging).
            if !auth_valid {
                // Auth already failed/absent above; skip a second network
                // call and don't imply a fresh check succeeded.
                println!(
                    "  Project:  {} {} ({}) — unverified",
                    style("--").bold().yellow(),
                    style(&project.name).bold(),
                    if !project.slug.is_empty() {
                        &project.slug
                    } else {
                        "-"
                    }
                );
                println!("            ID: {}", style(&project.id).dim());
                if token.is_none() {
                    println!(
                        "            Run 'nexus login' to verify this project exists at {}.",
                        api_url
                    );
                } else {
                    println!("            Cannot verify: authentication failed above.");
                }
            } else {
                let client = NexusClient::new(api_url, token.clone())?;
                match client.get_project(&project.id).await {
                    Ok(_) => {
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
                    Err(CoreError::NotFound(_)) => {
                        println!(
                            "  Project:  {} Not found at {}",
                            style("ERR").bold().red(),
                            api_url
                        );
                        println!(
                            "            Configured: {} (id: {})",
                            style(&project.name).bold(),
                            style(&project.id).dim()
                        );
                        println!(
                            "            This workspace may be pointed at the wrong backend. \
                             Check NEXUS_API_URL / --api-url / 'nexus config show', \
                             or re-link with 'nexus link'."
                        );
                    }
                    Err(CoreError::Forbidden(msg)) => {
                        println!(
                            "  Project:  {} Access denied at {}: {}",
                            style("ERR").bold().red(),
                            api_url,
                            msg
                        );
                        println!("            ID: {}", style(&project.id).dim());
                    }
                    Err(e) => {
                        println!(
                            "  Project:  {} Could not verify against {}: {}",
                            style("!").bold().yellow(),
                            api_url,
                            e
                        );
                        println!(
                            "            Local config: {} (id: {})",
                            style(&project.name).bold(),
                            style(&project.id).dim()
                        );
                    }
                }
            }
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
