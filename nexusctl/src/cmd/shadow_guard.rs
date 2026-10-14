//! Shadow guard: content rules for shadowed projects.
//!
//! When a project's shadow level is not `off`, the backend sends a
//! `shadow_guard` with the excluded agentic paths and portable regex
//! patterns, each limited to scopes (`commit_message`, `tracked_files`).
//! The CLI applies them as sent, never adding its own:
//! - `nexus shadow check` scans tracked files and recent commit messages
//!   and lists excluded paths that git still tracks (read-only);
//! - `nexus pull` caches the guard locally and installs a `commit-msg` hook
//!   that checks each message offline via `nexus shadow check-msg`;
//! - `nexus shadow status` / `nexus shadow <level>` print the effective
//!   state with a check summary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use console::style;
use nexus_core::api::{ShadowGuard, ShadowPattern};
use regex::Regex;
use serde::Serialize;

pub const SCOPE_COMMIT: &str = "commit_message";
pub const SCOPE_FILES: &str = "tracked_files";

/// First comment line of the hook Nexus installs; identifies it as ours.
const HOOK_MARKER: &str = "# nexus-shadow-guard";
/// Name an untracked foreign `commit-msg` hook is moved to; ours runs it first.
const CHAINED_HOOK: &str = "commit-msg.pre-nexus";
/// Files larger than this are not scanned.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Characters of context kept on each side of a match.
const EXCERPT_CONTEXT: usize = 24;

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// A compiled pattern of one scope.
pub struct Rule<'a> {
    pattern: &'a ShadowPattern,
    re: Regex,
}

/// The guard's patterns that apply to `scope`, compiled. A pattern that does
/// not compile is reported and skipped (the backend promises portable RE2).
pub fn compile<'a>(guard: &'a ShadowGuard, scope: &str) -> Vec<Rule<'a>> {
    guard
        .patterns
        .iter()
        .filter(|p| p.scopes.iter().any(|s| s == scope))
        .filter_map(|p| {
            let source = if p.flags.contains('i') {
                format!("(?i){}", p.regex)
            } else {
                p.regex.clone()
            };
            match Regex::new(&source) {
                Ok(re) => Some(Rule { pattern: p, re }),
                Err(e) => {
                    eprintln!(
                        "   {} shadow guard pattern '{}' skipped: {}",
                        style("!").bold().yellow(),
                        p.id,
                        e
                    );
                    None
                }
            }
        })
        .collect()
}

/// One match of a pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub pattern: String,
    pub message: String,
    /// `path:line` for files, `<commit> line <n>` for commit messages.
    pub location: String,
    /// The match with a little context, trimmed (never the whole line).
    pub excerpt: String,
}

