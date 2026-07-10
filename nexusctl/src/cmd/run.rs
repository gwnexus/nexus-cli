//! The `nexus run` command.
//!
//! Launches a tool (default: `opencode`) with platform-managed plugin env vars
//! injected into the process environment. Runs a pre-launch check, then spawns
//! the tool. After the tool exits, prints a session summary.
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
use std::time::Instant;
use std::{env, fs};

use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;

use super::preflight::{cmd_version, print_check, CheckResult};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run(
    api_url: &str,
    tool: Option<&str>,
    dry_run: bool,
    show_env: bool,
    no_db: bool,
    use_exec: bool,
    skip_checks: bool,
    force: bool,
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
    let secrets_map = parse_env_file(&workspace.join(".env.nexus.local"));
    for (k, v) in &secrets_map {
        plugin_env.insert(k.clone(), v.clone());
    }

    // ── 4. Build final injection map (skip vars already set in shell) ────────
    let mut to_inject: Vec<(String, String, &'static str)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    let mut sorted_keys: Vec<String> = plugin_env.keys().cloned().collect();
    sorted_keys.sort();

    for key in &sorted_keys {
        let value = plugin_env.get(key).unwrap();
        if env::var(key).is_ok() {
            skipped.push(key.clone());
        } else {
            let source = if secrets_map.contains_key(key) {
                ".env.nexus.local"
            } else {
                ".nexus/env"
            };
            to_inject.push((key.clone(), value.clone(), source));
        }
    }

    let effective_tool = tool.unwrap_or(default_tool);

    // ── 5. Dry-run / show-env output ─────────────────────────────────────────
    if dry_run || show_env {
        print_env_table(&workspace, effective_tool, &to_inject, &skipped, args);
        if dry_run {
            return Ok(());
        }
        // show_env: wait for user confirmation
        println!(
            "   Press {} to launch {}, or {} to abort...",
            style("Enter").bold(),
            style(effective_tool).bold(),
            style("Ctrl+C").bold()
        );
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        println!();
    }

    // ── 6. Pre-launch checks ─────────────────────────────────────────────────
    if !skip_checks {
        let should_continue =
            run_prelaunch_checks(&workspace, effective_tool, &env_file_path, force)?;
        if !should_continue {
            return Ok(());
        }
    }

    // ── 7. Inject env vars ────────────────────────────────────────────────────
    for (key, value, _) in &to_inject {
        env::set_var(key, value);
    }

    // ── 8. Launch the tool ───────────────────────────────────────────────────
    if use_exec {
        exec_tool(effective_tool, args)
    } else {
        // Capture git state before launch for post-session diff
        let head_before = git_head_sha(&workspace);
        let tags_before = git_tags(&workspace);
        let start = Instant::now();
        let run_start_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let exit_code = spawn_tool(effective_tool, args)?;

        // ── 9. Post-session summary ──────────────────────────────────────
        let elapsed = start.elapsed();
        let head_after = git_head_sha(&workspace);
        let tags_after = git_tags(&workspace);

        // Fetch token/cost + activity stats from Nexus session (best effort)
        let (token_stats, activity_stats) = if !no_db {
            fetch_session_stats(api_url, &workspace).await
        } else {
            (None, None)
        };

        print_session_summary(
            &workspace,
            elapsed,
            exit_code,
            head_before.as_deref(),
            head_after.as_deref(),
            &tags_before,
            &tags_after,
            run_start_epoch,
            token_stats.as_ref(),
            activity_stats.as_ref(),
        );

        std::process::exit(exit_code);
    }
}

// ---------------------------------------------------------------------------
// Env display
// ---------------------------------------------------------------------------

fn print_env_table(
    workspace: &Path,
    tool: &str,
    to_inject: &[(String, String, &str)],
    skipped: &[String],
    args: &[String],
) {
    let project_name = resolve_project_name(workspace);
    println!();
    println!("   Project: {}", style(project_name).bold());
    println!("   Tool:    {}", style(tool).bold());
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
        for (k, v, source) in to_inject {
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
            for k in skipped {
                println!("     {:<36}   (skipped — already in shell)", style(k).dim());
            }
        }
    }
    println!();
    println!(
        "   Would exec: {}{}",
        tool,
        if args.is_empty() {
            String::new()
        } else {
            format!(" {}", args.join(" "))
        }
    );
    println!();
}

// ---------------------------------------------------------------------------
// Pre-launch checks
// ---------------------------------------------------------------------------

