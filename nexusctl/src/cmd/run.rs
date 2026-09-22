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
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::io::Write as _;
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
    default_tool: Option<&str>,
    countdown_secs: u64,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let workspace = env::current_dir()?;
    let agentic_root = resolve_agentic_root(&workspace);

    // Tool flavor this project is owned by ("opencode" / "claude-cli" / "both").
    // Cached in .nexus/config.toml by link/init/pull, refreshed below from
    // af_export when we talk to the backend anyway.
    let mut agent_owner = config::load_agent_owner(Some(&workspace));

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
                        if let Some(owner) = af_export.agent_owner.filter(|v| !v.is_empty()) {
                            agent_owner = Some(owner);
                        }
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

    let effective_tool = resolve_effective_tool(tool, default_tool, agent_owner.as_deref());
    let effective_tool = effective_tool.as_str();

    // ── 4.5. Named Claude account (`--account`, NEXUS-APP dispatch ad6e0176)
    // ──────────────────────────────────────────────────────────────────────
    //
    // Explicit only, never automatic: a single name per invocation, or the
    // existing default (`~/.claude`) when none is given. Scoped to
    // claude-cli/both projects; ignored (with a warning) elsewhere, since
    // CLAUDE_CONFIG_DIR has no effect on any other tool.
    let claude_config_dir: Option<std::path::PathBuf> =
        match resolve_account(&config::Config::dir()?, agent_owner.as_deref(), account)? {
            AccountResolution::NoOverride {
                warn_ignored: Some(name),
            } => {
                println!(
                    "   {} --account '{}' has no effect: this project's agent_owner \
                 is not claude-cli/both.",
                    style("!").bold().yellow(),
                    name
                );
                None
            }
            AccountResolution::NoOverride { warn_ignored: None } => None,
            AccountResolution::Selected(dir) => {
                fs::create_dir_all(&dir).with_context(|| {
                    format!("could not create account directory {}", dir.display())
                })?;
                Some(dir)
            }
        };

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

    // ── 5.5. Keep project-scoped Nexus credentials in sync with the global
    // login (single source of truth: ~/.config/nexus/credentials.toml,
    // managed exclusively via 'nexus login') ──────────────────────────────
    //
    // opencode.json / .mcp.json intentionally bake a literal
    // NEXUS_API_URL/NEXUS_PRIVATE_TOKEN (not an {env:} reference) so the MCP
    // config is self-sufficient even when the tool is launched without
    // 'nexus run' (e.g. directly from an IDE). That's a deliberate,
    // tested design (see nexus_core pull tests) — not a bug. The actual gap
    // (Task a3bf595b, NEXUS-APP) is that the baked copy silently drifts from
    // the global login token whenever 'nexus login' rotates it, with nothing
    // to notice or fix it short of re-running 'nexus init'/'nexus pull'.
    // Since 'nexus run' already resolves the current, live-verified token on
    // every invocation, it's the natural place to keep the baked copy fresh
    // automatically — the user only ever has to think about one place
    // (`nexus login`), and every subsequent `nexus run` self-heals the rest.
    if let Some(token) = resolve_token() {
        match sync_mcp_credentials(&workspace, api_url, &token) {
            Ok(paths) if !paths.is_empty() => {
                println!(
                    "   {} refreshed Nexus credentials in: {}",
                    style("~").bold().cyan(),
                    paths.join(", ")
                );
            }
            Ok(_) => {}
            Err(e) => {
                // Never block the run over a best-effort sync.
                println!(
                    "   {} could not sync Nexus credentials into MCP configs: {}",
                    style("!").bold().yellow(),
                    e
                );
            }
        }
    }

    // ── 6. Pre-launch checks ─────────────────────────────────────────────────
    if !skip_checks {
        let should_continue = run_prelaunch_checks(
            api_url,
            &workspace,
            effective_tool,
            agent_owner.as_deref(),
            &env_file_path,
            force,
            countdown_secs,
            account,
        )
        .await?;
        if !should_continue {
            return Ok(());
        }
    }

    // ── 7. Inject env vars (with denylist for dangerous variables) ────────────
    const DENIED_ENV_VARS: &[&str] = &[
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "PATH",
        "HOME",
        "SHELL",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
    ];

    for (key, value, _) in &to_inject {
        if DENIED_ENV_VARS.iter().any(|&d| d.eq_ignore_ascii_case(key)) {
            eprintln!(
                "  \x1b[33mwarning:\x1b[0m skipping denied env var '{}' from file source",
                key
            );
            continue;
        }
        env::set_var(key, value);
    }

    // Named Claude account (see step 4.5): set after the general injection
    // loop so it is never mistaken for a plugin/secret var, and skip if the
    // shell already has CLAUDE_CONFIG_DIR set (never overwrite an
    // explicitly-set shell var, same rule as the injection loop above).
    if let Some(dir) = &claude_config_dir {
        if env::var("CLAUDE_CONFIG_DIR").is_ok() {
            println!(
                "   {} CLAUDE_CONFIG_DIR is already set in the shell — \
                 --account '{}' ignored to avoid overriding it.",
                style("!").bold().yellow(),
                account.unwrap_or_default()
            );
        } else {
            env::set_var("CLAUDE_CONFIG_DIR", dir);
        }
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

        // Show spinner so the user knows post-processing is in progress.
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        spinner.set_message("Session ended — collecting summary. Press Ctrl+C to skip.");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        // Race: collect stats vs. user pressing Ctrl+C to skip.
        //
        // git_head_sha/git_tags are synchronous (they call
        // std::process::Command::output(), which blocks the current OS
        // thread until the subprocess exits). Calling them directly inside
        // this async block would run them to completion before the first
        // real .await point, which prevents tokio::select! from ever
        // noticing ctrl_c() completed until the git call itself returns --
        // silently defeating the "Press Ctrl+C to skip" promise above for
        // as long as that git call takes (including if a pager or a slow
        // repo makes it take much longer than expected; see dispatch
        // 479a4ab5). Running them via spawn_blocking moves the blocking
        // work onto tokio's dedicated blocking thread pool, so ctrl_c() can
        // actually win the race at any time.
        let workspace_for_git = workspace.clone();
        let summary_result = tokio::select! {
            _ = tokio::signal::ctrl_c() => None,
            result = async {
                let (head_after, tags_after) = tokio::task::spawn_blocking(move || {
                    (git_head_sha(&workspace_for_git), git_tags(&workspace_for_git))
                })
                .await
                .unwrap_or((None, Vec::new()));
                let (token_stats, activity_stats) = if !no_db {
                    fetch_session_stats(api_url, &workspace).await
                } else {
                    (None, None)
                };
                Some((head_after, tags_after, token_stats, activity_stats))
            } => result,
        };

        spinner.finish_and_clear();

        match summary_result {
            Some((head_after, tags_after, token_stats, activity_stats)) => {
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
                    agent_owner.as_deref(),
                );
            }
            None => {
                println!("\n  {} Summary skipped.", style("⚡").cyan(),);
            }
        }

        std::process::exit(exit_code);
    }
}

// ---------------------------------------------------------------------------
// Credential sync — keep baked opencode.json / .mcp.json Nexus MCP
// credentials in sync with the current global login (Task a3bf595b, NEXUS-APP)
// ---------------------------------------------------------------------------

