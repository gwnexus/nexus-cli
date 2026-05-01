//! `nexus import` — Import existing agentic files into a Nexus project.
//!
//! Scans the workspace for known agentic file types (CLAUDE.md, AGENTS.md,
//! .cursorrules, copilot-instructions.md, .windsurf/rules/*.md, GEMINI.md,
//! .claude/settings.json), extracts directives from agent-category files,
//! resolves Markdown links to local documents, and POSTs everything to the
//! Nexus import API endpoint.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use console::style;
use regex::Regex;

use nexus_core::api::{
    ImportAgenticFile, ImportDirective, ImportPayload, ImportReferencedDoc, NexusClient,
};
use nexus_core::auth::require_token;
use nexus_core::config;

// ---------------------------------------------------------------------------
// Scan targets
// ---------------------------------------------------------------------------

/// (file_key, search_paths, category, extract_directives)
const SCAN_TARGETS: &[(&str, &[&str], &str, bool)] = &[
    (
        "CLAUDE.md",
        &["CLAUDE.md", ".claude/CLAUDE.md"],
        "agent",
        true,
    ),
    ("AGENTS.md", &["AGENTS.md"], "agent", true),
    (".cursorrules", &[".cursorrules"], "ide", false),
    (
        "copilot-instructions.md",
        &[".github/copilot-instructions.md"],
        "ide",
        false,
    ),
    ("GEMINI.md", &["GEMINI.md"], "agent", true),
    (
        ".claude/settings.json",
        &[".claude/settings.json"],
        "config",
        false,
    ),
];