/// Run pre-launch checks and return `true` if the tool should be launched.
fn run_prelaunch_checks(
    workspace: &Path,
    tool: &str,
    env_file: &Path,
    force: bool,
) -> anyhow::Result<bool> {
    println!();
    println!("{} Nexus Pre-launch Check", style(">>").bold().cyan());
    println!();

    let mut checks: Vec<(&str, CheckResult)> = Vec::new();

    // Workspace
    let nexus_dir = workspace.join(".nexus");
    let ws_check = if nexus_dir.exists() {
        match config::load_linked_project(Some(workspace)) {
            Ok(Some(p)) => {
                CheckResult::Pass(format!("{} ({})", p.name, &p.id[..8.min(p.id.len())]))
            }
            Ok(None) => CheckResult::Warn("No project linked — run 'nexus link'".into()),
            Err(_) => CheckResult::Warn("Could not read project config".into()),
        }
    } else {
        CheckResult::Fail("No .nexus/ directory — run 'nexus init'".into())
    };
    checks.push(("Workspace", ws_check));

    // Auth
    let auth_check = match resolve_token() {
        Some(t) if t.len() > 8 => CheckResult::Pass(format!("{}...", &t[..8])),
        Some(_) => CheckResult::Pass("token present".into()),
        None => CheckResult::Fail("Not authenticated — run 'nexus login'".into()),
    };
    checks.push(("Auth", auth_check));

    // MCP Config
    let oc_path = workspace.join("opencode.json");
    let mcp_check = if oc_path.exists() {
        let content = fs::read_to_string(&oc_path).unwrap_or_default();
        if content.contains("\"nexus\"") {
            CheckResult::Pass("opencode.json (nexus MCP configured)".into())
        } else {
            CheckResult::Warn("opencode.json exists but no nexus MCP block".into())
        }
    } else {
        CheckResult::Warn("No opencode.json — run 'nexus init' or 'nexus pull'".into())
    };
    checks.push(("MCP Config", mcp_check));

    // Plugin Env
    let env_check = if env_file.exists() {
        let map = parse_env_file(env_file);
        if map.is_empty() {
            CheckResult::Warn(".nexus/env exists but is empty".into())
        } else {
            CheckResult::Pass(format!(".nexus/env ({} vars)", map.len()))
        }
    } else {
        CheckResult::Warn("No .nexus/env — run 'nexus pull' first".into())
    };
    checks.push(("Plugin Env", env_check));

    // Tool binary
    let tool_check = match cmd_version(tool, &["--version"]) {
        Some(v) => {
            let short = v.lines().next().unwrap_or(&v);
            let short = if short.len() > 60 {
                format!("{}...", &short[..57])
            } else {
                short.to_string()
            };
            CheckResult::Pass(format!("{} ({})", tool, short))
        }
        None => CheckResult::Fail(format!("'{}' not found in PATH", tool)),
    };
    checks.push(("Tool", tool_check));

    // Headroom mode
    let headroom_mode = env::var("HEADROOM_MODE")
        .ok()
        .or_else(|| parse_env_file(env_file).get("HEADROOM_MODE").cloned());
    let headroom_check = match headroom_mode {
        Some(ref m) if m == "transform" => CheckResult::Pass(format!("HEADROOM_MODE={}", m)),
        Some(ref m) => CheckResult::Warn(format!(
            "HEADROOM_MODE={} (expected 'transform' for full compression)",
            m
        )),
        None => {
            CheckResult::Warn("HEADROOM_MODE not set — headroom will use 'observe' mode".into())
        }
    };
    checks.push(("Headroom", headroom_check));

    // Print results
    for (label, result) in &checks {
        print_check(label, result);
    }
    println!();

    let fail_count = checks.iter().filter(|(_, c)| c.is_fail()).count();
    let warn_count = checks.iter().filter(|(_, c)| c.is_warn()).count();
    let pass_count = checks.len() - fail_count - warn_count;

    if fail_count > 0 {
        println!(
            "  {} {} passed, {} warnings, {} failed",
            style("RESULT").bold().red(),
            pass_count,
            warn_count,
            fail_count,
        );
        println!();
        if force {
            println!(
                "   {} checks failed (--force: continuing anyway)",
                fail_count
            );
            println!();
        } else {
            println!("   {} checks failed. Continue anyway? [y/N] ", fail_count);
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            let answer = buf.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("   Aborted.");
                return Ok(false);
            }
            println!();
        }
    } else if warn_count > 0 {
        println!(
            "  {} {} passed, {} warnings",
            style("RESULT").bold().yellow(),
            pass_count,
            warn_count,
        );
        if !force {
            println!();
            println!(
                "   Press {} to launch {}, or {} to abort...",
                style("Enter").bold(),
                style(tool).bold(),
                style("Ctrl+C").bold()
            );
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
        }
        println!();
    } else {
        println!(
            "  {} All {} checks passed",
            style("RESULT").bold().green(),
            pass_count,
        );
        if !force {
            println!();
            println!(
                "   Press {} to launch {}, or {} to abort...",
                style("Enter").bold(),
                style(tool).bold(),
                style("Ctrl+C").bold()
            );
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
        }
        println!();
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Tool launching
// ---------------------------------------------------------------------------

/// Spawn the tool as a child process, wait for exit, return exit code.
fn spawn_tool(tool: &str, args: &[String]) -> anyhow::Result<i32> {
    let mut child = std::process::Command::new(tool)
        .args(args)
        .spawn()
        .with_context(|| format!("failed to launch '{tool}'"))?;

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for '{tool}'"))?;

    Ok(status.code().unwrap_or(1))
}

/// Replace the current process with the tool (Unix exec semantics).
/// On non-Unix, falls back to spawn+wait+exit.
fn exec_tool(tool: &str, args: &[String]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(tool).args(args).exec();
        Err(err).with_context(|| format!("failed to exec '{tool}'"))
    }

    #[cfg(not(unix))]
    {
        let code = spawn_tool(tool, args)?;
        std::process::exit(code);
    }
}

