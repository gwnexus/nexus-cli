//! `nexus doctor claude [--fix]`: host tools the Claude Code workspace needs
//! (NEXUS-APP ADR-0117 §7/§12, ADR-0120 §6, dispatch c4f507b5).
//!
//! Detects `claude`, `zellij`, `lazygit`, `ccusage` and `delta`. Which ones
//! are required comes from `af_export.claude_workspace.requires` (best
//! effort; offline it falls back to the cached run target). `--fix` prints
//! devbox / brew / npm install commands and runs them only after an explicit
//! confirmation; `nexus pull` never installs anything.

use std::path::Path;

use console::style;
use nexus_core::api::NexusClient;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use serde::Serialize;

use super::preflight::{cmd_version, print_check, CheckResult};

/// Tools `nexus doctor claude` knows, in display order.
const KNOWN_TOOLS: &[&str] = &["claude", "zellij", "lazygit", "ccusage", "delta"];

/// What each tool is for (shown next to a missing tool).
fn purpose(tool: &str) -> &'static str {
    match tool {
        "claude" => "Claude Code",
        "zellij" => "terminal workspace (nexus run)",
        "lazygit" => "git pane",
        "ccusage" => "usage pane",
        "delta" => "diff pager",
        _ => "required by the workspace",
    }
}

/// How a missing tool can be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Installer {
    Devbox,
    Brew,
    Npm,
}

/// The install command for `tool` with `installer`, if there is one.
fn install_command(tool: &str, installer: Installer) -> Option<Vec<String>> {
    let args: &[&str] = match (tool, installer) {
        ("claude", Installer::Devbox) => &["devbox", "add", "claude-code"],
        ("claude", Installer::Brew) => &["brew", "install", "--cask", "claude-code"],
        ("claude", Installer::Npm) => &["npm", "install", "-g", "@anthropic-ai/claude-code"],
        ("zellij", Installer::Devbox) => &["devbox", "add", "zellij"],
        ("zellij", Installer::Brew) => &["brew", "install", "zellij"],
        ("lazygit", Installer::Devbox) => &["devbox", "add", "lazygit"],
        ("lazygit", Installer::Brew) => &["brew", "install", "lazygit"],
        ("delta", Installer::Devbox) => &["devbox", "add", "delta"],
        ("delta", Installer::Brew) => &["brew", "install", "git-delta"],
        ("ccusage", Installer::Npm) => &["npm", "install", "-g", "ccusage"],
        _ => return None,
    };
    Some(args.iter().map(|s| s.to_string()).collect())
}

/// The first installer (in `preference` order) that can install `tool`.
fn pick_install(tool: &str, preference: &[Installer]) -> Option<Vec<String>> {
    preference.iter().find_map(|i| install_command(tool, *i))
}

