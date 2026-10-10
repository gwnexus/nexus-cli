//! Claude Code runtime details for `nexus status` (version vs. the CCX
//! compatibility range, enabled plugins, last headroom summary) and the
//! deprecated `nexus claude` aliases (NEXUS-APP dispatches 99f335e8,
//! 442f0e97, b5f7bfb0).

use std::path::Path;

use console::style;

use super::display::status_line;

/// Print the Claude Code runtime block of `nexus status` and return it as
/// JSON for `--output json`.
pub fn runtime_details(
    workspace: &Path,
    settings: &serde_json::Value,
    compat_range: Option<&str>,
    print: bool,
) -> serde_json::Value {
    let version = claude_code_version();
    let compatible = match (&version, compat_range) {
        (Some(v), Some(range)) => version_satisfies(v, range),
        _ => None,
    };
    let plugins = enabled_plugins(settings);
    let lock = super::ccx::load_lock(workspace, &super::run::resolve_agentic_root(workspace));
    let managed =
        super::claude_plugins::managed_plugins(lock.as_ref().and_then(|l| l.settings.as_ref()));
    let installed_by_nexus = lock.map(|l| l.plugins).unwrap_or_default();
    let installed = super::claude_plugins::list_installed(&super::claude_plugins::SystemClaude {
        workspace: workspace.to_path_buf(),
    });
    let scope_of = |id: &str| {
        installed
            .as_ref()
            .map(|i| super::claude_plugins::present_scope(i, id, workspace))
    };
    let headroom = super::run::read_headroom_stats(workspace, 0);

    if print {
        println!("{}", style("Claude Code runtime").bold());
        println!();
        match (&version, compat_range, compatible) {
            (None, _, _) => status_line(
                "Version:",
                style("not found (claude --version failed)").yellow(),
            ),
            (Some(v), Some(range), Some(false)) => status_line(
                "Version:",
                format_args!(
                    "{} {}",
                    v,
                    style(format!("outside supported range {range}")).yellow()
                ),
            ),
            (Some(v), Some(range), _) => {
                status_line("Version:", format_args!("{v} (supported: {range})"))
            }
            (Some(v), None, _) => status_line("Version:", v),
        }
        for plugin in &plugins {
            let state = match scope_of(plugin) {
                Some(Some(scope)) if installed_by_nexus.contains_key(plugin) => {
                    style(format!("installed ({scope} scope, by nexus)")).green()
                }
                Some(Some(scope)) => style(format!("installed ({scope} scope)")).green(),
                Some(None) if managed.contains(plugin) => {
                    style("NOT INSTALLED (nexus run installs it)".to_string()).yellow()
                }
                Some(None) => style("NOT INSTALLED".to_string()).yellow(),
                None => style("unknown (claude plugin list unavailable)".to_string()).dim(),
            };
            let tag = if managed.contains(plugin) {
                format!(" {}", style("[managed]").cyan())
            } else {
                String::new()
            };
            status_line("Plugin:", format_args!("{plugin}{tag}: {state}"));
        }
        if let Some(ref h) = headroom {
            status_line(
                "Headroom:",
                format_args!(
                    "last session mode {}, {} compression(s), ~{} tokens saved",
                    h.mode.as_deref().unwrap_or("unknown"),
                    h.compressions,
                    h.potential_saved_tokens
                ),
            );
        }
        println!();
    }

    serde_json::json!({
        "version": version,
        "compatibility": compat_range,
        "compatible": compatible,
        "plugins": plugins.iter().map(|p| {
            let scope = scope_of(p);
            serde_json::json!({
                "id": p,
                "managed": managed.contains(p),
                "installed": scope.as_ref().map(Option::is_some),
                "scope": scope.flatten(),
                "installed_by_nexus": installed_by_nexus.contains_key(p),
            })
        }).collect::<Vec<_>>(),
        "headroom": headroom.map(|h| serde_json::json!({
            "mode": h.mode,
            "compressions": h.compressions,
            "potential_saved_tokens": h.potential_saved_tokens,
        })),
    })
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
pub(crate) fn version_satisfies(version: &str, range: &str) -> Option<bool> {
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
        None,
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
    fn test_enabled_plugins_only_true() {
        let settings = serde_json::json!({"enabledPlugins": {"a@m": true, "b@m": false}});
        assert_eq!(enabled_plugins(&settings), vec!["a@m"]);
    }
}
