//! The local `.vault/` folder: output for screenshots and other captures
//! that may show customer data. It must stay on the machine and ignored by
//! git. `nexus link` and `nexus init` create it and make sure it is ignored.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use console::style;

/// The vault directory, relative to the workspace.
pub(crate) const VAULT_DIR: &str = ".vault";
/// Subdirectories created inside the vault.
const VAULT_SUBDIRS: &[&str] = &["screenshots"];
/// The ignore entry written to `.gitignore` or `.git/info/exclude`.
const IGNORE_ENTRY: &str = ".vault/";

/// Where the ignore entry for `.vault/` was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreChange {
    /// Git already ignores `.vault/`; nothing written.
    AlreadyIgnored,
    /// Added to the repository's `.gitignore`.
    Gitignore,
    /// Added to `.git/info/exclude` (shadowed or unknown shadow level).
    Exclude,
    /// Written, but git still does not ignore `.vault/` (e.g. a negation rule).
    StillNotIgnored,
    /// Not a git repository: nothing to ignore.
    NoRepository,
    /// Files under `.vault/` are tracked: nothing changed.
    Skipped,
}

/// What [`scaffold`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaultReport {
    /// Directories created (relative paths).
    pub created: Vec<String>,
    pub ignore: IgnoreChange,
    /// Number of files under `.vault/` that git tracks.
    pub tracked: usize,
}

/// Whether a shadow level hides Nexus changes from the repository. `None`
/// (level unknown, e.g. offline) counts as shadowed, so nothing visible is
/// written to a repository that may be shadowed.
pub(crate) fn hides_changes(shadow_level: Option<&str>) -> bool {
    !matches!(shadow_level.map(str::trim), Some("" | "off"))
}

/// Create `.vault/` (with `screenshots/`) in `workspace` and make sure git
/// ignores it: through `.gitignore`, or `.git/info/exclude` when
/// [`hides_changes`] says so. Existing content is never touched. When git
/// already tracks files under `.vault/`, nothing is changed at all.
pub(crate) fn scaffold(workspace: &Path, shadow_level: Option<&str>) -> VaultReport {
    let repo = repo_root(workspace);
    let tracked = repo.as_ref().map_or(0, |_| tracked_count(workspace));
    if tracked > 0 {
        return VaultReport {
            created: Vec::new(),
            ignore: IgnoreChange::Skipped,
            tracked,
        };
    }

    let mut created = Vec::new();
    let vault = workspace.join(VAULT_DIR);
    for dir in std::iter::once(String::new()).chain(VAULT_SUBDIRS.iter().map(|s| s.to_string())) {
        let path = if dir.is_empty() {
            vault.clone()
        } else {
            vault.join(&dir)
        };
        if !path.exists() && fs::create_dir_all(&path).is_ok() {
            created.push(if dir.is_empty() {
                format!("{VAULT_DIR}/")
            } else {
                format!("{VAULT_DIR}/{dir}/")
            });
        }
    }

    let Some(repo) = repo else {
        return VaultReport {
            created,
            ignore: IgnoreChange::NoRepository,
            tracked: 0,
        };
    };
    let ignore = if is_ignored(workspace) {
        IgnoreChange::AlreadyIgnored
    } else {
        let change = if hides_changes(shadow_level) {
            super::shadow::ensure_git_excluded_in(
                workspace,
                &[IGNORE_ENTRY],
                "local vault, screenshots may show customer data",
            );
            IgnoreChange::Exclude
        } else {
            append_gitignore(&repo.join(".gitignore"));
            IgnoreChange::Gitignore
        };
        if is_ignored(workspace) {
            change
        } else {
            IgnoreChange::StillNotIgnored
        }
    };
    VaultReport {
        created,
        ignore,
        tracked: 0,
    }
}

/// Print the report as link/init summary lines.
pub(crate) fn print_report(report: &VaultReport) {
    for path in &report.created {
        println!("   {} {path}", style("+").bold().green());
    }
    match report.ignore {
        IgnoreChange::Gitignore => println!(
            "   {} .gitignore: added {IGNORE_ENTRY} (local vault, never committed)",
            style("+").bold().green()
        ),
        IgnoreChange::Exclude => println!(
            "   {} .git/info/exclude: added {IGNORE_ENTRY} (local vault, never committed)",
            style("+").bold().green()
        ),
        IgnoreChange::StillNotIgnored => println!(
            "   {} {IGNORE_ENTRY} is still not ignored by git (check the ignore rules); \
             files there could be committed",
            style("!").bold().yellow()
        ),
        IgnoreChange::Skipped => {
            println!(
                "   {} {} file(s) under {IGNORE_ENTRY} are tracked by git; nothing changed. \
                 Screenshots may show customer data.",
                style("!").bold().yellow(),
                report.tracked
            );
            println!(
                "     untrack them (kept on disk): git rm -r --cached {VAULT_DIR}, then ignore {IGNORE_ENTRY}"
            );
        }
        IgnoreChange::AlreadyIgnored | IgnoreChange::NoRepository => {}
    }
}

/// The working tree root of the repository containing `workspace`.
fn repo_root(workspace: &Path) -> Option<std::path::PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().into())
        .filter(|p: &std::path::PathBuf| !p.as_os_str().is_empty())
}

/// Whether git ignores the `.vault` directory itself. A probe file inside
/// is not enough: `.vault/*` with `!.vault/screenshots/` ignores the probe
/// but reopens a subdirectory. Without the directory on disk git cannot
/// apply a directory-only rule, so a file inside is probed instead.
fn is_ignored(workspace: &Path) -> bool {
    let probe = if workspace.join(VAULT_DIR).is_dir() {
        VAULT_DIR
    } else {
        ".vault/x"
    };
    Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["check-ignore", "-q", "--", probe])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Number of files under `.vault/` that git tracks.
