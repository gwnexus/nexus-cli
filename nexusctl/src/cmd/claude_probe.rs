//! Claude Code preflight: is `claude` installed and runnable?
//!
//! Looks on `PATH` first, then in the native installer locations
//! (`~/.local/bin/claude`, `~/.claude/local/claude`), the same order as the
//! workspace init scripts. A binary found counts only when
//! `claude --version` succeeds within [`VERSION_TIMEOUT`].

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::preflight::CheckResult;

/// How long `claude --version` may take.
pub(crate) const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Printed when Claude Code is missing or broken.
pub(crate) const INSTALL_HINT: &str = "curl -fsSL https://claude.ai/install.sh | bash";

/// Install locations checked when `claude` is not on `PATH`, relative to
/// the home directory.
const FALLBACK_DIRS: &[&str] = &[".local/bin", ".claude/local"];

/// Result of the Claude Code probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeProbe {
    /// On `PATH` and runnable.
    OnPath { path: PathBuf, version: String },
    /// Runnable, but only found in a fallback location; `dir` must be put
    /// on `PATH` for the workspace panes.
    OffPath {
        path: PathBuf,
        dir: PathBuf,
        version: String,
    },
    /// Found, but `claude --version` failed or timed out.
    Broken { path: PathBuf, reason: String },
    /// Not found anywhere.
    Missing,
}

impl ClaudeProbe {
    /// Whether the launch must stop.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self, ClaudeProbe::Broken { .. } | ClaudeProbe::Missing)
    }

    /// The check row for `nexus run` and `nexus doctor`.
    pub(crate) fn check_result(&self) -> CheckResult {
        match self {
            ClaudeProbe::OnPath { version, .. } => CheckResult::Pass(version.clone()),
            ClaudeProbe::OffPath { path, dir, version } => CheckResult::Warn(format!(
                "{version} at {} (not on PATH; add {} to PATH)",
                path.display(),
                dir.display()
            )),
            ClaudeProbe::Broken { path, reason } => CheckResult::Fail(format!(
                "{} is not runnable ({reason}); reinstall: {INSTALL_HINT}",
                path.display()
            )),
            ClaudeProbe::Missing => CheckResult::Fail(format!(
                "Claude Code (claude) not found on PATH, in ~/.local/bin or ~/.claude/local; \
                 install: {INSTALL_HINT}"
            )),
        }
    }
}

/// Whether `path` is an executable regular file.
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Where `claude` is: `(path, on_path)`.
pub(crate) fn locate(path_var: Option<&OsStr>, home: Option<&Path>) -> Option<(PathBuf, bool)> {
    if let Some(paths) = path_var {
        if let Some(hit) = std::env::split_paths(paths)
            .map(|d| d.join("claude"))
            .find(|p| is_executable(p))
        {
            return Some((hit, true));
        }
    }
    let home = home?;
    FALLBACK_DIRS
        .iter()
        .map(|d| home.join(d).join("claude"))
        .find(|p| is_executable(p))
        .map(|p| (p, false))
}

/// Run `<bin> --version` with a timeout: the first output line, or why it
/// failed.
pub(crate) fn version_of(bin: &Path, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "--version did not answer within {}s",
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if !status.success() {
        return Err(format!("--version exited with {status}"));
    }
    let line = out.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        return Err("--version printed nothing".into());
    }
    Ok(line)
}

/// Probe with an explicit `PATH`, home and timeout (testable).
pub(crate) fn probe_with(
    path_var: Option<&OsStr>,
    home: Option<&Path>,
    timeout: Duration,
) -> ClaudeProbe {
    let Some((path, on_path)) = locate(path_var, home) else {
        return ClaudeProbe::Missing;
    };
    match version_of(&path, timeout) {
        Err(reason) => ClaudeProbe::Broken { path, reason },
        Ok(version) if on_path => ClaudeProbe::OnPath { path, version },
        Ok(version) => {
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            ClaudeProbe::OffPath { path, dir, version }
        }
    }
}

/// Probe the current environment.
pub(crate) fn probe() -> ClaudeProbe {
    probe_with(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .as_deref(),
        VERSION_TIMEOUT,
    )
}

