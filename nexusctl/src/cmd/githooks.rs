//! Git hook self-heal, run at the end of every `nexus pull` (v0.28.4).
//!
//! Repos that ship their hooks in `.githooks/` (e.g. a gitleaks pre-commit
//! scan) rely on `git config core.hooksPath .githooks`, which is local
//! state: a fresh clone, a re-created checkout or a tool resetting the
//! config silently disables the hooks. `nexus pull` restores the setting
//! when `core.hooksPath` is unset in every scope, but only after the
//! operator approves it: the hooks are code from the repository, which git
//! never activates for a clone on its own (`--trust` approves; a
//! non-interactive pull leaves them inactive with a hint). A value set on
//! purpose (e.g. company-wide global hooks or husky) is never overridden;
//! that only gets a hint. It also warns when the hook needs a scanner that
//! is not installed. Repos using the pre-commit framework instead only get
//! a hint when its hook is not installed. Never fails the pull (not a git
//! repo, git missing, ...).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use console::style;

/// The hooks directory Nexus-managed repos use.
const HOOKS_DIR: &str = ".githooks";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Not a git work tree, or no hook setup to check.
    Nothing,
    /// `.githooks/` is already active.
    Active,
    /// `core.hooksPath` was unset and is now `.githooks`.
    Set,
    /// `core.hooksPath` is unset and the operator did not approve the
    /// listed hooks: left inactive, hint only.
    NotApproved { hooks: Vec<String> },
    /// `core.hooksPath` points elsewhere (any scope): left alone, hint only.
    Elsewhere { current: String },
    /// `core.hooksPath` could not be set.
    SetFailed,
    /// `.pre-commit-config.yaml` exists but its git hook is not installed.
    PreCommitNotInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub outcome: HookOutcome,
    /// `.githooks/pre-commit` runs gitleaks, which is not on `PATH`.
    pub gitleaks_missing: bool,
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether an executable named `name` is on `PATH`.
pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(name);
            candidate.is_file() || (cfg!(windows) && dir.join(format!("{name}.exe")).is_file())
        })
    })
}

/// Whether a `core.hooksPath` value already points at `<root>/.githooks`.
fn points_at_githooks(value: &str, root: &Path) -> bool {
    let trimmed = value.trim_end_matches('/');
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    trimmed == HOOKS_DIR || Path::new(trimmed) == root.join(HOOKS_DIR)
}

/// The hook files in `dir`, sorted.
fn hook_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Check and repair the hook setup of the repository containing
/// `workspace`. `gitleaks_available` and `approve` (asked with the hook
/// names before `core.hooksPath` is set) are injected for tests.
pub fn self_heal(
    workspace: &Path,
    gitleaks_available: impl Fn() -> bool,
    approve: impl Fn(&[String]) -> bool,
) -> HookReport {
    let nothing = HookReport {
        outcome: HookOutcome::Nothing,
        gitleaks_missing: false,
    };
    let Some(root) = git(workspace, &["rev-parse", "--show-toplevel"]).map(PathBuf::from) else {
        return nothing;
    };

    let pre_commit = root.join(HOOKS_DIR).join("pre-commit");
    if pre_commit.is_file() {
        let current = git(&root, &["config", "--get", "core.hooksPath"]);
        let outcome = match current.as_deref() {
            Some(v) if points_at_githooks(v, &root) => HookOutcome::Active,
            Some(v) => HookOutcome::Elsewhere {
                current: v.to_string(),
            },
            None => {
                let hooks = hook_names(&root.join(HOOKS_DIR));
                if !approve(&hooks) {
                    HookOutcome::NotApproved { hooks }
                } else if git(&root, &["config", "--local", "core.hooksPath", HOOKS_DIR]).is_some()
                {
                    HookOutcome::Set
                } else {
                    HookOutcome::SetFailed
                }
            }
        };
        let gitleaks_missing = std::fs::read_to_string(&pre_commit)
            .is_ok_and(|hook| hook.contains("gitleaks"))
            && !gitleaks_available();
        return HookReport {
            outcome,
            gitleaks_missing,
        };
    }

    if root.join(".pre-commit-config.yaml").is_file() {
        let installed = git(&root, &["rev-parse", "--git-path", "hooks"])
            .map(|hooks| {
                let hooks = PathBuf::from(hooks);
                let hooks = if hooks.is_absolute() {
                    hooks
                } else {
                    root.join(hooks)
                };
                hooks.join("pre-commit").is_file()
            })
            .unwrap_or(true);
        if !installed {
            return HookReport {
                outcome: HookOutcome::PreCommitNotInstalled,
                gitleaks_missing: false,
            };
        }
    }
    nothing
}

/// Print the pull output lines for `report` (nothing when all is well).
pub fn print_report(report: &HookReport) {
    match &report.outcome {
        HookOutcome::Set => println!(
            "   {} git hooks: core.hooksPath set to {}",
            style("+").bold().green(),
            HOOKS_DIR
        ),
        HookOutcome::Elsewhere { current } => println!(
            "   {} git hooks: core.hooksPath is {current}, so {}/pre-commit is not active; to use it: git config core.hooksPath {}",
            style("i").bold().blue(),
            HOOKS_DIR,
            HOOKS_DIR
        ),
        HookOutcome::NotApproved { hooks } => println!(
            "   {} git hooks: {}/ ({}) not activated; review them, then: git config core.hooksPath {} (or nexus pull --trust)",
            style("i").bold().blue(),
            HOOKS_DIR,
            hooks.join(", "),
            HOOKS_DIR
        ),
        HookOutcome::SetFailed => println!(
            "   {} git hooks: could not set core.hooksPath; run: git config core.hooksPath {}",
            style("!").bold().yellow(),
            HOOKS_DIR
        ),
        HookOutcome::PreCommitNotInstalled => println!(
            "   {} git hooks: .pre-commit-config.yaml found but the hook is not installed; run: pre-commit install",
            style("i").bold().blue()
        ),
        HookOutcome::Nothing | HookOutcome::Active => {}
    }
    if report.gitleaks_missing {
        println!(
            "   {} git hooks: {}/pre-commit runs gitleaks, which is not installed; commits will fail until it is (devbox shell, or brew install gitleaks)",
            style("!").bold().yellow(),
            HOOKS_DIR
        );
    }
}