fn tracked_count(workspace: &Path) -> usize {
    Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "-z", "--", VAULT_DIR])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or(0, |o| {
            o.stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .count()
        })
}

/// Append the ignore entry to `.gitignore` (created when missing). The
/// existing bytes are kept as they are (any encoding): the entry is only
/// appended. A file that cannot be read is left alone; the caller then
/// reports the vault as still not ignored.
fn append_gitignore(path: &Path) {
    use std::io::Write;
    let existing = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return,
    };
    if existing
        .split(|b| *b == b'\n')
        .any(|l| l.trim_ascii() == IGNORE_ENTRY.as_bytes())
    {
        return;
    }
    let mut add = String::new();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        add.push('\n');
    }
    add.push_str("\n# Local vault (screenshots may show customer data)\n");
    add.push_str(IGNORE_ENTRY);
    add.push('\n');
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(add.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-vault-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        dir
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn test_append_gitignore_keeps_non_utf8_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".gitignore");
        let original = b"# caf\xe9 rules\n.env\n".to_vec();
        fs::write(&path, &original).unwrap();
        append_gitignore(&path);
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&original), "existing bytes changed");
        assert!(String::from_utf8_lossy(&after).contains(IGNORE_ENTRY));
        append_gitignore(&path);
        assert_eq!(fs::read(&path).unwrap(), after, "entry added twice");
    }

    #[test]
    fn test_hides_changes() {
        assert!(!hides_changes(Some("off")));
        assert!(!hides_changes(Some("")));
        assert!(hides_changes(Some("ai")));
        assert!(hides_changes(Some("nexus")));
        assert!(hides_changes(None));
    }

    #[test]
    fn test_scaffold_creates_and_ignores_via_gitignore() {
        let dir = repo("gitignore");
        fs::write(dir.join(".gitignore"), "target/").unwrap();
        let r = scaffold(&dir, Some("off"));
        assert_eq!(r.created, vec![".vault/", ".vault/screenshots/"]);
        assert_eq!(r.ignore, IgnoreChange::Gitignore);
        assert!(dir.join(".vault/screenshots").is_dir());
        let gi = read(&dir.join(".gitignore"));
        assert!(gi.starts_with("target/\n"), "{gi}");
        assert!(gi.lines().any(|l| l == ".vault/"), "{gi}");
        assert!(!read(&dir.join(".git/info/exclude")).contains(".vault/"));
        // Second run: nothing to do.
        let r = scaffold(&dir, Some("off"));
        assert!(r.created.is_empty());
        assert_eq!(r.ignore, IgnoreChange::AlreadyIgnored);
        assert_eq!(read(&dir.join(".gitignore")).matches(".vault/").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scaffold_adds_the_directory_when_a_subdirectory_is_reopened() {
        let dir = repo("reopened");
        fs::write(dir.join(".gitignore"), ".vault/*\n!.vault/screenshots/\n").unwrap();
        let r = scaffold(&dir, Some("off"));
        assert_eq!(r.ignore, IgnoreChange::Gitignore);
        assert!(read(&dir.join(".gitignore"))
            .lines()
            .any(|l| l == ".vault/"));
        assert!(is_ignored(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scaffold_shadowed_uses_exclude() {
        for level in [Some("ai"), None] {
            let dir = repo(&format!("shadow-{}", level.unwrap_or("unknown")));
            let r = scaffold(&dir, level);
            assert_eq!(r.ignore, IgnoreChange::Exclude);
            assert!(!dir.join(".gitignore").exists());
            assert!(read(&dir.join(".git/info/exclude"))
                .lines()
                .any(|l| l == ".vault/"));
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_scaffold_keeps_existing_content() {
        let dir = repo("existing");
        fs::create_dir_all(dir.join(".vault/screenshots")).unwrap();
        fs::write(dir.join(".vault/screenshots/a.png"), "x").unwrap();
        fs::write(dir.join(".gitignore"), ".vault/\n").unwrap();
        let r = scaffold(&dir, Some("off"));
        assert!(r.created.is_empty());
        assert_eq!(r.ignore, IgnoreChange::AlreadyIgnored);
        assert_eq!(read(&dir.join(".vault/screenshots/a.png")), "x");
        assert_eq!(read(&dir.join(".gitignore")), ".vault/\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scaffold_with_tracked_files_changes_nothing() {
        let dir = repo("tracked");
        fs::create_dir_all(dir.join(".vault")).unwrap();
        fs::write(dir.join(".vault/a.png"), "x").unwrap();
        fs::write(dir.join(".vault/b.png"), "y").unwrap();
        git(&dir, &["add", ".vault"]);
        let r = scaffold(&dir, Some("off"));
        assert_eq!(r.ignore, IgnoreChange::Skipped);
        assert_eq!(r.tracked, 2);
        assert!(r.created.is_empty());
        assert!(!dir.join(".vault/screenshots").exists());
        assert!(!dir.join(".gitignore").exists());
        assert!(!read(&dir.join(".git/info/exclude")).contains(".vault/"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scaffold_outside_git_only_creates_dirs() {
        let dir = std::env::temp_dir().join(format!("nexus-vault-{}-nogit", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let r = scaffold(&dir, Some("off"));
        // A temp dir may sit inside a repository on some machines; only
        // assert the outside case.
        if r.ignore == IgnoreChange::NoRepository {
            assert_eq!(r.created, vec![".vault/", ".vault/screenshots/"]);
            assert!(!dir.join(".gitignore").exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
