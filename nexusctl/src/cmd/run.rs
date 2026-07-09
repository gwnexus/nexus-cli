//! The `nexus run` command.
//!
//! Launches a tool (default: `opencode`) with platform-managed plugin env vars
//! injected into the process environment before exec.
//!
//! Env resolution priority (low → high — later wins for injection, shell is never overwritten):
//!
//! ```text
//! .nexus/env (plugin defaults from af_export)
//!   ↑ override by
//! .env.nexus.local (project secrets: ANTHROPIC_API_KEY, etc.)
//!   ↑ do not overwrite
//! process.env (shell — already-set vars are never overwritten)
//! ```

use anyhow::Context as _;
use console::style;
use std::collections::HashMap;
use std::path::Path;
use std::{env, fs};

use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(
    api_url: &str,
    tool: Option<&str>,
    dry_run: bool,
    show_env: bool,
    no_db: bool,
    args: &[String],
    default_tool: &str,
) -> anyhow::Result<()> {
    let workspace = env::current_dir()?;
    let agentic_root = resolve_agentic_root(&workspace);

    // ── 1. Load .nexus/env (plugin defaults from last pull) ────────────────
    let env_file_path = workspace.join(&agentic_root).join("env");
    let mut plugin_env = parse_env_file(&env_file_path);

    // ── 2. If !--no-db, refresh from af_export (fresher than file) ─────────
    if !no_db {
        if let Some(token) = resolve_token() {
            if let Ok(client) = NexusClient::new(api_url, Some(token)) {
                let project_id =
                    config::resolve_project_id(None, Some(&workspace)).unwrap_or_default();
                if !project_id.is_empty() {
                    if let Ok(af_export) = client.export_agent_files(&project_id).await {
                        // Merge: af_export wins (fresher)
                        for (k, v) in af_export.plugin_env {
                            plugin_env.insert(k, v);
                        }
                    }
                }
            }
        }
    }

    // ── 3. Load .env.nexus.local (secrets override plugin defaults) ─────────
    let secrets_env = parse_env_file(&workspace.join(".env.nexus.local"));
    for (k, v) in secrets_env {
        plugin_env.insert(k, v);
    }

    // ── 4. Build final injection map (skip vars already set in shell) ────────
    let mut to_inject: Vec<(String, String, &'static str)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    let plugin_env_keys: Vec<String> = plugin_env.keys().cloned().collect();
    let mut sorted_keys = plugin_env_keys;
    sorted_keys.sort();

    for key in &sorted_keys {
        let value = plugin_env.get(key).unwrap();
        if env::var(key).is_ok() {
            skipped.push(key.clone());
        } else {
            // Classify source for display
            let source = if parse_env_file(&workspace.join(".env.nexus.local")).contains_key(key) {
                ".env.nexus.local"
            } else {
                ".nexus/env"
            };
            to_inject.push((key.clone(), value.clone(), source));
        }
    }

    // ── 5. Dry-run / show-env output ─────────────────────────────────────────
    let effective_tool = tool.unwrap_or(default_tool);

    if dry_run || show_env {
        let project_name = resolve_project_name(&workspace);
        println!();
        println!("   Project: {}", style(project_name).bold());
        println!("   Tool:    {}", style(effective_tool).bold());
        println!();
        if to_inject.is_empty() && skipped.is_empty() {
            println!(
                "   {} No plugin env vars found (run 'nexus pull' first)",
                style("!").bold().yellow()
            );
        } else {
            println!(
                "   Resolved env-vars ({} injected, {} skipped — already in shell):",
                to_inject.len(),
                skipped.len()
            );
            for (k, v, source) in &to_inject {
                // Redact long values (likely secrets)
                let display_val = if v.len() > 40 {
                    format!("{}...", &v[..8])
                } else {
                    v.clone()
                };
                println!(
                    "     {:<36} = {:<20}  [{}]",
                    style(k).bold(),
                    display_val,
                    style(source).dim()
                );
            }
            if !skipped.is_empty() {
                println!();
                for k in &skipped {
                    println!("     {:<36}   (skipped — already in shell)", style(k).dim());
                }
            }
        }
        println!();
        if dry_run {
            println!(
                "   Dry run — would exec: {}{}",
                effective_tool,
                if args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", args.join(" "))
                }
            );
            println!();
            return Ok(());
        }
    }

    // ── 6. Inject env vars ────────────────────────────────────────────────────
    for (key, value, _) in &to_inject {
        env::set_var(key, value);
    }

    // ── 7. Exec the tool ─────────────────────────────────────────────────────
    exec_tool(effective_tool, args)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `KEY=value` env file. Skips blank lines and `#` comments.
/// Strips optional `export ` prefix and surrounding `"` / `'` quotes.
pub(crate) fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(content) = fs::read_to_string(path) else {
        return map;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            if !key.is_empty() {
                map.insert(key, value.to_string());
            }
        }
    }
    map
}