/// Self-heal and print, for `nexus pull`. `trust` approves activating the
/// repository's hooks; otherwise an interactive terminal is asked (default
/// no) and a non-interactive pull leaves them inactive.
pub fn run(workspace: &Path, trust: bool) {
    let approve = |hooks: &[String]| {
        if trust {
            return true;
        }
        if !std::io::stdin().is_terminal() {
            return false;
        }
        dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(format!(
                "Activate the repository's git hooks in {HOOKS_DIR}/ ({})? They run on git commands",
                hooks.join(", ")
            ))
            .default(false)
            .interact()
            .unwrap_or(false)
    };
    print_report(&self_heal(
        workspace,
        || binary_on_path("gitleaks"),
        approve,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-githooks-test-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(git(&dir, &["init", "-q"]).is_some());
        dir
    }

    fn local_hooks_path(dir: &Path) -> Option<String> {
        git(dir, &["config", "--local", "--get", "core.hooksPath"])
    }

    const HOOK: &str = "#!/bin/sh\ngitleaks protect --staged\n";

    #[test]
    fn test_sets_hooks_path_and_is_idempotent() {
        let dir = repo("set");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/pre-commit"), HOOK).unwrap();
        // A value set on purpose is never overridden.
        git(&dir, &["config", "--local", "core.hooksPath", ".husky"]).unwrap();
        assert_eq!(
            self_heal(&dir, || true, |_| true).outcome,
            HookOutcome::Elsewhere {
                current: ".husky".into()
            }
        );
        assert_eq!(local_hooks_path(&dir).as_deref(), Some(".husky"));
        git(&dir, &["config", "--local", "--unset", "core.hooksPath"]).unwrap();

        let report = self_heal(&dir, || true, |_| true);
        if git(&dir, &["config", "--get", "core.hooksPath"]).is_some() {
            // A global hooksPath on this machine: left alone as well.
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(report.outcome, HookOutcome::Set);
        assert!(!report.gitleaks_missing);
        assert_eq!(local_hooks_path(&dir).as_deref(), Some(".githooks"));

        // Second pull: nothing to do. Also from a subdirectory.
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(
            self_heal(&dir.join("sub"), || true, |_| true).outcome,
            HookOutcome::Active
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hooks_stay_inactive_without_approval() {
        let dir = repo("approve");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/pre-commit"), HOOK).unwrap();
        fs::write(dir.join(".githooks/post-merge"), "#!/bin/sh\n").unwrap();
        if git(&dir, &["config", "--get", "core.hooksPath"]).is_some() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let asked = std::cell::RefCell::new(Vec::new());
        let report = self_heal(
            &dir,
            || true,
            |hooks| {
                asked.borrow_mut().extend(hooks.iter().cloned());
                false
            },
        );
        assert_eq!(
            report.outcome,
            HookOutcome::NotApproved {
                hooks: vec!["post-merge".into(), "pre-commit".into()]
            }
        );
        assert_eq!(*asked.borrow(), vec!["post-merge", "pre-commit"]);
        assert_eq!(local_hooks_path(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_warns_when_gitleaks_missing() {
        let dir = repo("gitleaks");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/pre-commit"), HOOK).unwrap();
        git(&dir, &["config", "--local", "core.hooksPath", ".githooks"]).unwrap();
        let report = self_heal(&dir, || false, |_| true);
        assert_eq!(report.outcome, HookOutcome::Active);
        assert!(report.gitleaks_missing);
        print_report(&report);
        // A hook without gitleaks does not need it.
        fs::write(dir.join(".githooks/pre-commit"), "#!/bin/sh\n").unwrap();
        assert!(!self_heal(&dir, || false, |_| true).gitleaks_missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pre_commit_framework_hint_only() {
        let dir = repo("pre-commit");
        fs::write(dir.join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        git(&dir, &["config", "--local", "core.hooksPath", ".git/hooks"]).unwrap();
        assert_eq!(
            self_heal(&dir, || true, |_| true).outcome,
            HookOutcome::PreCommitNotInstalled
        );
        // Never changes the config in that case.
        assert_eq!(local_hooks_path(&dir).as_deref(), Some(".git/hooks"));
        fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        fs::write(dir.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        assert_eq!(
            self_heal(&dir, || true, |_| true).outcome,
            HookOutcome::Nothing
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_not_a_repo_is_a_no_op() {
        let dir =
            std::env::temp_dir().join(format!("nexus-githooks-test-{}-norepo", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/pre-commit"), HOOK).unwrap();
        if git(&dir, &["rev-parse", "--is-inside-work-tree"]).is_none() {
            assert_eq!(
                self_heal(&dir, || false, |_| true).outcome,
                HookOutcome::Nothing
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_points_at_githooks() {
        let root = Path::new("/r");
        assert!(points_at_githooks(".githooks", root));
        assert!(points_at_githooks("./.githooks/", root));
        assert!(points_at_githooks("/r/.githooks", root));
        assert!(!points_at_githooks(".git/hooks", root));
        assert!(binary_on_path("git"));
        assert!(!binary_on_path("definitely-not-a-binary-xyz"));
    }
}
