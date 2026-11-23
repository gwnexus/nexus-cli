// nexusctl/src/cmd/zellij_clipboard.rs
//
// Whether text copied in a Zellij pane reaches the system clipboard.
// Without `copy_command` Zellij copies via OSC 52, which iTerm2 drops unless
// clipboard access is enabled and Terminal.app does not support at all. The
// check reads the effective Zellij config, the OS and the terminal; the fix
// writes one `copy_command` line into a user-owned config, with a backup.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::run::ZellijConfigSource;

/// What `nexus doctor` and `nexus run` need to know about the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClipboardHost {
    pub macos: bool,
    pub linux: bool,
    /// `TERM_PROGRAM`, else `LC_TERMINAL`.
    pub terminal: Option<String>,
    /// `SSH_TTY` or `SSH_CONNECTION` set.
    pub ssh: bool,
    /// `WAYLAND_DISPLAY` set.
    pub wayland: bool,
    /// iTerm2 "Applications in terminal may access clipboard", when readable.
    pub iterm_clipboard_access: Option<bool>,
    pub has_wl_copy: bool,
    pub has_xclip: bool,
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl ClipboardHost {
    pub(crate) fn detect() -> Self {
        let macos = cfg!(target_os = "macos");
        let terminal = non_empty_env("TERM_PROGRAM").or_else(|| non_empty_env("LC_TERMINAL"));
        let iterm = terminal.as_deref().map(terminal_kind) == Some(TermKind::ITerm2);
        ClipboardHost {
            macos,
            linux: cfg!(target_os = "linux"),
            ssh: non_empty_env("SSH_TTY").is_some() || non_empty_env("SSH_CONNECTION").is_some(),
            wayland: non_empty_env("WAYLAND_DISPLAY").is_some(),
            iterm_clipboard_access: (macos && iterm).then(read_iterm_clipboard_access).flatten(),
            has_wl_copy: super::run::on_path("wl-copy"),
            has_xclip: super::run::on_path("xclip"),
            terminal,
        }
    }
}

/// `defaults read com.googlecode.iterm2 AllowClipboardAccess` (`1` = on).
fn read_iterm_clipboard_access() -> Option<bool> {
    let out = std::process::Command::new("defaults")
        .args(["read", "com.googlecode.iterm2", "AllowClipboardAccess"])
        .output()
        .ok()?;
    if !out.status.success() {
        // The key is absent until the setting was changed once: off.
        return Some(false);
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermKind {
    ITerm2,
    AppleTerm,
    /// Supports OSC 52 clipboard writes out of the box.
    Osc52,
    Other,
}

fn terminal_kind(name: &str) -> TermKind {
    match name.to_ascii_lowercase().as_str() {
        "iterm.app" | "iterm2" => TermKind::ITerm2,
        "apple_terminal" => TermKind::AppleTerm,
        "ghostty" | "wezterm" | "kitty" | "xterm-kitty" => TermKind::Osc52,
        _ => TermKind::Other,
    }
}

/// The clipboard-relevant settings of a Zellij config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ClipboardSettings {
    pub copy_command: Option<String>,
    pub copy_on_select: Option<bool>,
    pub mouse_mode: Option<bool>,
}

/// The first quoted string or bare word after `key` on an active line.
fn setting<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.trim().strip_prefix(key)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    match rest.strip_prefix('"') {
        Some(q) => q.find('"').map(|end| &q[..end]),
        None => rest.split_whitespace().next(),
    }
}

/// Parse the uncommented `copy_command`, `copy_on_select` and `mouse_mode`.
pub(crate) fn parse_settings(kdl: &str) -> ClipboardSettings {
    let mut out = ClipboardSettings::default();
    let mut in_block_comment = false;
    for line in kdl.lines() {
        let trimmed = line.trim();
        if in_block_comment {
            in_block_comment = !trimmed.contains("*/");
            continue;
        }
        if trimmed.starts_with("/*") {
            in_block_comment = !trimmed.contains("*/");
            continue;
        }
        if let Some(v) = setting(trimmed, "copy_command") {
            out.copy_command = Some(v.to_string());
        } else if let Some(v) = setting(trimmed, "copy_on_select") {
            out.copy_on_select = Some(v == "true");
        } else if let Some(v) = setting(trimmed, "mouse_mode") {
            out.mouse_mode = Some(v == "true");
        }
    }
    out
}