/// Keywords in headings that indicate a directive section.
const DIRECTIVE_KEYWORDS: &[&str] = &[
    "rule",
    "rules",
    "directive",
    "directives",
    "guideline",
    "guidelines",
    "convention",
    "conventions",
    "policy",
    "policies",
    "constraint",
    "constraints",
    "requirement",
    "requirements",
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the `nexus import` command.
pub async fn run(api_url: &str, dry_run: bool, auto_yes: bool) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // Step 1: Resolve project
    let project_id = config::resolve_project_id(None, Some(&workspace))?;

    // Step 2: Scan for agentic files
    let detected = scan_agentic_files(&workspace);

    if detected.is_empty() {
        println!(
            "\n   {} No existing agentic files detected.\n",
            style("i").bold().blue()
        );
        return Ok(());
    }

    // Step 3: Extract directives from agent-category files
    let mut directives: Vec<ImportDirective> = Vec::new();
    for af in &detected {
        if af.category == "agent" {
            let extracted = extract_directives(&af.file_key, &af.body);
            directives.extend(extracted);
        }
    }

    // Step 4: Resolve Markdown links
    let mut referenced_docs: Vec<ImportReferencedDoc> = Vec::new();
    for af in &detected {
        let refs = extract_markdown_links(&af.body, &workspace);
        referenced_docs.extend(refs);
    }

    // Step 5: Display results
    println!();
    println!(
        "   {} Scanning workspace for existing agentic files...",
        style(">>").bold().cyan()
    );
    println!();
    println!("   Found {} agentic file(s):", style(detected.len()).bold());
    for af in &detected {
        println!(
            "   {} {} ({}, {} bytes)",
            style("+").bold().green(),
            af.target_path,
            af.category,
            af.body.len()
        );
    }

    if !directives.is_empty() {
        println!();
        println!(
            "   Extracted {} directive(s) from agent files:",
            style(directives.len()).bold()
        );
        for d in &directives {
            println!(
                "   {} [{}] {}",
                style("+").bold().green(),
                d.category,
                d.title
            );
        }
    }

    if !referenced_docs.is_empty() {
        println!();
        println!(
            "   Resolved {} referenced document(s):",
            style(referenced_docs.len()).bold()
        );
        for doc in &referenced_docs {
            println!(
                "   {} {} ({} bytes)",
                style("+").bold().green(),
                doc.source_path,
                doc.body.len()
            );
        }
    }

    println!();

    if dry_run {
        println!(
            "   {} Dry run complete. No changes were made.\n",
            style("i").bold().blue()
        );
        return Ok(());
    }

    // Step 6: Confirm
    if !auto_yes {
        print!(
            "   {} Import into Nexus project? [y/N] ",
            style("?").bold().cyan()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            println!("   {} Import cancelled.\n", style("--").yellow());
            return Ok(());
        }
    }

    // Step 7: POST to API
    let token = require_token()?;
    let client = NexusClient::new(api_url, Some(token))?;

    let payload = ImportPayload {
        action: "import".to_string(),
        project_id: project_id.clone(),
        agent_id: None,
        agentic_files: detected,
        directives,
        referenced_docs,
    };

    println!("\n   {} Importing...", style(">>").bold().cyan());

    let response = client.import(&payload).await?;

    println!();
    println!("   {} Import complete.", style("OK").bold().green());
    println!(
        "   {} {} agentic file(s) ingested",
        style("+").bold().green(),
        response.summary.agentic_files_ingested
    );
    println!(
        "   {} {} directive(s) created (disabled — review in dashboard)",
        style("+").bold().green(),
        response.summary.directives_created
    );
    println!(
        "   {} {} referenced doc(s) ingested",
        style("+").bold().green(),
        response.summary.docs_ingested
    );
    println!();
    println!("   Next steps:");
    println!("     1. Review imported items in the Nexus dashboard");
    println!("     2. Enable imported directives as needed");
    println!(
        "     3. Run {} to update local agent files with cross-references",
        style("nexus pull").bold().cyan()
    );
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Scan the workspace for known agentic file types.
///
/// Returns detected files as `ImportAgenticFile` structs ready for the API.
/// Files inside `.nexus/` are excluded (Nexus-managed).
pub fn scan_agentic_files(workspace: &Path) -> Vec<ImportAgenticFile> {
    let mut detected: Vec<ImportAgenticFile> = Vec::new();

    for &(file_key, search_paths, category, _) in SCAN_TARGETS {
        for &rel_path in search_paths {
            // Skip anything inside .nexus/
            if rel_path.starts_with(".nexus/") {
                continue;
            }

            let full_path = workspace.join(rel_path);
            if full_path.exists() && full_path.is_file() {
                if let Ok(body) = fs::read_to_string(&full_path) {
                    // Skip empty files
                    if body.trim().is_empty() {
                        continue;
                    }
                    detected.push(ImportAgenticFile {
                        file_key: file_key.to_string(),
                        target_path: rel_path.to_string(),
                        body,
                        category: category.to_string(),
                    });
                    break; // First match wins for this file_key
                }
            }
        }
    }

    // Glob scan for .windsurf/rules/*.md
    let windsurf_dir = workspace.join(".windsurf/rules");
    if windsurf_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&windsurf_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "md") {
                    if let Ok(body) = fs::read_to_string(&path) {
                        if body.trim().is_empty() {
                            continue;
                        }
                        let filename = path.file_name().unwrap().to_string_lossy().to_string();
                        detected.push(ImportAgenticFile {
                            file_key: format!(".windsurf/rules/{}", filename),
                            target_path: format!(".windsurf/rules/{}", filename),
                            body,
                            category: "ide".to_string(),
                        });
                    }
                }
            }
        }
    }

    detected
}

// ---------------------------------------------------------------------------
// Directive extraction
// ---------------------------------------------------------------------------

/// Extract directives from an agent-category file using heading-based heuristics.
///
/// Scans for H2/H3/H4 headings containing directive keywords, then extracts
/// bullet points under those headings as individual directives.
pub fn extract_directives(file_key: &str, body: &str) -> Vec<ImportDirective> {
    let mut directives = Vec::new();
    let mut in_directive_section = false;

    for line in body.lines() {
        let trimmed = line.trim();

        // Detect headings (## / ### / ####)
        if trimmed.starts_with("## ") || trimmed.starts_with("### ") || trimmed.starts_with("#### ")
        {
            let heading_text = trimmed.trim_start_matches('#').trim();

            let lower = heading_text.to_lowercase();
            in_directive_section = DIRECTIVE_KEYWORDS.iter().any(|kw| lower.contains(kw));
            continue;
        }

        // Extract bullet points in directive sections
        if in_directive_section && (trimmed.starts_with("- ") || trimmed.starts_with("* ")) {
            let text = trimmed
                .trim_start_matches('-')
                .trim_start_matches('*')
                .trim()
                .to_string();

            if text.is_empty() {
                continue;
            }

            // Truncate long directives
            let title = if text.len() > 200 {
                format!("{}...", &text[..197])
            } else {
                text.clone()
            };

            let category = categorize_directive(&text);
            let priority = prioritize_directive(&text);
            let body = if text.len() > 200 { Some(text) } else { None };

            directives.push(ImportDirective {
                title,
                body,
                category,
                priority,
                source_file: file_key.to_string(),
            });
        }
    }

    directives
}

