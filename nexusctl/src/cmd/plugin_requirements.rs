//! Programs that enabled agent plugins need (`af_export.claude_plugin_requirements`),
//! e.g. the language server of an LSP plugin.
//!
//! `nexus pull`, the `nexus run` preflight and `nexus doctor claude` warn
//! about each missing program with a fix. The check never blocks a start.

use std::path::Path;

use nexus_core::api::PluginRequirement;
use serde::Serialize;

use super::preflight::CheckResult;

/// Where devbox installs a project's packages (`devbox add` fills it).
const DEVBOX_PROFILE_BIN: &str = ".devbox/nix/profile/default/bin";

/// The check result of one requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Status {
    pub plugin: String,
    pub binary: String,
    pub found: bool,
    /// The warning with its fix; `None` when the program is found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Check every requirement against `PATH` and, for a devbox project, the
/// devbox profile (where the agent finds devbox packages in the devbox
/// shell). Entries with a malformed plugin or program name are skipped.
pub(crate) fn check(reqs: &[PluginRequirement], workspace: &Path) -> Vec<Status> {
    check_with(reqs, workspace, &super::run::on_path)
}

/// Warning rows for the missing programs; empty when all are found.
pub(crate) fn missing_rows(reqs: &[PluginRequirement], workspace: &Path) -> Vec<CheckResult> {
    check(reqs, workspace)
        .into_iter()
        .filter_map(|s| s.message.map(CheckResult::Warn))
        .collect()
}

fn check_with(
    reqs: &[PluginRequirement],
    workspace: &Path,
    on_path: &dyn Fn(&str) -> bool,
) -> Vec<Status> {
    let devbox = workspace.join("devbox.json").is_file();
    reqs.iter()
        .filter(|r| {
            super::pull::valid_path_segment(&r.plugin) && super::pull::valid_path_segment(&r.binary)
        })
        .map(|r| {
            let found = on_path(&r.binary)
                || (devbox && workspace.join(DEVBOX_PROFILE_BIN).join(&r.binary).is_file());
            Status {
                plugin: r.plugin.clone(),
                binary: r.binary.clone(),
                found,
                message: (!found).then(|| message(r, devbox)),
            }
        })
        .collect()
}

/// A nixpkgs package name as `devbox add` takes it (`name` or `name@version`).
fn valid_package(pkg: &str) -> bool {
    !pkg.is_empty()
        && pkg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
}

fn message(r: &PluginRequirement, devbox: bool) -> String {
    match r.devbox_package.as_deref().filter(|p| valid_package(p)) {
        Some(pkg) if devbox => format!("{} needs {}: devbox add {pkg}", r.plugin, r.binary),
        Some(pkg) => format!(
            "{} needs {} on PATH (install it, nixpkgs package {pkg}, or disable the plugin in Nexus)",
            r.plugin, r.binary
        ),
        None => format!(
            "{} needs {} on PATH (install it, or disable the plugin in Nexus)",
            r.plugin, r.binary
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn req(plugin: &str, binary: &str, pkg: Option<&str>) -> PluginRequirement {
        PluginRequirement {
            plugin: plugin.into(),
            binary: binary.into(),
            devbox_package: pkg.map(str::to_string),
        }
    }

    fn workspace(suffix: &str, devbox: bool) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-plugin-req-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        if devbox {
            fs::write(dir.join("devbox.json"), "{}").unwrap();
        }
        dir
    }

    fn nothing_on_path(_: &str) -> bool {
        false
    }

    #[test]
    fn test_missing_with_devbox_package_in_a_devbox_project() {
        let dir = workspace("devbox", true);
        let reqs = [req("pyright-lsp", "pyright-langserver", Some("pyright"))];
        let s = check_with(&reqs, &dir, &nothing_on_path);
        assert_eq!(s.len(), 1);
        assert!(!s[0].found);
        assert_eq!(
            s[0].message.as_deref(),
            Some("pyright-lsp needs pyright-langserver: devbox add pyright")
        );
        // After `devbox add pyright` the program is in the devbox profile.
        let bin = dir.join(DEVBOX_PROFILE_BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("pyright-langserver"), "").unwrap();
        let s = check_with(&reqs, &dir, &nothing_on_path);
        assert!(s[0].found);
        assert_eq!(s[0].message, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_missing_without_devbox_package() {
        let dir = workspace("no-pkg", true);
        let s = check_with(
            &[req("kotlin-lsp", "kotlin-lsp", None)],
            &dir,
            &nothing_on_path,
        );
        assert_eq!(
            s[0].message.as_deref(),
            Some(
                "kotlin-lsp needs kotlin-lsp on PATH (install it, or disable the plugin in Nexus)"
            )
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_project_without_devbox_names_the_package_only() {
        let dir = workspace("no-devbox", false);
        // A devbox profile without devbox.json does not count.
        let bin = dir.join(DEVBOX_PROFILE_BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("pyright-langserver"), "").unwrap();
        let s = check_with(
            &[req("pyright-lsp", "pyright-langserver", Some("pyright"))],
            &dir,
            &nothing_on_path,
        );
        assert!(!s[0].found);
        assert!(s[0]
            .message
            .as_deref()
            .unwrap()
            .contains("nixpkgs package pyright"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_found_on_path_gives_no_warning() {
        let dir = workspace("on-path", true);
        let reqs = [req("pyright-lsp", "pyright-langserver", Some("pyright"))];
        let s = check_with(&reqs, &dir, &|b| b == "pyright-langserver");
        assert!(s[0].found);
        assert!(s.iter().all(|s| s.message.is_none()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_malformed_entries_are_skipped() {
        let dir = workspace("malformed", true);
        let reqs = [
            req("pyright-lsp", "../bin/sh", Some("pyright")),
            req("bad plugin", "x", None),
            req("ok-lsp", "ok-ls", Some("pkg; rm -rf /")),
        ];
        let s = check_with(&reqs, &dir, &nothing_on_path);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].plugin, "ok-lsp");
        // A malformed package name is not offered as a command.
        assert_eq!(
            s[0].message.as_deref(),
            Some("ok-lsp needs ok-ls on PATH (install it, or disable the plugin in Nexus)")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_no_requirements_no_rows() {
        let dir = workspace("empty", true);
        assert!(missing_rows(&[], &dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
