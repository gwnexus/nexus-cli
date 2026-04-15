//! Preflight check command.
//!
//! Verifies the user's environment is ready for Nexus development:
//! - Required tools (git, node, npm/npx)
//! - Nexus authentication
//! - API reachability
//! - Project workspace state
//! - MCP server availability

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::Credentials;
use nexus_core::config::Config;
use std::process::Command as Proc;

/// Result of a single preflight check.
#[derive(Debug)]
enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String),
}

impl CheckResult {
    fn is_fail(&self) -> bool {
        matches!(self, CheckResult::Fail(_))
    }
}

/// Display a single check result line.
fn print_check(label: &str, result: &CheckResult) {
    let (icon, msg) = match result {
        CheckResult::Pass(m) => (style("PASS").bold().green(), m.as_str()),
        CheckResult::Warn(m) => (style("WARN").bold().yellow(), m.as_str()),
        CheckResult::Fail(m) => (style("FAIL").bold().red(), m.as_str()),
    };
    println!("  {} {:<20} {}", icon, label, msg);
}

/// Run a command and capture stdout (first line, trimmed).
fn cmd_version(bin: &str, args: &[&str]) -> Option<String> {
    Proc::new(bin).args(args).output().ok().and_then(|o| {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Take first line only
            Some(s.lines().next().unwrap_or("").to_string())
        } else {
            None
        }
    })
}

/// Check: git
fn check_git() -> CheckResult {
    match cmd_version("git", &["--version"]) {
        Some(v) => CheckResult::Pass(v),
        None => CheckResult::Fail("git not found -- install from https://git-scm.com".into()),
    }
}

/// Check: Node.js
fn check_node() -> CheckResult {
    match cmd_version("node", &["--version"]) {
        Some(v) => {
            // Minimum: v18
            let major = v
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if major >= 18 {
                CheckResult::Pass(format!("node {}", v))
            } else {
                CheckResult::Warn(format!("node {} (>= v18 recommended)", v))
            }
        }
        None => CheckResult::Fail("node not found -- install from https://nodejs.org".into()),
    }
}

/// Check: npm / npx
fn check_npm() -> CheckResult {
    match cmd_version("npm", &["--version"]) {
        Some(v) => CheckResult::Pass(format!("npm {}", v)),
        None => CheckResult::Fail("npm not found".into()),
    }
}

/// Check: npx available
fn check_npx() -> CheckResult {
    match cmd_version("npx", &["--version"]) {
        Some(v) => CheckResult::Pass(format!("npx {}", v)),
        None => CheckResult::Warn("npx not found -- MCP server binary download may fail".into()),
    }
}

/// Check: Nexus CLI config
fn check_config() -> (CheckResult, Option<Config>) {
    match Config::load() {
        Ok(cfg) => (
            CheckResult::Pass(format!(
                "api_url={}, mcp_source={}",
                cfg.api_url, cfg.mcp_source
            )),
            Some(cfg),
        ),
        Err(e) => (
            CheckResult::Warn(format!("Could not load config: {}", e)),
            None,
        ),
    }
}

/// Check: Nexus auth credentials
fn check_credentials() -> (CheckResult, Option<String>) {
    match Credentials::load() {
        Ok(Some(creds)) => {
            let prefix = if creds.token.len() > 16 {
                format!("{}...", &creds.token[..16])
            } else {
                creds.token.clone()
            };
            (CheckResult::Pass(prefix), Some(creds.token))
        }
        Ok(None) => (
            CheckResult::Warn("Not authenticated -- run 'nexus login'".into()),
            None,
        ),
        Err(e) => (CheckResult::Fail(format!("Credential error: {}", e)), None),
    }
}

/// Check: API reachability (requires auth)
async fn check_api(api_url: &str, token: Option<String>) -> CheckResult {
    if token.is_none() {
        return CheckResult::Warn("Skipped (no credentials)".into());
    }

    match NexusClient::new(api_url, token) {
        Ok(client) => match client.auth_status().await {
            Ok(status) => CheckResult::Pass(format!(
                "{} ({})",
                status.user.email, status.user.platform_role
            )),
            Err(e) => CheckResult::Fail(format!("API error: {}", e)),
        },
        Err(e) => CheckResult::Fail(format!("Client error: {}", e)),
    }
}