/// The match plus up to [`EXCERPT_CONTEXT`] characters on each side.
fn excerpt(line: &str, start: usize, end: usize) -> String {
    let before: String = line[..start]
        .chars()
        .rev()
        .take(EXCERPT_CONTEXT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = line[end..].chars().take(EXCERPT_CONTEXT).collect();
    let lead = if before.len() < start { "…" } else { "" };
    let tail = if after.len() < line.len() - end {
        "…"
    } else {
        ""
    };
    format!(
        "{lead}{}{}{}{tail}",
        before.trim_start(),
        &line[start..end],
        after.trim_end()
    )
}

/// Every line of `text` matching a rule (one hit per rule and line).
fn scan_text(text: &str, rules: &[Rule], location: impl Fn(usize) -> String) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for rule in rules {
            if let Some(m) = rule.re.find(line) {
                hits.push(Hit {
                    pattern: rule.pattern.id.clone(),
                    message: rule.pattern.message.clone(),
                    location: location(i + 1),
                    excerpt: excerpt(line, m.start(), m.end()),
                });
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------------
// Repository scans
// ---------------------------------------------------------------------------

fn git_bytes(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Top level of the git work tree containing `dir`.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git_bytes(dir, &["rev-parse", "--show-toplevel"])?;
    let path = String::from_utf8_lossy(&out).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Whether the repository-relative `path` falls under the exclude `entry`
/// (gitignore-like: `dir/` covers everything below, a bare name matches at
/// any depth, a leading `/` anchors at the root).
pub fn is_excluded(path: &str, entry: &str) -> bool {
    let anchored = entry.starts_with('/');
    let e = entry.trim_start_matches('/');
    if e.is_empty() {
        return false;
    }
    if let Some(dir) = e.strip_suffix('/') {
        path.starts_with(&format!("{dir}/"))
            || (!anchored && !dir.contains('/') && path.contains(&format!("/{dir}/")))
    } else {
        path == e || (!anchored && !e.contains('/') && path.ends_with(&format!("/{e}")))
    }
}

fn tracked_files(root: &Path) -> Vec<String> {
    git_bytes(root, &["ls-files", "-z"])
        .map(|out| {
            out.split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// An excluded path that git still tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StillTracked {
    pub path: String,
    pub files: usize,
    /// The one-time fix; never run automatically.
    pub fix: String,
}

fn still_tracked(guard: &ShadowGuard, tracked: &[String]) -> Vec<StillTracked> {
    guard
        .excluded_paths
        .iter()
        .filter_map(|entry| {
            let files = tracked.iter().filter(|p| is_excluded(p, entry)).count();
            (files > 0).then(|| StillTracked {
                path: entry.clone(),
                files,
                fix: format!("git rm --cached -r -- {}", entry.trim_start_matches('/')),
            })
        })
        .collect()
}

fn scan_files(root: &Path, guard: &ShadowGuard, tracked: &[String]) -> Vec<Hit> {
    let rules = compile(guard, SCOPE_FILES);
    if rules.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for path in tracked {
        if guard.excluded_paths.iter().any(|e| is_excluded(path, e)) {
            continue;
        }
        let full = root.join(path);
        if fs::metadata(&full).map_or(true, |m| !m.is_file() || m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = fs::read(&full) else { continue };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        hits.extend(scan_text(&text, &rules, |n| format!("{path}:{n}")));
    }
    hits
}

/// Scan the last `limit` commit messages of the current branch. Returns the
/// hits and the number of commits scanned.
fn scan_commits(root: &Path, guard: &ShadowGuard, limit: usize) -> (Vec<Hit>, usize) {
    let rules = compile(guard, SCOPE_COMMIT);
    let Some(out) = git_bytes(
        root,
        &["log", "-n", &limit.to_string(), "--format=%h%x00%B%x1e"],
    ) else {
        return (Vec::new(), 0); // no commits yet
    };
    let text = String::from_utf8_lossy(&out);
    let mut hits = Vec::new();
    let mut scanned = 0;
    for record in text.split('\u{1e}') {
        let Some((sha, body)) = record.trim_start_matches('\n').split_once('\0') else {
            continue;
        };
        scanned += 1;
        hits.extend(scan_text(body, &rules, |n| {
            format!("commit {sha} line {n}")
        }));
    }
    (hits, scanned)
}

/// Result of `nexus shadow check`.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub level: String,
    pub files: Vec<Hit>,
    pub commits: Vec<Hit>,
    pub commits_scanned: usize,
    pub still_tracked: Vec<StillTracked>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.files.is_empty() && self.commits.is_empty() && self.still_tracked.is_empty()
    }
}

pub fn run_check(root: &Path, guard: &ShadowGuard, commits: usize) -> CheckReport {
    let tracked = tracked_files(root);
    let (commit_hits, commits_scanned) = scan_commits(root, guard, commits);
    CheckReport {
        level: guard.level.clone(),
        files: scan_files(root, guard, &tracked),
        commits: commit_hits,
        commits_scanned,
        still_tracked: still_tracked(guard, &tracked),
    }
}

/// One-line summary of a check, for `nexus shadow status` / `<level>`.
pub fn summary_line(report: &CheckReport) -> String {
    if report.is_clean() {
        return format!(
            "clean (tracked files, last {} commit message(s), excluded paths)",
            report.commits_scanned
        );
    }
    format!(
        "{} finding(s) in tracked files, {} in the last {} commit message(s), {} excluded path(s) still tracked (details: nexus shadow check)",
        report.files.len(),
        report.commits.len(),
        report.commits_scanned,
        report.still_tracked.len()
    )
}

pub fn print_report(report: &CheckReport) {
    println!(
        "{} Shadow check (level {})",
        style(">>").bold().cyan(),
        style(&report.level).bold()
    );
    let section = |title: &str, hits: &[Hit]| {
        println!();
        println!("  {} ({})", style(title).bold(), hits.len());
        for h in hits {
            println!(
                "    {}  {}: {}",
                h.location,
                style(&h.message).yellow(),
                style(&h.excerpt).dim()
            );
        }
    };
    section("Tracked files", &report.files);
    section(
        &format!("Commit messages (last {})", report.commits_scanned),
        &report.commits,
    );
    println!();
    println!(
        "  {} ({})",
        style("Excluded paths still tracked by git").bold(),
        report.still_tracked.len()
    );
    for t in &report.still_tracked {
        println!(
            "    {} ({} file(s)); fix once: {}",
            t.path,
            t.files,
            style(&t.fix).bold()
        );
    }
    println!();
    if report.is_clean() {
        println!("{} Nothing found.", style("OK").bold().green());
    } else {
        println!(
            "{} Rephrase the findings; commit history is not rewritten by Nexus.",
            style("!").bold().yellow()
        );
    }
}

// ---------------------------------------------------------------------------
// Local cache (read by the commit-msg hook, no network)
// ---------------------------------------------------------------------------

/// Workspace-relative path of the cached guard.
pub fn cache_rel_path(agentic_root: &str) -> String {
    format!(
        "{}/generated/shadow-guard.json",
        agentic_root.trim_end_matches('/')
    )
}

pub fn load_cache(path: &Path) -> Option<ShadowGuard> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

// ---------------------------------------------------------------------------
// commit-msg hook
// ---------------------------------------------------------------------------

/// `nexus shadow check-msg`: check a commit message file against the cached
/// guard. Returns the exit code (1 = rejected). A missing cache means the
/// guard is off: the message passes.
pub fn check_message_file(guard_path: &Path, message_path: &Path) -> anyhow::Result<i32> {
    let Some(guard) = load_cache(guard_path) else {
        return Ok(0);
    };
    let raw = fs::read_to_string(message_path)?;
    let hits = check_message(&guard, &raw);
    if hits.is_empty() {
        return Ok(0);
    }
    eprintln!(
        "Commit message rejected by the shadow guard (level {}):",
        guard.level
    );
    for h in &hits {
        eprintln!("  {}: {}", h.message, h.excerpt);
    }
    eprintln!("Rephrase it, or bypass the check once with: git commit --no-verify");
    Ok(1)
}

/// The hits of a raw commit message as git would store it: comment lines
/// and everything below the scissors line are ignored.
pub fn check_message(guard: &ShadowGuard, raw: &str) -> Vec<Hit> {
    let message: String = raw
        .lines()
        .take_while(|l| !l.starts_with("# ------------------------ >8"))
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    let rules = compile(guard, SCOPE_COMMIT);
    scan_text(&message, &rules, |n| format!("line {n}"))
}

/// The directory git runs hooks from (honours `core.hooksPath`).
fn hooks_dir(root: &Path) -> Option<PathBuf> {
    let out = git_bytes(root, &["rev-parse", "--git-path", "hooks"])?;
    let p = PathBuf::from(String::from_utf8_lossy(&out).trim());
    Some(if p.is_absolute() { p } else { root.join(p) })
}

/// A path as a single-quoted shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn hook_script(guard_rel: &str) -> String {
    format!(
        "#!/bin/sh\n\
         {HOOK_MARKER}: installed by nexus pull for the project's shadow guard,\n\
         # removed again when the guard or its commit-msg check is switched off.\n\
         hook_dir=$(dirname \"$0\")\n\
         if [ -x \"$hook_dir/{CHAINED_HOOK}\" ]; then\n\
         \x20 \"$hook_dir/{CHAINED_HOOK}\" \"$@\" || exit $?\n\
         fi\n\
         if command -v nexus >/dev/null 2>&1; then\n\
         \x20 exec nexus shadow check-msg --guard {} \"$1\"\n\
         fi\n\
         echo \"shadow guard: 'nexus' is not on PATH; commit message not checked.\" >&2\n\
         exit 0\n",
        sh_quote(guard_rel)
    )
}

/// Why a foreign hook is left alone.
fn foreign_reason(root: &Path, hook: &Path, content: &str) -> Option<String> {
    let rel = hook
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    if let Some(rel) = rel {
        if git_bytes(root, &["ls-files", "--error-unmatch", "--", &rel]).is_some() {
            return Some(format!("{rel} is tracked in the repository"));
        }
    }
    let dir = hook
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dir.contains(".husky") || content.contains("husky") {
        return Some("hooks are managed by husky".into());
    }
    if content.contains("lefthook") {
        return Some("hooks are managed by lefthook".into());
    }
    if content.contains("pre-commit.com") || content.contains("File generated by pre-commit") {
        return Some("hooks are managed by the pre-commit framework".into());
    }
    None
}

/// What [`install_hook`] / [`remove_hook`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    Installed {
        path: PathBuf,
        chained: bool,
    },
    Unchanged {
        path: PathBuf,
    },
    /// A foreign hook is in the way and was not touched.
    Skipped {
        path: PathBuf,
        reason: String,
        guard_rel: String,
    },
    Removed {
        path: PathBuf,
        restored: bool,
    },
    /// Not a git work tree.
    NoRepo,
    /// Nothing to remove.
    None,
}

/// Install (or update) the commit-msg hook. An untracked foreign hook is
/// chained (moved aside and run first); a tracked one or one owned by a hook
/// manager is left alone and reported.
pub fn install_hook(root: &Path, guard_rel: &str) -> anyhow::Result<HookAction> {
    let Some(dir) = hooks_dir(root) else {
        return Ok(HookAction::NoRepo);
    };
    let hook = dir.join("commit-msg");
    let script = hook_script(guard_rel);
    let mut chained = dir.join(CHAINED_HOOK).exists();
    if let Ok(existing) = fs::read_to_string(&hook) {
        if existing.contains(HOOK_MARKER) {
            if existing == script {
                return Ok(HookAction::Unchanged { path: hook });
            }
        } else if let Some(reason) = foreign_reason(root, &hook, &existing) {
            return Ok(HookAction::Skipped {
                path: hook,
                reason,
                guard_rel: guard_rel.to_string(),
            });
        } else if chained {
            anyhow::bail!(
                "{} and {} both exist; resolve them by hand",
                hook.display(),
                dir.join(CHAINED_HOOK).display()
            );
        } else {
            fs::rename(&hook, dir.join(CHAINED_HOOK))?;
            chained = true;
        }
    }
    fs::create_dir_all(&dir)?;
    fs::write(&hook, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    }
    // A hooks directory inside the work tree (core.hooksPath): keep the
    // generated hook out of the repository.
    if let Ok(rel) = hook.strip_prefix(root) {
        let rel = rel.to_string_lossy().into_owned();
        super::shadow::ensure_git_excluded_in(root, &[rel.as_str()], "shadow guard hook");
    }
    Ok(HookAction::Installed {
        path: hook,
        chained,
    })
}

/// Remove the hook Nexus installed (a foreign hook is never removed) and
/// restore a chained foreign hook.
pub fn remove_hook(root: &Path) -> anyhow::Result<HookAction> {
    let Some(dir) = hooks_dir(root) else {
        return Ok(HookAction::NoRepo);
    };
    let hook = dir.join("commit-msg");
    let ours = fs::read_to_string(&hook).is_ok_and(|c| c.contains(HOOK_MARKER));
    if !ours {
        return Ok(HookAction::None);
    }
    fs::remove_file(&hook)?;
    let chained = dir.join(CHAINED_HOOK);
    let restored = chained.exists();
    if restored {
        fs::rename(&chained, &hook)?;
    }
    Ok(HookAction::Removed {
        path: hook,
        restored,
    })
}

/// Whether the Nexus hook is the active commit-msg hook.
pub fn hook_installed(root: &Path) -> bool {
    hooks_dir(root)
        .and_then(|d| fs::read_to_string(d.join("commit-msg")).ok())
        .is_some_and(|c| c.contains(HOOK_MARKER))
}

fn print_hook_action(root: &Path, action: &HookAction) {
    let show = |p: &Path| {
        p.strip_prefix(root)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    };
    match action {
        HookAction::Installed { path, chained } => println!(
            "   {} shadow guard commit-msg hook installed ({}{})",
            style("+").bold().green(),
            show(path),
            if *chained {
                format!(", runs the existing hook {CHAINED_HOOK} first")
            } else {
                String::new()
            }
        ),
        HookAction::Removed { path, restored } => println!(
            "   {} shadow guard commit-msg hook removed ({}{})",
            style("-").bold().red(),
            show(path),
            if *restored {
                ", previous hook restored"
            } else {
                ""
            }
        ),
        HookAction::Skipped {
            path,
            reason,
            guard_rel,
        } => {
            println!(
                "   {} shadow guard commit-msg hook not installed: {} ({}). Add this line to that hook:",
                style("!").bold().yellow(),
                reason,
                show(path)
            );
            println!(
                "      nexus shadow check-msg --guard {} \"$1\" || exit 1",
                sh_quote(guard_rel)
            );
        }
        HookAction::Unchanged { .. } | HookAction::NoRepo | HookAction::None => {}
    }
}

// ---------------------------------------------------------------------------
// Integration
// ---------------------------------------------------------------------------

/// Apply the export's guard after `nexus pull` (or a level change): cache it
/// for the hook and install or remove the hook. Never fails the caller.
pub fn sync(workspace: &Path, agentic_root: &str, guard: Option<&ShadowGuard>) {
    let Some(root) = repo_root(workspace) else {
        return;
    };
    let rel = cache_rel_path(agentic_root);
    let cache = workspace.join(&rel);
    let guard_rel = workspace
        .strip_prefix(&root)
        .map(|p| p.join(&rel).to_string_lossy().into_owned())
        .unwrap_or(rel);
    let result = match guard {
        Some(g) => {
            let json = serde_json::to_string_pretty(g).unwrap_or_default() + "\n";
            if fs::read_to_string(&cache).ok().as_deref() != Some(json.as_str()) {
                if let Some(parent) = cache.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&cache, json);
            }
            if g.commit_msg_hook {
                install_hook(&root, &guard_rel)
            } else {
                remove_hook(&root)
            }
        }
        None => {
            let _ = fs::remove_file(&cache);
            remove_hook(&root)
        }
    };
    match result {
        Ok(action) => print_hook_action(&root, &action),
        Err(e) => println!(
            "   {} shadow guard commit-msg hook: {}",
            style("!").bold().yellow(),
            e
        ),
    }
}

/// The effective shadow state, for `nexus shadow status` and
/// `nexus shadow <level>` (printed also when nothing changed).
pub fn state_lines(
    level: &str,
    guard: Option<&ShadowGuard>,
    hook_installed: bool,
    report: Option<&CheckReport>,
) -> Vec<String> {
    let mut lines = vec![format!("Shadow level:     {level}")];
    let Some(g) = guard else {
        lines.push(if level == "off" {
            "Shadow guard:     off".to_string()
        } else {
            "Shadow guard:     not sent by the backend (older version)".to_string()
        });
        return lines;
    };
    let still = report.map_or(0, |r| r.still_tracked.len());
    lines.push(format!(
        "Excluded paths:   {} ({} still tracked by git)",
        g.excluded_paths.len(),
        still
    ));
    lines.push(format!(
        "Commit-msg hook:  {}",
        match (g.commit_msg_hook, hook_installed) {
            (true, true) => "installed",
            (true, false) => "not installed (run nexus pull)",
            (false, true) => "installed, but switched off for this project (run nexus pull)",
            (false, false) => "switched off for this project",
        }
    ));
    lines.push(
        "Note:             switching shadow on does not clean content that is already committed."
            .to_string(),
    );
    if let Some(r) = report {
        lines.push(format!("Check:            {}", summary_line(r)));
    }
    lines
}

pub fn print_state(root: Option<&Path>, level: &str, guard: Option<&ShadowGuard>) {
    let report = match (root, guard) {
        (Some(root), Some(g)) => Some(run_check(root, g, 50)),
        _ => None,
    };
    let installed = root.is_some_and(hook_installed);
    println!();
    for line in state_lines(level, guard, installed, report.as_ref()) {
        println!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(patterns: Vec<ShadowPattern>) -> ShadowGuard {
        ShadowGuard {
            level: "nexus".into(),
            commit_msg_hook: true,
            excluded_paths: vec![".claude/".into(), ".nexus/".into(), "AGENTS.md".into()],
            patterns,
        }
    }

    fn pat(id: &str, regex: &str, flags: &str, scopes: &[&str]) -> ShadowPattern {
        ShadowPattern {
            id: id.into(),
            regex: regex.into(),
            flags: flags.into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            message: format!("{id} found"),
        }
    }

    fn contract_guard() -> ShadowGuard {
        guard(vec![
            pat(
                "nexus-adr",
                r"\bADR-\d{3,4}\b",
                "i",
                &[SCOPE_COMMIT, SCOPE_FILES],
            ),
            pat(
                "ai-coauthor",
                r"co-authored-by:\s*claude",
                "i",
                &[SCOPE_COMMIT],
            ),
        ])
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-shadow-guard-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        dir.canonicalize().unwrap()
    }

    #[test]
    fn test_compile_respects_scopes_and_flags() {
        let g = guard(vec![
            pat("files-only", r"secret-term", "", &[SCOPE_FILES]),
            pat("msg-ci", r"adr-\d{4}", "i", &[SCOPE_COMMIT]),
            pat("msg-cs", r"adr-\d{4}", "", &[SCOPE_COMMIT]),
            pat("broken", r"(", "", &[SCOPE_COMMIT]),
        ]);
        let commit = compile(&g, SCOPE_COMMIT);
        let ids: Vec<&str> = commit.iter().map(|r| r.pattern.id.as_str()).collect();
        assert_eq!(ids, vec!["msg-ci", "msg-cs"]);
        let hits = scan_text("see ADR-0125", &commit, |n| n.to_string());
        assert_eq!(hits.len(), 1, "only the case-insensitive pattern matches");
        assert_eq!(hits[0].pattern, "msg-ci");
        assert!(scan_text("secret-term", &commit, |n| n.to_string()).is_empty());
        assert_eq!(compile(&g, SCOPE_FILES).len(), 1);
    }

    #[test]
    fn test_excerpt_is_trimmed_and_char_safe() {
        let line = format!("{}ADR-0125{}", "ä".repeat(60), "ö".repeat(60));
        let start = line.find("ADR").unwrap();
        let e = excerpt(&line, start, start + 8);
        assert!(e.starts_with('…') && e.ends_with('…') && e.contains("ADR-0125"));
        assert!(e.chars().count() <= 8 + 2 * EXCERPT_CONTEXT + 2);
        assert_eq!(excerpt("see ADR-0125", 4, 12), "see ADR-0125");
    }

    #[test]
    fn test_is_excluded() {
        assert!(is_excluded(".claude/settings.json", ".claude/"));
        assert!(is_excluded("sub/.claude/x", ".claude/"));
        assert!(!is_excluded("sub/.claude/x", "/.claude/"));
        assert!(!is_excluded(".claudette/x", ".claude/"));
        assert!(is_excluded("AGENTS.md", "AGENTS.md"));
        assert!(is_excluded("docs/AGENTS.md", "AGENTS.md"));
        assert!(!is_excluded("docs/AGENTS.md", "/AGENTS.md"));
        assert!(!is_excluded("AGENTS.md.bak", "AGENTS.md"));
        assert!(!is_excluded("x", ""));
    }

    #[test]
    fn test_check_finds_files_commits_and_still_tracked_paths() {
        let dir = repo("check");
        fs::write(dir.join("notes.md"), "Background: see ADR-0125 for why.\n").unwrap();
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(".claude/settings.json"), "{\"x\": \"ADR-0001\"}\n").unwrap();
        fs::write(dir.join("bin.dat"), b"ADR-0125\0\0binary").unwrap();
        git(&dir, &["add", "-A"]);
        git(
            &dir,
            &[
                "commit",
                "-q",
                "-m",
                "feat: x\n\nCo-Authored-By: Claude <noreply@x>",
            ],
        );
        git(
            &dir,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fix(api): handle null ids",
            ],
        );

        let report = run_check(&dir, &contract_guard(), 50);
        assert!(!report.is_clean());
        assert_eq!(report.commits_scanned, 2);
        assert_eq!(
            report.files.len(),
            1,
            "excluded and binary files are skipped"
        );
        assert_eq!(report.files[0].location, "notes.md:1");
        assert_eq!(report.commits.len(), 1);
        assert_eq!(report.commits[0].pattern, "ai-coauthor");
        assert_eq!(
            report.still_tracked,
            vec![StillTracked {
                path: ".claude/".into(),
                files: 1,
                fix: "git rm --cached -r -- .claude/".into()
            }]
        );
        assert!(summary_line(&report).starts_with("1 finding(s) in tracked files, 1 in the last 2"));

        // Only the last commit: its message is clean.
        assert!(run_check(&dir, &contract_guard(), 1).commits.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_message_ignores_comments_and_scissors() {
        let g = contract_guard();
        assert_eq!(check_message(&g, "see ADR-0125\n").len(), 1);
        assert!(check_message(&g, "fix(api): handle null ids\n").is_empty());
        let raw = "fix: x\n# see ADR-0125 in a comment\n# ------------------------ >8 ------------------------\ndiff ADR-0125\n";
        assert!(check_message(&g, raw).is_empty());
    }

    #[test]
    fn test_check_message_file_exit_codes() {
        let dir = repo("check-msg");
        let cache = dir.join("guard.json");
        let msg = dir.join("MSG");
        fs::write(&msg, "see ADR-0125\n").unwrap();
        // No cache: the guard is off, everything passes.
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 0);
        fs::write(&cache, serde_json::to_string(&contract_guard()).unwrap()).unwrap();
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 1);
        fs::write(&msg, "fix(api): handle null ids\n").unwrap();
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hook_install_update_and_remove() {
        let dir = repo("hook-plain");
        let hook = dir.join(".git/hooks/commit-msg");
        let a = install_hook(&dir, ".nexus/generated/shadow-guard.json").unwrap();
        assert_eq!(
            a,
            HookAction::Installed {
                path: hook.clone(),
                chained: false
            }
        );
        let script = fs::read_to_string(&hook).unwrap();
        assert!(script.contains(HOOK_MARKER));
        assert!(script.contains("check-msg --guard '.nexus/generated/shadow-guard.json'"));
        assert!(hook_installed(&dir));
        assert_eq!(
            install_hook(&dir, ".nexus/generated/shadow-guard.json").unwrap(),
            HookAction::Unchanged { path: hook.clone() }
        );
        assert_eq!(
            remove_hook(&dir).unwrap(),
            HookAction::Removed {
                path: hook.clone(),
                restored: false
            }
        );
        assert!(!hook.exists());
        assert_eq!(remove_hook(&dir).unwrap(), HookAction::None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hook_chains_untracked_foreign_hook_and_restores_it() {
        let dir = repo("hook-chain");
        let hooks = dir.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("commit-msg"), "#!/bin/sh\necho mine\n").unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert_eq!(
            a,
            HookAction::Installed {
                path: hooks.join("commit-msg"),
                chained: true
            }
        );
        assert_eq!(
            fs::read_to_string(hooks.join(CHAINED_HOOK)).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
        assert!(fs::read_to_string(hooks.join("commit-msg"))
            .unwrap()
            .contains(CHAINED_HOOK));
        let r = remove_hook(&dir).unwrap();
        assert!(matches!(r, HookAction::Removed { restored: true, .. }));
        assert_eq!(
            fs::read_to_string(hooks.join("commit-msg")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
        assert!(!hooks.join(CHAINED_HOOK).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hook_leaves_tracked_and_managed_hooks_alone() {
        // core.hooksPath pointing at a tracked directory with its own hook.
        let dir = repo("hook-tracked");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/commit-msg"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "hooks"]);
        Command::new("git")
            .args(["config", "core.hooksPath", ".githooks"])
            .current_dir(&dir)
            .status()
            .unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(&a, HookAction::Skipped { reason, .. } if reason.contains("tracked")));
        assert_eq!(
            fs::read_to_string(dir.join(".githooks/commit-msg")).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        assert_eq!(remove_hook(&dir).unwrap(), HookAction::None);
        let _ = fs::remove_dir_all(&dir);

        // An untracked hook written by a hook manager.
        let dir = repo("hook-lefthook");
        let hooks = dir.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("commit-msg"),
            "#!/bin/sh\nlefthook run commit-msg\n",
        )
        .unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(&a, HookAction::Skipped { reason, .. } if reason.contains("lefthook")));
        assert!(!hooks.join(CHAINED_HOOK).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hook_in_work_tree_hooks_path_is_git_excluded() {
        let dir = repo("hook-hookspath");
        Command::new("git")
            .args(["config", "core.hooksPath", ".githooks"])
            .current_dir(&dir)
            .status()
            .unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(a, HookAction::Installed { chained: false, .. }));
        assert!(dir.join(".githooks/commit-msg").exists());
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l.trim() == ".githooks/commit-msg"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_state_lines_for_an_already_shadowed_repo() {
        let g = contract_guard();
        let report = CheckReport {
            level: "nexus".into(),
            files: vec![],
            commits: vec![],
            commits_scanned: 50,
            still_tracked: vec![],
        };
        let lines = state_lines("nexus", Some(&g), true, Some(&report));
        assert_eq!(lines[0], "Shadow level:     nexus");
        assert_eq!(lines[1], "Excluded paths:   3 (0 still tracked by git)");
        assert_eq!(lines[2], "Commit-msg hook:  installed");
        assert!(lines[3].contains("does not clean content"));
        assert!(lines[4].starts_with("Check:            clean"));
        assert_eq!(
            state_lines("off", None, false, None),
            vec![
                "Shadow level:     off".to_string(),
                "Shadow guard:     off".to_string()
            ]
        );
    }

    #[test]
    fn test_shadow_guard_parses_contract() {
        let json = r#"{"level":"nexus","excluded_paths":[".claude/"],
            "patterns":[{"id":"nexus-adr","regex":"\\bADR-\\d{3,4}\\b","flags":"i",
            "scopes":["commit_message","tracked_files"],"message":"ADR number"}]}"#;
        let g: ShadowGuard = serde_json::from_str(json).unwrap();
        assert!(g.commit_msg_hook, "defaults to true");
        assert_eq!(g.patterns[0].scopes.len(), 2);
        assert_eq!(compile(&g, SCOPE_FILES).len(), 1);
    }

    /// End to end with real `git commit`s through the installed hook and the
    /// built `nexus` binary (`cargo build` first; run with `--ignored`).
    #[test]
    #[ignore]
    fn e2e_commit_msg_hook_with_built_binary() {
        let bin_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(bin_dir.join("nexus").exists(), "run cargo build first");
        let dir = repo("e2e");
        let rel = cache_rel_path(".nexus");
        fs::create_dir_all(dir.join(".nexus/generated")).unwrap();
        fs::write(
            dir.join(&rel),
            serde_json::to_string(&contract_guard()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            install_hook(&dir, &rel).unwrap(),
            HookAction::Installed { .. }
        ));
        let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
        let commit = |extra: &[&str], msg: &str| {
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                ])
                .args(extra)
                .args(["-m", msg])
                .current_dir(&dir)
                .env("PATH", &path)
                .output()
                .unwrap()
        };
        let rejected = commit(&[], "see ADR-0125");
        assert!(!rejected.status.success());
        let err = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            err.contains("nexus-adr found") && err.contains("--no-verify"),
            "{err}"
        );
        assert!(commit(&["--no-verify"], "see ADR-0125").status.success());
        assert!(commit(&[], "fix(api): handle null ids").status.success());
        let _ = fs::remove_dir_all(&dir);
    }
}
