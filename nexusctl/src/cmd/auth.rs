//! Authentication commands: login, logout, status.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::{
    mask_token, resolve_token_with_source, Credentials, TokenSource, TOKEN_PREFIX,
};
use nexus_core::error::Error as CoreError;

use super::display::status_line;

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

    let token = read_token()?;

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
    println!("{}", style("General").bold());
    println!();

    // --- API URL ---
    status_line(
        "API URL:",
        format_args!("{} ({})", style(api_url).dim(), style(api_url_source).dim()),
    );

    // --- Workspace ---
    let cwd = std::env::current_dir()?;
    let has_nexus_dir = cwd.join(".nexus").exists();
    let devbox = DevboxShell::detect(&cwd).label();
    if has_nexus_dir {
        status_line("Workspace:", format_args!("{}{devbox}", cwd.display()));
    } else {
        status_line(
            "Workspace:",
            format_args!("{} (no .nexus/ found){devbox}", style(cwd.display()).dim()),
        );
    }
    let tracked = super::shadow::tracked_excluded(&cwd);
    if !tracked.is_empty() {
        status_line(
            "",
            format_args!(
                "{} {} excluded path(s) still tracked by git ({}): stage with git add -u, or untrack with nexus pull --untrack-shadowed",
                style("!").bold().yellow(),
                tracked.len(),
                tracked.join(", ")
            ),
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
            status_line(
                "Auth:",
                format_args!("{} Not authenticated", style("--").bold().yellow()),
            );
            status_line("", "Run 'nexus login' to authenticate.");
        }
        Some(ref t) => {
            // Show the masked token + source
            let prefix = mask_token(t);
            let source = resolved
                .as_ref()
                .map(|(_, s)| *s)
                .unwrap_or(TokenSource::Global);
            let source_label = format!(" ({})", source);

            // Verify against API
            let client = NexusClient::new(api_url, Some(t.clone()))?;
            match client.auth_status().await {
                Ok(auth) => {
                    status_line(
                        "Auth:",
                        format_args!(
                            "{} {} ({})",
                            style("OK").bold().green(),
                            style(&auth.user.email).bold(),
                            auth.user.platform_role,
                        ),
                    );
                    if let Some(ref name) = auth.user.display_name {
                        status_line("", format_args!("Name: {}", name));
                    }
                    status_line(
                        "",
                        format_args!(
                            "Token: {}{}",
                            style(prefix).dim(),
                            style(source_label).dim()
                        ),
                    );
                    auth_valid = true;
                }
                Err(e) => {
                    status_line(
                        "Auth:",
                        format_args!("{} Token invalid: {}", style("ERR").bold().red(), e),
                    );
                    status_line(
                        "",
                        format_args!(
                            "Token: {}{}",
                            style(prefix).dim(),
                            style(source_label).dim()
                        ),
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
                status_line(
                    "Project:",
                    format_args!(
                        "{} {} ({}), unverified",
                        style("--").bold().yellow(),
                        style(&project.name).bold(),
                        if !project.slug.is_empty() {
                            &project.slug
                        } else {
                            "-"
                        }
                    ),
                );
                status_line("", format_args!("ID: {}", style(&project.id).dim()));
                if token.is_none() {
                    status_line(
                        "",
                        format_args!(
                            "Run 'nexus login' to verify this project exists at {}.",
                            api_url
                        ),
                    );
                } else {
                    status_line("", "Cannot verify: authentication failed above.");
                }
            } else {
                let client = NexusClient::new(api_url, token.clone())?;
                match client.get_project(&project.id).await {
                    Ok(_) => {
                        status_line(
                            "Project:",
                            format_args!(
                                "{} {} ({})",
                                style("OK").bold().green(),
                                style(&project.name).bold(),
                                if !project.slug.is_empty() {
                                    &project.slug
                                } else {
                                    "-"
                                }
                            ),
                        );
                        status_line("", format_args!("ID: {}", style(&project.id).dim()));
                    }
                    Err(CoreError::NotFound(_)) => {
                        status_line(
                            "Project:",
                            format_args!("{} Not found at {}", style("ERR").bold().red(), api_url),
                        );
                        status_line(
                            "",
                            format_args!(
                                "Configured: {} (id: {})",
                                style(&project.name).bold(),
                                style(&project.id).dim()
                            ),
                        );
                        status_line(
                            "",
                            "This workspace may be pointed at the wrong backend. \
                             Check NEXUS_API_URL / --api-url / 'nexus config show', \
                             or re-link with 'nexus link'.",
                        );
                    }
                    Err(CoreError::Forbidden(msg)) => {
                        status_line(
                            "Project:",
                            format_args!(
                                "{} Access denied at {}: {}",
                                style("ERR").bold().red(),
                                api_url,
                                msg
                            ),
                        );
                        status_line("", format_args!("ID: {}", style(&project.id).dim()));
                    }
                    Err(e) => {
                        status_line(
                            "Project:",
                            format_args!(
                                "{} Could not verify against {}: {}",
                                style("!").bold().yellow(),
                                api_url,
                                e
                            ),
                        );
                        status_line(
                            "",
                            format_args!(
                                "Local config: {} (id: {})",
                                style(&project.name).bold(),
                                style(&project.id).dim()
                            ),
                        );
                    }
                }
            }
        }
        None => {
            status_line(
                "Project:",
                format_args!("{} No project linked", style("--").bold().yellow()),
            );
            status_line("", "Run 'nexus link' to link this directory to a project.");
        }
    }

    Ok(())
}

/// Longest run of mask characters shown while typing; longer input adds
/// the character count instead, so the prompt never wraps.
const MASK_WIDTH: usize = 40;

/// Read the token: masked on a terminal, one plain line otherwise (for
/// `printf '%s' "$TOKEN" | nexus login`).
fn read_token() -> anyhow::Result<String> {
    use std::io::IsTerminal;
    let term = console::Term::stdout();
    let token = if std::io::stdin().is_terminal() && term.is_term() {
        read_masked(&term)?
    } else {
        read_line()?
    };
    if token.is_empty() {
        anyhow::bail!("No token provided");
    }
    Ok(token)
}

/// Prompt text for `len` typed characters.
fn masked_prompt(len: usize) -> String {
    let stars = "*".repeat(len.min(MASK_WIDTH));
    if len > MASK_WIDTH {
        format!("Token: {stars} ({len})")
    } else {
        format!("Token: {stars}")
    }
}

/// Read a token key by key without echo: each character shows as `*`,
/// Backspace deletes, Ctrl-U clears, Enter finishes. The final line shows
/// the masked token (prefix and last four characters).
fn read_masked(term: &console::Term) -> anyhow::Result<String> {
    use console::Key;
    let mut token = String::new();
    term.write_str(&masked_prompt(0))?;
    loop {
        match term.read_key()? {
            Key::Enter => break,
            Key::Backspace => {
                token.pop();
            }
            Key::Char('\u{15}') => token.clear(),
            Key::Char(c) if !c.is_control() => token.push(c),
            _ => continue,
        }
        term.clear_line()?;
        term.write_str(&masked_prompt(token.chars().count()))?;
    }
    let token = token.trim().to_string();
    term.clear_line()?;
    if token.is_empty() {
        term.write_line("Token:")?;
    } else {
        term.write_line(&format!("Token: {}", style(mask_token(&token)).dim()))?;
    }
    Ok(token)
}

/// Read one plain line from stdin (trimmed).
fn read_line() -> anyhow::Result<String> {
    use std::io::{self, BufRead, Write};
    print!("Token: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Whether `nexus status` runs inside a devbox shell, from the variables
/// devbox exports into it (`DEVBOX_SHELL_ENABLED`, `DEVBOX_PROJECT_ROOT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevboxShell {
    /// Inside the devbox shell of this workspace.
    Active,
    /// Inside the devbox shell of another project (its root).
    OtherProject(String),
    /// The workspace has a `devbox.json`, but no devbox shell is active.
    Inactive,
    /// No `devbox.json` and no devbox shell: nothing to report.
    NotUsed,
}

impl DevboxShell {
    pub(crate) fn detect(workspace: &std::path::Path) -> Self {
        Self::classify(
            workspace,
            workspace.join("devbox.json").exists(),
            std::env::var("DEVBOX_SHELL_ENABLED").ok().as_deref(),
            std::env::var("DEVBOX_PROJECT_ROOT").ok().as_deref(),
        )
    }

    fn classify(
        workspace: &std::path::Path,
        has_devbox_json: bool,
        enabled: Option<&str>,
        project_root: Option<&str>,
    ) -> Self {
        if enabled != Some("1") {
            return if has_devbox_json {
                Self::Inactive
            } else {
                Self::NotUsed
            };
        }
        let canonical = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        match project_root.filter(|r| !r.is_empty()) {
            Some(root)
                if !canonical(workspace).starts_with(canonical(std::path::Path::new(root))) =>
            {
                Self::OtherProject(root.to_string())
            }
            _ => Self::Active,
        }
    }

    /// Machine-readable state for `nexus status --json`.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::OtherProject(_) => "other_project",
            Self::Inactive => "inactive",
            Self::NotUsed => "none",
        }
    }

    /// Suffix for the `Workspace:` status line (empty when not used).
    fn label(&self) -> String {
        match self {
            Self::Active => format!("  {}", style("[devbox shell: active]").green()),
            Self::OtherProject(root) => format!(
                "  {}",
                style(format!("[devbox shell: active for {root}]")).yellow()
            ),
            Self::Inactive => format!(
                "  {}",
                style("[devbox shell: inactive, start with devbox shell]").yellow()
            ),
            Self::NotUsed => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_masked_prompt() {
        assert_eq!(masked_prompt(0), "Token: ");
        assert_eq!(masked_prompt(3), "Token: ***");
        assert_eq!(
            masked_prompt(MASK_WIDTH),
            format!("Token: {}", "*".repeat(MASK_WIDTH))
        );
        assert_eq!(
            masked_prompt(52),
            format!("Token: {} (52)", "*".repeat(MASK_WIDTH))
        );
    }

    #[test]
    fn test_devbox_shell_classify() {
        let ws = Path::new("/nonexistent/project");
        assert_eq!(
            DevboxShell::classify(ws, true, Some("1"), Some("/nonexistent/project")),
            DevboxShell::Active
        );
        // A subdirectory of the devbox project is still inside its shell.
        assert_eq!(
            DevboxShell::classify(ws, false, Some("1"), Some("/nonexistent")),
            DevboxShell::Active
        );
        // Older devbox without DEVBOX_PROJECT_ROOT: trust the shell flag.
        assert_eq!(
            DevboxShell::classify(ws, true, Some("1"), None),
            DevboxShell::Active
        );
        assert_eq!(
            DevboxShell::classify(ws, true, Some("1"), Some("/nonexistent/other")),
            DevboxShell::OtherProject("/nonexistent/other".into())
        );
        // Prefix of the name only is not the same project.
        assert_eq!(
            DevboxShell::classify(ws, true, Some("1"), Some("/nonexistent/proj")),
            DevboxShell::OtherProject("/nonexistent/proj".into())
        );
        assert_eq!(
            DevboxShell::classify(ws, true, None, None),
            DevboxShell::Inactive
        );
        assert_eq!(
            DevboxShell::classify(ws, true, Some("0"), Some("/nonexistent/project")),
            DevboxShell::Inactive
        );
        assert_eq!(
            DevboxShell::classify(ws, false, None, None),
            DevboxShell::NotUsed
        );
    }
}
