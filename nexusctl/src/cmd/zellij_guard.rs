// nexusctl/src/cmd/zellij_guard.rs
//
// Guards around the zellij launch of `nexus run`: refuse to start inside a
// running zellij session, check that the session socket path fits the OS
// limit, start fresh instead of resurrecting an exited session, and show
// zellij's log when it exits right away.

use std::path::{Path, PathBuf};

use super::preflight::CheckResult;

/// Zellij's client/server contract directory below the socket base.
const CONTRACT_DIR: &str = "contract_version_1";

/// Longest Unix socket path in bytes (`sun_path` minus the NUL).
#[cfg(target_os = "macos")]
const SOCKET_PATH_LIMIT: usize = 103;
#[cfg(not(target_os = "macos"))]
const SOCKET_PATH_LIMIT: usize = 107;

/// A zellij exit within this many seconds counts as a start failure.
pub const EARLY_EXIT_SECS: u64 = 5;

/// `session_name "<name>"` from a layout file.
pub fn session_name(layout: &str) -> Option<String> {
    layout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("session_name")?;
        let rest = rest.trim().strip_prefix('"')?;
        let name = &rest[..rest.find('"')?];
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The layout path from the zellij launch arguments (`--layout <path>`).
pub fn layout_arg(args: &[String]) -> Option<&Path> {
    args.iter()
        .position(|a| a == "--layout")
        .and_then(|i| args.get(i + 1))
        .map(Path::new)
}

fn uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Zellij's own temp directory (`$TMPDIR/zellij-<uid>`), which holds the
/// log and, on macOS, the sockets.
fn zellij_tmp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("zellij-{}", uid()))
}

/// Socket base directory as zellij resolves it: `ZELLIJ_SOCKET_DIR`, else
/// `$XDG_RUNTIME_DIR/zellij` (Linux), else `$TMPDIR/zellij-<uid>`; always
/// followed by the contract directory.
fn socket_dir(socket_env: Option<&str>, xdg_runtime: Option<&str>, tmp_dir: &Path) -> PathBuf {
    let base = match (socket_env, xdg_runtime) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(xdg)) if cfg!(target_os = "linux") => Path::new(xdg).join("zellij"),
        _ => tmp_dir.to_path_buf(),
    };
    base.join(CONTRACT_DIR)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Pure core of [`socket_check`].
fn check_socket_path(socket: &Path, limit: usize) -> CheckResult {
    let len = socket.as_os_str().len();
    if len > limit {
        CheckResult::Fail(format!(
            "socket path {} is {len} bytes, the limit is {limit}; \
             start with a shorter socket dir, e.g. ZELLIJ_SOCKET_DIR=/tmp/zj nexus run",
            socket.display()
        ))
    } else {
        CheckResult::Pass(format!("{len}/{limit} bytes"))
    }
}

/// Pre-launch row "Zellij socket" for a layout's `session_name`.
pub(crate) fn socket_check(session: &str) -> CheckResult {
    let dir = socket_dir(
        non_empty_env("ZELLIJ_SOCKET_DIR").as_deref(),
        non_empty_env("XDG_RUNTIME_DIR").as_deref(),
        &zellij_tmp_dir(),
    );
    check_socket_path(&dir.join(session), SOCKET_PATH_LIMIT)
}

/// What to do when `nexus run` is started inside a zellij session.
#[derive(Debug, PartialEq, Eq)]
pub enum Nested {
    /// Not inside zellij: launch.
    No,
    /// Inside the project's own session already.
    SameSession(String),
    /// Inside another session (name, if zellij exported it).
    OtherSession(Option<String>),
}

pub fn nested(inside: bool, current: Option<&str>, target: Option<&str>) -> Nested {
    if !inside {
        return Nested::No;
    }
    match (current, target) {
        (Some(c), Some(t)) if c == t => Nested::SameSession(c.to_string()),
        (c, _) => Nested::OtherSession(c.map(str::to_string)),
    }
}

pub fn detect_nested(target: Option<&str>) -> Nested {
    let current = non_empty_env("ZELLIJ_SESSION_NAME");
    nested(
        std::env::var_os("ZELLIJ").is_some(),
        current.as_deref(),
        target,
    )
}

/// State of a named session in `zellij list-sessions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Missing,
    Running,
    /// Serialized after an abnormal end; attaching resurrects the old
    /// layout instead of starting the current one.
    Exited,
}