/// Check: project workspace (.nexus/ directory)
fn check_workspace() -> CheckResult {
    let cwd = std::env::current_dir().unwrap_or_default();
    let nexus_dir = cwd.join(".nexus");
    let config_file = nexus_dir.join("config.toml");

    if !nexus_dir.exists() {
        return CheckResult::Warn("No .nexus/ directory -- run 'nexus init' to scaffold".into());
    }

    match nexus_core::config::load_linked_project(None) {
        Ok(Some(project)) => CheckResult::Pass(format!("{} ({})", project.name, project.id)),
        Ok(None) => {
            if config_file.exists() {
                CheckResult::Warn(
                    "Workspace exists but no project linked -- run 'nexus link'".into(),
                )
            } else {
                CheckResult::Warn("Workspace exists but no config.toml".into())
            }
        }
        Err(e) => CheckResult::Warn(format!("Could not read project config: {}", e)),
    }
}

/// Check: MCP server config files
fn check_mcp_configs() -> CheckResult {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Check common MCP config locations
    let locations = [
        (".claude/mcp.json", "Claude Code"),
        ("opencode.json", "OpenCode"),
        (".cursor/mcp.json", "Cursor"),
    ];

    let mut found = Vec::new();
    for (path, name) in &locations {
        if cwd.join(path).exists() {
            found.push(*name);
        }
    }

    if found.is_empty() {
        CheckResult::Warn("No MCP configs found -- run 'nexus init' to generate".into())
    } else {
        CheckResult::Pass(format!("Configured for: {}", found.join(", ")))
    }
}

/// Run all preflight checks.
pub async fn run(api_url: &str) -> anyhow::Result<()> {
    println!();
    println!("{} Nexus Preflight Check", style(">>").bold().cyan());
    println!();

    // ── Tool checks ──
    println!("  {}", style("Tools").bold().underlined());
    let git = check_git();
    print_check("git", &git);
    let node = check_node();
    print_check("node", &node);
    let npm = check_npm();
    print_check("npm", &npm);
    let npx = check_npx();
    print_check("npx", &npx);
    println!();

    // ── Config & Auth ──
    println!("  {}", style("Configuration").bold().underlined());
    let (config_check, _config) = check_config();
    print_check("config", &config_check);
    let (creds_check, token) = check_credentials();
    print_check("credentials", &creds_check);
    let api_check = check_api(api_url, token).await;
    print_check("api", &api_check);
    println!();

    // ── Workspace ──
    println!("  {}", style("Workspace").bold().underlined());
    let workspace = check_workspace();
    print_check("project", &workspace);
    let mcp = check_mcp_configs();
    print_check("mcp-configs", &mcp);
    println!();

    // ── Summary ──
    let all_checks = [
        &git,
        &node,
        &npm,
        &npx,
        &config_check,
        &creds_check,
        &api_check,
        &workspace,
        &mcp,
    ];
    let fail_count = all_checks.iter().filter(|c| c.is_fail()).count();
    let warn_count = all_checks
        .iter()
        .filter(|c| matches!(c, CheckResult::Warn(_)))
        .count();
    let pass_count = all_checks.len() - fail_count - warn_count;

    if fail_count > 0 {
        println!(
            "  {} {} passed, {} warnings, {} failed",
            style("RESULT").bold().red(),
            pass_count,
            warn_count,
            fail_count,
        );
        println!("  Fix the failures above before continuing.");
    } else if warn_count > 0 {
        println!(
            "  {} {} passed, {} warnings",
            style("RESULT").bold().yellow(),
            pass_count,
            warn_count,
        );
        println!("  Everything essential is ready. Resolve warnings for the best experience.");
    } else {
        println!(
            "  {} All {} checks passed",
            style("RESULT").bold().green(),
            pass_count,
        );
    }
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_result_is_fail() {
        assert!(CheckResult::Fail("x".into()).is_fail());
        assert!(!CheckResult::Pass("x".into()).is_fail());
        assert!(!CheckResult::Warn("x".into()).is_fail());
    }

    #[test]
    fn test_check_git_available() {
        // git should be available in any CI/dev environment
        let result = check_git();
        assert!(matches!(result, CheckResult::Pass(_)));
    }

    #[test]
    fn test_cmd_version_returns_first_line() {
        let v = cmd_version("echo", &["hello\nworld"]);
        assert!(v.is_some());
        assert_eq!(v.unwrap(), "hello");
    }

    #[test]
    fn test_cmd_version_missing_binary() {
        let v = cmd_version("__nonexistent_binary_42__", &["--version"]);
        assert!(v.is_none());
    }
}
