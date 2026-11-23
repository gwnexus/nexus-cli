//! Code signals: a small, code-free summary of what a repository contains.
//!
//! The hub cannot see the code, so the CLI reports which known marker files
//! exist (`Cargo.toml`, `package.json`, ...) and how many tracked files there
//! are per extension. The hub derives suggestions for codebase tags,
//! language-server plugins and a devbox blueprint from it.
//!
//! The report is built from `git ls-files` in the workspace (tracked files
//! only, so ignored build output never counts) and holds nothing else: no
//! file contents, no file names outside the marker list, no repository root
//! path, no remotes.
//!
//! It is sent on `nexus link`, on `nexus run` at most once per 24 hours per
//! project, and on demand with `nexus code-report`. The automatic reports
//! are off with `code_signals = false` or `NEXUS_NO_CODE_SIGNALS=1`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use console::style;
use nexus_core::api::{CodeSignals, NexusClient};
use nexus_core::auth::require_token;
use nexus_core::config::{self, Config};
use nexus_core::Error;
use serde::{Deserialize, Serialize};

/// Known marker files, matched against the file name. A single `*` matches
/// any run of characters.
const MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "tsconfig.json",
    "next.config.*",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "uv.lock",
    "poetry.lock",
    "composer.json",
    "Gemfile",
    "pom.xml",
    "build.gradle*",
    "*.csproj",
    "*.sln",
    "CMakeLists.txt",
    "Makefile",
    "meson.build",
    "Package.swift",
    "Dockerfile",
    "docker-compose*.yml",
    "docker-compose*.yaml",
    "compose*.yml",
    "compose*.yaml",
    "*.tf",
    "*.tofu",
    "ansible.cfg",
    "playbook*.yml",
    "playbook*.yaml",
    "galaxy.yml",
    "Chart.yaml",
    "kustomization.yaml",
    "devbox.json",
    "flake.nix",
];

/// Deepest directory level a marker may sit in (`a/b/c/Cargo.toml` is 3).
pub(crate) const MAX_MARKER_DEPTH: usize = 3;
/// Most marker paths in one report.
pub(crate) const MAX_MARKERS: usize = 200;
/// Most extensions in one report (the most frequent ones).
pub(crate) const MAX_EXTENSIONS: usize = 30;
/// Minimum time between two automatic reports from `nexus run`.
pub(crate) const RUN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Time budget of the report started by `nexus run`.
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(3);
/// Time budget of the report sent by `nexus link` and `nexus code-report`.
const LINK_TIMEOUT: Duration = Duration::from_secs(10);
/// Environment variable that turns the automatic reports off.
pub(crate) const OPT_OUT_ENV: &str = "NEXUS_NO_CODE_SIGNALS";

/// True for files Nexus manages in a workspace. They do not describe the
/// code, so they never count as markers or extensions and do not make a
/// repository non-empty.
fn is_managed(path: &str) -> bool {
    path.starts_with(".nexus/")
        || path.starts_with(".claude/")
        || path == "CLAUDE.md"
        || path == "AGENTS.md"
}

/// `pattern` with at most one `*` against `name`.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

/// True when the file name of `path` is a known marker.
fn is_marker(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    MARKERS.iter().any(|p| glob_match(p, name))
}

/// Directory level of `path` (`Cargo.toml` is 0, `crates/core/Cargo.toml` 2).
fn dir_depth(path: &str) -> usize {
    path.matches('/').count()
}

