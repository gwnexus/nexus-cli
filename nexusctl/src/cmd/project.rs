//! The `nexus project link` command group.
//!
//! Provisions and manages project inference tokens (`nxs_proj_*`) used to
//! authenticate CLIENT inference traffic against the Nexus Model Gateway
//! (gateway ADR-0005). The user PAT (`nxs_pat_*`) bootstraps issuance and
//! remains the CLI/MCP control-plane credential.
//!
//! Storage model (nexus-cli ADR: project inference token storage):
//! - Canonical secret store: `~/.config/nexus/project-tokens.toml` (mode 0600).
//! - Workspace exposure: `NEXUS_PROJECT_TOKEN` written to the gitignored
//!   `.env.nexus.local`, which OpenCode / `nexus run` load automatically.
//! - `NEXUS_PROJECT_TOKEN` in the environment always overrides the store (CI).
//!
//! Subcommands: `link`, `rotate`, `unlink`, `status`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use console::style;
use nexus_core::api::{InferenceTokenIssueRequest, NexusClient, ProfileCeiling};
use nexus_core::auth::{require_token, ProjectTokenEntry, ProjectTokenStore, PROJECT_TOKEN_ENV};
use nexus_core::config::{self, ProjectConfig, ProjectInfo};

/// Name of the gitignored workspace secrets file that exposes the token.
const ENV_LOCAL_FILE: &str = ".env.nexus.local";

/// Run `nexus project link` — issue and store a project inference token.
pub async fn link(
    api_url: &str,
    project_id: Option<&str>,
    runtime_id: Option<&str>,
    restrict_profiles: &[String],
    expires: Option<&str>,
) -> anyhow::Result<()> {
    println!(
        "{} Provision a Nexus project inference token",
        style(">>").bold().cyan()
    );
    println!();

    let token = require_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    // Resolve the target project (CLI flag > workspace link > interactive).
    let project = resolve_or_select_project(&client, project_id).await?;
    println!(
        "   {} Project: {} ({})",
        style("+").bold().green(),
        style(&project.name).bold(),
        style(&project.id).dim()
    );

    let runtime = match runtime_id {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => default_runtime_id(),
    };

    let expires_at = match expires {
        Some(raw) => Some(parse_expires(raw)?),
        None => None,
    };

    let profile_ceiling = if restrict_profiles.is_empty() {
        None
    } else {
        Some(ProfileCeiling {
            mode: "restrict".to_string(),
            profiles: restrict_profiles.to_vec(),
        })
    };

    let request = InferenceTokenIssueRequest {
        runtime_id: runtime.clone(),
        capabilities: None,
        profile_ceiling,
        budget_ceiling_ref: None,
        expires_at: expires_at.clone(),
    };

    println!("   Issuing token for runtime {}...", style(&runtime).bold());
    let issued = client
        .issue_inference_token(&project.id, &request)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to issue project token: {}", e))?;

    if let Some(ref warning) = issued.warning {
        println!("   {} {}", style("!").bold().yellow(), warning);
    }

    // Persist to the canonical store and expose to the workspace.
    persist_token(&project.id, &issued, &runtime, None)?;
    let workspace = std::env::current_dir()?;
    sync_env_token(&workspace, &issued.token)?;

    println!();
    println!(
        "{} Linked project token issued and stored.",
        style("OK").bold().green()
    );
    print_token_summary(&issued.token_prefix, &runtime, issued.expires_at.as_deref());
    println!(
        "   Exposed as {} in {} (gitignored).",
        style(PROJECT_TOKEN_ENV).bold(),
        style(ENV_LOCAL_FILE).dim()
    );

    Ok(())
}