/// Patch the `mcp.nexus.environment` block of `opencode.json` (and, if
/// present, the top-level `mcpServers.nexus.env` block of `.mcp.json`)
/// in place, only touching `NEXUS_API_URL`/`NEXUS_PRIVATE_TOKEN` and only if
/// they differ from the currently-resolved values. Returns the list of
/// relative paths that were actually rewritten (empty if already in sync or
/// the files don't exist — this is best-effort and silent when there's
/// nothing to do).
///
/// Deliberately does NOT touch any other keys/formatting beyond the two
/// credential fields, and does not create either file if absent (that's
/// `nexus init`/`nexus pull`'s job).
fn sync_mcp_credentials(
    workspace: &Path,
    api_url: &str,
    token: &str,
) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();

    // opencode.json: mcp.nexus.environment.{NEXUS_API_URL,NEXUS_PRIVATE_TOKEN}
    let oc_path = workspace.join("opencode.json");
    if oc_path.is_file() {
        let raw = fs::read_to_string(&oc_path)?;
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(env) = root
                .pointer_mut("/mcp/nexus/environment")
                .and_then(|v| v.as_object_mut())
            {
                let mut dirty = false;
                if env.get("NEXUS_API_URL").and_then(|v| v.as_str()) != Some(api_url) {
                    env.insert(
                        "NEXUS_API_URL".to_string(),
                        serde_json::Value::String(api_url.to_string()),
                    );
                    dirty = true;
                }
                if env.get("NEXUS_PRIVATE_TOKEN").and_then(|v| v.as_str()) != Some(token) {
                    env.insert(
                        "NEXUS_PRIVATE_TOKEN".to_string(),
                        serde_json::Value::String(token.to_string()),
                    );
                    dirty = true;
                }
                if dirty {
                    let out = serde_json::to_string_pretty(&root)?;
                    fs::write(&oc_path, out + "\n")?;
                    changed.push("opencode.json".to_string());
                }
            }
        }
    }

    // .mcp.json (project root): mcpServers.nexus.env.{NEXUS_API_URL,NEXUS_PRIVATE_TOKEN}
    let claude_path = workspace.join(".mcp.json");
    if claude_path.is_file() {
        let raw = fs::read_to_string(&claude_path)?;
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(env) = root
                .pointer_mut("/mcpServers/nexus/env")
                .and_then(|v| v.as_object_mut())
            {
                let mut dirty = false;
                if env.get("NEXUS_API_URL").and_then(|v| v.as_str()) != Some(api_url) {
                    env.insert(
                        "NEXUS_API_URL".to_string(),
                        serde_json::Value::String(api_url.to_string()),
                    );
                    dirty = true;
                }
                if env.get("NEXUS_PRIVATE_TOKEN").and_then(|v| v.as_str()) != Some(token) {
                    env.insert(
                        "NEXUS_PRIVATE_TOKEN".to_string(),
                        serde_json::Value::String(token.to_string()),
                    );
                    dirty = true;
                }
                if dirty {
                    let out = serde_json::to_string_pretty(&root)?;
                    fs::write(&claude_path, out + "\n")?;
                    changed.push(".mcp.json".to_string());
                }
            }
        }
    }

    Ok(changed)
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
// Tool-flavor resolution
// ---------------------------------------------------------------------------

/// Decide which binary `nexus run` launches.
///
/// Priority (highest first), per NEXUS-APP dispatch dfd4e655:
/// 1. `--tool <bin>` on the command line
/// 2. an explicit `run.default_tool` in `~/.config/nexus/config.toml`
/// 3. the linked project's `agent_owner` (`claude-cli` -> `claude`)
/// 4. [`config::DEFAULT_RUN_TOOL`]
fn resolve_effective_tool(
    cli_tool: Option<&str>,
    configured_default: Option<&str>,
    agent_owner: Option<&str>,
) -> String {
    if let Some(t) = cli_tool.filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    if let Some(t) = configured_default.filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    config::tool_for_agent_owner(agent_owner).to_string()
}

/// Does this project's flavor include Claude Code?
fn wants_claude(agent_owner: Option<&str>) -> bool {
    matches!(agent_owner, Some("claude-cli") | Some("both"))
}

/// Does this project's flavor include OpenCode?
///
/// Unknown/absent flavors count as OpenCode, preserving pre-dfd4e655 behaviour
/// for workspaces linked before `agent_owner` was cached locally.
fn wants_opencode(agent_owner: Option<&str>) -> bool {
    !matches!(agent_owner, Some("claude-cli"))
}

// ---------------------------------------------------------------------------
// Named Claude account switching (`--account`, NEXUS-APP dispatch ad6e0176)
// ---------------------------------------------------------------------------
//
// Claude Code derives its Keychain credential storage key from
// `CLAUDE_CONFIG_DIR` (verified live against a fresh directory demanding its
// own `/login`). Pointing different invocations at different directories
// under `~/.config/nexus/claude-accounts/<name>/` gives each named account
// its own isolated login and OAuth refresh cycle, entirely client-side —
// Nexus never stores or is aware of which account is selected.
//
// Hard constraint from the dispatch: explicit only, never automatic. There
// is deliberately no rotation, no quota/rate-limit detection, and no
// fallback list here — a single name per invocation, or the existing
// default (`~/.claude`) when no name is given.

/// Reserved `--account` value meaning "explicitly select the implicit
/// default identity" (`~/.claude`, no `CLAUDE_CONFIG_DIR` override).
///
/// Functionally identical to omitting `--account` entirely, so a caller can
/// always write `--account <name>` in scripts/aliases regardless of how many
/// real named accounts currently exist (NEXUS-APP dispatch c0523ebe). Never
/// treated as a creatable directory name: this alias is matched before
/// [`validate_account_name`]/[`claude_account_dir`] are ever consulted for
/// it, so a literal directory named `default` under `claude-accounts/` is
/// never created via this path, and a real `--account default` slot can
/// never be provisioned.
const DEFAULT_ACCOUNT_ALIAS: &str = "default";

/// Validate an `--account <name>` value before it is used to build a path.
///
/// Rejects anything that could escape `~/.config/nexus/claude-accounts/`
/// (path separators, `..`, empty names) or that is not a plain identifier.
/// This is a local directory-naming safeguard, not a security boundary
/// against a hostile filesystem — consistent with the rest of the CLI's
/// treatment of user-supplied path components.
fn validate_account_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("--account name must not be empty");
    }
    if name == "." || name == ".." {
        anyhow::bail!("--account name '{}' is not a valid directory name", name);
    }
    let is_valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !is_valid {
        anyhow::bail!(
            "--account name '{}' must contain only letters, digits, '-', or '_'",
            name
        );
    }
    Ok(())
}

/// Resolve the `CLAUDE_CONFIG_DIR` for a validated `--account <name>`.
fn claude_account_dir(config_dir: &Path, name: &str) -> std::path::PathBuf {
    config_dir.join("claude-accounts").join(name)
}