// ---------------------------------------------------------------------------
// Post-session summary
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn print_session_summary(
    workspace: &Path,
    elapsed: std::time::Duration,
    exit_code: i32,
    head_before: Option<&str>,
    head_after: Option<&str>,
    tags_before: &[String],
    tags_after: &[String],
    run_start_epoch: u64,
    token_stats: Option<&TokenStats>,
    activity_stats: Option<&ActivityStats>,
) {
    let hrs = elapsed.as_secs() / 3600;
    let mins = (elapsed.as_secs() % 3600) / 60;
    let secs = elapsed.as_secs() % 60;

    let duration_str = if hrs > 0 {
        format!("{}h {}m {}s", hrs, mins, secs)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    };

    println!();
    println!(
        "{}",
        style("─────────────────────────────────────────────").dim()
    );
    println!("  {}", style("Nexus Session Summary").bold());
    println!(
        "{}",
        style("─────────────────────────────────────────────").dim()
    );
    println!("  Duration:     {}", style(&duration_str).bold());
    println!(
        "  Exit code:    {}",
        if exit_code == 0 {
            style(exit_code.to_string()).green()
        } else {
            style(exit_code.to_string()).red()
        }
    );

    // Git activity
    let same_head = head_before == head_after;
    if !same_head {
        if let (Some(before), Some(_after)) = (head_before, head_after) {
            // Count commits between before..HEAD
            let commit_count = git_count_commits(workspace, before);
            let diff_stat = git_diff_stat(workspace, before);

            println!();
            println!("  {}:", style("Git Activity").bold());
            if let Some(n) = commit_count {
                println!("    Commits:    {}", n);
            }
            if let Some(ref stat) = diff_stat {
                println!("    Changes:    {}", stat);
            }
        }
    } else {
        println!();
        println!("  {}:", style("Git Activity").bold());
        println!("    {}", style("No commits during session").dim());
    }

    // New releases (tags)
    let new_tags: Vec<&String> = tags_after
        .iter()
        .filter(|t| !tags_before.contains(t))
        .collect();
    if !new_tags.is_empty() {
        println!(
            "    Releases:   {}",
            new_tags
                .iter()
                .map(|t| style(t).bold().green().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Headroom stats from .nexus/headroom-intercept.jsonl
    let headroom = read_headroom_stats(workspace, run_start_epoch);
    println!();
    print!("  {}:     ", style("Headroom").bold());
    match headroom {
        Some(ref h) => {
            let mode_style = if h.mode == "transform" {
                style(&h.mode).green()
            } else {
                style(&h.mode).yellow()
            };
            println!("{} mode", mode_style);
            if h.compressions > 0 || h.locally_applied > 0 {
                println!(
                    "    {} compressions, {} local transforms, ~{} tokens saved",
                    h.compressions, h.locally_applied, h.potential_saved_tokens
                );
            }
            println!(
                "    {} observations, {} skips, {} passthroughs",
                h.observations, h.skips, h.passthroughs
            );
            if h.cache_integrity_failures > 0 {
                println!(
                    "    {}",
                    style(format!(
                        "{} cache integrity failures",
                        h.cache_integrity_failures
                    ))
                    .yellow()
                );
            }
        }
        None => {
            println!("{}", style("no stats (headroom JSONL not found)").dim());
        }
    }

    // Token/Cost stats
    println!();
    print!("  {}:  ", style("Token Usage").bold());
    match token_stats {
        Some(ts) => {
            println!();
            println!(
                "    Input:      {:>10} tokens",
                format_number(ts.tokens_input)
            );
            println!(
                "    Output:     {:>10} tokens",
                format_number(ts.tokens_output)
            );
            if ts.tokens_cache_read > 0 {
                println!(
                    "    Cache:      {:>10} tokens (read)",
                    format_number(ts.tokens_cache_read)
                );
            }
            println!(
                "    Total:      {:>10} tokens",
                format_number(ts.total_tokens)
            );
            if ts.cost_usd > 0.0 {
                println!("    Est. Cost:  ${:.2}", ts.cost_usd);
            }
        }
        None => {
            println!("{}", style("unavailable (no session data)").dim());
        }
    }

    // Nexus Activity
    if let Some(activity) = activity_stats {
        if activity.has_any() {
            println!();
            println!("  {}:", style("Nexus Activity").bold());
            if activity.adrs_created > 0 || activity.adrs_accepted > 0 {
                println!(
                    "    ADRs:       {} created, {} accepted",
                    activity.adrs_created, activity.adrs_accepted
                );
            }
            if activity.tasks_created > 0 || activity.tasks_completed > 0 {
                println!(
                    "    Tasks:      {} created, {} completed",
                    activity.tasks_created, activity.tasks_completed
                );
            }
            if activity.dispatches_sent > 0 || activity.dispatches_replied > 0 {
                println!(
                    "    Dispatches: {} sent, {} replied",
                    activity.dispatches_sent, activity.dispatches_replied
                );
            }
            if activity.notes > 0 {
                println!("    Notes:      {}", activity.notes);
            }
        }
    }

    println!(
        "{}",
        style("─────────────────────────────────────────────").dim()
    );
    println!();
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn git_head_sha(workspace: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn git_tags(workspace: &Path) -> Vec<String> {
    std::process::Command::new("git")
        .args(["tag", "--sort=creatordate"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn git_count_commits(workspace: &Path, since_sha: &str) -> Option<u64> {
    std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{since_sha}..HEAD")])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
}

fn git_diff_stat(workspace: &Path, since_sha: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["diff", "--shortstat", since_sha, "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Token + Activity stats from Nexus session
// ---------------------------------------------------------------------------

/// Token usage summary extracted from session cost_snapshot entries.
struct TokenStats {
    tokens_input: u64,
    tokens_output: u64,
    tokens_cache_read: u64,
    total_tokens: u64,
    cost_usd: f64,
}

/// Nexus platform activity summary from session entries.
struct ActivityStats {
    adrs_created: u64,
    adrs_accepted: u64,
    tasks_created: u64,
    tasks_completed: u64,
    dispatches_sent: u64,
    dispatches_replied: u64,
    notes: u64,
}

impl ActivityStats {
    fn has_any(&self) -> bool {
        self.adrs_created > 0
            || self.adrs_accepted > 0
            || self.tasks_created > 0
            || self.tasks_completed > 0
            || self.dispatches_sent > 0
            || self.dispatches_replied > 0
            || self.notes > 0
    }
}

/// Fetch token and activity stats from the Nexus session (best effort).
async fn fetch_session_stats(
    api_url: &str,
    workspace: &Path,
) -> (Option<TokenStats>, Option<ActivityStats>) {
    let token = match resolve_token() {
        Some(t) => t,
        None => return (None, None),
    };
    let client = match NexusClient::new(api_url, Some(token)) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let project_id = config::resolve_project_id(None, Some(workspace)).unwrap_or_default();
    if project_id.is_empty() {
        return (None, None);
    }

    // 1. Find the open session for this project
    let session_list = match client.list_sessions(&project_id).await {
        Ok(v) => v,
        Err(_) => return (None, None),
    };

    let session_id = session_list
        .get("sessions")
        .and_then(|s| s.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str());

    let session_id = match session_id {
        Some(id) => id.to_string(),
        None => return (None, None),
    };

    // 2. Fetch full session with entries
    let session_data = match client.get_session(&session_id).await {
        Ok(v) => v,
        Err(_) => return (None, None),
    };

    let entries = session_data
        .get("document")
        .or_else(|| session_data.get("entries"))
        .and_then(|_| session_data.get("entries"))
        .and_then(|e| e.as_array());

    let entries = match entries {
        Some(e) => e,
        None => return (None, None),
    };

    // 3. Extract latest cost_snapshot
    let mut token_stats: Option<TokenStats> = None;
    for entry in entries.iter().rev() {
        let meta = entry.get("metadata");
        let is_cost = meta.and_then(|m| m.get("plugin")).and_then(|v| v.as_str())
            == Some("nexus-cost-control");
        if is_cost {
            if let Some(m) = meta {
                token_stats = Some(TokenStats {
                    tokens_input: m.get("tokens_input").and_then(|v| v.as_u64()).unwrap_or(0),
                    tokens_output: m.get("tokens_output").and_then(|v| v.as_u64()).unwrap_or(0),
                    tokens_cache_read: m
                        .get("tokens_cache_read")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    total_tokens: m.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    cost_usd: m.get("cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0),
                });
                break;
            }
        }
    }

    // 4. Extract activity counts from entry_type fields
    let mut activity = ActivityStats {
        adrs_created: 0,
        adrs_accepted: 0,
        tasks_created: 0,
        tasks_completed: 0,
        dispatches_sent: 0,
        dispatches_replied: 0,
        notes: 0,
    };

    for entry in entries {
        let entry_type = entry
            .get("entry_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match entry_type {
            "adr_drafted" => activity.adrs_created += 1,
            "adr_accepted" => activity.adrs_accepted += 1,
            "task_created" => activity.tasks_created += 1,
            "task_updated" => activity.tasks_completed += 1,
            "letter_sent" => activity.dispatches_sent += 1,
            "letter_replied" => activity.dispatches_replied += 1,
            "note" => activity.notes += 1,
            _ => {}
        }
    }

    let activity_result = if activity.has_any() {
        Some(activity)
    } else {
        None
    };

    (token_stats, activity_result)
}

/// Format a number with thousands separators.
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// Headroom JSONL reader
// ---------------------------------------------------------------------------

/// Parsed headroom session summary from `.nexus/headroom-intercept.jsonl`.
struct HeadroomSummary {
    mode: String,
    compressions: u64,
    locally_applied: u64,
    observations: u64,
    skips: u64,
    passthroughs: u64,
    potential_saved_tokens: u64,
    cache_integrity_failures: u64,
}

/// Read the last `session_summary` event from `.nexus/headroom-intercept.jsonl`
/// that was written after `run_start_epoch` (Unix seconds). Falls back to the
/// very last `session_summary` in the file if timestamp filtering fails.
fn read_headroom_stats(workspace: &Path, run_start_epoch: u64) -> Option<HeadroomSummary> {
    let jsonl_path = workspace.join(".nexus").join("headroom-intercept.jsonl");
    let content = fs::read_to_string(&jsonl_path).ok()?;

    let mut best: Option<(u64, HeadroomSummary)> = None;

    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("event").and_then(|v| v.as_str()) != Some("session_summary") {
            continue;
        }

        // Parse timestamp — may be ISO 8601 string or Unix millis number
        let ts_epoch = val
            .get("ts")
            .and_then(|v| {
                v.as_u64().or_else(|| {
                    // ISO string → approximate epoch (just compare against run_start)
                    v.as_str().map(|_| run_start_epoch) // treat as "current session"
                })
            })
            .unwrap_or(0);

        // Convert millis to seconds if necessary
        let ts_secs = if ts_epoch > 1_000_000_000_000 {
            ts_epoch / 1000
        } else {
            ts_epoch
        };

        let summary = HeadroomSummary {
            mode: val
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            compressions: val
                .get("compressions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            locally_applied: val
                .get("locallyAppliedTransforms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            observations: val
                .get("observations")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            skips: val.get("skips").and_then(|v| v.as_u64()).unwrap_or(0),
            passthroughs: val
                .get("passthroughs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            potential_saved_tokens: val
                .get("potentialSavedTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_integrity_failures: val
                .get("cacheIntegrityFailures")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        };

        // Prefer entries from after run start
        if ts_secs >= run_start_epoch {
            return Some(summary);
        }

        // Keep the most recent as fallback
        if best.is_none() || ts_secs > best.as_ref().unwrap().0 {
            best = Some((ts_secs, summary));
        }
    }

    best.map(|(_, s)| s)
}

// ---------------------------------------------------------------------------
// Env helpers
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

/// Resolve agentic root — fall back to `.nexus`.
fn resolve_agentic_root(_workspace: &Path) -> String {
    ".nexus".to_string()
}

/// Resolve a display project name from `.nexus/config.toml`.
fn resolve_project_name(workspace: &Path) -> String {
    config::load_project_config(Some(workspace))
        .ok()
        .flatten()
        .and_then(|pc| pc.project)
        .map(|p| {
            let short_id = if p.id.len() >= 8 { &p.id[..8] } else { &p.id };
            format!("{} ({})", p.name, short_id)
        })
        .unwrap_or_else(|| "(unlinked workspace)".to_string())
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
                exec,
                skip_checks,
                force,
                args,
            } => {
                assert!(tool.is_none());
                assert!(!dry_run);
                assert!(!show_env);
                assert!(!no_db);
                assert!(!exec);
                assert!(!skip_checks);
                assert!(!force);
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

    #[test]
    fn test_parse_cli_run_exec_flag() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run", "--exec"]).unwrap();
        assert!(matches!(cli.command, Command::Run { exec: true, .. }));
    }

    #[test]
    fn test_parse_cli_run_skip_checks() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run", "--skip-checks"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                skip_checks: true,
                ..
            }
        ));
    }

    #[test]
    fn test_git_head_sha_in_repo() {
        // We're running inside a git repo
        let cwd = std::env::current_dir().unwrap();
        let sha = git_head_sha(&cwd);
        assert!(sha.is_some());
        assert!(sha.unwrap().len() >= 7);
    }

    #[test]
    fn test_git_tags_returns_vec() {
        let cwd = std::env::current_dir().unwrap();
        let tags = git_tags(&cwd);
        // May or may not have tags, but must not panic
        let _ = tags;
    }

    #[test]
    fn test_read_headroom_stats_from_jsonl() {
        let dir = tmp_dir("headroom_jsonl");
        let nexus_dir = dir.join(".nexus");
        fs::create_dir_all(&nexus_dir).unwrap();
        let jsonl = nexus_dir.join("headroom-intercept.jsonl");

        let content = r#"{"event":"tool_intercept","tool":"nexus_kb_memory","ts":1752044000000}
{"event":"session_summary","mode":"transform","compressions":4,"locallyAppliedTransforms":2,"observations":3,"skips":115,"passthroughs":12,"potentialSavedTokens":10690,"cacheIntegrityFailures":0,"fullRetrievalDenied":0,"outputBudgetTruncated":0,"cacheReadFailures":0,"ts":1752044100000}
"#;
        fs::write(&jsonl, content).unwrap();

        let stats = read_headroom_stats(&dir, 0);
        assert!(stats.is_some());
        let s = stats.unwrap();
        assert_eq!(s.mode, "transform");
        assert_eq!(s.compressions, 4);
        assert_eq!(s.locally_applied, 2);
        assert_eq!(s.observations, 3);
        assert_eq!(s.skips, 115);
        assert_eq!(s.passthroughs, 12);
        assert_eq!(s.potential_saved_tokens, 10690);
        assert_eq!(s.cache_integrity_failures, 0);
    }

    #[test]
    fn test_read_headroom_stats_missing_file() {
        let dir = tmp_dir("headroom_missing");
        let stats = read_headroom_stats(&dir, 0);
        assert!(stats.is_none());
    }

    #[test]
    fn test_read_headroom_stats_filters_by_start_time() {
        let dir = tmp_dir("headroom_filter");
        let nexus_dir = dir.join(".nexus");
        fs::create_dir_all(&nexus_dir).unwrap();
        let jsonl = nexus_dir.join("headroom-intercept.jsonl");

        // Two summaries: old one and recent one
        let content = r#"{"event":"session_summary","mode":"observe","compressions":0,"locallyAppliedTransforms":0,"observations":1,"skips":50,"passthroughs":5,"potentialSavedTokens":0,"cacheIntegrityFailures":0,"ts":1000000000000}
{"event":"session_summary","mode":"transform","compressions":7,"locallyAppliedTransforms":3,"observations":5,"skips":200,"passthroughs":20,"potentialSavedTokens":15000,"cacheIntegrityFailures":0,"ts":1752044200000}
"#;
        fs::write(&jsonl, content).unwrap();

        // Start time after the old entry but before the new one
        let stats = read_headroom_stats(&dir, 1752044100);
        assert!(stats.is_some());
        let s = stats.unwrap();
        assert_eq!(s.mode, "transform");
        assert_eq!(s.compressions, 7);
    }
}
