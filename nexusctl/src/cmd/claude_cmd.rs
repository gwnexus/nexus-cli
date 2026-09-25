//! `nexus claude status | diff | launch` (NEXUS-APP ADR-0117, dispatch
//! 99f335e8).
//!
//! `status` and `diff` are read-only: the only network call is `af_export`,
//! and every classification reuses exactly what `nexus pull` would do
//! ([`ccx::plan_files`], [`claude_render::apply_generic_settings`] on a
//! copy, the `CLAUDE.md` block rule), so they never disagree with a pull.

use std::fs;
use std::path::PathBuf;

use console::style;
use nexus_core::api::{AgentFileExportResponse, NexusClient};
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;

use super::ccx::{self, FilePlan, FileState};
use super::claude_render;

/// Everything `status`/`diff` report on, computed without writing.
struct CcxView {
    export: AgentFileExportResponse,
    lock: Option<ccx::CcxLock>,
    plans: Vec<FilePlan>,
    /// `.claude/settings.json` as it is now.
    settings_before: serde_json::Value,
    /// `.claude/settings.json` as the next pull would leave it.
    settings_after: serde_json::Value,
    block: Option<BlockView>,
}

/// The `CLAUDE.md` managed block: local text between the markers vs. the
/// text a pull would write.
struct BlockView {
    state: FileState,
    local: Option<String>,
    desired: String,
}

impl CcxView {
    /// Managed settings keys whose value would change on the next pull
    /// (including keys that would be removed).
    fn settings_pending(&self) -> Vec<String> {
        let mut keys: Vec<&str> = Vec::new();
        let previous = self.lock.as_ref().and_then(|l| l.settings.as_ref());
        for spec in [self.export.claude_settings.as_ref(), previous]
            .into_iter()
            .flatten()
        {
            for key in &spec.managed_keys {
                if !keys.contains(&key.as_str()) {
                    keys.push(key);
                }
            }
        }
        keys.into_iter()
            .filter(|k| {
                ccx::json_get_path(&self.settings_before, k)
                    != ccx::json_get_path(&self.settings_after, k)
            })
            .map(str::to_string)
            .collect()
    }

    fn revision_pending(&self) -> bool {
        let desired = self.export.ccx.as_ref().map(|c| c.revision.as_str());
        let locked = self.lock.as_ref().and_then(|l| l.revision.as_deref());
        desired.is_some() && desired != locked
    }

    /// Exit-code rule shared by `status` and `diff`: anything a pull would
    /// change, or anything it would refuse to change, counts as pending.
    fn is_pending(&self) -> bool {
        self.revision_pending()
            || self.plans.iter().any(|p| p.state != FileState::Clean)
            || self
                .block
                .as_ref()
                .is_some_and(|b| b.state != FileState::Clean)
            || !self.settings_pending().is_empty()
    }
}

async fn load_view(
    api_url: &str,
    cli_project_id: Option<&str>,
) -> anyhow::Result<(PathBuf, CcxView)> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(cli_project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;
    let export = client.export_agent_files(&project_id).await?;

    let lock = ccx::load_lock(&workspace, &export.agentic_root);
    let plans = if export.ccx.is_some() {
        let manifest = super::sync::load_manifest_pub(&workspace);
        ccx::plan_files(
            &workspace,
            &ccx::ccx_files(&export.agent_files),
            lock.as_ref(),
            &manifest,
        )?
    } else {
        Vec::new()
    };

    let settings_before = claude_render::read_claude_settings(&workspace);
    let mut settings_after = settings_before.clone();
    claude_render::apply_generic_settings(
        &mut settings_after,
        export.claude_settings.as_ref(),
        lock.as_ref().and_then(|l| l.settings.as_ref()),
    );

    let block = export.claude_md_managed_block.as_deref().map(|b| {
        let claude_md = fs::read_to_string(workspace.join("CLAUDE.md")).ok();
        block_view(
            claude_md.as_deref(),
            b,
            lock.as_ref()
                .and_then(|l| l.claude_md_block_sha256.as_deref()),
        )
    });

    Ok((
        workspace,
        CcxView {
            export,
            lock,
            plans,
            settings_before,
            settings_after,
            block,
        },
    ))
}