/// Installer preference for this workspace: devbox when the project uses
/// it, then brew, then npm (as far as the binaries exist).
fn installer_preference(workspace: &Path, on_path: impl Fn(&str) -> bool) -> Vec<Installer> {
    let mut out = Vec::new();
    if workspace.join("devbox.json").is_file() && on_path("devbox") {
        out.push(Installer::Devbox);
    }
    if on_path("brew") {
        out.push(Installer::Brew);
    }
    if on_path("npm") {
        out.push(Installer::Npm);
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub tool: String,
    pub required: bool,
    pub found: bool,
    pub version: Option<String>,
    /// Suggested install command for a missing tool.
    pub install: Option<String>,
}

/// Which tools are required: `requires` from the workspace layout when the
/// backend sent it, else `claude` plus `zellij` when the run target uses it.
fn required_tools(requires: Option<&[String]>, zellij_workspace: bool) -> Vec<String> {
    let mut out: Vec<String> = vec!["claude".into()];
    match requires {
        Some(list) => out.extend(list.iter().cloned()),
        None if zellij_workspace => out.push("zellij".into()),
        None => {}
    }
    let mut seen = Vec::new();
    out.retain(|t| {
        let new = !seen.contains(t);
        seen.push(t.clone());
        new
    });
    out
}

/// Status of every known tool plus any extra required one.
fn collect(
    required: &[String],
    preference: &[Installer],
    probe: impl Fn(&str) -> Option<Option<String>>,
) -> Vec<ToolStatus> {
    let mut tools: Vec<String> = KNOWN_TOOLS.iter().map(|t| t.to_string()).collect();
    tools.extend(
        required
            .iter()
            .filter(|t| !tools.contains(t))
            .cloned()
            .collect::<Vec<_>>(),
    );
    tools
        .into_iter()
        .map(|tool| {
            let probed = probe(&tool);
            let install = if probed.is_none() {
                pick_install(&tool, preference).map(|c| c.join(" "))
            } else {
                None
            };
            ToolStatus {
                required: required.contains(&tool),
                found: probed.is_some(),
                version: probed.flatten(),
                install,
                tool,
            }
        })
        .collect()
}

/// `Some(version)` when `tool` is on PATH (`None` version if `--version`
/// failed), `None` when it is missing.
fn probe_tool(tool: &str) -> Option<Option<String>> {
    if !super::run::on_path(tool) {
        return None;
    }
    Some(
        cmd_version(tool, &["--version"])
            .filter(|v| !v.is_empty())
            .map(|v| tidy_version(tool, &v)),
    )
}

/// `lazygit --version` prints `commit=..., version=0.43.1, ...`: keep the
/// version only.
fn tidy_version(tool: &str, raw: &str) -> String {
    match raw
        .split(", ")
        .find_map(|p| p.trim().strip_prefix("version="))
    {
        Some(v) => format!("{tool} {v}"),
        None => raw.to_string(),
    }
}

/// `nexus doctor claude [--fix]`. Returns the exit code: 1 when a required
/// tool is missing.
pub async fn claude(api_url: &str, fix: bool, assume_yes: bool, json: bool) -> anyhow::Result<i32> {
    let workspace = std::env::current_dir()?;

    // Best effort: the layout's `requires` for this caller.
    let mut requires: Option<Vec<String>> = None;
    let mut preset: Option<String> = None;
    let mut compat_range: Option<String> = None;
    let mut run_target = config::load_run_target(Some(&workspace));
    if let (Some(token), Ok(project_id)) = (
        resolve_token(),
        config::resolve_project_id(None, Some(&workspace)),
    ) {
        if let Ok(client) = NexusClient::new(api_url, Some(token)) {
            if let Ok(export) = client.export_agent_files(&project_id).await {
                if let Some(layout) = export.claude_workspace {
                    requires = Some(layout.requires);
                    preset = layout.preset;
                }
                compat_range = export.ccx.and_then(|c| c.compatibility.claude_code);
                if export.run_target.is_some() {
                    run_target = export.run_target;
                }
            }
        }
    }
    let zellij_workspace =
        run_target.as_ref().and_then(|t| t.workspace.as_deref()) == Some("zellij");
    let required = required_tools(requires.as_deref(), zellij_workspace);
    let preference = installer_preference(&workspace, super::run::on_path);
    let tools = collect(&required, &preference, probe_tool);
    let missing_required: Vec<&ToolStatus> =
        tools.iter().filter(|t| t.required && !t.found).collect();
    let code = i32::from(!missing_required.is_empty());
    let claude_compatible = match (
        tools
            .iter()
            .find(|t| t.tool == "claude")
            .and_then(|t| t.version.as_deref()),
        compat_range.as_deref(),
    ) {
        (Some(v), Some(range)) => v
            .split_whitespace()
            .next()
            .and_then(|v| super::claude_cmd::version_satisfies(v, range)),
        _ => None,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": if requires.is_some() { "claude_workspace.requires" } else { "run_target" },
                "preset": preset,
                "installers": preference,
                "tools": tools,
                "claude_code_compatibility": compat_range,
                "claude_code_compatible": claude_compatible,
                "ok": code == 0,
            }))?
        );
        return Ok(code);
    }

    println!("{}", style("Claude Code workspace tools").bold());
    let source = match (&requires, &preset) {
        (Some(_), Some(p)) => format!("required by your workspace layout (preset {p})"),
        (Some(_), None) => "required by your workspace layout".to_string(),
        (None, _) if zellij_workspace => "required by the project run target (zellij)".into(),
        (None, _) => "required by the project run target".into(),
    };
    println!("  {}", style(source).dim());
    println!();
    for t in &tools {
        let result = match (t.found, t.required) {
            (true, _) => CheckResult::Pass(t.version.clone().unwrap_or_else(|| "found".into())),
            (false, true) => CheckResult::Fail(format!("missing ({})", purpose(&t.tool))),
            (false, false) => {
                CheckResult::Info(format!("not installed, optional ({})", purpose(&t.tool)))
            }
        };
        print_check(&t.tool, &result);
        if t.tool == "claude" && claude_compatible == Some(false) {
            print_check(
                "",
                &CheckResult::Warn(format!(
                    "outside the supported range {} of this project's Claude Code bundle",
                    compat_range.as_deref().unwrap_or_default()
                )),
            );
        }
    }
    let optional: Vec<String> = tools
        .iter()
        .filter(|t| !t.found && !t.required)
        .filter_map(|t| t.install.clone())
        .collect();

    let fixable: Vec<(&ToolStatus, String)> = tools
        .iter()
        .filter(|t| !t.found && t.required)
        .filter_map(|t| t.install.clone().map(|c| (t, c)))
        .collect();
    let unfixable: Vec<&&ToolStatus> = missing_required
        .iter()
        .filter(|t| t.install.is_none())
        .collect();

    if missing_required.is_empty() {
        println!();
        println!("  {} All required tools are installed.", style("✓").green());
        if fix && !optional.is_empty() {
            println!("  Optional (not run): {}", optional.join("; "));
        }
        return Ok(0);
    }
    println!();
    if !fix {
        println!(
            "  {} {} required tool(s) missing; run {} for install commands.",
            style("!").bold().yellow(),
            missing_required.len(),
            style("nexus doctor claude --fix").bold()
        );
        return Ok(code);
    }

    if !fixable.is_empty() {
        println!("  Install commands:");
        for (_, cmd) in &fixable {
            println!("    {}", style(cmd).bold());
        }
    }
    if !optional.is_empty() {
        println!("  Optional (not run): {}", optional.join("; "));
    }
    for t in &unfixable {
        println!(
            "  {} no installer found for {} (devbox, brew or npm); install it manually.",
            style("!").bold().yellow(),
            t.tool
        );
    }
    if fixable.is_empty() {
        return Ok(code);
    }
    let confirmed = if assume_yes {
        true
    } else if console::Term::stdout().is_term() {
        confirm("Run these commands now?")?
    } else {
        println!("  (not a terminal: nothing run; pass -y to run them)");
        false
    };
    if !confirmed {
        return Ok(code);
    }
    let mut failed = false;
    for (t, cmd) in &fixable {
        println!("  {} {}", style("$").dim(), cmd);
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let ok = std::process::Command::new(parts[0])
            .args(&parts[1..])
            .current_dir(&workspace)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            failed = true;
            println!("  {} installing {} failed", style("✗").red(), t.tool);
        }
    }
    if fixable.iter().any(|(_, c)| c.starts_with("devbox ")) {
        println!(
            "  {} devbox packages are available in the devbox shell (devbox shell, or re-enter the directory with direnv).",
            style("i").bold().blue()
        );
    }
    Ok(i32::from(failed || !unfixable.is_empty()))
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("  {} {} [y/N] ", style("?").bold().cyan(), question);
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tidy_version() {
        assert_eq!(
            tidy_version("lazygit", "commit=, build date=, version=0.43.1, os=darwin"),
            "lazygit 0.43.1"
        );
        assert_eq!(tidy_version("zellij", "zellij 0.45.0"), "zellij 0.45.0");
    }

    #[test]
    fn test_required_tools_from_requires_or_run_target() {
        let requires = vec!["zellij".to_string(), "ccusage".into(), "claude".into()];
        assert_eq!(
            required_tools(Some(&requires), false),
            vec!["claude", "zellij", "ccusage"]
        );
        assert_eq!(required_tools(None, true), vec!["claude", "zellij"]);
        assert_eq!(required_tools(None, false), vec!["claude"]);
        // An empty list from the backend means nothing beyond claude.
        assert_eq!(required_tools(Some(&[]), true), vec!["claude"]);
    }

    #[test]
    fn test_collect_marks_missing_and_suggests_installer() {
        let required = vec![
            "claude".to_string(),
            "zellij".into(),
            "ccusage".into(),
            "atuin".into(),
        ];
        let probe = |t: &str| match t {
            "claude" => Some(Some("2.1.260 (Claude Code)".to_string())),
            "delta" => Some(None),
            _ => None,
        };
        let tools = collect(&required, &[Installer::Brew, Installer::Npm], probe);
        let get = |n: &str| tools.iter().find(|t| t.tool == n).unwrap();
        assert!(get("claude").found && get("claude").required);
        assert_eq!(get("claude").install, None);
        assert_eq!(
            get("zellij").install.as_deref(),
            Some("brew install zellij")
        );
        assert_eq!(
            get("ccusage").install.as_deref(),
            Some("npm install -g ccusage")
        );
        assert!(get("delta").found && !get("delta").required);
        assert!(!get("lazygit").required);
        // Unknown required tools are listed, without an installer.
        assert!(get("atuin").required && get("atuin").install.is_none());
    }

    #[test]
    fn test_installer_preference_prefers_devbox_for_devbox_projects() {
        let dir = std::env::temp_dir().join(format!("nexus-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let all = |_: &str| true;
        assert_eq!(
            installer_preference(&dir, all),
            vec![Installer::Brew, Installer::Npm]
        );
        std::fs::write(dir.join("devbox.json"), "{}").unwrap();
        assert_eq!(
            installer_preference(&dir, all),
            vec![Installer::Devbox, Installer::Brew, Installer::Npm]
        );
        assert_eq!(
            pick_install("ccusage", &installer_preference(&dir, all))
                .unwrap()
                .join(" "),
            "npm install -g ccusage"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