/// Every session in `zellij list-sessions --no-formatting` output, e.g.
/// `name [Created 2days ago] (EXITED - attach to resurrect)`.
pub fn parse_sessions(output: &str) -> Vec<(String, SessionState)> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let name = line.split_whitespace().next()?;
            let state = if line.contains("(EXITED") {
                SessionState::Exited
            } else {
                SessionState::Running
            };
            Some((name.to_string(), state))
        })
        .collect()
}

/// State of `name` in a session list.
pub fn state_of(sessions: &[(String, SessionState)], name: &str) -> SessionState {
    sessions
        .iter()
        .find(|(n, _)| n == name)
        .map_or(SessionState::Missing, |(_, s)| *s)
}

/// Sessions as zellij lists them; `None` when zellij cannot be asked.
pub fn list_sessions() -> Option<Vec<(String, SessionState)>> {
    let out = std::process::Command::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    // Without sessions zellij exits non-zero ("No active zellij sessions").
    Some(parse_sessions(&String::from_utf8_lossy(&out.stdout)))
}

/// Delete an exited session (never `--force`, so a running one survives).
pub fn delete_session(name: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("zellij")
        .args(["delete-session", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(status.success(), "zellij delete-session {name} failed");
    Ok(())
}

/// Doctor rows for the project's session (`project`) and the names of
/// other exited sessions.
pub(crate) fn doctor_rows(
    sessions: &[(String, SessionState)],
    project: Option<&str>,
) -> (Option<CheckResult>, Vec<String>) {
    let row = project.map(|name| match state_of(sessions, name) {
        SessionState::Exited => CheckResult::Warn(format!(
            "{name}: exited; nexus run removes it and starts fresh (fix: zellij delete-session {name})"
        )),
        SessionState::Running => CheckResult::Pass(format!("{name}: running")),
        SessionState::Missing => CheckResult::Pass(format!("{name}: not started")),
    });
    let others = sessions
        .iter()
        .filter(|(n, s)| *s == SessionState::Exited && Some(n.as_str()) != project)
        .map(|(n, _)| n.clone())
        .collect();
    (row, others)
}

/// How `nexus run` enters the project's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStart {
    Attach,
    Fresh,
    /// Remove the exited session first, then start fresh.
    FreshAfterDelete,
    Resurrect,
}

pub fn session_start(state: SessionState, resume: bool) -> SessionStart {
    match (state, resume) {
        (SessionState::Running, _) => SessionStart::Attach,
        (SessionState::Missing, _) => SessionStart::Fresh,
        (SessionState::Exited, true) => SessionStart::Resurrect,
        (SessionState::Exited, false) => SessionStart::FreshAfterDelete,
    }
}

/// Pre-launch row "Zellij session".
pub(crate) fn session_start_check(name: &str, start: SessionStart) -> CheckResult {
    match start {
        SessionStart::Attach => CheckResult::Info(format!("{name}: attach to running session")),
        SessionStart::Fresh => CheckResult::Pass(format!("{name}: fresh start")),
        SessionStart::FreshAfterDelete => CheckResult::Warn(format!(
            "{name}: fresh start (removes the exited session; --resume resurrects it)"
        )),
        SessionStart::Resurrect => CheckResult::Warn(format!(
            "{name}: resurrect exited session (old layout, not the current one)"
        )),
    }
}

/// Path of zellij's log file.
pub fn log_path() -> PathBuf {
    zellij_tmp_dir().join("zellij-log").join("zellij.log")
}

/// The last `n` lines of `content`.
fn tail(content: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = content.lines().collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// The last lines of zellij's log, if it can be read.
pub fn log_tail(n: usize) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(log_path()).ok()?;
    Some(tail(&content, n).into_iter().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_name_from_layout() {
        let kdl = "layout {\n}\nsession_name \"nexus-cli\"\nattach_to_session true\n";
        assert_eq!(session_name(kdl).as_deref(), Some("nexus-cli"));
        assert_eq!(
            session_name("  session_name \"a b\" // x").as_deref(),
            Some("a b")
        );
        assert_eq!(session_name("layout {}\n"), None);
        assert_eq!(session_name("session_name \"\""), None);
        assert_eq!(session_name("session_name_x \"y\""), None);
    }

    #[test]
    fn test_layout_arg() {
        let args: Vec<String> = ["--config", "/c.kdl", "--layout", "/l.kdl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(layout_arg(&args), Some(Path::new("/l.kdl")));
        assert_eq!(layout_arg(&args[..3]), None);
    }

    #[test]
    fn test_socket_dir_precedence() {
        let tmp = Path::new("/var/folders/xx/T/zellij-502");
        assert_eq!(
            socket_dir(Some("/tmp/zj"), Some("/run/user/502"), tmp),
            Path::new("/tmp/zj/contract_version_1")
        );
        let fallback = socket_dir(None, None, tmp);
        assert_eq!(fallback, tmp.join("contract_version_1"));
        let xdg = socket_dir(None, Some("/run/user/502"), tmp);
        if cfg!(target_os = "linux") {
            assert_eq!(xdg, Path::new("/run/user/502/zellij/contract_version_1"));
        } else {
            assert_eq!(xdg, fallback);
        }
    }

    #[test]
    fn test_socket_path_over_limit_fails_with_length_and_hint() {
        // The reported case: 79-byte prefix plus a 27-character name.
        let prefix = format!(
            "/var/folders/xx/{}/T/zellij-502/contract_version_1/",
            "a".repeat(30)
        );
        assert_eq!(prefix.len(), 79);
        let socket = PathBuf::from(format!("{prefix}{}", "s".repeat(27)));
        match check_socket_path(&socket, 103) {
            CheckResult::Fail(msg) => {
                assert!(msg.contains("106 bytes"), "{msg}");
                assert!(msg.contains("limit is 103"), "{msg}");
                assert!(msg.contains("ZELLIJ_SOCKET_DIR=/tmp/zj"), "{msg}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        let short = PathBuf::from(format!("{prefix}{}", "s".repeat(24)));
        assert!(matches!(
            check_socket_path(&short, 103),
            CheckResult::Pass(_)
        ));
    }

    #[test]
    fn test_nested_detection() {
        assert_eq!(nested(false, Some("x"), Some("x")), Nested::No);
        assert_eq!(
            nested(true, Some("nexus-cli"), Some("nexus-cli")),
            Nested::SameSession("nexus-cli".into())
        );
        assert_eq!(
            nested(true, Some("nexus-app"), Some("nexus-cli")),
            Nested::OtherSession(Some("nexus-app".into()))
        );
        assert_eq!(
            nested(true, None, Some("nexus-cli")),
            Nested::OtherSession(None)
        );
        // A layout without session_name: any running session is another one.
        assert_eq!(
            nested(true, Some("a"), None),
            Nested::OtherSession(Some("a".into()))
        );
    }

    #[test]
    fn test_parse_sessions_and_state() {
        let out = "beta [Created 23days ago] (EXITED - attach to resurrect)\n\
                   cli [Created 14days ago] (current)\n\
                   app [Created 13days ago] \n";
        let sessions = parse_sessions(out);
        assert_eq!(state_of(&sessions, "beta"), SessionState::Exited);
        assert_eq!(state_of(&sessions, "cli"), SessionState::Running);
        assert_eq!(state_of(&sessions, "app"), SessionState::Running);
        assert_eq!(state_of(&sessions, "be"), SessionState::Missing);
        assert!(parse_sessions("").is_empty());
    }

    #[test]
    fn test_doctor_rows() {
        let sessions = parse_sessions(
            "proj [Created 1d ago] (EXITED - attach to resurrect)\n\
             other [Created 2d ago] (EXITED - attach to resurrect)\n\
             live [Created 3d ago] \n",
        );
        let (row, others) = doctor_rows(&sessions, Some("proj"));
        let row = row.unwrap();
        assert_eq!(row.level(), "warn");
        assert!(row.message().contains("zellij delete-session proj"));
        assert_eq!(others, ["other"]);
        let (row, _) = doctor_rows(&sessions, Some("live"));
        assert_eq!(row.unwrap().level(), "pass");
        let (row, others) = doctor_rows(&sessions, None);
        assert!(row.is_none());
        assert_eq!(others, ["proj", "other"]);
    }

    #[test]
    fn test_session_start() {
        use SessionStart::*;
        assert_eq!(session_start(SessionState::Running, false), Attach);
        assert_eq!(session_start(SessionState::Running, true), Attach);
        assert_eq!(session_start(SessionState::Missing, true), Fresh);
        assert_eq!(session_start(SessionState::Exited, false), FreshAfterDelete);
        assert_eq!(session_start(SessionState::Exited, true), Resurrect);
        assert!(session_start_check("p", FreshAfterDelete)
            .message()
            .contains("fresh start"));
        assert!(session_start_check("p", Attach)
            .message()
            .contains("attach to running session"));
        assert!(session_start_check("p", Resurrect)
            .message()
            .contains("resurrect"));
    }

    #[test]
    fn test_tail() {
        assert_eq!(tail("a\nb\nc\n", 2), ["b", "c"]);
        assert_eq!(tail("a", 5), ["a"]);
        assert!(tail("", 3).is_empty());
    }
}