/// Resolve agentic root from `.nexus/config.toml` or fall back to `.nexus`.
fn resolve_agentic_root(_workspace: &Path) -> String {
    // ProjectConfig doesn't currently store agentic_root; fall back to .nexus
    ".nexus".to_string()
}

/// Resolve a display project name from `.nexus/config.toml`.
fn resolve_project_name(workspace: &Path) -> String {
    config::load_project_config(Some(workspace))
        .ok()
        .flatten()
        .and_then(|pc| pc.project)
        .map(|p| format!("{} ({})", p.name, &p.id[..8]))
        .unwrap_or_else(|| "(unlinked workspace)".to_string())
}

/// Replace the current process with the tool (Unix exec semantics).
/// On Windows, spawns and waits, then propagates the exit code.
fn exec_tool(tool: &str, args: &[String]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(tool).args(args).exec();
        // exec() only returns on error
        Err(err).with_context(|| format!("failed to exec '{tool}'"))
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(tool)
            .args(args)
            .status()
            .with_context(|| format!("failed to launch '{tool}'"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus_run_test_{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_env_file_basic() {
        let dir = tmp_dir("basic");
        let path = dir.join("env");
        fs::write(&path, "HEADROOM_MODE=transform\nHEADROOM_DEBUG=false\n").unwrap();
        let map = parse_env_file(&path);
        assert_eq!(
            map.get("HEADROOM_MODE").map(|s| s.as_str()),
            Some("transform")
        );
        assert_eq!(map.get("HEADROOM_DEBUG").map(|s| s.as_str()), Some("false"));
    }

    #[test]
    fn test_parse_env_file_skips_comments_and_blanks() {
        let dir = tmp_dir("comments");
        let path = dir.join("env");
        fs::write(
            &path,
            "# comment\n\nHEADROOM_MODE=transform\n# another comment\n",
        )
        .unwrap();
        let map = parse_env_file(&path);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("HEADROOM_MODE"));
    }

    #[test]
    fn test_parse_env_file_strips_export_prefix() {
        let dir = tmp_dir("export_prefix");
        let path = dir.join("env");
        fs::write(&path, "export MY_KEY=my_value\n").unwrap();
        let map = parse_env_file(&path);
        assert_eq!(map.get("MY_KEY").map(|s| s.as_str()), Some("my_value"));
    }

    #[test]
    fn test_parse_env_file_missing_file_returns_empty() {
        let dir = tmp_dir("missing");
        let path = dir.join("nonexistent");
        let map = parse_env_file(&path);
        assert!(map.is_empty());
    }

    #[test]
    fn test_parse_env_file_quoted_values() {
        let dir = tmp_dir("quoted");
        let path = dir.join("env");
        fs::write(&path, "KEY=\"hello world\"\nKEY2='foo bar'\n").unwrap();
        let map = parse_env_file(&path);
        assert_eq!(map.get("KEY").map(|s| s.as_str()), Some("hello world"));
        assert_eq!(map.get("KEY2").map(|s| s.as_str()), Some("foo bar"));
    }

    #[test]
    fn test_parse_cli_run_no_args() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run"]).unwrap();
        match cli.command {
            Command::Run {
                tool,
                dry_run,
                show_env,
                no_db,
                args,
            } => {
                assert!(tool.is_none());
                assert!(!dry_run);
                assert!(!show_env);
                assert!(!no_db);
                assert!(args.is_empty());
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_cli_run_dry_run() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Command::Run { dry_run: true, .. }));
    }

    #[test]
    fn test_parse_cli_run_with_tool_and_args() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["nexus", "run", "--tool", "claude", "--", "--model", "opus"])
                .unwrap();
        match cli.command {
            Command::Run { tool, args, .. } => {
                assert_eq!(tool.as_deref(), Some("claude"));
                assert_eq!(args, vec!["--model", "opus"]);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_cli_run_no_db_and_show_env() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run", "--no-db", "--show-env"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                no_db: true,
                show_env: true,
                ..
            }
        ));
    }
}
