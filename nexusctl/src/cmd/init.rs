//! The `nexus init` command.
//!
//! Creates the standard Nexus project workspace structure:
//!
//! ```text
//! <project>/
//! ├── .nexus/
//! │   └── config.toml          # project-local Nexus config
//! ├── .claude/
//! │   ├── CLAUDE.md            # Claude agent instructions
//! │   └── skills/              # Claude skill definitions
//! ├── .opencode/
//! │   └── opencode.json        # OpenCode configuration
//! ├── AGENTS.md                # Agent role definitions
//! └── .gitignore (appended)    # Nexus-specific ignores
//! ```

use console::style;
use std::fs;
use std::path::{Path, PathBuf};

/// Run the init command.
pub async fn run(path: &str, name: Option<&str>, force: bool) -> anyhow::Result<()> {
    let target = PathBuf::from(path).canonicalize().unwrap_or_else(|_| {
        // Path doesn't exist yet; resolve relative to cwd
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    });

    let project_name = name.unwrap_or_else(|| {
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("nexus-project")
    });

    println!(
        "{} Initializing Nexus workspace: {}",
        style(">>").bold().cyan(),
        style(project_name).bold()
    );
    println!("   Target: {}", target.display());
    println!();

    // Check if already initialized
    let nexus_dir = target.join(".nexus");
    if nexus_dir.exists() && !force {
        anyhow::bail!(
            "Directory already contains .nexus/. Use --force to reinitialize."
        );
    }

    // Create directory structure
    create_nexus_dir(&target, project_name)?;
    create_claude_dir(&target, project_name)?;
    create_opencode_dir(&target, project_name)?;
    create_agents_md(&target, project_name)?;
    append_gitignore(&target)?;

    println!();
    println!(
        "{} Nexus workspace initialized successfully.",
        style("OK").bold().green()
    );
    println!();
    println!("Next steps:");
    println!("  1. Run 'nexus login' to authenticate");
    println!("  2. Edit AGENTS.md to define your agent roles");
    println!("  3. Add skills to .claude/skills/");

    Ok(())
}

/// Create .nexus/ directory with project-local config.
fn create_nexus_dir(target: &Path, project_name: &str) -> anyhow::Result<()> {
    let nexus_dir = target.join(".nexus");
    fs::create_dir_all(&nexus_dir)?;

    let config_content = format!(
        r#"# Nexus project-local configuration
# This file is managed by `nexus init` and can be edited manually.

[project]
name = "{name}"

[mcp]
# MCP server connection settings (resolved at runtime)
# server_url = "https://nexus.mpowr.tech/api/mcp"
"#,
        name = project_name,
    );

    fs::write(nexus_dir.join("config.toml"), config_content)?;
    print_created(".nexus/config.toml");

    Ok(())
}

/// Create .claude/ directory with instruction template and skills folder.
fn create_claude_dir(target: &Path, project_name: &str) -> anyhow::Result<()> {
    let claude_dir = target.join(".claude");
    fs::create_dir_all(claude_dir.join("skills"))?;

    let claude_md = format!(
        r#"---
type: bootstrap
scope: repo
project: {name}
status: active
---

# BOOTSTRAP SEQUENCE

1. Load agent identity from `AGENTS.md`
2. Connect to the Nexus MCP server
3. Load the project index from the Nexus platform
4. Review active planning and ADR context
5. Continue with the active workstream

---

# PROJECT

This workspace is configured for the **{name}** project.

Treat all project memory and coordination artifacts as architecture-critical.

---

# ENVIRONMENT

Read secrets only from `.env.local`.

NEVER:
- print secrets
- commit secrets
- persist secrets into shared memory
"#,
        name = project_name,
    );

    fs::write(claude_dir.join("CLAUDE.md"), claude_md)?;
    print_created(".claude/CLAUDE.md");
    print_created(".claude/skills/");

    Ok(())
}

/// Create .opencode/ directory with configuration template.
fn create_opencode_dir(target: &Path, project_name: &str) -> anyhow::Result<()> {
    let opencode_dir = target.join(".opencode");
    fs::create_dir_all(&opencode_dir)?;

    let opencode_json = format!(
        r#"{{
  "$schema": "https://opencode.ai/config.schema.json",
  "name": "{name}",
  "mcpServers": {{}}
}}
"#,
        name = project_name,
    );

    fs::write(opencode_dir.join("opencode.json"), opencode_json)?;
    print_created(".opencode/opencode.json");

    Ok(())
}