/// Run `nexus project rotate` — issue a replacement token with overlap.
///
/// Without `--finalize`, the previous token stays valid (zero-downtime).
/// With `--finalize`, the previously superseded token is revoked.
pub async fn rotate(api_url: &str, project_id: Option<&str>, finalize: bool) -> anyhow::Result<()> {
    let token = require_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    let project_id = resolve_project_id(project_id)?;
    let mut store = ProjectTokenStore::load()?;
    let current = store.get(&project_id).cloned().ok_or_else(|| {
        anyhow::anyhow!("No project token stored for this project. Run 'nexus project link' first.")
    })?;

    if finalize {
        if let Some(prev_id) = current.previous_token_id.clone() {
            println!(
                "{} Finalizing rotation: revoking previous token {}...",
                style(">>").bold().cyan(),
                style(&prev_id).dim()
            );
            client
                .revoke_inference_token(&project_id, &prev_id)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to revoke previous token: {}", e))?;
            let mut updated = current;
            updated.previous_token_id = None;
            store.set(&project_id, updated);
            store.save()?;
            println!("{} Rotation finalized.", style("OK").bold().green());
        } else {
            println!(
                "{} No previous token pending finalization.",
                style("--").bold().yellow()
            );
        }
        return Ok(());
    }

    println!(
        "{} Rotating project inference token (previous stays valid until finalize)...",
        style(">>").bold().cyan()
    );
    let issued = client
        .rotate_inference_token(&project_id, &current.token_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to rotate project token: {}", e))?;

    if let Some(ref warning) = issued.warning {
        println!("   {} {}", style("!").bold().yellow(), warning);
    }

    let runtime = issued
        .runtime_id
        .clone()
        .unwrap_or_else(|| current.runtime_id.clone());
    // Keep the old token_id as the overlapping previous token.
    persist_token(
        &project_id,
        &issued,
        &runtime,
        Some(current.token_id.clone()),
    )?;
    let workspace = std::env::current_dir()?;
    sync_env_token(&workspace, &issued.token)?;

    println!();
    println!("{} Token rotated.", style("OK").bold().green());
    print_token_summary(&issued.token_prefix, &runtime, issued.expires_at.as_deref());
    println!(
        "   Previous token {} remains valid. Run '{}' to revoke it.",
        style(&current.token_id).dim(),
        style("nexus project rotate --finalize").bold()
    );

    Ok(())
}

/// Run `nexus project unlink` — revoke the token and clear local state.
pub async fn unlink(api_url: &str, project_id: Option<&str>) -> anyhow::Result<()> {
    let project_id = resolve_project_id(project_id)?;
    let mut store = ProjectTokenStore::load()?;
    let entry = match store.get(&project_id).cloned() {
        Some(e) => e,
        None => {
            println!(
                "{} No project token stored for this project.",
                style("--").bold().yellow()
            );
            return Ok(());
        }
    };

    // Best-effort revocation: clear local state even if the API call fails.
    let token = require_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    for token_id in [
        Some(entry.token_id.clone()),
        entry.previous_token_id.clone(),
    ]
    .into_iter()
    .flatten()
    {
        match client.revoke_inference_token(&project_id, &token_id).await {
            Ok(()) => println!(
                "   {} Revoked token {}",
                style("+").bold().green(),
                style(&token_id).dim()
            ),
            Err(e) => println!(
                "   {} Could not revoke {} ({}). Clearing locally anyway.",
                style("!").bold().yellow(),
                style(&token_id).dim(),
                e
            ),
        }
    }

    store.remove(&project_id);
    store.save()?;

    let workspace = std::env::current_dir()?;
    clear_env_token(&workspace)?;

    println!();
    println!(
        "{} Project token unlinked and cleared.",
        style("OK").bold().green()
    );

    Ok(())
}

/// Run `nexus project status` — list issued tokens for the linked project.
pub async fn status(api_url: &str, project_id: Option<&str>) -> anyhow::Result<()> {
    let project_id = resolve_project_id(project_id)?;
    let token = require_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    println!(
        "{} Project inference tokens for {}",
        style(">>").bold().cyan(),
        style(&project_id).dim()
    );
    println!();

    // Show which token is active locally.
    let store = ProjectTokenStore::load()?;
    match store.get(&project_id) {
        Some(entry) => {
            let prefix = entry.token_prefix.as_deref().unwrap_or("nxs_proj_****");
            println!(
                "   Local: {} runtime={} prefix={}",
                style("active").bold().green(),
                style(&entry.runtime_id).bold(),
                style(prefix).dim()
            );
        }
        None => {
            if std::env::var(PROJECT_TOKEN_ENV).is_ok_and(|v| !v.is_empty()) {
                println!(
                    "   Local: {} (from {} env var)",
                    style("active").bold().green(),
                    style(PROJECT_TOKEN_ENV).dim()
                );
            } else {
                println!(
                    "   Local: {} No token stored. Run 'nexus project link'.",
                    style("--").bold().yellow()
                );
            }
        }
    }
    println!();

    let tokens = client
        .list_inference_tokens(&project_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list project tokens: {}", e))?;

    if tokens.is_empty() {
        println!("   {} No tokens issued server-side.", style("--").dim());
        return Ok(());
    }

    println!("   Server-side tokens:");
    for t in &tokens {
        let status_label = match t.status.as_deref() {
            Some("active") => style("active").green(),
            Some("expired") => style("expired").yellow(),
            Some("revoked") => style("revoked").red(),
            Some(other) => style(other).dim(),
            None => style("unknown").dim(),
        };
        println!(
            "   - {} {} runtime={} created={} last_used={} expires={}",
            status_label,
            style(t.token_prefix.as_deref().unwrap_or("nxs_proj_****")).dim(),
            t.runtime_id.as_deref().unwrap_or("-"),
            t.created_at.as_deref().unwrap_or("-"),
            t.last_used_at.as_deref().unwrap_or("-"),
            t.expires_at.as_deref().unwrap_or("never"),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A resolved project (id + display name).
struct ResolvedProject {
    id: String,
    name: String,
}

/// Resolve the project ID for rotate/unlink/status (no interactive fallback).
fn resolve_project_id(cli_project_id: Option<&str>) -> anyhow::Result<String> {
    let workspace = std::env::current_dir()?;
    config::resolve_project_id(cli_project_id, Some(&workspace)).map_err(|e| anyhow::anyhow!(e))
}

/// Resolve the target project for `link`, writing the workspace link if needed.
///
/// Priority: explicit `--project-id` > existing `.nexus/config.toml` link >
/// interactive selection from the user's project memberships.
async fn resolve_or_select_project(
    client: &NexusClient,
    project_id: Option<&str>,
) -> anyhow::Result<ResolvedProject> {
    // 1. Explicit flag.
    if let Some(pid) = project_id.filter(|p| !p.is_empty()) {
        let resp = client
            .get_project(pid)
            .await
            .map_err(|e| anyhow::anyhow!("Cannot access project '{}': {}", pid, e))?;
        let slug = resp.project.slug.clone().unwrap_or_default();
        ensure_workspace_linked(&resp.project.id, &resp.project.name, &slug)?;
        return Ok(ResolvedProject {
            id: resp.project.id,
            name: resp.project.name,
        });
    }

    // 2. Existing workspace link (handles pre-existing project links).
    if let Some(linked) = config::load_linked_project(None)? {
        if !linked.id.is_empty() {
            return Ok(ResolvedProject {
                id: linked.id,
                name: linked.name,
            });
        }
    }

    // 3. Interactive selection.
    let resp = client
        .list_projects()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list projects: {}", e))?;
    if resp.projects.is_empty() {
        anyhow::bail!("No projects found. Create a project in the Nexus dashboard first.");
    }

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

    let selection = read_selection(resp.projects.len())?;
    let chosen = resp.projects[selection].clone();
    let slug = chosen.slug.clone().unwrap_or_default();
    ensure_workspace_linked(&chosen.id, &chosen.name, &slug)?;
    Ok(ResolvedProject {
        id: chosen.id,
        name: chosen.name,
    })
}

/// Ensure `.nexus/config.toml` records the project link (write if missing).
fn ensure_workspace_linked(id: &str, name: &str, slug: &str) -> anyhow::Result<()> {
    if let Some(existing) = config::load_linked_project(None)? {
        if existing.id == id {
            return Ok(());
        }
    }
    let mut project_config = config::load_project_config(None)?.unwrap_or(ProjectConfig {
        project: None,
        mcp: None,
        mcp_extra: None,
        plugins: None,
    });
    project_config.project = Some(ProjectInfo {
        id: id.to_string(),
        name: name.to_string(),
        slug: slug.to_string(),
    });
    config::save_project_config(None, &project_config)?;
    Ok(())
}

/// Persist an issued/rotated token into the canonical store.
fn persist_token(
    project_id: &str,
    issued: &nexus_core::api::InferenceTokenResponse,
    runtime_id: &str,
    previous_token_id: Option<String>,
) -> anyhow::Result<()> {
    let mut store = ProjectTokenStore::load()?;
    store.set(
        project_id,
        ProjectTokenEntry {
            token: issued.token.clone(),
            token_id: issued.token_id.clone(),
            token_prefix: issued.token_prefix.clone(),
            runtime_id: runtime_id.to_string(),
            expires_at: issued.expires_at.clone(),
            created_at: Some(unix_to_rfc3339(now_secs())),
            previous_token_id,
        },
    );
    store.save()?;
    Ok(())
}

/// Print a one-line non-secret token summary.
fn print_token_summary(prefix: &Option<String>, runtime: &str, expires_at: Option<&str>) {
    println!(
        "   {} prefix={} runtime={} expires={}",
        style("token").dim(),
        style(prefix.as_deref().unwrap_or("nxs_proj_****")).bold(),
        style(runtime).bold(),
        style(expires_at.unwrap_or("never")).dim()
    );
}

/// Read a 1-indexed numeric selection from stdin.
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

/// Default logical runtime name from the host, falling back to a stable label.
fn default_runtime_id() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|h| !h.trim().is_empty());
    match raw {
        Some(host) => sanitize_runtime_id(&host),
        None => "developer-workstation".to_string(),
    }
}

/// Normalize a hostname into a runtime slug: lowercase alnum plus `-`.
fn sanitize_runtime_id(input: &str) -> String {
    let cleaned: String = input
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "developer-workstation".to_string()
    } else {
        trimmed
    }
}

/// Parse an `--expires` value into an ISO 8601 UTC timestamp.
///
/// Accepts a relative duration with a `d` (days), `h` (hours), or `w` (weeks)
/// suffix (e.g. `30d`, `12h`, `2w`). Any value containing a `T` is treated as
/// an already-formatted timestamp and passed through unchanged.
fn parse_expires(raw: &str) -> anyhow::Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        anyhow::bail!("--expires value is empty");
    }
    // Already an absolute timestamp.
    if value.contains('T') {
        return Ok(value.to_string());
    }
    let (num_part, unit) = value.split_at(value.len() - 1);
    let n: u64 = num_part.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid --expires '{}': expected e.g. 30d, 12h, 2w, or an ISO 8601 timestamp",
            raw
        )
    })?;
    let seconds = match unit {
        "h" | "H" => n * 3600,
        "d" | "D" => n * 86400,
        "w" | "W" => n * 604800,
        other => anyhow::bail!(
            "invalid --expires unit '{}': use h (hours), d (days), or w (weeks)",
            other
        ),
    };
    Ok(unix_to_rfc3339(now_secs() + seconds))
}