/// Classify the `CLAUDE.md` block with the same rule the pull applies:
/// without a lock hash a differing block is simply replaced (UPDATE), with
/// one a local edit is a DRIFTED/CONFLICT.
fn block_view(claude_md: Option<&str>, block: &str, locked: Option<&str>) -> BlockView {
    let desired = claude_render::claude_md_desired_block_text(block);
    let local = claude_md
        .and_then(claude_render::claude_md_block_text)
        .map(str::to_string);
    let state = match ccx::classify_file_state(
        Some(&sha256_hex(&desired)),
        local.as_deref().map(sha256_hex).as_deref(),
        locked,
    ) {
        FileState::Unmanaged => FileState::Updated,
        state => state,
    };
    BlockView {
        state,
        local,
        desired,
    }
}

// ---------------------------------------------------------------------------
// nexus claude status
// ---------------------------------------------------------------------------

/// `nexus claude status`. Returns the process exit code: 0 when everything
/// is clean, 1 when changes are pending or conflicts exist.
pub async fn status(
    api_url: &str,
    cli_project_id: Option<&str>,
    json: bool,
) -> anyhow::Result<i32> {
    let (workspace, view) = load_view(api_url, cli_project_id).await?;

    let claude_version = claude_code_version();
    let compat_range = view
        .export
        .ccx
        .as_ref()
        .and_then(|c| c.compatibility.claude_code.clone());
    let compatible = match (&claude_version, &compat_range) {
        (Some(v), Some(range)) => version_satisfies(v, range),
        _ => None,
    };
    let plugins = enabled_plugins(&view.settings_before);
    let installed = installed_plugins();
    let headroom = super::run::read_headroom_stats(&workspace, 0);
    let settings_pending = view.settings_pending();
    let pending = view.is_pending();

    if json {
        let out = serde_json::json!({
            "ccx": view.export.ccx,
            "lock": view.lock.as_ref().map(|l| serde_json::json!({
                "bundle": l.bundle,
                "version": l.version,
                "revision": l.revision,
                "applied_at": l.applied_at,
            })),
            "files": view.plans.iter().map(|p| serde_json::json!({
                "path": p.target_path,
                "file_key": p.file_key,
                "state": ccx::state_label(p.state),
            })).collect::<Vec<_>>(),
            "claude_md_block": view.block.as_ref().map(|b| ccx::state_label(b.state)),
            "settings": {
                "managed_keys": view.export.claude_settings.as_ref().map(|s| &s.managed_keys),
                "pending_keys": settings_pending,
            },
            "claude_code": {
                "version": claude_version,
                "compatibility": compat_range,
                "compatible": compatible,
            },
            "plugins": plugins.iter().map(|p| serde_json::json!({
                "id": p,
                "installed": installed.as_ref().map(|i| plugin_installed(i, p)),
            })).collect::<Vec<_>>(),
            "headroom": headroom.as_ref().map(|h| serde_json::json!({
                "mode": h.mode,
                "compressions": h.compressions,
                "potential_saved_tokens": h.potential_saved_tokens,
            })),
            "pending": pending,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(i32::from(pending));
    }

    println!();
    match &view.export.ccx {
        Some(info) => println!(
            "{} {}@{} ({})",
            style("Claude Code Experience:").bold(),
            info.bundle,
            info.version,
            info.revision
        ),
        None => println!(
            "{} not enabled for this project",
            style("Claude Code Experience:").bold()
        ),
    }
    match view.lock.as_ref().and_then(|l| l.revision.as_deref()) {
        Some(rev) => {
            let lock = view.lock.as_ref().expect("revision implies lock");
            println!(
                "  Lock:     {}@{} ({}){}",
                lock.bundle.as_deref().unwrap_or("?"),
                lock.version.as_deref().unwrap_or("?"),
                rev,
                if view.revision_pending() {
                    style("   new revision available, run nexus pull")
                        .yellow()
                        .to_string()
                } else {
                    String::new()
                }
            );
        }
        None if view.export.ccx.is_some() => {
            println!("  Lock:     none yet, run nexus pull")
        }
        None => {}
    }

    if !view.plans.is_empty() {
        println!();
        println!("  {}", style("Files").bold());
        for plan in &view.plans {
            print_state_line(plan.state, &plan.target_path);
        }
    }
    if let Some(ref block) = view.block {
        print_state_line(block.state, "CLAUDE.md (nexus-managed block)");
    }

    if let Some(ref spec) = view.export.claude_settings {
        println!();
        println!("  {}", style("Settings (.claude/settings.json)").bold());
        for key in &spec.managed_keys {
            if settings_pending.contains(key) {
                println!(
                    "    {} {}",
                    style(format!("{:<9}", "PENDING")).yellow(),
                    key
                );
            } else {
                println!("    {} {}", style(format!("{:<9}", "OK")).dim(), key);
            }
        }
        for key in settings_pending
            .iter()
            .filter(|k| !spec.managed_keys.contains(k))
        {
            println!(
                "    {} {}   {}",
                style(format!("{:<9}", "REMOVE")).yellow(),
                key,
                style("no longer managed").dim()
            );
        }
    }

    println!();
    match (&claude_version, &compat_range, compatible) {
        (None, _, _) => println!(
            "  Claude Code: {}",
            style("not found (claude --version failed)").yellow()
        ),
        (Some(v), Some(range), Some(false)) => println!(
            "  Claude Code: {} {}",
            v,
            style(format!("outside supported range {range}")).yellow()
        ),
        (Some(v), Some(range), _) => println!("  Claude Code: {v} (supported: {range})"),
        (Some(v), None, _) => println!("  Claude Code: {v}"),
    }

    if !plugins.is_empty() {
        println!("  Plugins:");
        for plugin in &plugins {
            let state = match installed.as_ref().map(|i| plugin_installed(i, plugin)) {
                Some(true) => style("installed".to_string()).green(),
                Some(false) => style("NOT INSTALLED".to_string()).yellow(),
                None => style("unknown (claude plugin list unavailable)".to_string()).dim(),
            };
            println!("    {plugin}: {state}");
        }
    }

    if let Some(h) = headroom {
        println!(
            "  Headroom (last session): mode {}, {} compression(s), ~{} tokens saved",
            h.mode, h.compressions, h.potential_saved_tokens
        );
    }

    println!();
    if pending {
        println!(
            "{} Changes pending: run {} (or {} to inspect).",
            style("!").bold().yellow(),
            style("nexus pull").bold(),
            style("nexus claude diff").bold()
        );
    } else {
        println!("{} Everything is clean.", style("OK").bold().green());
    }
    Ok(i32::from(pending))
}

fn print_state_line(state: FileState, label: &str) {
    let text = format!("{:<9}", ccx::state_label(state));
    let text = match state {
        FileState::Clean => style(text).dim(),
        FileState::Create | FileState::Updated | FileState::Adopt => style(text).green(),
        _ => style(text).yellow(),
    };
    println!("    {text} {label}");
}

/// `claude --version`, e.g. `"2.1.260 (Claude Code)"` -> `"2.1.260"`.
fn claude_code_version() -> Option<String> {
    let out = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find(|t| parse_version(t).is_some())
        .map(str::to_string)
}

fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `version` satisfies a space-separated comparator range such as
/// `">=2.1.257 <3.0.0"`. `None` if either side cannot be parsed.
fn version_satisfies(version: &str, range: &str) -> Option<bool> {
    let v = parse_version(version)?;
    for comparator in range.split_whitespace() {
        let (op, rest) = [">=", "<=", ">", "<", "="]
            .iter()
            .find_map(|op| comparator.strip_prefix(op).map(|rest| (*op, rest)))
            .unwrap_or(("=", comparator));
        let bound = parse_version(rest)?;
        let ok = match op {
            ">=" => v >= bound,
            "<=" => v <= bound,
            ">" => v > bound,
            "<" => v < bound,
            _ => v == bound,
        };
        if !ok {
            return Some(false);
        }
    }
    Some(true)
}

/// Plugin ids enabled in `settings.json` (`enabledPlugins: { id: true }`).
fn enabled_plugins(settings: &serde_json::Value) -> Vec<String> {
    settings
        .get("enabledPlugins")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, on)| on.as_bool() == Some(true))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Installed plugin ids from `claude plugin list --json`; `None` when the
/// command is unavailable or its output cannot be read.
fn installed_plugins() -> Option<Vec<String>> {
    let out = std::process::Command::new("claude")
        .args(["plugin", "list", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_plugin_list(&serde_json::from_slice(&out.stdout).ok()?)
}

/// Accepts an array of plugin objects (`id`, or `name` plus optional
/// `marketplace`), an object wrapping such an array under `plugins`, or an
/// object keyed by plugin id.
fn parse_plugin_list(value: &serde_json::Value) -> Option<Vec<String>> {
    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(map) => match map.get("plugins") {
            Some(serde_json::Value::Array(items)) => items,
            _ => return Some(map.keys().cloned().collect()),
        },
        _ => return None,
    };
    Some(
        items
            .iter()
            .filter_map(|item| {
                if let Some(id) = item.as_str() {
                    return Some(id.to_string());
                }
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    return Some(id.to_string());
                }
                let name = item.get("name")?.as_str()?;
                Some(match item.get("marketplace").and_then(|v| v.as_str()) {
                    Some(m) => format!("{name}@{m}"),
                    None => name.to_string(),
                })
            })
            .collect(),
    )
}

/// `nexus-core@gatewarden-nexus` matches an installed entry with the same
/// id, or with the same name when the list omits the marketplace.
fn plugin_installed(installed: &[String], id: &str) -> bool {
    let name = id.split('@').next().unwrap_or(id);
    installed.iter().any(|i| i == id || i == name)
}

// ---------------------------------------------------------------------------
// nexus claude diff
// ---------------------------------------------------------------------------

/// `nexus claude diff`: unified diff (local -> desired) for every non-clean
/// CCX file, a key-level diff for managed settings, and the `CLAUDE.md`
/// block. No writes. Same exit codes as [`status`].
pub async fn diff(api_url: &str, cli_project_id: Option<&str>) -> anyhow::Result<i32> {
    let (_, view) = load_view(api_url, cli_project_id).await?;

    for plan in view.plans.iter().filter(|p| p.state != FileState::Clean) {
        let local = plan
            .local
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let desired = plan.desired.clone().unwrap_or_default();
        print!(
            "{}",
            unified_diff(
                &local,
                &desired,
                &format!(
                    "{} (local, {})",
                    plan.target_path,
                    ccx::state_label(plan.state)
                ),
                &format!("{} (nexus)", plan.target_path),
            )
        );
    }

    if let Some(ref block) = view.block {
        if block.state != FileState::Clean {
            print!(
                "{}",
                unified_diff(
                    block.local.as_deref().unwrap_or(""),
                    &block.desired,
                    &format!(
                        "CLAUDE.md nexus-managed block (local, {})",
                        ccx::state_label(block.state)
                    ),
                    "CLAUDE.md nexus-managed block (nexus)",
                )
            );
        }
    }

    for key in view.settings_pending() {
        let render = |v: Option<&serde_json::Value>| {
            v.map(|v| serde_json::to_string_pretty(v).unwrap_or_default() + "\n")
                .unwrap_or_default()
        };
        print!(
            "{}",
            unified_diff(
                &render(ccx::json_get_path(&view.settings_before, &key)),
                &render(ccx::json_get_path(&view.settings_after, &key)),
                &format!(".claude/settings.json {key} (local)"),
                &format!(".claude/settings.json {key} (nexus)"),
            )
        );
    }

    let pending = view.is_pending();
    if !pending {
        println!("{} No differences.", style("OK").bold().green());
    }
    Ok(i32::from(pending))
}

fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = diff
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string();
    if out.is_empty() {
        // Identical content (e.g. ADOPT, or a pending lock-only change):
        // still name the file so the exit code is explained.
        out = format!("--- {old_label}\n+++ {new_label}\n(content identical)\n");
    }
    out
}