/// Severity of the clipboard check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Level {
    Ok,
    Info,
    Warn,
}

/// Result of the clipboard check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Verdict {
    pub level: Level,
    pub message: String,
    /// The `copy_command` line that fixes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_line: Option<String>,
    /// What to change on the terminal side instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternative: Option<String>,
}

const ITERM_SETTING: &str = "or enable iTerm2 Settings > General > Selection > \
     \"Applications in terminal may access clipboard\"";

/// The local copy command for this host, if there is a usable one.
fn local_copy_command(host: &ClipboardHost) -> Option<&'static str> {
    if host.macos {
        Some("pbcopy")
    } else if host.linux && host.wayland && host.has_wl_copy {
        Some("wl-copy")
    } else if host.linux && host.has_xclip {
        Some("xclip -selection clipboard")
    } else {
        None
    }
}

/// `copy_command "<cmd>"`.
pub(crate) fn fix_line(cmd: &str) -> String {
    format!("copy_command \"{cmd}\"")
}

/// Decide whether copying from a pane reaches the clipboard.
pub(crate) fn evaluate(settings: &ClipboardSettings, host: &ClipboardHost) -> Verdict {
    let verdict = |level, message: String, fix: Option<&str>, alt: Option<&str>| Verdict {
        level,
        message,
        fix_line: fix.map(fix_line),
        alternative: alt.map(str::to_string),
    };
    if let Some(cmd) = &settings.copy_command {
        return verdict(Level::Ok, format!("copy_command \"{cmd}\""), None, None);
    }
    if host.ssh {
        return verdict(
            Level::Ok,
            "OSC 52 (over SSH, the local terminal receives the copy)".into(),
            None,
            None,
        );
    }
    let terminal = host.terminal.as_deref().map(terminal_kind);
    let local = local_copy_command(host);
    if host.macos {
        return match terminal {
            Some(TermKind::ITerm2) if host.iterm_clipboard_access == Some(true) => verdict(
                Level::Ok,
                "OSC 52 (iTerm2 clipboard access is enabled)".into(),
                None,
                None,
            ),
            Some(TermKind::ITerm2) => verdict(
                Level::Warn,
                "no copy_command; iTerm2 drops OSC 52 clipboard writes by default".into(),
                local,
                Some(ITERM_SETTING),
            ),
            Some(TermKind::AppleTerm) => verdict(
                Level::Warn,
                "no copy_command; Terminal.app does not support OSC 52".into(),
                local,
                None,
            ),
            Some(TermKind::Osc52) => verdict(
                Level::Info,
                "OSC 52 (supported by this terminal)".into(),
                None,
                None,
            ),
            _ => verdict(
                Level::Info,
                "no copy_command; copying relies on the terminal's OSC 52 support".into(),
                local,
                None,
            ),
        };
    }
    if host.linux {
        return match local {
            Some(cmd) => verdict(
                Level::Warn,
                format!("no copy_command; {cmd} is available for the system clipboard"),
                Some(cmd),
                None,
            ),
            None => verdict(
                Level::Info,
                "no copy_command; copying relies on the terminal's OSC 52 support \
                 (install wl-copy or xclip for a local copy command)"
                    .into(),
                None,
                None,
            ),
        };
    }
    verdict(
        Level::Info,
        "no copy_command; copying relies on the terminal's OSC 52 support".into(),
        None,
        None,
    )
}

/// How to bypass Zellij's mouse capture for a native selection.
pub(crate) fn bypass_hint(host: &ClipboardHost) -> &'static str {
    if host.terminal.as_deref().map(terminal_kind) == Some(TermKind::ITerm2) {
        "hold Option while dragging to select with iTerm2 instead of Zellij"
    } else {
        "hold Shift while dragging to select with the terminal instead of Zellij"
    }
}