/// Create AGENTS.md with a template agent definition.
fn create_agents_md(target: &Path, project_name: &str) -> anyhow::Result<()> {
    let agents_path = target.join("AGENTS.md");
    if agents_path.exists() {
        println!(
            "   {} AGENTS.md already exists, skipping",
            style("--").yellow()
        );
        return Ok(());
    }

    let agents_md = format!(
        r#"---
type: agent-policy
scope: repo
project: {name}
status: active
---

# ACTIVE AGENTS

- app-agent (PRIMARY)

---

# AGENT ROLE DEFINITION

## app-agent (PRIMARY)

You are responsible for:

- Application architecture and development
- Code quality and testing
- Documentation and knowledge management

You are expected to:

- Maintain architectural clarity
- Keep durable truth out of ephemeral chat context
- Preserve auditability and handoff quality

---

# GLOBAL RULES

- Decisions must be documented (ADR or architectural note)
- Sessions are execution history, not long-term truth
- Durable learnings go to project memory
- No speculation presented as fact
- Correctness over speed
"#,
        name = project_name,
    );

    fs::write(&agents_path, agents_md)?;
    print_created("AGENTS.md");

    Ok(())
}

/// Append Nexus-specific entries to .gitignore if not already present.
fn append_gitignore(target: &Path) -> anyhow::Result<()> {
    let gitignore_path = target.join(".gitignore");
    let marker = "# Nexus CLI";

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if content.contains(marker) {
            return Ok(());
        }
    }

    let nexus_ignores = format!(
        r#"
{marker}
.env.local
.nexus/credentials.toml
"#,
        marker = marker,
    );

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)?;

    use std::io::Write;
    file.write_all(nexus_ignores.as_bytes())?;
    print_created(".gitignore (appended)");

    Ok(())
}

/// Print a "created" status line.
fn print_created(path: &str) {
    println!(
        "   {} {}",
        style("+").bold().green(),
        path
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a temp directory for testing init.
    fn temp_project_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_init_creates_structure() {
        let dir = temp_project_dir();
        run(dir.to_str().unwrap(), Some("test-project"), false)
            .await
            .unwrap();

        assert!(dir.join(".nexus/config.toml").exists());
        assert!(dir.join(".claude/CLAUDE.md").exists());
        assert!(dir.join(".claude/skills").is_dir());
        assert!(dir.join(".opencode/opencode.json").exists());
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join(".gitignore").exists());

        // Verify config content
        let config = fs::read_to_string(dir.join(".nexus/config.toml")).unwrap();
        assert!(config.contains("test-project"));

        // Verify AGENTS.md content
        let agents = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(agents.contains("test-project"));

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_fails_if_exists_without_force() {
        let dir = temp_project_dir();
        fs::create_dir_all(dir.join(".nexus")).unwrap();

        let result = run(dir.to_str().unwrap(), Some("test"), false).await;
        assert!(result.is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_force_reinitializes() {
        let dir = temp_project_dir();
        fs::create_dir_all(dir.join(".nexus")).unwrap();

        let result = run(dir.to_str().unwrap(), Some("reinit-test"), true).await;
        assert!(result.is_ok());
        assert!(dir.join(".nexus/config.toml").exists());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_preserves_existing_agents_md() {
        let dir = temp_project_dir();
        fs::write(dir.join("AGENTS.md"), "custom content").unwrap();

        run(dir.to_str().unwrap(), Some("test"), false)
            .await
            .unwrap();

        let agents = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert_eq!(agents, "custom content");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_init_gitignore_not_duplicated() {
        let dir = temp_project_dir();
        run(dir.to_str().unwrap(), Some("test"), false)
            .await
            .unwrap();

        // Run again with force
        run(dir.to_str().unwrap(), Some("test"), true)
            .await
            .unwrap();

        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let marker_count = gitignore.matches("# Nexus CLI").count();
        assert_eq!(marker_count, 1, "gitignore marker should appear only once");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }
}