/// Current wall-clock time in whole seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Whether a proleptic Gregorian year is a leap year.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format a Unix timestamp (seconds) as an RFC 3339 / ISO 8601 UTC string.
fn unix_to_rfc3339(secs: u64) -> String {
    let secs = secs as i64;
    let mut remaining_days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let mut year: i64 = 1970;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let leap = is_leap_year(year);
    let days_in_months: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &dim in &days_in_months {
        if remaining_days < dim {
            break;
        }
        remaining_days -= dim;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

// -- Workspace env exposure --------------------------------------------------

/// Write `NEXUS_PROJECT_TOKEN` into the workspace `.env.nexus.local`.
fn sync_env_token(workspace: &Path, token: &str) -> anyhow::Result<()> {
    ensure_env_local_ignored(workspace);
    let path = workspace.join(ENV_LOCAL_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = upsert_env_var(&existing, PROJECT_TOKEN_ENV, token);
    write_env_local(&path, &updated)?;
    Ok(())
}

/// Remove `NEXUS_PROJECT_TOKEN` from the workspace `.env.nexus.local`.
fn clear_env_token(workspace: &Path) -> anyhow::Result<()> {
    let path = workspace.join(ENV_LOCAL_FILE);
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let updated = remove_env_var(&existing, PROJECT_TOKEN_ENV);
    write_env_local(&path, &updated)?;
    Ok(())
}

/// Write the secrets file with restrictive permissions (0600 on Unix).
fn write_env_local(path: &Path, content: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// Guarantee `.env.nexus.local` is git-excluded via `.git/info/exclude`.
///
/// Best-effort: silently no-ops outside a git repository.
fn ensure_env_local_ignored(workspace: &Path) {
    let git_dir = workspace.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let info_dir = git_dir.join("info");
    if std::fs::create_dir_all(&info_dir).is_err() {
        return;
    }
    let exclude_path = info_dir.join("exclude");
    let content = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if content.lines().any(|l| l.trim() == ENV_LOCAL_FILE) {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)
    {
        use std::io::Write;
        let _ = writeln!(
            file,
            "\n# Nexus CLI — project token secret\n{}",
            ENV_LOCAL_FILE
        );
    }
}

/// Insert or replace `KEY=value` in an env-file body, preserving other lines.
fn upsert_env_var(content: &str, key: &str, value: &str) -> String {
    let new_line = format!("{}={}", key, value);
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let matches_key = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .split_once('=')
            .map(|(k, _)| k.trim() == key)
            .unwrap_or(false);
        if matches_key {
            if !replaced {
                out.push(new_line.clone());
                replaced = true;
            }
            // Drop duplicate assignments of the same key.
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(new_line);
    }
    let mut result = out.join("\n");
    result.push('\n');
    result
}

/// Remove all assignments of `KEY` from an env-file body.
fn remove_env_var(content: &str, key: &str) -> String {
    let out: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            let is_key = trimmed
                .strip_prefix("export ")
                .unwrap_or(trimmed)
                .split_once('=')
                .map(|(k, _)| k.trim() == key)
                .unwrap_or(false);
            !is_key
        })
        .collect();
    if out.iter().all(|l| l.trim().is_empty()) {
        return String::new();
    }
    let mut result = out.join("\n");
    result.push('\n');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_runtime_id() {
        assert_eq!(
            sanitize_runtime_id("MacBook-Pro.local"),
            "macbook-pro-local"
        );
        assert_eq!(sanitize_runtime_id("  CI Runner 7 "), "ci-runner-7");
        assert_eq!(sanitize_runtime_id("---"), "developer-workstation");
    }

    #[test]
    fn test_unix_to_rfc3339_epoch() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_unix_to_rfc3339_known() {
        // 2021-01-01T00:00:00Z == 1609459200
        assert_eq!(unix_to_rfc3339(1_609_459_200), "2021-01-01T00:00:00Z");
    }

    #[test]
    fn test_parse_expires_relative() {
        let ts = parse_expires("1d").unwrap();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[test]
    fn test_parse_expires_passthrough_iso() {
        assert_eq!(
            parse_expires("2027-01-01T00:00:00Z").unwrap(),
            "2027-01-01T00:00:00Z"
        );
    }

    #[test]
    fn test_parse_expires_bad_unit() {
        assert!(parse_expires("30y").is_err());
        assert!(parse_expires("abc").is_err());
    }

    #[test]
    fn test_upsert_env_var_inserts() {
        let out = upsert_env_var("FOO=bar\n", "NEXUS_PROJECT_TOKEN", "nxs_proj_abc");
        assert!(out.contains("FOO=bar"));
        assert!(out.contains("NEXUS_PROJECT_TOKEN=nxs_proj_abc"));
    }

    #[test]
    fn test_upsert_env_var_replaces() {
        let out = upsert_env_var(
            "NEXUS_PROJECT_TOKEN=old\nOTHER=1\n",
            "NEXUS_PROJECT_TOKEN",
            "new",
        );
        assert_eq!(out.matches("NEXUS_PROJECT_TOKEN=").count(), 1);
        assert!(out.contains("NEXUS_PROJECT_TOKEN=new"));
        assert!(out.contains("OTHER=1"));
    }

    #[test]
    fn test_upsert_env_var_dedupes() {
        let out = upsert_env_var("TOK=a\nTOK=b\n", "TOK", "c");
        assert_eq!(out.matches("TOK=").count(), 1);
        assert!(out.contains("TOK=c"));
    }

    #[test]
    fn test_remove_env_var() {
        let out = remove_env_var("NEXUS_PROJECT_TOKEN=x\nKEEP=1\n", "NEXUS_PROJECT_TOKEN");
        assert!(!out.contains("NEXUS_PROJECT_TOKEN"));
        assert!(out.contains("KEEP=1"));
    }

    #[test]
    fn test_remove_env_var_empties_file() {
        let out = remove_env_var("NEXUS_PROJECT_TOKEN=x\n", "NEXUS_PROJECT_TOKEN");
        assert_eq!(out, "");
    }
}