/// The user's global Zellij config file as Zellij resolves it:
/// `ZELLIJ_CONFIG_DIR`, else `~/.config/zellij`, else (macOS) the
/// application-support directory when a config exists there.
fn global_config_path(config_dir_env: Option<&str>, home: &Path) -> PathBuf {
    if let Some(dir) = config_dir_env.filter(|d| !d.trim().is_empty()) {
        return Path::new(dir).join("config.kdl");
    }
    let xdg = home.join(".config").join("zellij");
    let app_support = home
        .join("Library")
        .join("Application Support")
        .join("org.Zellij-Contributors.Zellij");
    if cfg!(target_os = "macos") && !xdg.is_dir() && app_support.join("config.kdl").is_file() {
        return app_support.join("config.kdl");
    }
    xdg.join("config.kdl")
}

/// The Zellij config a session uses and whether `--fix` may write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigTarget {
    pub path: PathBuf,
    /// False for the project-local config, which `nexus pull` owns.
    pub writable: bool,
}

pub(crate) fn config_target(source: &ZellijConfigSource, workspace: &Path) -> Option<ConfigTarget> {
    let target = match source {
        ZellijConfigSource::Env(p) => ConfigTarget {
            path: PathBuf::from(p),
            writable: true,
        },
        ZellijConfigSource::Project(rel) => ConfigTarget {
            path: workspace.join(rel),
            writable: false,
        },
        ZellijConfigSource::ProjectMissing(_) | ZellijConfigSource::Global => ConfigTarget {
            path: global_config_path(non_empty_env("ZELLIJ_CONFIG_DIR").as_deref(), &home_dir()?),
            writable: true,
        },
    };
    Some(target)
}

fn home_dir() -> Option<PathBuf> {
    non_empty_env("HOME").map(PathBuf::from)
}

/// Full check result for doctor (JSON) and run.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClipboardCheck {
    pub config: String,
    pub config_exists: bool,
    pub project_managed: bool,
    pub settings: ClipboardSettings,
    #[serde(flatten)]
    pub verdict: Verdict,
}

pub(crate) fn check(target: &ConfigTarget, host: &ClipboardHost) -> ClipboardCheck {
    let text = std::fs::read_to_string(&target.path).ok();
    let settings = text.as_deref().map(parse_settings).unwrap_or_default();
    ClipboardCheck {
        config: target.path.display().to_string(),
        config_exists: text.is_some(),
        project_managed: !target.writable,
        verdict: evaluate(&settings, host),
        settings,
    }
}

/// The config text with `line` active: an existing active `copy_command`
/// is kept (`None`, nothing to do), a commented template line with the same
/// command is uncommented, otherwise the line is appended. A missing file
/// becomes a minimal config with just that line.
pub(crate) fn apply_fix(current: Option<&str>, line: &str) -> Option<String> {
    let Some(text) = current else {
        return Some(format!(
            "// Zellij config; see `zellij setup --dump-config` for all options.\n{line}\n"
        ));
    };
    if parse_settings(text).copy_command.is_some() {
        return None;
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let template = lines.iter().position(|l| {
        l.trim()
            .strip_prefix("//")
            .map(str::trim_start)
            .is_some_and(|rest| setting(rest, "copy_command") == setting(line, "copy_command"))
    });
    match template {
        Some(i) => {
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            lines[i] = format!("{indent}{line}");
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(line.to_string());
        }
    }
    Some(lines.join("\n") + "\n")
}

/// Write the fix: back up an existing file to `config.kdl.bak-<date>`
/// first. Returns the backup path, if one was made.
pub(crate) fn write_fix(path: &Path, line: &str) -> anyhow::Result<Option<PathBuf>> {
    let current = std::fs::read_to_string(path).ok();
    let Some(next) = apply_fix(current.as_deref(), line) else {
        return Ok(None);
    };
    let backup = match &current {
        Some(text) => {
            let name = format!(
                "{}.bak-{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "config.kdl".into()),
                today()
            );
            let backup = path.with_file_name(name);
            std::fs::write(&backup, text)?;
            Some(backup)
        }
        None => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            None
        }
    };
    std::fs::write(path, next)?;
    Ok(backup)
}

