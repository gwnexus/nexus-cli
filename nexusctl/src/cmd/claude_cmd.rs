//! Claude Code runtime details for `nexus status` (version vs. the CCX
//! compatibility range, enabled plugins, last headroom summary) and the
//! deprecated `nexus claude` aliases (NEXUS-APP dispatches 99f335e8,
//! 442f0e97, b5f7bfb0).

use std::path::Path;

use console::style;

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
    let installed = installed_plugins();
    let headroom = super::run::read_headroom_stats(workspace, 0);

    if print {
        println!("{}", style("Claude Code runtime").bold());
        match (&version, compat_range, compatible) {
            (None, _, _) => println!(
                "  Version:  {}",
                style("not found (claude --version failed)").yellow()
            ),
            (Some(v), Some(range), Some(false)) => println!(
                "  Version:  {} {}",
                v,
                style(format!("outside supported range {range}")).yellow()
            ),
            (Some(v), Some(range), _) => println!("  Version:  {v} (supported: {range})"),
            (Some(v), None, _) => println!("  Version:  {v}"),
        }
        for plugin in &plugins {
            let state = match installed.as_ref().map(|i| plugin_installed(i, plugin)) {
                Some(true) => style("installed".to_string()).green(),
                Some(false) => style("NOT INSTALLED".to_string()).yellow(),
                None => style("unknown (claude plugin list unavailable)".to_string()).dim(),
            };
            println!("  Plugin:   {plugin}: {state}");
        }
        if let Some(ref h) = headroom {
            println!(
                "  Headroom: last session mode {}, {} compression(s), ~{} tokens saved",
                h.mode.as_deref().unwrap_or("unknown"),
                h.compressions,
                h.potential_saved_tokens
            );
        }
        println!();
    }

    serde_json::json!({
        "version": version,
        "compatibility": compat_range,
        "compatible": compatible,
        "plugins": plugins.iter().map(|p| serde_json::json!({
            "id": p,
            "installed": installed.as_ref().map(|i| plugin_installed(i, p)),
        })).collect::<Vec<_>>(),
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
}
