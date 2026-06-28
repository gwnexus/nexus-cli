//! The `nexus actors` command group.
//!
//! Provides subcommands for listing, viewing, and managing actor profiles
//! assigned to the linked Nexus project.

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use std::fs;
use std::path::Path;

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

/// Normalize actor markdown to YAML frontmatter format (ADR-0056).
///
/// Reads a markdown file, extracts or generates frontmatter fields (slug, name,
/// role, description, route_alias), and rewrites the file with normalized format.
pub fn normalize(path: &str) -> anyhow::Result<()> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        anyhow::bail!("File not found: {}", path);
    }

    let content = fs::read_to_string(file_path)?;
    let (frontmatter, body) = parse_frontmatter(&content);

    // Extract or infer fields from frontmatter and body
    let slug = frontmatter
        .get("slug")
        .cloned()
        .or_else(|| {
            file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let name = frontmatter
        .get("name")
        .cloned()
        .unwrap_or_else(|| slug.clone());
    let role = frontmatter
        .get("role")
        .cloned()
        .unwrap_or_else(|| "agent".to_string());
    let description = frontmatter.get("description").cloned();
    let route_alias = frontmatter.get("route_alias").cloned();

    // Rebuild normalized markdown
    let mut normalized = String::from("---\n");
    normalized.push_str(&format!("slug: {}\n", slug));
    normalized.push_str(&format!("name: {}\n", name));
    normalized.push_str(&format!("role: {}\n", role));
    if let Some(ref desc) = description {
        normalized.push_str(&format!("description: {}\n", desc));
    }
    if let Some(ref route) = route_alias {
        normalized.push_str(&format!("route_alias: {}\n", route));
    }
    normalized.push_str("source: nexus-platform\n");
    normalized.push_str("---\n\n");
    normalized.push_str(body.trim());
    normalized.push('\n');

    fs::write(file_path, &normalized)?;

    println!(
        "   {} Normalized: {} (slug: {})",
        style("+").bold().green(),
        path,
        style(&slug).cyan()
    );

    Ok(())
}

/// Validate actor profile(s) against the expected schema.
///
/// Checks: required frontmatter fields, route_alias references (if model routes
/// are available), and markdown structure.
pub async fn validate(
    api_url: &str,
    path: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<()> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        anyhow::bail!("File not found: {}", path);
    }

    let content = fs::read_to_string(file_path)?;
    let (frontmatter, _body) = parse_frontmatter(&content);

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Required fields
    if !frontmatter.contains_key("slug") {
        errors.push("Missing required field: slug".to_string());
    }
    if !frontmatter.contains_key("name") {
        errors.push("Missing required field: name".to_string());
    }
    if !frontmatter.contains_key("role") {
        errors.push("Missing required field: role".to_string());
    }

    // Validate slug format
    if let Some(slug) = frontmatter.get("slug") {
        if !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            errors.push(format!(
                "Invalid slug '{}': only alphanumeric, dash, underscore allowed",
                slug
            ));
        }
    }

    // Validate route_alias against model routes (if available)
    if let Some(route_alias) = frontmatter.get("route_alias") {
        let workspace = std::env::current_dir()?;
        if let Ok(project_id) = config::resolve_project_id(cli_project_id, Some(&workspace)) {
            if let Ok(token) = resolve_token()
                .ok_or_else(|| anyhow::anyhow!("no token"))
                .as_deref()
                .map(|t| t.to_string())
            {
                if let Ok(client) = NexusClient::new(api_url, Some(token)) {
                    if let Ok(af_export) = client.export_agent_files(&project_id).await {
                        if !af_export.model_routes.is_empty() {
                            let route_exists = af_export
                                .model_routes
                                .iter()
                                .any(|r| r.alias == *route_alias);
                            if !route_exists {
                                errors.push(format!(
                                    "route_alias '{}' not found in model route catalog",
                                    route_alias
                                ));
                            }
                            let is_deprecated = af_export
                                .model_routes
                                .iter()
                                .any(|r| r.alias == *route_alias && r.deprecated);
                            if is_deprecated {
                                warnings.push(format!(
                                    "route_alias '{}' references a deprecated route",
                                    route_alias
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Report results
    println!(
        "{} Validating: {}",
        style(">>").bold().cyan(),
        style(path).bold()
    );

    if errors.is_empty() && warnings.is_empty() {
        println!("   {} Valid actor profile", style("+").bold().green());
    } else {
        for err in &errors {
            println!("   {} {}", style("ERROR").bold().red(), err);
        }
        for warn in &warnings {
            println!("   {} {}", style("WARN").bold().yellow(), warn);
        }
        if !errors.is_empty() {
            anyhow::bail!(
                "Validation failed: {} error(s), {} warning(s)",
                errors.len(),
                warnings.len()
            );
        }
    }

    Ok(())
}

/// Import actor profiles from local markdown files into the Actor Registry.
pub async fn import(api_url: &str, path: &str, cli_project_id: Option<&str>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let file_path = Path::new(path);
    let files = if file_path.is_dir() {
        // Import all .md files in the directory
        let mut entries = Vec::new();
        for entry in fs::read_dir(file_path)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "md") {
                entries.push(entry.path());
            }
        }
        entries
    } else {
        vec![file_path.to_path_buf()]
    };

    if files.is_empty() {
        println!("   {} No .md files found at: {}", style("·").dim(), path);
        return Ok(());
    }

    let mut import_entries = Vec::new();
    for file in &files {
        let content = fs::read_to_string(file)?;
        let (frontmatter, body) = parse_frontmatter(&content);

        let slug = frontmatter.get("slug").cloned().unwrap_or_else(|| {
            file.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        import_entries.push(nexus_core::api::ActorImportEntry {
            slug,
            name: frontmatter
                .get("name")
                .cloned()
                .unwrap_or_else(|| "Unnamed".to_string()),
            role: frontmatter
                .get("role")
                .cloned()
                .unwrap_or_else(|| "agent".to_string()),
            description: frontmatter.get("description").cloned(),
            model_routing: frontmatter.get("model_routing").cloned(),
            route_alias: frontmatter.get("route_alias").cloned(),
            profile_body: Some(body.to_string()),
        });
    }

    println!(
        "{} Importing {} actor profile(s)...",
        style(">>").bold().cyan(),
        import_entries.len()
    );

    let client = NexusClient::new(api_url, Some(token))?;
    let payload = nexus_core::api::ActorImportPayload {
        action: "actor_import".to_string(),
        project_id,
        actors: import_entries,
    };

    let response = client.import_actors(&payload).await?;

    println!(
        "   {} {} actor(s) imported",
        style("+").bold().green(),
        response.imported
    );
    if let Some(ref msg) = response.message {
        println!("   {}", msg);
    }

    Ok(())
}

/// Export actor configuration for a target format.
pub async fn export(
    api_url: &str,
    target: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;

    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token))?;
    let response = client.export_actors(&project_id, target).await?;

    match target {
        "opencode" => {
            if let Some(ref agents) = response.opencode_agents {
                let json = serde_json::to_string_pretty(agents)?;
                println!("{}", json);
            } else {
                println!(
                    "   {} No opencode agent configuration available.",
                    style("·").dim()
                );
            }
        }
        _ => {
            anyhow::bail!("Unsupported export target: '{}'. Use 'opencode'.", target);
        }
    }

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

// ---------------------------------------------------------------------------
// Frontmatter parser (simple YAML key: value from --- delimited block)
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from a Markdown file.
///
/// Returns a map of key-value pairs and the remaining body text.
fn parse_frontmatter(content: &str) -> (std::collections::HashMap<String, String>, &str) {
    let mut map = std::collections::HashMap::new();

    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (map, content);
    }

    // Find the closing ---
    let after_first = &trimmed[3..].trim_start_matches(['\r', '\n']);
    let Some(end) = after_first.find("\n---") else {
        return (map, content);
    };

    let frontmatter_block = &after_first[..end];
    let body_start = after_first[end + 4..].trim_start_matches(['\r', '\n', '-']);

    for line in frontmatter_block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
    }

    (map, body_start)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_frontmatter tests --

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = "---\nslug: my-agent\nname: My Agent\nrole: developer\n---\n\n# Profile\n\nHello world.\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("slug").unwrap(), "my-agent");
        assert_eq!(fm.get("name").unwrap(), "My Agent");
        assert_eq!(fm.get("role").unwrap(), "developer");
        assert!(body.contains("# Profile"));
        assert!(body.contains("Hello world."));
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "# Just a markdown file\n\nNo frontmatter here.\n";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_frontmatter_empty_values_skipped() {
        let content = "---\nslug: my-agent\nempty_field:\nrole: agent\n---\n\nBody.\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm.get("slug").unwrap(), "my-agent");
        assert_eq!(fm.get("role").unwrap(), "agent");
        assert!(!fm.contains_key("empty_field"));
    }

    #[test]
    fn test_parse_frontmatter_comments_ignored() {
        let content = "---\n# This is a comment\nslug: test\nname: Test Agent\n---\n\nBody.\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm.len(), 2);
        assert_eq!(fm.get("slug").unwrap(), "test");
        assert_eq!(fm.get("name").unwrap(), "Test Agent");
    }

    #[test]
    fn test_parse_frontmatter_colon_in_value() {
        let content = "---\nslug: my-agent\ndescription: A description: with colon\n---\n\nBody.\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm.get("slug").unwrap(), "my-agent");
        assert_eq!(fm.get("description").unwrap(), "A description: with colon");
    }

    #[test]
    fn test_parse_frontmatter_unclosed_returns_raw() {
        let content = "---\nslug: my-agent\nname: Test\n";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_parse_frontmatter_with_route_alias() {
        let content =
            "---\nslug: coder\nname: Coder\nrole: developer\nroute_alias: fast-model\n---\n\nDoes coding.\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.get("slug").unwrap(), "coder");
        assert_eq!(fm.get("route_alias").unwrap(), "fast-model");
        assert!(body.contains("Does coding."));
    }

    #[test]
    fn test_parse_frontmatter_leading_whitespace() {
        let content = "\n\n---\nslug: test\n---\n\nBody.\n";
        let (fm, _body) = parse_frontmatter(content);
        assert_eq!(fm.get("slug").unwrap(), "test");
    }

    // -- normalize tests --

    #[test]
    fn test_normalize_creates_frontmatter() {
        let dir = std::env::temp_dir().join("nexus-actors-test-normalize");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file = dir.join("my-agent.md");
        fs::write(&file, "# My Agent\n\nJust a plain profile.\n").unwrap();

        normalize(file.to_str().unwrap()).unwrap();

        let result = fs::read_to_string(&file).unwrap();
        assert!(result.starts_with("---\n"));
        assert!(result.contains("slug: my-agent"));
        assert!(result.contains("name: my-agent"));
        assert!(result.contains("role: agent"));
        assert!(result.contains("source: nexus-platform"));
        assert!(result.contains("# My Agent"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_normalize_preserves_existing_frontmatter() {
        let dir = std::env::temp_dir().join("nexus-actors-test-normalize-existing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file = dir.join("coder.md");
        let input = "---\nslug: coder\nname: The Coder\nrole: developer\ndescription: Writes code\nroute_alias: opus-4\n---\n\n# Profile Body\n";
        fs::write(&file, input).unwrap();

        normalize(file.to_str().unwrap()).unwrap();

        let result = fs::read_to_string(&file).unwrap();
        assert!(result.contains("slug: coder"));
        assert!(result.contains("name: The Coder"));
        assert!(result.contains("role: developer"));
        assert!(result.contains("description: Writes code"));
        assert!(result.contains("route_alias: opus-4"));
        assert!(result.contains("source: nexus-platform"));
        assert!(result.contains("# Profile Body"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_normalize_nonexistent_file_errors() {
        let result = normalize("/tmp/nexus-does-not-exist-xyz.md");
        assert!(result.is_err());
    }
}