// ---------------------------------------------------------------------------
// nexus claude launch
// ---------------------------------------------------------------------------

/// `nexus claude launch`: deprecated alias (NEXUS-APP dispatch 442f0e97).
/// `nexus run` is the only start command; it follows the backend's
/// `run_target` (including the CCX zellij workspace).
pub async fn launch(
    api_url: &str,
    skip_checks: bool,
    force: bool,
    default_tool: Option<&str>,
    countdown_secs: u64,
    account: Option<&str>,
    assume_yes: bool,
) -> anyhow::Result<()> {
    println!(
        "   {} `nexus claude launch` is deprecated and will be removed; use {}.",
        style("!").bold().yellow(),
        style("nexus run").bold()
    );
    super::run::run(
        api_url,
        None,
        false,
        false,
        false,
        false,
        skip_checks,
        force,
        &[],
        default_tool,
        countdown_secs,
        account,
        assume_yes,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_satisfies_range() {
        assert_eq!(version_satisfies("2.1.260", ">=2.1.257 <3.0.0"), Some(true));
        assert_eq!(version_satisfies("2.1.257", ">=2.1.257 <3.0.0"), Some(true));
        assert_eq!(
            version_satisfies("2.1.256", ">=2.1.257 <3.0.0"),
            Some(false)
        );
        assert_eq!(version_satisfies("3.0.0", ">=2.1.257 <3.0.0"), Some(false));
        assert_eq!(version_satisfies("v2.2.0-beta.1", ">2.1"), Some(true));
        assert_eq!(version_satisfies("garbage", ">=1.0.0"), None);
        assert_eq!(version_satisfies("1.0.0", ">=x"), None);
    }

    #[test]
    fn test_parse_plugin_list_shapes() {
        let arr = serde_json::json!([
            {"id": "nexus-core@gatewarden-nexus"},
            {"name": "other", "marketplace": "mkt"},
            {"name": "bare"}
        ]);
        assert_eq!(
            parse_plugin_list(&arr).unwrap(),
            vec!["nexus-core@gatewarden-nexus", "other@mkt", "bare"]
        );
        let wrapped = serde_json::json!({"plugins": [{"id": "a@b"}]});
        assert_eq!(parse_plugin_list(&wrapped).unwrap(), vec!["a@b"]);
        let keyed = serde_json::json!({"a@b": {"version": "1"}});
        assert_eq!(parse_plugin_list(&keyed).unwrap(), vec!["a@b"]);
        assert!(parse_plugin_list(&serde_json::json!(42)).is_none());
    }

    #[test]
    fn test_plugin_installed_matches_id_or_name() {
        let installed = vec!["nexus-core".to_string(), "x@y".to_string()];
        assert!(plugin_installed(&installed, "nexus-core@gatewarden-nexus"));
        assert!(plugin_installed(&installed, "x@y"));
        assert!(!plugin_installed(&installed, "z@y"));
    }

    #[test]
    fn test_enabled_plugins_only_true() {
        let settings = serde_json::json!({"enabledPlugins": {"a@m": true, "b@m": false}});
        assert_eq!(enabled_plugins(&settings), vec!["a@m"]);
    }

    fn block_file(text: &str) -> String {
        format!(
            "<!-- BEGIN:nexus-managed -->{}<!-- END:nexus-managed -->\n\nuser text\n",
            claude_render::claude_md_desired_block_text(text)
        )
    }

    #[test]
    fn test_block_view_states() {
        let written = claude_render::claude_md_desired_block_text("v1");
        let lock = sha256_hex(&written);

        // Clean: local matches desired.
        let v = block_view(Some(&block_file("v1")), "v1", Some(&lock));
        assert_eq!(v.state, FileState::Clean);
        // New revision, no local edit.
        let v = block_view(Some(&block_file("v1")), "v2", Some(&lock));
        assert_eq!(v.state, FileState::Updated);
        // Local edit, no new revision.
        let v = block_view(Some(&block_file("edited")), "v1", Some(&lock));
        assert_eq!(v.state, FileState::Drifted);
        // Both changed.
        let v = block_view(Some(&block_file("edited")), "v2", Some(&lock));
        assert_eq!(v.state, FileState::Conflict);
        // No lock yet: the pull replaces the block, so report UPDATE.
        let v = block_view(Some(&block_file("edited")), "v2", None);
        assert_eq!(v.state, FileState::Updated);
        // No CLAUDE.md or no markers: CREATE.
        let v = block_view(None, "v1", None);
        assert_eq!(v.state, FileState::Create);
    }

    #[test]
    fn test_unified_diff_identical_content_still_names_file() {
        let out = unified_diff("a\n", "a\n", "x (local)", "x (nexus)");
        assert!(out.contains("x (local)"));
        assert!(out.contains("identical"));
        let out = unified_diff("a\n", "b\n", "x (local)", "x (nexus)");
        assert!(out.contains("-a"));
        assert!(out.contains("+b"));
    }
}