/// `PATH` with `dir` in front.
pub(crate) fn path_with(dir: &Path, path_var: Option<&OsStr>) -> Option<OsString> {
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(p) = path_var {
        dirs.extend(std::env::split_paths(p));
    }
    std::env::join_paths(dirs).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus_claude_probe_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_claude(dir: &Path, script: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let bin = dir.join("claude");
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    const OK: &str = "echo '2.1.0 (Claude Code)'";

    #[test]
    fn test_missing_claude_is_fatal_with_install_hint() {
        let root = tmp("missing");
        let home = root.join("home");
        let empty = root.join("bin");
        std::fs::create_dir_all(&empty).unwrap();
        let probe = probe_with(Some(empty.as_os_str()), Some(&home), VERSION_TIMEOUT);
        assert_eq!(probe, ClaudeProbe::Missing);
        assert!(probe.is_fatal());
        match probe.check_result() {
            CheckResult::Fail(msg) => assert!(msg.contains(INSTALL_HINT)),
            other => panic!("expected Fail, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_claude_only_in_local_bin_is_found_off_path() {
        let root = tmp("off_path");
        let home = root.join("home");
        let bin = fake_claude(&home.join(".local/bin"), OK);
        let empty = root.join("bin");
        std::fs::create_dir_all(&empty).unwrap();
        let probe = probe_with(Some(empty.as_os_str()), Some(&home), VERSION_TIMEOUT);
        match &probe {
            ClaudeProbe::OffPath { path, dir, version } => {
                assert_eq!(path, &bin);
                assert_eq!(dir, &home.join(".local/bin"));
                assert_eq!(version, "2.1.0 (Claude Code)");
            }
            other => panic!("expected OffPath, got {other:?}"),
        }
        assert!(!probe.is_fatal());
        assert!(probe.check_result().is_warn());
        let new_path = path_with(&home.join(".local/bin"), Some(empty.as_os_str())).unwrap();
        let first = std::env::split_paths(&new_path).next().unwrap();
        assert_eq!(first, home.join(".local/bin"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_claude_local_dir_is_the_second_fallback() {
        let root = tmp("claude_local");
        let home = root.join("home");
        let bin = fake_claude(&home.join(".claude/local"), OK);
        let probe = probe_with(None, Some(&home), VERSION_TIMEOUT);
        assert!(matches!(probe, ClaudeProbe::OffPath { ref path, .. } if path == &bin));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_claude_on_path_passes_with_version() {
        let root = tmp("on_path");
        let home = root.join("home");
        let path_dir = root.join("bin");
        fake_claude(&path_dir, OK);
        // A fallback copy must not win over PATH.
        fake_claude(&home.join(".local/bin"), "echo other");
        let probe = probe_with(Some(path_dir.as_os_str()), Some(&home), VERSION_TIMEOUT);
        match &probe {
            ClaudeProbe::OnPath { path, version } => {
                assert_eq!(path, &path_dir.join("claude"));
                assert_eq!(version, "2.1.0 (Claude Code)");
            }
            other => panic!("expected OnPath, got {other:?}"),
        }
        assert!(!probe.check_result().is_warn() && !probe.check_result().is_fail());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_failing_claude_is_broken() {
        let root = tmp("broken");
        let path_dir = root.join("bin");
        fake_claude(&path_dir, "exit 3");
        let probe = probe_with(Some(path_dir.as_os_str()), None, VERSION_TIMEOUT);
        assert!(matches!(probe, ClaudeProbe::Broken { .. }));
        assert!(probe.is_fatal());
        match probe.check_result() {
            CheckResult::Fail(msg) => {
                assert!(msg.contains(&path_dir.join("claude").display().to_string()));
                assert!(msg.contains(INSTALL_HINT));
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_hanging_claude_times_out() {
        let root = tmp("hang");
        let path_dir = root.join("bin");
        fake_claude(&path_dir, "exec sleep 5");
        let probe = probe_with(Some(path_dir.as_os_str()), None, Duration::from_millis(200));
        match probe {
            ClaudeProbe::Broken { reason, .. } => assert!(reason.contains("did not answer")),
            other => panic!("expected Broken, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_non_executable_file_is_ignored() {
        let root = tmp("noexec");
        let path_dir = root.join("bin");
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::write(path_dir.join("claude"), "not a program").unwrap();
        assert_eq!(locate(Some(path_dir.as_os_str()), None), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