/// Categorize a directive based on keyword matching.
fn categorize_directive(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("token")
        || lower.contains("security")
        || lower.contains("auth")
    {
        "security".to_string()
    } else if lower.contains("test")
        || lower.contains("vitest")
        || lower.contains("jest")
        || lower.contains("spec")
    {
        "testing".to_string()
    } else if lower.contains("deploy")
        || lower.contains("netlify")
        || lower.contains("ci/cd")
        || lower.contains("pipeline")
    {
        "deployment".to_string()
    } else if lower.contains("commit") || lower.contains("git") || lower.contains("branch") {
        "commit".to_string()
    } else if lower.contains("migration")
        || lower.contains("database")
        || lower.contains("supabase")
    {
        "migration".to_string()
    } else if lower.contains("lint")
        || lower.contains("format")
        || lower.contains("prettier")
        || lower.contains("eslint")
        || lower.contains("style")
    {
        "code_style".to_string()
    } else if lower.contains("doc") || lower.contains("readme") || lower.contains("comment") {
        "documentation".to_string()
    } else {
        "general".to_string()
    }
}

/// Determine directive priority based on keyword matching.
fn prioritize_directive(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("must")
        || lower.contains("always")
        || lower.contains("never")
        || lower.contains("critical")
        || lower.contains("required")
    {
        "high".to_string()
    } else if lower.contains("should") || lower.contains("prefer") || lower.contains("recommend") {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

// ---------------------------------------------------------------------------
// Markdown link resolution
// ---------------------------------------------------------------------------

/// Extract and resolve local Markdown links from file content.
///
/// Parses `[text](path)` links, filters to local `.md`/`.txt` files,
/// reads their content, and returns them as `ImportReferencedDoc` structs.
fn extract_markdown_links(body: &str, workspace: &Path) -> Vec<ImportReferencedDoc> {
    let mut docs = Vec::new();
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();

    for cap in link_re.captures_iter(body) {
        let text = &cap[1];
        let path = &cap[2];

        // Skip URLs, anchors, images
        if path.starts_with("http") || path.starts_with('#') || path.starts_with("mailto:") {
            continue;
        }

        // Only .md and .txt files
        if !path.ends_with(".md") && !path.ends_with(".txt") {
            continue;
        }

        let resolved = workspace.join(path);
        if !resolved.exists() || !resolved.is_file() {
            continue;
        }

        // Size limit: 100KB
        if resolved.metadata().map(|m| m.len()).unwrap_or(0) > 100_000 {
            continue;
        }

        if let Ok(content) = fs::read_to_string(&resolved) {
            docs.push(ImportReferencedDoc {
                title: text.to_string(),
                body: content,
                source_path: path.to_string(),
            });
        }
    }

    docs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_empty_workspace() {
        let tmp = std::env::temp_dir().join("nexus_test_import_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let detected = scan_agentic_files(&tmp);
        assert!(detected.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_detects_claude_md() {
        let tmp = std::env::temp_dir().join("nexus_test_import_claude");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("CLAUDE.md"), "# My Rules\n- Do things").unwrap();

        let detected = scan_agentic_files(&tmp);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].file_key, "CLAUDE.md");
        assert_eq!(detected[0].category, "agent");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_detects_multiple_files() {
        let tmp = std::env::temp_dir().join("nexus_test_import_multi");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("CLAUDE.md"), "# Rules").unwrap();
        fs::write(tmp.join("AGENTS.md"), "# Agents").unwrap();
        fs::write(tmp.join(".cursorrules"), "{}").unwrap();

        let detected = scan_agentic_files(&tmp);
        assert_eq!(detected.len(), 3);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_skips_empty_files() {
        let tmp = std::env::temp_dir().join("nexus_test_import_empty_file");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("CLAUDE.md"), "  \n  ").unwrap();

        let detected = scan_agentic_files(&tmp);
        assert!(detected.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_windsurf_rules() {
        let tmp = std::env::temp_dir().join("nexus_test_import_windsurf");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".windsurf/rules")).unwrap();
        fs::write(tmp.join(".windsurf/rules/style.md"), "# Style rules").unwrap();
        fs::write(tmp.join(".windsurf/rules/testing.md"), "# Test rules").unwrap();

        let detected = scan_agentic_files(&tmp);
        assert_eq!(detected.len(), 2);
        assert!(detected.iter().all(|f| f.category == "ide"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_extract_directives_from_rules_section() {
        let body = r#"# My Project

## Rules

- Always run tests before committing
- Never commit secrets to the repository
- Use named exports

## Architecture

Some text about architecture.
"#;

        let directives = extract_directives("CLAUDE.md", body);
        assert_eq!(directives.len(), 3);
        assert_eq!(directives[0].title, "Always run tests before committing");
        assert_eq!(directives[0].category, "testing");
        assert_eq!(directives[0].priority, "high"); // "always"
        assert_eq!(directives[1].category, "security"); // "secrets"
        assert_eq!(directives[1].priority, "high"); // "never"
        assert_eq!(directives[2].category, "general");
        assert_eq!(directives[2].priority, "low");
    }

    #[test]
    fn test_extract_directives_multiple_sections() {
        let body = r#"## Guidelines

- Should prefer functional components
- Use TypeScript strict mode

## Other stuff

- This is not a directive
"#;

        let directives = extract_directives("CLAUDE.md", body);
        assert_eq!(directives.len(), 2);
    }

    #[test]
    fn test_extract_directives_no_directive_sections() {
        let body = r#"## Architecture

- Use microservices
- Deploy to AWS
"#;

        let directives = extract_directives("CLAUDE.md", body);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_categorize_directive() {
        assert_eq!(categorize_directive("Never expose secrets"), "security");
        assert_eq!(categorize_directive("Run vitest before push"), "testing");
        assert_eq!(categorize_directive("Deploy via Netlify"), "deployment");
        assert_eq!(categorize_directive("Use conventional commits"), "commit");
        assert_eq!(categorize_directive("Run supabase migrations"), "migration");
        assert_eq!(
            categorize_directive("Use prettier for formatting"),
            "code_style"
        );
        assert_eq!(
            categorize_directive("Update documentation"),
            "documentation"
        );
        assert_eq!(categorize_directive("Use named exports"), "general");
    }

    #[test]
    fn test_prioritize_directive() {
        assert_eq!(prioritize_directive("Must always run tests"), "high");
        assert_eq!(prioritize_directive("Never commit secrets"), "high");
        assert_eq!(prioritize_directive("Should prefer TypeScript"), "medium");
        assert_eq!(prioritize_directive("Use named exports"), "low");
    }

    #[test]
    fn test_extract_markdown_links_resolves_local() {
        let tmp = std::env::temp_dir().join("nexus_test_import_links");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("docs")).unwrap();
        fs::write(tmp.join("docs/guide.md"), "# Guide content").unwrap();

        let body = r#"See the [Architecture Guide](docs/guide.md) for details.
Also check [Google](https://google.com) and [anchor](#section).
"#;

        let docs = extract_markdown_links(body, &tmp);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].title, "Architecture Guide");
        assert_eq!(docs[0].source_path, "docs/guide.md");
        assert!(docs[0].body.contains("Guide content"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_extract_markdown_links_skips_missing() {
        let tmp = std::env::temp_dir().join("nexus_test_import_links_missing");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let body = "See [missing file](docs/nonexistent.md)";
        let docs = extract_markdown_links(body, &tmp);
        assert!(docs.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }
}