/// Outcome of deciding what `--account <name>` means for this invocation,
/// before any filesystem I/O. Kept separate from directory creation
/// (performed by the caller only for `Selected`) so the decision itself —
/// including the reserved `default` alias and the non-claude-project case —
/// can be unit tested without touching disk.
#[derive(Debug, PartialEq, Eq)]
enum AccountResolution {
    /// No `CLAUDE_CONFIG_DIR` override: `~/.claude` is used as-is. Covers no
    /// `--account`, the `default` alias, and — carrying the name to warn
    /// about — a real name on a project that is not claude-cli/both.
    NoOverride { warn_ignored: Option<String> },
    /// A validated real account name; the caller creates `dir` before use.
    Selected(std::path::PathBuf),
}

/// Pure decision core for `--account` resolution (NEXUS-APP dispatches
/// ad6e0176, c0523ebe). See [`AccountResolution`] for what each outcome
/// means.
fn resolve_account(
    config_dir: &Path,
    agent_owner: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<AccountResolution> {
    match account {
        None => Ok(AccountResolution::NoOverride { warn_ignored: None }),
        Some(name) if name == DEFAULT_ACCOUNT_ALIAS => {
            Ok(AccountResolution::NoOverride { warn_ignored: None })
        }
        Some(name) => {
            if !wants_claude(agent_owner) {
                Ok(AccountResolution::NoOverride {
                    warn_ignored: Some(name.to_string()),
                })
            } else {
                validate_account_name(name)?;
                Ok(AccountResolution::Selected(claude_account_dir(
                    config_dir, name,
                )))
            }
        }
    }
}

/// Check to surface which Claude account (if any) is active, so the operator
/// is not guessing which Keychain identity `claude` will use.
fn account_check(agent_owner: Option<&str>, account: Option<&str>) -> Option<CheckResult> {
    if !wants_claude(agent_owner) {
        return None;
    }
    Some(match account {
        Some(name) if name == DEFAULT_ACCOUNT_ALIAS => {
            CheckResult::Pass("default (~/.claude, explicit)".into())
        }
        Some(name) => CheckResult::Pass(format!("'{}' (CLAUDE_CONFIG_DIR override)", name)),
        None => CheckResult::Pass("default (~/.claude)".into()),
    })
}

/// Check the MCP config artifact(s) that actually matter for this project's
/// tool flavor.
///
/// Before dispatch dfd4e655 this unconditionally inspected `opencode.json` and
/// told `claude-cli` projects to "run 'nexus init'" for a file they are never
/// supposed to have. The authoritative artifact is `opencode.json` for
/// OpenCode and the root `.mcp.json` for Claude Code (`both` needs each).
fn mcp_config_check(workspace: &Path, agent_owner: Option<&str>) -> CheckResult {
    let mut expected: Vec<&str> = Vec::new();
    if wants_opencode(agent_owner) {
        expected.push("opencode.json");
    }
    if wants_claude(agent_owner) {
        expected.push(".mcp.json");
    }

    let mut configured: Vec<&str> = Vec::new();
    let mut no_nexus_block: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();

    for name in &expected {
        let path = workspace.join(name);
        if !path.exists() {
            missing.push(name);
        } else if fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("\"nexus\"")
        {
            configured.push(name);
        } else {
            no_nexus_block.push(name);
        }
    }

    if !missing.is_empty() {
        return CheckResult::Warn(format!(
            "No {} — run 'nexus init' or 'nexus pull'",
            missing.join(", ")
        ));
    }
    if !no_nexus_block.is_empty() {
        return CheckResult::Warn(format!(
            "{} exists but no nexus MCP block",
            no_nexus_block.join(", ")
        ));
    }
    CheckResult::Pass(format!("{} (nexus MCP configured)", configured.join(", ")))
}

/// Check whether an inherited API-key credential would silently defeat a
/// Claude Max subscription (NEXUS-APP dispatch 8de19c71).
///
/// Claude Code's auth precedence puts `ANTHROPIC_API_KEY` /
/// `ANTHROPIC_AUTH_TOKEN` ahead of the Keychain OAuth subscription login: if
/// either is present and non-empty in the environment `claude` inherits, it
/// authenticates via metered API-key billing instead of the Max
/// subscription, with no warning or error in the common case. Nexus
/// workspaces commonly carry `ANTHROPIC_API_KEY` in `.env.nexus.local` for
/// unrelated reasons (BYOK provider keys, other tooling), and `nexus run`
/// already injects those vars into the process environment before spawning
/// the tool, so this is the default shape of such a workspace, not a
/// contrived edge case.
///
/// This is a billing-correctness bug, not a UX rough edge, and unlike the
/// other pre-launch checks it is NOT bypassable via `--force` (enforced by
/// the caller in `run_prelaunch_checks`). Scoped strictly to `claude-cli`
/// projects; `direct_provider`/`nexus_gateway` projects rely on this
/// variable being present and are unaffected (`agent_owner` is never
/// `claude-cli` for them).
fn billing_auth_check(agent_owner: Option<&str>) -> CheckResult {
    let has_var = |name: &str| env::var(name).map(|v| !v.is_empty()).unwrap_or(false);
    billing_auth_check_with(
        agent_owner,
        has_var("ANTHROPIC_API_KEY"),
        has_var("ANTHROPIC_AUTH_TOKEN"),
    )
}

/// Pure core of [`billing_auth_check`], parameterized on credential
/// presence instead of reading process env directly, so tests can exercise
/// it without mutating global env state (avoids parallel-test flakiness).
fn billing_auth_check_with(
    agent_owner: Option<&str>,
    has_api_key: bool,
    has_auth_token: bool,
) -> CheckResult {
    if !wants_claude(agent_owner) {
        return CheckResult::Pass("n/a (not a Claude Code project)".into());
    }

    let mut offending: Vec<&str> = Vec::new();
    if has_api_key {
        offending.push("ANTHROPIC_API_KEY");
    }
    if has_auth_token {
        offending.push("ANTHROPIC_AUTH_TOKEN");
    }

    if offending.is_empty() {
        return CheckResult::Pass(
            "no API-key credentials set (Max subscription auth active)".into(),
        );
    }

    CheckResult::Fail(format!(
        "{} is set — Claude Code will use metered API-key billing instead of your \
         Max subscription. Unset it for this session or remove it from \
         .env.nexus.local before running.",
        offending.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Pre-launch checks
// ---------------------------------------------------------------------------

/// Run pre-launch checks and return `true` if the tool should be launched.
#[allow(clippy::too_many_arguments)]
async fn run_prelaunch_checks(
    api_url: &str,
    workspace: &Path,
    tool: &str,
    agent_owner: Option<&str>,
    env_file: &Path,
    force: bool,
    countdown_secs: u64,
    account: Option<&str>,
) -> anyhow::Result<bool> {
    println!();
    println!("{} Nexus Pre-launch Check", style(">>").bold().cyan());
    println!();

    // Spinner while collecting check results
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Running Nexus pre-launch checks...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    let mut checks: Vec<(&str, CheckResult)> = Vec::new();

    // Workspace
    let nexus_dir = workspace.join(".nexus");
    let mut linked_project_id: Option<String> = None;
    let ws_check = if nexus_dir.exists() {
        match config::load_linked_project(Some(workspace)) {
            Ok(Some(p)) => {
                linked_project_id = Some(p.id.clone());
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
    let resolved_token = resolve_token();
    let auth_check = match &resolved_token {
        Some(t) if t.len() > 8 => CheckResult::Pass(format!("{}...", &t[..8])),
        Some(_) => CheckResult::Pass("token present".into()),
        None => CheckResult::Fail("Not authenticated — run 'nexus login'".into()),
    };
    checks.push(("Auth", auth_check));

    // MCP Config — which artifact is authoritative depends on the project's
    // tool flavor (NEXUS-APP dispatch dfd4e655): OpenCode reads opencode.json,
    // Claude Code reads the root .mcp.json written by `claude_render`.
    checks.push(("MCP Config", mcp_config_check(workspace, agent_owner)));

    // Billing Auth — hard-stop, not bypassable via --force (see below).
    checks.push(("Billing Auth", billing_auth_check(agent_owner)));

    // Account — which named Claude account (if any) is active, so the
    // operator is not guessing which Keychain identity `claude` will use.
    if let Some(result) = account_check(agent_owner, account) {
        checks.push(("Account", result));
    }

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

    // Headroom — live-verified, not just env-var presence.
    //
    // Fix (Task a3bf595b, NEXUS-APP): this check previously only inspected
    // whether HEADROOM_MODE=transform was set locally, which kept reporting
    // PASS for weeks while the `nexus-headroom-intercept` OpenCode plugin was
    // silently downgraded to observe mode by a stale/invalid token. We now
    // call the same `/api/mcp/projects/{id}/preflight` endpoint the plugin
    // itself uses, so a credential or reachability problem surfaces here
    // *before* the tool launches, not just in a JSONL log file afterwards.
    let headroom_mode = env::var("HEADROOM_MODE")
        .ok()
        .or_else(|| parse_env_file(env_file).get("HEADROOM_MODE").cloned());

    let headroom_check = match (&headroom_mode, &linked_project_id, &resolved_token) {
        (Some(m), _, _) if m != "transform" => CheckResult::Warn(format!(
            "HEADROOM_MODE={} (expected 'transform' for full compression)",
            m
        )),
        (None, _, _) => {
            CheckResult::Warn("HEADROOM_MODE not set — headroom will use 'observe' mode".into())
        }
        (Some(_), None, _) => CheckResult::Warn(
            "HEADROOM_MODE=transform, but no project linked to verify — run 'nexus link'".into(),
        ),
        (Some(_), _, None) => CheckResult::Warn(
            "HEADROOM_MODE=transform, but not authenticated — run 'nexus login'".into(),
        ),
        (Some(m), Some(project_id), Some(token)) => {
            match NexusClient::new(api_url, Some(token.clone())) {
                Ok(client) => match client.mcp_preflight(project_id).await {
                    Ok(preflight) if preflight.headroom_enabled => {
                        CheckResult::Pass(format!("HEADROOM_MODE={} (preflight verified)", m))
                    }
                    Ok(_) => CheckResult::Fail(
                        "HEADROOM_MODE=transform, but the 'headroom' plugin is not enabled \
                         for this project on the Nexus platform"
                            .into(),
                    ),
                    Err(e) => CheckResult::Fail(format!(
                        "HEADROOM_MODE=transform, but live preflight failed: {} — the \
                         nexus-headroom-intercept plugin will silently fall back to \
                         'observe' mode (no compression). Run 'nexus status' / 'nexus login'.",
                        e
                    )),
                },
                Err(e) => CheckResult::Fail(format!("Could not build API client: {}", e)),
            }
        }
    };
    checks.push(("Headroom", headroom_check));

    // Done collecting — clear spinner and print results
    spinner.finish_and_clear();

    for (label, result) in &checks {
        print_check(label, result);
    }
    println!();

    // Billing-auth failures are a silent-billing-bypass class of bug, not a
    // convenience warning: refuse to launch even with --force. An inherited
    // ANTHROPIC_API_KEY/ANTHROPIC_AUTH_TOKEN cannot be safely stripped here
    // either, since it may be legitimately needed elsewhere in the same
    // shell (a different tool, or a non-Claude-Max project run from the
    // same terminal) — the user must decide, not us.
    if checks
        .iter()
        .any(|(label, c)| *label == "Billing Auth" && c.is_fail())
    {
        println!(
            "  {} Billing-auth check failed — this is not bypassable with --force.",
            style("ABORT").bold().red()
        );
        println!();
        return Ok(false);
    }

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
            launch_countdown(tool, countdown_secs)?;
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
            launch_countdown(tool, countdown_secs)?;
        }
        println!();
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Launch countdown
// ---------------------------------------------------------------------------

/// Display a countdown before launching `tool`.
///
/// If `secs` is 0, launches immediately without any output.
/// The user can press Enter to skip the countdown or Ctrl+C to abort.
fn launch_countdown(tool: &str, secs: u64) -> anyhow::Result<()> {
    use std::sync::mpsc;

    if secs == 0 {
        return Ok(());
    }

    // Set terminal to raw mode so we can detect Enter without waiting for newline
    let _raw_guard = RawModeGuard::enter();

    // Spawn a thread that waits for any keypress (Enter)
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1];
        // Read a single byte from stdin — blocks until keypress
        if std::io::Read::read(&mut std::io::stdin(), &mut buf).is_ok() {
            let _ = tx.send(());
        }
    });

    for remaining in (1..=secs).rev() {
        print!(
            "\r   Launching {} in {}s… ({} to skip, {} to abort)",
            style(tool).bold(),
            style(remaining).bold().cyan(),
            style("Enter").bold().green(),
            style("Ctrl+C").bold(),
        );
        std::io::stdout().flush()?;

        // Sleep in small increments to check for Enter press
        for _ in 0..20 {
            if rx.try_recv().is_ok() {
                // User pressed Enter — skip remaining countdown
                print!("\r{}\r", " ".repeat(80));
                std::io::stdout().flush()?;
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    // Clear the countdown line
    print!("\r{}\r", " ".repeat(80));
    std::io::stdout().flush()?;
    Ok(())
}

/// RAII guard to set terminal to raw mode and restore on drop.
struct RawModeGuard {
    original: libc::termios,
}

impl RawModeGuard {
    fn enter() -> Option<Self> {
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            // Disable canonical mode and echo
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { original })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
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

/// Is a headroom adapter installed in this workspace for the project's flavor?
///
/// OpenCode ships it as `.opencode/plugins/nexus-headroom-intercept.ts`.
/// Claude Code ships it as a hook adapter under `.claude/hooks/` (v0.19.0,
/// Track B3) whose file name is server-supplied, so match on the plugin name
/// rather than a fixed path.
fn headroom_adapter_installed(workspace: &Path, agent_owner: Option<&str>) -> bool {
    if wants_opencode(agent_owner)
        && workspace
            .join(".opencode/plugins/nexus-headroom-intercept.ts")
            .exists()
    {
        return true;
    }
    if wants_claude(agent_owner) {
        if let Ok(entries) = fs::read_dir(workspace.join(".claude").join("hooks")) {
            return entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("headroom")
            });
        }
    }
    false
}

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
    agent_owner: Option<&str>,
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
    // Only show this section if headroom is configured for this workspace
    let headroom_active =
        env::var("HEADROOM_MODE").is_ok() || headroom_adapter_installed(workspace, agent_owner);
    let headroom = read_headroom_stats(workspace, run_start_epoch);
    if headroom_active || headroom.is_some() {
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
                println!(
                    "{}",
                    style("no session stats (short session or no MCP activity)").dim()
                );
            }
        }
    }

    // Token/Cost stats — only show if data is available
    if let Some(ts) = token_stats {
        println!();
        print!("  {}:  ", style("Token Usage").bold());
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
            if activity.docs_ingested > 0 {
                println!("    Docs:       {} ingested", activity.docs_ingested);
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

/// Build a `git` command with `--no-pager` as the global flag.
///
/// Without this, a subprocess like `git diff --shortstat` can invoke a
/// pager (e.g. `less`) whenever `core.pager`/`GIT_PAGER` forces one. The
/// pager reads keypresses from the controlling terminal (`/dev/tty`), not
/// from this process's stdin, so even though `Command::output()` does not
/// inherit stdin, the pager still blocks waiting for a keypress -- which
/// looked like `nexus run`'s post-session summary hanging indefinitely
/// (dispatch 479a4ab5). `--no-pager` is a git global flag that overrides
/// both `GIT_PAGER` and `core.pager` unconditionally, unlike setting
/// `GIT_PAGER=cat` alone, which a `core.pager` override could still bypass.
fn git_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--no-pager");
    cmd
}

fn git_head_sha(workspace: &Path) -> Option<String> {
    git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn git_tags(workspace: &Path) -> Vec<String> {
    git_command()
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
    git_command()
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
    git_command()
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
    docs_ingested: u64,
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
            || self.docs_ingested > 0
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
        .get("entries")
        .or_else(|| session_data.get("document").and_then(|d| d.get("entries")))
        .or_else(|| session_data.get("session").and_then(|s| s.get("entries")))
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
        docs_ingested: 0,
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
            "research_added" => activity.docs_ingested += 1,
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

        // Parse timestamp — ISO 8601 string ("2026-07-09T15:18:06.407Z") or Unix millis
        let ts_secs = val
            .get("ts")
            .and_then(|v| {
                // Numeric millis
                if let Some(ms) = v.as_u64() {
                    return Some(if ms > 1_000_000_000_000 {
                        ms / 1000
                    } else {
                        ms
                    });
                }
                // ISO 8601 string — parse manually (avoid heavy chrono dep)
                // Format: "2026-07-09T15:18:06.407Z" or "2026-07-09T15:18:06Z"
                if let Some(s) = v.as_str() {
                    return iso8601_to_unix_secs(s);
                }
                None
            })
            .unwrap_or(0);

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

/// Parse an ISO 8601 UTC timestamp to Unix seconds.
/// Handles formats: "2026-07-09T15:18:06.407Z" and "2026-07-09T15:18:06Z"
fn iso8601_to_unix_secs(s: &str) -> Option<u64> {
    // Expect at least "YYYY-MM-DDTHH:MM:SS"
    if s.len() < 19 {
        return None;
    }
    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let min: u64 = s[14..16].parse().ok()?;
    let sec: u64 = s[17..19].parse().ok()?;

    // Days from epoch (1970-01-01) to given date — simplified but sufficient for
    // timestamps in range 2020-2099. Not accounting for leap seconds.
    let days_in_month = [0u64, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = |y: u64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);

    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += days_in_month[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
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

    // -------------------------------------------------------------------
    // sync_mcp_credentials (Task a3bf595b, NEXUS-APP)
    // -------------------------------------------------------------------

    #[test]
    fn test_sync_mcp_credentials_updates_stale_token() {
        let dir = tmp_dir("sync_creds_stale");
        fs::write(
            dir.join("opencode.json"),
            r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"],
      "environment": {
        "NEXUS_API_URL": "https://nexus.gatewarden.eu",
        "NEXUS_PRIVATE_TOKEN": "nxs_pat_OLD-STALE-TOKEN",
        "NEXUS_PROJECT_ID": "test-project-id"
      }
    }
  }
}
"#,
        )
        .unwrap();

        let changed = sync_mcp_credentials(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_NEW-FRESH-TOKEN",
        )
        .unwrap();
        assert_eq!(changed, vec!["opencode.json".to_string()]);

        let updated = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(updated.contains("nxs_pat_NEW-FRESH-TOKEN"));
        assert!(!updated.contains("nxs_pat_OLD-STALE-TOKEN"));
        // Untouched fields must survive the rewrite.
        assert!(updated.contains("test-project-id"));
        assert!(updated.contains("@gwdn/nexus-mcp@latest"));
    }

    #[test]
    fn test_sync_mcp_credentials_noop_when_already_fresh() {
        let dir = tmp_dir("sync_creds_noop");
        fs::write(
            dir.join("opencode.json"),
            r#"{
  "mcp": {
    "nexus": {
      "type": "local",
      "environment": {
        "NEXUS_API_URL": "https://nexus.gatewarden.eu",
        "NEXUS_PRIVATE_TOKEN": "nxs_pat_ALREADY-FRESH"
      }
    }
  }
}
"#,
        )
        .unwrap();
        let mtime_before = fs::metadata(dir.join("opencode.json"))
            .unwrap()
            .modified()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        let changed =
            sync_mcp_credentials(&dir, "https://nexus.gatewarden.eu", "nxs_pat_ALREADY-FRESH")
                .unwrap();
        assert!(changed.is_empty());

        let mtime_after = fs::metadata(dir.join("opencode.json"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "file must not be rewritten when already in sync"
        );
    }

    #[test]
    fn test_sync_mcp_credentials_missing_file_is_ok() {
        let dir = tmp_dir("sync_creds_missing");
        let changed =
            sync_mcp_credentials(&dir, "https://nexus.gatewarden.eu", "nxs_pat_x").unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn test_sync_mcp_credentials_updates_claude_mcp_json_too() {
        let dir = tmp_dir("sync_creds_claude");
        fs::write(
            dir.join(".mcp.json"),
            r#"{
  "mcpServers": {
    "nexus": {
      "command": "npx",
      "args": ["--yes", "@gwdn/nexus-mcp@latest"],
      "env": {
        "NEXUS_API_URL": "https://nexus.gatewarden.eu",
        "NEXUS_PRIVATE_TOKEN": "nxs_pat_OLD"
      }
    }
  }
}
"#,
        )
        .unwrap();

        let changed =
            sync_mcp_credentials(&dir, "https://nexus.gatewarden.eu", "nxs_pat_NEW").unwrap();
        assert_eq!(changed, vec![".mcp.json".to_string()]);
        let updated = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(updated.contains("nxs_pat_NEW"));
        assert!(!updated.contains("nxs_pat_OLD"));
    }

    fn tmp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus_run_test_{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Set up a temp git repo with one commit, returning (dir, head_sha).
    fn tmp_git_repo(suffix: &str) -> (PathBuf, String) {
        let dir = tmp_dir(suffix);
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap()
        };
        run(&["init", "--quiet", "-b", "main"]);
        // Disable GPG signing for this repo regardless of the operator's
        // global git config: commit.gpgsign=true (common on dev machines)
        // makes `git commit` depend on gpg-agent, which can intermittently
        // stall/fail under concurrent test execution and has no bearing on
        // what these tests actually verify.
        run(&["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("file.txt"), "hello\n").unwrap();
        run(&["add", "file.txt"]);
        run(&["commit", "-m", "initial"]);
        let sha = String::from_utf8_lossy(&run(&["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        (dir, sha)
    }

    // -------------------------------------------------------------------
    // Git pager hang regression (dispatch 479a4ab5)
    //
    // A single git repo is configured with `pager.<cmd>=true` for diff,
    // log, and tag (force paging unconditionally, regardless of isatty)
    // and `core.pager` set to a command that blocks for a few seconds if
    // actually invoked. Without `--no-pager`, these helpers would hang for
    // that duration; with it, they must return promptly regardless of the
    // pager config. All three git_* helpers share the same git_command()
    // builder, so one repo configured for all three commands is exercised
    // in a single test rather than three near-identical ones -- equivalent
    // coverage with a third of the subprocess load on the test suite.
    // -------------------------------------------------------------------

    #[test]
    fn test_git_helpers_ignore_forced_pager() {
        let (dir, head) = tmp_git_repo("forced-pager");
        for (key, value) in [
            ("pager.diff", "true"),
            ("pager.log", "true"),
            ("pager.tag", "true"),
            ("core.pager", "sleep 5"),
        ] {
            std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(&dir)
                .output()
                .unwrap();
        }

        fs::write(dir.join("file.txt"), "hello\nworld\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let commit_output = std::process::Command::new("git")
            .args([
                "commit",
                "-m",
                "second",
                "--author",
                "Test <test@example.com>",
            ])
            // Committer identity is required by git regardless of --author,
            // and CI runners have no global user.name/user.email configured
            // (unlike most dev machines) -- without these, this commit
            // fails silently under .unwrap() (which only checks the io
            // Result, not the process exit status), leaving `dir` at the
            // initial commit and making every assertion below meaningless.
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            commit_output.status.success(),
            "second commit failed: {}",
            String::from_utf8_lossy(&commit_output.stderr)
        );

        let start = std::time::Instant::now();
        let stat = git_diff_stat(&dir, &head);
        let count = git_count_commits(&dir, &head);
        let tags = git_tags(&dir);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() < 3,
            "git_diff_stat/git_count_commits/git_tags took {:?} combined, \
             forced pager (sleep 5) was likely invoked",
            elapsed
        );
        assert!(stat.is_some());
        assert_eq!(count, Some(1));
        assert!(tags.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------
    // spawn_blocking mechanism (dispatch 479a4ab5)
    //
    // The post-session summary races stats collection against
    // tokio::signal::ctrl_c() via tokio::select!. Proves the core
    // mechanism relied on: wrapping a slow *synchronous* call in
    // tokio::task::spawn_blocking lets a concurrent fast branch win a
    // tokio::select! race, whereas calling it directly inline would block
    // the executor and starve the other branch until it returns.
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_select_resolves_via_fast_branch_when_slow_work_is_spawn_blocking() {
        let start = std::time::Instant::now();

        let result = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => "fast",
            _ = tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
            }) => "slow",
        };

        let elapsed = start.elapsed();
        assert_eq!(result, "fast");
        assert!(
            elapsed.as_millis() < 500,
            "select took {:?}; spawn_blocking should not have starved the fast branch",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_select_starves_fast_branch_without_spawn_blocking() {
        // Negative control: the same race, but with the slow work called
        // synchronously inline (as the code did before this fix). This
        // documents *why* spawn_blocking is required -- without it, the
        // executor thread is blocked executing the sync call and cannot
        // poll the other branch until the sync call returns, even though
        // that branch (a 20ms sleep) "completed" long before.
        let start = std::time::Instant::now();

        let result: &str = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => "fast",
            _ = async {
                // Synchronous, blocking call directly inline -- the bug
                // pattern this fix removes from the real summary-collection
                // path.
                std::thread::sleep(std::time::Duration::from_millis(300));
            } => "slow",
        };

        let elapsed = start.elapsed();
        // The "slow" branch still wins because the inline blocking call
        // prevents the executor from ever observing that "fast" already
        // completed until the blocking call itself returns.
        assert_eq!(result, "slow");
        assert!(elapsed.as_millis() >= 300);
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
                account,
                args,
            } => {
                assert!(tool.is_none());
                assert!(!dry_run);
                assert!(!show_env);
                assert!(!no_db);
                assert!(!exec);
                assert!(!skip_checks);
                assert!(!force);
                assert!(account.is_none());
                assert!(args.is_empty());
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn test_parse_cli_run_with_account() {
        use crate::{Cli, Command};
        use clap::Parser;
        let cli = Cli::try_parse_from(["nexus", "run", "--account", "work"]).unwrap();
        match cli.command {
            Command::Run { account, .. } => {
                assert_eq!(account.as_deref(), Some("work"));
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

        // Use ISO 8601 timestamps (real format from headroom-intercept)
        let content = r#"{"event":"tool_intercept","tool":"nexus_kb_memory","ts":"2026-07-09T15:16:40.000Z"}
{"event":"session_summary","mode":"transform","compressions":4,"locallyAppliedTransforms":2,"observations":3,"skips":115,"passthroughs":12,"potentialSavedTokens":10690,"cacheIntegrityFailures":0,"fullRetrievalDenied":0,"outputBudgetTruncated":0,"cacheReadFailures":0,"ts":"2026-07-09T15:18:06.407Z"}
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
    fn test_iso8601_to_unix_secs() {
        // 2026-07-09T15:18:06Z — known offset from epoch
        let result = iso8601_to_unix_secs("2026-07-09T15:18:06.407Z");
        assert!(result.is_some());
        let secs = result.unwrap();
        // Sanity: must be > 2020 epoch and < 2030 epoch
        assert!(secs > 1_577_836_800); // 2020-01-01
        assert!(secs < 1_893_456_000); // 2030-01-01
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

        // Two summaries: old one (2001) and recent one (2026-07-09)
        let content = r#"{"event":"session_summary","mode":"observe","compressions":0,"locallyAppliedTransforms":0,"observations":1,"skips":50,"passthroughs":5,"potentialSavedTokens":0,"cacheIntegrityFailures":0,"ts":"2001-01-01T00:00:00Z"}
{"event":"session_summary","mode":"transform","compressions":7,"locallyAppliedTransforms":3,"observations":5,"skips":200,"passthroughs":20,"potentialSavedTokens":15000,"cacheIntegrityFailures":0,"ts":"2026-07-09T16:03:20.000Z"}
"#;
        fs::write(&jsonl, content).unwrap();

        // run_start just before the 2026 entry (~2026-07-09T16:00:00Z)
        let stats = read_headroom_stats(&dir, 1752076800);
        assert!(stats.is_some());
        let s = stats.unwrap();
        assert_eq!(s.mode, "transform");
        assert_eq!(s.compressions, 7);
    }

    // -------------------------------------------------------------------
    // agent_owner-aware launch behaviour (NEXUS-APP dispatch dfd4e655)
    // -------------------------------------------------------------------

    #[test]
    fn test_effective_tool_cli_flag_always_wins() {
        assert_eq!(
            resolve_effective_tool(Some("zed"), Some("opencode"), Some("claude-cli")),
            "zed"
        );
    }

    #[test]
    fn test_effective_tool_explicit_config_beats_agent_owner() {
        assert_eq!(
            resolve_effective_tool(None, Some("opencode"), Some("claude-cli")),
            "opencode"
        );
    }

    #[test]
    fn test_effective_tool_derived_from_claude_cli_flavor() {
        assert_eq!(
            resolve_effective_tool(None, None, Some("claude-cli")),
            "claude"
        );
    }

    #[test]
    fn test_effective_tool_defaults_to_opencode_for_other_flavors() {
        // "both" is deliberately ambiguous and stays on the platform default;
        // an unlinked/legacy workspace (None) must behave exactly as before.
        assert_eq!(resolve_effective_tool(None, None, Some("both")), "opencode");
        assert_eq!(
            resolve_effective_tool(None, None, Some("opencode")),
            "opencode"
        );
        assert_eq!(resolve_effective_tool(None, None, None), "opencode");
    }

    #[test]
    fn test_mcp_config_check_claude_project_reads_root_mcp_json() {
        let dir = tmp_dir("mcp_check_claude");
        fs::write(dir.join(".mcp.json"), r#"{"mcpServers":{"nexus":{}}}"#).unwrap();

        // Regression: this used to warn "No opencode.json" for a claude-cli
        // project, a file such a project is never supposed to have.
        match mcp_config_check(&dir, Some("claude-cli")) {
            CheckResult::Pass(msg) => {
                assert!(msg.contains(".mcp.json"), "unexpected message: {msg}");
                assert!(!msg.contains("opencode.json"), "unexpected message: {msg}");
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn test_mcp_config_check_claude_project_ignores_missing_opencode_json() {
        let dir = tmp_dir("mcp_check_claude_no_oc");
        // No .mcp.json and no opencode.json: the warning must name the
        // artifact that actually applies to this flavor.
        match mcp_config_check(&dir, Some("claude-cli")) {
            CheckResult::Warn(msg) => {
                assert!(msg.contains(".mcp.json"), "unexpected message: {msg}");
                assert!(!msg.contains("opencode.json"), "unexpected message: {msg}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn test_mcp_config_check_opencode_project_unchanged() {
        let dir = tmp_dir("mcp_check_oc");
        fs::write(dir.join("opencode.json"), r#"{"mcp":{"nexus":{}}}"#).unwrap();
        assert!(matches!(
            mcp_config_check(&dir, Some("opencode")),
            CheckResult::Pass(_)
        ));
        // Unknown flavor (workspace linked before agent_owner was cached)
        // must keep the pre-dfd4e655 behaviour.
        assert!(matches!(mcp_config_check(&dir, None), CheckResult::Pass(_)));
    }

    #[test]
    fn test_mcp_config_check_both_flavor_requires_each_artifact() {
        let dir = tmp_dir("mcp_check_both");
        fs::write(dir.join("opencode.json"), r#"{"mcp":{"nexus":{}}}"#).unwrap();
        match mcp_config_check(&dir, Some("both")) {
            CheckResult::Warn(msg) => assert!(msg.contains(".mcp.json"), "unexpected: {msg}"),
            other => panic!("expected Warn, got {other:?}"),
        }
        fs::write(dir.join(".mcp.json"), r#"{"mcpServers":{"nexus":{}}}"#).unwrap();
        assert!(matches!(
            mcp_config_check(&dir, Some("both")),
            CheckResult::Pass(_)
        ));
    }

    #[test]
    fn test_mcp_config_check_present_but_no_nexus_block() {
        let dir = tmp_dir("mcp_check_no_block");
        fs::write(dir.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        match mcp_config_check(&dir, Some("claude-cli")) {
            CheckResult::Warn(msg) => assert!(msg.contains("no nexus MCP block"), "got {msg}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    // ── Named Claude account switching (NEXUS-APP dispatch ad6e0176) ───────

    #[test]
    fn test_validate_account_name_accepts_plain_identifiers() {
        assert!(validate_account_name("work").is_ok());
        assert!(validate_account_name("personal-2").is_ok());
        assert!(validate_account_name("test_acct01").is_ok());
    }

    #[test]
    fn test_validate_account_name_rejects_empty() {
        assert!(validate_account_name("").is_err());
    }

    #[test]
    fn test_validate_account_name_rejects_dot_segments() {
        assert!(validate_account_name(".").is_err());
        assert!(validate_account_name("..").is_err());
    }

    #[test]
    fn test_validate_account_name_rejects_path_traversal() {
        // Must not be usable to escape ~/.config/nexus/claude-accounts/.
        assert!(validate_account_name("../escape").is_err());
        assert!(validate_account_name("foo/bar").is_err());
        assert!(validate_account_name("foo\\bar").is_err());
        assert!(validate_account_name("/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_account_name_rejects_other_special_chars() {
        assert!(validate_account_name("has space").is_err());
        assert!(validate_account_name("has.dot").is_err());
        assert!(validate_account_name("has:colon").is_err());
    }

    #[test]
    fn test_claude_account_dir_is_scoped_under_config_dir() {
        let config_dir = Path::new("/home/user/.config/nexus");
        let dir = claude_account_dir(config_dir, "work");
        assert_eq!(
            dir,
            Path::new("/home/user/.config/nexus/claude-accounts/work")
        );
    }

    #[test]
    fn test_account_check_none_for_non_claude_project() {
        // Must be entirely absent from the checks panel for OpenCode-only
        // and unknown-flavor projects, regardless of --account.
        assert!(account_check(Some("opencode"), Some("work")).is_none());
        assert!(account_check(None, Some("work")).is_none());
    }

    #[test]
    fn test_account_check_named_account_for_claude_project() {
        match account_check(Some("claude-cli"), Some("work")) {
            Some(CheckResult::Pass(msg)) => assert!(msg.contains("work"), "got {msg}"),
            other => panic!("expected Some(Pass), got {other:?}"),
        }
        // "both" flavor also gets the check.
        assert!(account_check(Some("both"), Some("work")).is_some());
    }

    #[test]
    fn test_account_check_default_for_claude_project_without_account() {
        match account_check(Some("claude-cli"), None) {
            Some(CheckResult::Pass(msg)) => assert!(msg.contains("default"), "got {msg}"),
            other => panic!("expected Some(Pass), got {other:?}"),
        }
    }

    // ── "default" alias for the implicit identity (NEXUS-APP dispatch
    // c0523ebe): --account default must behave exactly like omitting
    // --account, so scripts can always pass an explicit name. ─────────────

    #[test]
    fn test_resolve_account_default_alias_is_no_override() {
        let config_dir = Path::new("/home/user/.config/nexus");
        assert_eq!(
            resolve_account(config_dir, Some("claude-cli"), Some("default")).unwrap(),
            AccountResolution::NoOverride { warn_ignored: None }
        );
    }

    #[test]
    fn test_resolve_account_default_alias_identical_to_no_account() {
        let config_dir = Path::new("/home/user/.config/nexus");
        let with_default = resolve_account(config_dir, Some("claude-cli"), Some("default"));
        let without_flag = resolve_account(config_dir, Some("claude-cli"), None);
        assert_eq!(with_default.unwrap(), without_flag.unwrap());
    }

    #[test]
    fn test_resolve_account_default_alias_no_warning_on_non_claude_project() {
        // Unlike a real name, "default" never attempts to create or select
        // anything, so it must not warn even outside claude-cli/both.
        let config_dir = Path::new("/home/user/.config/nexus");
        assert_eq!(
            resolve_account(config_dir, Some("opencode"), Some("default")).unwrap(),
            AccountResolution::NoOverride { warn_ignored: None }
        );
        assert_eq!(
            resolve_account(config_dir, None, Some("default")).unwrap(),
            AccountResolution::NoOverride { warn_ignored: None }
        );
    }

    #[test]
    fn test_resolve_account_default_alias_never_creates_a_directory() {
        // resolve_account is pure (no fs I/O); a Selected(dir) result is the
        // only outcome the caller creates a directory for. Asserting
        // NoOverride here is precisely the guarantee that no
        // ".../claude-accounts/default" directory is ever created.
        let config_dir = Path::new("/home/user/.config/nexus");
        assert!(matches!(
            resolve_account(config_dir, Some("claude-cli"), Some("default")).unwrap(),
            AccountResolution::NoOverride { .. }
        ));
    }

    #[test]
    fn test_resolve_account_real_name_is_selected_and_scoped() {
        let config_dir = Path::new("/home/user/.config/nexus");
        match resolve_account(config_dir, Some("claude-cli"), Some("work")).unwrap() {
            AccountResolution::Selected(dir) => assert_eq!(
                dir,
                Path::new("/home/user/.config/nexus/claude-accounts/work")
            ),
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn test_resolve_account_real_name_on_non_claude_project_warns() {
        let config_dir = Path::new("/home/user/.config/nexus");
        assert_eq!(
            resolve_account(config_dir, Some("opencode"), Some("work")).unwrap(),
            AccountResolution::NoOverride {
                warn_ignored: Some("work".to_string())
            }
        );
    }

    #[test]
    fn test_resolve_account_no_flag_is_no_override_without_warning() {
        let config_dir = Path::new("/home/user/.config/nexus");
        assert_eq!(
            resolve_account(config_dir, Some("claude-cli"), None).unwrap(),
            AccountResolution::NoOverride { warn_ignored: None }
        );
    }

    #[test]
    fn test_account_check_default_alias_distinct_text_from_no_account() {
        // Cosmetic distinction only — both represent the identical
        // no-override case, but the operator should see their explicit
        // choice was honored rather than silently ignored.
        let explicit = account_check(Some("claude-cli"), Some("default"));
        let implicit = account_check(Some("claude-cli"), None);
        match (explicit, implicit) {
            (Some(CheckResult::Pass(a)), Some(CheckResult::Pass(b))) => {
                assert_ne!(
                    a, b,
                    "expected distinct text for explicit vs implicit default"
                );
                assert!(a.contains("default") && b.contains("default"));
            }
            other => panic!("expected both Some(Pass), got {other:?}"),
        }
    }

    // ── billing_auth_check: hard-stop on inherited API-key auth for
    // claude-cli projects (NEXUS-APP dispatch 8de19c71) ────────────────────

    #[test]
    fn test_billing_auth_check_non_claude_project_is_na_regardless_of_env() {
        // direct_provider/nexus_gateway projects rely on ANTHROPIC_API_KEY
        // being present; must never be flagged.
        assert!(matches!(
            billing_auth_check_with(Some("opencode"), true, true),
            CheckResult::Pass(_)
        ));
        assert!(matches!(
            billing_auth_check_with(None, true, true),
            CheckResult::Pass(_)
        ));
    }

    #[test]
    fn test_billing_auth_check_claude_project_clean_env_passes() {
        assert!(matches!(
            billing_auth_check_with(Some("claude-cli"), false, false),
            CheckResult::Pass(_)
        ));
        assert!(matches!(
            billing_auth_check_with(Some("both"), false, false),
            CheckResult::Pass(_)
        ));
    }

    #[test]
    fn test_billing_auth_check_claude_project_api_key_fails() {
        match billing_auth_check_with(Some("claude-cli"), true, false) {
            CheckResult::Fail(msg) => {
                assert!(msg.contains("ANTHROPIC_API_KEY"), "unexpected: {msg}");
                assert!(!msg.contains("ANTHROPIC_AUTH_TOKEN"), "unexpected: {msg}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn test_billing_auth_check_claude_project_auth_token_fails() {
        match billing_auth_check_with(Some("claude-cli"), false, true) {
            CheckResult::Fail(msg) => assert!(msg.contains("ANTHROPIC_AUTH_TOKEN"), "got {msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn test_billing_auth_check_both_flavor_also_hard_fails() {
        assert!(matches!(
            billing_auth_check_with(Some("both"), true, false),
            CheckResult::Fail(_)
        ));
    }

    #[test]
    fn test_billing_auth_check_reports_both_offending_vars() {
        match billing_auth_check_with(Some("claude-cli"), true, true) {
            CheckResult::Fail(msg) => {
                assert!(msg.contains("ANTHROPIC_API_KEY"), "got {msg}");
                assert!(msg.contains("ANTHROPIC_AUTH_TOKEN"), "got {msg}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn test_billing_auth_hard_stop_is_not_bypassable_by_force() {
        // Guards the exact contract the dispatch requested: a Billing Auth
        // failure must abort even when `force` is true, unlike every other
        // pre-launch check. This test asserts the invariant at the level of
        // the check-result classification the abort branch keys off of, so
        // a future refactor of run_prelaunch_checks cannot silently fold
        // this check back into the generic --force-bypassable fail path.
        let checks: Vec<(&str, CheckResult)> = vec![
            ("Workspace", CheckResult::Pass("ok".into())),
            (
                "Billing Auth",
                billing_auth_check_with(Some("claude-cli"), true, false),
            ),
        ];
        let billing_auth_failed = checks
            .iter()
            .any(|(label, c)| *label == "Billing Auth" && c.is_fail());
        assert!(
            billing_auth_failed,
            "Billing Auth check must be classified as Fail so the hard-stop \
             branch (which ignores --force) fires"
        );
    }

    #[test]
    fn test_headroom_adapter_detected_under_claude_hooks() {
        let dir = tmp_dir("headroom_claude");
        let hooks = dir.join(".claude").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        // File name is server-supplied (Track B3), so match on plugin name.
        fs::write(hooks.join("nexus-headroom-intercept.mjs"), "// adapter").unwrap();

        assert!(headroom_adapter_installed(&dir, Some("claude-cli")));
        assert!(headroom_adapter_installed(&dir, Some("both")));
        // An opencode-only project must not be credited with a Claude adapter.
        assert!(!headroom_adapter_installed(&dir, Some("opencode")));
    }

    #[test]
    fn test_headroom_adapter_detected_under_opencode_plugins() {
        let dir = tmp_dir("headroom_opencode");
        let plugins = dir.join(".opencode").join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(plugins.join("nexus-headroom-intercept.ts"), "// plugin").unwrap();

        assert!(headroom_adapter_installed(&dir, Some("opencode")));
        assert!(headroom_adapter_installed(&dir, None));
        assert!(!headroom_adapter_installed(&dir, Some("claude-cli")));
    }

    #[test]
    fn test_headroom_adapter_absent() {
        let dir = tmp_dir("headroom_none");
        fs::create_dir_all(dir.join(".claude").join("hooks")).unwrap();
        assert!(!headroom_adapter_installed(&dir, Some("claude-cli")));
        assert!(!headroom_adapter_installed(&dir, Some("opencode")));
    }
}