/// Lowercase extension of `path`, `""` for none (also for dotfiles such as
/// `.gitignore`).
fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Build the report from workspace-relative tracked paths.
pub(crate) fn collect(paths: &[String], scanned_at: String) -> CodeSignals {
    let code: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .filter(|p| !p.is_empty() && !is_managed(p))
        .collect();

    let mut markers: Vec<&str> = code
        .iter()
        .copied()
        .filter(|p| dir_depth(p) <= MAX_MARKER_DEPTH && is_marker(p))
        .collect();
    // Shallow markers first: they describe the project best when the cap hits.
    markers.sort_by(|a, b| dir_depth(a).cmp(&dir_depth(b)).then(a.cmp(b)));
    markers.truncate(MAX_MARKERS);

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in &code {
        *counts.entry(extension(p)).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(MAX_EXTENSIONS);

    CodeSignals {
        source: "cli".into(),
        cli_version: env!("CARGO_PKG_VERSION").into(),
        scanned_at,
        file_count: paths.iter().filter(|p| !p.is_empty()).count(),
        empty: code.is_empty(),
        markers: markers.into_iter().map(str::to_string).collect(),
        extensions: ranked.into_iter().collect(),
    }
}

/// Tracked files below `workspace`, relative to it. Outside a git repository
/// (or without git) the list is empty.
pub(crate) fn tracked_files(workspace: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "-z"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Scan `workspace` now.
pub(crate) fn scan(workspace: &Path) -> CodeSignals {
    collect(
        &tracked_files(workspace),
        super::dispatch_status::rfc3339_now(),
    )
}

/// True unless the user turned the automatic reports off.
pub(crate) fn enabled(config: &Config, opt_out: Option<&str>) -> bool {
    let opted_out = opt_out
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"));
    config.code_signals && !opted_out
}

/// [`enabled`] with the effective config and the environment.
fn enabled_here(workspace: &Path) -> bool {
    let config = Config::load_effective(Some(workspace)).unwrap_or_default();
    enabled(&config, std::env::var(OPT_OUT_ENV).ok().as_deref())
}

// ---------------------------------------------------------------------------
// Last report time (per project, local)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReportState {
    /// Unix seconds of the last report the hub answered.
    reported_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `<dir>/<project>.json`; `None` for an id that is not a plain UUID-like
/// string, so it can never leave the directory.
fn state_file(dir: &Path, project_id: &str) -> Option<PathBuf> {
    let valid = !project_id.is_empty()
        && project_id.len() <= 64
        && project_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then(|| dir.join(format!("{project_id}.json")))
}

fn state_dir() -> Option<PathBuf> {
    Config::dir()
        .ok()
        .map(|d| d.join("state").join("code-signals"))
}

fn last_report(dir: &Path, project_id: &str) -> Option<u64> {
    let path = state_file(dir, project_id)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<ReportState>(&text)
        .ok()
        .map(|s| s.reported_at)
}

fn record_report(dir: &Path, project_id: &str, at: u64) {
    let Some(path) = state_file(dir, project_id) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string(&ReportState { reported_at: at }) {
        let _ = std::fs::write(path, text);
    }
}

/// True when no report was sent within [`RUN_INTERVAL`] (a time in the
/// future, from a changed clock, counts as due too).
pub(crate) fn due(last: Option<u64>, now: u64) -> bool {
    match last {
        None => true,
        Some(t) if t > now => true,
        Some(t) => now - t >= RUN_INTERVAL.as_secs(),
    }
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// How the hub took a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Stored {
        suggestions: u32,
    },
    /// The hub has no code-signals endpoint yet (404).
    Unsupported,
    Failed(String),
}

impl Outcome {
    /// True when the hub answered, so the 24-hour interval starts.
    fn answered(&self) -> bool {
        !matches!(self, Outcome::Failed(_))
    }
}

pub(crate) fn outcome_of(result: Result<nexus_core::api::CodeSignalsResponse, Error>) -> Outcome {
    match result {
        Ok(r) => Outcome::Stored {
            suggestions: r.suggestions,
        },
        Err(Error::NotFound(_)) => Outcome::Unsupported,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// The one line to print for an automatic report, if any: suggestions or a
/// warning. Nothing for a plain store or an older hub.
pub(crate) fn notice_line(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Stored { suggestions } if *suggestions > 0 => Some(format!(
            "Nexus: {suggestions} project improvements suggested, see the project page."
        )),
        Outcome::Failed(e) => Some(format!("code signals not sent: {e}")),
        _ => None,
    }
}

fn print_notice(outcome: &Outcome) {
    if let Some(line) = notice_line(outcome) {
        let mark = match outcome {
            Outcome::Failed(_) => style("!").bold().yellow(),
            _ => style("i").bold().blue(),
        };
        println!("   {mark} {line}");
    }
}

/// Scan `workspace` and send the report within `budget`; records the time
/// when the hub answered.
async fn scan_and_send(
    client: NexusClient,
    project_id: String,
    workspace: PathBuf,
    budget: Duration,
) -> Outcome {
    let work = async {
        let ws = workspace.clone();
        let signals = tokio::task::spawn_blocking(move || scan(&ws))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(outcome_of(
            client.report_code_signals(&project_id, &signals).await,
        ))
    };
    let outcome = match tokio::time::timeout(budget, work).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => Outcome::Failed(e),
        Err(_) => Outcome::Failed(format!("no answer within {} s", budget.as_secs())),
    };
    if outcome.answered() {
        if let Some(dir) = state_dir() {
            record_report(&dir, &project_id, now_secs());
        }
    }
    outcome
}

/// Report after `nexus link`. Never fails the link.
pub(crate) async fn report_after_link(client: &NexusClient, project_id: &str, workspace: &Path) {
    if !enabled_here(workspace) {
        return;
    }
    let outcome = scan_and_send(
        client.clone(),
        project_id.to_string(),
        workspace.to_path_buf(),
        LINK_TIMEOUT,
    )
    .await;
    print_notice(&outcome);
}

/// Start the `nexus run` report in the background when it is due. The task
/// finishes within [`RUN_TIMEOUT`] and yields the line to print, if any.
pub(crate) fn spawn_for_run(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
) -> Option<tokio::task::JoinHandle<Option<String>>> {
    if !enabled_here(workspace) {
        return None;
    }
    let last = state_dir().and_then(|d| last_report(&d, project_id));
    if !due(last, now_secs()) {
        return None;
    }
    let (client, project_id, workspace) = (
        client.clone(),
        project_id.to_string(),
        workspace.to_path_buf(),
    );
    Some(tokio::spawn(async move {
        let outcome = scan_and_send(client, project_id, workspace, RUN_TIMEOUT).await;
        notice_line(&outcome)
    }))
}

/// Wait for the report started by [`spawn_for_run`] and print its line.
pub(crate) async fn finish_for_run(handle: Option<tokio::task::JoinHandle<Option<String>>>) {
    let Some(handle) = handle else { return };
    // The task bounds itself; the extra second covers scheduling only.
    let line = tokio::time::timeout(RUN_TIMEOUT + Duration::from_secs(1), handle)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    if let Some(line) = line {
        let mark = if line.starts_with("Nexus:") {
            style("i").bold().blue()
        } else {
            style("!").bold().yellow()
        };
        println!("   {mark} {line}");
    }
}

/// `nexus code-report [--dry-run]`.
pub async fn code_report(api_url: &str, dry_run: bool) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    if dry_run {
        let signals = scan(&workspace);
        println!("{}", serde_json::to_string_pretty(&signals)?);
        return Ok(());
    }
    let project_id =
        config::resolve_project_id(None, Some(&workspace)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = NexusClient::new(api_url, Some(require_token()?))?;
    let outcome = scan_and_send(client, project_id, workspace, LINK_TIMEOUT).await;
    match outcome {
        Outcome::Stored { suggestions } => {
            println!("{} Code signals sent.", style("OK").bold().green());
            if suggestions > 0 {
                println!(
                    "   {} Nexus: {suggestions} project improvements suggested, see the project page.",
                    style("i").bold().blue()
                );
            }
            Ok(())
        }
        Outcome::Unsupported => {
            println!(
                "{} The hub does not accept code signals yet; nothing sent.",
                style("--").bold().yellow()
            );
            Ok(())
        }
        Outcome::Failed(e) => anyhow::bail!("code signals not sent: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn signals(list: &[&str]) -> CodeSignals {
        collect(&paths(list), "2026-01-01T00:00:00Z".into())
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "cargo.toml"));
        assert!(glob_match("next.config.*", "next.config.mjs"));
        assert!(glob_match("*.csproj", "App.csproj"));
        assert!(glob_match("build.gradle*", "build.gradle"));
        assert!(glob_match("build.gradle*", "build.gradle.kts"));
        assert!(glob_match("docker-compose*.yml", "docker-compose.dev.yml"));
        assert!(!glob_match("*.tf", "tf"));
        assert!(!glob_match("compose*.yml", "compose.yaml"));
    }

    #[test]
    fn test_markers_match_file_names_only() {
        let s = signals(&[
            "Cargo.toml",
            "nexusctl/Cargo.toml",
            "src/main.rs",
            "infra/main.tf",
            "Cargo.toml.bak",
            "docs/Dockerfile.md",
            "deploy/Dockerfile",
        ]);
        assert_eq!(
            s.markers,
            vec![
                "Cargo.toml",
                "deploy/Dockerfile",
                "infra/main.tf",
                "nexusctl/Cargo.toml"
            ]
        );
    }

    #[test]
    fn test_marker_depth_limit() {
        let s = signals(&["a/b/c/package.json", "a/b/c/d/package.json", "go.mod"]);
        assert_eq!(s.markers, vec!["go.mod", "a/b/c/package.json"]);
    }

    #[test]
    fn test_marker_cap_keeps_shallow_ones() {
        let mut list: Vec<String> = (0..250)
            .map(|i| format!("p{i:03}/x/package.json"))
            .collect();
        list.push("Cargo.toml".into());
        let s = collect(&list, "t".into());
        assert_eq!(s.markers.len(), MAX_MARKERS);
        assert_eq!(s.markers[0], "Cargo.toml");
    }

    #[test]
    fn test_extensions_top_30_cut() {
        let mut list = Vec::new();
        for i in 0..40 {
            for _ in 0..(i + 1) {
                list.push(format!("f.e{i:02}"));
            }
        }
        let s = collect(&list, "t".into());
        assert_eq!(s.extensions.len(), MAX_EXTENSIONS);
        // The 30 most frequent are e10..e39; the rarest ten are cut.
        assert!(s.extensions.contains_key("e39"));
        assert!(s.extensions.contains_key("e10"));
        assert!(!s.extensions.contains_key("e09"));
        assert_eq!(s.extensions["e39"], 40);
    }

    #[test]
    fn test_extensions_are_lowercase_and_cover_files_without_one() {
        let s = signals(&["a.RS", "b.rs", "Makefile", ".gitignore", "x.tar.GZ"]);
        assert_eq!(s.extensions["rs"], 2);
        assert_eq!(s.extensions[""], 2);
        assert_eq!(s.extensions["gz"], 1);
    }

    #[test]
    fn test_empty_detection_ignores_managed_files() {
        let s = signals(&[
            ".nexus/config.toml",
            ".claude/settings.json",
            "CLAUDE.md",
            "AGENTS.md",
        ]);
        assert!(s.empty);
        assert_eq!(s.file_count, 4);
        assert!(s.markers.is_empty());
        assert!(s.extensions.is_empty());

        let s = signals(&[".nexus/devbox.json", "README.md"]);
        assert!(!s.empty);
        assert!(s.markers.is_empty(), "managed paths are no markers");

        let s = signals(&[]);
        assert!(s.empty);
        assert_eq!(s.file_count, 0);
    }

    #[test]
    fn test_payload_holds_only_the_agreed_fields() {
        let s = signals(&["Cargo.toml", "src/secret_plan.rs"]);
        let v = serde_json::to_value(&s).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "cli_version",
                "empty",
                "extensions",
                "file_count",
                "markers",
                "scanned_at",
                "source"
            ]
        );
        assert_eq!(v["source"], "cli");
        let text = v.to_string();
        assert!(!text.contains("secret_plan"), "{text}");
    }

    #[test]
    fn test_tracked_files_reads_git_and_skips_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(root.join("sub/crate")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("sub/crate/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("sub/notes.txt"), "").unwrap();
        std::fs::write(root.join("untracked.rs"), "").unwrap();
        git(&["add", "Cargo.toml", "sub"]);

        let mut all = tracked_files(root);
        all.sort();
        assert_eq!(
            all,
            vec!["Cargo.toml", "sub/crate/Cargo.toml", "sub/notes.txt"]
        );

        // A sub-directory workspace sees only its own files, relative to it.
        let mut sub = tracked_files(&root.join("sub"));
        sub.sort();
        assert_eq!(sub, vec!["crate/Cargo.toml", "notes.txt"]);

        let outside = tempfile::tempdir().unwrap();
        assert!(tracked_files(outside.path()).is_empty());
    }

    #[test]
    fn test_enabled_honours_config_and_env() {
        let on = Config::default();
        assert!(enabled(&on, None));
        assert!(enabled(&on, Some("")));
        assert!(enabled(&on, Some("0")));
        assert!(enabled(&on, Some("false")));
        assert!(!enabled(&on, Some("1")));
        assert!(!enabled(&on, Some("yes")));
        let off = Config {
            code_signals: false,
            ..Default::default()
        };
        assert!(!enabled(&off, None));
    }

    #[test]
    fn test_due_after_24_hours() {
        let day = RUN_INTERVAL.as_secs();
        assert!(due(None, 1_000));
        assert!(!due(Some(1_000), 1_000 + day - 1));
        assert!(due(Some(1_000), 1_000 + day));
        assert!(due(Some(5_000), 1_000), "a future time counts as due");
    }

    #[test]
    fn test_report_state_round_trip_and_safe_names() {
        let dir = tempfile::tempdir().unwrap();
        let id = "7c3e9a21-4b6d-4f80-9e12-3a5b7c9d1e2f";
        assert_eq!(last_report(dir.path(), id), None);
        record_report(dir.path(), id, 42);
        assert_eq!(last_report(dir.path(), id), Some(42));
        assert!(state_file(dir.path(), "../x").is_none());
        assert!(state_file(dir.path(), "").is_none());
    }

    #[test]
    fn test_outcomes_and_lines() {
        use nexus_core::api::CodeSignalsResponse;
        let stored = outcome_of(Ok(CodeSignalsResponse {
            stored: true,
            suggestions: 3,
        }));
        assert_eq!(stored, Outcome::Stored { suggestions: 3 });
        assert_eq!(
            notice_line(&stored).unwrap(),
            "Nexus: 3 project improvements suggested, see the project page."
        );
        assert!(notice_line(&Outcome::Stored { suggestions: 0 }).is_none());

        let old_hub = outcome_of(Err(Error::NotFound("HTTP 404".into())));
        assert_eq!(old_hub, Outcome::Unsupported);
        assert!(notice_line(&old_hub).is_none(), "a 404 stays silent");
        assert!(old_hub.answered());

        let failed = outcome_of(Err(Error::Api("HTTP 500".into())));
        assert!(!failed.answered());
        assert!(notice_line(&failed).unwrap().contains("HTTP 500"));
    }
}