/// Days since the Unix epoch (UTC).
fn epoch_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// `YYYY-MM-DD` (UTC) for an epoch day.
fn civil_date(day: u64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn today() -> String {
    civil_date(epoch_day())
}

/// Whether the `nexus run` warning is due: not yet shown today with this
/// CLI version. `state` is the content of the notice file.
fn notice_due(state: Option<&str>, version: &str, day: &str) -> bool {
    state.map(str::trim) != Some(&format!("{version} {day}"))
}

/// The `nexus run` warning line, at most once per day and CLI version.
/// `None` when the check passes or the warning was already shown.
pub(crate) fn run_notice(
    source: &ZellijConfigSource,
    workspace: &Path,
    nexus_dir: &Path,
) -> Option<String> {
    let target = config_target(source, workspace)?;
    let result = check(&target, &ClipboardHost::detect());
    if result.verdict.level != Level::Warn {
        return None;
    }
    let state_file = nexus_dir.join("state").join("zellij-clipboard-notice");
    let stamp_day = today();
    let version = env!("CARGO_PKG_VERSION");
    let state = std::fs::read_to_string(&state_file).ok();
    if !notice_due(state.as_deref(), version, &stamp_day) {
        return None;
    }
    if let Some(parent) = state_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&state_file, format!("{version} {stamp_day}\n"));
    Some(format!(
        "Zellij copy may not reach the clipboard ({}); run nexus doctor claude --fix",
        result.verdict.message
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_iterm() -> ClipboardHost {
        ClipboardHost {
            macos: true,
            terminal: Some("iTerm.app".into()),
            ..Default::default()
        }
    }

    fn linux() -> ClipboardHost {
        ClipboardHost {
            linux: true,
            terminal: Some("gnome-terminal".into()),
            has_wl_copy: true,
            has_xclip: true,
            ..Default::default()
        }
    }

    const TEMPLATE: &str = "// copy_command \"xclip -selection clipboard\" // x11\n\
        // copy_command \"wl-copy\"                    // wayland\n\
        // copy_command \"pbcopy\"                     // osx\n\
        mouse_mode true\n";

    #[test]
    fn test_parse_settings_ignores_comments() {
        let s = parse_settings(TEMPLATE);
        assert_eq!(s.copy_command, None);
        assert_eq!(s.mouse_mode, Some(true));
        let s = parse_settings(
            "/*\ncopy_command \"wl-copy\"\n*/\ncopy_on_select false\n  copy_command \"pbcopy\" // osx\n",
        );
        assert_eq!(s.copy_command.as_deref(), Some("pbcopy"));
        assert_eq!(s.copy_on_select, Some(false));
        // Keys only match as whole words.
        assert_eq!(parse_settings("copy_commands \"x\"\n").copy_command, None);
    }

    #[test]
    fn test_macos_iterm_without_copy_command_warns() {
        let v = evaluate(&ClipboardSettings::default(), &mac_iterm());
        assert_eq!(v.level, Level::Warn);
        assert_eq!(v.fix_line.as_deref(), Some("copy_command \"pbcopy\""));
        assert!(v.alternative.unwrap().contains("Applications in terminal"));

        // Clipboard access enabled in iTerm2: OSC 52 works.
        let host = ClipboardHost {
            iterm_clipboard_access: Some(true),
            ..mac_iterm()
        };
        assert_eq!(
            evaluate(&ClipboardSettings::default(), &host).level,
            Level::Ok
        );
    }

    #[test]
    fn test_macos_with_pbcopy_is_ok() {
        let s = parse_settings("copy_command \"pbcopy\"\n");
        let v = evaluate(&s, &mac_iterm());
        assert_eq!(v.level, Level::Ok);
        assert_eq!(v.fix_line, None);
    }

    #[test]
    fn test_macos_terminals() {
        let host = |t: &str| ClipboardHost {
            terminal: Some(t.into()),
            ..mac_iterm()
        };
        let none = ClipboardSettings::default();
        assert_eq!(evaluate(&none, &host("Apple_Terminal")).level, Level::Warn);
        assert_eq!(evaluate(&none, &host("ghostty")).level, Level::Info);
        assert_eq!(evaluate(&none, &host("WezTerm")).level, Level::Info);
        assert_eq!(evaluate(&none, &host("ghostty")).fix_line, None);
    }

    #[test]
    fn test_linux_wayland_and_x11() {
        let none = ClipboardSettings::default();
        let wayland = ClipboardHost {
            wayland: true,
            ..linux()
        };
        let v = evaluate(&none, &wayland);
        assert_eq!(v.level, Level::Warn);
        assert_eq!(v.fix_line.as_deref(), Some("copy_command \"wl-copy\""));
        let v = evaluate(&none, &linux());
        assert_eq!(
            v.fix_line.as_deref(),
            Some("copy_command \"xclip -selection clipboard\"")
        );
        // Wayland without wl-copy falls back to xclip; neither: info only.
        let bare = ClipboardHost {
            wayland: true,
            has_wl_copy: false,
            has_xclip: false,
            ..linux()
        };
        let v = evaluate(&none, &bare);
        assert_eq!(v.level, Level::Info);
        assert_eq!(v.fix_line, None);
    }

    #[test]
    fn test_ssh_prefers_osc52() {
        let none = ClipboardSettings::default();
        for host in [mac_iterm(), linux()] {
            let v = evaluate(&none, &ClipboardHost { ssh: true, ..host });
            assert_eq!(v.level, Level::Ok);
            assert_eq!(v.fix_line, None);
        }
    }

    #[test]
    fn test_apply_fix_missing_file_creates_minimal_config() {
        let out = apply_fix(None, "copy_command \"pbcopy\"").unwrap();
        assert_eq!(parse_settings(&out).copy_command.as_deref(), Some("pbcopy"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn test_apply_fix_uncomments_template_line_and_is_idempotent() {
        let out = apply_fix(Some(TEMPLATE), "copy_command \"pbcopy\"").unwrap();
        assert!(out.contains("\ncopy_command \"pbcopy\"\n"));
        // The other template lines stay commented, other settings unchanged.
        assert!(out.contains("// copy_command \"wl-copy\""));
        assert!(out.contains("mouse_mode true"));
        assert_eq!(out.lines().count(), TEMPLATE.lines().count());
        assert_eq!(apply_fix(Some(&out), "copy_command \"pbcopy\""), None);
    }

    #[test]
    fn test_apply_fix_appends_without_template() {
        let out = apply_fix(Some("theme \"dracula\""), "copy_command \"wl-copy\"").unwrap();
        assert_eq!(out, "theme \"dracula\"\n\ncopy_command \"wl-copy\"\n");
        assert_eq!(apply_fix(Some(&out), "copy_command \"wl-copy\""), None);
    }

    #[test]
    fn test_write_fix_backs_up_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zellij").join("config.kdl");
        // Missing file: created, no backup.
        assert_eq!(write_fix(&path, "copy_command \"pbcopy\"").unwrap(), None);
        // Already fixed: untouched.
        assert_eq!(write_fix(&path, "copy_command \"pbcopy\"").unwrap(), None);

        std::fs::write(&path, TEMPLATE).unwrap();
        let backup = write_fix(&path, "copy_command \"pbcopy\"")
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), TEMPLATE);
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("config.kdl.bak-"));
        let now = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_settings(&now).copy_command.as_deref(), Some("pbcopy"));
    }

    #[test]
    fn test_config_target_project_is_read_only() {
        let ws = Path::new("/ws");
        let t = config_target(&ZellijConfigSource::Project("z.kdl".into()), ws).unwrap();
        assert_eq!(t.path, Path::new("/ws/z.kdl"));
        assert!(!t.writable);
        let t = config_target(&ZellijConfigSource::Env("/e.kdl".into()), ws).unwrap();
        assert!(t.writable && t.path == Path::new("/e.kdl"));
    }

    #[test]
    fn test_global_config_path_respects_config_dir() {
        let home = Path::new("/nonexistent-home");
        assert_eq!(
            global_config_path(Some("/cfg"), home),
            Path::new("/cfg/config.kdl")
        );
        assert_eq!(
            global_config_path(None, home),
            Path::new("/nonexistent-home/.config/zellij/config.kdl")
        );
    }

    #[test]
    fn test_notice_once_per_day_and_version() {
        assert!(notice_due(None, "0.33.0", "2026-11-01"));
        assert!(!notice_due(
            Some("0.33.0 2026-11-01\n"),
            "0.33.0",
            "2026-11-01"
        ));
        assert!(notice_due(
            Some("0.33.0 2026-11-01"),
            "0.33.0",
            "2026-11-02"
        ));
        assert!(notice_due(
            Some("0.32.0 2026-11-01"),
            "0.33.0",
            "2026-11-01"
        ));
    }

    #[test]
    fn test_civil_date() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_393), "2025-11-01");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
